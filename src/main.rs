mod consts;
mod decode;
mod encode;

use std::fs::{File, OpenOptions, create_dir_all, remove_file};
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail, ensure};
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

    /// Overwrite existing files
    #[arg(short, long)]
    overwrite: bool,
    /// Skip files with existing target files
    #[arg(short = 'n', long)]
    only_new: bool,
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

    macro_rules! log {
        ($($args:expr),*) => {
            if !params.quiet {
                eprintln!($($args),*);
            }
        };
    }

    let bitrate_kbps = params.bitrate.unwrap_or_else(|| {
        log!("Using default bitrate: {}kbps", DEFAULT_BITRATE);
        DEFAULT_BITRATE
    });
    ensure!(matches!(bitrate_kbps, 6.0..=256.0), "bitrate should be in range 6.0-256.0");
    let bitrate = (bitrate_kbps * 1000.0).round() as _;

    let complexity = params.complexity.unwrap_or_else(|| {
        log!("Using default complexity: {}", DEFAULT_COMPLEXITY);
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
        .map(|str| {
            let (w, h) = str.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .map(|opt| opt.ok_or_else(|| anyhow!("dimensions should be in the format of \"WxH\"")))
        .transpose()?;

    let quality = params.quality.unwrap_or_else(|| {
        if !matches!(target_format, ImageFormat::Copy) {
            log!("Using default image quality: {}", DEFAULT_QUALITY);
        }
        DEFAULT_QUALITY
    });
    ensure!(matches!(quality, 0.0..100.0), "quality value should be in range 0.0-100.0");

    if !params.output.exists() {
        create_dir_all(&params.output)?;
        log!("Created directory: {}", params.output.display());
    }

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
    progress_bar.inc(0); // show the bar

    files.into_par_iter().try_for_each(|path| -> Result<()> {
        macro_rules! unwrap {
            ($result:expr) => {
                match $result {
                    Ok(val) => val,
                    Err(err) if params.skip_errors => {
                        progress_bar.suspend(|| {
                            log!("Error when processing file: {}", path.display());
                            log!("Error: {}", err);
                        });
                        progress_bar.inc(1);
                        return Ok(());
                    }
                    Err(err) => return Err(err.into()),
                }
            };
        }

        if params.only_new && path.exists() {
            log!("Skipping file: {}", path.display());
            return Ok(());
        }

        let decoded = unwrap!(if path.extension().unwrap() == "ncm" {
            DecodedData::from_ncm_file(&path)
        } else {
            DecodedData::from_file(&path)
        });

        let filename = Path::new(path.file_name().unwrap());
        let mut new_path = params.output.join(filename);
        new_path.set_extension("opus");

        let mut overwritten_or_filename_altered = false;
        let new_file = if params.overwrite {
            overwritten_or_filename_altered = new_path.exists();
            unwrap!(File::create(&new_path))
        } else {
            let mut new_stem = filename.file_stem().unwrap().to_string_lossy().into_owned();
            loop {
                match OpenOptions::new().write(true).create_new(true).open(&new_path) {
                    Ok(file) => break file,
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                        // add a number prefix to the filename stem
                        // for example, "thing" renamed to "thing (1)" and "thing (41)" to "thing (42)"
                        new_stem = if let Some(left) = new_stem.strip_suffix(')')
                            && let Some((stem, num_str)) = left.rsplit_once(" (")
                            && let Ok(n) = num_str.parse::<usize>()
                        {
                            format!("{stem} ({})", n + 1)
                        } else {
                            format!("{new_stem} (1)")
                        };
                        new_path.set_file_name(&new_stem);
                        new_path.add_extension("opus");
                        overwritten_or_filename_altered = true;
                    }
                    Err(e) => unwrap!(Err(e)),
                }
            }
        };

        macro_rules! unwrap_clean {
            ($result:expr) => {{
                let result = $result;
                if result.is_err() {
                    let _ = remove_file(&new_path);
                }
                unwrap!(result)
            }};
        }

        let mut encoder = unwrap_clean!(OpusOggEncoder::new(
            decoded,
            bitrate,
            complexity,
            &mut ImageProcessor { target_format, new_dimensions, quality },
            new_file,
        ));

        while unwrap_clean!(encoder.write_packet()) {}

        if overwritten_or_filename_altered {
            let og_filename = path.file_name().unwrap().display();
            let new_filename = new_path.file_name().unwrap().display();
            progress_bar.suspend(|| {
                if params.overwrite {
                    log!("{} saved as and overwrote {}", og_filename, new_filename);
                } else {
                    log!("{} saved as {}", og_filename, new_filename);
                }
            });
        }

        progress_bar.inc(1);
        Ok(())
    })?;

    progress_bar.finish();
    Ok(())
}
