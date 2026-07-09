//! AWS Event Stream 流式解码器
//!
//! 使用状态机处理流式数据，支持断点续传和容错处理
//!
//! ## 状态机设计
//!
//! 参考 kiro-kt 项目的状态机设计，采用四态模型：
//!
//! ```text
//! ┌─────────────────┐
//! │      Ready      │  (初始态，就绪接收数据)
//! └────────┬────────┘
//!          │ feed() 提供数据
//!          ↓
//! ┌─────────────────┐
//! │     Parsing     │  decode() 尝试解析
//! └────────┬────────┘
//!          │
//!     ┌────┴────────────┐
//!     ↓                 ↓
//!  [成功]            [失败]
//!     │                 │
//!     ↓                 ├─> error_count++
//! ┌─────────┐           │
//! │  Ready  │           ├─> error_count < max_errors?
//! └─────────┘           │    YES → Recovering → Ready
//!                       │    NO  ↓
//!                  ┌────────────┐
//!                  │   Stopped  │ (终止态)
//!                  └────────────┘
//! ```

use super::error::{ParseError, ParseResult};
use super::frame::{Frame, PRELUDE_SIZE, parse_frame};
use bytes::{Buf, BytesMut};

/// 默认最大缓冲区大小 (16 MB)
pub const DEFAULT_MAX_BUFFER_SIZE: usize = 16 * 1024 * 1024;

/// 默认最大连续错误数
pub const DEFAULT_MAX_ERRORS: usize = 5;

/// 默认初始缓冲区容量
pub const DEFAULT_BUFFER_CAPACITY: usize = 8192;

/// 解码器状态
///
/// 采用四态模型，参考 kiro-kt 的设计：
/// - Ready: 就绪状态，可以接收数据
/// - Parsing: 正在解析帧
/// - Recovering: 恢复中（尝试跳过损坏数据）
/// - Stopped: 已停止（错误过多，终止态）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecoderState {
    /// 就绪，可以接收数据
    Ready,
    /// 正在解析帧
    Parsing,
    /// 恢复中（跳过损坏数据）
    Recovering,
    /// 已停止（错误过多）
    Stopped,
}

/// 流式事件解码器
///
/// 用于从字节流中解析 AWS Event Stream 消息帧
///
/// # Example
///
/// ```rust,ignore
/// use kirostudio::kiro::parser::EventStreamDecoder;
///
/// let mut decoder = EventStreamDecoder::new();
///
/// // 提供流数据
/// decoder.feed(chunk)?;
///
/// // 解码所有可用帧
/// for result in decoder.decode_iter() {
///     match result {
///         Ok(frame) => println!("Got frame: {:?}", frame.event_type()),
///         Err(e) => eprintln!("Parse error: {}", e),
///     }
/// }
/// ```
pub struct EventStreamDecoder {
    /// 内部缓冲区
    buffer: BytesMut,
    /// 当前状态
    state: DecoderState,
    /// 已处理的帧数量
    frames_decoded: usize,
    /// 连续错误计数
    error_count: usize,
    /// 最大连续错误数
    max_errors: usize,
    /// 最大缓冲区大小
    max_buffer_size: usize,
    /// 跳过的字节数（用于调试）
    bytes_skipped: usize,
}

