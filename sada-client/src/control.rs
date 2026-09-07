//! Unix socket control client used by exported functions.

use std::{cell::RefCell, os::unix::net::UnixStream, time::Duration};

use sada_common::{AuthCode, Ckey, ControlFrameBuffer, ControlRequest, ControlResponse, Freq, SessionId};

thread_local! {
    /// Connected control socket state.
    static CONTROL: RefCell<Option<ControlState>> = const { RefCell::new(None) };
}

/// Persistent control connection state.
struct ControlState {
    /// Connected Unix socket.
    stream: UnixStream,
    /// Reused frame payload buffer.
    buffer: ControlFrameBuffer,
}

impl ControlState {
    /// Create control state for `stream`.
    fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            buffer: ControlFrameBuffer::new(),
        }
    }

    /// Perform the wire-format request/response exchange.
    fn request(&mut self, request: &ControlRequest) -> Result<ControlResponse, Box<dyn std::error::Error>> {
        self.buffer.write(&mut self.stream, request)?;
        match self.buffer.read(&mut self.stream) {
            Ok(Some(response)) => Ok(response),
            Ok(None) => Err("unexpected EOF while reading control response".into()),
            Err(err) => Err(format!("failed to read control response: {err}").into()),
        }
    }
}

/// Send one request and read one response over the persistent socket.
fn request(request: ControlRequest) -> ControlResponse {
    CONTROL.with_borrow_mut(|slot| {
        let Some(control) = slot.as_mut() else {
            return ControlResponse::Error {
                message: "sada client is not initialized".to_owned(),
            };
        };

        match control.request(&request) {
            Ok(response) => response,
            Err(err) => {
                *slot = None;
                ControlResponse::Error {
                    message: err.to_string(),
                }
            },
        }
    })
}

/// Connect to the server control socket and return its version.
pub fn init(path: &str) -> ControlResponse {
    match UnixStream::connect(path) {
        Ok(stream) => {
            let _ = stream.set_read_timeout(Some(Duration::from_micros(1)));
            let _ = stream.set_write_timeout(Some(Duration::from_micros(1)));
            CONTROL.set(Some(ControlState::new(stream)));
            request(ControlRequest::Version)
        },
        Err(err) => ControlResponse::Error {
            message: format!("failed to connect to {path}: {err}"),
        },
    }
}

/// Register a single-use code the game has shown to a player.
pub fn register_code(code: &str, ckey: &str) -> ControlResponse {
    request(ControlRequest::RegisterCode {
        code: AuthCode::from(code),
        ckey: Ckey::from(ckey),
    })
}

/// Look up the session a player has authenticated, if any.
pub fn check_auth(ckey: &str) -> ControlResponse { request(ControlRequest::CheckAuth { ckey: Ckey::from(ckey) }) }

/// Start transmitting, either locally or on a radio frequency.
///
/// `channel` is the raw frequency, or `None` for local speech.
pub fn set_ptt(session: u64, channel: Option<u32>) -> ControlResponse {
    request(ControlRequest::SetPtt {
        session: SessionId::from_raw(session),
        channel: channel.map(Freq),
    })
}

/// Stop transmitting.
pub fn clear_ptt(session: u64) -> ControlResponse {
    request(ControlRequest::ClearPtt {
        session: SessionId::from_raw(session),
    })
}
