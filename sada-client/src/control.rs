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

use std::{
    cell::RefCell,
    collections::VecDeque,
    error::Error,
    mem,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use sada_common::{
    AuthCode,
    Ckey,
    ControlEvent,
    ControlMessage,
    ControlRequest,
    ControlResponse,
    Freq,
    PlayerId,
    PlayerPatch,
    SessionId,
    Transmit,
};
use sada_ipc::{Endpoint, Receiver, RecvError, SendError, Sender};
use tokio::{runtime, sync::oneshot, time::timeout};

use crate::player;

/// How many jobs may be queued for the worker before the game is told to back off.
const JOB_QUEUE_DEPTH: usize = 256;

/// How many failures nobody is waiting for are kept before the oldest are dropped.
const MAX_QUEUED_ERRORS: usize = 256;

/// How many pushed events are kept before the oldest are dropped.
///
/// The game takes them every fire; if it stops there is no point growing the queue without bound.
const MAX_QUEUED_EVENTS: usize = 4096;

/// How many requests may pile up in one batch before it is refused.
///
/// A round has far fewer players than this, so hitting it means the game stopped flushing rather than that it had a
/// lot to say.
const MAX_PENDING_REQUESTS: usize = 4096;

/// How long the worker waits on a wedged server before dropping the connection and reconnecting.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a request somebody waits on may wait for the worker to start on it before it is dropped unsent.
const ANSWER_TIMEOUT: Duration = IO_TIMEOUT.saturating_mul(3);

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

/// Game-thread half of the control client.
pub struct Client {
    /// Jobs handed to the worker.
    jobs: flume::Sender<Job>,
    /// Failures and events the worker has for the game.
    replies: flume::Receiver<Reply>,
    /// Requests waiting to go out together as one batch.
    ///
    /// The game describes many players per tick and the protocol is built to carry that in a single frame, so patches
    /// pile up here until [`Client::flush`] sends them.
    pending: Vec<ControlRequest>,
    /// Failures from requests nobody is waiting for, oldest first, for [`Client::take_error`] to report.
    orphan_errors: VecDeque<String>,
    /// Events the server pushed, oldest first, for [`Client::take_events`] to hand over.
    events: VecDeque<ControlEvent>,
    /// Set when the client goes away, so its worker drops whatever it has not started on.
    retired: Arc<AtomicBool>,
}

impl Client {
    /// Create a new client with the given channels.
    fn new(jobs: flume::Sender<Job>, replies: flume::Receiver<Reply>) -> Self {
        Self {
            jobs,
            replies,
            pending: Vec::new(),
            orphan_errors: VecDeque::new(),
            events: VecDeque::new(),
            retired: Arc::default(),
        }
    }

    /// Start a worker for `endpoint` on a thread of its own, returning the client and the worker's thread.
    ///
    /// The worker starts once `previous`, the thread of the worker it replaces, has finished. Dropping the client
    /// stops the worker as soon as it is done with the exchange it is in; whatever is still queued is dropped unsent.
    pub fn start(
        endpoint: Endpoint,
        previous: Option<thread::JoinHandle<()>>,
    ) -> Result<(Self, thread::JoinHandle<()>), String> {
        let runtime = runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|err| format!("failed to build the control runtime: {err}"))?;

        let (jobs_tx, jobs_rx) = flume::bounded(JOB_QUEUE_DEPTH);
        let (replies_tx, replies_rx) = flume::unbounded();

        let client = Self::new(jobs_tx, replies_rx);
        let worker = Worker::new(endpoint, jobs_rx, replies_tx, client.retired.clone());

        let thread = thread::Builder::new()
            .name("sada-control".to_owned())
            .spawn(move || {
                if let Some(previous) = previous {
                    let _ = previous.join();
                }
                worker.serve(runtime)
            })
            .map_err(|err| format!("failed to spawn the control worker: {err}"))?;

        Ok((client, thread))
    }

    /// Hand `job` to the worker without waiting, or say why it could not be.
    fn queue(&self, job: Job) -> Result<(), String> {
        self.jobs.try_send(job).map_err(|refused| match refused {
            flume::TrySendError::Full(_) => "control queue is full".to_owned(),
            flume::TrySendError::Disconnected(_) => "control worker has stopped".to_owned(),
        })
    }

    /// Queue `request` now and return a future for its answer, or say why it could not be queued.
    ///
    /// The request takes its place among the jobs at the call, not when the future is first polled, so it travels in
    /// order with whatever the game queues around it. One the worker has not started on after [`ANSWER_TIMEOUT`] is
    /// answered with that and never sent; one it has started on is waited for until the exchange is over.
    pub fn submit(
        &self,
        request: ControlRequest,
    ) -> Result<impl Future<Output = ControlResponse> + Send + 'static + use<>, String> {
        let (answer, mut answered) = oneshot::channel();
        let claim = Arc::new(AtomicBool::new(false));

        self.queue(Job {
            request,
            waiter: Some(Waiter {
                answer,
                claim: claim.clone(),
            }),
        })?;

        Ok(async move {
            let answer = match timeout(ANSWER_TIMEOUT, &mut answered).await {
                Ok(answer) => answer,
                Err(_) if take(&claim) => return error("the voice server did not get to the request in time"),
                Err(_) => answered.await,
            };

            answer.unwrap_or_else(|_| error("the control worker stopped before the server answered"))
        })
    }

    /// Queue `request` without anybody waiting for its answer.
    ///
    /// One that cannot be queued at all leaves its failure for [`Client::take_error`]: nobody else will hear of it,
    /// and the game has to hear that what it described never went out.
    fn submit_and_forget(&mut self, request: ControlRequest) {
        if let Err(refused) = self.queue(Job { request, waiter: None }) {
            self.push_orphan_error(refused);
        }
    }

    /// Add `request` to the batch that the next [`Client::flush`] will send.
    fn enqueue(&mut self, request: ControlRequest) {
        if self.pending.len() >= MAX_PENDING_REQUESTS {
            self.push_orphan_error("control batch overflowed, the game is not flushing it".to_owned());
            return;
        }

        self.pending.push(request);
    }

    /// Send everything that has piled up as one batch.
    pub fn flush(&mut self) {
        if self.pending.is_empty() {
            return;
        }

        let batch = ControlRequest::Batch(mem::take(&mut self.pending));

        self.submit_and_forget(batch);
    }

    /// Add one player's state delta to the batch that the next [`Client::flush`] will send.
    ///
    /// The error says why the patch was refused outright.
    pub fn patch_player(&mut self, player: &str, patch: PlayerPatch) -> Result<(), String> {
        let player = parse_player(player)?;
        self.enqueue(ControlRequest::PatchPlayer { player, patch });
        Ok(())
    }

    /// Forget a player entirely, behind whatever is still batched.
    pub fn remove_player(&mut self, player: PlayerId) {
        // This has to travel behind whatever is still batched. A patch overtaken by the removal would land on a
        // vacant entry and recreate the player, who would then stay in the routing table for the rest of the round.
        self.enqueue(ControlRequest::RemovePlayer { player });
        self.flush();
    }

    /// Send one transmit change, without waiting for it.
    pub fn set_transmit(&mut self, session: SessionId, transmit: Option<Transmit>) {
        self.submit_and_forget(ControlRequest::SetTransmit { session, transmit });
    }

    /// Move everything the worker has for the game into the queues the game collects from.
    fn drain(&mut self) {
        while let Ok(reply) = self.replies.try_recv() {
            match reply {
                Reply::Failure(message) => self.push_orphan_error(message),
                Reply::Events(events) => self.push_events(events),
            }
        }
    }

    /// Record one failure nobody is waiting for, dropping the oldest if the game has stopped taking them.
    fn push_orphan_error(&mut self, message: String) {
        if self.orphan_errors.len() >= MAX_QUEUED_ERRORS {
            self.orphan_errors.pop_front();
        }
        self.orphan_errors.push_back(message);
    }

    /// Record pushed events, dropping the oldest if the game has stopped taking them.
    fn push_events(&mut self, events: Vec<ControlEvent>) {
        self.events.extend(events);

        // Trimming after rather than before: one frame can hold more events than the queue keeps, and taking the
        // overflow off the front first would ask for more than is there.
        if self.events.len() > MAX_QUEUED_EVENTS {
            let over = self.events.len() - MAX_QUEUED_EVENTS;
            self.events.drain(..over);
        }
    }

    /// Take the oldest error that nobody was waiting for.
    pub fn take_error(&mut self) -> Option<String> {
        self.drain();
        self.orphan_errors.pop_front()
    }

    /// Take up to `max` of the events the server has pushed, oldest first.
    pub fn take_events(&mut self, max: usize) -> Vec<ControlEvent> {
        self.drain();

        let taken = self.events.len().min(max);

        self.events.drain(..taken).collect()
    }
}

