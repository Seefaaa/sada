//! WebRTC session setup and event loop handling.

use std::{
    collections::HashMap,
    io,
    net::IpAddr,
    sync::{
        Arc,
        OnceLock,
        atomic::{AtomicU32, Ordering},
    },
    time::Instant,
};

use str0m::{
    Candidate,
    Event,
    IceConnectionState,
    Input,
    Output,
    Rtc,
    RtcConfig,
    change::{SdpAnswer, SdpOffer, SdpPendingOffer},
    error::{IceError, RtcError, SdpError},
    format::PayloadParams,
    media::{Direction, MediaData, MediaKind, MediaTime, Mid},
    net::{Protocol, Receive},
};
use systemstat::{Platform, System};
use thiserror::Error;
use tokio::{
    net::UdpSocket,
    select,
    sync::{
        broadcast::{self, Receiver, Sender, error::RecvError},
        mpsc,
    },
    time,
};

#[cfg(feature = "audio_dump")]
use crate::media::AudioSink;
use crate::signaling::{AppState, SignalMessage};

/// Result type used by session setup.
pub type Result<T> = std::result::Result<T, Error>;

/// Identifier assigned to a connected WebRTC session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SessionId(u32);

/// Audio frame forwarded between sessions.
#[derive(Clone, Debug)]
struct ForwardedAudio {
    /// Session that produced the frame.
    origin: SessionId,
    /// Codec payload parameters from the incoming frame.
    params: PayloadParams,
    /// RTP media timestamp.
    time: MediaTime,
    /// Wallclock time associated with the received frame.
    network_time: Instant,
    /// Audio frame data.
    data: Vec<u8>,
    /// Whether this frame starts a talk spurt.
    start_of_talkspurt: bool,
}

impl ForwardedAudio {
    /// Build a forwarded audio frame from str0m media data.
    fn new(origin: SessionId, data: MediaData) -> Self {
        Self {
            origin,
            params: data.params,
            time: data.time,
            network_time: data.network_time,
            data: data.data,
            start_of_talkspurt: data.audio_start_of_talk_spurt,
        }
    }
}

/// Shared audio room used by all connected sessions.
#[derive(Clone)]
pub struct Room {
    /// Counter used to allocate session IDs.
    next_id: Arc<AtomicU32>,
    /// Broadcast channel carrying room-wide audio and lifecycle events.
    room_tx: Sender<RoomEvent>,
}

impl Room {
    /// Create an empty room.
    pub fn new() -> Self {
        let (room_tx, _) = broadcast::channel(256);

        Self {
            next_id: Arc::new(AtomicU32::new(1)),
            room_tx,
        }
    }

    /// Register a new session and return its room handle.
    fn join(&self) -> SessionRoom {
        SessionRoom {
            id: SessionId(self.next_id.fetch_add(1, Ordering::AcqRel)),
            room_tx: self.room_tx.clone(),
            room_rx: self.room_tx.subscribe(),
        }
    }
}

/// Event broadcast to every session in a room.
#[derive(Debug, Clone)]
enum RoomEvent {
    /// A session has produced a new audio frame to be forwarded to other sessions.
    Audio(ForwardedAudio),
    /// A session has left and its forwarded audio should be released.
    Left(SessionId),
}

/// Per-session view of the shared room.
struct SessionRoom {
    /// Current session ID.
    id: SessionId,
    /// Sender used to publish this session's room events.
    room_tx: Sender<RoomEvent>,
    /// Receiver used to consume room events from other sessions.
    room_rx: Receiver<RoomEvent>,
}

impl Drop for SessionRoom {
    fn drop(&mut self) {
        if let Err(err) = self.room_tx.send(RoomEvent::Left(self.id)) {
            warn!(session_id = self.id.0, ?err, "failed to send room leave event");
        }
    }
}

/// Local SDP offer waiting for a client answer.
struct PendingNegotiation {
    /// str0m handle required to accept the corresponding answer.
    offer: SdpPendingOffer,
    /// Locally-created media slots that become writable after the answer is accepted.
    mids: Vec<Mid>,
}

/// Select the first usable non-loopback IPv4 address from the host.
fn select_host_address() -> Result<IpAddr> {
    let system = System::new();
    let networks = system.networks().map_err(Error::ListNetworkInterfaces)?;

    for net in networks.values() {
        for n in &net.addrs {
            if let systemstat::IpAddr::V4(v) = n.addr
                && !v.is_loopback()
                && !v.is_link_local()
                && !v.is_broadcast()
            {
                return Ok(IpAddr::V4(v));
            }
        }
    }

    Err(Error::NoUsableNetworkInterface)
}

