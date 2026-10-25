//! Game-thread half of the control client.
//!
//! Nothing here waits on the worker: jobs go to it through `try_send`, and whatever it has for the game is collected
//! through `try_recv` when the game asks for errors or events.

use std::{
    collections::VecDeque,
    mem,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use sada_common::{ControlEvent, ControlRequest, ControlResponse, PlayerId, PlayerPatch, SessionId, Transmit};
use sada_ipc::Endpoint;
use tokio::{sync::oneshot, time::timeout};

use super::{
    Job,
    Reply,
    Waiter,
    error,
    take,
    worker::{self, Worker},
};

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

/// How long a request somebody waits on may wait for the worker to start on it before it is dropped unsent.
const ANSWER_TIMEOUT: Duration = worker::IO_TIMEOUT.saturating_mul(3);

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
        let (jobs_tx, jobs_rx) = flume::bounded(JOB_QUEUE_DEPTH);
        let (replies_tx, replies_rx) = flume::unbounded();

        let client = Self::new(jobs_tx, replies_rx);
        let thread = Worker::new(endpoint, jobs_rx, replies_tx, client.retired.clone()).spawn(previous)?;

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
    pub fn patch_player(&mut self, player: PlayerId, patch: PlayerPatch) {
        self.enqueue(ControlRequest::PatchPlayer { player, patch });
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
    pub fn push_orphan_error(&mut self, message: String) {
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

#[cfg(test)]
mod tests {
    use sada_common::Position;

    use super::{super::testing::*, *};

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

        control.patch_player(sefa, patch);
        control.patch_player(player("ahmet"), muted());
        control.flush();

        let batch = seen.recv_timeout(PATIENCE).expect("the batch reaches the server");
        let ControlRequest::Batch(requests) = batch else {
            panic!("patches have to be batched, got {batch:?}");
        };

        assert_eq!(requests.len(), 2, "both patches belong to the same frame");
        assert!(matches!(
            &requests[0],
            ControlRequest::PatchPlayer { player, patch }
                if *player == sefa
                    && patch.mute == Some(false)
                    && patch.position.is_some_and(|at| (at.x, at.y, at.z) == (4, 9, 2))
        ));

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
        control.patch_player(sefa, muted());
        control.remove_player(sefa);

        let batch = seen.recv_timeout(PATIENCE).expect("the removal");
        let ControlRequest::Batch(requests) = batch else {
            panic!("the removal has to travel behind the batch, got {batch:?}");
        };

        assert!(matches!(&requests[0], ControlRequest::PatchPlayer { player, .. } if *player == sefa));
        assert!(matches!(&requests[1], ControlRequest::RemovePlayer { player } if *player == sefa));

        drop(control);
        server.join().expect("the fake server does not panic");
    }

    #[test]
    fn the_event_queue_keeps_the_newest_of_an_oversized_frame() {
        let (jobs, _held) = flume::bounded(1);
        let (_replies, answers) = flume::unbounded();
        let mut control = Client::new(jobs, answers);

        let events = Vec::from_iter(
            (0..MAX_QUEUED_EVENTS as u64 + 10).map(|player| ControlEvent::Disconnected {
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
    fn a_batch_that_cannot_be_queued_leaves_an_error() {
        // The batch is gone from `pending` the moment `flush` takes it, and the game has already recorded those
        // fields as sent, so a batch that never reaches the worker has to surface here or those players keep a stale
        // description for the rest of the round.
        let (jobs, held) = flume::bounded(0);
        let (_replies, answers) = flume::unbounded();
        let mut control = Client::new(jobs, answers);

        control.patch_player(player("sefa"), muted());
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
}
