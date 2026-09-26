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
    num::NonZeroU16,
    os::unix::net::UnixStream,
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
    PlayerId,
    PlayerPatch,
    SessionId,
    Transmit,
};

use crate::player;

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

/// First ticket handed out, and the one numbering wraps back to.
const FIRST_TICKET: NonZeroU16 = NonZeroU16::new(1).unwrap();

/// Identifies a deferred response.
pub type Ticket = NonZeroU16;

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
    jobs: flume::Sender<Job>,
    /// Replies the worker has finished.
    replies: flume::Receiver<(Option<Ticket>, ControlResponse)>,
    /// Tickets issued but not yet answered, oldest first.
    issued: VecDeque<Ticket>,
    /// Replies that arrived before the game asked for them, oldest first.
    ready: VecDeque<(Ticket, ControlResponse)>,
    /// Requests waiting to go out together as one batch.
    ///
    /// The game describes many players per tick and the protocol is built to carry that in a single frame, so patches
    /// pile up here until [`flush`] sends them.
    pending: Vec<ControlRequest>,
    /// Failures from requests nobody is waiting for, oldest first, for [`take_error`] to report.
    orphan_errors: VecDeque<ControlResponse>,
    /// Ticket the next request that wants one will be given.
    next_ticket: Ticket,
}

impl ControlState {
    /// Create a new control state with the given channels.
    fn new(jobs: flume::Sender<Job>, replies: flume::Receiver<(Option<Ticket>, ControlResponse)>) -> Self {
        Self {
            jobs,
            replies,
            issued: VecDeque::new(),
            ready: VecDeque::new(),
            pending: Vec::new(),
            orphan_errors: VecDeque::new(),
            next_ticket: FIRST_TICKET,
        }
    }

    /// Take the next ticket, wrapping rather than overflowing.
    fn next_ticket(&mut self) -> Ticket {
        let ticket = self.next_ticket;
        self.next_ticket = self.next_ticket.checked_add(1).unwrap_or(FIRST_TICKET);
        ticket
    }

    /// Queue `request` and take a ticket for its response.
    ///
    /// The error says why the request could not be queued at all, which the game has to hear because no ticket will
    /// ever answer for it.
    fn submit(&mut self, request: ControlRequest) -> Result<Ticket, String> {
        let ticket = self.next_ticket();
        self.submit_inner(request, Some(ticket))?;
        Ok(ticket)
    }

    /// Queue `request` without taking a ticket, for a request whose answer nobody collects.
    ///
    /// One that cannot be queued at all leaves its failure for [`ControlState::take_error`]: there is no ticket to
    /// answer on, and the game has to hear that what it described never went out.
    fn submit_and_forget(&mut self, request: ControlRequest) {
        if let Err(refused) = self.submit_inner(request, None) {
            self.push_orphan_error(error(refused));
        }
    }

    /// Hand `request` to the worker, remembering `ticket` as outstanding when there is one.
    fn submit_inner(&mut self, request: ControlRequest, ticket: Option<Ticket>) -> Result<(), String> {
        match self.jobs.try_send(Job { ticket, request }) {
            Ok(()) => {
                if let Some(ticket) = ticket {
                    self.issued.push_back(ticket);
                }
                Ok(())
            },
            Err(flume::TrySendError::Full(_)) => Err("control queue is full".to_owned()),
            Err(flume::TrySendError::Disconnected(_)) => Err("control worker has stopped".to_owned()),
        }
    }

    /// Answer a request the client refused itself, on a ticket the game can collect like any other.
    ///
    /// Nothing is sent, so there is no response to wait for; the answer is ready before the ticket is handed out.
    fn refuse(&mut self, message: String) -> Ticket {
        let ticket = self.next_ticket();
        self.push_ready(ticket, error(message));
        ticket
    }

