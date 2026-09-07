//! Rewriting of RTP timing when one outgoing stream carries successive speakers.
//!
//! An outgoing media slot is reused: when a speaker leaves, the next one to become audible inherits the same m-line,
//! and therefore the same SSRC and the same sequence number space. str0m passes the *source's* RTP timestamp through
//! verbatim, so without intervention the timestamp on that one stream would jump by an arbitrary amount at every
//! speaker change. A receiver reads that as a timestamp discontinuity and flushes its jitter buffer, costing a few
//! hundred milliseconds of audio.
//!
//! This keeps a monotonic output clock per slot, advancing it by the *delta* observed on the input, so the outgoing
//! stream stays continuous no matter how many speakers pass through it.

use sada_common::SessionId;

/// Samples in one Opus frame at 48 kHz, i.e. 20 ms.
///
/// Used as the gap inserted at a speaker change, and as the basis for clamping implausible input jumps.
const FRAME_SAMPLES: u64 = 960;

/// Largest input gap forwarded as-is, in samples.
///
/// Anything longer is a silence gap, a stream restart or a corrupt timestamp; collapsing it to a single frame keeps the
/// output clock close to real time instead of jumping minutes ahead.
const MAX_GAP_SAMPLES: u64 = FRAME_SAMPLES * 50;

/// Monotonic output clock for one outgoing media slot.
#[derive(Debug, Default)]
pub struct SlotTimeline {
    /// Speaker the current input timeline belongs to.
    source: Option<SessionId>,
    /// Last input timestamp seen from that speaker.
    last_input: u64,
    /// Last timestamp written out on this slot, absent until the first frame.
    ///
    /// This is distinct from `source` being absent: releasing a slot clears the speaker but must not restart the
    /// clock, or the next speaker would rewind it back to their own arbitrary starting point.
    last_output: Option<u64>,
}

/// How a frame should be emitted on a slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Emit {
    /// Timestamp to write, in the slot's own continuous clock.
    pub timestamp: u64,
    /// Whether this frame begins a new talkspurt.
    ///
    /// Set at every speaker change so the receiver treats the discontinuity as
    /// a fresh burst of speech rather than as loss.
    pub start_of_talkspurt: bool,
}

impl SlotTimeline {
    /// Map an incoming timestamp onto this slot's output clock.
    pub fn map(&mut self, source: SessionId, input: u64) -> Emit {
        let switched = self.source != Some(source);

        let advance = if switched {
            // A new speaker's clock has no relation to the previous one's, so
            // step by exactly one frame rather than by a meaningless delta.
            FRAME_SAMPLES
        } else {
            input
                .checked_sub(self.last_input)
                .filter(|&delta| delta > 0 && delta <= MAX_GAP_SAMPLES)
                .unwrap_or(FRAME_SAMPLES)
        };

        // The very first frame on a slot starts the clock rather than advancing it.
        let timestamp = match self.last_output {
            Some(previous) => previous.wrapping_add(advance),
            None => input,
        };

        self.last_output = Some(timestamp);
        self.last_input = input;
        self.source = Some(source);

        Emit {
            timestamp,
            start_of_talkspurt: switched,
        }
    }

    /// Forget the current speaker without disturbing the output clock.
    ///
    /// The next frame is then treated as a speaker change, which is what should
    /// happen when the slot is handed to someone else.
    pub fn release(&mut self) { self.source = None; }
}

#[cfg(test)]
mod tests {
    use sada_common::SessionId;

    use super::{FRAME_SAMPLES, MAX_GAP_SAMPLES, SlotTimeline};

    /// Build a session id for tests.
    fn session(raw: u32) -> SessionId { SessionId::new(raw, 1) }

    #[test]
    fn the_first_frame_starts_the_clock() {
        let mut timeline = SlotTimeline::default();

        let emit = timeline.map(session(1), 5_000);

        assert_eq!(emit.timestamp, 5_000);
        assert!(emit.start_of_talkspurt);
    }

    #[test]
    fn a_steady_speaker_advances_by_its_own_delta() {
        let mut timeline = SlotTimeline::default();

        timeline.map(session(1), 5_000);

        assert_eq!(timeline.map(session(1), 5_960).timestamp, 5_960);
        assert_eq!(timeline.map(session(1), 6_920).timestamp, 6_920);
    }

    #[test]
    fn a_speaker_change_advances_by_exactly_one_frame() {
        let mut timeline = SlotTimeline::default();

        timeline.map(session(1), 5_000);

        let before = timeline.map(session(1), 5_960).timestamp;

        // The new speaker's clock is wildly different, but the output must not
        // jump, that is what flushes the receiver's jitter buffer.
        let emit = timeline.map(session(2), 900_000_000);

        assert_eq!(emit.timestamp, before + FRAME_SAMPLES);
        assert!(emit.start_of_talkspurt);
    }

    #[test]
    fn the_output_clock_never_goes_backwards() {
        let mut timeline = SlotTimeline::default();

        timeline.map(session(1), 100_000);

        let first = timeline.map(session(1), 100_960).timestamp;

        // A speaker whose clock starts far lower than the previous one's.
        let second = timeline.map(session(2), 10).timestamp;

        assert!(second > first);
    }

    #[test]
    fn a_repeated_or_reordered_input_still_advances() {
        let mut timeline = SlotTimeline::default();

        timeline.map(session(1), 5_000);

        let first = timeline.map(session(1), 5_960).timestamp;

        // Zero and negative deltas fall back to one frame rather than stalling or rewinding the output clock.
        let repeat = timeline.map(session(1), 5_960).timestamp;
        let backwards = timeline.map(session(1), 4_000).timestamp;

        assert_eq!(repeat, first + FRAME_SAMPLES);
        assert_eq!(backwards, repeat + FRAME_SAMPLES);
    }

    #[test]
    fn an_implausible_jump_is_collapsed() {
        let mut timeline = SlotTimeline::default();

        timeline.map(session(1), 5_000);

        let first = timeline.map(session(1), 5_960).timestamp;

        let jumped = timeline.map(session(1), 5_960 + MAX_GAP_SAMPLES + 1).timestamp;

        assert_eq!(jumped, first + FRAME_SAMPLES);
    }

    #[test]
    fn a_gap_within_the_limit_is_preserved() {
        let mut timeline = SlotTimeline::default();

        timeline.map(session(1), 5_000);

        let first = timeline.map(session(1), 5_960).timestamp;

        // A real silence gap should carry through, so the receiver keeps timing.
        let gap = FRAME_SAMPLES * 10;
        let after = timeline.map(session(1), 5_960 + gap).timestamp;

        assert_eq!(after, first + gap);
    }

    #[test]
    fn releasing_makes_the_next_frame_a_new_talkspurt() {
        let mut timeline = SlotTimeline::default();

        timeline.map(session(1), 5_000);
        timeline.release();

        // Even the same speaker returning counts as a fresh talkspurt, because
        // the slot may have carried someone else in between.
        let emit = timeline.map(session(1), 5_960);
        assert!(emit.start_of_talkspurt);
    }

    #[test]
    fn a_returning_speaker_does_not_rewind_the_clock() {
        let mut timeline = SlotTimeline::default();

        timeline.map(session(1), 500_000);

        let high = timeline.map(session(1), 500_960).timestamp;

        timeline.release();

        let after = timeline.map(session(2), 1_000).timestamp;

        assert_eq!(after, high + FRAME_SAMPLES);
    }
}
