//! A bridge library between game server and VC server.

#![feature(macro_attr)]

mod control;
mod player;

use sada_byondapi::{byond, byond_fn, sys::CByondValue};
use sada_common::{ControlEvent, ControlResponse, PlayerId, PlayerPatch, SessionId};

/// Returns the version of this library.
#[byond_fn]
fn get_version() -> String { env!("CARGO_PKG_VERSION").to_string() }

/// Starts the control worker and asks the server for its version.
///
/// Answers with the server's version, or with an error when the worker could not be started or the server could not
/// be reached. The worker is installed before the calling proc goes to sleep. One already running is stopped as by
/// `stop`, and the new one starts only once the old one has finished.
#[byond_fn]
fn init(path: String) -> impl Future<Output = ControlResponse> { control::init(&path) }

/// Stops the control worker.
///
/// A request already on its way is still answered. One still queued is dropped unsent and its waiting proc answered
/// with an error.
#[byond_fn]
fn stop() { control::stop() }

/// Takes the oldest error nobody was waiting for, or null when there is none.
#[byond_fn]
fn take_error() -> Option<String> { control::take_error() }

/// Returns the player id token for `ckey`, or null when another ckey already holds the id it derives.
#[byond_fn]
fn player_id(ckey: String) -> Option<String> { player::issue(&ckey).map(PlayerId::token) }

/// Registers an authentication code for `player`, greeting the browser that redeems it as `ckey`.
#[byond_fn]
fn register_code(code: String, player: String, ckey: String) -> impl Future<Output = ControlResponse> {
    control::register_code(&code, &player, &ckey)
}

/// Looks up the session bound to `player`.
#[byond_fn]
fn check_auth(player: String) -> impl Future<Output = ControlResponse> { control::check_auth(&player) }

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

/// Takes up to `max` of the events the server has pushed, or null when there are none.
///
/// Nothing is asked of the server here: events arrive on their own and wait in the client until the game takes them.
#[byond_fn]
fn take_events(max: u16) -> Option<Events> {
    let events = control::take_events(max);
    (!events.is_empty()).then_some(Events(events))
}

/// Events on their way to DM, which sees them as a `/datum/sada_events`.
struct Events(Vec<ControlEvent>);

impl From<Events> for CByondValue {
    fn from(value: Events) -> Self {
        let events = value.0.into_iter().map(CByondValue::from).collect::<Vec<_>>();
        let value = byond::new(c"/datum/sada_events", &events);
        events.iter().for_each(byond::value_decref);
        value
    }
}
