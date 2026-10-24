//! Identity newtypes shared by the game bridge, the server and the browser client.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Opaque handle for one connected voice session.
///
/// Allocated by the server when a browser completes the signaling handshake, an handed to the game so it can address a
/// player's voice state (push-to-talk mute) without knowing anything about WebRTC.
///
/// The value packs a storage slot and a generation counter. Slots are recycled when a session ends, so without the
/// generation a stale id held by the game would silently start addressing whoever took the slot over. Bumping the
/// generation on reuse makes a stale id fail to resolve instead.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[serde(transparent)]
pub struct SessionId(u64);

impl SessionId {
    /// Build a session id addressing `slot` in its `generation`.
    ///
    /// Generations start at 1, so a valid id is never zero and zero stays available as the "no session" sentinel the DM
    /// side expects.
    #[must_use]
    pub const fn new(slot: u32, generation: u32) -> Self { Self((generation as u64) << 32 | slot as u64) }

    /// Build a session id from its raw representation.
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self { Self(raw) }

    /// Storage slot this id addresses.
    #[must_use]
    pub const fn slot(self) -> u32 { self.0 as u32 }

    /// Generation of the session occupying the slot.
    #[must_use]
    pub const fn generation(self) -> u32 { (self.0 >> 32) as u32 }

    /// Raw representation, for wire formats that cannot carry a newtype.
    #[must_use]
    pub const fn as_raw(self) -> u64 { self.0 }

    /// The token the game carries this id as.
    ///
    /// DM numbers are single-precision floats and this is a 64-bit value, so it travels as a decimal string that DM
    /// only ever hands back.
    #[must_use]
    pub fn token(self) -> String { self.0.to_string() }

    /// Read back a token, or `None` if the game handed over something that is not one.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> { token.parse().map(Self::from_raw).ok() }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { format!("{}.{}", self.slot(), self.generation()).fmt(f) }
}

/// A BYOND player key, the game's canonical player identity.
#[derive(Clone, Debug, Serialize, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[serde(transparent)]
pub struct Ckey(String);

impl Ckey {
    /// Wrap a player key.
    #[must_use]
    pub const fn new(key: String) -> Self { Self(key) }
}

impl fmt::Display for Ckey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { self.0.fmt(f) }
}

impl From<String> for Ckey {
    fn from(key: String) -> Self { Self::new(key) }
}

impl From<&str> for Ckey {
    fn from(key: &str) -> Self { Self::new(key.to_owned()) }
}

/// A player, as the game names them to the voice server.
///
/// The game assigns it and the server only ever compares it; nothing here derives one, checks one against a ckey, or
/// reads any meaning into the number. Two players the game gives the same id are one player as far as the server can
/// tell, so keeping ids apart is the game's side of the contract.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[serde(transparent)]
pub struct PlayerId(u64);

impl PlayerId {
    /// Build an id from its raw representation.
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self { Self(raw) }

    /// Raw representation, for wire formats that cannot carry a newtype.
    #[must_use]
    pub const fn as_raw(self) -> u64 { self.0 }

    /// The three numbers DM carries this id as: bits 40..64, 16..40 and 0..16.
    ///
    /// None is wider than 24 bits, because DM numbers are single-precision floats and exact only to 2^24.
    #[must_use]
    pub const fn to_chunks(self) -> [u32; 3] {
        [
            (self.0 >> 40) as u32,
            ((self.0 >> 16) & 0x00FF_FFFF) as u32,
            (self.0 & 0xFFFF) as u32,
        ]
    }

    /// Put an id back together from [`PlayerId::to_chunks`].
    #[must_use]
    pub const fn from_chunks([high, middle, low]: [u32; 3]) -> Self {
        Self((high as u64) << 40 | (middle as u64) << 16 | low as u64)
    }

    /// The id as 16 hex digits, which is what DM compares ids and keys lists by.
    #[must_use]
    pub fn key(self) -> String { format!("{:016x}", self.0) }

    /// Convert the player ID to a BYOND datum (reference-counted, free after use).
    ///
    /// The datum is a `/datum/sada_player_id` built from [`PlayerId::to_chunks`] and [`PlayerId::key`].
    #[cfg(feature = "byondapi")]
    #[must_use]
    pub fn into_datum(self) -> sada_byondapi::sys::CByondValue {
        use sada_byondapi::{byond, sys::CByondValue};

        let [high, middle, low] = self.to_chunks().map(|chunk| CByondValue::from(chunk as f32));
        let key = CByondValue::from(self.key());

        let value = byond::new(c"/datum/sada_player_id", &[high, middle, low, key]);

        byond::value_decref(&key);

        value
    }
}

