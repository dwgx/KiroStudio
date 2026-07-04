# Kiro 上游协议文档

> KiroStudio 与 Kiro 上游（AWS CodeWhisperer Streaming API）之间的通信协议规范。

---

## 1. AWS Event Stream 二进制帧格式

Kiro 上游使用 **AWS Event Stream** 二进制协议传输流式响应。所有多字节整数均为 **Big Endian（网络字节序）**。

### 1.1 完整帧结构图

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                            MESSAGE FRAME                                     │
├──────────────┬──────────────┬──────────────┬──────────┬──────────┬──────────┤
│ Total Length │ Header Length│ Prelude CRC  │ Headers  │ Payload  │ Msg CRC  │
│   (4 bytes)  │   (4 bytes)  │   (4 bytes)  │  (变长)   │  (变长)   │ (4 bytes)│
│   u32 BE     │   u32 BE     │   u32 BE     │          │          │  u32 BE  │
└──────────────┴──────────────┴──────────────┴──────────┴──────────┴──────────┘
│◄──────────── PRELUDE (12 bytes) ──────────►│                     │          │
│                                             │                     │          │
│◄─────────── Prelude CRC 校验范围 ──────────►│                     │          │
│  (前 8 字节: total_length + header_length)  │                     │          │
│                                                                              │
│◄──────────────── Message CRC 校验范围 ──────────────────────────►│          │
│  (整个消息除最后 4 字节)                                          │          │
└──────────────────────────────────────────────────────────────────────────────┘
```

### 1.2 Prelude（前导，固定 12 字节）

| 偏移 | 长度 | 字段 | 说明 |
|------|------|------|------|
| 0 | 4 | `total_length` | 整个消息帧的总字节数（含自身） |
| 4 | 4 | `header_length` | Headers 区域的字节数 |
| 8 | 4 | `prelude_crc` | 前 8 字节的 CRC32 校验值 |

**约束条件：**
- `total_length` 最小值：16 字节（Prelude 12 + Message CRC 4）
- `total_length` 最大值：16 MB (16,777,216 字节)
- `payload_length = total_length - 12 - header_length - 4`

### 1.3 Headers（变长）

Headers 紧跟 Prelude 之后，总字节数由 `header_length` 指定。

**单个 Header 格式：**

```
┌────────────┬──────────┬───────────┬────────────┐
│ Name Length│   Name   │Value Type │   Value    │
│  (1 byte)  │(变长UTF-8)│ (1 byte)  │  (变长)     │
└────────────┴──────────┴───────────┴────────────┘
```

**10 种值类型（HeaderValueType）：**

| 类型ID | 名称 | 值编码 | 大小 |
|--------|------|--------|------|
| 0 | BoolTrue | 无额外数据 | 0 |
| 1 | BoolFalse | 无额外数据 | 0 |
| 2 | Byte | 1 字节有符号整数 (i8) | 1 |
| 3 | Short | 2 字节有符号整数 (i16 BE) | 2 |
| 4 | Integer | 4 字节有符号整数 (i32 BE) | 4 |
| 5 | Long | 8 字节有符号整数 (i64 BE) | 8 |
| 6 | ByteArray | u16 长度前缀 + 原始字节 | 2 + N |
| 7 | String | u16 长度前缀 + UTF-8 字符串 | 2 + N |
| 8 | Timestamp | 8 字节有符号整数 (i64 BE，毫秒) | 8 |
| 9 | UUID | 16 字节固定 | 16 |

**AWS 标准 Header 键：**
- `:message-type` — 消息类型 (`"event"` / `"exception"` / `"error"`)
- `:event-type` — 事件类型 (`"messageStart"` / `"contentBlockDelta"` 等)
- `:content-type` — 载荷格式（通常 `"application/json"`）

### 1.4 Payload（载荷）

位于 Headers 之后、Message CRC 之前。通常为 JSON 格式。

```
payload_start = 12 + header_length
payload_end   = total_length - 4
payload       = buffer[payload_start..payload_end]
```

### 1.5 Message CRC（消息校验，4 字节）

帧的最后 4 字节是对**整个消息（除最后 4 字节自身外）**的 CRC32 校验。

### 1.6 CRC32 算法

- **标准**：ISO-HDLC（即以太网/ZIP CRC32）
- **多项式**：`0xEDB88320`（反向表示）
- **验证值**：`CRC32("123456789") = 0xCBF43926`
- **Rust 实现**：使用 `crc` crate 的 `CRC_32_ISO_HDLC` 预编译实例

---

## 2. Kiro 事件类型

上游响应流由多个帧组成，每帧的 `:message-type` 和 `:event-type` Header 决定事件类型。

### 2.1 AssistantResponseEvent

**触发条件**：`:event-type = "assistantResponseEvent"`

```json
{
  "content": "这是模型输出的一段文字"
}
```

- `content`：文本片段（流式逐块到达）
- 高频事件：一次对话可能产生数百个此类帧

### 2.2 ToolUseEvent

**触发条件**：`:event-type = "toolUseEvent"`

```json
{
  "name": "Read",
  "tool_use_id": "toolu_abc123",
  "input": "{\"file_path\": \"/src/main.rs\"}",
  "stop": true
}
```

- `name`：工具名称
- `tool_use_id`：唯一标识（用于关联 tool_result）
- `input`：JSON 字符串形式的工具输入（流式分块时可能是部分 JSON）
- `stop`：是否为该工具调用的最后一个块（`true` 表示 input 已完整）

### 2.3 MeteringEvent

**触发条件**：`:event-type = "meteringEvent"`

```json
{
  "unit": "credit",
  "unit_plural": "credits",
  "usage": 0.15
}
```

- 出现在响应流末尾
- `usage`：本次请求消耗的 credit 数量

### 2.4 ContextUsageEvent

**触发条件**：`:event-type = "contextUsageEvent"`

```json
{
  "context_usage_percentage": 42.5
}
```

- 出现在响应流初期（message_start 之前或之后）
- 表示上下文窗口已使用的百分比

### 2.5 Error

**触发条件**：`:message-type = "error"`

```json
{
  "error_code": "RATE_LIMIT_EXCEEDED",
  "error_message": "Too many requests"
}
```

常见错误码：
- `RATE_LIMIT_EXCEEDED`：限流
- `CONTENT_LENGTH_EXCEEDS_THRESHOLD`：请求体过大
- `BAD_REQUEST`：请求格式错误
- `MONTHLY_REQUEST_COUNT`：月度配额耗尽
- `INVALID_BEARER_TOKEN`：Token 无效

### 2.6 Exception

**触发条件**：`:message-type = "exception"`

```json
{
  "exception_type": "InternalServerError",
  "message": "An internal error occurred"
}
```

---

## 3. Kiro 请求格式 (KiroRequest)

请求体为 JSON，发往 `POST https://q.{region}.amazonaws.com/generateAssistantResponse`。