    /// Add `request` to the batch that the next [`ControlState::flush`] will send.
    fn enqueue(&mut self, request: ControlRequest) {
        if self.pending.len() >= MAX_PENDING_REQUESTS {
            self.push_orphan_error(error("control batch overflowed, the game is not flushing it"));
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

        self.submit_and_forget(batch);
    }

    /// Move everything the worker has finished into [`ControlState::ready`].
    fn drain(&mut self) {
        while let Ok((ticket, response)) = self.replies.try_recv() {
            match ticket {
                Some(ticket) => {
                    if let Some(at) = self.issued.iter().position(|t| *t == ticket) {
                        self.issued.remove(at);
                    }
                    self.push_ready(ticket, response);
                },
                None => self.push_orphan_error(response),
            }
        }
    }

    /// Record one reply, dropping the oldest if the game has stopped collecting them.
    fn push_ready(&mut self, ticket: Ticket, response: ControlResponse) {
        if self.ready.len() >= MAX_READY_REPLIES {
            self.ready.pop_front();
        }
        self.ready.push_back((ticket, response));
    }

    /// Record one failure nobody is waiting for, dropping the oldest if the game has stopped taking them.
    fn push_orphan_error(&mut self, response: ControlResponse) {
        if self.orphan_errors.len() >= MAX_READY_REPLIES {
            self.orphan_errors.pop_front();
        }
        self.orphan_errors.push_back(response);
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

        match self.orphan_errors.pop_front() {
            Some(ControlResponse::Error { message }) => Some(message),
            Some(unexpected) => unreachable!("got a non-error response to a fire-and-forget request: {unexpected:?}"),
            None => None,
        }
    }
}

/// Socket-owning half of the control client.
struct Worker {
    /// Socket path, kept for reconnecting.
    path: String,
    /// Jobs from the game thread.
    jobs: flume::Receiver<Job>,
    /// Finished replies, sent back to the game thread.
    replies: flume::Sender<(Option<Ticket>, ControlResponse)>,
    /// Current connection, or `None` until the next job reconnects.
    stream: Option<UnixStream>,
    /// Reused frame payload buffer.
    buffer: ControlFrameBuffer,
}

impl Worker {
    /// Create a new worker with the given channels.
    fn new(
        path: String,
        jobs: flume::Receiver<Job>,
        replies: flume::Sender<(Option<Ticket>, ControlResponse)>,
    ) -> Self {
        Self {
            path,
            jobs,
            replies,
            stream: None,
            buffer: ControlFrameBuffer::new(),
        }
    }

