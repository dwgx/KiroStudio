# EXP-0 实测结果与交接状态

> 2026-07-28 · 实测已完成并上线 · 配套阅读 `CACHE-RESEARCH.md`（研究）与 `CACHE-RFC.md`（方案）

这份文档的作用：**RFC 里每一层都依赖 EXP-0 的结论，而结论现在有了。**
接手者先读这份，再决定要不要读那两份的对应章节。

---

## 一句话结论

**上游不发 prompt cache 真值。RFC 落在「分叉 C 附近」——L2-2（接入上游真值）整个不用做。**

但 EXP-0 顺带发现了一个比原目标更值钱的缺陷：`reasoningContentEvent` 被静默丢弃。

---

## 已做并已上线

`src/kiro/model/events/base.rs` 的 `EventType::Unknown` 分支此前静默丢弃未知事件，
**连事件类型名都不留**。已加日志：

- `warn` 级：`event_type` + `payload_bytes`（低频，每个新类型值得知道一次）
- `debug` 级：payload 截断至 512 字符（避免正常流量刷屏）

配套测试 `test_metadata_event_still_unclassified`：断言 `metadataEvent` 目前仍落 Unknown。
**这条测试是故意会失败的** —— 将来接入 metadataEvent 解析时它必须同时更新，
以此防止「加了变体但忘了从 Unknown 分支摘出来」的半成品状态。

线上状态：二进制 `512bdcdf14fcf77b`，889 tests passed，debug 日志已关闭（仅在诊断时临时开）。

诊断时临时开启的方法：

```bash
ssh ws-vps 'mkdir -p /etc/systemd/system/kirostudio.service.d
cat > /etc/systemd/system/kirostudio.service.d/30-exp0-events.conf <<EOF
[Service]
Environment=RUST_LOG=info,kirostudio::kiro::model::events=debug
EOF
systemctl daemon-reload && systemctl restart kirostudio'
# 采集完必须删掉该文件并 daemon-reload，否则持续刷屏
```

⚠️ **journalctl 输出带 ANSI 转义，裸 grep 会漏匹配**。统计前先 `sed` 去色，
否则会得出「零命中」的错误结论。

---

## 实测数据（18 分钟窗口，20 个探针请求全部 200）

### 上游实际发送的未知事件类型

| 事件类型 | 出现次数 | payload 实测原文 |
|---|---|---|
| `metadataEvent` | 26 | `{"stopReason":"END_TURN"}` —— **仅此一种形态** |
| `initial-response` | 26 | `{"conversationId":""}` —— 恒为空串 |
| `reasoningContentEvent` | 46 帧 | `{"text":"..."}` 增量文本，最大单帧 **1228 字节** |

### 判据：tokenUsage / cacheRead* → 零命中

grep `tokenUsage|cacheReadInputTokens|cacheWriteInputTokens|uncachedInputTokens` **无任何命中**。

`metadataEvent` 字样出现 49–51 次，但**全部来自我们自己新加的日志行**的 `event_type=` 字段，
payload 里只有 `stopReason`。

### 这推翻了什么

RESEARCH 文档基于 AWS 官方 `amazon-q-developer-cli` 的 Smithy 客户端，指出
`MetadataEvent.tokenUsage` 含 `uncachedInputTokens` / `cacheReadInputTokens` /
`cacheWriteInputTokens`，并推断「真信号可能一直在线上，我们从没看过一眼」。

**实测否定了这个推断。** `runtime.{region}.kiro.dev/generateAssistantResponse` 这条链路上的
`metadataEvent` 是精简版。那些字段可能只在 CodeWhisperer / Q CLI 端点投递，或需显式 opt-in。

**同时这也排除了一个担忧**：本仓的影子估算**没有在覆盖任何上游真值**。
先前「我们可能一直在用估算覆盖真值」的顾虑不成立，`handlers.rs` 的 cache_breakdown 注入
虽然仍是估算值（问题依旧，见 RFC L2-1），但至少不是在丢弃更好的数据。

---

## 更值钱的发现：reasoningContentEvent 被丢弃

`reasoningContentEvent` 同样落在 Unknown 分支被**整条丢弃**，而它是上游的
**结构化 thinking 增量流**：

```
{"text":"I"}
{"text":"'m"}
{"text":" not"}
{"text":" seeing there"}
{"text":" model_information section provided to"}
{"text":" answer directly"}
```

为什么这比原目标重要：KiroStudio 现在的 thinking 是**从响应文本里正则抓 `<thinking>` 标签**
实现的（`stream.rs:340` 的 `extract_thinking_from_complete_text`），还要处理跨 chunk 分割、
伪造 `THINKING_SIGNATURE_PLACEHOLDER`（`stream.rs:140`）。

