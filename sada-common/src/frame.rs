//! Length-prefixed postcard framing for the game bridge control channel.
//!
//! Both ends of the Unix socket speak the same wire format: a little-endian `u32` payload length followed by a
//! postcard-encoded value. The buffer grows to fit the largest frame it has seen and is then reused, so the game does
//! not allocate per message.

use std::io::{self, Read, Write};

use serde::{Serialize, de::DeserializeOwned};
use thiserror::Error;
#[cfg(feature = "async")]
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

/// Result type used by the shared control protocol helpers.
pub type Result<T> = std::result::Result<T, Error>;

/// Largest accepted control frame payload.
///
/// A single game tick is sent as one [`crate::ControlRequest::Batch`], so this has to cover the whole crew at once.
pub const MAX_CONTROL_FRAME_LEN: usize = 1024 * 1024;

/// The length prefix is a `u32`, so the limit has to fit in one.
const _: () = assert!(MAX_CONTROL_FRAME_LEN <= u32::MAX as usize);

/// Number of bytes in a frame's length prefix.
const LENGTH_PREFIX_LEN: usize = size_of::<u32>();

/// Reusable storage for one encoded or decoded control frame.
///
/// Holds no allocation until the first frame passes through it, then keeps whatever capacity that needed.
#[derive(Debug, Default)]
pub struct ControlFrameBuffer {
    /// Raw frame payload bytes.
    ///
    /// Its length is a high-water mark rather than the current frame size. It only ever grows, so a small frame after
    /// a large one costs nothing and the zero-fill on growth is paid once per new maximum.
    bytes: Vec<u8>,
}

impl ControlFrameBuffer {
    /// Create an empty frame buffer.
    #[must_use]
    pub const fn new() -> Self { Self { bytes: Vec::new() } }

    /// Read one frame from `reader`.
    ///
    /// Returns `Ok(None)` when the stream ends cleanly.
    pub fn read<R, T>(&mut self, reader: &mut R) -> Result<Option<T>>
    where
        R: Read,
        T: DeserializeOwned,
    {
        let mut prefix = [0; LENGTH_PREFIX_LEN];
        let mut filled = 0;

        while filled < prefix.len() {
            // read rather than read_exact because read_exact cannot tell a clean end from a truncated one
            match reader.read(&mut prefix[filled..]) {
                Ok(0) if filled == 0 => return Ok(None),
                Ok(0) => return Err(Error::TruncatedFrameLength { read: filled }),
                Ok(read) => filled += read,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(source) => return Err(Error::ReadFrameLength(source)),
            }
        }

        let len = payload_len(prefix)?;
        let buf = self.prepare(len);

        reader.read_exact(buf).map_err(Error::ReadFramePayload)?;

        Ok(Some(Self::decode(buf)?))
    }

    /// Encode and write one frame to `writer`.
    pub fn write<W, T>(&mut self, writer: &mut W, value: &T) -> Result<()>
    where
        W: Write,
        T: Serialize,
    {
        let frame = self.encode_frame(value)?;

        writer.write_all(frame).map_err(Error::WriteFrame)?;
        writer.flush().map_err(Error::Flush)?;

        Ok(())
    }

    /// Read one frame from an asynchronous `reader`.
    ///
    /// Behaves exactly as [`ControlFrameBuffer::read`].
    #[cfg(feature = "async")]
    pub async fn read_async<R, T>(&mut self, reader: &mut R) -> Result<Option<T>>
    where
        R: AsyncRead + Unpin,
        T: DeserializeOwned,
    {
        let mut prefix = [0; LENGTH_PREFIX_LEN];
        let mut filled = 0;

        while filled < prefix.len() {
            match reader.read(&mut prefix[filled..]).await {
                Ok(0) if filled == 0 => return Ok(None),
                Ok(0) => return Err(Error::TruncatedFrameLength { read: filled }),
                Ok(read) => filled += read,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(source) => return Err(Error::ReadFrameLength(source)),
            }
        }

        let len = payload_len(prefix)?;
        let buf = self.prepare(len);

        reader.read_exact(buf).await.map_err(Error::ReadFramePayload)?;

        Ok(Some(Self::decode(buf)?))
    }

