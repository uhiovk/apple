use std::borrow::Cow;
use std::io::{Cursor, Write};

use anyhow::Result;
use base64::prelude::*;
use image::codecs::avif::AvifEncoder;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, PngEncoder};
use image::imageops::FilterType::Lanczos3;
use image::{DynamicImage, GenericImageView, ImageReader, load_from_memory};
use ogg::{PacketWriteEndInfo, PacketWriter};
use opus2::{Application, Bitrate, Channels, Encoder as OpusEncoder};
use webp::Encoder as WebpEncoder;

use crate::consts::{MAGIC, SAMPLE_RATE, VENDOR_STRING};
use crate::decode::{DecodedData, Metadata};

pub struct OpusOggEncoder<W: Write> {
    decoded: DecodedData,
    encoder: OpusEncoder,
    granule_position: u64,
    packet_writer: PacketWriter<'static, W>,

    frame_size: usize,

    input_buffer: Vec<f32>,
    output_buffer: [u8; BUFFER_CAPACITY],
}

const BUFFER_CAPACITY: usize = 4096;

impl<W: Write> OpusOggEncoder<W> {
    pub fn new(
        decoded: DecodedData,
        bitrate: i32,
        complexity: i32,
        image_processor: &mut ImageProcessor,
        writer: W,
    ) -> Result<Self> {
        let num_channels = decoded.audio.num_channels();
        let frame_size = decoded.audio.frame_size();

        let channels = match num_channels {
            1 => Channels::Mono,
            2 => Channels::Stereo,
            _ => unreachable!(),
        };

        let mut encoder = OpusEncoder::new(SAMPLE_RATE as _, channels, Application::Audio)?;
        encoder.set_bitrate(Bitrate::Bits(bitrate))?;
        encoder.set_complexity(complexity)?;

        let mut packet_writer = PacketWriter::new(writer);

        let image = decoded.cover_image.as_ref().and_then(|img| image_processor.process(img).ok());
        let id_header = build_ogg_id_header(num_channels, encoder.get_lookahead()?);
        let comment_header = build_ogg_comment_header(&decoded.metadata, image);
        packet_writer.write_packet(id_header, MAGIC, PacketWriteEndInfo::EndPage, 0)?;
        packet_writer.write_packet(comment_header, MAGIC, PacketWriteEndInfo::EndPage, 0)?;

        Ok(Self {
            decoded,
            encoder,
            granule_position: 0,
            packet_writer,
            frame_size,
            input_buffer: vec![0.0; num_channels * frame_size],
            output_buffer: [0; BUFFER_CAPACITY],
        })
    }

    pub fn write_packet(&mut self) -> Result<bool> {
        let eos_info = self.decoded.audio.next_frame(&mut self.input_buffer)?;
        let packet_len = self.encoder.encode_float(&self.input_buffer, &mut self.output_buffer)?;
        let payload = self.output_buffer[..packet_len].to_owned();

        let packet_info = if let Some(num_samples_left) = eos_info {
            self.granule_position += num_samples_left as u64;
            PacketWriteEndInfo::EndStream
        } else {
            self.granule_position += self.frame_size as u64;
            PacketWriteEndInfo::NormalPacket
        };

        self.packet_writer.write_packet(payload, MAGIC, packet_info, self.granule_position)?;
        Ok(eos_info.is_none())
    }
}

pub struct Image<'a> {
    data: Cow<'a, [u8]>,
    mime_type: &'static str,
    width: u32,
    height: u32,
}

#[derive(Clone, Copy)]
pub enum ImageFormat {
    Copy,
    Png,
    Jpeg,
    Webp,
    Avif,
}

pub struct ImageProcessor {
    pub target_format: ImageFormat,
    pub new_dimensions: Option<(u32, u32)>,
    pub quality: f32,
}

