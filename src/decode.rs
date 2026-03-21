use std::borrow::Cow;
use std::fs::File;
use std::io::Cursor;
use std::io::ErrorKind::UnexpectedEof;
use std::path::Path;

use anyhow::{Result, anyhow, ensure};
use audioadapter_buffers::direct::{InterleavedSlice, SequentialSlice};
use ncmdump::Ncmdump;
use ringbuf::LocalRb;
use ringbuf::storage::Heap;
use ringbuf::traits::{Consumer, Observer, Producer};
use rubato::{Fft, FixedSync, Resampler};
use symphonia::core::audio::{AudioBuffer, AudioBufferRef, Signal};
use symphonia::core::codecs::{CODEC_TYPE_NULL, Decoder};
use symphonia::core::errors::Error::{DecodeError, IoError};
use symphonia::core::formats::FormatReader;
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::StandardTagKey::{Album, Artist, TrackNumber, TrackTitle};
use symphonia::default::{get_codecs, get_probe};

use crate::consts::{FRAME_SIZE, SAMPLE_RATE};

pub struct DecodedData {
    pub audio: Audio,
    pub metadata: Metadata,
    pub cover_image: Option<Vec<u8>>,
}

pub struct Audio {
    format_reader: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    resampler: Option<Fft<f32>>,

    num_channels: usize,
    frame_size: usize,

    input_buffer: Vec<LocalRb<Heap<f32>>>,
    scratch: Vec<f32>,
    eos_info: Option<usize>,
}

#[derive(Default)]
pub struct Metadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub track_number: Option<String>,
}

const INPUT_BUFFER_CAPACITY: usize = 1024 * 64;

impl DecodedData {
    pub fn from_reader(source: impl MediaSource + 'static, frame_size: usize) -> Result<Self> {
        let mss = MediaSourceStream::new(Box::new(source), <_>::default());
        let probed = get_probe().format(&<_>::default(), mss, &<_>::default(), &<_>::default())?;
        let mut format_reader = probed.format;

        let track = format_reader
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or_else(|| anyhow!("no audio track found"))?;
        let track_id = track.id;
        let codec_params = &track.codec_params;

        let decoder = get_codecs().make(codec_params, &<_>::default())?;

        let source_rate =
            codec_params.sample_rate.ok_or_else(|| anyhow!("unknown sample rate"))? as usize;
        let num_channels =
            codec_params.channels.ok_or_else(|| anyhow!("unknown channel count"))?.count();
        ensure!(num_channels == 2, "wtf kind of music are you listening to");

        let resampler = if source_rate == SAMPLE_RATE {
            None
        } else {
            Some(Fft::new(
                source_rate,
                SAMPLE_RATE,
                frame_size,
                1,
                num_channels,
                FixedSync::Output,
            )?)
        };

        let mut input_buffer = Vec::with_capacity(num_channels);
        for _ in 0..num_channels {
            input_buffer.push(LocalRb::new(INPUT_BUFFER_CAPACITY));
        }

        let mut metadata = Metadata::default();
        let mut cover_image = None;

        macro_rules! extract_tags {
            ($meta:expr) => {
                for tag in $meta.tags() {
                    match tag.std_key {
                        Some(TrackTitle) => metadata.title = Some(tag.value.to_string()),
                        Some(Artist) => metadata.artist = Some(tag.value.to_string()),
                        Some(Album) => metadata.album = Some(tag.value.to_string()),
                        Some(TrackNumber) => metadata.track_number = Some(tag.value.to_string()),
                        _ => {}
                    }
                }

                if let [visual, ..] = $meta.visuals() {
                    cover_image = Some(visual.data.to_vec());
                }
            };
        }

        // MP3 tags
        if let Some(mut metadata_log) = probed.metadata.into_inner()
            && let Some(meta) = metadata_log.metadata().current()
        {
            extract_tags!(meta);
        }

        // other formats' tags
        if let Some(meta) = format_reader.metadata().current() {
            extract_tags!(meta);
        }

        let audio = Audio {
            format_reader,
            decoder,
            track_id,
            resampler,
            num_channels,
            frame_size,
            input_buffer,
            scratch: Vec::new(),
            eos_info: None,
        };

        Ok(Self { audio, metadata, cover_image })
    }

    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_reader(File::open(&path)?, FRAME_SIZE)
    }

    pub fn from_ncm_file(path: impl AsRef<Path>) -> Result<Self> {
        let mut dumped = Ncmdump::from_reader(File::open(path)?)?;

        let audio_data = dumped.get_data()?;
        let cover_image = dumped.get_image().ok();

        let mut result = Self::from_reader(Cursor::new(audio_data), FRAME_SIZE)?;
        result.cover_image = result.cover_image.or(cover_image);

        Ok(result)
    }
}

impl Audio {
    pub fn num_channels(&self) -> usize {
        self.num_channels
    }

    pub fn frame_size(&self) -> usize {
        self.frame_size
    }

    pub fn next_frame(&mut self, target: &mut [f32]) -> Result<Option<usize>> {
        if self.eos_info.is_some() {
            return Ok(Some(0));
        }

        let samples_needed = if let Some(resampler) = &self.resampler {
            resampler.input_frames_next()
        } else {
            self.frame_size
        };

        while self.input_buffer[0].occupied_len() < samples_needed {
            let packet = match self.format_reader.next_packet() {
                Ok(p) => p,
                Err(IoError(err)) if err.kind() == UnexpectedEof => {
                    let num_samples_left = self.input_buffer[0].occupied_len();
                    self.eos_info = Some(num_samples_left);
                    let silence = vec![0.0; samples_needed - num_samples_left];
                    for buffer in &mut self.input_buffer {
                        buffer.push_slice(&silence);
                    }
                    break;
                }
                Err(err) => return Err(err.into()),
            };

            if packet.track_id() != self.track_id {
                continue;
            }

            let decoded = match self.decoder.decode(&packet) {
                Ok(AudioBufferRef::F32(buf)) => buf,
                Ok(buf) => {
                    let mut new_buf = AudioBuffer::new(buf.capacity() as _, *buf.spec());
                    buf.convert(&mut new_buf);
                    Cow::Owned(new_buf)
                }
                Err(IoError(_) | DecodeError(_)) => continue,
                Err(err) => return Err(err.into()),
            };

            for (ch, buffer) in self.input_buffer.iter_mut().enumerate() {
                let data = decoded.chan(ch);
                assert!(
                    buffer.push_slice(data) == data.len(),
                    "input buffer overflow while decoding"
                );
            }
        }

        self.scratch.resize(self.num_channels * samples_needed, 0.0);
        for (ch, buffer) in self.input_buffer.iter_mut().enumerate() {
            let start = ch * samples_needed;
            let end = start + samples_needed;
            buffer.pop_slice(&mut self.scratch[start..end]);
        }

        if let Some(resampler) = &mut self.resampler {
            let input_adapter =
                SequentialSlice::new(&self.scratch, self.num_channels, samples_needed).unwrap();
            let mut output_adapter =
                InterleavedSlice::new_mut(target, self.num_channels, self.frame_size).unwrap();
            resampler.process_into_buffer(&input_adapter, &mut output_adapter, None).unwrap();
        } else {
            for (ch, channel) in self.scratch.chunks_exact(samples_needed).enumerate() {
                for (i, &sample) in channel.iter().enumerate() {
                    target[i * self.num_channels + ch] = sample;
                }
            }
        }

        Ok(self.eos_info)
    }
}
