//! AWS Event Stream 消息帧解析
//!
//! ## 消息格式
//!
//! ```text
//! ┌──────────────┬──────────────┬──────────────┬──────────┬──────────┬───────────┐
//! │ Total Length │ Header Length│ Prelude CRC  │ Headers  │ Payload  │ Msg CRC   │
//! │   (4 bytes)  │   (4 bytes)  │   (4 bytes)  │ (变长)    │ (变长)    │ (4 bytes) │
//! └──────────────┴──────────────┴──────────────┴──────────┴──────────┴───────────┘
//! ```
//!
//! - Total Length: 整个消息的总长度（包括自身 4 字节）
//! - Header Length: 头部数据的长度
//! - Prelude CRC: 前 8 字节（Total Length + Header Length）的 CRC32 校验
//! - Headers: 头部数据
//! - Payload: 载荷数据（通常是 JSON）
//! - Message CRC: 整个消息（不含 Message CRC 自身）的 CRC32 校验

use super::crc::crc32;
use super::error::{ParseError, ParseResult};
use super::header::{Headers, parse_headers};

/// Prelude 固定大小 (12 字节)
pub const PRELUDE_SIZE: usize = 12;

/// 最小消息大小 (Prelude + Message CRC)
pub const MIN_MESSAGE_SIZE: usize = PRELUDE_SIZE + 4;

/// 最大消息大小限制 (16 MB)
pub const MAX_MESSAGE_SIZE: u32 = 16 * 1024 * 1024;

/// 解析后的消息帧
#[derive(Debug, Clone)]
pub struct Frame {
    /// 消息头部
    pub headers: Headers,
    /// 消息负载
    pub payload: Vec<u8>,
}

impl Frame {
    /// 获取消息类型
    pub fn message_type(&self) -> Option<&str> {
        self.headers.message_type()
    }

    /// 获取事件类型
    pub fn event_type(&self) -> Option<&str> {
        self.headers.event_type()
    }

    /// 将 payload 解析为 JSON
    pub fn payload_as_json<T: serde::de::DeserializeOwned>(&self) -> ParseResult<T> {
        serde_json::from_slice(&self.payload).map_err(ParseError::PayloadDeserialize)
    }

    /// 将 payload 解析为字符串
    pub fn payload_as_str(&self) -> String {
        String::from_utf8_lossy(&self.payload).to_string()
    }

    /// 只解析 payload 的**前 `max_bytes` 字节**（日志采样用）。
    ///
    /// 与 [`Self::payload_as_str`] 的区别是**不按整帧分配**：未知事件类型只取 512 字符样本，
    /// 但旧写法先 `to_string()` 整帧（上限 16MiB）再 `chars().take(512)`，等于每帧未知事件
    /// 都白分配一次整帧 String（2026-10-14 审计指出）。截断点落在 UTF-8 中间字符时
    /// `from_utf8_lossy` 会以 U+FFFD 收尾，对日志样本无害。
    pub fn payload_as_str_truncated(&self, max_bytes: usize) -> String {
        let end = self.payload.len().min(max_bytes);
        String::from_utf8_lossy(&self.payload[..end]).to_string()
    }
}

