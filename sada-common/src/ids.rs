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
    use super::SessionId;

    #[test]
    fn slot_and_generation_round_trip() {
        let id = SessionId::new(7, 3);
        assert_eq!(id.slot(), 7);
        assert_eq!(id.generation(), 3);
        assert_eq!(SessionId::from_raw(id.as_raw()), id);
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
