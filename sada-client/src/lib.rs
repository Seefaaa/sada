#![feature(macro_attr)]

//! A bridge library between game server and VC server.

mod control;
mod dm;

use sada_byondapi::byond_fn;

use crate::{
    control::{Poll, Ticket},
    dm::encode_response,
};

/// Answer to a poll for a response that has not arrived yet.
const PENDING: &str = "pending";

/// Answer to a poll for a ticket that was never issued, has already been collected, or went stale.
const UNKNOWN: &str = "unknown";

/// Returns the version of this library.
#[byond_fn]
fn get_version() -> String { env!("CARGO_PKG_VERSION").to_string() }

/// Starts the control worker and asks the server for its version.
///
/// Returns the ticket that version answer arrives under, or `0` if the worker could not be started. Nothing here
/// touches the socket, so a server that is not up yet shows as an error on that ticket.
#[byond_fn]
fn init(path: String) -> Ticket { control::init(&path) }

/// Stops the control worker and forgets every outstanding ticket.
#[byond_fn]
fn stop() { control::stop() }

/// Collects the response for a ticket.
///
/// Returns `pending` while the request is still with the worker, `unknown` for a ticket that was never issued or has
/// already been collected, and the JSON-encoded response otherwise.
#[byond_fn]
fn poll_ticket(ticket: Ticket) -> String {
    match control::poll(ticket) {
        Poll::Ready(response) => encode_response(response),
        Poll::Pending => PENDING.to_owned(),
        Poll::Unknown => UNKNOWN.to_owned(),
    }
}

/// Takes the oldest error no ticket was waiting for, or an empty string when there is none.
#[byond_fn]
fn take_error() -> String { control::take_error().unwrap_or_default() }

/// Registers an authentication code. Returns the ticket its result arrives under.
#[byond_fn]
fn register_code(code: String, ckey: String) -> Ticket { control::register_code(&code, &ckey) }

/// Looks up the session bound to `ckey`. Returns the ticket its result arrives under.
#[byond_fn]
fn check_auth(ckey: String) -> Ticket { control::check_auth(&ckey) }

/// Starts transmitting. An empty `freq` means local speech.
#[byond_fn]
fn set_ptt(session: String, freq: String) {
    let Ok(session) = session.parse() else { return };
    let channel = if freq.is_empty() { None } else { freq.parse().ok() };
    control::set_ptt(session, channel);
}

/// Stops transmitting.
#[byond_fn]
fn clear_ptt(session: String) {
    let Ok(session) = session.parse() else { return };
    control::clear_ptt(session);
}

/// Adds one player's state delta to the batch that the next `flush` sends.
///
/// `patch` is a JSON object of the fields that changed; absent fields mean unchanged. Returns an empty string when
/// the patch was accepted, or the reason it was not. Nothing reaches the socket until `flush`.
#[byond_fn]
fn patch_player(ckey: String, patch: String) -> String {
    control::patch_player(&ckey, &patch).err().unwrap_or_default()
}

/// Sends everything `patch_player` has piled up as one batch.
#[byond_fn]
fn flush() { control::flush() }

/// Forgets a player entirely, dropping their authentication with it.
#[byond_fn]
fn remove_player(ckey: String) { control::remove_player(&ckey) }

/// Asks for up to `max` queued server events. Returns the ticket they arrive under.
#[byond_fn]
fn poll_events(max: u16) -> Ticket { control::poll_events(max) }

#[cfg(test)]
mod tests {
    use sada_common::{ControlEvent, ControlResponse, SessionId};

    use super::*;

    #[test]
    fn a_session_id_reaches_byond_as_a_string() {
        // DM numbers are single-precision floats. As a number this id would arrive as 4294967296 and the slot would be
        // gone, so every set_ptt built from it would name the wrong session.
        let session = SessionId::new(7, 1);
        assert!(session.as_raw() > 1 << 24, "the trap only bites past 2^24");

        let response = encode_response(ControlResponse::Session { session: Some(session) });
        assert_eq!(
            response,
            format!(r#"{{"session":{{"session":"{}"}}}}"#, session.as_raw())
        );

        let events = encode_response(ControlResponse::Events(vec![ControlEvent::Authenticated {
            ckey: "sefa".into(),
            session,
        }]));
        assert!(
            events.contains(&format!(r#""session":"{}""#, session.as_raw())),
            "got {events}"
        );
    }

    #[test]
    fn an_unauthenticated_player_has_no_session() {
        assert_eq!(
            encode_response(ControlResponse::Session { session: None }),
            r#"{"session":{"session":null}}"#
        );
    }
}
