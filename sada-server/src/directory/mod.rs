//! Player identity and the audio routing policy.
//!
//! The directory is the only owner of game-supplied state. It accepts commands from the control channel and the
//! WebSocket handlers, keeps the player table up to date, and pushes a fresh routing snapshot to the SFU whenever that
//! state changes so the audio path never has to ask another task a question.

pub mod codes;
pub mod player;
pub mod routing;

use std::{mem, sync::Arc, time::Duration};

use rustc_hash::FxHashMap;
use sada_common::{AuthCode, Ckey, ControlEvent, PlayerId, PlayerPatch, SessionId, Transmit};
use sada_utils::shutdown::Shutdown;
use tokio::{
    sync::{broadcast, mpsc, oneshot},
    time::{MissedTickBehavior, interval},
};

use crate::{
    config::{Config, RoutingPolicy},
    directory::{
        codes::CodeTable,
        player::PlayerTable,
        routing::{BroadcastRouter, HearerListRouter, ProximityRouter, Router},
    },
    sfu::{SfuEvent, WorkerCommand, WorkerHandle},
};

/// How many events a control connection may fall behind by before it starts losing the oldest of them.
const EVENT_CHANNEL_CAPACITY: usize = 4096;

/// How often codes nobody redeemed are swept out.
///
/// An expired code is refused the moment it is presented, so this only decides how long the entry lingers in memory
/// after it stopped working.
const CODE_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// A request for the directory.
pub enum DirectoryCommand {
    /// The game minted a code for a player.
    RegisterCode {
        /// Code shown to the player.
        code: AuthCode,
        /// Player it belongs to.
        player: PlayerId,
        /// Name the browser is greeted with.
        ckey: Ckey,
    },
    /// A browser presented a code. Answers with the player it identifies, spending the code.
    Redeem {
        /// Code the browser sent.
        code: AuthCode,
        /// Where to send the resolved player, and the name to greet them with.
        reply: oneshot::Sender<Option<(PlayerId, Ckey)>>,
    },
    /// A browser session finished connecting and is bound to the player whose code it redeemed.
    Bind {
        /// Player that authenticated.
        player: PlayerId,
        /// Session now bound to them.
        session: SessionId,
    },
    /// The game asks which session a player has authenticated.
    CheckAuth {
        /// Player to look up.
        player: PlayerId,
        /// Where to send the answer.
        reply: oneshot::Sender<Option<SessionId>>,
    },
    /// The game changed what a session is transmitting on.
    SetTransmit {
        /// Session to update.
        session: SessionId,
        /// What they are transmitting on, or `None` to stop them.
        transmit: Option<Transmit>,
    },
    /// The game updated a player's state.
    PatchPlayer {
        /// Player to update.
        player: PlayerId,
        /// Fields that changed.
        patch: PlayerPatch,
    },
    /// The game dropped a player.
    RemovePlayer {
        /// Player to forget.
        player: PlayerId,
    },
    /// A control connection is asking which sessions are bound, to catch up on what it may have missed.
    Bindings {
        /// Where to send them.
        reply: oneshot::Sender<Vec<(PlayerId, SessionId)>>,
    },
}

/// Handle used to talk to the directory.
#[derive(Clone)]
pub struct DirectoryHandle {
    /// Command channel.
    commands: mpsc::Sender<DirectoryCommand>,
    /// Events for the game, for whoever is serving the control channel to subscribe to.
    events: broadcast::Sender<ControlEvent>,
}

impl DirectoryHandle {
    /// Send a command, ignoring the case where the directory has stopped.
    pub async fn send(&self, command: DirectoryCommand) { let _ = self.commands.send(command).await; }

    /// Spend an auth code, answering with the player it identifies.
    pub async fn redeem(&self, code: AuthCode) -> Option<(PlayerId, Ckey)> {
        let (reply, answer) = oneshot::channel();
        let command = DirectoryCommand::Redeem { code, reply };
        self.commands.send(command).await.ok()?;
        answer.await.ok().flatten()
    }

    /// Look up the session bound to a player.
    pub async fn check_auth(&self, player: PlayerId) -> Option<SessionId> {
        let (reply, answer) = oneshot::channel();
        let command = DirectoryCommand::CheckAuth { player, reply };
        self.commands.send(command).await.ok()?;
        answer.await.ok().flatten()
    }

    /// Every session bound right now.
    pub async fn bindings(&self) -> Vec<(PlayerId, SessionId)> {
        let (reply, answer) = oneshot::channel();
        let command = DirectoryCommand::Bindings { reply };
        if self.commands.send(command).await.is_err() {
            return Vec::new();
        }
        answer.await.unwrap_or_default()
    }

    /// Follow the events the server produces for the game.
    ///
    /// Only what the server produces from here on: a subscriber that arrives late has missed what it missed, which is
    /// why a fresh control connection asks for [`DirectoryHandle::bindings`] as well.
    pub fn subscribe(&self) -> broadcast::Receiver<ControlEvent> { self.events.subscribe() }
}

/// Owns game state and derives routing from it.
pub struct Directory {
    /// Accumulated per-player state.
    players: PlayerTable,
    /// Policy turning that state into listener sets.
    router: Box<dyn Router>,
    /// Codes the game has minted that nobody has connected with yet.
    codes: CodeTable,
    /// Which session each player is bound to.
    bindings: FxHashMap<PlayerId, SessionId>,
    /// Where events for the game are published.
    events: broadcast::Sender<ControlEvent>,
    /// Whether player state has moved since the last snapshot went to the SFU.
    routing_dirty: bool,
    /// Handle used to push routing and transmit changes to the SFU.
    worker: WorkerHandle,
    /// Commands from the control channel and WebSocket handlers.
    commands: mpsc::Receiver<DirectoryCommand>,
    /// Notifications from the SFU.
    sfu_events: mpsc::Receiver<SfuEvent>,
    /// Shutdown signal.
    shutdown: Shutdown,
}

