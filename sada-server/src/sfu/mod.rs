//! The selective forwarding unit: WebRTC transport and audio relay.
//!
//! The worker runs on its own thread. Its body is synchronous CPU work with no natural await points, and a long fan-out
//! burst on a shared runtime thread would stall the WebSocket handlers.

pub mod deadlines;
pub mod demux;
pub mod peer;
pub mod peers;
pub mod slots;
pub mod timeline;

use std::{
    collections::{HashMap, HashSet},
    io,
    mem,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use sada_common::{Ckey, SessionId};
use str0m::{
    Candidate,
    Event,
    IceConnectionState,
    Input,
    Output,
    Rtc,
    RtcConfig,
    change::SdpOffer,
    media::MediaData,
    net::{Protocol, Receive},
};
use systemstat::{Platform, System};
use thiserror::Error;
use tokio::{
    net::UdpSocket,
    select,
    sync::{mpsc, oneshot},
    time::sleep_until,
};

use crate::{
    config::Config,
    directory::routing::Routing,
    proto::ServerMessage,
    sfu::{
        deadlines::Deadlines,
        demux::AddressMap,
        peer::{Peer, Relay, Transmit},
        peers::Peers,
    },
    shutdown::Shutdown,
};

/// Largest UDP datagram accepted.
const BUFFER_SIZE: usize = 2048;

/// How long a peer may take to connect before it is reaped.
///
/// A browser that goes away between the answer and the first STUN leaves an
/// `Rtc` that will never wake itself up again, because str0m reports "no
/// deadline" for it. Without this it would sit in the slab forever.
const ESTABLISH_TIMEOUT: Duration = Duration::from_secs(30);

/// A request for the worker.
pub enum WorkerCommand {
    /// A browser finished its handshake and its offer has been answered.
    ///
    /// The `Rtc` arrives already built: constructing one costs the better part
    /// of a millisecond, which is time the worker would spend relaying nobody's
    /// audio. Registering it is all that is left, and that has to happen here
    /// because only the worker owns the peer table.
    Connect {
        /// The session's state machine. Boxed to keep the command enum small.
        rtc: Box<Rtc>,
        /// Player the session is bound to, if it authenticated.
        ckey: Option<Ckey>,
        /// Channel to push signaling messages back to the browser.
        signal: mpsc::Sender<ServerMessage>,
        /// Where to send the assigned session id.
        reply: oneshot::Sender<SessionId>,
    },
    /// The browser answered an offer the server sent.
    Answer {
        /// Session that answered.
        session: SessionId,
        /// Session description.
        sdp: String,
    },
    /// The browser muted or unmuted itself.
    Mute {
        /// Session that changed.
        session: SessionId,
        /// Whether the microphone is now muted.
        muted: bool,
    },
    /// The game changed a player's transmit intent.
    SetTransmit {
        /// Session that changed.
        session: SessionId,
        /// What they are transmitting on, `None` to stop.
        transmit: Option<Transmit>,
    },
    /// A new routing snapshot from the directory.
    Routing(Arc<Routing>),
    /// Drop a session.
    Disconnect {
        /// Session to drop.
        session: SessionId,
    },
}

/// A session the worker accepted.
pub struct ConnectAccepted {
    /// Identifier assigned to the session.
    pub session: SessionId,
    /// Answer to hand back to the browser.
    pub answer: String,
}

/// Why a browser's offer was refused.
#[derive(Debug, Error)]
pub enum ConnectError {
    /// The offer could not be turned into a session.
    #[error("failed to accept the offer")]
    Offer,
    /// The worker is gone.
    #[error("the media worker is not running")]
    Unavailable,
    /// The host candidate could not be built or accepted.
    #[error("host candidate could not be built or accepted")]
    Host,
}

/// Something the worker wants the rest of the server to know.
#[derive(Clone, Debug)]
pub enum SfuEvent {
    /// A session ended.
    Closed {
        /// Session that ended.
        session: SessionId,
        /// Player it was bound to, if any.
        ckey: Option<Ckey>,
    },
}

/// Handle used to talk to the worker from other tasks.
#[derive(Clone)]
pub struct WorkerHandle {
    /// Command channel.
    commands: mpsc::Sender<WorkerCommand>,
    /// Address advertised as the host ICE candidate.
    local_addr: SocketAddr,
}

impl WorkerHandle {
    /// Send a command, ignoring the case where the worker has stopped.
    pub async fn send(&self, command: WorkerCommand) { let _ = self.commands.send(command).await; }

    /// Turn a browser's offer into a registered session.
    ///
    /// The [`Rtc`] is built and the offer answered here, on the caller's task, because that is measurably expensive and
    /// the worker is the one place in the process where spending time blocks everybody's audio.
    ///
    /// The answer is still not handed to the browser until the worker has registered the peer: this waits for the
    /// session id first, so the browser's opening STUN cannot arrive before there is anything to accept it.
    pub async fn connect(
        &self,
        offer: SdpOffer,
        ckey: Option<Ckey>,
        signal: mpsc::Sender<ServerMessage>,
    ) -> Result<ConnectAccepted, ConnectError> {
        let (rtc, answer) = accept_offer(self.local_addr, offer)?;

        let (reply, session) = oneshot::channel();

        self.commands
            .send(WorkerCommand::Connect {
                rtc: Box::new(rtc),
                ckey,
                signal,
                reply,
            })
            .await
            .map_err(|_| ConnectError::Unavailable)?;

        let session = session.await.map_err(|_| ConnectError::Unavailable)?;

        Ok(ConnectAccepted { session, answer })
    }
}

/// Owns the UDP socket and every peer.
pub struct Worker {
    /// Shared socket carrying all WebRTC traffic.
    socket: UdpSocket,
    /// Address advertised as the host ICE candidate.
    local_addr: SocketAddr,
    /// Every connected peer.
    peers: Peers<Peer>,
    /// Learned source-address attributions.
    addresses: AddressMap,
    /// When each peer next needs to be driven forward.
    deadlines: Deadlines,
    /// Peers touched since the last drain.
    dirty: HashSet<SessionId>,
    /// Current routing policy.
    routing: Arc<Routing>,
    /// Reverse index from player to session.
    by_ckey: HashMap<Ckey, SessionId>,
    /// Incoming commands.
    commands: mpsc::Receiver<WorkerCommand>,
    /// Outgoing notifications.
    events: mpsc::Sender<SfuEvent>,
    /// Shutdown signal.
    shutdown: Shutdown,
}

impl Worker {
    /// Bind the shared media socket.
    ///
    /// Kept separate from [`Worker::spawn`] and free of any runtime, because the worker runs on its own thread with its
    /// own runtime and a tokio socket belongs to whichever runtime created it.
    pub fn bind(config: &Config) -> Result<(std::net::UdpSocket, SocketAddr), Error> {
        let ip = match config.webrtc.host_ip {
            Some(ip) => ip,
            None => select_host_address()?,
        };

        let socket = std::net::UdpSocket::bind((ip, 0)).map_err(|s| Error::BindSocket(ip, s))?;
        socket.set_nonblocking(true).map_err(|s| Error::BindSocket(ip, s))?;
        let local_addr = socket.local_addr().map_err(Error::LocalAddress)?;

        info!(addr = %local_addr, "media socket bound");

        Ok((socket, local_addr))
    }

    /// Start the worker on a dedicated thread.
    ///
    /// The worker's body is synchronous CPU work with no natural await points, so sharing a runtime thread with the
    /// WebSocket handlers would let a long fan-out burst stall signaling.
    pub fn spawn(
        socket: std::net::UdpSocket,
        local_addr: SocketAddr,
        events: mpsc::Sender<SfuEvent>,
        shutdown: Shutdown,
    ) -> WorkerHandle {
        let (sender, commands) = mpsc::channel(64);

        thread::Builder::new()
            .name("sada-sfu".to_owned())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(runtime) => runtime,
                    Err(err) => {
                        error!(?err, "could not start the media worker runtime");
                        return;
                    },
                };

                runtime.block_on(async move {
                    let socket = match UdpSocket::from_std(socket) {
                        Ok(socket) => socket,
                        Err(err) => {
                            error!(?err, "could not adopt the media socket");
                            return;
                        },
                    };

                    Worker {
                        socket,
                        local_addr,
                        peers: Peers::new(),
                        addresses: AddressMap::new(),
                        deadlines: Deadlines::new(),
                        dirty: HashSet::new(),
                        routing: Arc::new(Routing::Unrestricted),
                        by_ckey: HashMap::new(),
                        commands,
                        events,
                        shutdown,
                    }
                    .run()
                    .await;
                });
            })
            .expect("spawning a thread at startup");

        WorkerHandle {
            commands: sender,
            local_addr,
        }
    }

    /// Run until shutdown.
    pub async fn run(mut self) {
        info!(addr = %self.local_addr, "media worker listening");

        let mut buf = vec![0; BUFFER_SIZE];

        loop {
            self.drain_dirty();
            self.reap_stalled();

            let deadline = self.deadlines.next_deadline();

            select! {
                biased;

                () = self.shutdown.recv() => break,

                command = self.commands.recv() => match command {
                    Some(command) => self.on_command(command).await,
                    None => break,
                },

                () = sleep_until(deadline_instant(deadline)), if deadline.is_some() => self.on_deadline(),

                received = self.socket.recv_from(&mut buf) => match received {
                    Ok((len, source)) => self.on_datagram(&buf[..len], source),
                    Err(err) => {
                        error!(?err, "UDP receive failed");
                        break;
                    },
                },
            }
        }

        self.close_all();

        info!("media worker stopped");
    }

    /// Handle one command.
    async fn on_command(&mut self, command: WorkerCommand) {
        match command {
            WorkerCommand::Connect {
                rtc,
                ckey,
                signal,
                reply,
            } => {
                let _ = reply.send(self.register(*rtc, ckey, signal));
            },
            WorkerCommand::Answer { session, sdp } => {
                let Some(peer) = self.peers.get_mut(session) else {
                    return;
                };
                if let Err(err) = peer.accept_answer(&sdp) {
                    warn!(%session, ?err, "answer rejected");
                }
                self.touch(session);
            },
            WorkerCommand::Mute { session, muted } => {
                if let Some(peer) = self.peers.get_mut(session) {
                    peer.self_muted = muted;
                }
            },
            WorkerCommand::SetTransmit { session, transmit } => {
                if let Some(peer) = self.peers.get_mut(session) {
                    peer.transmit = transmit;
                }
            },
            WorkerCommand::Routing(routing) => {
                self.routing = routing;
            },
            WorkerCommand::Disconnect { session } => self.remove_peer(session, "disconnected"),
        }
    }

    /// Register an already-built session and assign it an id.
    ///
    /// Everything expensive has happened by the time this runs; see [`WorkerHandle::connect`].
    fn register(&mut self, rtc: Rtc, ckey: Option<Ckey>, signal: mpsc::Sender<ServerMessage>) -> SessionId {
        let peer = Peer::new(rtc, ckey.clone(), signal, Instant::now());
        let session = self.peers.insert(peer);

        if let Some(ckey) = ckey {
            self.by_ckey.insert(ckey, session);
        }

        #[cfg(feature = "audio_dump")]
        if let Some(peer) = self.peers.get_mut(session) {
            peer.enable_capture(session);
        }

        self.touch(session);

        debug!(%session, peers = self.peers.len(), "session accepted");

        session
    }

    /// Attribute a datagram to a peer and feed it in.
    fn on_datagram(&mut self, datagram: &[u8], source: SocketAddr) {
        let Ok(contents) = datagram.try_into() else {
            return;
        };

        let input = Input::Receive(
            Instant::now(),
            Receive {
                proto: Protocol::Udp,
                source,
                destination: self.local_addr,
                contents,
            },
        );

        let Some(session) = self.route(source, &input) else {
            // common right after an offer
            debug!(%source, "no peer accepted the datagram");
            return;
        };

        if let Some(peer) = self.peers.get_mut(session)
            && let Err(err) = peer.rtc.handle_input(input)
        {
            debug!(%session, ?err, "peer rejected input");
        }

        self.touch(session);
    }

    /// Decide which peer a datagram belongs to.
    ///
    /// The learned address map is only a hint: ICE can renominate mid-call without a restart, and a NAT rebinding can
    /// hand an address to a different peer, so the hint is always confirmed with [`accepts`](str0m::Rtc::accepts)
    /// before it is used.
    fn route(&mut self, source: SocketAddr, input: &Input) -> Option<SessionId> {
        if let Some(session) = self.addresses.lookup(source) {
            match self.peers.get(session) {
                Some(peer) if peer.rtc.accepts(input) => return Some(session),
                // stale: the peer moved, went away, or the address was reissued.
                _ => self.addresses.forget_address(source),
            }
        }

        // ICE restart, NAT rebinding and late trickle candidates all make a
        // connected peer start speaking from a new address.
        let session = self
            .peers
            .iter()
            .find(|(_, peer)| peer.rtc.accepts(input))
            .map(|(session, _)| session)?;

        self.addresses.learn(source, session);

        Some(session)
    }

    /// Drive every peer whose deadline has passed.
    fn on_deadline(&mut self) {
        let now = Instant::now();

        for session in self.deadlines.take_expired(now) {
            if let Some(peer) = self.peers.get_mut(session)
                && let Err(err) = peer.rtc.handle_input(Input::Timeout(now))
            {
                debug!(%session, ?err, "peer rejected timeout");
            }
            self.touch(session);
        }
    }

    /// Mark a peer as needing a drain.
    fn touch(&mut self, session: SessionId) { self.dirty.insert(session); }

    /// Drain every peer touched since the last pass.
    fn drain_dirty(&mut self) {
        while !self.dirty.is_empty() {
            // potential optimization: double buffer swap
            for session in mem::take(&mut self.dirty) {
                self.drain_peer(session);
            }
        }
    }

    /// Poll one peer to its next deadline, acting on everything it produces.
    fn drain_peer(&mut self, session: SessionId) {
        let Some(mut peer) = self.peers.take(session) else {
            return;
        };

        let now = Instant::now();
        let mut deadline = None;

        let mut alive = loop {
            match peer.rtc.poll_output() {
                Ok(Output::Timeout(at)) => {
                    deadline = Some(at);
                    break true;
                },
                Ok(Output::Transmit(transmit)) => {
                    // learning the destination here keeps the address map fresh across renomination and ICE restart
                    self.addresses.learn(transmit.destination, session);

                    if let Err(err) = self.socket.try_send_to(&transmit.contents, transmit.destination)
                        && err.kind() != io::ErrorKind::WouldBlock
                    {
                        debug!(%session, ?err, "UDP send failed");
                    }
                },
                Ok(Output::Event(event)) => {
                    if !self.on_event(session, &mut peer, event) {
                        break false;
                    }
                },
                Err(err) => {
                    debug!(%session, ?err, "peer stopped");
                    break false;
                },
            }
        };

        // Growing the slot pool is the server's job alone: str0m allows one
        // negotiation in flight and drops a pending offer the moment it accepts
        // an incoming one, so only one side may ever offer.
        if alive && let Some(sdp) = peer.take_offer() {
            if !peer.notify(ServerMessage::Offer { sdp }) {
                alive = false;
            }
            self.touch(session);
        }

        self.peers.restore(session, peer);

        if alive {
            if let Some(at) = deadline {
                self.deadlines.set(session, at, now);
            }
        } else {
            self.drop_peer(session);
        }
    }

    /// Act on one str0m event. Returns whether the peer is still usable.
    fn on_event(&mut self, session: SessionId, peer: &mut Peer, event: Event) -> bool {
        match event {
            Event::Connected => info!(%session, "peer connected"),
            Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                info!(%session, "peer disconnected");
                return false;
            },
            Event::IceConnectionStateChange(state) => debug!(%session, ?state, "ICE state changed"),
            Event::MediaAdded(added) => peer.on_media_added(added.mid, added.kind, added.direction),
            Event::MediaData(data) => {
                #[cfg(feature = "audio_dump")]
                peer.capture(&data);
                self.relay(session, peer, &data);
            },
            Event::SenderFeedback(_) | Event::StreamPaused(_) => {},
            other => debug!(%session, ?other, "unhandled event"),
        }

        true
    }

    /// Send one speaker's frame to everyone who should hear it.
    fn relay(&mut self, speaker: SessionId, peer: &Peer, data: &MediaData) {
        for listener in self.listeners_for(speaker, peer) {
            let Some(target) = self.peers.get_mut(listener) else {
                continue;
            };

            match target.relay(speaker, data) {
                // Writing queues work that only a drain will flush, so the
                // listener must be re-examined before the loop sleeps again.
                Relay::Written | Relay::NoSlot => self.touch(listener),
                Relay::NotConnected | Relay::Rejected => {},
            }
        }
    }

    /// Resolve who should hear a speaker right now.
    fn listeners_for(&self, speaker: SessionId, peer: &Peer) -> Vec<SessionId> {
        if peer.self_muted {
            return Vec::new();
        }

        match self.routing.as_ref() {
            // With no game driving the server there is no push-to-talk and no
            // topology, so everyone hears everyone. This is the standalone and
            // development behaviour.
            Routing::Unrestricted => self
                .peers
                .iter()
                .map(|(session, _)| session)
                .filter(|&session| session != speaker)
                .collect(),

            Routing::Explicit(table) => {
                // With a game attached, silence is the default: a player is only
                // heard while actually transmitting.
                if !peer.is_transmitting() {
                    return Vec::new();
                }

                let (Some(ckey), Some(transmit)) = (peer.ckey.as_ref(), peer.transmit) else {
                    return Vec::new();
                };

                if !table.can_speak(ckey) {
                    return Vec::new();
                }

                let listeners = match transmit {
                    Transmit::Local => table.local_listeners(ckey),
                    // Naming a frequency is not the same as being allowed to use
                    // it; the game decides which radios are actually keyed up.
                    Transmit::Radio(channel) if table.can_transmit_on(ckey, channel) => table.radio_listeners(channel),
                    Transmit::Radio(_) => return Vec::new(),
                };

                listeners
                    .iter()
                    .filter_map(|listener| self.by_ckey.get(listener).copied())
                    .filter(|&session| session != speaker)
                    .collect()
            },
        }
    }

    /// Drop peers that never finished connecting.
    fn reap_stalled(&mut self) {
        let now = Instant::now();

        let stalled = self
            .peers
            .iter()
            .filter(|(_, peer)| !peer.rtc.is_alive() || stalled(peer, now))
            .map(|(session, _)| session)
            .collect::<Vec<_>>();

        for session in stalled {
            info!(%session, "reaping a session that never connected");
            self.drop_peer(session);
        }
    }

    /// Remove a peer and tell everyone who cares.
    fn remove_peer(&mut self, session: SessionId, reason: &str) {
        if let Some(peer) = self.peers.get(session) {
            peer.notify(ServerMessage::Bye {
                reason: reason.to_owned(),
            });
        }
        self.drop_peer(session);
    }

    /// Forget a peer, releasing the slots it occupied elsewhere.
    fn drop_peer(&mut self, session: SessionId) {
        self.deadlines.remove(session);

        let Some(peer) = self.peers.remove(session) else {
            return;
        };

        self.addresses.forget_session(session);

        // Only forget the player if the index still points at this session: they may already be on a newer one, and
        // dropping that entry would leave a live session nobody can route audio to.
        if let Some(ckey) = &peer.ckey
            && self.by_ckey.get(ckey) == Some(&session)
        {
            self.by_ckey.remove(ckey);
        }

        // Everyone listening to this speaker can reuse the slot it held.
        for fellow in self.peers.ids() {
            if let Some(peer) = self.peers.get_mut(fellow) {
                peer.release_speaker(session);
            }
        }

        let _ = self.events.try_send(SfuEvent::Closed {
            session,
            ckey: peer.ckey,
        });
    }

    /// Say goodbye to every peer on the way out.
    fn close_all(&mut self) {
        for session in self.peers.ids() {
            self.remove_peer(session, "server shutting down");
        }
    }
}

