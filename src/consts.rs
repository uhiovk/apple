pub const SAMPLE_RATE: usize = 48000;
pub const FRAME_SIZE_MS: f32 = 20.0;
pub const FRAME_SIZE: usize = (SAMPLE_RATE as f32 * FRAME_SIZE_MS / 1000.0).round() as _;
pub const MAGIC: u32 = 0x1FEE1BAD;
pub const VENDOR_STRING: &[u8] = b"Your PC";

pub const DEFAULT_BITRATE: f32 = 128.0;
pub const DEFAULT_QUALITY: f32 = 90.0;

pub const PROGRESS_BAR_TEMPLATE: &str =
    "[{bar:50}] {elapsed_precise}/{eta_precise} {pos}/{len} {percent}%";
pub const PROGRESS_BAR_CHARS: &str = "##-";
