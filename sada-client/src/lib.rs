#![feature(macro_attr)]

//! A bridge library between game server and VC server.

mod byond;
mod control;

use meowtonin::ByondValue;
use sada_common::ControlResponse;

/// Encodes a [`ControlResponse`] into a JSON string. If encoding fails, returns an error message as JSON string.
fn encode_response(response: ControlResponse) -> String {
    serde_json::to_string(&response).unwrap_or_else(|err| {
        serde_json::json!({
            "error": {
                "message": format!("failed to encode response: {err}"),
            },
        })
        .to_string()
    })
}

#[byond::function]
fn get_version() -> &str { env!("CARGO_PKG_VERSION") }

#[byond::function]
fn init(path: &str) -> String { encode_response(control::init(path)) }

/// Registers an authentication code.
#[byond::function]
fn register_code(code: &str, ckey: &str) -> String {
    match control::register_code(code, ckey) {
        ControlResponse::Ok => "ok".to_owned(),
        ControlResponse::Error { message } => message,
        other => format!("unexpected response: {other:?}"),
    }
}

/// Returns the session bound to `ckey` as a decimal string, or `0` if none.
#[byond::function]
fn check_auth(ckey: &str) -> String {
    match control::check_auth(ckey) {
        ControlResponse::Session { session } => session.map_or(0, sada_common::SessionId::as_raw).to_string(),
        _ => "0".to_owned(),
    }
}

/// Starts transmitting. An empty `freq` means local speech.
#[byond::function]
fn set_ptt(session: u64, freq: &str) -> String {
    let channel = if freq.is_empty() { None } else { freq.parse().ok() };
    encode_response(control::set_ptt(session, channel))
}

/// Stops transmitting.
#[byond::function]
fn clear_ptt(session: u64) -> String { encode_response(control::clear_ptt(session)) }

#[byond::function]
fn echo(arg: &str) -> &str { arg }

#[byond::function]
fn void() {}

#[byond::function]
fn panicing() -> i32 {
    panic!("This function panics!");
}

#[byond::byondapi]
fn echo_bapi(arg: String) -> String { arg }

#[byond::byondapi]
fn panicing_bapi() -> i32 {
    panic!("This function panics too!");
}

#[byond::byondapi]
fn update_position(mob: ByondValue, x: i32, y: i32) {
    let Ok(name) = mob.read_var::<_, String>("name") else {
        return;
    };

    println!("Updating position of {} to ({}, {})", name, x, y);
}
