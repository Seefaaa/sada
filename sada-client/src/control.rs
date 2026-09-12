//! Non-blocking Unix socket control client used by the exported functions.
//!
//! `call_ext` runs on BYOND's only thread, so nothing here may wait on the socket: a stalled voice server would stall
//! the whole world. A worker thread owns the connection and performs every syscall; the game thread only hands it
//! jobs and collects whatever has finished. Requests whose answer the game wants are issued a [`Ticket`] and polled
//! for on a later tick.
//!
//! The protocol is strictly one response per request and the server answers in order on a single connection, so the
//! worker needs no request ids on the wire: the response it is reading belongs to the job it just wrote.

use std::{
    cell::RefCell,
    collections::VecDeque,
    mem,
    os::unix::net::UnixStream,
    sync::mpsc::{self, Receiver, SyncSender, TrySendError},
    thread,
    time::Duration,
};

use sada_common::{
    AuthCode,
    Ckey,
    ControlFrameBuffer,
    ControlRequest,
    ControlResponse,
    Freq,
    PlayerPatch,
    SessionId,
    Transmit,
};

use crate::dm::PatchJson;

/// How many jobs may be queued for the worker before the game is told to back off.
const JOB_QUEUE_DEPTH: usize = 256;

/// How many uncollected replies are kept before the oldest are dropped.
const MAX_READY_REPLIES: usize = 256;

/// How many requests may pile up in one batch before it is refused.
///
/// A round has far fewer players than this, so hitting it means the game stopped flushing rather than that it had a
/// lot to say.
const MAX_PENDING_REQUESTS: usize = 4096;

/// How long the worker waits on a wedged server before dropping the connection and reconnecting.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Ticket value that is never handed out, used for errors nobody asked for.
const NO_TICKET: Ticket = 0;

/// Identifies a deferred response.
pub type Ticket = u16;

thread_local! {
    /// Game-thread half of the control client, installed by [`init`].
    static CONTROL: RefCell<Option<ControlState>> = const { RefCell::new(None) };
}

/// State of one deferred response.
pub enum Poll {
    /// The response has arrived.
    Ready(ControlResponse),
    /// The request is still with the worker.
    Pending,
    /// No such ticket: never issued, already collected, or dropped as stale.
    Unknown,
}

/// One request queued for the worker.
struct Job {
    /// Ticket to answer under, or `None` when the game does not want the response.
    ticket: Option<Ticket>,
    /// Request to send.
    request: ControlRequest,
}

/// Game-thread half of the control client.
struct ControlState {
    /// Jobs handed to the worker.
    jobs: SyncSender<Job>,
    /// Replies the worker has finished.
    replies: Receiver<(Ticket, ControlResponse)>,
    /// Tickets issued but not yet answered, oldest first.
    issued: VecDeque<Ticket>,
    /// Replies that arrived before the game asked for them, oldest first.
    ready: VecDeque<(Ticket, ControlResponse)>,
    /// Ticket the next request that wants one will be given.
    next_ticket: Ticket,
    /// Requests waiting to go out together as one batch.
    ///
    /// The game describes many players per tick and the protocol is built to carry that in a single frame, so patches
    /// pile up here until [`flush`] sends them.
    pending: Vec<ControlRequest>,
}

impl ControlState {
    /// Queue `request`, taking a ticket for its response when `want_reply`.
    ///
    /// Returns [`NO_TICKET`] for fire-and-forget requests and for requests that could not be queued at all; the
    /// latter also leave an error for [`take_error`].
    fn submit(&mut self, request: ControlRequest, want_reply: bool) -> Ticket {
        let ticket = if want_reply {
            self.next_ticket = self.next_ticket.wrapping_add(1);
            self.next_ticket
        } else {
            NO_TICKET
        };

        let job = Job {
            ticket: want_reply.then_some(ticket),
            request,
        };

        match self.jobs.try_send(job) {
            Ok(()) => {
                if want_reply {
                    self.issued.push_back(ticket);
                }
                ticket
            },
            Err(TrySendError::Full(_)) => {
                self.push_ready(
                    NO_TICKET,
                    error("control queue is full, the voice server is not keeping up"),
                );
                NO_TICKET
            },
            Err(TrySendError::Disconnected(_)) => {
                self.push_ready(NO_TICKET, error("control worker has stopped"));
                NO_TICKET
            },
        }
    }

