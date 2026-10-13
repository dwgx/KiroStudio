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
use super::frame::{Frame, MAX_MESSAGE_SIZE, PRELUDE_SIZE, parse_frame};
use bytes::{Buf, BytesMut};

/// 默认最大缓冲区大小。
///
/// = 单帧上限（`MAX_MESSAGE_SIZE`，16MiB）+ 一个 chunk 的余量 `BUFFER_CHUNK_SLACK`。
///
/// 为什么要余量：缓冲区在「整帧已到齐」之前一直持有**未完成**的帧，而 chunk 是整块进出的。
/// 若上限与单帧上限相等，一帧长度逼近 16MiB 时，最后一个 chunk 会让
/// `buffer.len() + data.len() > 上限` 成立 → **整个 chunk 被拒绝**（调用方只记一条 warn 就
/// 继续 drain）→ 该帧永远凑不齐 → 流卡死。留出一个 chunk 的余量后，`total_length <= 16MiB`
/// 的帧只要 chunk 不超过该余量就一定能收全（2026-10-14 审计条目）。
pub const DEFAULT_MAX_BUFFER_SIZE: usize = MAX_MESSAGE_SIZE as usize + BUFFER_CHUNK_SLACK;

/// 缓冲区上限里留给「最后一个 chunk」的余量。
///
/// reqwest/hyper 的 chunk 通常 ≤ 64KiB，故取 64KiB：既够覆盖正常分块，又不放大内存占用
/// （缓冲区只在真有未完成帧时才长到这个量级）。
const BUFFER_CHUNK_SLACK: usize = 64 * 1024;

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
    /// - `Ok(())` - 数据已添加到缓冲区（**或解码器已处于终止态、数据被丢弃**）
    /// - `Err(BufferOverflow)` - 缓冲区已满
    ///
    /// ⚠️ 边界：上限 = 单帧上限 + `BUFFER_CHUNK_SLACK`（见 `DEFAULT_MAX_BUFFER_SIZE`），
    /// 故「长度 ≤ 16MiB 的帧 + 一个 ≤64KiB 的 chunk」一定收得下。但比该余量更大的单个 chunk
    /// 仍会被**整体拒绝**（调用方 `decode_frames_into` 只记一条 warn 就继续 drain）——
    /// 那意味着丢字节、这一帧再也凑不齐。要改得先定策略（超限即判流截断，还是按需扩容）。
    pub fn feed(&mut self, data: &[u8]) -> ParseResult<()> {
        // 终止态：解码器再也不会产出任何帧（`decode()` 只返回 `TooManyErrors`，迭代器直接
        // 结束），继续缓存只会有害 —— 缓冲区会一路涨到 `max_buffer_size`(≈16MiB)，之后每次
        // `feed` 都返回 `BufferOverflow`（调用方只记一条 warn），即「16MiB 死缓冲区 +
        // 每 chunk 一条噪音日志」。数据直接丢弃：它本来就不可能被解析出来。
        if self.state == DecoderState::Stopped {
            return Ok(());
        }

        // 检查缓冲区大小限制。用 checked_add：`len + len` 理论可回绕（32 位 usize + 巨块），
        // 回绕后会绕过上限判断直接进 `extend_from_slice`。回绕即视为溢出。
        let new_size = self
            .buffer
            .len()
            .checked_add(data.len())
            .unwrap_or(usize::MAX);
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
                    // 终止态不可逆，缓冲区里剩下的字节永远不会再被解析 —— 就地释放
                    // （最多 `max_buffer_size`）。不清的话调用方继续 feed 只会把它撑到上限后开始报
                    // BufferOverflow（见 `feed` 的终止态分支）。
                    self.buffer.clear();
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
            //
            // `MessageTooLarge` 也归这一类：长度前缀只由 prelude CRC 背书，而「>16MiB 的巨帧」
            // 在本上游从未出现过，超限更可能是失步（垃圾字节凑巧 CRC 对上）⇒ 逐字节重同步的
            // 收益大于代价。已知取舍：若真是一帧超限数据，5 次连错预算会耗尽并转入 Stopped
            // （后续帧全丢）—— 需要时按此验：喂一帧 total_length > MAX_MESSAGE_SIZE、prelude
            // CRC 正确的数据，再喂一帧合法数据，观察第二帧是否被解析出来。
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

            // Data 阶段错误：帧边界可信 ⇒ 跳过整帧。
            //
            // 为什么可信：这些错误只在 **prelude CRC 已通过**之后才可能返回，而 prelude CRC
            // 正好覆盖 `total_length`（前 8 字节）—— 长度前缀是被校验过的，不是待验证的原始值。
            // （`MessageCrcMismatch` 时 message CRC 自然是失败的，但它管的是帧内数据，不管边界。）
            //
            // 为什么 `InvalidHeaderType` / `Incomplete` 也走这条：它们只可能由 `parse_headers`
            // 返回，而 `parse_frame` 走到那里时 prelude CRC 与 message CRC **都已通过**
            // （长度与「整帧已在缓冲区」也校验过）。这里的 `Incomplete` **不表示「等更多数据」**：
            // 整帧已在缓冲区里，它只表示头部编码与 `header_length` 自相矛盾。
            //
            // 逐字节重同步在这里只有坏处：5 次连错预算会被一帧内部的垃圾耗尽（到顶即永久
            // `Stopped`，后续所有合法帧被丢）。
            ParseError::MessageCrcMismatch { .. }
            | ParseError::HeaderParseFailed(_)
            | ParseError::InvalidHeaderType(_)
            | ParseError::Incomplete { .. } => {
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

    /// 确定性 xorshift64*（不引入外部依赖），保证测试可复现。
    fn lcg_seeded(seed: u64) -> impl FnMut() -> u64 {
        let mut s = seed;
        move || {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            s.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
    }

    /// 审计：随机畸形输入不得 panic（`try_recover` / `parse_frame` / `parse_headers`
    /// 全部走边界检查，任何输入只能产出 Ok/Err，且 drain 必须在有界步数内终止）。
    #[test]
    fn fuzz_random_bytes_never_panics() {
        let mut rng = lcg_seeded(0x1234_5678_9abc_def0);
        for _ in 0..2000 {
            let len = (rng() % 300) as usize;
            let buf: Vec<u8> = (0..len).map(|_| (rng() & 0xff) as u8).collect();

            let mut decoder = EventStreamDecoder::new();
            decoder.feed(&buf).unwrap();
            let mut steps = 0usize;
            for result in decoder.decode_iter() {
                let _ = result;
                steps += 1;
                assert!(steps < 1000, "drain 必须有界（每次错误至少推进 1 字节）");
            }
        }
    }

    /// 审计：prelude 合法（CRC 通过）但头部/payload/Message CRC 随机构造的帧不得 panic，
    /// 必须覆盖 `MessageCrcMismatch` / `HeaderParseFailed` 两条恢复路径。
    #[test]
    fn fuzz_valid_prelude_malformed_body_never_panics() {
        use super::super::crc::crc32;
        let mut rng = lcg_seeded(0xdead_beef_cafe_f00d);
        for _ in 0..2000 {
            let payload_len = (rng() % 400) as u32;
            let header_len = (rng() % 64) as u32;
            // total >= PRELUDE_SIZE(12) + 0 + 0 + 4 = 16 = MIN_MESSAGE_SIZE，天然合法
            let total = PRELUDE_SIZE as u32 + header_len + payload_len + 4;

            let mut buf = Vec::new();
            buf.extend_from_slice(&total.to_be_bytes());
            buf.extend_from_slice(&header_len.to_be_bytes());
            let prelude_crc = crc32(&buf[..8]);
            buf.extend_from_slice(&prelude_crc.to_be_bytes());
            for _ in 0..(total as usize - PRELUDE_SIZE - 4) {
                buf.push((rng() & 0xff) as u8);
            }
            // 随机一半帧 Message CRC 合法、一半损坏，覆盖两个分支
            let msg_crc = crc32(&buf);
            let crc = if rng() & 1 == 0 { msg_crc } else { msg_crc ^ 0xffff_ffff };
            buf.extend_from_slice(&crc.to_be_bytes());

            let mut decoder = EventStreamDecoder::new();
            decoder.feed(&buf).unwrap();
            for result in decoder.decode_iter() {
                let _ = result;
            }
        }
    }

    /// 头部失败（`InvalidHeaderType`）必须**整帧跳过**：帧边界已被两个 CRC 确认，
    /// 逐字节重同步会白耗连错预算（到顶即永久 `Stopped`，后续合法帧全丢）。
    ///
    /// 回退即 FAIL：把 `InvalidHeaderType` 移出整帧跳过分支后，坏帧之后的合法帧解析不出来
    /// （`ok` 会是 0 —— 逐字节跳过让缓冲区头部落到垃圾长度上，解码器改成「等更多数据」）。
    #[test]
    fn test_recover_skips_whole_frame_on_invalid_header_type() {
        use super::super::crc::crc32;

        // 坏帧：头部 = name_len(1) + name("x") + 非法类型(99)，两个 CRC 都算对。
        let mut hdr = Vec::new();
        hdr.push(1u8);
        hdr.push(b'x');
        hdr.push(99u8); // 非法 HeaderValueType
        let payload = b"p";
        let total = (PRELUDE_SIZE + hdr.len() + payload.len() + 4) as u32;
        let mut bad = Vec::new();
        bad.extend_from_slice(&total.to_be_bytes());
        bad.extend_from_slice(&(hdr.len() as u32).to_be_bytes());
        let crc = crc32(&bad[..8]);
        bad.extend_from_slice(&crc.to_be_bytes());
        bad.extend_from_slice(&hdr);
        bad.extend_from_slice(payload);
        let msg_crc = crc32(&bad);
        bad.extend_from_slice(&msg_crc.to_be_bytes());

        // 后面跟一个合法帧：整帧跳过之后它必须还能被解析出来。
        let mut data = bad.clone();
        data.extend_from_slice(&build_minimal_frame());

        let mut decoder = EventStreamDecoder::new();
        decoder.feed(&data).unwrap();
        let mut ok = 0usize;
        let mut err = 0usize;
        for result in decoder.decode_iter() {
            match result {
                Ok(_) => ok += 1,
                Err(_) => err += 1,
            }
        }
        assert_eq!(err, 1, "坏头部帧应恰好抛一次错误");
        assert_eq!(
            ok, 1,
            "坏帧之后的合法帧必须被解析出来（回退即 FAIL：逐字节跳过会把它一起吃掉）"
        );
        assert!(!decoder.is_stopped(), "单帧一次错误不该打进终止态");
    }

    /// 终止态后 `feed` 不得继续囤数据。
    ///
    /// 回退即 FAIL：去掉 `feed` 的终止态分支后，缓冲区会涨到 16MiB 上限，
    /// 第 5 次 `feed` 就会返回 `BufferOverflow`（调用方只记一条噪音日志）。
    #[test]
    fn test_feed_after_stopped_does_not_accumulate() {
        let mut decoder = EventStreamDecoder::new();
        // 全 0 字节：total_length=0 触发 Prelude CRC 不匹配，逐字节跳过直到连错上限。
        decoder.feed(&[0u8; 64]).unwrap();
        for result in decoder.decode_iter() {
            let _ = result;
        }
        assert!(decoder.is_stopped(), "连续错误应把解码器打进终止态");

        // 终止态后继续喂远超缓冲区上限的数据：既不该报错，也不该缓存。
        let big = vec![0xABu8; 4 * 1024 * 1024];
        for i in 0..8 {
            assert!(
                decoder.feed(&big).is_ok(),
                "终止态下第 {i} 次 feed 应为无操作而不是 BufferOverflow"
            );
        }
        assert!(
            decoder.decode_iter().next().is_none(),
            "终止态不应再产出帧"
        );
    }

    /// 缓冲区上限必须比单帧上限**多出一个 chunk 的余量**。
    ///
    /// 回退即 FAIL：把 `DEFAULT_MAX_BUFFER_SIZE` 改回 `16 * 1024 * 1024`（与单帧上限相等）后，
    /// 一帧 `total_length` 逼近上限时最后一个 chunk 会把 `buffer + chunk` 顶过上限 → 整块被拒
    /// （调用方只记 warn 就 drain）→ 该帧永远凑不齐 → 流卡死。
    #[test]
    fn buffer_cap_leaves_room_for_one_chunk() {
        assert!(
            DEFAULT_MAX_BUFFER_SIZE >= MAX_MESSAGE_SIZE as usize + 64 * 1024,
            "上限 {DEFAULT_MAX_BUFFER_SIZE} 必须 ≥ 单帧上限 {MAX_MESSAGE_SIZE} + 一个 chunk"
        );
    }
}
