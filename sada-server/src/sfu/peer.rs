//! One connected browser, and everything the worker knows about it.

use std::{
    collections::HashMap,
    io::Cursor,
    time::{Duration, Instant},
};

use sada_common::{Ckey, SessionId, Transmit};
use serde::Serialize;
use str0m::{
    Rtc,
    change::{SdpAnswer, SdpPendingOffer},
    channel::ChannelId,
    media::{Direction, MediaData, MediaKind, MediaTime, Mid},
};
use thiserror::Error;
use tokio::sync::mpsc;

#[cfg(feature = "audio_dump")]
use crate::audio::AudioSink;
use crate::{
    proto::{AudibleSpeaker, Offset, ServerMessage, ServerOrderedMessage, ServerUnorderedMessage},
    sfu::{
        slots::{Grant, SlotTable},
        timeline::SlotTimeline,
    },
};

/// Label of the channel that keeps its promises, and the only one the browser may send on.
const ORDERED_CHANNEL_LABEL: &str = "ordered";

/// Label of the channel that keeps none of them.
const UNORDERED_CHANNEL_LABEL: &str = "unordered";

/// How long a peer may go without a position update before one is sent whether anything changed or not.
///
/// Positions travel on a channel that drops messages rather than repairing them, so one that goes missing is only
/// corrected by the next send. Without a floor, a speaker who has stopped moving would stay wherever the lost
/// message left them for as long as they stand still.
pub const POSITION_REFRESH: Duration = Duration::from_secs(5);

/// One speaker a listener can hear, as the worker tracks it.
///
/// Kept apart from [`AudibleSpeaker`], the shape that goes on the wire, so that comparing one round against the last
/// allocates nothing: a [`Mid`] is [`Copy`], the string it is sent as is not.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Audible {
    /// Session occupying the slot.
    pub session: SessionId,
    /// The m-line carrying them.
    pub mid: Mid,
    /// Where they are relative to the listener, absent when they cannot be placed.
    pub offset: Option<Offset>,
}

impl From<&Audible> for AudibleSpeaker {
    fn from(audible: &Audible) -> Self {
        Self {
            session: audible.session,
            mid: audible.mid.to_string(),
            offset: audible.offset,
        }
    }
}

/// What a peer was last told about positions, and when.
#[derive(Default)]
struct PositionState {
    /// The set as it was last sent, in session order.
    last: Vec<Audible>,
    /// When that went out. `None` until the first one does.
    sent: Option<Instant>,
}

impl PositionState {
    /// Whether `next` is worth sending.
    ///
    /// Being unchanged is not enough to stay quiet, hence [`POSITION_REFRESH`]. `next` is expected in session
    /// order, which is what lets an unchanged set compare equal however the slot table happens to iterate.
    fn needs(&self, next: &[Audible], now: Instant) -> bool {
        self.last != next
            || self
                .sent
                .is_none_or(|sent| now.duration_since(sent) >= POSITION_REFRESH)
    }

    /// Remember what was just sent, so the next round can tell whether anything moved.
    fn record(&mut self, next: Vec<Audible>, now: Instant) {
        self.last = next;
        self.sent = Some(now);
    }
}

/// The browser's data channels, named for what each one guarantees.
///
/// Either may be absent: they open a moment after the peer connects, and can close on their own.
#[derive(Default)]
struct Channels {
    /// Ordered and reliable. Renegotiation and mute.
    ordered: Option<ChannelId>,
    /// Unordered, with no retransmits. Nothing on it may be state the receiver has to accumulate.
    unordered: Option<ChannelId>,
}

/// An SDP offer the server has sent and not yet had answered.
struct Negotiation {
    /// str0m handle required to accept the matching answer.
    pending: SdpPendingOffer,
    /// Slots that become usable once the answer is accepted.
    ///
    /// Nothing is applied to the session until then, so if the negotiation is
    /// abandoned these simply never come into existence.
    mids: Vec<Mid>,
}

/// Outcome of trying to relay one frame to a peer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Relay {
    /// The frame was queued for sending.
    Written,
    /// The peer is not connected yet, so writing would be discarded.
    NotConnected,
    /// No outgoing slot was free; a renegotiation has been requested.
    NoSlot,
    /// No outgoing slot was free and none can be added, so the frame is dropped.
    ///
    /// Distinct from [`Relay::NoSlot`] because nothing is pending: the caller has no reason to drain this peer, and
    /// at the slot ceiling that would otherwise happen for every frame of every speaker who does not fit.
    Unheard,
    /// The peer has a slot but could not accept the frame.
    Rejected,
}