/// Select and cache the automatically detected host IP address.
fn auto_host_address() -> Result<IpAddr> {
    static ADDR: OnceLock<IpAddr> = OnceLock::new();

    match ADDR.get() {
        Some(addr) => Ok(*addr),
        None => {
            let addr = select_host_address()?;
            match ADDR.set(addr) {
                Ok(()) => Ok(addr),
                Err(addr) => Ok(addr),
            }
        },
    }
}

/// Next action requested by the WebRTC polling loop.
enum Loop {
    /// Wait until the given instant before polling again.
    Timeout(Instant),
    /// Continue polling immediately.
    Continue,
    /// Stop the session loop.
    Done,
}

/// A single WebRTC session backed by one UDP socket.
pub struct Session {
    /// WebRTC instance.
    rtc: Rtc,
    /// UDP socket used for ICE and RTP traffic.
    socket: UdpSocket,
    /// Room state used to forward audio between peers.
    room: SessionRoom,
    /// Debug audio sink.
    #[cfg(feature = "audio_dump")]
    sink: AudioSink,
    /// Sender for outgoing messages to the client WebSocket.
    signal_tx: mpsc::Sender<SignalMessage>,
    /// Receiver for incoming messages from the client WebSocket.
    signal_rx: mpsc::Receiver<SignalMessage>,
    /// Negotiated outgoing audio slots that are not assigned to a remote speaker.
    available_audio_mids: Vec<Mid>,
    /// Mapping from remote speaker sessions to their outgoing audio slot.
    remote_audio_mids: HashMap<SessionId, Mid>,
    /// In-flight local SDP offer.
    pending_negotiation: Option<PendingNegotiation>,
}

impl Session {
    /// Maximum UDP datagram size accepted for WebRTC input.
    const BUFFER_SIZE: usize = 65535;

    /// Return the numeric room-scoped identifier for this session.
    #[inline]
    fn id(&self) -> u32 { self.room.id.0 }

    /// Run the session event loop until the peer disconnects or an error occurs.
    pub async fn run(mut self) {
        info!("str0m run loop started");

        let mut buf = vec![0; Session::BUFFER_SIZE];

        loop {
            let timeout = match self.poll().await {
                Loop::Timeout(t) => t,
                Loop::Continue => continue,
                Loop::Done => return,
            };

            let sleep = timeout - Instant::now();

            if sleep.is_zero() {
                self.rtc.handle_input(Input::Timeout(Instant::now())).unwrap();
                continue;
            }

            select! {
                room_event = self.room.room_rx.recv() => {
                    match room_event {
                        Ok(RoomEvent::Audio(frame)) => {
                            self.handle_forwarded_audio(frame).await;
                        }
                        Ok(RoomEvent::Left(origin)) => {
                            self.release_forwarded_audio(origin);
                        }
                        Err(RecvError::Lagged(skipped)) => {
                            warn!(session_id = self.id(), skipped, "audio relay receiver lagged");
                        }
                        Err(RecvError::Closed) => {
                            break;
                        }
                    }
                }
                signal = self.signal_rx.recv() => {
                    let Some(signal) = signal else {
                        info!(session_id = self.id(), "signaling session closed by client");
                        break;
                    };
                    match signal {
                        SignalMessage::Answer { sdp } => {
                            let Some(pending) = self.pending_negotiation.take() else {
                                warn!(session_id = self.id(), "unexpected client answer without pending offer");
                                continue;
                            };

                            let Ok(answer) = SdpAnswer::from_sdp_string(&sdp) else {
                                warn!(session_id = self.id(), "failed to parse client answer SDP");
                                continue;
                            };
                            if let Err(err) = self.rtc.sdp_api().accept_answer(pending.offer, answer) {
                                warn!(session_id = self.id(), ?err, "failed to accept client answer");
                                continue;
                            }

                            debug!(session_id = self.id(), "client answer accepted, negotiation complete");

                            self.available_audio_mids.extend(pending.mids);
                        }
                        SignalMessage::Offer { .. } => {}, // unreachable
                        SignalMessage::Close => break,
                    }
                }
                datagram = self.socket.recv_from(&mut buf) => {
                    match datagram {
                        Ok((len, addr)) => {
                            let input = Input::Receive(
                                Instant::now(),
                                Receive {
                                    proto: Protocol::Udp,
                                    source: addr,
                                    destination: self.socket.local_addr().unwrap(),
                                    contents: buf[..len].try_into().unwrap(),
                                },
                            );
                            if let Err(err) = self.rtc.handle_input(input) {
                                error!(?err, "str0m handle_input error");
                                break;
                            }
                        }
                        Err(err) => {
                            error!(?err, "UDP recv error");
                            break;
                        }
                    }
                }
                _ = time::sleep(sleep) => {
                    self.rtc.handle_input(Input::Timeout(Instant::now())).unwrap();
                }
            }
        }
    }