impl Directory {
    /// Build a directory and its handle.
    pub fn new(
        policy: RoutingPolicy,
        code_ttl: Duration,
        worker: WorkerHandle,
        sfu_events: mpsc::Receiver<SfuEvent>,
        shutdown: Shutdown,
    ) -> (Self, DirectoryHandle) {
        let (sender, commands) = mpsc::channel(256);
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);

        let router: Box<dyn Router> = match policy {
            RoutingPolicy::Broadcast => Box::new(BroadcastRouter),
            RoutingPolicy::HearerList => Box::new(HearerListRouter),
            RoutingPolicy::Proximity => Box::new(ProximityRouter::default()),
        };

        info!(policy = router.name(), "routing policy selected");

        let directory = Self {
            players: PlayerTable::new(),
            router,
            codes: CodeTable::new(code_ttl),
            bindings: FxHashMap::default(),
            events: events.clone(),
            routing_dirty: false,
            worker,
            commands,
            sfu_events,
            shutdown,
        };

        let handle = DirectoryHandle {
            commands: sender,
            events,
        };

        (directory, handle)
    }

    /// Run until shutdown.
    pub async fn run(mut self) {
        let mut sweep = interval(CODE_SWEEP_INTERVAL);
        sweep.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                () = self.shutdown.recv() => break,
                _ = sweep.tick() => self.sweep_codes(),
                command = self.commands.recv() => {
                    let Some(command) = command else { break };

                    self.on_command(command).await;

                    // Empty the queue before recomputing anything. The game describes every player it can see in
                    // one batch, and control.rs forwards those one command at a time, so publishing per patch
                    // would mean a full recompute and a separate snapshot for the worker N times a tick.
                    while let Ok(command) = self.commands.try_recv() {
                        self.on_command(command).await;
                    }

                    self.publish_routing().await;
                },
                event = self.sfu_events.recv() => match event {
                    Some(event) => self.on_sfu_event(event),
                    None => break,
                },
            }
        }

        info!("directory stopped");
    }

    /// Spawn a task to run the directory until shutdown, returning a handle to talk to it.
    pub fn spawn(
        config: &Config,
        worker: WorkerHandle,
        sfu_events: mpsc::Receiver<SfuEvent>,
        shutdown: Shutdown,
    ) -> DirectoryHandle {
        let (directory, handle) = Self::new(
            config.routing.policy,
            config.auth.code_ttl(),
            worker,
            sfu_events,
            shutdown,
        );
        tokio::spawn(directory.run());
        handle
    }

    /// Handle one command.
    async fn on_command(&mut self, command: DirectoryCommand) {
        match command {
            DirectoryCommand::RegisterCode { code, player, ckey } => {
                debug!(?code, %player, %ckey, "code registered");
                self.codes.register(code, player, ckey);
            },

            DirectoryCommand::Redeem { code, reply } => {
                let player = self.codes.redeem(&code);
                debug!(?code, accepted = player.is_some(), "code redeemed");
                let _ = reply.send(player);
            },

            DirectoryCommand::Bind { player, session } => {
                debug!(%player, %session, "player bound to session");
                self.bindings.insert(player, session);
                self.queue(ControlEvent::Authenticated { player, session });
            },

            DirectoryCommand::CheckAuth { player, reply } => {
                let _ = reply.send(self.bindings.get(&player).copied());
            },

            DirectoryCommand::SetTransmit { session, transmit } => {
                debug!(session = %session, transmit = ?transmit, "player changed transmit intent");
                self.worker.send(WorkerCommand::SetTransmit { session, transmit }).await;
            },

            DirectoryCommand::PatchPlayer { player, patch } => {
                self.routing_dirty |= self.players.apply(player, patch);
            },

            DirectoryCommand::RemovePlayer { player } => {
                debug!(%player, "player removed");
                self.codes.spend(player);
                self.bindings.remove(&player);
                self.routing_dirty |= self.players.remove(player);
            },

            DirectoryCommand::Bindings { reply } => {
                let _ = reply.send(self.bindings.iter().map(|(p, s)| (*p, *s)).collect());
            },
        }
    }

    /// Handle one notification from the SFU.
    fn on_sfu_event(&mut self, event: SfuEvent) {
        match event {
            SfuEvent::Closed { session, player } => {
                let Some(player) = player else {
                    return;
                };

                // Only clear the binding if it still points at this session; the
                // player may already have reconnected on a new one.
                if self.bindings.get(&player) == Some(&session) {
                    self.bindings.remove(&player);
                }

                self.queue(ControlEvent::Disconnected { player, session });
            },
        }
    }

    /// Drop the codes nobody came back for.
    fn sweep_codes(&mut self) {
        match self.codes.sweep() {
            0 => {},
            expired => debug!(expired, "auth codes expired"),
        }
    }

    /// Recompute the routing snapshot and hand it to the SFU, if player state has moved since the last one.
    ///
    /// Called once per drained batch of commands rather than once per change.
    async fn publish_routing(&mut self) {
        if !mem::take(&mut self.routing_dirty) {
            return;
        }

        let routing = Arc::new(self.router.compute(&self.players));
        debug!(players = self.players.len(), "routing recomputed");
        self.worker.send(WorkerCommand::Routing(routing)).await;
    }

    /// Publish an event for the game.
    ///
    /// Nobody serving the control channel means nobody to tell, and the event is dropped.
    fn queue(&self, event: ControlEvent) { let _ = self.events.send(event); }
}