/// Answer the browser's offer.
///
/// Deliberately a free function rather than a method: it touches nothing the worker owns, which is what allows it to
/// run on the connecting task instead of in the worker loop.
fn accept_offer(local_addr: SocketAddr, offer: SdpOffer) -> Result<(Rtc, String), ConnectError> {
    let mut rtc = RtcConfig::new()
        .clear_codecs()
        .enable_opus(true)
        // The server has a stable address and never needs to probe, so it can be
        // a passive ICE endpoint. This removes all connectivity-check timers and
        // the nomination churn that goes with them.
        .set_ice_lite(true)
        .build(Instant::now());

    let candidate = Candidate::host(local_addr, "udp").map_err(|err| {
        error!(?err, "failed to build the host candidate");
        ConnectError::Host
    })?;

    if rtc.add_local_candidate(candidate).is_none() {
        error!("host candidate was rejected");
        return Err(ConnectError::Host);
    }

    let answer = rtc.sdp_api().accept_offer(offer).map_err(|err| {
        warn!(?err, "offer rejected");
        ConnectError::Offer
    })?;

    Ok((rtc, answer.to_sdp_string()))
}

/// Whether a peer has taken too long to connect.
fn stalled(peer: &Peer, now: Instant) -> bool {
    !peer.rtc.is_connected() && now.saturating_duration_since(peer.created) > ESTABLISH_TIMEOUT
}

