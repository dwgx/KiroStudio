//! Inbound image downscaling and re-encoding
//!
//! Downscales the base64-encoded images carried in Anthropic protocol ContentBlocks **locally on CPU** to
//! a long side <= `KIRO_RS_IMAGE_MAX_LONG_SIDE` px and a byte size <= `KIRO_RS_IMAGE_MAX_BYTES`,
//! then re-encodes to base64 and writes it back into the KiroImage. Why this step is required:
//!
//! 1. The AWS Q (`q.us-east-1.amazonaws.com`) backend enforces a hard per-field size limit. A ~700 KB
//!    toolResult.content[0].text triggers `CONTENT_LENGTH_EXCEEDS_THRESHOLD`,
//!    and an iPhone screenshot (1206x2622 PNG) whose single base64 string is ~700K chars triggers it too.
//! 2. Anthropic recommends a long side <= 1568 px; this value is the vision encoder's patch
//!    grid boundary. Beyond it the server downscales again, yet tokens are still billed against the original.
//! 3. ChatGPT/OpenAI servers downscale to this size automatically; AWS Q does not. That is the root
//!    cause of the same iPhone screenshots succeeding on GPT models while Kiro Opus returns 400.
//!
//! Design principles:
//! - Small images pass through directly (no decode, no re-encode, zero overhead)
//! - Large images are downscaled to the long-side cap and re-encoded as JPEG (PNG/WebP/JPEG all
//!   emit JPEG; GIF is the exception and keeps its original format because it may be animated)
//! - On decode failure **keep the original image** and log a warning; a bad image must never fail the whole request
//! - Everything is driven by `KIRO_RS_IMAGE_*` env vars, sharing the same contract as the observability env-var family
//!
//! 移植自 GreyGunG/Kiro-RS-Tool 的 `image_resize.rs`（多 fork 合并增强版）。与源模块的差异：
//! 去掉了 `estimate_image_tokens` 与 `RequestImageLimits`（KiroStudio 已有自己的 token 估算与
//! `MAX_TOTAL_IMAGES` 配额）；`ResizeError` 用手工 `Display` 替代 `thiserror` 派生，避免新增依赖。
//! 所有 `KIRO_RS_IMAGE_*` 环境变量、默认值、收敛算法均与源模块保持一致。

use std::io::Cursor;

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use image::{ImageFormat, ImageReader, imageops::FilterType};
use tracing::{debug, warn};

/// Default long-side threshold (Anthropic's recommended value)
const DEFAULT_MAX_LONG_SIDE: u32 = 1568;
/// Default byte threshold (leaves a safe margin below the AWS Q per-field limit)
const DEFAULT_MAX_BYTES: usize = 400_000;
/// Default JPEG quality
const DEFAULT_JPEG_QUALITY: u8 = 85;
/// Default per-image base64 hard limit. This is a security cap, not the resize target.
const DEFAULT_MAX_BASE64_BYTES: usize = 8 * 1024 * 1024;
/// Default per-image decoded-byte hard limit.
const DEFAULT_MAX_DECODED_BYTES: usize = 6 * 1024 * 1024;
/// Default decoded pixel hard limit, matching the review recommendation.
const DEFAULT_MAX_PIXELS: u64 = 40_000_000;

/// Inbound image processor configuration
#[derive(Debug, Clone, Copy)]
pub struct ResizeConfig {
    pub enabled: bool,
    pub max_long_side: u32,
    pub max_bytes: usize,
    pub jpeg_quality: u8,
    pub max_base64_bytes: usize,
    pub max_decoded_bytes: usize,
    pub max_pixels: u64,
}