### 3.1 顶层结构

```json
{
  "conversationState": { /* ... */ },
  "profileArn": "arn:aws:codewhisperer:..."
}
```

### 3.2 ConversationState

```json
{
  "conversationId": "uuid-v4",
  "agentTaskType": "vibe",
  "chatTriggerType": "MANUAL",
  "currentMessage": { /* CurrentMessage */ },
  "history": [ /* Message[] */ ]
}
```

### 3.3 CurrentMessage（当前消息）

```json
{
  "userInputMessage": {
    "content": "用户消息文本",
    "modelId": "14",
    "userInputMessageContext": {
      "tools": [ /* Tool[] */ ],
      "assistantContinuation": null,
      "assistantResponseContinuation": null
    }
  },
  "messageId": "uuid",
  "origin": "USER"
}
```

### 3.4 History（历史消息数组）

历史消息交替出现 User 和 Assistant 类型：

**User 消息：**
```json
{
  "userInputMessage": {
    "content": "...",
    "modelId": "14",
    "userInputMessageContext": {
      "tools": [],
      "assistantContinuation": {
        "toolResults": [
          {
            "toolUseId": "toolu_abc",
            "content": [{"text": "工具执行结果"}],
            "status": "success",
            "isError": false
          }
        ]
      }
    }
  }
}
```

