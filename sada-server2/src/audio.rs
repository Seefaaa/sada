//! Debug capture of incoming audio.
//!
//! Enabled by the `audio_dump` feature. Each session's incoming Opus stream is decoded and written to its own WAV file,
//! which is the quickest way to tell "the browser is not sending" apart from "the server is not relaying".

use std::{fs::File, io::BufWriter, path::Path};

use hound::{WavSpec, WavWriter};
use opus::Channels;
use sada_common::SessionId;
use str0m::media::MediaData;
use thiserror::Error;

/// Sample rate of the Opus streams the server negotiates.
const SAMPLE_RATE: u32 = 48_000;

/// Largest number of samples one Opus packet can decode to at 48 kHz.
const MAX_FRAME_SAMPLES: usize = 5760;

/// Consumes incoming audio frames for one session.
pub struct AudioSink {
    /// Number of frames observed.
    frames: usize,
    /// Total encoded bytes observed.
    bytes: usize,
    /// WAV dumper, absent if it could not be created.
    dumper: Option<AudioDumper>,
}

impl AudioSink {
    /// Create a sink writing to a file named after `session`.
    #[must_use]
    pub fn new(session: SessionId) -> Self {
        let path = format!("audio_dump_{}.wav", session.as_raw());

        let dumper = match AudioDumper::create(&path) {
            Ok(dumper) => {
                info!(path, "audio dump: writing");
                Some(dumper)
            },
            Err(err) => {
                warn!(?err, "audio dump: disabled");
                None
            },
        };

        Self {
            frames: 0,
            bytes: 0,
            dumper,
        }
    }

    /// Record one frame.
    pub fn handle_frame(&mut self, data: &MediaData) {
        self.frames += 1;
        self.bytes += data.data.len();

        if self.frames <= 5 || self.frames.is_multiple_of(500) {
            debug!(
                frames = self.frames,
                bytes = data.data.len(),
                total = self.bytes,
                "audio dump: frame"
            );
        }

        if let Some(dumper) = &mut self.dumper
            && let Err(err) = dumper.write_frame(&data.data)
            && (self.frames <= 3 || self.frames.is_multiple_of(100))
        {
            warn!(?err, "audio dump: decode failed");
        }
    }
}

/// Decodes Opus and writes the samples to a WAV file.
struct AudioDumper {
    /// Decoder for the negotiated stream.
    decoder: opus::Decoder,
    /// Output file.
    writer: WavWriter<BufWriter<File>>,
    /// Number of samples written.
    samples: usize,
}

impl AudioDumper {
    /// Create a dumper writing to `path`.
    fn create(path: impl AsRef<Path>) -> Result<Self, Error> {
        let decoder = opus::Decoder::new(SAMPLE_RATE, Channels::Mono).map_err(Error::CreateDecoder)?;

        let spec = WavSpec {
            channels: 1,
            sample_rate: SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };

        let writer = WavWriter::create(path, spec).map_err(Error::CreateFile)?;

        Ok(Self {
            decoder,
            writer,
            samples: 0,
        })
    }

    /// Decode one packet and append it.
    fn write_frame(&mut self, opus: &[u8]) -> Result<(), Error> {
        let mut pcm = [0; MAX_FRAME_SAMPLES];

        let decoded = self.decoder.decode(opus, &mut pcm, false).map_err(Error::Decode)?;

        for &sample in &pcm[..decoded] {
            self.writer.write_sample(sample).map_err(Error::WriteSample)?;
        }

        self.samples += decoded;

        Ok(())
    }
}

impl Drop for AudioDumper {
    fn drop(&mut self) {
        let seconds = self.samples as f64 / f64::from(SAMPLE_RATE);
        info!(seconds, samples = self.samples, "audio dump: finalizing");

        if let Err(err) = self.writer.flush() {
            error!(?err, "audio dump: flush failed");
        }
    }
}

/// Errors that can happen while dumping audio.
#[derive(Debug, Error)]
enum Error {
    /// The Opus decoder could not be created.
    #[error("failed to create the Opus decoder")]
    CreateDecoder(#[source] opus::Error),
    /// The WAV file could not be created.
    #[error("failed to create the WAV file")]
    CreateFile(#[source] hound::Error),
    /// A packet could not be decoded.
    #[error("Opus decode failed")]
    Decode(#[source] opus::Error),
    /// A sample could not be written.
    #[error("WAV write failed")]
    WriteSample(#[source] hound::Error),
}
