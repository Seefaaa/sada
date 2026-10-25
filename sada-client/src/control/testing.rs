//! What the control client's tests share: a fake server to talk to, and
//! ways to drive the client the way the exports do.

use std::{
    sync::{
        atomic::{AtomicU32, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use sada_common::{
    ControlEvent,
    ControlMessage,
    ControlRequest,
    ControlResponse,
    PROTOCOL_VERSION,
    PlayerId,
    PlayerPatch,
};
use sada_ipc::{Endpoint, Listener};
use tokio::{runtime, time::timeout};

use super::{answered, client::Client};
use crate::player;

/// How long a test waits for the worker before giving up.
pub const PATIENCE: Duration = Duration::from_secs(5);

/// Distinguishes the endpoint of one test from another's.
static NEXT_ENDPOINT: AtomicU32 = AtomicU32::new(0);

/// What a fake server does before it hangs up.
#[derive(Default)]
pub struct Plan {
    /// How many requests it answers.
    pub answers: usize,
    /// Player whose patches it refuses, so a failure can be buried in a batch.
    pub poison: Option<PlayerId>,
    /// Events it pushes as soon as the client connects.
    pub push: Vec<ControlEvent>,
}

/// An endpoint no other test uses.
pub fn endpoint() -> Endpoint {
    let id = NEXT_ENDPOINT.fetch_add(1, Ordering::Relaxed);

    Endpoint::from(std::env::temp_dir().join(format!("sada-client-test-{}-{id}.sock", std::process::id())))
}

/// Mint the id for `ckey` the way an export would.
///
/// The registry is thread-local and every test runs on its own thread, so one test's ids never meet another's.
pub fn player(ckey: &str) -> PlayerId { player::issue(ckey).expect("a free id") }

/// Start a fake server on `endpoint` and wait for it to be listening.
///
/// Every request it serves is reported on the returned receiver, so a test can assert what actually went over the
/// wire.
pub fn serve(endpoint: &Endpoint, plan: Plan) -> (thread::JoinHandle<()>, mpsc::Receiver<ControlRequest>) {
    let (ready_tx, ready_rx) = mpsc::channel();
    let (seen_tx, seen_rx) = mpsc::channel();
    let endpoint = endpoint.clone();

    let handle = thread::spawn(move || runtime(false).block_on(fake_server(endpoint, plan, ready_tx, seen_tx)));

    ready_rx
        .recv_timeout(PATIENCE)
        .expect("the fake server starts listening");

    (handle, seen_rx)
}

/// Serve one client the way the real server would, as far as `plan` goes.
async fn fake_server(endpoint: Endpoint, plan: Plan, ready: mpsc::Sender<()>, seen: mpsc::Sender<ControlRequest>) {
    let mut listener = Listener::bind(&endpoint).expect("a fresh endpoint");

    let _ = ready.send(());

    let Ok((to_client, from_client)) = listener.accept::<ControlMessage, ControlRequest>().await else {
        return;
    };

    if !plan.push.is_empty() {
        let _ = to_client.send_async(ControlMessage::Events(plan.push)).await;
    }

    for _ in 0..plan.answers {
        let Ok(request) = from_client.recv_async().await else {
            return;
        };

        let response = respond(&request, plan.poison);
        let _ = seen.send(request);

        if to_client.send_async(ControlMessage::Response(response)).await.is_err() {
            return;
        }
    }

    // Hanging up with the last answer still queued would take it down with the runtime this thread is about to
    // drop, and the client would see the connection close where it expects an answer.
    let _ = to_client.flush_async().await;
}

/// Answer one request the way the real server would.
fn respond(request: &ControlRequest, poison: Option<PlayerId>) -> ControlResponse {
    match request {
        ControlRequest::Version => ControlResponse::Version {
            protocol: PROTOCOL_VERSION,
            version: "test".to_owned(),
        },
        ControlRequest::CheckAuth { .. } => ControlResponse::Session { session: None },
        ControlRequest::Batch(requests) => {
            ControlResponse::Batch(requests.iter().map(|request| respond(request, poison)).collect())
        },
        ControlRequest::PatchPlayer { player, .. } if Some(*player) == poison => ControlResponse::Error {
            message: "this player cannot be patched".to_owned(),
        },
        _ => ControlResponse::Ok,
    }
}

/// Start a client against `endpoint`, as `init` does for the game.
pub fn start(endpoint: &Endpoint) -> Client { Client::start(endpoint.clone(), &mut None).expect("the worker starts") }

/// A runtime of the test's own, with its clock `paused` or not.
fn runtime(paused: bool) -> runtime::Runtime {
    runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(paused)
        .build()
        .expect("a runtime for the test")
}

/// Drive `future` to completion on a runtime of the test's own, as the async exports' runtime would.
pub fn block_on<F: Future>(future: F) -> F::Output { runtime(false).block_on(future) }

/// Drive `future` on a runtime whose clock is paused, so it skips ahead to the next timer whenever it would idle.
pub fn block_on_paused<F: Future>(future: F) -> F::Output { runtime(true).block_on(future) }

/// Send `request` and wait for its answer, the way an awaited export does.
pub fn ask(control: &Client, request: ControlRequest) -> ControlResponse {
    let answer = answered(control.submit(request));
    let answer = block_on(async { timeout(PATIENCE, answer).await });
    answer.expect("the worker never answered")
}

/// Ask the server for its version, which every exchange with a fake server starts with.
pub fn version(control: &Client) -> ControlResponse { ask(control, ControlRequest::Version) }

/// Ask for the session bound to `player`.
pub fn check_auth(control: &Client, player: PlayerId) -> ControlResponse {
    ask(control, ControlRequest::CheckAuth { player })
}

/// Wait for a failure nobody was waiting for.
pub fn wait_for_error(control: &mut Client) -> String {
    let deadline = Instant::now() + PATIENCE;

    loop {
        if let Some(message) = control.take_error() {
            return message;
        }
        assert!(Instant::now() < deadline, "the failure never reached the game");
        thread::sleep(Duration::from_millis(1));
    }
}

/// Wait for the server to push something.
pub fn wait_for_events(control: &mut Client) -> Vec<ControlEvent> {
    let deadline = Instant::now() + PATIENCE;

    loop {
        let events = control.take_events(32);

        if !events.is_empty() {
            return events;
        }

        assert!(Instant::now() < deadline, "the events never reached the game");
        thread::sleep(Duration::from_millis(1));
    }
}

/// The patch a test sends when it only cares that a patch travelled.
pub fn muted() -> PlayerPatch {
    PlayerPatch {
        mute: Some(true),
        ..PlayerPatch::default()
    }
}
