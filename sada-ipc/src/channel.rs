//! The two ends of a channel, and the pumps that keep them fed.

use std::{
    marker::PhantomData,
    sync::{Arc, OnceLock},
    time::Duration,
};

use sada_utils::shutdown::Shutdown;
use serde::{Serialize, de::DeserializeOwned};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt as _},
    sync::oneshot,
};

use crate::{
    ChannelConfig,
    frame::{self, FrameBuffer},
};

/// One thing the write pump is asked to do.
enum Outgoing {
    /// Write this frame, length prefix included.
    Frame(Vec<u8>),
    /// Answer this once everything queued ahead of it has been written.
    Fence(oneshot::Sender<()>),
}

/// The sending end of a channel.
///
/// Cloning yields another handle on the same channel, and the frames two handles send never interleave: one task owns
/// the transport and writes them one after another.
///
/// A send that returns `Ok` has queued the value rather than written it, so a transport failure is reported on the
/// receiving end rather than here. What is reported here is a value that could not be encoded, which leaves the channel
/// unharmed, and a channel that has closed.
#[derive(Debug)]
pub struct Sender<T> {
    /// Encoded frames, waiting for the write pump.
    frames: flume::Sender<Outgoing>,
    /// Largest frame this channel will send.
    max_frame_len: u32,
    /// Shared end of life of the channel.
    state: Arc<State>,
    /// The type this end sends, which it only ever encodes.
    marker: PhantomData<T>,
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self {
            frames: self.frames.clone(),
            max_frame_len: self.max_frame_len,
            state: Arc::clone(&self.state),
            marker: PhantomData,
        }
    }
}

impl<T: Serialize> Sender<T> {
    /// Send `value`, waiting for room in the queue.
    ///
    /// Blocks the calling thread, so it belongs on a thread of its own rather than in a task: on a current thread
    /// runtime it would park the pump it is waiting for.
    pub fn send(&self, value: T) -> Result<(), SendError> {
        self.frames.send(self.encode(&value)?).map_err(|_| self.closed())
    }

    /// Wait until everything queued so far has reached the transport.
    ///
    /// An end that is about to go away has to flush, because dropping it with frames still queued closes the transport
    /// under them.
    ///
    /// Blocks the calling thread; see [`Sender::send`] for where that is allowed.
    pub fn flush(&self) -> Result<(), SendError> {
        let (reply, written) = oneshot::channel();

        self.frames.send(Outgoing::Fence(reply)).map_err(|_| self.closed())?;

        written.blocking_recv().map_err(|_| self.closed())
    }

    /// Wait until everything queued so far has reached the transport.
    ///
    /// Behaves exactly as [`Sender::flush`].
    pub async fn flush_async(&self) -> Result<(), SendError> {
        let (reply, written) = oneshot::channel();

        self.frames
            .send_async(Outgoing::Fence(reply))
            .await
            .map_err(|_| self.closed())?;

        written.await.map_err(|_| self.closed())
    }

    /// Send `value`, waiting for room in the queue.
    pub async fn send_async(&self, value: T) -> Result<(), SendError> {
        self.frames
            .send_async(self.encode(&value)?)
            .await
            .map_err(|_| self.closed())
    }

    /// Send `value` if the queue has room for it right now.
    pub fn try_send(&self, value: T) -> Result<(), SendError> {
        self.frames.try_send(self.encode(&value)?).map_err(|err| match err {
            flume::TrySendError::Full(_) => SendError::Full,
            flume::TrySendError::Disconnected(_) => self.closed(),
        })
    }

    /// Turn `value` into the frame that carries it.
    fn encode(&self, value: &T) -> Result<Outgoing, SendError> {
        Ok(Outgoing::Frame(frame::encode(value, self.max_frame_len)?))
    }
}

impl<T> Sender<T> {
    /// Why the channel is no longer taking values.
    fn closed(&self) -> SendError {
        match self.state.cause() {
            Some(cause) => SendError::Failed(cause),
            None => SendError::Closed,
        }
    }
}

/// The receiving end of a channel.
///
/// Values that already arrived are handed out before the channel reports that it has closed, so nothing the peer
/// managed to send is lost to its hanging up.
#[derive(Debug)]
pub struct Receiver<T> {
    /// Values the read pump has decoded.
    values: flume::Receiver<T>,
    /// Shared end of life of the channel.
    state: Arc<State>,
}

impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        Self {
            values: self.values.clone(),
            state: Arc::clone(&self.state),
        }
    }
}

impl<T> Receiver<T> {
    /// Take the next value, waiting for one to arrive.
    ///
    /// Blocks the calling thread; see [`Sender::send`] for where that is allowed.
    pub fn recv(&self) -> Result<T, RecvError> { self.values.recv().map_err(|_| self.closed()) }

    /// Take the next value, waiting for one to arrive.
    pub async fn recv_async(&self) -> Result<T, RecvError> { self.values.recv_async().await.map_err(|_| self.closed()) }

    /// Take the next value if one has already arrived.
    pub fn try_recv(&self) -> Result<T, RecvError> {
        self.values.try_recv().map_err(|err| match err {
            flume::TryRecvError::Empty => RecvError::Empty,
            flume::TryRecvError::Disconnected => self.closed(),
        })
    }

    /// Take the next value, waiting at most `timeout` for one.
    ///
    /// Blocks the calling thread; see [`Sender::send`] for where that is allowed.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<T, RecvError> {
        self.values.recv_timeout(timeout).map_err(|err| match err {
            flume::RecvTimeoutError::Timeout => RecvError::Timeout,
            flume::RecvTimeoutError::Disconnected => self.closed(),
        })
    }

    /// Why the channel has no more values coming.
    fn closed(&self) -> RecvError {
        match self.state.cause() {
            Some(cause) => RecvError::Failed(cause),
            None => RecvError::Closed,
        }
    }
}

/// Shared end of life of one channel.
///
/// Either pump stopping takes the other down with it: the channel closes as a whole, in both directions. A named pipe
/// has no way to express the half-closed state a unix socket can, so this is the one behaviour both platforms keep.
#[derive(Debug)]
struct State {
    /// Why the channel closed, set once by whichever pump failed first, and absent when nothing failed.
    cause: OnceLock<Arc<crate::Error>>,
    /// Signal that takes both pumps down together.
    shutdown: Shutdown,
}

impl State {
    /// Create the state of a channel that is still open.
    fn new() -> Self {
        Self {
            cause: OnceLock::new(),
            shutdown: Shutdown::new(),
        }
    }

    /// Record `error` as the reason the channel is about to close.
    fn fail(&self, error: crate::Error) { let _ = self.cause.set(Arc::new(error)); }

    /// Close the channel, whether or not anything went wrong on it.
    fn close(&self) { self.shutdown.trigger() }

    /// The failure that closed the channel, if it was one.
    fn cause(&self) -> Option<Arc<crate::Error>> { self.cause.get().map(Arc::clone) }
}

/// Put `stream` behind the two ends of a channel.
///
/// Must be called from within a tokio runtime, which drives the pumps for as long as either end is held.
pub(crate) fn spawn<S, Tx, Rx>(stream: S, config: ChannelConfig) -> (Sender<Tx>, Receiver<Rx>)
where
    S: AsyncRead + AsyncWrite + Send + 'static,
    Rx: DeserializeOwned + Send + 'static,
{
    let (frames, outgoing) = flume::bounded(config.capacity);
    let (incoming, values) = flume::bounded(config.capacity);
    let state = Arc::new(State::new());
    let (reader, writer) = tokio::io::split(stream);

    tokio::spawn(write_pump(writer, outgoing, Arc::clone(&state)));
    tokio::spawn(read_pump(reader, incoming, Arc::clone(&state), config.max_frame_len));

    let sender = Sender {
        frames,
        max_frame_len: config.max_frame_len,
        state: Arc::clone(&state),
        marker: PhantomData,
    };

    let receiver = Receiver { values, state };

    (sender, receiver)
}

/// Write queued frames to the transport until the channel closes.
async fn write_pump<W: AsyncWrite + Unpin>(mut writer: W, frames: flume::Receiver<Outgoing>, state: Arc<State>) {
    let mut shutdown = state.shutdown.clone();

    tokio::select! {
        () = shutdown.recv() => {},
        () = write_frames(&mut writer, &frames, &state) => {},
    }

    state.close();
}