    /// Add `request` to the batch that the next [`ControlState::flush`] will send.
    fn enqueue(&mut self, request: ControlRequest) {
        if self.pending.len() >= MAX_PENDING_REQUESTS {
            self.push_ready(
                NO_TICKET,
                error("control batch overflowed, the game is not flushing it"),
            );
            return;
        }

        self.pending.push(request);
    }

    /// Send everything that has piled up as one batch.
    fn flush(&mut self) {
        if self.pending.is_empty() {
            return;
        }

        let batch = ControlRequest::Batch(mem::take(&mut self.pending));

        self.submit(batch, false);
    }

    /// Move everything the worker has finished into [`ControlState::ready`].
    fn drain(&mut self) {
        while let Ok((ticket, response)) = self.replies.try_recv() {
            if let Some(at) = self.issued.iter().position(|t| *t == ticket) {
                self.issued.remove(at);
            }
            self.push_ready(ticket, response);
        }
    }

    /// Record one reply, dropping the oldest if the game has stopped collecting them.
    fn push_ready(&mut self, ticket: Ticket, response: ControlResponse) {
        if self.ready.len() >= MAX_READY_REPLIES {
            self.ready.pop_front();
        }
        self.ready.push_back((ticket, response));
    }

    /// Collect the response for `ticket`, if it has arrived.
    fn poll(&mut self, ticket: Ticket) -> Poll {
        self.drain();

        if let Some(at) = self.ready.iter().position(|(t, _)| *t == ticket) {
            let (_, response) = self.ready.remove(at).expect("position came from this deque");
            return Poll::Ready(response);
        }

        if self.issued.contains(&ticket) {
            Poll::Pending
        } else {
            Poll::Unknown
        }
    }

    /// Take the oldest error that no ticket was waiting for.
    fn take_error(&mut self) -> Option<String> {
        self.drain();

        let at = self.ready.iter().position(|(t, _)| *t == NO_TICKET)?;

        match self.ready.remove(at) {
            Some((_, ControlResponse::Error { message })) => Some(message),
            _ => None,
        }
    }
}

/// Socket-owning half of the control client.
struct Worker {
    /// Socket path, kept for reconnecting.
    path: String,
    /// Jobs from the game thread.
    jobs: Receiver<Job>,
    /// Finished replies, sent back to the game thread.
    replies: mpsc::Sender<(Ticket, ControlResponse)>,
    /// Current connection, or `None` until the next job reconnects.
    stream: Option<UnixStream>,
    /// Reused frame payload buffer.
    buffer: ControlFrameBuffer,
}

impl Worker {
    /// Serve jobs until the game thread drops its end.
    fn run(mut self) {
        while let Ok(job) = self.jobs.recv() {
            let response = self.exchange(&job.request);

            match job.ticket {
                Some(ticket) => {
                    let _ = self.replies.send((ticket, response));
                },
                // Nobody is waiting on a fire-and-forget request, but its failures still have to reach the game.
                None => {
                    if let Some(failure) = first_failure(response) {
                        let _ = self.replies.send((NO_TICKET, failure));
                    }
                },
            }
        }
    }

    /// Send one request and read its response, connecting first if the last exchange broke the connection.
    fn exchange(&mut self, request: &ControlRequest) -> ControlResponse {
        if self.stream.is_none() {
            match UnixStream::connect(&self.path) {
                Ok(stream) => {
                    // A wedged server must not park the worker forever, or the job queue fills and the game starts
                    // seeing "queue is full" instead of a reconnect.
                    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
                    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
                    self.stream = Some(stream);
                },
                Err(err) => return error(format!("failed to connect to {}: {err}", self.path)),
            }
        }

        let stream = self.stream.as_mut().expect("just connected");

        match Self::exchange_on(&mut self.buffer, stream, request) {
            Ok(response) => response,
            Err(message) => {
                // Half of a frame may have gone out, so the connection is no longer trustworthy. Drop it and let the
                // next job reconnect; retrying this one could apply it twice.
                self.stream = None;
                error(message)
            },
        }
    }

