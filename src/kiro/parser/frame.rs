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

/// 协议判别时截取的响应头部字符数（仅用于日志/错误信息，避免打印整个 body）
const PROTOCOL_SNIFF_HEAD_CHARS: usize = 96;

/// 判断缓冲区开头是否**不可能**是 AWS Event Stream 帧，即上游把协议降级成了
/// JSON / XML / 纯文本。
///
/// # 为何这是充分判据而非启发式
///
/// 合法帧的前 4 字节是大端 `total_length`，而 [`MAX_MESSAGE_SIZE`] = 16 MiB =
/// `0x0100_0000`。因此**任何**合法帧的首字节只能是 `0x00`（长度 < 16 MiB）或
/// `0x01`（恰好 16 MiB）。而 JSON/XML/文本响应的首字节是 `{`(0x7b) / `[`(0x5b) /
/// `<`(0x3c) / 空白 —— 全部 ≥ 0x09，与合法帧的取值域**完全不相交**。
///
/// 所以「首字节 ≥ 0x09」⇒ 必然不是 event-stream，不存在误判合法帧的可能。
///
/// # 为何必须单独识别
///
/// 不识别时，`{"Ou` 会被当成 `total_length = 2065846133`（约 19 亿）触发
/// [`ParseError::MessageTooLarge`]，解码器按「帧边界错位」逐字节跳过，5 次后
/// 撞上 `max_errors` 永久停止 —— 报出"消息长度 19 亿字节"这种与真实原因毫无
/// 关系的误导性错误（生产实证：2026-08-04，845 次）。
pub fn sniff_non_event_stream(buffer: &[u8]) -> Option<String> {
    let first = *buffer.first()?;
    // 合法帧首字节只能是 0x00 / 0x01；文本协议首字节必然 >= 0x09（\t）。
    if first < 0x09 {
        return None;
    }
    let head: String = String::from_utf8_lossy(buffer)
        .chars()
        .take(PROTOCOL_SNIFF_HEAD_CHARS)
        .collect();
    Some(head.trim().to_string())
}

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
    // 协议判别放在「数据不足」判断**之前**：判据只需 1 字节即可定论，而短小的文本
    // 响应体（如 `no`、`{}`）不足 12 字节。若先返回 Ok(None) 等更多数据，上游已经
    // 把整个 body 发完并关闭连接，解码器会永远停在"等数据"而非如实报出协议不符。
    if let Some(head) = sniff_non_event_stream(buffer) {
        return Err(ParseError::NotEventStream { head });
    }

    // 检查是否有足够的数据读取 prelude
    if buffer.len() < PRELUDE_SIZE {
        return Ok(None);
    }

    // 读取 prelude
    let total_length = u32::from_be_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]);
    let header_length = u32::from_be_bytes([buffer[4], buffer[5], buffer[6], buffer[7]]);
    let prelude_crc = u32::from_be_bytes([buffer[8], buffer[9], buffer[10], buffer[11]]);

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

    // 验证 Prelude CRC
    let actual_prelude_crc = crc32(&buffer[..8]);
    if actual_prelude_crc != prelude_crc {
        return Err(ParseError::PreludeCrcMismatch {
            expected: prelude_crc,
            actual: actual_prelude_crc,
        });
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

    /// 回归（生产事故 2026-08-04）：上游返回 AWS JSON 1.0 信封而非 event-stream，
    /// 旧实现把 `{"Ou` 当 4 字节大端长度读出 2065846133，逐字节啃 5 次后停止，
    /// 报出"消息长度 19 亿字节"——把协议不符伪装成长度异常，根因完全被埋掉。
    ///
    /// 这里用**生产日志中的真实字节**断言现在被识别为 NotEventStream。
    #[test]
    fn test_json_envelope_detected_as_not_event_stream() {
        let body = br#"{"Output":{"__type":"com.amazon.coral.service#InternalServerException"},"Version":"1.0"}"#;
        match parse_frame(body) {
            Err(ParseError::NotEventStream { head }) => {
                assert!(
                    head.starts_with('{'),
                    "错误里应带上响应体开头供诊断，实际: {head}"
                );
            }
            other => panic!("JSON 信封应判定为 NotEventStream，实际: {other:?}"),
        }
    }

    /// 生产日志里出现过的五个"长度"数字全部源自 ASCII 文本，逐一验证判据能拦住它们。
    #[test]
    fn test_all_production_misleading_lengths_now_rejected() {
        // 这些 u32 大端展开就是 `{"Ou` / `"Out` / `Outp` / `utpu` / `tput`
        for n in [2065846133u32, 575632756, 1333097584, 1970565237, 1953527156] {
            let mut buf = n.to_be_bytes().to_vec();
            buf.extend_from_slice(&[0u8; 12]); // 补足 PRELUDE_SIZE
            match parse_frame(&buf) {
                Err(ParseError::NotEventStream { .. }) => {}
                other => panic!(
                    "长度 {n} (ascii {:?}) 应被判为协议不符，实际: {other:?}",
                    String::from_utf8_lossy(&n.to_be_bytes())
                ),
            }
        }
    }

    /// 边界：判据基于 `total_length <= 16MB` ⇒ 首字节只能是 0x00/0x01。
    /// 0x01 必须**放行**（合法的大帧），不能被误判成协议不符。
    #[test]
    fn test_legal_large_frame_first_byte_not_rejected() {
        // total_length = 0x01000000 = 16MB（恰好等于上限，合法）
        let mut buf = 0x01000000u32.to_be_bytes().to_vec();
        buf.extend_from_slice(&[0u8; 12]);
        assert!(
            !matches!(parse_frame(&buf), Err(ParseError::NotEventStream { .. })),
            "首字节 0x01 是合法帧长度的一部分，绝不能判为协议不符"
        );
        // 首字节 0x00 同理（绝大多数正常帧）
        let mut buf = 4096u32.to_be_bytes().to_vec();
        buf.extend_from_slice(&[0u8; 12]);
        assert!(!matches!(
            parse_frame(&buf),
            Err(ParseError::NotEventStream { .. })
        ));
    }

    /// 短于 PRELUDE_SIZE 的文本响应体也必须立即判定，不能停在"等更多数据"。
    /// 上游发完短 body 就关连接，等下去等不到任何字节。
    #[test]
    fn test_short_text_body_detected_without_full_prelude() {
        for body in [&b"no"[..], &b"{}"[..], &b"{"[..], &b"Forbidden"[..]] {
            assert!(body.len() < PRELUDE_SIZE, "本用例前提是 body 短于 prelude");
            match parse_frame(body) {
                Err(ParseError::NotEventStream { .. }) => {}
                other => panic!(
                    "短文本 body {:?} 应立即判为协议不符而非 Ok(None)，实际: {other:?}",
                    String::from_utf8_lossy(body)
                ),
            }
        }
    }

    /// 对照：真正的「数据不足」（首字节合法）仍须返回 Ok(None) 等待更多数据，
    /// 不能被协议判别误吞。
    #[test]
    fn test_genuine_partial_frame_still_waits() {
        let partial = &4096u32.to_be_bytes()[..3]; // 只到了 3 字节
        assert!(matches!(parse_frame(partial), Ok(None)));
    }

    /// SSE / 纯文本错误页同样应被识别（兼容性：不止 JSON 一种形态）。
    #[test]
    fn test_other_text_bodies_detected() {
        for body in [
            &b"event: message\ndata: {}\n\n"[..],
            &b"<html><body>502 Bad Gateway</body></html>"[..],
            &b"[{\"error\":\"x\"}]"[..],
            &b"Internal Server Error"[..],
        ] {
            match parse_frame(body) {
                Err(ParseError::NotEventStream { .. }) => {}
                other => panic!("文本响应体应判为协议不符: {other:?}"),
            }
        }
    }

    /// 根因实证回归（2026-08-04，真实上游抓包）。
    ///
    /// 下面这段 body 是**真实上游返回的原文**，抓取条件 = 生产代码路径：
    /// `provider.rs` 先设 `content-type: application/json`，`cli.rs` 再 append
    /// `application/x-amz-json-1.0`（reqwest 的 `.header()` 是 append 而非 insert，
    /// 故一个请求带两个值）。服务端取**第一个**值 `application/json`，Coral 框架
    /// 不认该操作，于是回 `UnknownOperationException` —— 关键在于它用 **HTTP 200**
    /// 返回，包在 `{"Output":..,"Version":"1.0"}` 信封里。
    ///
    /// 于是旧实现：200 ⇒ `report_success` ⇒ 健康分只升不降 ⇒ 把 JSON 喂进二进制
    /// 解码器 ⇒ 前 4 字节 `{"Ou` 被当成大端长度 = 2065846133（约 19 亿）⇒ 逐字节
    /// 啃 5 次 ⇒ 解码器永久停止 ⇒ 客户端收到 502，而根因被彻底埋掉。
    ///
    /// 对照实证：同一请求只发一个正确的 content-type ⇒ HTTP 400 + 正常的
    /// `ValidationException`；`{"Ou` 这个特征串**只在双 content-type 时出现**。
    #[test]
    fn test_real_captured_upstream_200_envelope_is_detected() {
        let captured = br#"{"Output":{"__type":"com.amazon.coral.service#UnknownOperationException","message":"The requested operation is not recognized by the service."},"Version":"1.0"}"#;

        // 前 4 字节正是生产日志里那个"19 亿字节"的来源。
        assert_eq!(&captured[..4], b"{\"Ou");
        assert_eq!(
            u32::from_be_bytes([captured[0], captured[1], captured[2], captured[3]]),
            2065846133,
            "这个数字必须与生产日志逐位一致，否则回归样本已失真"
        );

        match parse_frame(captured) {
            Err(ParseError::NotEventStream { head }) => {
                assert!(
                    head.contains("Output"),
                    "错误信息应带上响应头部，便于运维一眼看出上游说了什么: {head}"
                );
            }
            other => panic!(
                "真实抓包的 200 + Coral 信封必须判为协议不符（而非 19 亿字节长度错误）: {other:?}"
            ),
        }
    }
}
