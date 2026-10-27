//! A bridge library between game server and VC server.

#![feature(macro_attr)]
#![forbid(unsafe_code)]

mod control;
mod player;

use sada_byondapi::{byond, byond_fn, sys::CByondValue};
use sada_common::{ControlRequest, ControlResponse, PlayerId, SessionId};

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

/// Returns the player id for `ckey` as 16 hex digits, or null when another ckey already holds the id it derives.
#[byond_fn]
fn player_id(ckey: String) -> Option<PlayerId> { player::issue(&ckey) }

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

/// Takes up to `max` of the events the server has pushed, as a list of them, or null when there are none.
///
/// Nothing is asked of the server here: events arrive on their own and wait in the client until the game takes them.
#[byond_fn]
fn take_events(max: u16) -> Option<CByondValue> {
    let events = control::take_events(max);
    (!events.is_empty()).then(|| byond::list_from(events))
}

/// One of every event, each field telling itself apart from the others, for the harness to check against the layout
/// `dmsrc/sada.dm` gives each tag.
///
/// Debug builds only: the harness loads one, and a release build has no use for it.
#[cfg(debug_assertions)]
#[byond_fn]
fn sample_events() -> CByondValue {
    use sada_common::ControlEvent;

    byond::list_from(vec![
        ControlEvent::Authenticated {
            player: PlayerId::from_raw(1),
            session: SessionId::from_raw(11),
        },
        ControlEvent::Synchronized {
            players: vec![PlayerId::from_raw(2), PlayerId::from_raw(3)],
        },
        ControlEvent::Disconnected {
            player: PlayerId::from_raw(4),
            session: SessionId::from_raw(12),
        },
        ControlEvent::Speaking {
            speaker: PlayerId::from_raw(5),
            listeners: vec![PlayerId::from_raw(6), PlayerId::from_raw(7)],
        },
        ControlEvent::Heard {
            speaker: PlayerId::from_raw(8),
            listener: PlayerId::from_raw(9),
            channel: Some(sada_common::Freq(1459)),
            language: Some("common".to_owned()),
        },
        ControlEvent::Heard {
            speaker: PlayerId::from_raw(10),
            listener: PlayerId::from_raw(11),
            channel: None,
            language: None,
        },
    ])
}