impl Drop for Client {
    fn drop(&mut self) { self.retired.store(true, Ordering::Release); }
}

/// Take `claim` for the side calling this, or say the other side already has it.
fn take(claim: &AtomicBool) -> bool { !claim.swap(true, Ordering::AcqRel) }

/// The two ends of one control channel.
struct Connection {
    /// Requests out.
    to_server: Sender<ControlRequest>,
    /// Answers and events in.
    from_server: Receiver<ControlMessage>,
}

/// Channel-owning half of the control client.
struct Worker {
    /// Endpoint, kept for reconnecting.
    endpoint: Endpoint,
    /// Jobs from the game thread.
    jobs: flume::Receiver<Job>,
    /// Failures nobody is waiting for and pushed events, for the game thread.
    replies: flume::Sender<Reply>,
    /// Current connection, or `None` until the next job reconnects.
    connection: Option<Connection>,
    /// Set once the client has gone away, after which nothing more is sent.
    retired: Arc<AtomicBool>,
}

/// Why one exchange produced no answer.
enum Failed {
    /// The request was refused before any of it went out, so the connection is untouched.
    Refused(String),
    /// Something went wrong on the connection, which cannot be trusted with the next request.
    Broken(String),
}

/// Why a request got no answer from the server.
enum Unanswered {
    /// There was no connection to be had, which only whoever sent the request is told.
    Unreachable(String),
    /// The connection broke with the request on it, which the game has already been told.
    Lost(String),
}

