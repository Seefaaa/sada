//! Length-prefixed [`postcard`] framing.
//!
//! A frame is a little-endian `u32` payload length followed by a postcard-encoded value. Both ends of a channel speak
//! it, whatever transport carries them.

use std::io::{self, Write};

use serde::{Serialize, de::DeserializeOwned};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt as _};

/// Result type used by the framing helpers.
pub type Result<T> = std::result::Result<T, Error>;

/// Largest frame payload a channel accepts when its configuration does not say.
pub const DEFAULT_MAX_FRAME_LEN: u32 = 1024 * 1024;

/// Number of bytes in a frame's length prefix.
const LENGTH_PREFIX_LEN: usize = size_of::<u32>();

/// Reusable storage for one decoded frame payload.
///
/// Holds no allocation until the first frame passes through it, then keeps whatever capacity that needed.
#[derive(Debug)]
pub struct FrameBuffer {
    /// Raw frame payload bytes.
    ///
    /// Its length is a high-water mark rather than the current frame size. It only ever grows, so a small frame after
    /// a large one costs nothing and the zero-fill on growth is paid once per new maximum.
    bytes: Vec<u8>,
    /// Largest payload this side is willing to read.
    max_len: u32,
}

impl FrameBuffer {
    /// Create an empty frame buffer that refuses anything longer than `max_len`.
    #[must_use]
    pub const fn new(max_len: u32) -> Self {
        Self {
            bytes: Vec::new(),
            max_len,
        }
    }

    /// Read one frame from `reader`.
    ///
    /// Returns `Ok(None)` when the stream ends cleanly. Not cancel safe: a dropped future may have consumed part of a
    /// frame, so the reader is only good for another call if this one returned.
    pub async fn read<R, T>(&mut self, reader: &mut R) -> Result<Option<T>>
    where
        R: AsyncRead + Unpin,
        T: DeserializeOwned,
    {
        let mut prefix = [0; LENGTH_PREFIX_LEN];
        let mut filled = 0;

        while filled < prefix.len() {
            // read rather than read_exact because read_exact cannot tell a clean end from a truncated one
            match reader.read(&mut prefix[filled..]).await {
                Ok(0) if filled == 0 => return Ok(None),
                Ok(0) => return Err(Error::TruncatedFrameLength { read: filled }),
                Ok(read) => filled += read,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(source) => return Err(Error::ReadFrameLength(source)),
            }
        }

        let len = self.payload_len(prefix)?;
        let buf = self.prepare(len);

        reader.read_exact(buf).await.map_err(Error::ReadFramePayload)?;

        Ok(Some(postcard::from_bytes(buf).map_err(Error::DecodeFrame)?))
    }

    /// Validate a length prefix and return the payload length it states.
    fn payload_len(&self, prefix: [u8; LENGTH_PREFIX_LEN]) -> Result<usize> {
        let len = u32::from_le_bytes(prefix);

        if len > self.max_len {
            return Err(Error::FrameTooLarge { len });
        }

        Ok(len as usize)
    }

    /// Borrow exactly `len` bytes of the buffer, growing it if it is short.
    fn prepare(&mut self, len: usize) -> &mut [u8] {
        if self.bytes.len() < len {
            self.bytes.resize(len, 0);
        }
        &mut self.bytes[..len]
    }
}

/// Encode `value` as one complete frame, length prefix included.
///
/// The prefix is written into the space [`PayloadWriter`] reserves for it.
pub fn encode<T: Serialize>(value: &T, max_len: u32) -> Result<Vec<u8>> {
    let mut bytes = vec![0; LENGTH_PREFIX_LEN];

    let end = match postcard::to_io(value, PayloadWriter::new(&mut bytes, max_len)) {
        Ok(writer) => writer.frame_len(),
        Err(postcard::Error::SerializeBufferFull) => return Err(Error::EncodedFrameTooLarge),
        Err(source) => return Err(Error::EncodeFrame(source)),
    };

    let len = (end - LENGTH_PREFIX_LEN) as u32;

    bytes.truncate(end);
    bytes[..LENGTH_PREFIX_LEN].copy_from_slice(&len.to_le_bytes());

    Ok(bytes)
}

/// Writes a frame payload into a buffer, behind a gap left for the length prefix.
struct PayloadWriter<'a> {
    /// Buffer being written into.
    buf: &'a mut Vec<u8>,
    /// How many bytes of the buffer are in use, which is also the write offset. Starts past the length prefix.
    pos: usize,
    /// Largest payload the frame may carry.
    max_len: u32,
}

impl<'a> PayloadWriter<'a> {
    /// Start writing into `buf`, leaving the length prefix to be filled in afterwards.
    const fn new(buf: &'a mut Vec<u8>, max_len: u32) -> Self {
        Self {
            buf,
            pos: LENGTH_PREFIX_LEN,
            max_len,
        }
    }

    /// Length of the whole frame written so far, prefix included.
    #[inline]
    const fn frame_len(&self) -> usize { self.pos }
}