    /// Serve jobs until the game thread drops its end.
    fn run(mut self) {
        while let Ok(job) = self.jobs.recv() {
            let response = self.exchange(&job.request);

            match job.ticket {
                Some(ticket) => {
                    let _ = self.replies.send((Some(ticket), response));
                },
                // Nobody is waiting on a fire-and-forget request, but its failures still have to reach the game.
                None => {
                    if let Some(failure) = first_failure(response) {
                        let _ = self.replies.send((None, failure));
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

/// Read a player id token the game passed in.
fn parse_player(token: &str) -> Result<PlayerId, String> {
    PlayerId::from_token(token).ok_or_else(|| format!("{token:?} is not a player id"))
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
fn with_control<T>(f: impl FnOnce(&mut ControlState) -> T) -> Result<T, String> {
    CONTROL
        .with_borrow_mut(|slot| slot.as_mut().map(f))
        .ok_or_else(|| "control client is not running".to_owned())
}

/// Queue `request` without waiting for the server.
///
/// Returns the ticket its response will arrive under, or the reason it could not be queued at all.
fn submit(request: ControlRequest) -> Result<Ticket, String> { with_control(|control| control.submit(request))? }

/// Queue `request` without waiting for the server and without a ticket for its answer.
///
/// A request that cannot be queued leaves an error for [`take_error`]. The one failure that goes unreported is
/// having no client at all, because there is then nowhere to record it and nobody collecting.
fn submit_and_forget(request: ControlRequest) { let _ = with_control(|control| control.submit_and_forget(request)); }

/// Start the worker and ask the server for its version.
///
/// Returns the ticket that version answer will arrive under. Connecting happens on the worker, so a server that is
/// not up yet shows as an error on that ticket rather than as a failure here.
pub fn init(path: &str) -> Result<Ticket, String> {
    let (jobs_tx, jobs_rx) = flume::bounded(JOB_QUEUE_DEPTH);
    let (replies_tx, replies_rx) = flume::unbounded();

    let worker = Worker::new(path.to_owned(), jobs_rx, replies_tx);
    let control = ControlState::new(jobs_tx, replies_rx);

    thread::Builder::new()
        .name("sada-control".to_owned())
        .spawn(move || worker.run())
        .map_err(|err| format!("failed to spawn the control worker: {err}"))?;

    CONTROL.set(Some(control));

    submit(ControlRequest::Version)
}

/// Stop the worker and forget every outstanding ticket.
///
/// The worker finishes the exchange it is in and then exits, because its job channel is closed.
pub fn stop() { CONTROL.set(None); }

/// Collect the response for `ticket`.
pub fn poll(ticket: Ticket) -> Poll { with_control(|control| control.poll(ticket)).unwrap_or(Poll::Unknown) }

/// Take the oldest error that no ticket was waiting for, such as a failed fire-and-forget request.
///
/// There is nothing to report when the client is not running.
pub fn take_error() -> Option<String> { with_control(ControlState::take_error).ok().flatten() }

/// Register a single-use code the game has shown to a player.
///
/// `ckey` is only what the browser that redeems the code is greeted with.
pub fn register_code(code: &str, player: &str, ckey: &str) -> Result<Ticket, String> {
    with_control(|control| match parse_player(player) {
        Ok(player) => control.submit(ControlRequest::RegisterCode {
            code: AuthCode::from(code),
            player,
            ckey: Ckey::from(ckey),
        }),
        Err(refused) => Ok(control.refuse(refused)),
    })?
}

/// Look up the session a player has authenticated, if any.
pub fn check_auth(player: &str) -> Result<Ticket, String> {
    with_control(|control| match parse_player(player) {
        Ok(player) => control.submit(ControlRequest::CheckAuth { player }),
        Err(refused) => Ok(control.refuse(refused)),
    })?
}

/// Start transmitting, either locally or on a radio frequency.
///
/// `channel` is the raw frequency, or `None` for local speech.
pub fn start_transmitting(session: SessionId, channel: Option<u16>) {
    let transmit = Some(channel.map_or(Transmit::Local, |freq| Transmit::Radio(Freq(freq))));
    set_transmit(session, transmit);
}

/// Stop transmitting.
pub fn stop_transmitting(session: SessionId) { set_transmit(session, None); }

/// Send one transmit change.
///
/// Split out for convenience on the DM side.
fn set_transmit(session: SessionId, transmit: Option<Transmit>) {
    submit_and_forget(ControlRequest::SetTransmit { session, transmit });
}

/// Add one player's state delta to the batch that the next [`flush`] will send.
///
/// Nothing reaches the socket until [`flush`]; the error says why the patch was refused outright.
pub fn patch_player(player: &str, patch: PlayerPatch) -> Result<(), String> {
    let player = parse_player(player)?;

    // Reporting a dropped patch as accepted would leave the game believing it had described a player it never did,
    // and there is no error queue to fall back on when there is no client at all.
    with_control(|control| control.enqueue(ControlRequest::PatchPlayer { player, patch }))
}

/// Send everything [`patch_player`] has piled up as one batch.
pub fn flush() { let _ = with_control(ControlState::flush); }

/// Forget a player entirely.
///
/// This also drops the player's authentication on the server, so it belongs to a client going away rather than to a
/// player changing mobs.
pub fn remove_player(player: &str) {
    let Ok(player) = parse_player(player) else { return };

    // The id goes back to the pool whether or not the removal reaches the server: the game is done with this player.
    player::forget(player);

    let _ = with_control(|control| {
        // This has to travel behind whatever is still batched. A patch overtaken by the removal would land on a
        // vacant entry and recreate the player, who would then stay in the routing table for the rest of the round.
        control.enqueue(ControlRequest::RemovePlayer { player });
        control.flush();
    });
}

/// Ask for up to `max` queued server events.
pub fn poll_events(max: u16) -> Result<Ticket, String> { submit(ControlRequest::PollEvents { max }) }

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

    use sada_common::{PROTOCOL_VERSION, PlayerId, Position};

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

    /// Mint the id for `ckey` the way an export would, and render the token the game passes back.
    ///
    /// The registry is thread-local and every test runs on its own thread, so one test's ids never meet another's.
    fn player(ckey: &str) -> String { player::issue(ckey).expect("a free id").token() }

    /// Answer `count` requests on `listener`, then hang up.
    ///
    /// Serving a fixed number is what lets a test close the connection at a chosen point. Every request served is
    /// reported on the returned receiver, so a test can assert what actually went over the wire.
    fn serve(
        listener: UnixListener,
        count: usize,
        poison: Option<PlayerId>,
    ) -> (thread::JoinHandle<()>, mpsc::Receiver<ControlRequest>) {
        let (seen_tx, seen_rx) = mpsc::channel();

        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("the client connects");
            let mut buffer = ControlFrameBuffer::new();

            for _ in 0..count {
                let request: ControlRequest = match buffer.read(&mut stream) {
                    Ok(Some(request)) => request,
                    _ => return,
                };

                let response = respond(&request, poison);
                let _ = seen_tx.send(request);

                buffer.write(&mut stream, &response).expect("the client is still there");
            }
        });

        (handle, seen_rx)
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
            ControlRequest::PollEvents { .. } => ControlResponse::Events(Vec::new()),
            _ => ControlResponse::Ok,
        }
    }

    /// Bind a fresh listener, replacing whatever the last one left behind.
    fn listen(path: &Path) -> UnixListener {
        let _ = fs::remove_file(path);
        UnixListener::bind(path).expect("a fresh socket path")
    }

    /// Start the client against `path`, as `init` does for the game.
    fn start(path: &Path) -> Ticket { init(path.to_str().expect("a valid utf-8 path")).expect("the worker starts") }

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

    /// Wait for a failure nobody held a ticket for.
    fn wait_for_error() -> String {
        let deadline = Instant::now() + PATIENCE;

        loop {
            if let Some(message) = take_error() {
                return message;
            }
            assert!(Instant::now() < deadline, "the failure never reached the game");
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// The patch a test sends when it only cares that a patch travelled.
    fn muted() -> PlayerPatch {
        PlayerPatch {
            mute: Some(true),
            ..PlayerPatch::default()
        }
    }

    #[test]
    fn a_ticket_carries_the_response() {
        let path = socket_path();
        let (server, _seen) = serve(listen(&path), 2, None);

        let version = start(&path);
        assert!(matches!(wait(version), ControlResponse::Version { .. }));

        let auth = check_auth(&player("sefa")).expect("the request is queued");
        assert!(matches!(wait(auth), ControlResponse::Session { session: None }));

        stop();
        server.join().expect("the fake server does not panic");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn a_collected_ticket_is_not_offered_twice() {
        let path = socket_path();
        let (server, _seen) = serve(listen(&path), 1, None);

        let version = start(&path);
        wait(version);

        assert!(matches!(poll(version), Poll::Unknown));

        stop();
        server.join().expect("the fake server does not panic");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn the_worker_reconnects_after_the_server_restarts() {
        let path = socket_path();
        let (first, _seen) = serve(listen(&path), 1, None);

        let version = start(&path);
        assert!(matches!(wait(version), ControlResponse::Version { .. }));
        first.join().expect("the fake server does not panic");

        let sefa = player("sefa");

        let orphaned = check_auth(&sefa).expect("the request is queued");
        assert!(
            matches!(wait(orphaned), ControlResponse::Error { .. }),
            "a request in flight when the server goes away has to fail rather than hang"
        );

        let (second, _seen) = serve(listen(&path), 1, None);
        let after_restart = check_auth(&sefa).expect("the request is queued");
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
        let (server, seen) = serve(listen(&path), 2, None);

        let version = start(&path);
        wait(version);
        seen.recv().expect("the version request");

        let sefa = player("sefa");
        let patch = PlayerPatch {
            mute: Some(false),
            position: Some(Position { x: 4, y: 9, z: 2 }),
            ..PlayerPatch::default()
        };

        assert!(patch_player(&sefa, patch).is_ok());
        assert!(patch_player(&player("ahmet"), muted()).is_ok());
        flush();

        let batch = seen.recv_timeout(PATIENCE).expect("the batch reaches the server");
        let ControlRequest::Batch(requests) = batch else {
            panic!("patches have to be batched, got {batch:?}");
        };

        assert_eq!(requests.len(), 2, "both patches belong to the same frame");
        assert!(matches!(
            &requests[0],
            ControlRequest::PatchPlayer { player, patch }
                if player.token() == sefa
                    && patch.mute == Some(false)
                    && patch.position.is_some_and(|at| (at.x, at.y, at.z) == (4, 9, 2))
        ));

        stop();
        server.join().expect("the fake server does not panic");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn a_patch_for_something_that_is_not_a_player_is_refused_on_the_spot() {
        // The patch itself is decoded in `PlayerPatch::try_from`, which needs BYOND loaded and so is exercised by
        // the DM harness instead; what a refusal has to do either way is reach the game rather than vanish here.
        let path = socket_path();
        let (server, seen) = serve(listen(&path), 1, None);

        let version = start(&path);
        wait(version);
        seen.recv().expect("the version request");

        assert!(patch_player("", muted()).is_err(), "no player at all");
        assert!(patch_player("sefa", muted()).is_err(), "a ckey where a token belongs");
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
        let poison = player::issue(POISON).expect("a free id");
        let (server, _seen) = serve(listen(&path), 2, Some(poison));

        let version = start(&path);
        wait(version);

        // The batch itself succeeds; only one of its elements is rejected.
        assert!(patch_player(&player("sefa"), muted()).is_ok());
        assert!(patch_player(&poison.token(), muted()).is_ok());
        flush();

        let message = wait_for_error();
        assert!(message.contains("cannot be patched"), "got {message}");

        stop();
        server.join().expect("the fake server does not panic");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn events_and_removals_reach_the_server() {
        let path = socket_path();
        let (server, seen) = serve(listen(&path), 3, None);

        let version = start(&path);
        wait(version);
        seen.recv().expect("the version request");

        // A patch left in the batch and then a removal. Order is the whole point: were the removal to overtake the
        // patch, the patch would land on a vacant entry and bring the player back for the rest of the round.
        let sefa = player("sefa");
        assert!(patch_player(&sefa, muted()).is_ok());
        remove_player(&sefa);

        let events = poll_events(32).expect("the request is queued");
        assert!(matches!(wait(events), ControlResponse::Events(_)));

        let batch = seen.recv_timeout(PATIENCE).expect("the removal");
        let ControlRequest::Batch(requests) = batch else {
            panic!("the removal has to travel behind the batch, got {batch:?}");
        };

        assert!(matches!(&requests[0], ControlRequest::PatchPlayer { player, .. } if player.token() == sefa));
        assert!(matches!(&requests[1], ControlRequest::RemovePlayer { player } if player.token() == sefa));

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
        start(&path);
        stop_transmitting(SessionId::new(1, 1));

        wait_for_error();

        stop();
    }

    #[test]
    fn a_batch_that_cannot_be_queued_leaves_an_error() {
        // The batch is gone from `pending` the moment `flush` takes it, and the game has already recorded those
        // fields as sent, so a batch that never reaches the worker has to surface here or those players keep a stale
        // description for the rest of the round.
        let (jobs, held) = flume::bounded(0);
        let (_replies, answers) = flume::unbounded();
        let mut control = ControlState::new(jobs, answers);

        control.enqueue(ControlRequest::PatchPlayer {
            player: player::issue("sefa").expect("a free id"),
            patch: muted(),
        });
        control.flush();

        let message = control.take_error().expect("the lost batch leaves an error");
        assert!(message.contains("full"), "got {message}");

        drop(held);
    }

    #[test]
    fn nothing_is_asked_of_a_client_that_is_not_running() {
        // The game calls this on every fire, including the ones after `stop`, so an answer of "no errors" is the
        // only thing it can do here: a panic would surface as a DM runtime error every tick.
        assert_eq!(take_error(), None);
        assert!(matches!(poll(FIRST_TICKET), Poll::Unknown));
        assert!(patch_player(&player("sefa"), muted()).is_err());
    }
}