/// A connected browser.
pub struct Peer {
    /// WebRTC state machine. Owned exclusively by the worker task.
    pub rtc: Rtc,
    /// Player this session is bound to, absent while anonymous.
    pub ckey: Option<Ckey>,
    /// What the player is transmitting on, `None` when not transmitting.
    ///
    /// Applied the moment the game says so rather than at snapshot cadence: a
    /// late release would leave the microphone hot for a fifth of a second.
    pub transmit: Option<Transmit>,
    /// Whether the browser has muted itself.
    pub self_muted: bool,
    /// When the peer was created, used to reap connections that never complete.
    pub created: Instant,
    /// Channel back to this peer's WebSocket task.
    signal: mpsc::Sender<ServerMessage>,
    /// Outgoing audio slots and their assignment to speakers.
    slots: SlotTable<Mid>,
    /// Output clock for each slot, so reuse does not break receiver timing.
    timelines: HashMap<Mid, SlotTimeline>,
    /// In-flight offer, if any.
    negotiation: Option<Negotiation>,
    /// Set when a speaker needed a slot and none was free.
    wants_slots: bool,
    /// Debug capture of this peer's incoming audio.
    ///
    /// Created once the session id is known, which is only after the peer has
    /// been stored.
    #[cfg(feature = "audio_dump")]
    sink: Option<AudioSink>,
    /// The browser's data channels.
    channels: Channels,
    /// What this peer was last told about where the speakers it hears are standing.
    positions: PositionState,
    /// Scratch space for encoding an outgoing channel message, reused from one send to the next.
    send_on_buffer: Vec<u8>,
}

impl Peer {
    /// Wrap a freshly built [`Rtc`].
    pub fn new(rtc: Rtc, ckey: Option<Ckey>, signal: mpsc::Sender<ServerMessage>, now: Instant) -> Self {
        Self {
            rtc,
            ckey,
            transmit: None,
            self_muted: false,
            created: now,
            signal,
            slots: SlotTable::new(),
            timelines: HashMap::new(),
            negotiation: None,
            wants_slots: false,
            #[cfg(feature = "audio_dump")]
            sink: None,
            channels: Channels::default(),
            positions: PositionState::default(),
            send_on_buffer: Vec::new(),
        }
    }

    /// Whether this peer's audio should currently reach anyone.
    #[must_use]
    pub fn is_transmitting(&self) -> bool { !self.self_muted && self.transmit.is_some() }

    /// Try to send a signaling message, reporting whether the socket is still there.
    pub fn notify(&self, message: ServerMessage) -> bool { self.signal.try_send(message).is_ok() }

    /// Remember one of the browser's data channels, which is how the server reaches it outside the handshake.
    pub fn on_channel_open(&mut self, channel_id: ChannelId, label: &str) {
        let slot = match label {
            ORDERED_CHANNEL_LABEL => &mut self.channels.ordered,
            UNORDERED_CHANNEL_LABEL => &mut self.channels.unordered,
            _ => {
                debug!(?channel_id, ?label, "ignoring an unknown data channel");
                return;
            },
        };

        *slot = Some(channel_id);

        debug!(?channel_id, ?label, "data channel opened");
    }

    /// Forget a data channel, which leaves the peer connected but no longer reachable that way.
    pub fn on_channel_close(&mut self, channel_id: ChannelId) {
        let slot = if self.channels.ordered == Some(channel_id) {
            &mut self.channels.ordered
        } else if self.channels.unordered == Some(channel_id) {
            &mut self.channels.unordered
        } else {
            debug!(?channel_id, "ignoring the close of an unknown data channel");
            return;
        };

        *slot = None;

        debug!(?channel_id, "data channel closed");
    }

    /// The open ordered channel, if there is one.
    #[must_use]
    pub fn ordered_channel(&self) -> Option<ChannelId> { self.channels.ordered }

    /// Whether there is an ordered channel to send on.
    ///
    /// Not the same as being connected: the channel opens a moment after the peer does, and may close on its own.
    #[must_use]
    pub fn ordered_ready(&self) -> bool { self.channels.ordered.is_some() }

    /// Try to send a message that must arrive, reporting whether it went out.
    ///
    /// A `false` means the message was lost; nothing is retried here and the caller decides what that is worth.
    pub fn notify_ordered(&mut self, message: ServerOrderedMessage) -> bool {
        self.send_on(self.channels.ordered, &message)
    }

    /// Try to send a message that is free to go missing, reporting whether it went out.
    ///
    /// Losing one is not worth repairing, which is what the channel is for, but the caller still has to know: a send
    /// recorded as having happened is one the next round will not retry.
    pub fn notify_unordered(&mut self, message: &ServerUnorderedMessage) -> bool {
        self.send_on(self.channels.unordered, message)
    }