    /// Encode and write one frame to an asynchronous `writer`.
    ///
    /// Behaves exactly as [`ControlFrameBuffer::write`].
    #[cfg(feature = "async")]
    pub async fn write_async<W, T>(&mut self, writer: &mut W, value: &T) -> Result<()>
    where
        W: AsyncWrite + Unpin,
        T: Serialize,
    {
        let frame = self.encode_frame(value)?;

        writer.write_all(frame).await.map_err(Error::WriteFrame)?;
        writer.flush().await.map_err(Error::Flush)?;

        Ok(())
    }

    /// Borrow exactly `len` bytes of the buffer, growing it if it is short.
    fn prepare(&mut self, len: usize) -> &mut [u8] {
        if self.bytes.len() < len {
            self.bytes.resize(len, 0);
        }
        &mut self.bytes[..len]
    }

    /// Encode `value` into this buffer and return the complete frame, length prefix included.
    ///
    /// The prefix is written into the space [`PayloadWriter`] reserves for it, so the whole frame goes out in one
    /// `write_all`.
    fn encode_frame<T: Serialize>(&mut self, value: &T) -> Result<&[u8]> {
        let end = match postcard::to_io(value, PayloadWriter::new(&mut self.bytes)) {
            Ok(writer) => writer.frame_len(),
            Err(postcard::Error::SerializeBufferFull) => return Err(Error::EncodedFrameTooLarge),
            Err(e) => return Err(Error::EncodeFrame(e)),
        };

        let len = (end - LENGTH_PREFIX_LEN) as u32;
        let frame = &mut self.bytes[..end];

        frame[..LENGTH_PREFIX_LEN].copy_from_slice(&len.to_le_bytes());

        Ok(frame)
    }

    /// Decode the payload currently held in the buffer.
    fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
        postcard::from_bytes(bytes).map_err(Error::DecodeFrame)
    }
}

/// Writes a frame payload over the front of a reusable buffer, behind a gap left for the length prefix.
///
/// Overwrites in place and extends only when the frame runs past what the buffer already holds, so the buffer's
/// length stays a high-water mark. That is what lets one buffer serve both directions: encoding a small response does
/// not shrink the length that the read path grew for a large request.
struct PayloadWriter<'a> {
    /// Buffer being written over.
    buf: &'a mut Vec<u8>,
    /// How many bytes of the buffer are in use, which is also the write offset. Starts past the length prefix.
    pos: usize,
}

impl<'a> PayloadWriter<'a> {
    /// Start writing into `buf`, leaving the length prefix to be filled in afterwards.
    const fn new(buf: &'a mut Vec<u8>) -> Self {
        Self {
            buf,
            pos: LENGTH_PREFIX_LEN,
        }
    }

    /// Length of the whole frame written so far, prefix included.
    #[inline]
    const fn frame_len(&self) -> usize { self.pos }
}

impl Write for PayloadWriter<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let end = self.pos + data.len();

        if end - LENGTH_PREFIX_LEN > MAX_CONTROL_FRAME_LEN {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "encoded control frame is too large",
            ));
        }

        if self.buf.len() < end {
            self.buf.resize(end, 0);
        }

        self.buf[self.pos..end].copy_from_slice(data);
        self.pos = end;

        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

/// Validate a length prefix and return the payload length it states.
fn payload_len(prefix: [u8; LENGTH_PREFIX_LEN]) -> Result<usize> {
    let len = u32::from_le_bytes(prefix) as usize;

    if len > MAX_CONTROL_FRAME_LEN {
        return Err(Error::FrameTooLarge { len });
    }

    Ok(len)
}

