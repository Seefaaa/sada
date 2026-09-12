//! Player identity and the audio routing policy.
//!
//! The directory is the only owner of game-supplied state. It accepts commands from the control channel and the
//! WebSocket handlers, keeps the player table up to date, and pushes a fresh routing snapshot to the SFU whenever that
//! state changes so the audio path never has to ask another task a question.

pub mod codes;
pub mod player;
pub mod routing;

use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::Duration,
};

use sada_common::{AuthCode, Ckey, ControlEvent, Freq, PlayerPatch, SessionId};
use tokio::{
    sync::{mpsc, oneshot},
    time::{MissedTickBehavior, interval},
};

use crate::{
    config::{Config, RoutingPolicy},
    directory::{
        codes::CodeTable,
        player::PlayerTable,
        routing::{BroadcastRouter, HearerListRouter, ProximityRouter, Router},
    },
    sfu::{SfuEvent, WorkerCommand, WorkerHandle, peer::Transmit},
    shutdown::Shutdown,
};

/// Largest number of queued events held for the game.
///
/// The game polls every tick; if it stops polling there is no point growing the queue without bound, so the oldest
/// events are dropped.
const MAX_QUEUED_EVENTS: usize = 4096;

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
        ckey: Ckey,
    },
    /// A browser presented a code. Answers with the player it identifies, spending the code.
    Redeem {
        /// Code the browser sent.
        code: AuthCode,
        /// Where to send the resolved player.
        reply: oneshot::Sender<Option<Ckey>>,
    },
    /// A browser session finished connecting and is bound to the player whose code it redeemed.
    Bind {
        /// Player that authenticated.
        ckey: Ckey,
        /// Session now bound to them.
        session: SessionId,
    },
    /// The game asks which session a player has authenticated.
    CheckAuth {
        /// Player to look up.
        ckey: Ckey,
        /// Where to send the answer.
        reply: oneshot::Sender<Option<SessionId>>,
    },
    /// The game changed a player's transmit intent.
    SetPtt {
        /// Session to update.
        session: SessionId,
        /// Frequency, or `None` for local speech.
        channel: Option<Freq>,
    },
    /// The game stopped a player transmitting.
    ClearPtt {
        /// Session to update.
        session: SessionId,
    },
    /// The game updated a player's state.
    PatchPlayer {
        /// Player to update.
        ckey: Ckey,
        /// Fields that changed.
        patch: PlayerPatch,
    },
    /// The game dropped a player.
    RemovePlayer {
        /// Player to forget.
        ckey: Ckey,
    },
    /// The game is collecting queued events.
    PollEvents {
        /// Maximum number to return.
        max: usize,
        /// Where to send them.
        reply: oneshot::Sender<Vec<ControlEvent>>,
    },
}

/// Handle used to talk to the directory.
#[derive(Clone)]
pub struct DirectoryHandle {
    /// Command channel.
    commands: mpsc::Sender<DirectoryCommand>,
}

impl DirectoryHandle {
    /// Send a command, ignoring the case where the directory has stopped.
    pub async fn send(&self, command: DirectoryCommand) { let _ = self.commands.send(command).await; }

    /// Spend an auth code, answering with the player it identifies.
    pub async fn redeem(&self, code: AuthCode) -> Option<Ckey> {
        let (reply, answer) = oneshot::channel();
        let command = DirectoryCommand::Redeem { code, reply };
        self.commands.send(command).await.ok()?;
        answer.await.ok().flatten()
    }

    /// Look up the session bound to a player.
    pub async fn check_auth(&self, ckey: Ckey) -> Option<SessionId> {
        let (reply, answer) = oneshot::channel();
        let command = DirectoryCommand::CheckAuth { ckey, reply };
        self.commands.send(command).await.ok()?;
        answer.await.ok().flatten()
    }