**上游一直在用结构化事件发同一份内容，我们却扔了去文本里捞。**

### 但这件事**已评估并决定暂不做**

曾起过一个 workflow（3 路并行侦察 → 设计 → 2 路对抗审查），中途停止。停止理由：

1. `stream.rs` 有 **469 处** thinking 引用，且**另一会话本轮刚改过那块**
   （invoke 嗅探、thinking 文本路由），改动窗口不干净
2. Anthropic 协议要求 thinking block 在 text 之前、`content_block_index` 不能冲突。
   上游事件到达顺序不可控，交错时容易产出**非法事件流** ——
   客户端表现是解析崩溃而非优雅降级
3. `signature` 是完整性签名。现在伪造占位符能用，但结构化路径下客户端可能校验更严
4. 实测 `reasoningContentEvent` **只在部分请求出现**，所以正则路径必须保留；
   两条路径共存就有**重复发 thinking block** 的风险

收益是「thinking 更干净」，代价是核心 SSE 链路可能崩。**有真实用户在跑时这个交换不划算。**

### 若将来要做，这些是已探明的约束

- 改动面：`src/kiro/model/events/`、`src/anthropic/stream.rs`（8 处 match Event）、
  `src/anthropic/handlers.rs`（3 处 match Event）
- `src/debug.rs` 里也 match 了 `Event::Unknown { event_type, payload }`（**带字段**，
  与现在的 `Unknown {}` 不符），但它是**孤儿文件**——`main.rs` 的 mod 列表里没有
  `mod debug;`，不参与编译，可忽略
- 必须先决定与正则路径的关系（结构化优先 / 并存 / 完全替换），
  且**不能直接删正则路径**，因为不是所有请求都发该事件
- 必须验证 buffered ctx 路径（`ccAutoBuffer=true` 是线上默认）与流式路径行为一致

### 附带发现（未处理）

`initial-response` 的 `conversationId` **恒为空串**。目前无影响——我们的会话亲和键是从
**请求侧**派生 conversationId（`converter.rs:637-641` 的 `extract_session_id`），
不依赖上游回值。但这说明上游可能不认我们发的那个值，值得在排查亲和问题时留意。

---

## RFC 的分叉判定

按 `CACHE-RFC.md` 的「分叉预案」章节：

**不是分叉 A**（上游有 tokenUsage）—— 已证否。

**接近分叉 C**（上游无缓存信号），但**未完全落实**：EXP-0 只证明了「不发 usage 字段」，
**没有证明「上游没有缓存折扣」**。这两件事不同：

- `converter.rs:747` 记录过一次 0.141 → 0.075 的 credit 观测（约 47% 折扣）
- 若该观测为真，则上游**有**隐式前缀缓存，只是不告诉我们命中量
- 若为假（credit 阶梯取整的假象），则真的没有缓存

**下一步该做 EXP-1 / EXP-2**（见 RFC）：先判断 credit 粒度，再用同前缀连打测折扣是否存在。
这决定 L0（前缀稳定）到底有没有价值。

### 分叉 C 若被证实，优先级要重排

RFC 原文已写明：上游既然不给缓存折扣，**L3（网关自建响应缓存）反而升为最高优先**，
因为那时它是唯一的省钱手段。而 L0/L1 的价值退化为纯 token 节省。

---

## 当前线上配置（与本主题相关）

| 项 | 值 | 说明 |
|---|---|---|
| `promptCacheEnabled` | 死配置 | 零读取点，见 RFC L2-3 要把它复活为真开关 |
| `promptCacheTtlSeconds` | 3600 | RFC 建议改 300 对齐上游 5m |
| cache_breakdown 注入 | **仍在注入估算值** | RFC L2-1 建议停止，本轮未做 |
| `extractThinking` | true | 正则抓标签路径，未改动 |

---

## 给接手者的三条

1. **不要重复 EXP-0。** 结论已在上面，日志改动已上线。要重新采集只需临时开 debug 日志。
2. **做 EXP-1/EXP-2 之前先读 RFC 的实验矩阵。** 它有完整的预算控制（专用测试凭据、
   QPS ≤ 0.2、遇 429 停 5min、总量 ≤ 350 请求 ≈ 70 credits）和 8 组实验设计。
3. **reasoningContentEvent 那件事有明确的「为什么现在不做」。** 若要推翻这个决定，
   先确认 `stream.rs` 的改动窗口是否干净（另一会话是否已收工），
   并准备好对抗性审查而不是直接改。