impl Default for EventStreamDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl EventStreamDecoder {
    /// 创建新的解码器
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_BUFFER_CAPACITY)
    }

    /// 创建具有指定缓冲区大小的解码器
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buffer: BytesMut::with_capacity(capacity),
            state: DecoderState::Ready,
            frames_decoded: 0,
            error_count: 0,
            max_errors: DEFAULT_MAX_ERRORS,
            max_buffer_size: DEFAULT_MAX_BUFFER_SIZE,
            bytes_skipped: 0,
        }
    }

    /// 向解码器提供数据
    ///
    /// # Returns
    /// - `Ok(())` - 数据已添加到缓冲区
    /// - `Err(BufferOverflow)` - 缓冲区已满
    pub fn feed(&mut self, data: &[u8]) -> ParseResult<()> {
        // 检查缓冲区大小限制
        let new_size = self.buffer.len() + data.len();
        if new_size > self.max_buffer_size {
            return Err(ParseError::BufferOverflow {
                size: new_size,
                max: self.max_buffer_size,
            });
        }

        self.buffer.extend_from_slice(data);

        // 从 Recovering 状态恢复到 Ready
        if self.state == DecoderState::Recovering {
            self.state = DecoderState::Ready;
        }

        Ok(())
    }

    /// 尝试解码下一个帧
    ///
    /// # Returns
    /// - `Ok(Some(frame))` - 成功解码一个帧
    /// - `Ok(None)` - 数据不足，需要更多数据
    /// - `Err(e)` - 解码错误
    pub fn decode(&mut self) -> ParseResult<Option<Frame>> {
        // 如果已停止，直接返回错误
        if self.state == DecoderState::Stopped {
            return Err(ParseError::TooManyErrors {
                count: self.error_count,
                last_error: "解码器已停止".to_string(),
            });
        }

        // 缓冲区为空，保持 Ready 状态
        if self.buffer.is_empty() {
            self.state = DecoderState::Ready;
            return Ok(None);
        }

        // 转移到 Parsing 状态
        self.state = DecoderState::Parsing;

        match parse_frame(&self.buffer) {
            Ok(Some((frame, consumed))) => {
                // 成功解析
                self.buffer.advance(consumed);
                self.state = DecoderState::Ready;
                self.frames_decoded += 1;
                self.error_count = 0; // 重置连续错误计数
                Ok(Some(frame))
            }
            Ok(None) => {
                // 数据不足，回到 Ready 状态等待更多数据
                self.state = DecoderState::Ready;
                Ok(None)
            }
            Err(e) => {
                self.error_count += 1;
                let error_msg = e.to_string();

                // 检查是否超过最大错误数
                if self.error_count >= self.max_errors {
                    self.state = DecoderState::Stopped;
                    tracing::error!(
                        "解码器停止: 连续 {} 次错误，最后错误: {}",
                        self.error_count,
                        error_msg
                    );
                    return Err(ParseError::TooManyErrors {
                        count: self.error_count,
                        last_error: error_msg,
                    });
                }

                // 根据错误类型采用不同的恢复策略
                self.try_recover(&e);
                self.state = DecoderState::Recovering;
                Err(e)
            }
        }
    }

    /// 创建解码迭代器
    pub fn decode_iter(&mut self) -> DecodeIter<'_> {
        DecodeIter { decoder: self }
    }

    /// 解码器是否已进入终止态（连续错误超限而永久停止）
    ///
    /// 终止态是不可逆的：一旦 `Stopped`，`decode()` 只会返回 `TooManyErrors`，
    /// `feed()` 也不再复位它（只复位可恢复的 `Recovering`）。迭代器据此判定是否
    /// 应停止 drain。注意：`Recovering` 只是**可恢复中间态**，不是终止态。
    pub fn is_stopped(&self) -> bool {
        self.state == DecoderState::Stopped
    }

    /// 尝试容错恢复
    ///
    /// 根据错误类型采用不同的恢复策略（参考 kiro-kt 的设计）：
    /// - Prelude 阶段错误（CRC 失败、长度异常）：跳过 1 字节，尝试找下一帧边界
    /// - Data 阶段错误（Message CRC 失败、Header 解析失败）：跳过整个损坏帧
    fn try_recover(&mut self, error: &ParseError) {
        if self.buffer.is_empty() {
            return;
        }

        match error {
            // Prelude 阶段错误：可能是帧边界错位，逐字节扫描找下一个有效边界
            ParseError::PreludeCrcMismatch { .. }
            | ParseError::MessageTooSmall { .. }
            | ParseError::MessageTooLarge { .. } => {
                let skipped_byte = self.buffer[0];
                self.buffer.advance(1);
                self.bytes_skipped += 1;
                tracing::warn!(
                    "Prelude 错误恢复: 跳过字节 0x{:02x} (累计跳过 {} 字节)",
                    skipped_byte,
                    self.bytes_skipped
                );
            }

            // Data 阶段错误：帧边界正确但数据损坏，跳过整个帧
            ParseError::MessageCrcMismatch { .. } | ParseError::HeaderParseFailed(_) => {
                // 尝试读取 total_length 来跳过整帧
                if self.buffer.len() >= PRELUDE_SIZE {
                    let total_length = u32::from_be_bytes([
                        self.buffer[0],
                        self.buffer[1],
                        self.buffer[2],
                        self.buffer[3],
                    ]) as usize;

                    // 确保 total_length 合理且缓冲区有足够数据
                    if total_length >= 16 && total_length <= self.buffer.len() {
                        tracing::warn!("Data 错误恢复: 跳过损坏帧 ({} 字节)", total_length);
                        self.buffer.advance(total_length);
                        self.bytes_skipped += total_length;
                        return;
                    }
                }

                // 无法确定帧长度，回退到逐字节跳过
                let skipped_byte = self.buffer[0];
                self.buffer.advance(1);
                self.bytes_skipped += 1;
                tracing::warn!(
                    "Data 错误恢复 (回退): 跳过字节 0x{:02x} (累计跳过 {} 字节)",
                    skipped_byte,
                    self.bytes_skipped
                );
            }

            // 其他错误：逐字节跳过
            _ => {
                let skipped_byte = self.buffer[0];
                self.buffer.advance(1);
                self.bytes_skipped += 1;
                tracing::warn!(
                    "通用错误恢复: 跳过字节 0x{:02x} (累计跳过 {} 字节)",
                    skipped_byte,
                    self.bytes_skipped
                );
            }
        }
    }

}

