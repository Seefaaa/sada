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

    /// The token the game carries this id as, 16 lowercase hex digits; the generation's eight, then the slot's.
    ///
    /// DM numbers are single-precision floats and this is a 64-bit value, so it travels as a string that DM only ever
    /// hands back.
    #[must_use]
    pub fn token(self) -> String { format!("{:016x}", self.0) }

    /// Read back a token from [`SessionId::token`], refusing anything not spelled exactly that way.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> { read_hex(token).map(Self) }
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

    /// The id as 16 lowercase hex digits, which is how DM carries it.
    #[must_use]
    pub fn key(self) -> String { self.to_string() }

    /// Read back an id from [`PlayerId::key`], refusing anything not spelled exactly that way.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> { read_hex(key).map(Self) }
}

/// Read a 64-bit value spelled as exactly 16 lowercase hex digits, the way DM carries both ids.
fn read_hex(text: &str) -> Option<u64> {
    let spelled = text.len() == 16 && text.bytes().all(|digit| matches!(digit, b'0'..=b'9' | b'a'..=b'f'));
    spelled.then(|| u64::from_str_radix(text, 16).ok()).flatten()
}

#[cfg(feature = "byondapi")]
impl From<PlayerId> for sada_byondapi::sys::CByondValue {
    /// The id as DM carries it, a string of [`PlayerId::key`] (reference-counted, free after use).
    fn from(value: PlayerId) -> Self { Self::from(value.key()) }
}

#[cfg(feature = "byondapi")]
impl TryFrom<sada_byondapi::sys::CByondValue> for PlayerId {
    type Error = String;

    /// Read back the string [`PlayerId::key`] spelled.
    ///
    /// Refused, naming what DM handed over, when it is anything else.
    fn try_from(value: sada_byondapi::sys::CByondValue) -> Result<Self, Self::Error> {
        use sada_byondapi::byond;

        let text = byond::to_string(&value);

        byond::value_isstr(&value)
            .then(|| Self::from_key(&text))
            .flatten()
            .ok_or_else(|| format!("{text:?} is not a player id"))
    }
}

impl fmt::Display for PlayerId {
    /// The id as [`PlayerId::key`] spells it, so logs on either side of the bridge name a player the same way.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{:016x}", self.0) }
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
    fn a_player_id_round_trips_through_its_key() {
        for raw in [0, 1, 0x0000_0100_0000_0001, 0x0a24_ad61_c256_2a55, u64::MAX] {
            let id = PlayerId::from_raw(raw);
            assert_eq!(PlayerId::from_key(&id.key()), Some(id), "{raw:#x}");
        }
    }

    #[test]
    fn only_a_key_spelled_exactly_reads_back() {
        for key in [
            "alpha",
            "",
            "000000000000001",
            "00000000000000001",
            "0A24AD61C2562A55",
            "+000000000000001",
            "0a24ad61c2562a5g",
        ] {
            assert_eq!(PlayerId::from_key(key), None, "{key:?}");
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
    fn a_session_token_is_generation_then_slot_in_sixteen_digits() {
        assert_eq!(SessionId::new(7, 3).token(), "0000000300000007");
        assert_eq!(SessionId::new(u32::MAX, u32::MAX).token(), "ffffffffffffffff");
    }

    #[test]
    fn only_a_token_spelled_exactly_reads_back() {
        for token in [
            "",
            "4294967303",
            "100000007",
            "0000000100000007 ",
            "00000001000000007",
            "000000010000000A",
        ] {
            assert_eq!(SessionId::from_token(token), None, "{token:?}");
        }
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