    /// Perform one exchange over `stream`.
    fn exchange_on(
        buffer: &mut ControlFrameBuffer,
        stream: &mut UnixStream,
        request: &ControlRequest,
    ) -> Result<ControlResponse, String> {
        buffer
            .write(stream, request)
            .map_err(|err| format!("failed to send control request: {err}"))?;

        match buffer.read(stream) {
            Ok(Some(response)) => Ok(response),
            Ok(None) => Err("control socket was closed by the server".to_owned()),
            Err(err) => Err(format!("failed to read control response: {err}")),
        }
    }
}

/// Build a [`ControlResponse::Error`] from anything printable.
fn error(message: impl Into<String>) -> ControlResponse {
    ControlResponse::Error {
        message: message.into(),
    }
}

/// The first failure in a response, looking inside a batch.
///
/// A batch answers with one response per element, so a rejected patch sitting among successes would otherwise leave
/// no trace at all: the batch itself succeeded.
fn first_failure(response: ControlResponse) -> Option<ControlResponse> {
    match response {
        failure @ ControlResponse::Error { .. } => Some(failure),
        ControlResponse::Batch(responses) => responses.into_iter().find_map(first_failure),
        _ => None,
    }
}

/// Run `f` against the installed control state.
///
/// Returns `None` when [`init`] has not been called, which the exports report as an error rather than a panic.
fn with_control<T>(f: impl FnOnce(&mut ControlState) -> T) -> Option<T> {
    CONTROL.with_borrow_mut(|slot| slot.as_mut().map(f))
}

/// Queue `request` without waiting for the server.
///
/// Returns the ticket its response will arrive under, or [`NO_TICKET`] when the request was fire-and-forget or could
/// not be queued.
fn submit(request: ControlRequest, want_reply: bool) -> Ticket {
    with_control(|control| control.submit(request, want_reply)).unwrap_or(NO_TICKET)
}

/// Start the worker and ask the server for its version.
///
/// Returns the ticket that version answer will arrive under. Connecting happens on the worker, so a server that is
/// not up yet shows as an error on that ticket rather than as a failure here.
pub fn init(path: &str) -> Ticket {
    let (jobs_tx, jobs_rx) = mpsc::sync_channel(JOB_QUEUE_DEPTH);
    let (replies_tx, replies_rx) = mpsc::channel();

    let worker = Worker {
        path: path.to_owned(),
        jobs: jobs_rx,
        replies: replies_tx,
        stream: None,
        buffer: ControlFrameBuffer::new(),
    };

    if thread::Builder::new()
        .name("sada-control".to_owned())
        .spawn(move || worker.run())
        .is_err()
    {
        return NO_TICKET;
    }

    // Replacing the state drops the previous sender, which ends the previous worker once it finishes what it holds.
    CONTROL.set(Some(ControlState {
        jobs: jobs_tx,
        replies: replies_rx,
        issued: VecDeque::new(),
        ready: VecDeque::new(),
        next_ticket: NO_TICKET,
        pending: Vec::new(),
    }));

    submit(ControlRequest::Version, true)
}

/// Stop the worker and forget every outstanding ticket.
///
/// The worker finishes the exchange it is in and then exits, because its job channel is closed.
pub fn stop() { CONTROL.set(None); }

/// Collect the response for `ticket`.
pub fn poll(ticket: Ticket) -> Poll { with_control(|control| control.poll(ticket)).unwrap_or(Poll::Unknown) }

/// Take the oldest error that no ticket was waiting for, such as a failed fire-and-forget request.
pub fn take_error() -> Option<String> { with_control(ControlState::take_error).flatten() }