impl ResizeConfig {
    /// Reads from `KIRO_RS_IMAGE_*` env vars, falling back to defaults when unset
    pub fn from_env() -> Self {
        let enabled = !matches!(
            std::env::var("KIRO_RS_IMAGE_RESIZE")
                .unwrap_or_else(|_| "1".to_string())
                .to_ascii_lowercase()
                .as_str(),
            "0" | "false" | "no" | "off"
        );
        let max_long_side = std::env::var("KIRO_RS_IMAGE_MAX_LONG_SIDE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MAX_LONG_SIDE);
        let max_bytes = std::env::var("KIRO_RS_IMAGE_MAX_BYTES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MAX_BYTES);
        let jpeg_quality = std::env::var("KIRO_RS_IMAGE_JPEG_QUALITY")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_JPEG_QUALITY);
        let max_base64_bytes = std::env::var("KIRO_RS_IMAGE_MAX_BASE64_BYTES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MAX_BASE64_BYTES);
        let max_decoded_bytes = std::env::var("KIRO_RS_IMAGE_MAX_DECODED_BYTES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MAX_DECODED_BYTES);
        let max_pixels = std::env::var("KIRO_RS_IMAGE_MAX_PIXELS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MAX_PIXELS);
        Self {
            enabled,
            max_long_side,
            max_bytes,
            jpeg_quality,
            max_base64_bytes,
            max_decoded_bytes,
            max_pixels,
        }
    }
}

/// Result of processing one image (explicitly distinguishes the "kept as-is" and "re-encoded" states)
///
/// `was_resized` / `original_bytes` / `final_bytes` are consumed only by test assertions and structured logs;
/// non-test runtime paths do not read them, so the whole struct is marked `allow(dead_code)` to keep the diagnostic fields.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ProcessedImage {
    /// Output format ("jpeg" / "png" / "gif" / "webp")
    pub format: String,
    /// Output base64 string
    pub data_base64: String,
    /// Whether re-encoding actually happened (used for logs/metrics)
    pub was_resized: bool,
    /// Input byte count (before decoding)
    pub original_bytes: usize,
    /// Output byte count
    pub final_bytes: usize,
}

/// Main entry: processes a single inbound image with the rule "small enough -> pass / large -> shrink"
///
/// `format` is the last segment of the source media-type ("png" / "jpeg" / "gif" / "webp"),
/// `data_base64` is the base64-encoded raw bytes.
///
/// Never panics. Hard-limit failures return an error so callers can reject unsafe payloads
/// instead of passing huge or failed-decode images through unchecked.
pub fn maybe_shrink_image(
    cfg: ResizeConfig,
    format: &str,
    data_base64: &str,
) -> Result<ProcessedImage, ResizeError> {
    let format_lc = format.to_ascii_lowercase();
    let original_bytes = data_base64.len();
    validate_base64_limits(cfg, data_base64)?;

    // 1) Disabled: return as-is
    if !cfg.enabled {
        validate_passthrough_safe(cfg, &format_lc, data_base64)?;
        return Ok(passthrough(format_lc, data_base64));
    }
    // 2) Bytes small enough: return as-is (small images need no work, saves CPU)
    if data_base64.len() <= cfg.max_bytes {
        // Even with small bytes, check whether the dimensions are oversized (rare, e.g. a 7000x100 banner)
        // Use a lightweight probe (header only): image::ImageReader::with_guessed_format
        match peek_dimensions_checked(cfg, &format_lc, data_base64) {
            Ok(Some((w, h))) if w.max(h) <= cfg.max_long_side => {
                return Ok(passthrough(format_lc, data_base64));
            }
            Ok(Some(_)) => {
                // Small bytes but oversized dimensions: still take the re-encode path
            }
            Ok(None) => {
                // Small opaque/corrupt inputs are left to upstream MIME validation; the hard-size
                // path below refuses oversized failed-decode payloads.
                return Ok(passthrough(format_lc, data_base64));
            }
            Err(e) => return Err(e),
        }
    }
    // 3) Animated images (multi-frame GIF) keep their original format unchanged - JPEG would lose the animation
    if format_lc == "gif" {
        validate_passthrough_safe(cfg, &format_lc, data_base64)?;
        if data_base64.len() > cfg.max_bytes {
            return Err(ResizeError::LimitExceeded(format!(
                "gif image too large for passthrough: {} > {} base64 bytes",
                data_base64.len(),
                cfg.max_bytes
            )));
        }
        debug!(
            target: "kiro_rs::image_resize",
            original_bytes = original_bytes,
            "skip GIF (potential animation)"
        );
        return Ok(passthrough(format_lc, data_base64));
    }

    // 4) Actually shrink the image
    match shrink_static_image(cfg, &format_lc, data_base64) {
        Ok(processed) => Ok(processed),
        Err(e) => {
            warn!(
                target: "kiro_rs::image_resize",
                error = %e,
                format = %format_lc,
                original_bytes = original_bytes,
                "image resize failed"
            );
            Err(e)
        }
    }
}

fn passthrough(format: String, data_base64: &str) -> ProcessedImage {
    let n = data_base64.len();
    // Correct the format from the real magic bytes: the host may label it png while the bytes are actually jpeg,
    // and faithful passthrough would trip Bedrock's strict MIME check with IMAGE_MIME_MISMATCH. If detection fails, keep the original label (never drop the image).
    let format = match detect_format_from_bytes(data_base64) {
        Some(real) if real != format => {
            debug!(
                target: "kiro_rs::image_resize",
                declared = %format,
                actual = %real,
                "passthrough format corrected from magic bytes"
            );
            real
        }
        _ => format,
    };
    ProcessedImage {
        format,
        data_base64: data_base64.to_string(),
        was_resized: false,
        original_bytes: n,
        final_bytes: n,
    }
}

/// Detects the format from the real magic bytes, returning "png"/"jpeg"/"gif"/"webp".
/// Decoding only the first ~16 bytes (first 24 base64 chars) is enough to cover every magic number and saves CPU.
/// On detection failure (decode error / unknown format) it returns None, and the caller safely keeps the original label.
fn detect_format_from_bytes(data_base64: &str) -> Option<String> {
    let head: String = data_base64.chars().take(24).collect();
    let bytes = BASE64.decode(head.as_bytes()).ok()?;
    match image::guess_format(&bytes).ok()? {
        ImageFormat::Png => Some("png".to_string()),
        ImageFormat::Jpeg => Some("jpeg".to_string()),
        ImageFormat::Gif => Some("gif".to_string()),
        ImageFormat::WebP => Some("webp".to_string()),
        _ => None,
    }
}

/// Reads the encoded bytes and asks the image reader for dimensions without decoding pixels.
fn peek_dimensions_checked(
    cfg: ResizeConfig,
    format: &str,
    data_base64: &str,
) -> Result<Option<(u32, u32)>, ResizeError> {
    validate_base64_limits(cfg, data_base64)?;
    let bytes = BASE64
        .decode(data_base64)
        .map_err(|e| ResizeError::Base64(e.to_string()))?;
    if bytes.len() > cfg.max_decoded_bytes {
        return Err(ResizeError::LimitExceeded(format!(
            "decoded image too large: {} > {} bytes",
            bytes.len(),
            cfg.max_decoded_bytes
        )));
    }
    read_dimensions_from_raw(cfg, format, &bytes)
}

fn read_dimensions_from_raw(
    cfg: ResizeConfig,
    format: &str,
    raw: &[u8],
) -> Result<Option<(u32, u32)>, ResizeError> {
    let cursor = Cursor::new(raw);
    let mut reader = ImageReader::new(cursor);
    if let Some(fmt) = guess_format(format) {
        reader.set_format(fmt);
    } else {
        reader = match reader.with_guessed_format() {
            Ok(r) => r,
            Err(_) => return Ok(None),
        };
    }
    let Some((w, h)) = reader.into_dimensions().ok() else {
        return Ok(None);
    };
    validate_pixel_count(cfg, w, h)?;
    Ok(Some((w, h)))
}

fn validate_passthrough_safe(
    cfg: ResizeConfig,
    format: &str,
    data_base64: &str,
) -> Result<(), ResizeError> {
    match peek_dimensions_checked(cfg, format, data_base64) {
        Ok(Some(_)) => Ok(()),
        Ok(None) | Err(ResizeError::Base64(_)) | Err(ResizeError::Decode(_))
            if data_base64.len() <= cfg.max_bytes =>
        {
            Ok(())
        }
        Ok(None) | Err(ResizeError::Base64(_)) | Err(ResizeError::Decode(_)) => {
            Err(ResizeError::LimitExceeded(
                "oversized image payload has unknown or invalid dimensions".to_string(),
            ))
        }
        Err(e) => Err(e),
    }
}

fn guess_format(s: &str) -> Option<ImageFormat> {
    match s {
        "png" => Some(ImageFormat::Png),
        "jpeg" | "jpg" => Some(ImageFormat::Jpeg),
        "webp" => Some(ImageFormat::WebP),
        "gif" => Some(ImageFormat::Gif),
        _ => None,
    }
}

fn shrink_static_image(
    cfg: ResizeConfig,
    format: &str,
    data_base64: &str,
) -> Result<ProcessedImage, ResizeError> {
    let original_bytes = data_base64.len();

    validate_base64_limits(cfg, data_base64)?;
    let raw = BASE64
        .decode(data_base64)
        .map_err(|e| ResizeError::Base64(e.to_string()))?;
    if raw.len() > cfg.max_decoded_bytes {
        return Err(ResizeError::LimitExceeded(format!(
            "decoded image too large: {} > {} bytes",
            raw.len(),
            cfg.max_decoded_bytes
        )));
    }
    let Some((w, h)) = read_dimensions_from_raw(cfg, format, &raw)? else {
        return Err(ResizeError::Decode(
            "image dimensions are unavailable".to_string(),
        ));
    };

    let cursor = Cursor::new(&raw);
    let mut reader = ImageReader::new(cursor);
    if let Some(fmt) = guess_format(format) {
        reader.set_format(fmt);
    } else {
        reader = reader
            .with_guessed_format()
            .map_err(|e| ResizeError::Decode(e.to_string()))?;
    }
    let img = reader
        .decode()
        .map_err(|e| ResizeError::Decode(e.to_string()))?;

    // Initial proportional scaling to the configured long-side cap (preserves aspect ratio).
    let (w, h) = (w.max(img.width()), h.max(img.height()));
    let long_initial = w.max(h);
    let mut cur_long = long_initial.min(cfg.max_long_side).max(1);

    // Two-level convergence to honor max_bytes: for each long-side cap, encode at the
    // configured quality and progressively lower the quality; if the budget still isn't met
    // at the minimum quality, downscale the long side further and retry. This guarantees the
    // output actually fits max_bytes (down to a small floor) instead of returning oversized data.
    const MIN_JPEG_QUALITY: u8 = 35;
    const MIN_LONG_SIDE: u32 = 256;
    let mut out;
    let mut quality;
    loop {
        let resized = if w.max(h) > cur_long {
            let scale = cur_long as f32 / w.max(h) as f32;
            let new_w = ((w as f32) * scale).round().max(1.0) as u32;
            let new_h = ((h as f32) * scale).round().max(1.0) as u32;
            // FilterType::Lanczos3 gives good visual quality; ~80ms for 1206x2622 -> 1024x~470 on one core.
            img.resize_exact(new_w, new_h, FilterType::Lanczos3)
        } else {
            img.clone()
        };
        // Force RGB8 (JPEG has no alpha; dropping alpha is harmless for screenshots).
        let rgb = resized.to_rgb8();
        quality = cfg.jpeg_quality;
        loop {
            out = Vec::with_capacity(64 * 1024);
            let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
            rgb.write_with_encoder(encoder)
                .map_err(|e| ResizeError::Encode(e.to_string()))?;
            // base64 inflates by ~4/3; stop once the encoded base64 length fits the budget.
            if out.len().saturating_mul(4) / 3 <= cfg.max_bytes || quality <= MIN_JPEG_QUALITY {
                break;
            }
            quality = quality.saturating_sub(10).max(MIN_JPEG_QUALITY);
        }
        if out.len().saturating_mul(4) / 3 <= cfg.max_bytes || cur_long <= MIN_LONG_SIDE {
            break;
        }
        // Quality floor hit but still oversized: shrink the long side and retry.
        cur_long = ((cur_long as f32 * 0.8) as u32).max(MIN_LONG_SIDE);
    }
    let final_bytes_raw = out.len();
    let data_b64 = BASE64.encode(&out);
    let final_bytes = data_b64.len();

    debug!(
        target: "kiro_rs::image_resize",
        original_bytes = original_bytes,
        final_bytes = final_bytes,
        ratio = format!("{:.2}x", original_bytes as f64 / final_bytes.max(1) as f64),
        decoded_w = w,
        decoded_h = h,
        out_jpeg_bytes = final_bytes_raw,
        "image resized"
    );

    Ok(ProcessedImage {
        format: "jpeg".to_string(),
        data_base64: data_b64,
        was_resized: true,
        original_bytes,
        final_bytes,
    })
}

fn validate_base64_limits(cfg: ResizeConfig, data_base64: &str) -> Result<(), ResizeError> {
    if data_base64.len() > cfg.max_base64_bytes {
        return Err(ResizeError::LimitExceeded(format!(
            "image base64 too large: {} > {} bytes",
            data_base64.len(),
            cfg.max_base64_bytes
        )));
    }
    let decoded_estimate = estimated_decoded_len(data_base64);
    if decoded_estimate > cfg.max_decoded_bytes {
        return Err(ResizeError::LimitExceeded(format!(
            "decoded image too large: {} > {} bytes",
            decoded_estimate, cfg.max_decoded_bytes
        )));
    }
    Ok(())
}

fn estimated_decoded_len(data_base64: &str) -> usize {
    let trimmed = data_base64.trim_end_matches('=');
    trimmed.len().saturating_mul(3) / 4
}

fn validate_pixel_count(cfg: ResizeConfig, w: u32, h: u32) -> Result<(), ResizeError> {
    let pixels = (w as u64).saturating_mul(h as u64);
    if pixels > cfg.max_pixels {
        return Err(ResizeError::LimitExceeded(format!(
            "image pixels too large: {} > {}",
            pixels, cfg.max_pixels
        )));
    }
    Ok(())
}

#[derive(Debug)]
pub enum ResizeError {
    #[allow(dead_code)]
    LimitExceeded(String),
    Base64(String),
    Decode(String),
    Encode(String),
}

impl std::fmt::Display for ResizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResizeError::LimitExceeded(msg) => write!(f, "image rejected: {}", msg),
            ResizeError::Base64(msg) => write!(f, "base64 decode: {}", msg),
            ResizeError::Decode(msg) => write!(f, "image decode: {}", msg),
            ResizeError::Encode(msg) => write!(f, "image encode: {}", msg),
        }
    }
}