/// 解码迭代器
pub struct DecodeIter<'a> {
    decoder: &'a mut EventStreamDecoder,
}

impl<'a> Iterator for DecodeIter<'a> {
    type Item = ParseResult<Frame>;

    fn next(&mut self) -> Option<Self::Item> {
        // 只有终止态 Stopped 才停止迭代。
        //
        // 历史 BUG：这里曾把 `Recovering` 也当作终止条件直接 `return None`，
        // 导致单次 feed 中遇到**可恢复**错误（如一帧 CRC 损坏）后迭代器立即结束，
        // 缓冲区里后续所有有效帧被整体丢弃——但外层仍按 200/正常收尾返回，
        // 造成“截断输出被当成功”。实际上 `try_recover()` 在报错前已推进缓冲区跳过
        // 损坏字节，`Recovering` 只是可恢复中间态：应继续 drain，让后续有效帧被解析出来。
        //
        // 终止性由 `decode()` 自身保证：连续错误累计到 `max_errors`(5) 即转入
        // `Stopped` 并返回 `TooManyErrors`；每次 `try_recover()` 至少推进 1 字节，
        // 缓冲区耗尽时 `decode()` 先行返回 `Ok(None)`，故不会死循环。
        if self.decoder.is_stopped() {
            return None;
        }

        match self.decoder.decode() {
            Ok(Some(frame)) => Some(Ok(frame)),
            // 数据不足/缓冲区耗尽：本轮 drain 到此为止（等待下次 feed）
            Ok(None) => None,
            // 可恢复错误：如实抛出本帧错误，但**不终止迭代**——
            // 下次 next() 会在已跳过损坏字节的缓冲区上继续解析后续帧。
            Err(e) => Some(Err(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decoder_feed() {
        let mut decoder = EventStreamDecoder::new();
        assert!(decoder.feed(&[1, 2, 3, 4]).is_ok());
    }

    #[test]
    fn test_decoder_insufficient_data() {
        let mut decoder = EventStreamDecoder::new();
        decoder.feed(&[0u8; 10]).unwrap();

        let result = decoder.decode();
        assert!(matches!(result, Ok(None)));
    }

    /// 构造一个最小的合法帧（空头部 + 空 payload），仅用于测试 drain 行为，
    /// 不关心帧语义（解析出的 Event 是 Unknown）。
    ///
    /// 布局：total_length(4) + header_length(4) + prelude_crc(4) + payload + msg_crc(4)。
    fn build_minimal_frame() -> Vec<u8> {
        use super::super::crc::crc32;
        let payload: &[u8] = b"";
        let header_length: u32 = 0;
        let total_length: u32 = (PRELUDE_SIZE + payload.len() + 4) as u32;

        let mut buf = Vec::new();
        buf.extend_from_slice(&total_length.to_be_bytes());
        buf.extend_from_slice(&header_length.to_be_bytes());
        let prelude_crc = crc32(&buf[..8]);
        buf.extend_from_slice(&prelude_crc.to_be_bytes());
        buf.extend_from_slice(payload);
        let msg_crc = crc32(&buf);
        buf.extend_from_slice(&msg_crc.to_be_bytes());
        buf
    }

    #[test]
    fn test_decode_iter_drains_multiple_valid_frames() {
        // 单次 feed 里塞入 3 个合法帧，迭代器应完整 drain 出 3 个
        let mut decoder = EventStreamDecoder::new();
        let mut data = Vec::new();
        for _ in 0..3 {
            data.extend_from_slice(&build_minimal_frame());
        }
        decoder.feed(&data).unwrap();

        let frames: Vec<_> = decoder.decode_iter().filter_map(|r| r.ok()).collect();
        assert_eq!(frames.len(), 3, "应 drain 出全部 3 个合法帧");
        assert!(!decoder.is_stopped());
    }

    /// 构造一个 prelude 合法但 Message CRC 损坏的帧。
    ///
    /// 触发 `MessageCrcMismatch` → try_recover 的 Data 分支「按 total_length 跳过整帧」，
    /// 单帧一次错误即可干净跳过，用于验证损坏帧之后的合法帧仍能被 drain 出来。
    fn build_corrupt_crc_frame() -> Vec<u8> {
        let mut frame = build_minimal_frame();
        // 翻转最后一个字节，破坏 Message CRC（prelude 仍合法，故能读出 total_length 跳整帧）
        let last = frame.len() - 1;
        frame[last] ^= 0xFF;
        frame
    }

    #[test]
    fn test_decode_iter_recovers_and_drains_after_corrupt_frame() {
        // 回归 BUG②：损坏帧夹在两个合法帧之间。
        // 单次 feed 中遇到可恢复错误后不应永久终止，应继续 drain 出损坏帧之后的合法帧。
        let mut decoder = EventStreamDecoder::new();

        let mut data = Vec::new();
        data.extend_from_slice(&build_minimal_frame());
        data.extend_from_slice(&build_corrupt_crc_frame());
        data.extend_from_slice(&build_minimal_frame());

        decoder.feed(&data).unwrap();

        let mut ok_frames = 0usize;
        let mut err_frames = 0usize;
        for result in decoder.decode_iter() {
            match result {
                Ok(_) => ok_frames += 1,
                Err(_) => err_frames += 1,
            }
        }

        assert_eq!(
            ok_frames, 2,
            "损坏帧前后的两个合法帧都应被解析出来（旧实现会丢掉损坏帧之后的帧）"
        );
        assert_eq!(err_frames, 1, "中间损坏帧应恰好抛出一次错误");
        assert!(
            !decoder.is_stopped(),
            "单个可恢复错误不应把解码器打进终止态"
        );
    }

    #[test]
    fn test_decode_iter_stops_after_too_many_errors() {
        // 连续 max_errors 次不可恢复错误后进入终止态，迭代器停止 drain
        let mut decoder = EventStreamDecoder::new();
        // 全 0 字节：total_length=0 触发 MessageTooSmall，逐字节跳过，连续错误累积
        decoder.feed(&[0u8; 64]).unwrap();

        let mut err_count = 0usize;
        for result in decoder.decode_iter() {
            if result.is_err() {
                err_count += 1;
            }
        }

        assert!(err_count >= 1, "应至少抛出一次错误");
        assert!(
            decoder.is_stopped(),
            "连续错误超过 max_errors 后应进入终止态"
        );
        // 终止态后再次迭代应立即返回 None（不再产出）
        assert!(decoder.decode_iter().next().is_none());
    }
}
