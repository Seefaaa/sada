//! Media frame handling.

use std::{fs::File, io::BufWriter, path::Path};

use hound::{WavSpec, WavWriter};
use opus::Channels;
use str0m::media::MediaData;
use thiserror::Error;

/// Result type used by media helpers.
type Result<T> = std::result::Result<T, Error>;

/// Consumes incoming audio media frames.
pub struct AudioSink {
    /// Number of media frames observed.
    frame_count: u64,
    /// Total number of encoded bytes observed.
    byte_count: u64,
    /// Optional WAV dumper used for local debugging.
    dumper: Option<AudioDumper>,
}

impl AudioSink {
    /// Create a new audio sink.
    pub fn new(id: u32) -> Self {
        let dumper = if cfg!(feature = "audio_dump") {
            let path = format!("audio_dump_{id}.wav");
            match AudioDumper::create(&path) {
                Ok(d) => {
                    info!(path, "audio dumper: writing to file");
                    Some(d)
                },
                Err(err) => {
                    warn!(?err, "audio dumper: disabled");
                    None
                },
            }
        } else {
            None
        };

        Self {
            frame_count: 0,
            byte_count: 0,
            dumper,
        }
    }

    /// Process one encoded media frame.
    pub fn handle_frame(&mut self, data: &MediaData) {
        self.frame_count += 1;
        self.byte_count += data.data.len() as u64;

        #[cfg(debug_assertions)]
        if self.frame_count <= 5 || self.frame_count.is_multiple_of(500) {
            info!(
                frame = self.frame_count,
                bytes = data.data.len(),
                mid = ?data.mid,
                pt = ?data.pt,
                total_bytes = self.byte_count,
                "audio frame",
            );
        }

        if let Some(dumper) = &mut self.dumper
            && let Err(err) = dumper.write_frame(&data.data)
            && (self.frame_count <= 3 || self.frame_count.is_multiple_of(100))
        {
            warn!(?err, "audio dumper: decode error");
        }
    }
}

/// Decodes Opus frames and writes the resulting PCM samples to a WAV file.
struct AudioDumper {
    /// Opus decoder configured for the negotiated audio stream.
    decoder: opus::Decoder,
    /// WAV writer for decoded samples.
    writer: WavWriter<BufWriter<File>>,
    /// Number of decoded PCM samples written.
    sample_count: u64,
}

impl AudioDumper {
    /// Create a dumper that writes to `path`.
    fn create(path: impl AsRef<Path>) -> Result<Self> {
        let decoder = opus::Decoder::new(48000, Channels::Mono).map_err(Error::CreateOpusDecoder)?;

        let spec = WavSpec {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };

        let writer = WavWriter::create(path, spec).map_err(Error::CreateWavFile)?;

        Ok(Self {
            decoder,
            writer,
            sample_count: 0,
        })
    }

    /// Decode one Opus packet and append its samples to the WAV file.
    fn write_frame(&mut self, opus_data: &[u8]) -> Result<()> {
        let mut pcm_buf = [0; 5760];

        let samples = self
            .decoder
            .decode(opus_data, &mut pcm_buf, false)
            .map_err(Error::DecodeOpus)?;

        for &sample in &pcm_buf[..samples] {
            self.writer.write_sample(sample).map_err(Error::WriteWavSample)?;
        }

        self.sample_count += samples as u64;

        if self.sample_count % 240_000 < samples as u64 {
            let secs = self.sample_count as f64 / 48000.0;
            info!(duration_secs = secs, "audio dumper: audio written");
        }

        Ok(())
    }
}

impl Drop for AudioDumper {
    fn drop(&mut self) {
        let secs = self.sample_count as f64 / 48000.0;
        info!(
            duration_secs = secs,
            samples = self.sample_count,
            "audio dumper: finalizing WAV",
        );
        if let Err(err) = self.writer.flush() {
            error!(?err, "audio dumper: flush error");
        }
    }
}

/// Errors that can happen while dumping incoming audio.
#[derive(Debug, Error)]
enum Error {
    /// The Opus decoder could not be initialized.
    #[error("failed to create Opus decoder")]
    CreateOpusDecoder(#[source] opus::Error),
    /// The WAV output file could not be created.
    #[error("failed to create WAV file")]
    CreateWavFile(#[source] hound::Error),
    /// An Opus packet could not be decoded.
    #[error("Opus decode failed")]
    DecodeOpus(#[source] opus::Error),
    /// A decoded PCM sample could not be written to the WAV output.
    #[error("WAV write failed")]
    WriteWavSample(#[source] hound::Error),
}