**Assistant 消息：**
```json
{
  "assistantResponseMessage": {
    "content": [
      {"text": "我来帮你..."},
      {"toolUse": {"toolUseId": "toolu_abc", "name": "Read", "input": {"path": "/file"}}}
    ]
  }
}
```

### 3.5 Tool 定义

```json
{
  "toolSpecification": {
    "name": "Read",
    "description": "读取文件内容",
    "inputSchema": {
      "json": {
        "type": "object",
        "properties": {
          "file_path": {"type": "string", "description": "文件路径"}
        },
        "required": ["file_path"]
      }
    }
  }
}
```

### 3.6 ToolResult 格式

```json
{
  "toolUseId": "toolu_abc123",
  "content": [
    {"text": "文件内容..."}
  ],
  "status": "success",
  "isError": false
}
```

---

## 4. Anthropic → Kiro 格式映射

### 4.1 模型 ID 映射表

| Anthropic 模型名 | Kiro 模型 ID |
|------------------|--------------|
| `claude-sonnet-4-20250514` | `14` |
| `claude-opus-4-20250514` | `13` |
| `claude-sonnet-3-7-20250219` | `12` |
| `claude-haiku-3-5-20241022` | `10` |

### 4.2 消息格式转换规则

**Anthropic（平铺序列）：**
```json
{
  "messages": [
    {"role": "user", "content": "你好"},
    {"role": "assistant", "content": "你好！"},
    {"role": "user", "content": "帮我写代码"}
  ]
}
```

**Kiro（分层结构）：**
```json
{
  "conversationState": {
    "history": [
      {"userInputMessage": {"content": "你好", ...}},
      {"assistantResponseMessage": {"content": [{"text": "你好！"}]}}
    ],
    "currentMessage": {
      "userInputMessage": {"content": "帮我写代码", ...}
    }
  }
}
```

**核心差异：**
- Anthropic：所有消息平铺在一个数组里
- Kiro：最后一条 user 消息提升为 `currentMessage`，之前的成为 `history`

### 4.3 tool_use / tool_result 转换

**Anthropic assistant 消息中的 tool_use：**
```json
{"role": "assistant", "content": [
  {"type": "text", "text": "让我查看文件"},
  {"type": "tool_use", "id": "toolu_abc", "name": "Read", "input": {"path": "/f"}}
]}
```

**→ Kiro assistantResponseMessage：**
```json
{"assistantResponseMessage": {"content": [
  {"text": "让我查看文件"},
  {"toolUse": {"toolUseId": "toolu_abc", "name": "Read", "input": {"path": "/f"}}}
]}}
```

**Anthropic user 消息中的 tool_result：**
```json
{"role": "user", "content": [
  {"type": "tool_result", "tool_use_id": "toolu_abc", "content": "文件内容"}
]}
```

**→ Kiro userInputMessage.context.assistantContinuation.toolResults：**
```json
{"userInputMessageContext": {
  "assistantContinuation": {
    "toolResults": [{"toolUseId": "toolu_abc", "content": [{"text": "文件内容"}]}]
  }
}}
```

### 4.4 Thinking 处理方式

- Anthropic 的 `thinking` 块在请求中被**忽略**（不传给 Kiro）
- Kiro 通过配置参数 `enable_extended_thinking` 触发模型思考
- 响应中模型的思考内容作为 `<thinking>...</thinking>` 标签嵌入在文本中
- 如果配置 `extract_thinking = true`，stream.rs 会将标签内容提取为独立 ContentBlock

---

## 5. Kiro → Anthropic SSE 映射

### 5.1 SSE 事件序列

一次完整对话的 SSE 输出序列：

