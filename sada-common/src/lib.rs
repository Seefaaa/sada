#![feature(derive_const, const_default, const_trait_impl)]

//! Shared types and utilities for the Sada workspace.
//!
//! Everything here is on the wire between the BYOND game server, the voice server and the browser, so changes are
//! breaking by definition: the game bridge is a 32-bit shared library built from the same source tree and must be
//! rebuilt alongside the server.

mod control;
mod ids;

pub use crate::{
    control::{
        ControlEvent,
        ControlMessage,
        ControlRequest,
        ControlResponse,
        Freq,
        PROTOCOL_VERSION,
        PlayerPatch,
        Position,
        Transmit,
    },
    ids::{AuthCode, Ckey, PlayerId, SessionId},
};
