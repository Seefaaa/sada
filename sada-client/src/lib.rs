//! A bridge library between game server and VC server.

#![feature(macro_attr)]

mod control;
mod player;

use sada_byondapi::{byond, byond_fn, sys::CByondValue};
use sada_common::{ControlEvent, ControlRequest, ControlResponse, PlayerId, SessionId};

/// Returns the version of this library.
#[byond_fn]
fn get_version() -> String { env!("CARGO_PKG_VERSION").to_string() }

/// Starts the control worker and asks the server for its version.
///
/// Answers with the server's version, or with an error when the worker could not be started or the server could not
/// be reached. The worker is installed before the calling proc goes to sleep. One already running is stopped as by
/// `stop`, and the new one starts only once the old one has finished.
#[byond_fn]
fn init(path: String) -> impl Future<Output = ControlResponse> { control::answered(control::init(&path)) }

/// Stops the control worker.
///
/// A request already on its way is still answered. One still queued is dropped unsent and its waiting proc answered
/// with an error.
#[byond_fn]
fn stop() { control::stop() }

/// Takes the oldest error nobody was waiting for, or null when there is none.
#[byond_fn]
fn take_error() -> Option<String> { control::take_error() }

/// Returns the `/datum/sada_player_id` for `ckey`, or null when another ckey already holds the id it derives.
#[byond_fn]
fn player_id(ckey: String) -> Option<CByondValue> { player::issue(&ckey).map(PlayerId::into_datum) }

/// Registers an authentication code for `player`, greeting the browser that redeems it as `ckey`.
#[byond_fn]
fn register_code(code: String, player: CByondValue, ckey: String) -> impl Future<Output = ControlResponse> {
    control::answered(PlayerId::try_from(player).and_then(|player| control::register_code(&code, player, &ckey)))
}

/// Looks up the session bound to `player`.
#[byond_fn]
fn check_auth(player: CByondValue) -> impl Future<Output = ControlResponse> {
    control::answered(PlayerId::try_from(player).and_then(control::check_auth))
}

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

/// Sends the batch DM has built as one frame, answering null when it was handed over or why it was not.
///
/// `entries` is a list of `list(id, patch)` and `list(id)`, a patch and a removal, sent in that order. An entry that
/// cannot be read is left out and leaves an error for `take_error`; the rest still go.
#[byond_fn]
fn send_batch(entries: CByondValue) -> Option<String> {
    if !byond::value_islist(&entries) {
        return Some("a batch is a list".to_owned());
    }

    let entries = byond::read_list(&entries, 64);

    let requests = Vec::from_iter(entries.iter().enumerate().filter_map(|(at, &entry)| {
        ControlRequest::from_batch_entry(entry)
            .map_err(|refused| control::report(format!("send_batch: entry {}: {refused}", at + 1)))
            .ok()
    }));

    entries.iter().for_each(byond::value_decref);

    control::send_batch(requests).err()
}

/// Frees a player's id for another ckey, once the game is done with that player.
///
/// The removal itself travels in the batch. Something that is not a player id leaves an error for `take_error`.
#[byond_fn]
fn release_player_id(player: CByondValue) {
    PlayerId::try_from(player)
        .map(player::forget)
        .unwrap_or_else(|refused| control::report(format!("release_player_id: {refused}")));
}

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