impl Write for PayloadWriter<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let end = self.pos + data.len();

        if end - LENGTH_PREFIX_LEN > self.max_len as usize {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "encoded frame is too large",
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

/// Errors that can happen while moving a frame on or off a stream.
#[derive(Debug, Error)]
pub enum Error {
    /// The frame length prefix could not be read.
    #[error("failed to read frame length")]
    ReadFrameLength(#[source] io::Error),
    /// The stream ended part-way through a length prefix.
    #[error("frame length prefix was truncated after {read} bytes")]
    TruncatedFrameLength {
        /// Number of prefix bytes that did arrive.
        read: usize,
    },
    /// The incoming frame announced a payload larger than this side accepts.
    #[error("incoming frame is {len} bytes, over the limit")]
    FrameTooLarge {
        /// Length the peer announced.
        len: u32,
    },
    /// The frame payload could not be read.
    #[error("failed to read frame payload")]
    ReadFramePayload(#[source] io::Error),
    /// The value could not be encoded.
    #[error("failed to encode frame")]
    EncodeFrame(#[source] postcard::Error),
    /// The encoded value was larger than the channel allows.
    ///
    /// Nothing reaches the wire, so the channel stays usable; the sender has to split what it was sending.
    #[error("encoded frame is over the limit")]
    EncodedFrameTooLarge,
    /// The frame could not be written.
    #[error("failed to write frame")]
    WriteFrame(#[source] io::Error),
    /// The frame could not be flushed to the transport.
    #[error("failed to flush frame")]
    Flush(#[source] io::Error),
    /// The frame payload could not be decoded.
    #[error("failed to decode frame")]
    DecodeFrame(#[source] postcard::Error),
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_MAX_FRAME_LEN, Error, FrameBuffer, encode};

    /// A value that encodes to `payload` bytes of message, give or take framing overhead.
    fn value_of(payload: usize) -> String { "a".repeat(payload) }

    /// Read one frame back out of `wire`, with the default limit.
    async fn read<T: serde::de::DeserializeOwned>(wire: &[u8]) -> super::Result<Option<T>> {
        let mut reader = wire;
        FrameBuffer::new(DEFAULT_MAX_FRAME_LEN).read(&mut reader).await
    }

    #[tokio::test]
    async fn a_frame_round_trips_through_a_stream() {
        let wire = encode(&value_of(8), DEFAULT_MAX_FRAME_LEN).unwrap();

        let decoded: String = read(&wire).await.unwrap().expect("a complete frame was written");

        assert_eq!(decoded, value_of(8));
    }

    #[tokio::test]
    async fn several_frames_share_one_buffer() {
        let long = value_of(500);
        let short = value_of(1);

        let mut wire = encode(&long, DEFAULT_MAX_FRAME_LEN).unwrap();
        wire.extend_from_slice(&encode(&short, DEFAULT_MAX_FRAME_LEN).unwrap());

        let mut reader = wire.as_slice();
        let mut buffer = FrameBuffer::new(DEFAULT_MAX_FRAME_LEN);
        let first: String = buffer.read(&mut reader).await.unwrap().unwrap();
        let second: String = buffer.read(&mut reader).await.unwrap().unwrap();

        assert_eq!(first, long);
        assert_eq!(second, short);
    }

    #[tokio::test]
    async fn a_value_that_encodes_to_nothing_still_travels() {
        // A unit type writes no payload at all, so the frame is its prefix and nothing else.
        let wire = encode(&(), DEFAULT_MAX_FRAME_LEN).unwrap();

        assert_eq!(wire, [0, 0, 0, 0]);
        assert_eq!(read::<()>(&wire).await.unwrap(), Some(()));
    }

    #[tokio::test]
    async fn end_of_stream_is_not_an_error() {
        let frame: Option<String> = read(&[]).await.unwrap();
        assert!(frame.is_none());
    }

    #[tokio::test]
    async fn a_truncated_payload_is_an_error() {
        let mut wire = encode(&value_of(8), DEFAULT_MAX_FRAME_LEN).unwrap();
        wire.pop();

        let result = read::<String>(&wire).await;
        assert!(matches!(result, Err(Error::ReadFramePayload(_))));
    }

    #[tokio::test]
    async fn a_truncated_length_prefix_is_not_a_clean_end() {
        let result = read::<String>(&[1, 2]).await;
        assert!(matches!(result, Err(Error::TruncatedFrameLength { read: 2 })));
    }

    #[tokio::test]
    async fn an_oversized_length_prefix_is_rejected() {
        // A hostile or corrupt peer must not be able to make us allocate or read forever.
        let mut wire = u32::MAX.to_le_bytes().to_vec();
        wire.extend_from_slice(b"payload");

        let result = read::<String>(&wire).await;
        assert!(matches!(result, Err(Error::FrameTooLarge { .. })));
    }

    #[tokio::test]
    async fn an_oversized_value_names_the_limit() {
        // The whole message is lost when this happens, so the error has to say why rather than surfacing postcard's
        // own "serialize buffer full".
        let result = encode(&value_of(DEFAULT_MAX_FRAME_LEN as usize + 1), DEFAULT_MAX_FRAME_LEN);

        assert!(matches!(result, Err(Error::EncodedFrameTooLarge)));
    }

    #[tokio::test]
    async fn a_frame_just_under_the_limit_round_trips() {
        let value = value_of(DEFAULT_MAX_FRAME_LEN as usize - 1024);

        let wire = encode(&value, DEFAULT_MAX_FRAME_LEN).unwrap();
        let decoded: String = read(&wire).await.unwrap().expect("a complete frame was written");

        assert_eq!(decoded, value);
    }
}