impl Unanswered {
    /// What went wrong, for whoever waits on the request.
    fn into_message(self) -> String {
        match self {
            Self::Unreachable(message) | Self::Lost(message) => message,
        }
    }
}

/// What woke the worker up.
enum Wake {
    /// The game wants a request sent.
    Job(Job),
    /// The server said something with no request outstanding.
    Unsolicited(Result<ControlMessage, RecvError>),
    /// The game dropped its end, so the worker is done.
    Stopped,
}

impl Worker {
    /// Create a new worker with the given channels.
    fn new(
        endpoint: Endpoint,
        jobs: flume::Receiver<Job>,
        replies: flume::Sender<Reply>,
        retired: Arc<AtomicBool>,
    ) -> Self {
        Self {
            endpoint,
            jobs,
            replies,
            connection: None,
            retired,
        }
    }

    /// Serve jobs until the game thread drops its end.
    ///
    /// Jobs still queued then are dropped unsent, which answers whoever waits on them with an error.
    async fn run(mut self) {
        loop {
            // A connection there is none of has nothing to wait on, and a channel that has closed would answer at
            // once, so the arm only exists while one is up.
            let wake = match self.connection.as_ref() {
                Some(Connection { from_server, .. }) => tokio::select! {
                    job = self.jobs.recv_async() => match job {
                        Ok(job) => Wake::Job(job),
                        Err(_) => Wake::Stopped,
                    },
                    message = from_server.recv_async() => Wake::Unsolicited(message),
                },
                None => match self.jobs.recv_async().await {
                    Ok(job) => Wake::Job(job),
                    Err(_) => Wake::Stopped,
                },
            };

            if self.retired.load(Ordering::Acquire) {
                break;
            }

            match wake {
                Wake::Stopped => break,
                Wake::Unsolicited(message) => self.on_unsolicited(message),
                Wake::Job(Job {
                    request,
                    waiter: Some(Waiter { answer, claim }),
                }) => {
                    if !take(&claim) {
                        continue;
                    }

                    let exchanged = self.exchange(request).await;
                    let _ = answer.send(exchanged.unwrap_or_else(|failed| error(failed.into_message())));
                },
                Wake::Job(Job { request, waiter: None }) => match self.exchange(request).await {
                    Ok(response) => {
                        if let Some(failure) = first_failure(response) {
                            self.report(failure);
                        }
                    },
                    Err(Unanswered::Unreachable(message)) => {
                        self.report(message);
                    },
                    Err(Unanswered::Lost(_)) => {},
                },
            }
        }
    }

    /// Serve on a runtime of this thread's own, until the game thread drops its end.
    fn serve(self, runtime: runtime::Runtime) { runtime.block_on(self.run()) }

    /// Send one request and wait for its answer, connecting first if the last exchange broke the connection.
    ///
    /// Fails when there is no connection to be had or this exchange broke it. A connection that breaks is reported to
    /// the game here, whichever request found it broken; one that could not be made is left to the caller.
    async fn exchange(&mut self, request: ControlRequest) -> Result<ControlResponse, Unanswered> {
        if self.connection.is_none() {
            match sada_ipc::connect(&self.endpoint).await {
                Ok((to_server, from_server)) => {
                    self.connection = Some(Connection { to_server, from_server });
                },
                Err(err) => {
                    return Err(Unanswered::Unreachable(format!(
                        "failed to connect to {}: {}",
                        self.endpoint,
                        describe(&err)
                    )));
                },
            }
        }

        match self.exchange_on(request).await {
            Ok(response) => Ok(response),
            Err(Failed::Refused(message)) => Ok(error(message)),
            // Half of a frame may have gone out, so the connection is no longer trustworthy. Drop it and let the next
            // job reconnect; retrying this one could apply it twice.
            Err(Failed::Broken(message)) => {
                self.connection = None;
                Err(Unanswered::Lost(self.report(message)))
            },
        }
    }