    /// Write one JSON message to a channel, reporting whether the peer took it.
    fn send_on(&mut self, channel_id: Option<ChannelId>, message: &impl Serialize) -> bool {
        let Some(channel_id) = channel_id else {
            return false;
        };

        let mut writer = Cursor::new(&mut self.send_on_buffer);

        if let Err(err) = serde_json::to_writer(&mut writer, message) {
            error!(?err, "failed to encode a channel message");
            return false;
        }

        let length = writer.position() as usize;
        let data = &self.send_on_buffer[..length];

        let Some(mut channel) = self.rtc.channel(channel_id) else {
            debug!(?channel_id, "the data channel is gone");
            return false;
        };

        match channel.write(false, data) {
            Ok(true) => true,
            // str0m refuses a write it cannot take whole rather than buffering part of it.
            Ok(false) => {
                warn!(
                    ?channel_id,
                    len = data.len(),
                    "the data channel had no room for a message"
                );
                false
            },
            Err(err) => {
                warn!(?channel_id, ?err, "the data channel write failed");
                false
            },
        }
    }

    /// Every speaker this peer currently holds a slot for, with the m-line carrying them.
    pub fn audible(&self) -> impl Iterator<Item = (SessionId, Mid)> {
        self.slots.assignments().map(|(speaker, mid)| (speaker, *mid))
    }

    /// Whether this peer should be told `speakers`, which must be in session order.
    #[must_use]
    pub fn needs_positions(&self, speakers: &[Audible], now: Instant) -> bool { self.positions.needs(speakers, now) }

    /// Record the set this peer has just been told about.
    pub fn remember_positions(&mut self, speakers: Vec<Audible>, now: Instant) { self.positions.record(speakers, now); }

    /// Record a media slot the remote peer offered us.
    ///
    /// Only remotely added media raises this; slots the server adds itself arrive through [`Peer::accept_answer`].
    pub fn on_media_added(&mut self, mid: Mid, kind: MediaKind, direction: Direction) {
        if kind == MediaKind::Audio && direction.is_sending() {
            self.slots.add_negotiated([mid]);
        }
    }

    /// Relay one speaker's frame to this peer.
    ///
    /// `now` is the instant this drain pass started, which is what marks the speaker as still being heard.
    pub fn relay(&mut self, speaker: SessionId, data: &MediaData, now: Instant) -> Relay {
        if !self.rtc.is_connected() {
            return Relay::NotConnected;
        }

        let (mid, reclaimed) = match self.slots.slot_for(speaker, now) {
            Grant::Ready(mid) => (mid, false),
            Grant::Reclaimed(mid) => (mid, true),
            Grant::Denied => {
                // Only ask for a renegotiation that can actually happen: at the ceiling there is nothing to add, and
                // asking anyway would put this peer through a full drain for every frame it cannot carry.
                self.wants_slots = self.slots.growth_target() > 0;
                return if self.wants_slots {
                    Relay::NoSlot
                } else {
                    Relay::Unheard
                };
            },
        };

        // The slot may have carried a different speaker a moment ago, whose RTP
        // clock has no relation to this one's. Map onto the slot's own clock.
        let timeline = self.timelines.entry(mid).or_default();

        if reclaimed {
            // Taking the slot is a speaker change like any other; the clock survives it, the source does not.
            timeline.release();
        }

        let emit = timeline.map(speaker, data.time.numer());

        let Some(writer) = self.rtc.writer(mid) else {
            return Relay::Rejected;
        };
        let Some(pt) = writer.match_params(data.params) else {
            return Relay::Rejected;
        };

        let time = MediaTime::new(emit.timestamp, data.time.frequency());
        let started = emit.start_of_talkspurt || data.audio_start_of_talk_spurt;

        // The arrival time is the wallclock str0m recommends for an SFU, and it
        // is never in the future, which sender reports require.
        match writer
            .start_of_talkspurt(started)
            .write(pt, data.network_time, time, data.data.clone())
        {
            Ok(()) => Relay::Written,
            Err(_) => Relay::Rejected,
        }
    }

    /// Hand back the slot a disconnected speaker was using.
    ///
    /// Disconnection is what reaches this. A speaker who merely goes quiet keeps
    /// their slot until somebody else needs one, which [`Peer::relay`] handles
    /// through [`Grant::Reclaimed`] and which ends in the same clock reset.
    pub fn release_speaker(&mut self, speaker: SessionId) {
        if let Some(mid) = self.slots.release(speaker)
            && let Some(timeline) = self.timelines.get_mut(&mid)
        {
            // Clearing the source, but not the clock, makes the next speaker on
            // this slot start a fresh talkspurt without rewinding time.
            timeline.release();
        }
    }