    /// Poll str0m once and perform any requested I/O.
    async fn poll(&mut self) -> Loop {
        match self.rtc.poll_output() {
            Ok(Output::Timeout(i)) => Loop::Timeout(i),
            Ok(Output::Transmit(t)) => {
                if let Err(err) = self.socket.send_to(&t.contents, t.destination).await {
                    warn!(?err, "UDP send error");
                }
                Loop::Continue
            },
            Ok(Output::Event(event)) => self.handle_event(event),
            Err(err) => {
                debug!(?err, "str0m poll_output error");
                Loop::Done
            },
        }
    }

    /// Handle a single str0m event.
    fn handle_event(&mut self, event: Event) -> Loop {
        match event {
            Event::Connected => {
                info!(session_id = self.id(), "WebRTC peer connected");
            },
            Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                info!(session_id = self.id(), "ICE disconnected, ending run loop");
                return Loop::Done;
            },
            Event::IceConnectionStateChange(state) => {
                info!(session_id = self.id(), ?state, "ICE connection state change");
            },
            Event::MediaAdded(ma) => {
                info!(session_id = self.id(), mid = ?ma.mid, kind = ?ma.kind, "media added");
                if ma.kind == MediaKind::Audio && ma.direction.is_sending() {
                    self.available_audio_mids.push(ma.mid);
                }
            },
            Event::MediaData(data) => {
                #[cfg(feature = "audio_dump")]
                self.sink.handle_frame(&data);
                if let Err(err) = self
                    .room
                    .room_tx
                    .send(RoomEvent::Audio(ForwardedAudio::new(self.room.id, data)))
                {
                    warn!(session_id = self.id(), ?err, "audio relay has no receivers");
                }
            },
            Event::SenderFeedback(_) | Event::StreamPaused(_) => {},
            _ => debug!(session_id = self.id(), ?event, "unhandled event"),
        };
        Loop::Continue
    }

    /// Write audio received from another session to this WebRTC peer.
    async fn handle_forwarded_audio(&mut self, frame: ForwardedAudio) {
        if !self.rtc.is_connected() {
            return;
        }

        if frame.origin == self.room.id {
            return;
        }

        let Some(mid) = self.forwarded_audio_mid(frame.origin) else {
            #[cfg(debug_assertions)]
            debug!(session_id = self.id(), ?frame.origin, "no available media slot for forwarded audio");
            self.request_negotiation().await;
            return;
        };

        let Some(writer) = self.rtc.writer(mid) else {
            warn!(session_id = self.id(), ?mid, "no writer for forwarded audio");
            return;
        };

        let Some(pt) = writer.match_params(frame.params) else {
            warn!(
                session_id = self.id(),
                ?mid,
                "no matching payload type for forwarded audio"
            );
            return;
        };

        if let Err(err) =
            writer
                .start_of_talkspurt(frame.start_of_talkspurt)
                .write(pt, frame.network_time, frame.time, frame.data)
        {
            warn!(session_id = self.id(), ?err, "failed to write forwarded audio");
        }
    }

    /// Return or allocate the outgoing media slot for a remote speaker.
    fn forwarded_audio_mid(&mut self, origin: SessionId) -> Option<Mid> {
        if let Some(mid) = self.remote_audio_mids.get(&origin) {
            return Some(*mid);
        }

        let mid = self.available_audio_mids.pop()?;
        self.remote_audio_mids.insert(origin, mid);
        Some(mid)
    }

    /// Release the outgoing media slot used for a departed speaker.
    fn release_forwarded_audio(&mut self, origin: SessionId) {
        if let Some(mid) = self.remote_audio_mids.remove(&origin) {
            self.available_audio_mids.push(mid);
        }
    }

    /// Current number of remote speakers this peer can receive.
    fn remote_audio_capacity(&self) -> usize { self.remote_audio_mids.len() + self.available_audio_mids.len() }

    /// Request more outgoing audio slots from the browser peer.
    async fn request_negotiation(&mut self) {
        if self.pending_negotiation.is_some() {
            return;
        }

        let current = self.remote_audio_capacity();
        let desired = current.max(1).saturating_mul(2);

        if desired <= current {
            return;
        }

        let mut changes = self.rtc.sdp_api();
        let mut mids = Vec::with_capacity(desired - current);

        for _ in current..desired {
            mids.push(changes.add_media(MediaKind::Audio, Direction::SendOnly, None, None, None));
        }

        let Some((offer, pending)) = changes.apply() else {
            return;
        };

        let offer = SignalMessage::Offer {
            sdp: offer.to_sdp_string(),
        };
        if self.signal_tx.send(offer).await.is_err() {
            return;
        }

        self.pending_negotiation = Some(PendingNegotiation { offer: pending, mids });

        debug!(current, desired, session_id = self.id(), "requested renegotiation");
    }
}

