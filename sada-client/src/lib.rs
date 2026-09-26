//! A bridge library between game server and VC server.

#![cfg(target_os = "linux")]
#![feature(macro_attr, const_trait_impl, const_convert)]

mod control;
mod player;

use std::num::NonZeroU16;

use sada_byondapi::{byond, byond_fn, sys::CByondValue};
use sada_common::{PlayerId, PlayerPatch, SessionId};

use crate::control::Poll;

/// A ticket on its way to DM, which sees it as a `/datum/sada_ticket`.
struct Ticket(NonZeroU16);

impl From<Ticket> for CByondValue {
    fn from(value: Ticket) -> Self { byond::new(c"/datum/sada_ticket", &[CByondValue::from(value.0.get())]) }
}

/// Returns the version of this library.
#[byond_fn]
fn get_version() -> String { env!("CARGO_PKG_VERSION").to_string() }

/// Starts the control worker and asks the server for its version.
///
/// Returns the ticket that version answer arrives under, or `0` if the worker could not be started. Nothing here
/// touches the socket, so a server that is not up yet shows as an error on that ticket.
#[byond_fn]
fn init(path: String) -> Result<Ticket, String> { control::init(&path).map(Ticket) }

/// Stops the control worker and forgets every outstanding ticket.
#[byond_fn]
fn stop() { control::stop() }

/// Collects the response for a ticket.
///
/// Returns `pending` while the request is still with the worker, `unknown` for a ticket that was never issued or has
/// already been collected, and the JSON-encoded response otherwise.
#[byond_fn]
fn poll_ticket(ticket: u16) -> CByondValue {
    let Some(ticket) = NonZeroU16::new(ticket) else {
        return CByondValue::NULL;
    };

    match control::poll(ticket) {
        Poll::Ready(response) => response.into(),
        Poll::Pending => const { 1u16.into() },
        Poll::Unknown => CByondValue::NULL,
    }
}

/// Takes the oldest error no ticket was waiting for, or null when there is none.
#[byond_fn]
fn take_error() -> Option<String> { control::take_error() }

/// Returns the player id token for `ckey`, or null when another ckey already holds the id it derives.
#[byond_fn]
fn player_id(ckey: String) -> Option<String> { player::issue(&ckey).map(PlayerId::token) }

/// Registers an authentication code for `player`, greeting the browser that redeems it as `ckey`.
#[byond_fn]
fn register_code(code: String, player: String, ckey: String) -> Result<Ticket, String> {
    control::register_code(&code, &player, &ckey).map(Ticket)
}

/// Looks up the session bound to `player`.
#[byond_fn]
fn check_auth(player: String) -> Result<Ticket, String> { control::check_auth(&player).map(Ticket) }

/// Starts transmitting. An empty `freq` means local speech.
#[byond_fn]
fn start_transmitting(session: String, freq: CByondValue) {
    let Some(session) = SessionId::from_token(&session) else {
        return;
    };
    let channel = (freq != CByondValue::NULL).then(|| freq.into());
    control::start_transmitting(session, channel);
}

/// Stops transmitting.
#[byond_fn]
fn stop_transmitting(session: String) {
    let Some(session) = SessionId::from_token(&session) else {
        return;
    };
    control::stop_transmitting(session);
}

/// Adds one player's state delta to the batch that the next `flush` sends.
///
/// `patch` is an assoc list of the fields that changed; absent fields mean unchanged. Returns an empty string when
/// the patch was accepted, or the reason it was not. Nothing reaches the socket until `flush`.
#[byond_fn]
fn patch_player(player: String, patch: CByondValue) -> String {
    match PlayerPatch::try_from(patch) {
        Ok(patch) => control::patch_player(&player, patch).err().unwrap_or_default(),
        Err(refused) => refused,
    }
}

/// Sends everything `patch_player` has piled up as one batch.
#[byond_fn]
fn flush() { control::flush() }

/// Forgets a player entirely, dropping their authentication with it and freeing their id.
#[byond_fn]
fn remove_player(player: String) { control::remove_player(&player) }

/// Asks for up to `max` queued server events. Returns the ticket they arrive under.
#[byond_fn]
fn poll_events(max: u16) -> Result<Ticket, String> { control::poll_events(max).map(Ticket) }

/// Sleeps a second off the game thread and answers afterwards, to exercise the async export shape.
#[cfg(feature = "async")]
#[byond_fn]
async fn async_test() -> CByondValue {
    use std::time::Duration;

    use tokio::time::sleep;

    println!("async test 1");

    sleep(Duration::from_secs(1)).await;

    println!("async test 2");

    let ret = byond::sync::with_main(|| CByondValue::from("async test done".to_string())).await;

    println!("async test 3");

    ret
}