/// 尝试从缓冲区解析一个完整的帧
///
/// 这是一个无状态的纯函数，每次调用独立解析。
/// 缓冲区管理由上层 `EventStreamDecoder` 负责。
///
/// # Arguments
/// * `buffer` - 输入缓冲区
///
/// # Returns
/// - `Ok(Some((frame, consumed)))` - 成功解析，返回帧和消费的字节数
/// - `Ok(None)` - 数据不足，需要更多数据
/// - `Err(e)` - 解析错误
pub fn parse_frame(buffer: &[u8]) -> ParseResult<Option<(Frame, usize)>> {
    // 检查是否有足够的数据读取 prelude
    if buffer.len() < PRELUDE_SIZE {
        return Ok(None);
    }

    // 读取 prelude
    let total_length = u32::from_be_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]);
    let header_length = u32::from_be_bytes([buffer[4], buffer[5], buffer[6], buffer[7]]);
    let prelude_crc = u32::from_be_bytes([buffer[8], buffer[9], buffer[10], buffer[11]]);

    // ⚠️ Prelude CRC 必须在**任何依赖 total_length 的动作之前**校验（长度范围判断、
    // 「等整帧到齐」的提前返回都依赖它）。
    //
    // 顺序为什么重要：缓冲区失步时 `total_length` 是垃圾值。旧顺序先信这个垃圾长度 ——
    // 若它恰好落在 `MIN..=MAX` 区间，`buffer.len() < total_length` 会让解码器**一直等**，
    // 直到凑满这个根本不存在的长度才回头发现 CRC 不匹配：白白缓冲最多 16MiB，期间 `feed`
    // 还会因缓冲区上限拒绝新 chunk（真实数据被丢）。prelude 只有 12 字节，CRC 随时可校验，
    // 失步帧应当**立刻**失败并走 1 字节重同步。
    let actual_prelude_crc = crc32(&buffer[..8]);
    if actual_prelude_crc != prelude_crc {
        return Err(ParseError::PreludeCrcMismatch {
            expected: prelude_crc,
            actual: actual_prelude_crc,
        });
    }

    // 验证消息长度范围
    if total_length < MIN_MESSAGE_SIZE as u32 {
        return Err(ParseError::MessageTooSmall {
            length: total_length,
            min: MIN_MESSAGE_SIZE as u32,
        });
    }

    if total_length > MAX_MESSAGE_SIZE {
        return Err(ParseError::MessageTooLarge {
            length: total_length,
            max: MAX_MESSAGE_SIZE,
        });
    }

    let total_length = total_length as usize;
    let header_length = header_length as usize;

    // 检查是否有完整的消息
    if buffer.len() < total_length {
        return Ok(None);
    }

    // 读取 Message CRC
    let message_crc = u32::from_be_bytes([
        buffer[total_length - 4],
        buffer[total_length - 3],
        buffer[total_length - 2],
        buffer[total_length - 1],
    ]);

    // 验证 Message CRC (对整个消息不含最后4字节)
    let actual_message_crc = crc32(&buffer[..total_length - 4]);
    if actual_message_crc != message_crc {
        return Err(ParseError::MessageCrcMismatch {
            expected: message_crc,
            actual: actual_message_crc,
        });
    }

    // 解析头部
    let headers_start = PRELUDE_SIZE;
    let headers_end = headers_start + header_length;

    // 验证头部边界
    if headers_end > total_length - 4 {
        return Err(ParseError::HeaderParseFailed(
            "头部长度超出消息边界".to_string(),
        ));
    }

    let headers = parse_headers(&buffer[headers_start..headers_end], header_length)?;

    // 提取 payload (去除最后4字节的 message_crc)
    let payload_start = headers_end;
    let payload_end = total_length - 4;
    let payload = buffer[payload_start..payload_end].to_vec();

    Ok(Some((Frame { headers, payload }, total_length)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frame_insufficient_data() {
        let buffer = [0u8; 10]; // 小于 PRELUDE_SIZE
        assert!(matches!(parse_frame(&buffer), Ok(None)));
    }

    #[test]
    fn test_frame_message_too_small() {
        // 构造一个 total_length = 10 的 prelude (小于最小值)
        let mut buffer = vec![0u8; 16];
        buffer[0..4].copy_from_slice(&10u32.to_be_bytes()); // total_length
        buffer[4..8].copy_from_slice(&0u32.to_be_bytes()); // header_length
        let prelude_crc = crc32(&buffer[0..8]);
        buffer[8..12].copy_from_slice(&prelude_crc.to_be_bytes());

        let result = parse_frame(&buffer);
        assert!(matches!(result, Err(ParseError::MessageTooSmall { .. })));
    }

    /// 审计 2026-10-14：日志采样只解码前缀，不为 512 字符样本分配整帧。
    #[test]
    fn payload_as_str_truncated_stops_at_cap_and_survives_split_utf8() {
        let frame = Frame {
            headers: Headers::new(),
            payload: b"abcdefghij".to_vec(),
        };
        assert_eq!(frame.payload_as_str_truncated(4), "abcd");
        // 上限大于实际长度 = 全量，不 panic。
        assert_eq!(frame.payload_as_str_truncated(999), "abcdefghij");
        // 截断点落在多字节字符中间：lossy 以 U+FFFD 收尾，不 panic、不返回半个字符。
        let frame = Frame {
            headers: Headers::new(),
            payload: "中文".as_bytes().to_vec(),
        };
        let out = frame.payload_as_str_truncated(4);
        assert_eq!(out.chars().count(), 2, "3 字节字符被截在中间应得 U+FFFD: {out:?}");
        assert!(out.starts_with('中'));
        assert!(out.ends_with('\u{FFFD}'));
    }

    /// 构造一个**两个 CRC 都算对**的合法帧。
    fn build_frame(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
        let mut hdr = Vec::new();
        for (name, value) in headers {
            hdr.push(name.len() as u8);
            hdr.extend_from_slice(name.as_bytes());
            hdr.push(7u8); // HeaderValueType::String
            hdr.extend_from_slice(&(value.len() as u16).to_be_bytes());
            hdr.extend_from_slice(value.as_bytes());
        }
        let total_length = (PRELUDE_SIZE + hdr.len() + payload.len() + 4) as u32;
        let mut buf = Vec::new();
        buf.extend_from_slice(&total_length.to_be_bytes());
        buf.extend_from_slice(&(hdr.len() as u32).to_be_bytes());
        let prelude_crc = crc32(&buf[..8]);
        buf.extend_from_slice(&prelude_crc.to_be_bytes());
        buf.extend_from_slice(&hdr);
        buf.extend_from_slice(payload);
        let msg_crc = crc32(&buf);
        buf.extend_from_slice(&msg_crc.to_be_bytes());
        buf
    }

    /// 正常往返：头部、payload、消费字节数都对。
    #[test]
    fn test_frame_round_trip() {
        let buf = build_frame(
            &[(":message-type", "event"), (":event-type", "x")],
            b"{\"a\":1}",
        );
        let (frame, consumed) = parse_frame(&buf).expect("应可解析").expect("应有整帧");
        assert_eq!(consumed, buf.len(), "消费长度应等于整帧长度");
        assert_eq!(frame.message_type(), Some("event"));
        assert_eq!(frame.event_type(), Some("x"));
        assert_eq!(frame.payload, b"{\"a\":1}".to_vec());
    }

    /// Prelude CRC 不匹配：即使 `total_length` 是垃圾值也必须**先**报 CRC 错。
    ///
    /// 锁的是校验顺序（2026-10-14 审计）：旧顺序先信垃圾长度并「等整帧到齐」，失步帧会让
    /// 解码器白白缓冲最多 16MiB 才回头发现 CRC 不匹配（期间 `feed` 还会因缓冲区上限丢数据）。
    /// 回退即 FAIL：把 CRC 校验挪回长度判断之后，本用例会得到 `Ok(None)`。
    #[test]
    fn test_frame_prelude_crc_mismatch_detected_before_length_wait() {
        let mut buf = vec![0u8; PRELUDE_SIZE];
        buf[0..4].copy_from_slice(&1024u32.to_be_bytes()); // 远大于当前缓冲区
        buf[4..8].copy_from_slice(&0u32.to_be_bytes());
        // 保证与真实 CRC 不同（不用硬编码值，免得撞上真值让测试变脆）。
        let bad_crc = crc32(&buf[..8]).wrapping_add(1);
        buf[8..12].copy_from_slice(&bad_crc.to_be_bytes());
        assert!(matches!(
            parse_frame(&buf),
            Err(ParseError::PreludeCrcMismatch { .. })
        ));
    }

    /// Message CRC 不匹配（prelude 合法）。
    #[test]
    fn test_frame_message_crc_mismatch() {
        let mut buf = build_frame(&[(":message-type", "event")], b"body");
        let last = buf.len() - 1;
        buf[last] ^= 0xFF;
        assert!(matches!(
            parse_frame(&buf),
            Err(ParseError::MessageCrcMismatch { .. })
        ));
    }

    /// 长度超上限 / 头部越界都必须报类型化错误，不得 panic。
    #[test]
    fn test_frame_message_too_large_and_header_overflow() {
        // total_length 超过 16MiB 上限（prelude CRC 合法，才能走到长度判断）。
        let mut buf = vec![0u8; PRELUDE_SIZE];
        buf[0..4].copy_from_slice(&(MAX_MESSAGE_SIZE + 1).to_be_bytes());
        buf[4..8].copy_from_slice(&0u32.to_be_bytes());
        let crc = crc32(&buf[..8]);
        buf[8..12].copy_from_slice(&crc.to_be_bytes());
        assert!(matches!(
            parse_frame(&buf),
            Err(ParseError::MessageTooLarge { .. })
        ));

        // header_length 越过消息边界 → HeaderParseFailed（而不是切片 panic）。
        let payload = b"x";
        let total = (PRELUDE_SIZE + payload.len() + 4) as u32;
        let mut buf = Vec::new();
        buf.extend_from_slice(&total.to_be_bytes());
        buf.extend_from_slice(&(total + 100).to_be_bytes()); // 头部长度越界
        let crc = crc32(&buf[..8]);
        buf.extend_from_slice(&crc.to_be_bytes());
        buf.extend_from_slice(payload);
        let msg_crc = crc32(&buf);
        buf.extend_from_slice(&msg_crc.to_be_bytes());
        assert!(matches!(
            parse_frame(&buf),
            Err(ParseError::HeaderParseFailed(_))
        ));
    }
}
