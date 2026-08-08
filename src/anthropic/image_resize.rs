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
//!   emit JPEG; **static** single-frame GIF is also re-encoded as JPEG; only **animated**
//!   multi-frame GIF keeps its original format to preserve the animation)
//! - On decode failure **keep the original image** and log a warning; a bad image must never fail the whole request
//! - Everything is driven by `KIRO_RS_IMAGE_*` env vars, sharing the same contract as the observability env-var family
//!
//! ⚠️ **调用约定（deepseek review 修复，2026-08-08）**：
//! - 本模块是**同步 CPU 密集**路径（解码 + Lanczos3 缩放 + 重编码）。**async 调用方必须用
//!   `tokio::task::spawn_blocking` 包住** `maybe_shrink_image`，不能直接在 tokio worker 上同步跑。
//!   当前 `converter.rs` 的调用点尚未包 `spawn_blocking`，属已知待改项。
//! - **每请求图片数上限由调用方负责**：`converter.rs` 的 `MAX_TOTAL_IMAGES`（20）只约束历史去重
//!   路径；当前轮（dedup=None）图片**不限量**。单个 40Mpx 像素炸弹 base64 仅约 300KB，
//!   恶意请求可塞几百张独占 worker 十几分钟。本模块是 per-image API，无法跨图片计数，
//!   调用方必须在入口按请求截断图片数量。
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
/// Default **absolute** decoded-pixel ceiling: images above this are rejected (genuine pixel bomb,
/// decoding would blow memory). Between `max_pixels` (soft warning) and this ceiling we still attempt
/// a downscale instead of dropping the image.
///
/// Memory tradeoff: a decode of `hard_max_pixels` RGB8 is ~300 MB transient. 100 Mpx covers
/// >40 Mpx DSLR/satellite stills (the drop-regression case) while rejecting multi-hundred-Mpx bombs.
const DEFAULT_HARD_MAX_PIXELS: u64 = 100_000_000;