    /// Perform one exchange over the current connection.
    async fn exchange_on(&self, request: ControlRequest) -> Result<ControlResponse, Failed> {
        let connection = self.connection.as_ref().expect("just connected");

        connection.to_server.send_async(request).await.map_err(|err| {
            let message = describe(&err);

            match err {
                // A request too large to encode never reached the wire, so the connection is still good and the next
                // one can use it. Losing it here would cost a reconnect and, through the failure the game is told
                // about, a re-description of every player.
                SendError::Encode(_) => Failed::Refused(format!("control request refused: {message}")),
                _ => Failed::Broken(format!("failed to send control request: {message}")),
            }
        })?;

        match timeout(IO_TIMEOUT, self.answer_to(connection)).await {
            Ok(response) => response,
            Err(_) => Err(Failed::Broken("the server did not answer in time".to_owned())),
        }
    }

    /// Wait for the answer to the request just sent, forwarding whatever arrives ahead of it.
    async fn answer_to(&self, connection: &Connection) -> Result<ControlResponse, Failed> {
        loop {
            match connection.from_server.recv_async().await {
                Ok(ControlMessage::Response(response)) => return Ok(response),
                Ok(ControlMessage::Events(events)) => self.forward_events(events),
                Err(RecvError::Closed) => {
                    return Err(Failed::Broken("the server closed the control channel".to_owned()));
                },
                Err(err) => {
                    return Err(Failed::Broken(format!(
                        "failed to read the control response: {}",
                        describe(&err)
                    )));
                },
            }
        }
    }

    /// React to something the server said with no request outstanding.
    fn on_unsolicited(&mut self, message: Result<ControlMessage, RecvError>) {
        match message {
            Ok(ControlMessage::Events(events)) => self.forward_events(events),
            // One response per request is the whole of the correlation, so an unasked-for answer means the two ends
            // are out of step and nothing further on this connection can be trusted.
            Ok(ControlMessage::Response(_)) => self.lost("the server answered a request that was never sent"),
            Err(RecvError::Closed) => self.lost("the server closed the control channel"),
            Err(err) => self.lost(format!("the control channel failed: {}", describe(&err))),
        }
    }

    /// Hand pushed events to the game thread.
    fn forward_events(&self, events: Vec<ControlEvent>) { let _ = self.replies.send(Reply::Events(events)); }

    /// Drop the connection and tell the game why.
    fn lost(&mut self, reason: impl Into<String>) {
        self.connection = None;
        self.report(reason.into());
    }

    /// Tell the game about a failure nobody is waiting for, handing the message back.
    fn report(&self, message: String) -> String {
        let _ = self.replies.send(Reply::Failure(message.clone()));
        message
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

/// Render an error together with everything that caused it.
///
/// Only the message reaches DM, so a cause chain that stopped at the outermost error would lose the part that says
/// what actually went wrong.
fn describe(err: &dyn Error) -> String {
    let mut message = err.to_string();
    let mut source = err.source();

    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }

    message
}

/// The first failure in a response, looking inside a batch.
///
/// A batch answers with one response per element, so a rejected patch sitting among successes would otherwise leave
/// no trace at all: the batch itself succeeded.
fn first_failure(response: ControlResponse) -> Option<String> {
    match response {
        ControlResponse::Error { message } => Some(message),
        ControlResponse::Batch(responses) => responses.into_iter().find_map(first_failure),
        _ => None,
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

/// Start the worker, replacing any that is running, and ask the server for its version.
///
/// A client this replaces is stopped as by [`stop`], and the new worker starts only once the last one has finished.
pub fn init(endpoint: &str) -> impl Future<Output = ControlResponse> + Send + 'static + use<> {
    let started = Client::start(Endpoint::new(endpoint), LAST_WORKER.take()).map(|(client, worker)| {
        LAST_WORKER.set(Some(worker));
        CONTROL.set(Some(client));
        ControlRequest::Version
    });

    ask(started)
}

/// Stop the worker.
///
/// The worker finishes the exchange it is in, if any, and then exits, and its runtime goes with it. Everything not yet
/// sent is dropped: queued requests, whose callers are answered with an error, the pending batch and any unreported
/// failures.
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
    player: &str,
    ckey: &str,
) -> impl Future<Output = ControlResponse> + Send + 'static + use<> {
    let request = parse_player(player).map(|player| ControlRequest::RegisterCode {
        code: AuthCode::from(code),
        player,
        ckey: Ckey::from(ckey),
    });

    ask(request)
}

/// Look up the session a player has authenticated, if any.
pub fn check_auth(player: &str) -> impl Future<Output = ControlResponse> + Send + 'static + use<> {
    ask(parse_player(player).map(|player| ControlRequest::CheckAuth { player }))
}

/// Submit `request` to the installed client, or answer at once with why it could not be built or submitted.
fn ask(request: Result<ControlRequest, String>) -> impl Future<Output = ControlResponse> + Send + 'static {
    answered(request.and_then(|request| with_control(|control| control.submit(request))?))
}