/// Register a single-use code the game has shown to a player.
pub fn register_code(code: &str, ckey: &str) -> Ticket {
    submit(
        ControlRequest::RegisterCode {
            code: AuthCode::from(code),
            ckey: Ckey::from(ckey),
        },
        true,
    )
}

/// Look up the session a player has authenticated, if any.
pub fn check_auth(ckey: &str) -> Ticket { submit(ControlRequest::CheckAuth { ckey: Ckey::from(ckey) }, true) }

/// Start transmitting, either locally or on a radio frequency.
///
/// `channel` is the raw frequency, or `None` for local speech.
pub fn start_transmitting(session: u64, channel: Option<u32>) {
    let transmit = Some(channel.map_or(Transmit::Local, |freq| Transmit::Radio(Freq(freq))));
    set_transmit(session, transmit);
}

/// Stop transmitting.
pub fn stop_transmitting(session: u64) { set_transmit(session, None); }

/// Send one transmit change.
///
/// Split out for convenience on the DM side.
fn set_transmit(session: u64, transmit: Option<Transmit>) {
    submit(
        ControlRequest::SetTransmit {
            session: SessionId::from_raw(session),
            transmit,
        },
        false,
    );
}

/// Add one player's state delta to the batch that the next [`flush`] will send.
///
/// `patch` is the JSON shape described by [`PatchJson`]. Nothing reaches the socket until [`flush`]; the error says
/// why the patch was refused outright, which the game has to hear because no batch will ever carry it.
pub fn patch_player(ckey: &str, patch: &str) -> Result<(), String> {
    let patch = match serde_json::from_str::<PatchJson>(patch) {
        Ok(patch) => PlayerPatch::from(patch),
        Err(err) => return Err(format!("failed to parse the patch for {ckey}: {err}")),
    };

    // Reporting a dropped patch as accepted would leave the game believing it had described a player it never did,
    // and there is no error queue to fall back on when there is no client at all.
    with_control(|control| {
        control.enqueue(ControlRequest::PatchPlayer {
            ckey: Ckey::from(ckey),
            patch,
        });
    })
    .ok_or_else(|| format!("the control client is not running, dropped the patch for {ckey}"))
}

/// Send everything [`patch_player`] has piled up as one batch.
pub fn flush() { with_control(ControlState::flush); }

/// Forget a player entirely.
///
/// This also drops the player's authentication on the server, so it belongs to a client going away rather than to a
/// player changing mobs.
pub fn remove_player(ckey: &str) {
    with_control(|control| {
        // This has to travel behind whatever is still batched. A patch overtaken by the removal would land on a
        // vacant entry and recreate the player, who would then stay in the routing table for the rest of the round.
        control.enqueue(ControlRequest::RemovePlayer { ckey: Ckey::from(ckey) });
        control.flush();
    });
}