/// Partially constructed session created while accepting a WebRTC offer.
pub struct SessionBuilder {
    /// WebRTC instance.
    rtc: Rtc,
    /// UDP socket used for ICE and RTP traffic.
    socket: UdpSocket,
}

impl SessionBuilder {
    /// Create a session builder from a remote SDP offer and return the SDP answer.
    pub async fn from_offer(offer_sdp: &str, state: &Arc<AppState>) -> Result<(Self, String)> {
        let offer = SdpOffer::from_sdp_string(offer_sdp).map_err(Error::ParseOffer)?;

        let mut rtc = RtcConfig::new().clear_codecs().enable_opus(true).build(Instant::now());

        let ip = match state.config.webrtc.host_ip {
            Some(ip) => ip,
            None => auto_host_address()?,
        };

        let socket = UdpSocket::bind((ip, 0)).await.map_err(|s| Error::BindSocket(ip, s))?;
        let addr = socket.local_addr().map_err(Error::LocalSocketAddress)?;
        let candidate = Candidate::host(addr, "udp").map_err(Error::CreateHostCandidate)?;
        rtc.add_local_candidate(candidate).ok_or(Error::AddLocalCandidate)?;

        let answer = rtc.sdp_api().accept_offer(offer).map_err(Error::AcceptOffer)?;

        let answer_sdp = answer.to_sdp_string();

        Ok((Self { rtc, socket }, answer_sdp))
    }

    /// Join the room and finish the session with an outgoing signaling sender.
    pub fn build(
        self,
        signal_tx: mpsc::Sender<SignalMessage>,
        signal_rx: mpsc::Receiver<SignalMessage>,
        room: &Room,
    ) -> Session {
        let room = room.join();
        #[cfg(feature = "audio_dump")]
        let id = room.id.0;

        Session {
            rtc: self.rtc,
            socket: self.socket,
            room,
            #[cfg(feature = "audio_dump")]
            sink: AudioSink::new(id),
            signal_tx,
            signal_rx,
            available_audio_mids: Vec::new(),
            remote_audio_mids: HashMap::new(),
            pending_negotiation: None,
        }
    }
}

/// Errors that can happen while creating a WebRTC session.
#[derive(Debug, Error)]
pub enum Error {
    /// The remote SDP offer could not be parsed.
    #[error("failed to parse SDP offer")]
    ParseOffer(#[source] SdpError),
    /// Local network interfaces could not be listed.
    #[error("failed to list network interfaces")]
    ListNetworkInterfaces(#[source] io::Error),
    /// No usable non-loopback IPv4 address was available.
    #[error("found no usable network interface")]
    NoUsableNetworkInterface,
    /// The UDP socket for WebRTC traffic could not be bound.
    #[error("failed to bind UDP socket to {0}")]
    BindSocket(IpAddr, #[source] io::Error),
    /// The bound UDP socket's local address could not be read.
    #[error("failed to get local socket address")]
    LocalSocketAddress(#[source] io::Error),
    /// A host ICE candidate could not be created.
    #[error("failed to create host candidate")]
    CreateHostCandidate(#[source] IceError),
    /// The host ICE candidate was rejected by str0m.
    #[error("failed to add local candidate")]
    AddLocalCandidate,
    /// str0m rejected the remote offer.
    #[error("failed to accept offer")]
    AcceptOffer(#[source] RtcError),
}
