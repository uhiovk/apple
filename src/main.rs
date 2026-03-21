mod consts;
mod decode;
mod encode;

use std::fs::{File, create_dir_all, remove_file};
use std::path::{Path, PathBuf};

use anyhow::{Error, Result, anyhow, bail, ensure};
use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use walkdir::WalkDir;

use crate::consts::{
    DEFAULT_BITRATE, DEFAULT_COMPLEXITY, DEFAULT_QUALITY, PROGRESS_BAR_CHARS, PROGRESS_BAR_TEMPLATE,
};
use crate::decode::DecodedData;
use crate::encode::{ImageFormat, ImageProcessor, OpusOggEncoder};

#[derive(Parser)]
struct Cli {
    /// Directory of files to be processed. Scanned recursively
    source: PathBuf,
    /// Directory to store the results
    output: PathBuf,
    /// Skip files that failed to be processed
    #[arg(short, long)]
    skip_errors: bool,
    /// Suppress error messages
    #[arg(long)]
    quiet: bool,

    /// Opus bitrate in kbps, default is 128.0
    #[arg(short, long)]
    bitrate: Option<f32>,
    /// Opus encoding complexity, value from 1 to 10, default is 10
    #[arg(short, long)]
    complexity: Option<i32>,
    /// Image format, possible values are copy/png/jpeg/webp/avif, default is copy
    #[arg(short, long)]
    format: Option<String>,
    /// Target dimensions of cover images, in the format `WxH`. No resizing by default
    #[arg(short, long = "img-dims")]
    dimensions: Option<String>,
    /// Image encoding quality, value from 0.0 to 100.0, default is 90.0
    #[arg(short, long)]
    quality: Option<f32>,
}

fn main() -> Result<()> {
    let params = Cli::parse();

    let bitrate_kbps = params.bitrate.unwrap_or_else(|| {
        if !params.quiet {
            eprintln!("Using default bitrate: {}kbps", DEFAULT_BITRATE);
        }
        DEFAULT_BITRATE
    });
    ensure!(bitrate_kbps >= 6.0, "bitrate {}kbps is too low", bitrate_kbps);
    ensure!(bitrate_kbps <= 256.0, "bitrate {}kbps is too high", bitrate_kbps);
    let bitrate = (bitrate_kbps * 1000.0).round() as _;

    let complexity = params.complexity.unwrap_or_else(|| {
        if !params.quiet {
            eprintln!("Using default complexity: {}", DEFAULT_COMPLEXITY);
        }
        DEFAULT_COMPLEXITY
    });
    ensure!(matches!(complexity, 1..=10), "complexity should be in range 1-10");

    let target_format =
        match params.format.as_deref().unwrap_or("copy").to_ascii_lowercase().as_str() {
            "copy" => ImageFormat::Copy,
            "png" => ImageFormat::Png,
            "jpeg" => ImageFormat::Jpeg,
            "webp" => ImageFormat::Webp,
            "avif" => ImageFormat::Avif,
            _ => bail!("invalid image format, copy/png/jpeg/webp/avif is accepted"),
        };

    let new_dimensions = params
        .dimensions
        .map(|str| -> Result<_> {
            let err = || anyhow!("dimensions should be in the format of \"WxH\"");
            let (w, h) = str.split_once('x').ok_or_else(err)?;
            Ok((w.parse().map_err(|_| err())?, h.parse().map_err(|_| err())?))
        })
        .transpose()?;

    let quality = params.quality.unwrap_or_else(|| {
        if !params.quiet && !matches!(target_format, ImageFormat::Copy) {
            eprintln!("Using default image quality: {}", DEFAULT_QUALITY);
        }
        DEFAULT_QUALITY
    });
    ensure!(matches!(quality, 0.0..100.0), "quality value should be in range 0.0-100.0");

    create_dir_all(&params.output)?;

    let files: Vec<_> = WalkDir::new(params.source)
        .into_iter()
        .flatten() // ignore error results
        .filter(|d| d.file_type().is_file())
        .map(|d| d.into_path())
        .filter(|path| {
            let ext = path.extension().and_then(|ext| ext.to_str());
            matches!(ext, Some("ncm" | "flac" | "mp3" | "wav"))
        })
        .collect();

    let progress_style = ProgressStyle::with_template(PROGRESS_BAR_TEMPLATE)
        .unwrap()
        .progress_chars(PROGRESS_BAR_CHARS);
    let progress_bar = ProgressBar::new(files.len() as _).with_style(progress_style);
    progress_bar.inc(0);

    files
        .into_par_iter()
        .map(|path| -> Result<Option<(Error, _, _)>> {
            macro_rules! unwrap {
                ($val:expr, $new_path:expr) => {
                    match $val {
                        Ok(val) => val,
                        Err(err) if params.skip_errors => {
                            return Ok(Some((err.into(), path, $new_path)))
                        }
                        Err(err) => return Err(err.into()),
                    }
                };
            }

            let decoded = unwrap!(
                if path.extension().unwrap() == "ncm" {
                    DecodedData::from_ncm_file(&path)
                } else {
                    DecodedData::from_file(&path)
                },
                None
            );

            let new_filename = Path::new(path.file_name().unwrap()).with_extension("opus");
            let new_path = params.output.join(new_filename);
            let new_file = unwrap!(File::create(&new_path), None);

            let mut encoder = unwrap!(
                OpusOggEncoder::new(
                    decoded,
                    bitrate,
                    complexity,
                    &mut ImageProcessor { target_format, new_dimensions, quality },
                    new_file,
                ),
                Some(new_path)
            );

            while unwrap!(encoder.write_packet(), Some(new_path)) {}

            Ok(None)
        })
        .try_for_each(|result| {
            progress_bar.inc(1);
            if let Ok(Some((err, path, new_path))) = &result {
                if let Some(path) = new_path {
                    remove_file(path)?;
                }

                if !params.quiet {
                    eprintln!("Error when processing file: {}", path.display());
                    eprintln!("Error: {}", err);
                }
            }
            result.map(|_| ())
        })?;

    progress_bar.finish_and_clear();

    Ok(())
}