/// Write every frame the senders queue, until they are all gone or the transport fails.
async fn write_frames<W: AsyncWrite + Unpin>(writer: &mut W, frames: &flume::Receiver<Outgoing>, state: &State) {
    while let Ok(outgoing) = frames.recv_async().await {
        let frame = match outgoing {
            Outgoing::Frame(frame) => frame,
            // Everything queued ahead of a fence has been written by the time it comes out of the queue.
            Outgoing::Fence(reply) => {
                let _ = reply.send(());
                continue;
            },
        };

        if let Err(source) = writer.write_all(&frame).await {
            state.fail(frame::Error::WriteFrame(source).into());
            return;
        }

        if let Err(source) = writer.flush().await {
            state.fail(frame::Error::Flush(source).into());
            return;
        }
    }
}

/// Decode frames off the transport until the channel closes.
async fn read_pump<R, T>(mut reader: R, values: flume::Sender<T>, state: Arc<State>, max_frame_len: u32)
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut shutdown = state.shutdown.clone();

    tokio::select! {
        () = shutdown.recv() => {},
        () = read_values(&mut reader, &values, &state, max_frame_len) => {},
    }

    state.close();
}

/// Decode every frame that arrives, until the peer hangs up or a frame cannot be trusted.
///
/// A frame that fails to decode ends the channel rather than being reported and skipped.
async fn read_values<R, T>(reader: &mut R, values: &flume::Sender<T>, state: &State, max_frame_len: u32)
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut buffer = FrameBuffer::new(max_frame_len);

    loop {
        match buffer.read(reader).await {
            Ok(Some(value)) => {
                if values.send_async(value).await.is_err() {
                    return;
                }
            },
            Ok(None) => return, // eof
            Err(source) => {
                state.fail(source.into());
                return;
            },
        }
    }
}

