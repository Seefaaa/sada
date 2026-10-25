//! Channel-owning half of the control client.
//!
//! The worker runs on a thread of its own with a `current_thread` runtime of its own, takes one job at a time.

use std::{
    error::Error,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use sada_common::{ControlEvent, ControlMessage, ControlRequest, ControlResponse};
use sada_ipc::{Endpoint, Receiver, RecvError, SendError, Sender};
use tokio::{runtime, time::timeout};

use super::{Job, Reply, Waiter, error, take};

/// How long the worker waits on a wedged server before dropping the connection and reconnecting.
pub const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// The two ends of one control channel.
struct Connection {
    /// Requests out.
    to_server: Sender<ControlRequest>,
    /// Answers and events in.
    from_server: Receiver<ControlMessage>,
}

/// Channel-owning half of the control client.
pub struct Worker {
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
    pub fn new(
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

    /// Serve jobs on a thread and a runtime of the worker's own, until the game thread drops its end.
    ///
    /// The worker starts once `previous`, the thread of the worker it replaces, has finished.
    pub fn spawn(self, previous: Option<thread::JoinHandle<()>>) -> Result<thread::JoinHandle<()>, String> {
        let runtime = runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|err| format!("failed to build the control runtime: {err}"))?;

        thread::Builder::new()
            .name("sada-control".to_owned())
            .spawn(move || {
                if let Some(previous) = previous {
                    let _ = previous.join();
                }
                runtime.block_on(self.run())
            })
            .map_err(|err| format!("failed to spawn the control worker: {err}"))
    }

    /// Serve jobs until the game thread drops its end.
    ///
    /// Jobs still queued then are dropped unsent, which answers whoever waits on them with an error.
    async fn run(mut self) {
        loop {
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
                    Err(Unanswered::Unreachable(message)) => self.report(message),
                    Err(Unanswered::Lost(_)) => {},
                },
            }
        }
    }

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

#[cfg(test)]
mod tests {
    use sada_common::{PlayerId, PlayerPatch, SessionId};
    use tokio::sync::oneshot;

    use super::{super::testing::*, *};

    const POISON: &str = "poison";

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
            check_auth(&control, player("sefa")),
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
            matches!(check_auth(&control, sefa), ControlResponse::Error { .. }),
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
            matches!(check_auth(&control, sefa), ControlResponse::Session { session: None }),
            "the next request has to reconnect on its own"
        );

        drop(control);
        second.join().expect("the fake server does not panic");
    }

    #[test]
    fn a_failure_inside_a_batch_reaches_the_game() {
        let endpoint = endpoint();
        let poison = crate::player::issue(POISON).expect("a free id");
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
        control.patch_player(player("sefa"), muted());
        control.patch_player(poison, muted());
        control.flush();

        let message = wait_for_error(&mut control);
        assert!(message.contains("cannot be patched"), "got {message}");

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
            let player = PlayerId::from_raw(id);
            control.patch_player(player, bulky.clone());
        }
        control.flush();

        let message = wait_for_error(&mut control);
        assert!(message.contains("refused"), "got {message}");

        assert!(
            matches!(
                check_auth(&control, player("sefa")),
                ControlResponse::Session { session: None }
            ),
            "the connection the refused batch never touched is still good"
        );

        drop(control);
        server.join().expect("the fake server does not panic");
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
        let (jobs, queued) = flume::bounded(1);
        let (replies, _replies) = flume::unbounded();
        let (answer, mut answered) = oneshot::channel();

        jobs.send(Job {
            request: ControlRequest::Version,
            waiter: Some(Waiter {
                answer,
                claim: Arc::default(),
            }),
        })
        .expect("room in the queue");

        // The client sets this as it goes away, which is what the worker has to find before sending.
        let retired = Arc::new(AtomicBool::new(true));

        Worker::new(endpoint(), queued, replies, retired)
            .spawn(None)
            .expect("the worker starts")
            .join()
            .expect("the worker does not panic");

        assert!(
            matches!(answered.try_recv(), Err(oneshot::error::TryRecvError::Closed)),
            "the request has to be dropped unanswered"
        );
    }
}