/// Ask for up to `max` queued server events.
pub fn poll_events(max: u16) -> Ticket { submit(ControlRequest::PollEvents { max }, true) }

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::net::UnixListener,
        path::{Path, PathBuf},
        sync::{
            atomic::{AtomicU32, Ordering},
            mpsc,
        },
        thread,
        time::{Duration, Instant},
    };

    use sada_common::{Ckey, ControlFrameBuffer, ControlRequest, ControlResponse, PROTOCOL_VERSION};

    use super::*;

    /// How long a test waits for the worker before giving up.
    const PATIENCE: Duration = Duration::from_secs(5);

    /// Distinguishes the socket of one test from another's.
    static NEXT_SOCKET: AtomicU32 = AtomicU32::new(0);

    /// A socket path no other test uses.
    fn socket_path() -> PathBuf {
        let id = NEXT_SOCKET.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("sada-client-test-{}-{id}.sock", std::process::id()))
    }

    /// Ckey the fake server refuses to patch, so a test can put a failure inside a batch.
    const POISON: &str = "poison";

    /// Answer `count` requests on `listener`, then hang up.
    ///
    /// Serving a fixed number is what lets a test close the connection at a chosen point. Every request served is
    /// reported on the returned receiver, so a test can assert what actually went over the wire.
    fn serve(listener: UnixListener, count: usize) -> (thread::JoinHandle<()>, mpsc::Receiver<ControlRequest>) {
        let (seen_tx, seen_rx) = mpsc::channel();

        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("the client connects");
            let mut buffer = ControlFrameBuffer::new();

            for _ in 0..count {
                let request: ControlRequest = match buffer.read(&mut stream) {
                    Ok(Some(request)) => request,
                    _ => return,
                };

                let response = respond(&request);
                let _ = seen_tx.send(request);

                buffer.write(&mut stream, &response).expect("the client is still there");
            }
        });

        (handle, seen_rx)
    }

    /// Answer one request the way the real server would.
    fn respond(request: &ControlRequest) -> ControlResponse {
        match request {
            ControlRequest::Version => ControlResponse::Version {
                protocol: PROTOCOL_VERSION,
                version: "test".to_owned(),
            },
            ControlRequest::CheckAuth { .. } => ControlResponse::Session { session: None },
            ControlRequest::Batch(requests) => ControlResponse::Batch(requests.iter().map(respond).collect()),
            ControlRequest::PatchPlayer { ckey, .. } if *ckey == Ckey::from(POISON) => ControlResponse::Error {
                message: "this player cannot be patched".to_owned(),
            },
            ControlRequest::PollEvents { .. } => ControlResponse::Events(Vec::new()),
            _ => ControlResponse::Ok,
        }
    }

    /// Bind a fresh listener, replacing whatever the last one left behind.
    fn listen(path: &Path) -> UnixListener {
        let _ = fs::remove_file(path);
        UnixListener::bind(path).expect("a fresh socket path")
    }

    /// Wait for `ticket` to be answered.
    fn wait(ticket: Ticket) -> ControlResponse {
        let deadline = Instant::now() + PATIENCE;

        loop {
            match poll(ticket) {
                Poll::Ready(response) => return response,
                Poll::Pending => assert!(Instant::now() < deadline, "the worker never answered"),
                Poll::Unknown => panic!("ticket {ticket} was never issued"),
            }
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn a_ticket_carries_the_response() {
        let path = socket_path();
        let (server, _seen) = serve(listen(&path), 2);

        let version = init(path.to_str().expect("a valid utf-8 path"));
        assert!(matches!(wait(version), ControlResponse::Version { .. }));

        let auth = check_auth("sefa");
        assert!(matches!(wait(auth), ControlResponse::Session { session: None }));

        stop();
        server.join().expect("the fake server does not panic");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn a_collected_ticket_is_not_offered_twice() {
        let path = socket_path();
        let (server, _seen) = serve(listen(&path), 1);

        let version = init(path.to_str().expect("a valid utf-8 path"));
        wait(version);

        assert!(matches!(poll(version), Poll::Unknown));

        stop();
        server.join().expect("the fake server does not panic");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn the_worker_reconnects_after_the_server_restarts() {
        let path = socket_path();
        let (first, _seen) = serve(listen(&path), 1);

        let version = init(path.to_str().expect("a valid utf-8 path"));
        assert!(matches!(wait(version), ControlResponse::Version { .. }));
        first.join().expect("the fake server does not panic");

        let orphaned = check_auth("sefa");
        assert!(
            matches!(wait(orphaned), ControlResponse::Error { .. }),
            "a request in flight when the server goes away has to fail rather than hang"
        );

        let (second, _seen) = serve(listen(&path), 1);
        let after_restart = check_auth("sefa");
        assert!(
            matches!(wait(after_restart), ControlResponse::Session { session: None }),
            "the next request has to reconnect on its own"
        );

        stop();
        second.join().expect("the fake server does not panic");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn patches_travel_as_one_batch() {
        let path = socket_path();
        let (server, seen) = serve(listen(&path), 2);

        let version = init(path.to_str().expect("a valid utf-8 path"));
        wait(version);
        seen.recv().expect("the version request");

        assert!(patch_player("sefa", r#"{"mute":0,"position":{"x":4,"y":9,"z":2}}"#).is_ok());
        assert!(patch_player("ahmet", r#"{"deaf":true}"#).is_ok());
        flush();

        let batch = seen.recv_timeout(PATIENCE).expect("the batch reaches the server");
        let ControlRequest::Batch(requests) = batch else {
            panic!("patches have to be batched, got {batch:?}");
        };

        assert_eq!(requests.len(), 2, "both patches belong to the same frame");
        assert!(matches!(
            &requests[0],
            ControlRequest::PatchPlayer { ckey, patch }
                if *ckey == Ckey::from("sefa")
                    && patch.mute == Some(false)
                    && patch.position.is_some_and(|at| (at.x, at.y, at.z) == (4, 9, 2))
        ));

        stop();
        server.join().expect("the fake server does not panic");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn a_patch_that_does_not_parse_is_refused_on_the_spot() {
        let path = socket_path();
        let (server, seen) = serve(listen(&path), 1);

        let version = init(path.to_str().expect("a valid utf-8 path"));
        wait(version);
        seen.recv().expect("the version request");

        assert!(patch_player("sefa", "{\"mute\": ").is_err(), "truncated json");
        assert!(patch_player("sefa", r#"{"muet":true}"#).is_err(), "misspelled field");
        flush();

        // Nothing was queued, so flush has nothing to send and the server sees no second request.
        assert!(seen.recv_timeout(Duration::from_millis(200)).is_err());

        stop();
        server.join().expect("the fake server does not panic");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn a_failure_inside_a_batch_reaches_the_game() {
        let path = socket_path();
        let (server, _seen) = serve(listen(&path), 2);

        let version = init(path.to_str().expect("a valid utf-8 path"));
        wait(version);

        // The batch itself succeeds; only one of its elements is rejected.
        assert!(patch_player("sefa", r#"{"mute":0}"#).is_ok());
        assert!(patch_player(POISON, r#"{"mute":0}"#).is_ok());
        flush();

        let deadline = Instant::now() + PATIENCE;
        loop {
            if let Some(message) = take_error() {
                assert!(message.contains("cannot be patched"), "got {message}");
                break;
            }
            assert!(Instant::now() < deadline, "the rejected patch left no trace");
            thread::sleep(Duration::from_millis(1));
        }

        stop();
        server.join().expect("the fake server does not panic");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn events_and_removals_reach_the_server() {
        let path = socket_path();
        let (server, seen) = serve(listen(&path), 3);

        let version = init(path.to_str().expect("a valid utf-8 path"));
        wait(version);
        seen.recv().expect("the version request");

        // A patch left in the batch and then a removal. Order is the whole point: were the removal to overtake the
        // patch, the patch would land on a vacant entry and bring the player back for the rest of the round.
        assert!(patch_player("sefa", r#"{"mute":0}"#).is_ok());
        remove_player("sefa");

        let events = poll_events(32);
        assert!(matches!(wait(events), ControlResponse::Events(_)));

        let batch = seen.recv_timeout(PATIENCE).expect("the removal");
        let ControlRequest::Batch(requests) = batch else {
            panic!("the removal has to travel behind the batch, got {batch:?}");
        };

        assert!(matches!(&requests[0], ControlRequest::PatchPlayer { ckey, .. } if *ckey == Ckey::from("sefa")));
        assert!(matches!(&requests[1], ControlRequest::RemovePlayer { ckey } if *ckey == Ckey::from("sefa")));

        assert!(matches!(
            seen.recv_timeout(PATIENCE).expect("the poll"),
            ControlRequest::PollEvents { max: 32 }
        ));

        stop();
        server.join().expect("the fake server does not panic");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn a_failed_fire_and_forget_leaves_an_error() {
        let path = socket_path();

        // No server at all, nothing is listening on this path.
        init(path.to_str().expect("a valid utf-8 path"));
        stop_transmitting(1);

        let deadline = Instant::now() + PATIENCE;
        loop {
            if take_error().is_some() {
                break;
            }
            assert!(Instant::now() < deadline, "the failure never reached the game");
            thread::sleep(Duration::from_millis(1));
        }

        stop();
    }
}
