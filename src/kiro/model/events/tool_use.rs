//! 工具使用事件
//!
//! 处理 toolUseEvent 类型的事件

use serde::Deserialize;

use crate::kiro::parser::error::ParseResult;
use crate::kiro::parser::frame::Frame;

use super::base::EventPayload;

/// 工具使用事件
///
/// 包含工具调用的流式数据
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolUseEvent {
    /// 工具名称
    pub name: String,
    /// 工具调用 ID
    pub tool_use_id: String,
    /// 工具输入数据 (JSON 字符串，可能是流式的部分数据)
    #[serde(default)]
    pub input: String,
    /// 是否是最后一个块
    #[serde(default)]
    pub stop: bool,
}

impl EventPayload for ToolUseEvent {
    fn from_frame(frame: &Frame) -> ParseResult<Self> {
        frame.payload_as_json()
    }
}

/// 手写 Debug：与 `Display` 同策略，`input` 原文不进 `{:?}`。
///
/// 派生 Debug 会让 `Event` 的 `{:?}`（错误链、测试 panic 消息、任何诊断日志）把原始工具
/// 参数一起打出来，绕过本仓对工具原文的显式开关（见 `Display` 的注释）。
impl std::fmt::Debug for ToolUseEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolUseEvent")
            .field("name", &self.name)
            .field("tool_use_id", &self.tool_use_id)
            .field("input_len", &self.input.len())
            .field("stop", &self.stop)
            .finish()
    }
}

impl std::fmt::Display for ToolUseEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // ⚠️ 只打标识与长度，**绝不打 `input` 原文**：它是原始工具参数流（文件内容、shell
        // 命令、token 都可能在里面），本仓对它的既定策略是「仅在显式开 `KIRO_TOOL_TRACE`
        // 探针时输出」（见 `anthropic/stream.rs::trace_tool_frame` 的文档：
        // 「原文可能含用户数据，故仅在显式开探针时输出」）。Display 是默认路径，
        // 把原文放这里等于给任何一条 `format!("{ev}")` 开了后门，且单帧上限 16MiB。
        let state = if self.stop { "complete" } else { "partial" };
        write!(
            f,
            "ToolUse[{}] (id={}, {}): input_len={}",
            self.name,
            self.tool_use_id,
            state,
            self.input.len()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Display / Debug 都不得输出 `input` 原文（原始工具参数可能含用户数据）。
    ///
    /// 回退即 FAIL：把 `Display` 改回打印 `self.input`（或把 `#[derive(Debug)]` 加回来），
    /// 这两个断言立刻变红。
    #[test]
    fn display_and_debug_omit_raw_input() {
        let ev = ToolUseEvent {
            name: "Read".to_string(),
            tool_use_id: "toolu_1".to_string(),
            input: r#"{"path":"/secret/token.txt"}"#.to_string(),
            stop: true,
        };
        let shown = format!("{ev}");
        assert!(shown.contains("Read"), "标识仍应可见: {shown}");
        assert!(shown.contains("toolu_1"), "id 仍应可见: {shown}");
        assert!(shown.contains("complete"), "stop 状态仍应可见: {shown}");
        assert!(shown.contains("input_len="), "长度仍应可见: {shown}");
        assert!(
            !shown.contains("secret"),
            "Display 不得输出 input 原文: {shown}"
        );

        let dbg = format!("{ev:?}");
        assert!(dbg.contains("input_len"), "Debug 应给长度: {dbg}");
        assert!(!dbg.contains("secret"), "Debug 不得输出 input 原文: {dbg}");
    }
}