impl ImageProcessor {
    fn process<'a>(&mut self, data: &'a [u8]) -> Result<Image<'a>> {
        use ImageFormat::*;
        match self.target_format {
            Copy => {
                let reader = ImageReader::new(Cursor::new(data)).with_guessed_format()?;
                let mime_type = reader.format().unwrap().to_mime_type();
                let (width, height) = reader.into_dimensions().unwrap();

                Ok(Image { data: Cow::Borrowed(data), mime_type, width, height })
            }
            Webp => {
                let (image, width, height) = load_and_resize(data, self.new_dimensions)?;

                let encoded = WebpEncoder::from_image(&DynamicImage::ImageRgb8(image.to_rgb8()))
                    .unwrap()
                    .encode(self.quality)
                    .to_owned();

                Ok(Image { data: Cow::Owned(encoded), mime_type: "image/webp", width, height })
            }
            Png => {
                let (image, width, height) = load_and_resize(data, self.new_dimensions)?;

                let mut encoded = Vec::new();
                image.write_with_encoder(PngEncoder::new_with_quality(
                    &mut encoded,
                    CompressionType::Default,
                    <_>::default(),
                ))?;

                Ok(Image { data: Cow::Owned(encoded), mime_type: "image/png", width, height })
            }
            Jpeg => {
                let (image, width, height) = load_and_resize(data, self.new_dimensions)?;

                let mut encoded = Vec::new();
                image.write_with_encoder(JpegEncoder::new_with_quality(
                    &mut encoded,
                    self.quality.round() as _,
                ))?;

                Ok(Image { data: Cow::Owned(encoded), mime_type: "image/jpeg", width, height })
            }
            Avif => {
                let (image, width, height) = load_and_resize(data, self.new_dimensions)?;

                let mut encoded = Vec::new();
                image.write_with_encoder(AvifEncoder::new_with_speed_quality(
                    &mut encoded,
                    5,
                    self.quality.round() as _,
                ))?;

                Ok(Image { data: Cow::Owned(encoded), mime_type: "image/avif", width, height })
            }
        }
    }
}

fn load_and_resize(
    data: &[u8],
    new_dimensions: Option<(u32, u32)>,
) -> Result<(DynamicImage, u32, u32)> {
    let mut image = load_from_memory(data)?;

    if let Some((nw, nh)) = new_dimensions {
        let (w, h) = image.dimensions();
        if w > nw && h > nh {
            image = image.resize(nw, nh, Lanczos3);
        }
    }
    let (width, height) = image.dimensions();

    Ok((image, width, height))
}

fn build_ogg_id_header(num_channels: usize, pre_skip: i32) -> Vec<u8> {
    let mut header = Vec::with_capacity(19);
    header.extend(b"OpusHead");
    header.push(1); // version
    header.push(num_channels as u8);
    header.extend((pre_skip as u16).to_le_bytes());
    header.extend((SAMPLE_RATE as u32).to_le_bytes());
    header.extend(0_i16.to_le_bytes()); // gain
    header.push(0); // mapping family
    header
}

fn build_ogg_comment_header(metadata: &Metadata, image: Option<Image>) -> Vec<u8> {
    let mut header = Vec::new();

    header.extend(b"OpusTags");
    header.extend((VENDOR_STRING.len() as u32).to_le_bytes());
    header.extend(VENDOR_STRING);

    let mut comments = Vec::new();
    if let Some(title) = &metadata.title {
        comments.push(format!("TITLE={title}"));
    }
    if let Some(artist) = &metadata.artist {
        comments.push(format!("ARTIST={artist}"));
    }
    if let Some(album) = &metadata.album {
        comments.push(format!("ALBUM={album}"));
    }
    if let Some(track_number) = &metadata.track_number {
        comments.push(format!("TRACKNUMBER={track_number}"));
    }

    if let Some(Image { data, mime_type, width, height }) = image {
        let mut buffer = Vec::new();
        buffer.extend(3_u32.to_be_bytes()); // 3 for "Front Cover"
        buffer.extend((mime_type.len() as u32).to_be_bytes());
        buffer.extend(mime_type.as_bytes());
        buffer.extend(0_u32.to_be_bytes()); // description length
        buffer.extend(width.to_be_bytes());
        buffer.extend(height.to_be_bytes());
        buffer.extend(24_u32.to_be_bytes()); // color depth
        buffer.extend(0_u32.to_be_bytes()); // 0 for non-indexed pictures (non-GIF)
        buffer.extend((data.len() as u32).to_be_bytes());
        buffer.extend(&*data);

        let encoded = BASE64_STANDARD.encode(buffer);
        comments.push(format!("METADATA_BLOCK_PICTURE={encoded}"));
    }

    header.extend((comments.len() as u32).to_le_bytes());
    for comment in comments {
        header.extend((comment.len() as u32).to_le_bytes());
        header.extend(comment.as_bytes());
    }

    header
}
