//! Control channel client used by the exported functions.
//!
//! `call_ext` runs on BYOND's only thread, so nothing on that thread may wait on the channel: a stalled voice server
//! would stall the whole world. A worker thread owns the connection, and a tokio runtime of its own, and performs every
//! syscall; the game thread only hands it jobs and collects whatever has finished. A request whose answer the game
//! wants is queued on the game thread like any other, in order with everything around it, and the export hands back
//! a future for its answer, which the runtime waits on while the calling proc sleeps.
//!
//! The protocol is strictly one response per request and the server answers in order on a single connection, so the
//! worker needs no request ids on the wire: the response it is reading belongs to the job it just wrote. Events are the
//! exception, arriving whenever the server has some, and are queued for the game to take rather than answering
//! anything.
//!
//! The game thread's half is [`client`] and the worker's is [`worker`]. What passes between them is defined here,
//! along with the functions the exports call, which go through the one client installed by [`init`].

mod client;
#[cfg(test)]
mod testing;
mod worker;

use std::{
    cell::RefCell,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

use sada_common::{AuthCode, Ckey, ControlEvent, ControlRequest, ControlResponse, Freq, PlayerId, SessionId, Transmit};
use sada_ipc::Endpoint;
use tokio::sync::oneshot;

use self::client::Client;

thread_local! {
    /// Game-thread half of the control client, installed by [`init`].
    static CONTROL: RefCell<Option<Client>> = const { RefCell::new(None) };

    /// Thread of the last worker [`init`] started, which the next one waits out before it starts.
    static LAST_WORKER: RefCell<Option<thread::JoinHandle<()>>> = const { RefCell::new(None) };
}

/// One request queued for the worker.
struct Job {
    /// Request to send.
    request: ControlRequest,
    /// Whoever waits for its answer, or `None` when nobody does.
    waiter: Option<Waiter>,
}

/// The worker's end of a request somebody waits on.
struct Waiter {
    /// Where the answer goes.
    answer: oneshot::Sender<ControlResponse>,
    /// Taken by whichever side gets to the request first: the worker sending it, or its caller giving up on it.
    claim: Arc<AtomicBool>,
}

/// Something the worker has for the game thread that no waiting request asked for.
enum Reply {
    /// A failure nobody is waiting for: a fire-and-forget request that failed, or the connection going away.
    Failure(String),
    /// Events the server pushed, which answer nothing.
    Events(Vec<ControlEvent>),
}

/// Take `claim` for the side calling this, or say the other side already has it.
fn take(claim: &AtomicBool) -> bool { !claim.swap(true, Ordering::AcqRel) }

/// Build a [`ControlResponse::Error`] from anything printable.
fn error(message: impl Into<String>) -> ControlResponse {
    ControlResponse::Error {
        message: message.into(),
    }
}

/// Run `f` against the installed control client.
///
/// Fails when [`init`] has not been called, which the exports report as an error rather than a panic.
fn with_control<T>(f: impl FnOnce(&mut Client) -> T) -> Result<T, String> {
    CONTROL
        .with_borrow_mut(|slot| slot.as_mut().map(f))
        .ok_or_else(|| "control client is not running".to_owned())
}

/// The answer to a request the game waits on, or an error saying why it could not be submitted.
pub async fn answered(submitted: Result<impl Future<Output = ControlResponse>, String>) -> ControlResponse {
    match submitted {
        Ok(answer) => answer.await,
        Err(message) => error(message),
    }
}

/// Start the worker, replacing any that is running, and ask the server for its version.
///
/// A client this replaces is stopped as by [`stop`], and the new worker starts only once the last one has finished.
pub fn init(endpoint: &str) -> Result<impl Future<Output = ControlResponse> + Send + 'static + use<>, String> {
    let client = LAST_WORKER.with_borrow_mut(|last| Client::start(Endpoint::new(endpoint), last))?;
    CONTROL.set(Some(client));
    with_control(|control| control.submit(ControlRequest::Version))?
}

/// Stop the worker.
///
/// The worker finishes the exchange it is in, if any, and then exits, and its runtime goes with it. Everything not yet
/// sent is dropped: queued requests, whose callers are answered with an error, and any unreported failures.
pub fn stop() { CONTROL.set(None); }

/// Take the oldest error that nobody was waiting for, such as a failed fire-and-forget request.
///
/// There is nothing to report when the client is not running.
pub fn take_error() -> Option<String> { with_control(Client::take_error).ok().flatten() }