/// Why a value could not be sent.
#[derive(Debug, Error)]
pub enum SendError {
    /// The value could not be encoded, which leaves the channel open and usable.
    #[error(transparent)]
    Encode(#[from] frame::Error),
    /// The queue was full. Only [`Sender::try_send`] reports this.
    #[error("the channel queue is full")]
    Full,
    /// The channel closed with nothing going wrong on it, because an end of it was dropped.
    #[error("the channel is closed")]
    Closed,
    /// The channel closed because of a failure.
    #[error("the channel failed")]
    Failed(#[source] Arc<crate::Error>),
}

/// Why no value came back.
#[derive(Debug, Error)]
pub enum RecvError {
    /// Nothing had arrived yet. Only [`Receiver::try_recv`] reports this.
    #[error("the channel is empty")]
    Empty,
    /// Nothing arrived in time. Only [`Receiver::recv_timeout`] reports this.
    #[error("timed out waiting on the channel")]
    Timeout,
    /// The channel closed with nothing going wrong on it, which is how a peer hanging up cleanly arrives.
    #[error("the channel is closed")]
    Closed,
    /// The channel closed because of a failure.
    #[error("the channel failed")]
    Failed(#[source] Arc<crate::Error>),
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use serde::de::DeserializeOwned;

    use super::{Receiver, RecvError, SendError, Sender};
    use crate::{ChannelConfig, Endpoint, Error, FrameError, Listener, connect, connect_with};

    /// An endpoint no other test and no other process is using.
    fn unique_endpoint() -> Endpoint {
        static NEXT: AtomicU32 = AtomicU32::new(0);

        let name = format!(
            "sada-ipc-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );

        endpoint_named(&name)
    }

    /// Put `name` where this platform keeps test endpoints.
    #[cfg(unix)]
    fn endpoint_named(name: &str) -> Endpoint { Endpoint::from(std::env::temp_dir().join(format!("{name}.sock"))) }

    /// Put `name` where this platform keeps test endpoints.
    #[cfg(windows)]
    fn endpoint_named(name: &str) -> Endpoint { Endpoint::new(name) }

    /// A listener and the channel to one client of it, with `config` on both ends.
    ///
    /// The client connects before the listener accepts, which both platforms allow, and the listener is handed back
    /// because dropping it takes the endpoint with it.
    async fn pair<A, B>(config: ChannelConfig) -> (Listener, (Sender<A>, Receiver<B>), (Sender<B>, Receiver<A>))
    where
        A: DeserializeOwned + Send + 'static,
        B: DeserializeOwned + Send + 'static,
    {
        let endpoint = unique_endpoint();
        let mut listener = Listener::bind_with(&endpoint, config).unwrap();

        let client = connect_with::<B, A>(&endpoint, config).await.unwrap();
        let server = listener.accept::<A, B>().await.unwrap();

        (listener, server, client)
    }

    #[tokio::test]
    async fn a_value_round_trips_in_both_directions() {
        let (_listener, (to_client, from_client), (to_server, from_server)) =
            pair::<String, u32>(ChannelConfig::default()).await;

        to_server.send_async(7).await.unwrap();
        assert_eq!(from_client.recv_async().await.unwrap(), 7);

        to_client.send_async("hello".to_owned()).await.unwrap();
        assert_eq!(from_server.recv_async().await.unwrap(), "hello");
    }

    #[tokio::test]
    async fn frames_keep_their_order() {
        let (_listener, (_to_client, from_client), (to_server, _from_server)) =
            pair::<(), u32>(ChannelConfig::default()).await;

        for value in 0..64 {
            to_server.send_async(value).await.unwrap();
        }

        for expected in 0..64 {
            assert_eq!(from_client.recv_async().await.unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn two_clients_get_their_own_channels() {
        // The game uses one connection, but a debugging tool or a reconnect racing a stale connection makes two.
        let endpoint = unique_endpoint();
        let mut listener = Listener::bind(&endpoint).unwrap();

        let first = connect::<u32, u32>(&endpoint).await.unwrap();
        let first_served = listener.accept::<u32, u32>().await.unwrap();

        let second = connect::<u32, u32>(&endpoint).await.unwrap();
        let second_served = listener.accept::<u32, u32>().await.unwrap();

        first.0.send_async(1).await.unwrap();
        second.0.send_async(2).await.unwrap();

        assert_eq!(first_served.1.recv_async().await.unwrap(), 1);
        assert_eq!(second_served.1.recv_async().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn a_peer_hanging_up_closes_the_channel() {
        let (_listener, (_to_client, from_client), client) = pair::<(), u32>(ChannelConfig::default()).await;

        client.0.send_async(1).await.unwrap();
        drop(client);

        // whatever arrived before the peer left is handed over first
        assert_eq!(from_client.recv_async().await.unwrap(), 1);
        assert!(matches!(from_client.recv_async().await, Err(RecvError::Closed)));
    }

    #[tokio::test]
    async fn cloned_senders_do_not_interleave_frames() {
        let (_listener, (_to_client, from_client), (to_server, _from_server)) =
            pair::<(), (u8, u32)>(ChannelConfig::default()).await;

        let second = to_server.clone();
        let sender = tokio::spawn(async move {
            for value in 0..32 {
                second.send_async((1, value)).await.unwrap();
            }
        });

        for value in 0..32 {
            to_server.send_async((0, value)).await.unwrap();
        }

        sender.await.unwrap();

        let mut next = [0, 0];

        for _ in 0..64 {
            let (id, value) = from_client.recv_async().await.unwrap();
            assert_eq!(value, next[id as usize], "a sender's own frames arrive in order");
            next[id as usize] += 1;
        }
    }

    #[tokio::test]
    async fn an_oversized_value_leaves_the_channel_usable() {
        let config = ChannelConfig {
            max_frame_len: 64,
            capacity: 4,
        };
        let (_listener, (_to_client, from_client), (to_server, _from_server)) = pair::<(), String>(config).await;

        let refused = to_server.send_async("a".repeat(128)).await;

        assert!(matches!(
            refused,
            Err(SendError::Encode(FrameError::EncodedFrameTooLarge))
        ));

        to_server.send_async("small".to_owned()).await.unwrap();
        assert_eq!(from_client.recv_async().await.unwrap(), "small");
    }

    #[tokio::test]
    async fn a_frame_over_the_limit_ends_the_channel() {
        // A peer that announces more than this end accepts cannot be read past, so the channel closes rather than
        // carrying on out of step with it.
        let endpoint = unique_endpoint();
        let mut listener = Listener::bind_with(
            &endpoint,
            ChannelConfig {
                max_frame_len: 32,
                capacity: 4,
            },
        )
        .unwrap();

        let (to_server, _from_server) = connect::<String, ()>(&endpoint).await.unwrap();
        let (_to_client, from_client) = listener.accept::<(), String>().await.unwrap();

        to_server.send_async("a".repeat(64)).await.unwrap();

        let Err(RecvError::Failed(cause)) = from_client.recv_async().await else {
            panic!("the channel should have failed");
        };

        assert!(matches!(*cause, Error::Frame(FrameError::FrameTooLarge { .. })));
    }

    #[tokio::test]
    async fn a_flush_waits_for_what_was_queued() {
        // A send only queues, so an end whose runtime goes away straight afterwards would otherwise take frames with
        // it that the peer never sees. This is what the flush is for, and dropping the runtime is how a caller that is
        // done with a channel usually ends it.
        let endpoint = unique_endpoint();
        let mut listener = Listener::bind(&endpoint).unwrap();

        let client = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a runtime");

            runtime.block_on(async {
                let (to_server, _from_server) = connect::<u32, ()>(&endpoint).await.unwrap();

                for value in 0..16 {
                    to_server.send_async(value).await.unwrap();
                }

                to_server.flush_async().await.unwrap();
            });
        });

        let (_to_client, from_client) = listener.accept::<(), u32>().await.unwrap();

        for expected in 0..16 {
            assert_eq!(from_client.recv_async().await.unwrap(), expected);
        }

        client.join().unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_ends_can_be_used_from_a_thread_of_their_own() {
        use std::time::Duration;

        let (_listener, (to_client, from_client), (to_server, from_server)) =
            pair::<u32, u32>(ChannelConfig::default()).await;

        let game = std::thread::spawn(move || {
            to_server.send(1).unwrap();
            assert_eq!(from_server.recv_timeout(Duration::from_secs(5)).unwrap(), 2);
            assert!(matches!(from_server.try_recv(), Err(RecvError::Empty)));
        });

        assert_eq!(from_client.recv_async().await.unwrap(), 1);
        to_client.send_async(2).await.unwrap();

        game.join().unwrap();
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn a_listener_leaves_a_socket_it_did_not_bind() {
        // Whatever holds the endpoint when this listener goes away may not be the socket it bound, and taking that one
        // with it would leave its owner running and unreachable with nothing to say why.
        let endpoint = unique_endpoint();
        let listener = Listener::bind(&endpoint).unwrap();
        let path = std::path::PathBuf::from(endpoint.as_str());

        std::fs::remove_file(&path).unwrap();
        let theirs = std::os::unix::net::UnixListener::bind(&path).unwrap();

        drop(listener);
        assert!(path.exists(), "the socket bound after ours is still there");

        drop(theirs);
        std::fs::remove_file(&path).unwrap();
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn binding_over_a_live_listener_is_refused() {
        // Removing a socket something is still serving on does not stop that server, it only makes it unreachable, and
        // the game would quietly be talking to the wrong process.
        let endpoint = unique_endpoint();
        let listening = Listener::bind(&endpoint).unwrap();

        let refused = Listener::bind(&endpoint);

        let Err(Error::Bind(_, source)) = refused else {
            panic!("binding over a live listener should have been refused");
        };
        assert_eq!(source.kind(), std::io::ErrorKind::AddrInUse);

        // The one that was there is untouched.
        drop(listening);
        assert!(!std::path::Path::new(endpoint.as_str()).exists());
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn a_socket_nobody_answers_on_is_replaced() {
        // What a process killed without a chance to clean up leaves behind,
        // and the reason binding removes anything at all.
        let endpoint = unique_endpoint();
        let path = std::path::PathBuf::from(endpoint.as_str());

        let abandoned = std::os::unix::net::UnixListener::bind(&path).unwrap();
        drop(abandoned);
        assert!(path.exists(), "the standard library leaves the file behind");

        let listener = Listener::bind(&endpoint).unwrap();

        drop(listener);
        assert!(!path.exists());
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn an_endpoint_that_is_not_a_socket_is_left_alone() {
        // A misconfigured endpoint pointing at a real file must not delete it.
        let endpoint = unique_endpoint();
        let path = std::path::PathBuf::from(endpoint.as_str());

        std::fs::write(&path, b"not a socket").unwrap();

        let refused = Listener::bind(&endpoint);

        assert!(matches!(refused, Err(Error::Bind(..))));
        assert!(path.exists(), "the file is still there");

        std::fs::remove_file(&path).unwrap();
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn the_socket_file_goes_away_with_the_listener() {
        let endpoint = unique_endpoint();
        let listener = Listener::bind(&endpoint).unwrap();
        let path = std::path::PathBuf::from(endpoint.as_str());

        assert!(path.exists());

        drop(listener);

        assert!(!path.exists());
    }
}