/// Turn an optional deadline into something [`sleep_until`] accepts.
///
/// The branch is guarded on the deadline existing, so the fallback is never actually awaited.
fn deadline_instant(deadline: Option<Instant>) -> tokio::time::Instant {
    tokio::time::Instant::from_std(deadline.unwrap_or_else(Instant::now))
}

/// Pick the first usable non-loopback IPv4 address on the host.
fn select_host_address() -> Result<IpAddr, Error> {
    let system = System::new();
    let networks = system.networks().map_err(Error::ListInterfaces)?;

    for network in networks.values() {
        for address in &network.addrs {
            if let systemstat::IpAddr::V4(v4) = address.addr
                && !v4.is_loopback()
                && !v4.is_link_local()
                && !v4.is_broadcast()
            {
                return Ok(IpAddr::V4(v4));
            }
        }
    }

    Err(Error::NoUsableInterface)
}

/// Errors that can stop the worker from starting.
#[derive(Debug, Error)]
pub enum Error {
    /// Local network interfaces could not be listed.
    #[error("failed to list network interfaces")]
    ListInterfaces(#[source] io::Error),
    /// No usable non-loopback IPv4 address was available.
    #[error("found no usable network interface")]
    NoUsableInterface,
    /// The UDP socket could not be bound.
    #[error("failed to bind the media socket to {0}")]
    BindSocket(IpAddr, #[source] io::Error),
    /// The bound socket's address could not be read.
    #[error("failed to read the media socket address")]
    LocalAddress(#[source] io::Error),
}
