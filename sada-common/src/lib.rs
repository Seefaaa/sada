//! Shared types and utilities for the Sada workspace.
//!
//! Everything here is on the wire between the BYOND game server, the voice server and the browser, so changes are
//! breaking by definition: the game bridge is a 32-bit shared library built from the same source tree and must be
//! rebuilt alongside the server.

mod control;
mod frame;
mod ids;

pub use crate::{
    control::{ControlEvent, ControlRequest, ControlResponse, Freq, PROTOCOL_VERSION, PlayerPatch, Position, Transmit},
    frame::{ControlFrameBuffer, Error, MAX_CONTROL_FRAME_LEN, Result},
    ids::{AuthCode, Ckey, SessionId},
};