/// Errors that can happen while encoding or decoding control frames.
#[derive(Debug, Error)]
pub enum Error {
    /// The control frame length prefix could not be read.
    #[error("failed to read control frame length")]
    ReadFrameLength(#[source] io::Error),
    /// The stream ended part-way through a length prefix.
    #[error("control frame length prefix was truncated after {read} bytes")]
    TruncatedFrameLength {
        /// Number of prefix bytes that did arrive.
        read: usize,
    },
    /// The incoming frame announced a payload larger than the protocol allows.
    #[error("incoming control frame is {len} bytes, over the limit")]
    FrameTooLarge {
        /// Length the peer announced.
        len: usize,
    },
    /// The control frame payload could not be read.
    #[error("failed to read control frame payload")]
    ReadFramePayload(#[source] io::Error),
    /// The value could not be encoded into the control protocol.
    #[error("failed to encode control frame")]
    EncodeFrame(#[source] postcard::Error),
    /// The encoded value was larger than the protocol allows.
    ///
    /// A whole game tick travels as one batch, so this means the tick was too big to send at all; the sender has to
    /// split it against [`MAX_CONTROL_FRAME_LEN`].
    #[error("encoded control frame is over the limit")]
    EncodedFrameTooLarge,
    /// The control frame could not be written.
    #[error("failed to write control frame")]
    WriteFrame(#[source] io::Error),
    /// The control frame could not be flushed to the socket.
    #[error("failed to flush control frame")]
    Flush(#[source] io::Error),
    /// The control frame payload could not be decoded.
    #[error("failed to decode control frame")]
    DecodeFrame(#[source] postcard::Error),
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::{ControlFrameBuffer, Error, MAX_CONTROL_FRAME_LEN};
    use crate::control::{ControlRequest, ControlResponse};

    /// A request small enough to be uninteresting, used where the value does not matter.
    fn request() -> ControlRequest {
        ControlRequest::RegisterCode {
            code: "AB12CD".into(),
            ckey: "sefa".into(),
        }
    }

    /// A response that encodes to `payload` bytes of message, give or take framing overhead.
    fn response_of(payload: usize) -> ControlResponse {
        ControlResponse::Error {
            message: "a".repeat(payload),
        }
    }

    #[test]
    fn a_frame_round_trips_through_a_stream() {
        let mut wire = Vec::new();
        ControlFrameBuffer::new().write(&mut wire, &request()).unwrap();

        let decoded: ControlRequest = ControlFrameBuffer::new()
            .read(&mut Cursor::new(&wire))
            .unwrap()
            .expect("a complete frame was written");

        assert_eq!(decoded, request());
    }

    #[test]
    fn several_frames_share_one_buffer() {
        let long = response_of(500);
        let short = ControlResponse::Ok;

        let mut wire = Vec::new();
        let mut buffer = ControlFrameBuffer::new();
        buffer.write(&mut wire, &long).unwrap();
        buffer.write(&mut wire, &short).unwrap();

        let mut reader = Cursor::new(&wire);
        let mut buffer = ControlFrameBuffer::new();
        let first: ControlResponse = buffer.read(&mut reader).unwrap().unwrap();
        let second: ControlResponse = buffer.read(&mut reader).unwrap().unwrap();

        assert_eq!(first, long);
        assert_eq!(second, short);
    }

    #[test]
    fn end_of_stream_is_not_an_error() {
        let mut reader = Cursor::new(Vec::new());
        let frame: Option<ControlRequest> = ControlFrameBuffer::new().read(&mut reader).unwrap();
        assert!(frame.is_none());
    }

    #[test]
    fn a_truncated_payload_is_an_error() {
        let mut wire = Vec::new();
        ControlFrameBuffer::new()
            .write(&mut wire, &ControlRequest::Version)
            .unwrap();
        wire.pop();

        let result = ControlFrameBuffer::new().read::<_, ControlRequest>(&mut Cursor::new(&wire));
        assert!(matches!(result, Err(Error::ReadFramePayload(_))));
    }

    #[test]
    fn a_truncated_length_prefix_is_not_a_clean_end() {
        // A peer that died mid-write must be distinguishable from one that hung up at a frame boundary, or the server
        // logs nothing when the game crashes.
        let wire = [1, 2];

        let result = ControlFrameBuffer::new().read::<_, ControlRequest>(&mut Cursor::new(&wire));
        assert!(matches!(result, Err(Error::TruncatedFrameLength { read: 2 })));
    }

    #[test]
    fn an_oversized_length_prefix_is_rejected() {
        // A hostile or corrupt peer must not be able to make us allocate or read forever.
        let mut wire = u32::MAX.to_le_bytes().to_vec();
        wire.extend_from_slice(b"payload");

        let result = ControlFrameBuffer::new().read::<_, ControlRequest>(&mut Cursor::new(&wire));
        assert!(matches!(result, Err(Error::FrameTooLarge { .. })));
    }

    #[test]
    fn an_oversized_value_names_the_limit() {
        // The whole tick is lost when this happens, so the error has to say why rather than surfacing postcard's own
        // "serialize buffer full".
        let mut wire = Vec::new();
        let result = ControlFrameBuffer::new().write(&mut wire, &response_of(MAX_CONTROL_FRAME_LEN + 1));

        assert!(matches!(result, Err(Error::EncodedFrameTooLarge)));
        assert!(wire.is_empty(), "nothing should reach the wire");
    }

    #[test]
    fn a_frame_just_under_the_limit_round_trips() {
        let value = response_of(MAX_CONTROL_FRAME_LEN - 1024);

        let mut wire = Vec::new();
        ControlFrameBuffer::new().write(&mut wire, &value).unwrap();

        let decoded: ControlResponse = ControlFrameBuffer::new()
            .read(&mut Cursor::new(&wire))
            .unwrap()
            .expect("a complete frame was written");

        assert_eq!(decoded, value);
    }
}

#[cfg(all(test, feature = "async"))]
mod async_tests {
    use super::{ControlFrameBuffer, Error, MAX_CONTROL_FRAME_LEN};
    use crate::control::{ControlRequest, ControlResponse};

    /// A response that encodes to `payload` bytes of message, give or take framing overhead.
    fn response_of(payload: usize) -> ControlResponse {
        ControlResponse::Error {
            message: "a".repeat(payload),
        }
    }

    #[tokio::test]
    async fn a_frame_round_trips_through_a_stream() {
        let value = ControlRequest::RemovePlayer { ckey: "sefa".into() };

        let mut wire = Vec::new();
        ControlFrameBuffer::new().write_async(&mut wire, &value).await.unwrap();

        let decoded: ControlRequest = ControlFrameBuffer::new()
            .read_async(&mut wire.as_slice())
            .await
            .unwrap()
            .expect("a complete frame was written");

        assert_eq!(decoded, value);
    }

    #[tokio::test]
    async fn several_frames_share_one_buffer() {
        let long = response_of(500);
        let short = ControlResponse::Ok;

        let mut wire = Vec::new();
        let mut buffer = ControlFrameBuffer::new();
        buffer.write_async(&mut wire, &long).await.unwrap();
        buffer.write_async(&mut wire, &short).await.unwrap();

        let mut reader = wire.as_slice();
        let mut buffer = ControlFrameBuffer::new();
        let first: ControlResponse = buffer.read_async(&mut reader).await.unwrap().unwrap();
        let second: ControlResponse = buffer.read_async(&mut reader).await.unwrap().unwrap();

        assert_eq!(first, long);
        assert_eq!(second, short);
    }

    #[tokio::test]
    async fn end_of_stream_is_not_an_error() {
        let frame: Option<ControlRequest> = ControlFrameBuffer::new().read_async(&mut [].as_slice()).await.unwrap();
        assert!(frame.is_none());
    }

    #[tokio::test]
    async fn a_truncated_payload_is_an_error() {
        let mut wire = Vec::new();
        ControlFrameBuffer::new()
            .write_async(&mut wire, &ControlRequest::Version)
            .await
            .unwrap();
        wire.pop();

        let result: super::Result<Option<ControlRequest>> =
            ControlFrameBuffer::new().read_async(&mut wire.as_slice()).await;
        assert!(matches!(result, Err(Error::ReadFramePayload(_))));
    }

    #[tokio::test]
    async fn a_truncated_length_prefix_is_not_a_clean_end() {
        let result: super::Result<Option<ControlRequest>> =
            ControlFrameBuffer::new().read_async(&mut [1, 2].as_slice()).await;
        assert!(matches!(result, Err(Error::TruncatedFrameLength { read: 2 })));
    }

    #[tokio::test]
    async fn an_oversized_length_prefix_is_rejected() {
        let mut wire = u32::MAX.to_le_bytes().to_vec();
        wire.extend_from_slice(b"payload");

        let result: super::Result<Option<ControlRequest>> =
            ControlFrameBuffer::new().read_async(&mut wire.as_slice()).await;
        assert!(matches!(result, Err(Error::FrameTooLarge { .. })));
    }

    #[tokio::test]
    async fn an_oversized_value_names_the_limit() {
        let mut wire = Vec::new();
        let result = ControlFrameBuffer::new()
            .write_async(&mut wire, &response_of(MAX_CONTROL_FRAME_LEN + 1))
            .await;

        assert!(matches!(result, Err(Error::EncodedFrameTooLarge)));
        assert!(wire.is_empty(), "nothing should reach the wire");
    }

    #[tokio::test]
    async fn a_frame_just_under_the_limit_round_trips() {
        let value = response_of(MAX_CONTROL_FRAME_LEN - 1024);

        let mut wire = Vec::new();
        ControlFrameBuffer::new().write_async(&mut wire, &value).await.unwrap();

        let decoded: ControlResponse = ControlFrameBuffer::new()
            .read_async(&mut wire.as_slice())
            .await
            .unwrap()
            .expect("a complete frame was written");

        assert_eq!(decoded, value);
    }
}