    /// Produce an offer adding more outgoing slots, if any are wanted.
    ///
    /// Returns `None` when no growth is needed, when one is already in flight,
    /// or when the slot ceiling has been reached.
    pub fn take_offer(&mut self) -> Option<String> {
        if !self.wants_slots || self.negotiation.is_some() {
            return None;
        }

        let count = self.slots.growth_target();

        if count == 0 {
            // at the ceiling: stop asking, and let excess speakers go unheard.
            self.wants_slots = false;
            return None;
        }

        let mut changes = self.rtc.sdp_api();

        let mids = (0..count)
            .map(|_| changes.add_media(MediaKind::Audio, Direction::SendOnly, None, None, None))
            .collect::<Vec<_>>();

        let (offer, pending) = changes.apply()?;

        self.negotiation = Some(Negotiation { pending, mids });
        self.wants_slots = false;

        Some(offer.to_sdp_string())
    }

    /// Apply the browser's answer to the offer in flight.
    pub fn accept_answer(&mut self, sdp: &str) -> Result<(), AnswerError> {
        let negotiation = self.negotiation.take().ok_or(AnswerError::Unexpected)?;
        let answer = SdpAnswer::from_sdp_string(sdp).map_err(|_| AnswerError::Malformed)?;

        self.rtc
            .sdp_api()
            .accept_answer(negotiation.pending, answer)
            .map_err(|_| AnswerError::Rejected)?;

        self.slots.add_negotiated(negotiation.mids);

        // A speaker may have gone unheard while the offer was in flight.
        self.wants_slots = self.slots.is_exhausted();

        Ok(())
    }

    /// Start capturing this peer's incoming audio to a file.
    #[cfg(feature = "audio_dump")]
    pub fn enable_capture(&mut self, session: SessionId) { self.sink = Some(AudioSink::new(session)); }

    /// Record a frame this peer sent, for debugging.
    #[cfg(feature = "audio_dump")]
    pub fn capture(&mut self, data: &MediaData) {
        if let Some(sink) = &mut self.sink {
            sink.handle_frame(data);
        }
    }
}

/// Why an answer could not be applied.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum AnswerError {
    /// No offer was in flight.
    #[error("no offer was awaiting an answer")]
    Unexpected,
    /// The SDP could not be parsed.
    #[error("the answer was not valid SDP")]
    Malformed,
    /// str0m refused the answer.
    #[error("the answer did not match the offer")]
    Rejected,
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use sada_common::SessionId;
    use str0m::media::Mid;

    use super::{Audible, POSITION_REFRESH, PositionState};
    use crate::proto::Offset;

    /// One audible speaker, placed where the arguments say.
    fn audible(session: u32, mid: &str, offset: Option<(i32, i32)>) -> Audible {
        Audible {
            session: SessionId::new(session, 1),
            mid: Mid::from(mid),
            offset: offset.map(|(x, y)| Offset { x, y }),
        }
    }

    #[test]
    fn the_first_round_is_always_sent() {
        let state = PositionState::default();

        // Even the empty set, which is what a peer hearing nobody would be told, and which matches the state it
        // starts out holding.
        assert!(state.needs(&[], Instant::now()));
    }

    #[test]
    fn an_unchanged_set_is_not_repeated() {
        let now = Instant::now();
        let speakers = vec![audible(1, "0", Some((3, 4)))];
        let mut state = PositionState::default();

        state.record(speakers.clone(), now);

        assert!(!state.needs(&speakers, now));
    }

    #[test]
    fn a_moved_speaker_is_sent_again() {
        let now = Instant::now();
        let mut state = PositionState::default();

        state.record(vec![audible(1, "0", Some((3, 4)))], now);

        assert!(state.needs(&[audible(1, "0", Some((3, 5)))], now));
    }

    #[test]
    fn a_speaker_who_became_audible_is_sent_again() {
        let now = Instant::now();
        let mut state = PositionState::default();

        state.record(vec![audible(1, "0", None)], now);

        assert!(state.needs(&[audible(1, "0", None), audible(2, "1", None)], now));
    }

    #[test]
    fn a_speaker_who_went_away_is_sent_again() {
        let now = Instant::now();
        let mut state = PositionState::default();

        state.record(vec![audible(1, "0", None)], now);

        // The emptying is the whole message: a listener never told would go on placing a voice that has stopped.
        assert!(state.needs(&[], now));
    }

    #[test]
    fn an_unchanged_set_is_repeated_after_the_refresh_interval() {
        let now = Instant::now();
        let speakers = vec![audible(1, "0", Some((3, 4)))];
        let mut state = PositionState::default();

        state.record(speakers.clone(), now);

        assert!(!state.needs(&speakers, now + POSITION_REFRESH - Duration::from_millis(1)));
        assert!(state.needs(&speakers, now + POSITION_REFRESH));
    }

    #[test]
    fn the_same_speaker_on_a_different_slot_is_sent_again() {
        let now = Instant::now();
        let mut state = PositionState::default();

        state.record(vec![audible(1, "0", None)], now);

        // A reclaimed slot moves the speaker to another m-line, and the browser tracks them by that.
        assert!(state.needs(&[audible(1, "1", None)], now));
    }
}