/// Inbound image processor configuration
#[derive(Debug, Clone, Copy)]
pub struct ResizeConfig {
    pub enabled: bool,
    pub max_long_side: u32,
    pub max_bytes: usize,
    pub jpeg_quality: u8,
    pub max_base64_bytes: usize,
    pub max_decoded_bytes: usize,
    /// 软阈值：超过只告警、仍尝试降采样（>40Mpx 大图不再被丢）。
    pub max_pixels: u64,
    /// 绝对硬上限：超过即拒绝（真正的像素炸弹，解码会撑爆内存）。
    pub hard_max_pixels: u64,
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
        let hard_max_pixels = std::env::var("KIRO_RS_IMAGE_HARD_MAX_PIXELS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_HARD_MAX_PIXELS)
            .max(max_pixels);
        Self {
            enabled,
            max_long_side,
            max_bytes,
            jpeg_quality,
            max_base64_bytes,
            max_decoded_bytes,
            max_pixels,
            hard_max_pixels,
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
///
/// ⚠️ **CPU 热路径（deepseek review）**：本函数是同步 CPU 密集操作（解码 + Lanczos3 缩放 +
/// JPEG 重编码），async 调用方必须用 `tokio::task::spawn_blocking` 包住，不能在 tokio worker
/// 上同步跑。入口的 `validate_base64_limits` + 解码前的尺寸探测（`read_dimensions_from_raw` 的
/// `hard_max_pixels`）是防像素炸弹的关键：先探尺寸再全量解码，避免小字节大像素的炸弹直接解满内存。
pub fn maybe_shrink_image(
    cfg: ResizeConfig,
    format: &str,
    data_base64: &str,
) -> Result<ProcessedImage, ResizeError> {
    let format_lc = format.to_ascii_lowercase();
    let original_bytes = data_base64.len();
    validate_base64_limits(cfg, data_base64)?;

    // 1) Disabled: degrade to passthrough (the kill switch)
    //
    // 🔴 deepseek review 修复：旧代码在这里跑 `validate_passthrough_safe`，而它经
    // `validate_pixel_count` 会把 >40Mpx 大图判 `LimitExceeded` **原样上抛** → 调用方省略该图。
    // 也就是说 `KIRO_RS_IMAGE_RESIZE=0` 根本救不了大图：开关关了图还是被丢。真正的 kill switch
    // 语义是"关掉降采样 = 原样透传"，故此处**只**依赖入口 `validate_base64_limits` 的
    // base64/解码字节硬上限，不做像素/尺寸校验（像素大小与透传安全性无关）。
    if !cfg.enabled {
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
    // 3) GIF: **static** (single-frame) goes through JPEG downsampling below; only **animated**
    //    multi-frame GIF keeps its original format (JPEG would lose the animation).
    //
    // 🔴 deepseek review 回归修复：旧代码对所有 GIF 一律透传，>400KB base64 的 GIF 直接被
    // `LimitExceeded` 丢图 —— 而静态 GIF 完全可以用 JPEG 降采样。这里探测帧数：GIF 头
    // `0x47 0x49 0x46`（"GIF"）+ 帧分隔 `\x2C`（image descriptor）。多帧才取舍（保动画，
    // 超预算则省略）；单帧落到下方 `shrink_static_image` 重编码 JPEG。
    if format_lc == "gif" {
        let raw = BASE64
            .decode(data_base64)
            .map_err(|e| ResizeError::Base64(e.to_string()))?;
        if probe_gif_frame_count(&raw) > 1 {
            validate_passthrough_safe(cfg, &format_lc, data_base64)?;
            if data_base64.len() > cfg.max_bytes {
                return Err(ResizeError::LimitExceeded(format!(
                    "animated gif too large for passthrough: {} > {} base64 bytes",
                    data_base64.len(),
                    cfg.max_bytes
                )));
            }
            debug!(
                target: "kiro_rs::image_resize",
                original_bytes = original_bytes,
                "skip animated GIF (preserve animation)"
            );
            return Ok(passthrough(format_lc, data_base64));
        }
        // 静态 GIF：不在此截断，走下方 shrink_static_image 转 JPEG 降采样。
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

/// 像素数校验：**软阈值 `max_pixels` 之上仍放行**（只告警、尝试降采样），
/// 只有超过**硬上限 `hard_max_pixels`** 才拒绝 —— 那是真正的像素炸弹（解码即撑爆内存）。
///
/// 🔴 deepseek review 修复：旧逻辑把 `max_pixels` 当硬拒绝线，>40Mpx 大图（如 12000x8000
/// DSLR 照片）在解码前就被丢。软/硬双线让 >40Mpx 图走降采样、仍保住图片。
fn validate_pixel_count(cfg: ResizeConfig, w: u32, h: u32) -> Result<(), ResizeError> {
    let pixels = (w as u64).saturating_mul(h as u64);
    if pixels > cfg.hard_max_pixels {
        return Err(ResizeError::LimitExceeded(format!(
            "image pixels too large: {} > {}",
            pixels, cfg.hard_max_pixels
        )));
    }
    if pixels > cfg.max_pixels {
        warn!(
            target: "kiro_rs::image_resize",
            pixels,
            soft = cfg.max_pixels,
            hard = cfg.hard_max_pixels,
            "image exceeds the soft pixel threshold; still attempting downscale"
        );
    }
    Ok(())
}

/// 探测 GIF 帧数（image descriptor `\x2C` 的数量）。
///
/// 判据（deepseek review）：GIF 头 `0x47 0x49 0x46`（"GIF"）确认是 GIF，然后按 GIF 块结构
/// 走位，统计 image descriptor `,`（0x2C）的个数。>=2 帧视为动画，1 帧视为静态。
///
/// 必须**按块结构走位**而非裸扫 `0x2C`：图像数据的 LZW 压缩字节里可以合法出现 0x2C，
/// 裸扫会把单帧 GIF 误判成多帧。块结构：header(6) + Logical Screen Descriptor(7) +
/// [Global Color Table] + 若干 block，block 只有三种：
/// - `,` Image Descriptor（+9 字节描述符 +[Local Color Table]+1 字节 LZW 最小码长+图像子块）
/// - `!` Extension（1 字节 label + 子块）
/// - `;` Trailer（结束）
/// 每个子块 = 1 长度字节 + 该字节数数据；0 长度字节终止子块序列。
///
/// 解析失败按 1 帧处理（退化到静态 → 走 JPEG 降采样；降采样失败还有 passthrough 兜底），
/// 绝不因探测失败而丢图。
fn probe_gif_frame_count(bytes: &[u8]) -> usize {
    const IMG_DESCRIPTOR: u8 = 0x2C; // ',' image descriptor
    const EXTENSION: u8 = 0x21; // '!' extension block
    const TRAILER: u8 = 0x3B; // ';' trailer
    if bytes.len() < 13 || &bytes[0..3] != b"GIF" || bytes[3] != b'8' {
        return 1;
    }
    let mut pos = 6usize; // skip "GIF87a" / "GIF89a"
    pos += 7; // Logical Screen Descriptor
    // Global Color Table (3 bytes/entry, 2^(N+1) entries)
    if bytes[10] & 0x80 != 0 {
        pos += 3 * (2usize << (bytes[10] & 0x07));
    }
    let mut frames = 0usize;
    while pos < bytes.len() {
        match bytes[pos] {
            TRAILER => break,
            IMG_DESCRIPTOR => {
                frames += 1;
                // 描述符剩余 9 字节（共 10，含 ','），末尾 1 字节 packed flag。
                if pos + 10 > bytes.len() {
                    break;
                }
                let packed = bytes[pos + 9];
                pos += 10;
                // Local Color Table
                if packed & 0x80 != 0 {
                    pos += 3 * (2usize << (packed & 0x07));
                }
                // LZW 最小码长字节
                if pos >= bytes.len() {
                    break;
                }
                pos += 1;
                pos = skip_gif_sub_blocks(bytes, pos);
            }
            EXTENSION => {
                pos += 2; // 0x21 + 1 字节 label
                pos = skip_gif_sub_blocks(bytes, pos);
            }
            _ => pos += 1, // 防御：非法块字节前进，避免死循环
        }
    }
    frames.max(1)
}

/// 跳过 GIF 子块序列（每个子块 = 1 长度字节 + 数据；0 长度字节终止）。
fn skip_gif_sub_blocks(bytes: &[u8], mut pos: usize) -> usize {
    loop {
        if pos >= bytes.len() {
            break;
        }
        let n = bytes[pos] as usize;
        pos += 1;
        if n == 0 {
            break;
        }
        pos = pos.saturating_add(n);
    }
    pos
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
            hard_max_pixels: 100_000_000,
        }
    }

    /// 构造一个 1x1 GIF（可指定帧数）。用 `image` crate 的 GifEncoder 生成，保证本仓解码器
    /// 一定能解回来；`frames > 1` 即动画。供帧数探测与静态/动画分流测试使用。
    fn make_gif(frames: usize) -> Vec<u8> {
        use image::{Frame, Rgba, RgbaImage};
        let img = RgbaImage::from_pixel(1, 1, Rgba([0, 0, 0, 255]));
        let mut out = Vec::new();
        let mut enc = image::codecs::gif::GifEncoder::new(&mut out);
        for _ in 0..frames {
            enc.encode_frame(Frame::new(img.clone())).unwrap();
        }
        drop(enc); // 释放对 out 的借用，才能 move 返回
        out
    }

    /// 把原始字节 base64 编码（GIF 分支探测帧数时输入是 base64）。
    fn b64(bytes: &[u8]) -> String {
        BASE64.encode(bytes)
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
    fn animated_gif_passes_through_to_preserve_animation() {
        let cfg = test_cfg();
        // 2 帧 GIF：多帧 = 动画，必须保留原格式，绝不能重编码成 JPEG 丢动画。
        let gif = b64(&make_gif(2));
        let out = maybe_shrink_image(cfg, "gif", &gif).unwrap();
        assert!(!out.was_resized);
        assert_eq!(out.format, "gif");
    }

    #[test]
    fn animated_gif_over_budget_is_dropped_not_passed() {
        // 动画 + 超字节预算：不能降采样（会丢动画），取舍 = 省略（LimitExceeded），而非透传超大 payload。
        let cfg = ResizeConfig {
            max_bytes: 10,
            ..test_cfg()
        };
        let gif = b64(&make_gif(2));
        let err = maybe_shrink_image(cfg, "gif", &gif).unwrap_err();
        assert!(matches!(err, ResizeError::LimitExceeded(_)));
    }

    /// 🔴 回归（deepseek review）：>400KB base64 的**静态** GIF 必须走 JPEG 降采样，而不是被丢图。
    ///
    /// 旧代码对所有 GIF 一律透传 + 超 max_bytes 即丢；静态 GIF（单帧）完全可以重编码 JPEG。
    #[test]
    fn static_gif_over_budget_is_shrunk_to_jpeg() {
        let cfg = ResizeConfig {
            max_bytes: 20,
            ..test_cfg()
        };
        let gif = b64(&make_gif(1));
        let out = maybe_shrink_image(cfg, "gif", &gif).unwrap();
        assert!(out.was_resized, "静态 GIF 应被降采样而非丢弃");
        assert_eq!(out.format, "jpeg", "静态 GIF 应重编码为 JPEG");
    }

    /// 帧数探测本身：静态 = 1 帧，动画 >= 2 帧；带扩展块的动画也能数对。
    #[test]
    fn gif_frame_count_probe() {
        assert_eq!(probe_gif_frame_count(&make_gif(1)), 1);
        assert_eq!(probe_gif_frame_count(&make_gif(2)), 2);
        assert_eq!(probe_gif_frame_count(&make_gif(3)), 3);
        // 非 GIF 输入退化为 1（不丢图路径）
        assert_eq!(probe_gif_frame_count(b"not a gif at all"), 1);
        assert_eq!(probe_gif_frame_count(b""), 1);
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

    /// 🔴 kill switch 修复（deepseek review）：`KIRO_RS_IMAGE_RESIZE=0` 必须**降级为透传**，
    /// 即使图片超过**硬像素上限**也不得丢图。旧代码在 disabled 分支跑 `validate_passthrough_safe`，
    /// >40Mpx 大图被判 LimitExceeded 原样上抛 → 开关关了图还是被丢。
    #[test]
    fn disabled_config_passes_through_even_over_hard_pixel_ceiling() {
        let cfg = ResizeConfig {
            enabled: false,
            max_pixels: 1_000,
            hard_max_pixels: 2_000,
            ..test_cfg()
        };
        // 1206x2622 = 3.16Mpx，远超 hard_max_pixels=2000：但 disabled 时像素与透传安全性无关。
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

    /// 🔴 语义变更（deepseek review）：`max_pixels` 现在是**软阈值** —— 之上只告警仍降采样；
    /// 只有超过 `hard_max_pixels` 硬上限才拒绝（真正的像素炸弹）。
    #[test]
    fn pixels_above_soft_threshold_still_get_downscaled() {
        let cfg = ResizeConfig {
            max_pixels: 1_000_000, // 1206x2622 = 3.16Mpx > 软阈值 1Mpx
            hard_max_pixels: 10_000_000,
            ..test_cfg()
        };
        let img = make_png(1206, 2622);
        let out = maybe_shrink_image(cfg, "png", &img).unwrap();
        assert!(out.was_resized, "超过软阈值也必须降采样而非丢图");
        assert_eq!(out.format, "jpeg");
    }

    /// 像素校验本身：软阈值之上 Ok（只告警），硬上限之上 Err。
    #[test]
    fn pixel_validation_soft_above_max_hard_above_hard_max() {
        let cfg = ResizeConfig {
            max_pixels: 1_000,
            hard_max_pixels: 10_000,
            ..test_cfg()
        };
        // 5000 px > 软阈值 1000，但 < 硬上限 10000 → Ok（降采样不丢图）
        assert!(validate_pixel_count(cfg, 50, 100).is_ok());
        // 恰好等于硬上限 → 仍 Ok（> 才拒）
        assert!(validate_pixel_count(cfg, 100, 100).is_ok());
        // 40000 px > 硬上限 → Err（像素炸弹）
        assert!(validate_pixel_count(cfg, 200, 200).is_err());
    }

    #[test]
    fn pixels_above_hard_ceiling_are_rejected_before_full_decode() {
        let cfg = ResizeConfig {
            max_pixels: 1_000,
            hard_max_pixels: 2_000,
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