/// The answer `submitted` resolves to, or why it could not be submitted at all.
async fn answered(submitted: Result<impl Future<Output = ControlResponse>, String>) -> ControlResponse {
    match submitted {
        Ok(answer) => answer.await,
        Err(message) => error(message),
    }
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

/// Add one player's state delta to the batch that the next [`flush`] will send.
///
/// Nothing reaches the channel until [`flush`]; the error says why the patch was refused outright.
pub fn patch_player(player: &str, patch: PlayerPatch) -> Result<(), String> {
    // Reporting a dropped patch as accepted would leave the game believing it had described a player it never did,
    // and there is no error queue to fall back on when there is no client at all.
    with_control(|control| control.patch_player(player, patch))?
}

/// Send everything [`patch_player`] has piled up as one batch.
pub fn flush() { let _ = with_control(Client::flush); }

/// Forget a player entirely.
///
/// This also drops the player's authentication on the server, so it belongs to a client going away rather than to a
/// player changing mobs.
pub fn remove_player(player: &str) {
    let Ok(player) = parse_player(player) else { return };

    // The id goes back to the pool whether or not the removal reaches the server: the game is done with this player.
    player::forget(player);

    let _ = with_control(|control| control.remove_player(player));
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        sync::{
            atomic::{AtomicU32, Ordering},
            mpsc,
        },
        time::Instant,
    };

    use sada_common::{PROTOCOL_VERSION, PlayerId, Position, SessionId};
    use sada_ipc::Listener;

    use super::*;

    /// How long a test waits for the worker before giving up.
    const PATIENCE: Duration = Duration::from_secs(5);

    /// Distinguishes the endpoint of one test from another's.
    static NEXT_ENDPOINT: AtomicU32 = AtomicU32::new(0);

    /// Ckey the fake server refuses to patch, so a test can put a failure inside a batch.
    const POISON: &str = "poison";

    /// What a fake server does before it hangs up.
    #[derive(Default)]
    struct Plan {
        /// How many requests it answers.
        answers: usize,
        /// Player whose patches it refuses, so a failure can be buried in a batch.
        poison: Option<PlayerId>,
        /// Events it pushes as soon as the client connects.
        push: Vec<ControlEvent>,
    }

    /// An endpoint no other test uses.
    fn endpoint() -> Endpoint {
        let id = NEXT_ENDPOINT.fetch_add(1, Ordering::Relaxed);

        Endpoint::from(std::env::temp_dir().join(format!("sada-client-test-{}-{id}.sock", std::process::id())))
    }

    /// Mint the id for `ckey` the way an export would, and render the token the game passes back.
    ///
    /// The registry is thread-local and every test runs on its own thread, so one test's ids never meet another's.
    fn player(ckey: &str) -> String { player::issue(ckey).expect("a free id").token() }

    /// Start a fake server on `endpoint` and wait for it to be listening.
    ///
    /// Every request it serves is reported on the returned receiver, so a test can assert what actually went over the
    /// wire.
    fn serve(endpoint: &Endpoint, plan: Plan) -> (thread::JoinHandle<()>, mpsc::Receiver<ControlRequest>) {
        let (ready_tx, ready_rx) = mpsc::channel();
        let (seen_tx, seen_rx) = mpsc::channel();
        let endpoint = endpoint.clone();

        let handle = thread::spawn(move || {
            let runtime = runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a runtime for the fake server");

            runtime.block_on(fake_server(endpoint, plan, ready_tx, seen_tx));
        });

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
    fn start(endpoint: &Endpoint) -> Client { Client::start(endpoint.clone(), None).expect("the worker starts").0 }

    /// Drive `future` to completion on a runtime of the test's own, as the async exports' runtime would.
    fn block_on<F: Future>(future: F) -> F::Output {
        runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime for the test")
            .block_on(future)
    }

    /// Send `request` and wait for its answer, the way an awaited export does.
    fn ask(control: &Client, request: ControlRequest) -> ControlResponse {
        let answer = answered(control.submit(request));
        let answer = block_on(async { timeout(PATIENCE, answer).await });
        answer.expect("the worker never answered")
    }

    /// Ask the server for its version, which every exchange with a fake server starts with.
    fn version(control: &Client) -> ControlResponse { ask(control, ControlRequest::Version) }

    /// Ask for the session bound to `player`.
    fn check_auth(control: &Client, player: &str) -> ControlResponse {
        let player = PlayerId::from_token(player).expect("a player token");
        ask(control, ControlRequest::CheckAuth { player })
    }

    /// Wait for a failure nobody was waiting for.
    fn wait_for_error(control: &mut Client) -> String {
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
    fn wait_for_events(control: &mut Client) -> Vec<ControlEvent> {
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
    fn muted() -> PlayerPatch {
        PlayerPatch {
            mute: Some(true),
            ..PlayerPatch::default()
        }
    }

    #[test]
    fn a_request_is_answered() {
        let endpoint = endpoint();
        let (server, _seen) = serve(
            &endpoint,
            Plan {
                answers: 2,
                ..Plan::default()
            },
        );

        let control = start(&endpoint);
        assert!(matches!(version(&control), ControlResponse::Version { .. }));
        assert!(matches!(
            check_auth(&control, &player("sefa")),
            ControlResponse::Session { session: None }
        ));

        drop(control);
        server.join().expect("the fake server does not panic");
    }

    #[test]
    fn the_worker_reconnects_after_the_server_restarts() {
        let endpoint = endpoint();
        let (first, _seen) = serve(
            &endpoint,
            Plan {
                answers: 1,
                ..Plan::default()
            },
        );

        let mut control = start(&endpoint);
        assert!(matches!(version(&control), ControlResponse::Version { .. }));
        first.join().expect("the fake server does not panic");

        let sefa = player("sefa");

        assert!(
            matches!(check_auth(&control, &sefa), ControlResponse::Error { .. }),
            "a request sent to a server that has gone away has to fail rather than hang"
        );

        // The connection that request found broken is lost for everybody, and the game can only start over if it
        // hears of that too, whoever happened to be waiting.
        wait_for_error(&mut control);

        let (second, _seen) = serve(
            &endpoint,
            Plan {
                answers: 1,
                ..Plan::default()
            },
        );
        assert!(
            matches!(check_auth(&control, &sefa), ControlResponse::Session { session: None }),
            "the next request has to reconnect on its own"
        );

        drop(control);
        second.join().expect("the fake server does not panic");
    }

    #[test]
    fn patches_travel_as_one_batch() {
        let endpoint = endpoint();
        let (server, seen) = serve(
            &endpoint,
            Plan {
                answers: 2,
                ..Plan::default()
            },
        );

        let mut control = start(&endpoint);
        version(&control);
        seen.recv().expect("the version request");

        let sefa = player("sefa");
        let patch = PlayerPatch {
            mute: Some(false),
            position: Some(Position { x: 4, y: 9, z: 2 }),
            ..PlayerPatch::default()
        };

        assert!(control.patch_player(&sefa, patch).is_ok());
        assert!(control.patch_player(&player("ahmet"), muted()).is_ok());
        control.flush();

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

        drop(control);
        server.join().expect("the fake server does not panic");
    }

    #[test]
    fn a_patch_for_something_that_is_not_a_player_is_refused_on_the_spot() {
        // The patch itself is decoded in `PlayerPatch::try_from`, which needs BYOND loaded and so is exercised by
        // the DM harness instead; what a refusal has to do either way is reach the game rather than vanish here.
        let endpoint = endpoint();
        let (server, seen) = serve(
            &endpoint,
            Plan {
                answers: 1,
                ..Plan::default()
            },
        );

        let mut control = start(&endpoint);
        version(&control);
        seen.recv().expect("the version request");

        assert!(control.patch_player("", muted()).is_err(), "no player at all");
        assert!(
            control.patch_player("sefa", muted()).is_err(),
            "a ckey where a token belongs"
        );
        control.flush();

        // Nothing was queued, so flush has nothing to send and the server sees no second request.
        assert!(seen.recv_timeout(Duration::from_millis(200)).is_err());

        drop(control);
        server.join().expect("the fake server does not panic");
    }

    #[test]
    fn a_failure_inside_a_batch_reaches_the_game() {
        let endpoint = endpoint();
        let poison = player::issue(POISON).expect("a free id");
        let (server, _seen) = serve(
            &endpoint,
            Plan {
                answers: 2,
                poison: Some(poison),
                ..Plan::default()
            },
        );

        let mut control = start(&endpoint);
        version(&control);

        // The batch itself succeeds; only one of its elements is rejected.
        assert!(control.patch_player(&player("sefa"), muted()).is_ok());
        assert!(control.patch_player(&poison.token(), muted()).is_ok());
        control.flush();

        let message = wait_for_error(&mut control);
        assert!(message.contains("cannot be patched"), "got {message}");

        drop(control);
        server.join().expect("the fake server does not panic");
    }

    #[test]
    fn a_removal_travels_behind_the_batch() {
        let endpoint = endpoint();
        let (server, seen) = serve(
            &endpoint,
            Plan {
                answers: 2,
                ..Plan::default()
            },
        );

        let mut control = start(&endpoint);
        version(&control);
        seen.recv().expect("the version request");

        // A patch left in the batch and then a removal. Order is the whole point: were the removal to overtake the
        // patch, the patch would land on a vacant entry and bring the player back for the rest of the round.
        let sefa = player("sefa");
        assert!(control.patch_player(&sefa, muted()).is_ok());
        control.remove_player(PlayerId::from_token(&sefa).expect("a player token"));

        let batch = seen.recv_timeout(PATIENCE).expect("the removal");
        let ControlRequest::Batch(requests) = batch else {
            panic!("the removal has to travel behind the batch, got {batch:?}");
        };

        assert!(matches!(&requests[0], ControlRequest::PatchPlayer { player, .. } if player.token() == sefa));
        assert!(matches!(&requests[1], ControlRequest::RemovePlayer { player } if player.token() == sefa));

        drop(control);
        server.join().expect("the fake server does not panic");
    }

    #[test]
    fn pushed_events_reach_the_game() {
        // Nothing asks for these: the server sends them when it has them and the game takes whatever has arrived.
        let endpoint = endpoint();
        let pushed = ControlEvent::Authenticated {
            player: PlayerId::from_raw(7),
            session: SessionId::new(1, 1),
        };
        let (server, _seen) = serve(
            &endpoint,
            Plan {
                answers: 1,
                push: vec![pushed.clone()],
                ..Plan::default()
            },
        );

        let mut control = start(&endpoint);
        version(&control);

        assert_eq!(wait_for_events(&mut control), vec![pushed]);
        assert!(control.take_events(32).is_empty(), "events are handed over once");

        drop(control);
        server.join().expect("the fake server does not panic");
    }

    #[test]
    fn a_batch_too_large_to_encode_does_not_cost_the_connection() {
        // Nothing reached the wire, so losing the connection here would buy a reconnect and, through the failure the
        // game is told about, a re-description of every player, which is how the same oversized batch comes back.
        let endpoint = endpoint();
        let (server, seen) = serve(
            &endpoint,
            Plan {
                answers: 2,
                ..Plan::default()
            },
        );

        let mut control = start(&endpoint);
        assert!(matches!(version(&control), ControlResponse::Version { .. }));
        seen.recv().expect("the version request");

        let bulky = PlayerPatch {
            known_languages: Some(vec!["x".repeat(1024)]),
            ..PlayerPatch::default()
        };

        for id in 1..1200 {
            let player = PlayerId::from_raw(id).token();
            assert!(control.patch_player(&player, bulky.clone()).is_ok());
        }
        control.flush();

        let message = wait_for_error(&mut control);
        assert!(message.contains("refused"), "got {message}");

        assert!(
            matches!(
                check_auth(&control, &player("sefa")),
                ControlResponse::Session { session: None }
            ),
            "the connection the refused batch never touched is still good"
        );

        drop(control);
        server.join().expect("the fake server does not panic");
    }

    #[test]
    fn the_event_queue_keeps_the_newest_of_an_oversized_frame() {
        let (jobs, _held) = flume::bounded(1);
        let (_replies, answers) = flume::unbounded();
        let mut control = Client::new(jobs, answers);

        let events = Vec::from_iter(
            (0..MAX_QUEUED_EVENTS as u32 + 10).map(|player| ControlEvent::Disconnected {
                player: PlayerId::from_raw(player + 1),
                session: SessionId::new(1, 1),
            }),
        );

        control.push_events(events);

        let taken = control.take_events(MAX_QUEUED_EVENTS * 2);

        assert_eq!(taken.len(), MAX_QUEUED_EVENTS);
        assert_eq!(
            taken[0],
            ControlEvent::Disconnected {
                player: PlayerId::from_raw(11),
                session: SessionId::new(1, 1),
            }
        );
    }

    #[test]
    fn a_failed_fire_and_forget_leaves_an_error() {
        let endpoint = endpoint();

        // No server at all, nothing is listening on this endpoint.
        let mut control = start(&endpoint);
        control.set_transmit(SessionId::new(1, 1), None);

        wait_for_error(&mut control);

        drop(control);
    }

    #[test]
    fn a_batch_that_cannot_be_queued_leaves_an_error() {
        // The batch is gone from `pending` the moment `flush` takes it, and the game has already recorded those
        // fields as sent, so a batch that never reaches the worker has to surface here or those players keep a stale
        // description for the rest of the round.
        let (jobs, held) = flume::bounded(0);
        let (_replies, answers) = flume::unbounded();
        let mut control = Client::new(jobs, answers);

        assert!(control.patch_player(&player("sefa"), muted()).is_ok());
        control.flush();

        let message = control.take_error().expect("the lost batch leaves an error");
        assert!(message.contains("full"), "got {message}");

        drop(held);
    }

    #[test]
    fn a_request_the_worker_drops_is_answered_with_an_error() {
        // A worker that goes away with the job in hand, as it does when it cannot be kept running, must not leave the
        // proc waiting on the answer asleep for good.
        let (jobs, worker) = flume::bounded(1);
        let (_replies, answers) = flume::unbounded();
        let control = Client::new(jobs, answers);

        let worker = thread::spawn(move || drop(worker.recv()));

        let answer = version(&control);
        assert!(
            matches!(&answer, ControlResponse::Error { message } if message.contains("stopped")),
            "got {answer:?}"
        );

        worker.join().expect("the worker does not panic");
    }

    #[test]
    fn a_waited_request_on_a_full_queue_is_answered_at_once() {
        // Waiting for room would leave the proc asleep behind every exchange a wedged server times out on.
        let (jobs, held) = flume::bounded(0);
        let (_replies, answers) = flume::unbounded();
        let control = Client::new(jobs, answers);

        let answer = version(&control);
        assert!(
            matches!(&answer, ControlResponse::Error { message } if message.contains("full")),
            "got {answer:?}"
        );

        drop(held);
    }

    /// Drive `future` on a runtime whose clock is paused, so it skips ahead to the next timer whenever it would idle.
    fn block_on_paused<F: Future>(future: F) -> F::Output {
        runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .expect("a runtime for the test")
            .block_on(future)
    }

    #[test]
    fn a_request_the_worker_never_gets_to_is_answered_in_time_and_never_sent() {
        // A worker stuck behind a wedged server holds on to the job and never answers it.
        let (jobs, held) = flume::bounded(1);
        let (_replies, answers) = flume::unbounded();
        let control = Client::new(jobs, answers);

        let answer = block_on_paused(control.submit(ControlRequest::Version).expect("room in the queue"));

        assert!(
            matches!(&answer, ControlResponse::Error { message } if message.contains("in time")),
            "got {answer:?}"
        );

        // Sent after its caller was told it did not come, a registration would retire the code the player still has.
        let job = held.try_recv().expect("the job is still queued");
        let waiter = job.waiter.expect("somebody waited on it");
        assert!(!take(&waiter.claim), "the worker has to find the request given up on");
    }

    #[test]
    fn a_request_the_worker_has_started_on_is_waited_for_past_the_deadline() {
        let (jobs, held) = flume::bounded(1);
        let (_replies, answers) = flume::unbounded();
        let control = Client::new(jobs, answers);

        let answer = control.submit(ControlRequest::Version).expect("room in the queue");

        let job = held.try_recv().expect("the job is queued");
        let Waiter { answer: reply, claim } = job.waiter.expect("somebody waits on it");
        assert!(take(&claim), "nobody has given up on it yet");

        let answer = block_on_paused(async move {
            tokio::spawn(async move {
                tokio::time::sleep(ANSWER_TIMEOUT * 2).await;
                let _ = reply.send(ControlResponse::Ok);
            });

            answer.await
        });

        assert!(matches!(answer, ControlResponse::Ok), "got {answer:?}");
    }

    #[test]
    fn a_waited_request_that_cannot_connect_leaves_no_error() {
        // Nothing was lost, so only the caller hears of it; telling the game too would re-describe every player.
        let endpoint = endpoint();
        let mut control = start(&endpoint);

        let answer = version(&control);
        assert!(
            matches!(&answer, ControlResponse::Error { message } if message.contains("failed to connect")),
            "got {answer:?}"
        );

        // The worker answers after anything it reports, so a report would already be waiting.
        assert_eq!(control.take_error(), None);
    }

    #[test]
    fn a_retired_worker_sends_nothing_more() {
        // With nothing listening, a request that went out would be answered with the failed connect instead.
        let endpoint = endpoint();
        let (jobs, queued) = flume::bounded(1);
        let (replies, answers) = flume::unbounded();
        let control = Client::new(jobs, answers);
        let worker = Worker::new(endpoint, queued, replies, control.retired.clone());

        let answer = control.submit(ControlRequest::Version).expect("room in the queue");
        drop(control);

        let runtime = runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime for the worker");
        thread::spawn(move || worker.serve(runtime))
            .join()
            .expect("the worker does not panic");

        let answer = block_on(answer);
        assert!(
            matches!(&answer, ControlResponse::Error { message } if message.contains("stopped")),
            "got {answer:?}"
        );
    }

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
            block_on(init(endpoint.as_str())),
            ControlResponse::Version { .. }
        ));

        let sefa = player("sefa");
        let registered = register_code("ABC123", &sefa, "sefa");
        remove_player(&sefa);

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

        let answer = init(endpoint.as_str());
        stop();

        let answer = block_on(answer);
        assert!(
            matches!(&answer, ControlResponse::Error { message } if message.contains("failed to connect") || message.contains("stopped")),
            "got {answer:?}"
        );
        assert!(matches!(
            block_on(super::check_auth(&player("sefa"))),
            ControlResponse::Error { message } if message.contains("not running")
        ));
    }

    #[test]
    fn removing_a_player_frees_their_id() {
        let (first, second) = player::COLLIDING;
        let token = player(first);
        assert!(
            player::issue(second).is_none(),
            "the id is taken while its holder is in the game"
        );

        remove_player(&token);

        assert!(
            player::issue(second).is_some(),
            "the id is free once its holder is removed"
        );
    }

    #[test]
    fn nothing_is_asked_of_a_client_that_is_not_running() {
        assert_eq!(take_error(), None);
        assert!(take_events(32).is_empty());
        assert!(patch_player(&player("sefa"), muted()).is_err());
        assert!(matches!(
            block_on(super::check_auth(&player("sefa"))),
            ControlResponse::Error { .. }
        ));
    }
}
