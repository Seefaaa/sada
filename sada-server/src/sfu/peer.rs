//! One connected browser, and everything the worker knows about it.

use std::{collections::HashMap, time::Instant};

use sada_common::{Ckey, SessionId, Transmit};
use str0m::{
    Rtc,
    change::{SdpAnswer, SdpPendingOffer},
    media::{Direction, MediaData, MediaKind, MediaTime, Mid},
};
use thiserror::Error;
use tokio::sync::mpsc;

#[cfg(feature = "audio_dump")]
use crate::audio::AudioSink;
use crate::{
    proto::ServerMessage,
    sfu::{
        slots::{Grant, SlotTable},
        timeline::SlotTimeline,
    },
};

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
        }
    }

    /// Whether this peer's audio should currently reach anyone.
    #[must_use]
    pub fn is_transmitting(&self) -> bool { !self.self_muted && self.transmit.is_some() }

    /// Try to send a signaling message, reporting whether the socket is still there.
    pub fn notify(&self, message: ServerMessage) -> bool { self.signal.try_send(message).is_ok() }

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
    /// `now` is the instant this drain pass started; it is what tells the slot table this speaker is still being
    /// heard, and a few milliseconds of staleness is nothing against the idle threshold it is compared with.
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