impl std::error::Error for ResizeError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_png(w: u32, h: u32) -> String {
        use image::{Rgb, RgbImage};
        let mut img = RgbImage::new(w, h);
        // Gradient fill: its compression ratio is closer to real screenshots than a solid color
        for y in 0..h {
            for x in 0..w {
                img.put_pixel(x, y, Rgb([(x % 256) as u8, (y % 256) as u8, 128]));
            }
        }
        let mut buf = Vec::new();
        img.write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)
            .unwrap();
        BASE64.encode(&buf)
    }

    fn test_cfg() -> ResizeConfig {
        ResizeConfig {
            enabled: true,
            max_long_side: 1568,
            max_bytes: 400_000,
            jpeg_quality: 85,
            max_base64_bytes: 8 * 1024 * 1024,
            max_decoded_bytes: 8 * 1024 * 1024,
            max_pixels: 40_000_000,
        }
    }

    #[test]
    fn small_image_passes_through() {
        let cfg = test_cfg();
        let small = make_png(64, 64);
        let out = maybe_shrink_image(cfg, "png", &small).unwrap();
        assert!(!out.was_resized);
        assert_eq!(out.format, "png");
        assert_eq!(out.data_base64, small);
    }

    #[test]
    fn iphone_screenshot_gets_shrunk_below_limit() {
        let cfg = test_cfg();
        // 1206x2622 ~ iPhone Pro Max screenshot ratio
        let big = make_png(1206, 2622);
        let out = maybe_shrink_image(cfg, "png", &big).unwrap();
        assert!(out.was_resized, "should have been resized");
        assert_eq!(out.format, "jpeg", "should have been re-encoded as JPEG");
        assert!(
            out.final_bytes < cfg.max_bytes,
            "final {} should be < cap {}",
            out.final_bytes,
            cfg.max_bytes
        );
        // The gradient test image compresses worse than a real screenshot (blocky UI elements); we only need it below the threshold
        let _ = out.original_bytes;
    }

    #[test]
    fn within_dimensions_but_oversized_bytes_converges_under_cap() {
        // Dimensions are under max_long_side, so the resize branch is skipped; the only way
        // to honor max_bytes is the progressive quality reduction in the encode loop.
        let cfg = ResizeConfig {
            max_bytes: 20_000,
            ..test_cfg()
        };
        let img = make_png(1024, 1024);
        let out = maybe_shrink_image(cfg, "png", &img).unwrap();
        assert!(out.was_resized, "should have been re-encoded");
        assert!(
            out.final_bytes <= cfg.max_bytes,
            "final {} must be <= cap {} after quality reduction",
            out.final_bytes,
            cfg.max_bytes
        );
    }

    #[test]
    fn gif_passes_through_to_preserve_animation() {
        let cfg = test_cfg();
        // A 1x1 GIF is enough; what matters here is exercising the branch
        let tiny_gif = "R0lGODlhAQABAAAAACw=";
        let out = maybe_shrink_image(cfg, "gif", tiny_gif).unwrap();
        assert!(!out.was_resized);
        assert_eq!(out.format, "gif");
    }

    #[test]
    fn disabled_config_passes_through_even_huge() {
        let cfg = ResizeConfig {
            enabled: false,
            ..test_cfg()
        };
        let big = make_png(1206, 2622);
        let out = maybe_shrink_image(cfg, "png", &big).unwrap();
        assert!(!out.was_resized);
        assert_eq!(out.format, "png");
    }

    #[test]
    fn small_corrupt_data_passes_through_with_warning() {
        let cfg = ResizeConfig {
            max_long_side: 1568,
            max_bytes: 2_000,
            ..test_cfg()
        };
        // Small corrupt data can still pass through for upstream MIME validation.
        let bogus = "X".repeat(1000);
        let out = maybe_shrink_image(cfg, "png", &bogus).unwrap();
        assert!(!out.was_resized, "corrupt input should fall through");
        assert_eq!(out.format, "png");
        assert_eq!(out.data_base64, bogus);
    }

    fn make_jpeg(w: u32, h: u32) -> String {
        use image::{Rgb, RgbImage};
        let mut img = RgbImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                img.put_pixel(x, y, Rgb([(x % 256) as u8, (y % 256) as u8, 128]));
            }
        }
        let mut buf = Vec::new();
        img.write_to(&mut Cursor::new(&mut buf), ImageFormat::Jpeg)
            .unwrap();
        BASE64.encode(&buf)
    }

    #[test]
    fn mislabeled_png_header_jpeg_bytes_corrected_to_jpeg() {
        let cfg = test_cfg();
        // Real JPEG bytes, but the caller mislabels format="png" (host-side header/body mismatch, faithfully passed through).
        // Small images take the passthrough path. The outbound format must be corrected to jpeg per the real bytes, otherwise Bedrock returns IMAGE_MIME_MISMATCH.
        let jpeg = make_jpeg(64, 64);
        let out = maybe_shrink_image(cfg, "png", &jpeg).unwrap();
        assert_eq!(out.data_base64, jpeg, "must not mutate image bytes");
        assert_eq!(
            out.format, "jpeg",
            "format must be corrected to match actual JPEG bytes"
        );
    }

    #[test]
    fn oversized_base64_is_rejected_before_decode() {
        let cfg = ResizeConfig {
            max_base64_bytes: 32,
            max_decoded_bytes: 1024,
            ..test_cfg()
        };
        let err = maybe_shrink_image(cfg, "png", &"A".repeat(64)).unwrap_err();
        assert!(matches!(err, ResizeError::LimitExceeded(_)));
    }

    #[test]
    fn oversized_pixels_are_rejected_before_full_decode() {
        let cfg = ResizeConfig {
            max_pixels: 1_000,
            ..test_cfg()
        };
        let img = make_png(64, 64);
        let err = maybe_shrink_image(cfg, "png", &img).unwrap_err();
        assert!(matches!(err, ResizeError::LimitExceeded(_)));
    }

    #[test]
    fn oversized_corrupt_payload_is_not_passed_through() {
        let cfg = ResizeConfig {
            max_bytes: 100,
            ..test_cfg()
        };
        let bogus = "X".repeat(1000);
        assert!(maybe_shrink_image(cfg, "png", &bogus).is_err());
    }
}