/// Take up to `max` of the events the server has pushed since the last call.
///
/// There is nothing to hand over when the client is not running.
pub fn take_events(max: u16) -> Vec<ControlEvent> {
    with_control(|control| control.take_events(max as _)).unwrap_or_default()
}

/// Register a single-use code the game has shown to a player.
///
/// `ckey` is only what the browser that redeems the code is greeted with.
pub fn register_code(
    code: &str,
    player: PlayerId,
    ckey: &str,
) -> Result<impl Future<Output = ControlResponse> + Send + 'static + use<>, String> {
    let request = ControlRequest::RegisterCode {
        code: AuthCode::from(code),
        player,
        ckey: Ckey::from(ckey),
    };
    with_control(|control| control.submit(request))?
}

/// Look up the session a player has authenticated, if any.
pub fn check_auth(player: PlayerId) -> Result<impl Future<Output = ControlResponse> + Send + 'static + use<>, String> {
    with_control(|control| control.submit(ControlRequest::CheckAuth { player }))?
}

/// Start transmitting, either locally or on a radio frequency.
///
/// `channel` is the raw frequency, or `None` for local speech.
pub fn start_transmitting(session: SessionId, channel: Option<u16>) {
    set_transmit(
        session,
        Some(channel.map_or(Transmit::Local, |freq| Transmit::Radio(Freq(freq)))),
    );
}

/// Stop transmitting.
pub fn stop_transmitting(session: SessionId) { set_transmit(session, None); }

/// Send one transmit change, if the client is running.
fn set_transmit(session: SessionId, transmit: Option<Transmit>) {
    let _ = with_control(|control| control.set_transmit(session, transmit));
}

/// Hand `requests` to the worker as one batch, or say why it could not take them.
///
/// Nothing is kept for later, a batch that was not handed over is the caller's to deal with.
pub fn send_batch(requests: Vec<ControlRequest>) -> Result<(), String> {
    with_control(|control| control.send_batch(requests))?
}

/// Leave a failure for [`take_error`], for a fire-and-forget request refused before it was ever queued.
///
/// There is nobody to tell when the client is not running.
pub fn report(message: String) { let _ = with_control(|control| control.push_orphan_error(message)); }

#[cfg(test)]
mod tests {
    use super::{
        testing::{PATIENCE, Plan, block_on, endpoint, muted, player, serve},
        *,
    };

    #[test]
    fn a_code_registered_before_a_removal_reaches_the_server_first() {
        // Overtaken by the removal, the registration would leave a live code behind for a player the game is done
        // with. The exports go through the client they share, which is thread-local, so this test has its own.
        let endpoint = endpoint();
        let (server, seen) = serve(
            &endpoint,
            Plan {
                answers: 3,
                ..Plan::default()
            },
        );

        assert!(matches!(
            block_on(answered(init(endpoint.as_str()))),
            ControlResponse::Version { .. }
        ));

        let sefa = player("sefa");
        let registered = answered(register_code("ABC123", sefa, "sefa"));
        send_batch(vec![ControlRequest::RemovePlayer { player: sefa }]).expect("room in the queue");

        assert!(matches!(block_on(registered), ControlResponse::Ok));

        let order = Vec::from_iter((0..3).map(|_| seen.recv_timeout(PATIENCE).expect("every request")));
        assert!(matches!(order[0], ControlRequest::Version));
        assert!(matches!(order[1], ControlRequest::RegisterCode { .. }), "got {order:?}");
        assert!(
            matches!(&order[2], ControlRequest::Batch(requests) if matches!(requests[..], [ControlRequest::RemovePlayer { .. }])),
            "got {order:?}"
        );

        stop();
        server.join().expect("the fake server does not panic");
    }

    #[test]
    fn a_stop_right_after_init_still_answers_it() {
        let endpoint = endpoint();

        let answer = answered(init(endpoint.as_str()));
        stop();

        let answer = block_on(answer);
        assert!(
            matches!(&answer, ControlResponse::Error { message } if message.contains("failed to connect") || message.contains("stopped")),
            "got {answer:?}"
        );
        assert!(matches!(
            block_on(answered(check_auth(player("sefa")))),
            ControlResponse::Error { message } if message.contains("not running")
        ));
    }

    #[test]
    fn nothing_is_asked_of_a_client_that_is_not_running() {
        assert_eq!(take_error(), None);
        assert!(take_events(32).is_empty());
        assert!(
            send_batch(vec![ControlRequest::PatchPlayer {
                player: player("sefa"),
                patch: muted(),
            }])
            .is_err()
        );
        assert!(matches!(
            block_on(answered(check_auth(player("sefa")))),
            ControlResponse::Error { .. }
        ));
    }
}