    /// Collect queued events for the game.
    pub async fn poll_events(&self, max: usize) -> Vec<ControlEvent> {
        let (reply, answer) = oneshot::channel();
        let command = DirectoryCommand::PollEvents { max, reply };
        if self.commands.send(command).await.is_err() {
            return Vec::new();
        }
        answer.await.unwrap_or_default()
    }
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
    bindings: HashMap<Ckey, SessionId>,
    /// Events waiting for the game to collect them.
    events: VecDeque<ControlEvent>,
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
            bindings: HashMap::new(),
            events: VecDeque::new(),
            worker,
            commands,
            sfu_events,
            shutdown,
        };

        (directory, DirectoryHandle { commands: sender })
    }

    /// Run until shutdown.
    pub async fn run(mut self) {
        let mut sweep = interval(CODE_SWEEP_INTERVAL);
        sweep.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                () = self.shutdown.recv() => break,
                _ = sweep.tick() => self.sweep_codes(),
                command = self.commands.recv() => match command {
                    Some(command) => self.on_command(command).await,
                    None => break,
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
            DirectoryCommand::RegisterCode { code, ckey } => {
                debug!(?code, ckey = %ckey, "code registered");
                self.codes.register(code, ckey);
            },

            DirectoryCommand::Redeem { code, reply } => {
                let ckey = self.codes.redeem(&code);
                debug!(?code, accepted = ckey.is_some(), "code redeemed");
                let _ = reply.send(ckey);
            },

            DirectoryCommand::Bind { ckey, session } => {
                debug!(ckey = %ckey, session = %session, "player bound to session");
                self.bindings.insert(ckey.clone(), session);
                self.queue(ControlEvent::Authenticated { ckey, session });
            },

            DirectoryCommand::CheckAuth { ckey, reply } => {
                let _ = reply.send(self.bindings.get(&ckey).copied());
            },

            DirectoryCommand::SetPtt { session, channel } => {
                debug!(session = %session, channel = ?channel, "player changed transmit intent");
                let transmit = Some(channel.map_or(Transmit::Local, Transmit::Radio));
                self.worker.send(WorkerCommand::SetTransmit { session, transmit }).await;
            },

            DirectoryCommand::ClearPtt { session } => {
                debug!(session = %session, "player stopped transmitting");
                self.worker
                    .send(WorkerCommand::SetTransmit {
                        session,
                        transmit: None,
                    })
                    .await;
            },

            DirectoryCommand::PatchPlayer { ckey, patch } => {
                if self.players.apply(ckey, patch) {
                    self.publish_routing().await;
                }
            },

            DirectoryCommand::RemovePlayer { ckey } => {
                debug!(ckey = %ckey, "player removed");
                self.codes.spend(&ckey);
                self.bindings.remove(&ckey);
                if self.players.remove(&ckey) {
                    self.publish_routing().await;
                }
            },

            DirectoryCommand::PollEvents { max, reply } => {
                let taken = self.events.drain(..self.events.len().min(max)).collect();
                let _ = reply.send(taken);
            },
        }
    }

    /// Handle one notification from the SFU.
    fn on_sfu_event(&mut self, event: SfuEvent) {
        match event {
            SfuEvent::Closed { session, ckey } => {
                let Some(ckey) = ckey else {
                    return;
                };

                // Only clear the binding if it still points at this session; the
                // player may already have reconnected on a new one.
                if self.bindings.get(&ckey) == Some(&session) {
                    self.bindings.remove(&ckey);
                }

                self.queue(ControlEvent::Disconnected { ckey, session });
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

    /// Recompute the routing snapshot and hand it to the SFU.
    async fn publish_routing(&mut self) {
        let routing = Arc::new(self.router.compute(&self.players));
        debug!(players = self.players.len(), "routing recomputed");
        self.worker.send(WorkerCommand::Routing(routing)).await;
    }

    /// Queue an event for the game to collect.
    fn queue(&mut self, event: ControlEvent) {
        if self.events.len() >= MAX_QUEUED_EVENTS {
            self.events.pop_front();
            warn!("event queue is full, dropping the oldest event");
        }
        self.events.push_back(event);
    }
}