```
event: message_start
data: {"type":"message_start","message":{"id":"msg_xxx","model":"claude-sonnet-4-20250514","role":"assistant","content":[],"usage":{"input_tokens":150,"output_tokens":0}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"你好"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"！我来"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":42}}

event: message_stop
data: {"type":"message_stop"}
```

### 5.2 Kiro 事件 → SSE 事件映射

| Kiro 事件 | 生成的 SSE 事件 |
|-----------|----------------|
| 首个 `contextUsageEvent` | 触发 `message_start`（注入 input_tokens） |
| `assistantResponseEvent` (首个) | `content_block_start` + `content_block_delta` |
| `assistantResponseEvent` (后续) | `content_block_delta` |
| `toolUseEvent` (stop=false) | `content_block_start`(type:tool_use) + `input_json_delta` |
| `toolUseEvent` (stop=true) | `input_json_delta` + `content_block_stop` |
| `meteringEvent` | 触发 `message_delta`(带 output_tokens) + `message_stop` |
| `error` / `exception` | 错误响应或 SSE 错误事件 |

### 5.3 /cc/v1 端点差异（Claude Code 兼容）

标准 `/v1` 端点立即发送 `message_start`（input_tokens 为估算值）。

`/cc/v1` 端点使用 `BufferedStreamContext`：
1. 缓冲 `message_start` 不立即发送
2. 等待 `contextUsageEvent` 获取精确 `input_tokens`
3. 修正 `message_start.usage.input_tokens` 后再发送
4. 后续事件正常流式输出

---

## 6. 解码器状态机

### 6.1 状态转换图

```
                   feed()
    ┌──────────────────────────────┐
    │                              │
    ▼                              │
┌─────────┐     decode()      ┌─────────┐
│  Ready  │ ──────────────→  │ Parsing │
└─────────┘                   └────┬────┘
    ▲                              │
    │         Ok(Some(frame))      │
    ├──────────────────────────────┤
    │                              │
    │         Ok(None)             │
    ├──────────────────────────────┤
    │                              │
    │         Err(e)               │
    │         error_count++        │
    │                              ▼
    │                       ┌────────────┐
    │   error_count < 5     │ Recovering │
    ├───────────────────────┘            │
    │   (跳过1字节)                       │
    │                                    │
    │         error_count >= 5           │
    │                              ┌─────┴───┐
    └──────────────────────────── │ Stopped  │
              (不可恢复)           └──────────┘
```

### 6.2 四种状态

| 状态 | 含义 | 允许的操作 |
|------|------|-----------|
| `Ready` | 就绪，可接收数据 | `feed()` / `decode()` |
| `Parsing` | 正在解析缓冲区中的帧 | `decode()` |
| `Recovering` | 恢复中，跳过损坏字节 | `decode()`（重试） |
| `Stopped` | 终止，不再接受数据 | 无（需 `reset()` 重置） |

### 6.3 容错机制

**单字节跳跃恢复：**
- 当帧解析失败（CRC 不匹配、头部格式错误等），解码器跳过 1 字节
- 然后尝试从下一个字节位置重新同步帧边界
- 相当于在二进制流中"滑动窗口"寻找下一个有效帧头

**错误计数保护：**
- 每次成功解码帧后 `error_count` 重置为 0
- 连续解析失败时 `error_count` 递增
- 达到阈值（默认 5 次）后进入 `Stopped` 状态

### 6.4 缓冲区保护

| 参数 | 默认值 | 说明 |
|------|--------|------|
| 初始容量 | 8 KB | BytesMut 预分配 |
| 最大缓冲区 | 16 MB | 超过则拒绝 feed() |
| 最大连续错误 | 5 | 超过则停止解码器 |

### 6.5 迭代器模式

```rust
// 典型用法
decoder.feed(&chunk)?;
for result in decoder.decode_iter() {
    match result {
        Ok(frame) => process_frame(frame),
        Err(e) => log::warn!("帧解析错误: {}", e),
    }
}
```

`decode_iter()` 返回一个迭代器，连续调用 `decode()` 直到返回 `None`（数据不足）或进入 `Stopped`/`Recovering` 状态。