#[cfg(feature = "byondapi")]
impl TryFrom<sada_byondapi::sys::CByondValue> for PlayerId {
    type Error = String;

    /// Read back a `/datum/sada_player_id`, whose `chunk1`, `chunk2` and `chunk3` hold [`PlayerId::to_chunks`].
    ///
    /// Refused, naming what DM handed over, when one of them is missing.
    fn try_from(value: sada_byondapi::sys::CByondValue) -> Result<Self, Self::Error> {
        use sada_byondapi::byond;

        let chunk = |var| byond::read_var(&value, var).map(|chunk| unsafe { chunk.data.num } as u32);

        match (chunk(c"chunk1"), chunk(c"chunk2"), chunk(c"chunk3")) {
            (Some(high), Some(middle), Some(low)) => Ok(Self::from_chunks([high, middle, low])),
            _ => Err(format!("{:?} is not a player id", byond::to_string(&value))),
        }
    }
}

impl fmt::Display for PlayerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { self.0.fmt(f) }
}

/// A single-use code the game mints so a browser can prove which player it is.
///
/// The game generates the code, registers it with the server, and shows it to
/// the player, who types it into the web client.
#[derive(Clone, Debug, Serialize, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[serde(transparent)]
pub struct AuthCode(String);

impl AuthCode {
    /// Wrap a code minted by the game.
    #[must_use]
    pub const fn new(code: String) -> Self { Self(code) }

    /// Borrow the underlying code.
    #[must_use]
    pub fn as_str(&self) -> &str { &self.0 }
}

impl From<String> for AuthCode {
    fn from(code: String) -> Self { Self::new(code) }
}

impl From<&str> for AuthCode {
    fn from(code: &str) -> Self { Self::new(code.to_owned()) }
}

#[cfg(test)]
mod tests {
    use super::{PlayerId, SessionId};

    #[test]
    fn a_player_id_round_trips_through_its_raw_form() {
        let id = PlayerId::from_raw(0xfef1_9f30);
        assert_eq!(PlayerId::from_raw(id.as_raw()), id);
    }

    #[test]
    fn a_player_id_round_trips_through_its_chunks() {
        for raw in [0, 1, 0x0000_0100_0000_0001, 0x0a24_ad61_c256_2a55, u64::MAX] {
            let id = PlayerId::from_raw(raw);
            let [high, middle, low] = id.to_chunks();

            assert!(
                high < 1 << 24 && middle < 1 << 24 && low < 1 << 16,
                "{raw:#x} needs a float to round"
            );
            assert_eq!(PlayerId::from_chunks(id.to_chunks()), id, "{raw:#x}");
        }
    }

    #[test]
    fn a_player_key_is_always_sixteen_digits() {
        assert_eq!(PlayerId::from_raw(1).key(), "0000000000000001");
        assert_eq!(PlayerId::from_raw(0x0a24_ad61_c256_2a55).key(), "0a24ad61c2562a55");
        assert_eq!(PlayerId::from_raw(u64::MAX).key(), "ffffffffffffffff");
    }

    #[test]
    fn slot_and_generation_round_trip() {
        let id = SessionId::new(7, 3);
        assert_eq!(id.slot(), 7);
        assert_eq!(id.generation(), 3);
        assert_eq!(SessionId::from_raw(id.as_raw()), id);
    }

    #[test]
    fn a_session_token_round_trips_and_is_not_the_display_form() {
        // DM numbers are single-precision floats. This id past 2^24 arrives mangled as a number, and as the Display
        // form it does not read back at all, so every transmit built from it would name the wrong session or none.
        let session = SessionId::new(7, 1);
        assert!(session.as_raw() > 1 << 24, "the trap only bites past 2^24");

        assert_eq!(SessionId::from_token(&session.token()), Some(session));
        assert_ne!(session.token(), session.to_string(), "Display is for people to read");
        assert_eq!(SessionId::from_token(&session.to_string()), None);
    }

    #[test]
    fn a_recycled_slot_yields_a_different_id() {
        // The whole point of the generation: the same storage slot handed out
        // again must not be addressable by the previous session's id.
        assert_ne!(SessionId::new(7, 1), SessionId::new(7, 2));
    }

    #[test]
    fn extreme_values_do_not_bleed_between_fields() {
        let id = SessionId::new(u32::MAX, u32::MAX);
        assert_eq!(id.slot(), u32::MAX);
        assert_eq!(id.generation(), u32::MAX);
    }

    #[test]
    fn a_valid_id_is_never_zero() {
        assert_ne!(SessionId::new(0, 1).as_raw(), 0);
    }
}
