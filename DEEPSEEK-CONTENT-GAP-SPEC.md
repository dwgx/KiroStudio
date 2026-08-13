# 补充规格：deepseek 归一化缺一条 content 形态转换

这份是 `PASSTHROUGH-COMPAT-SPEC.md`（P0–P5）的**独立补充**，改的是另一个文件，
和那六条不冲突，可以并行做。

调查时间 2026-08-09，行号基于当时 `dev` 分支，改前 `git pull` 重新核对。

## 参考实现在哪

同机有一个跑在生产、面向同一个上游（opencode Zen）的 TypeScript 实现，
所有坑位都在真实流量里验证过，可以随便参考：

```
~/Documents/WorkSpace/Project/fuckopencode/
```

对照重点：

| 你的文件 | 参考文件 |
|---|---|
| `src/kiro/deepseek_normalize.rs` | `src/deepseek.ts` |
| `src/kiro/passthrough_think_filter.rs` | `src/deepseek.ts` 的 `filterThinkingFromStream` / `completeStreamEvents` |
| — | `.claude/docs/DEEPSEEK-QUIRKS.md`（11 条坑位的完整清单与实测依据） |

那份 `DEEPSEEK-QUIRKS.md` 建议先读一遍，它记了每条坑的**实测复现方式**，
比代码注释更完整。

## 先说清楚：你的实现已经很好，只缺一条

我把 `deepseek_normalize.rs`（688 行）逐条对照了 11 条已知坑位：

| 坑位 | 状态 |
|---|---|
| 模型名映射 | 已覆盖 |
| `thinking: adaptive` → `enabled` + 去 `budget_tokens` | 已覆盖 |
| `reasoning_effort` → `output_config.effort` | 已覆盖，还额外处理了非字符串 effort |
| `context_management` / `strict` / `defer_loading` 剥离 | 已覆盖 |
| 多轮工具注入空 thinking 块 | 已覆盖（但见下方「交互问题」）|
| `max_tokens` 下限保护 | 已覆盖，且比参考实现更早做 |
| `thinking` + `tool_choice` 冲突 | 已覆盖 |
| 内置 `web_search` 工具剥离 | 已覆盖，且比参考实现更早做 |
| 响应侧剥 thinking 块 | **已覆盖**（`passthrough_think_filter.rs`，1060 行）|
| **`content` 字符串 → 内容块数组** | **缺** ← 本规格要补的 |

⚠️ 顺带修个过期注释：`deepseek_normalize.rs:8-12` 的模块头写着「响应侧未实现
`filterThinkingFromStream`」，但 `passthrough_think_filter.rs` 已经实现了
（流式逐事件状态机 + 非流式 content 数组过滤，fail-open）。改代码时把这段
过期的范围声明一并更新，否则后来人会重复实现。

## 要补的：content 字符串必须转成块数组

### 上游行为（实测，2026-08-09）

opencode Zen 的 `/v1/messages` **只接受内容块数组**：

```jsonc
// 上游拒绝 → {"error":{"message":"Empty input messages"}}
{"messages":[{"role":"user","content":"hi"}]}

// 上游接受 → 200
{"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}
```

复现方式（直连上游，绕开所有网关）：

```bash
curl -s -X POST https://opencode.ai/zen/go/v1/chat/completions \
  -H "Authorization: Bearer <上游key>" -H 'content-type: application/json' \
  -d '{"model":"deepseek-v4-flash","max_tokens":32,"messages":[{"role":"user","content":"hi"}]}'
```

注意这个报错**极具误导性** —— `Empty input messages` 会让人以为 messages 是空的，
实际是 content 的形态不对。参考实现当初就因此排查绕了很久。

Anthropic 官方 API 两种形态都支持，而 **Claude Code 发的是字符串形态**，
所以这条一定会触发，不是「可能」。

### 更隐蔽的第二个后果：thinking 注入会静默失效

`inject_missing_thinking_blocks`（`deepseek_normalize.rs:316`）这一行：

```rust
let Some(content) = msg_obj.get_mut("content").and_then(|c| c.as_array_mut()) else {
    continue;
};
```

要求 content 是数组，字符串形态直接 `continue`。

所以：**多轮工具历史里如果 assistant 的 content 是字符串，空 thinking 块注入不会
发生**，次轮就会命中「deepseek 要求回传 reasoning」那条坑，间歇 400。

这意味着补 content 转换不只是修一个 400，还顺带修复了已有 thinking 注入逻辑的
覆盖盲区。

### 实现要求

新增一个转换步骤，语义与参考实现 `src/deepseek.ts:221-231` 的
`normalizeMessageContent` 一致：

```typescript
// 参考实现（fuckopencode/src/deepseek.ts:221）
function normalizeMessageContent(body: Record<string, unknown>): void {
  const messages = body.messages;
  if (!Array.isArray(messages)) return;
  for (const m of messages) {
    if (m == null || typeof m !== 'object' || Array.isArray(m)) continue;
    const msg = m as Record<string, unknown>;
    if (typeof msg.content === 'string' && msg.content !== '') {
      msg.content = [{ type: 'text', text: msg.content }];
    }
  }
}
```

三个必须遵守的细节：

1. **空字符串不转。** 转成 `[]` 会再次触发 `Empty input messages`。留着空串交给
   上游按原样判断，不要制造更坏的形态。
2. **顺序：必须排在 `inject_missing_thinking_blocks` 之前。** 参考实现的步骤顺序是
   `... → 6) content 转换 → 7) thinking 注入 → 8) max_tokens 下限`
   （见 `fuckopencode/src/deepseek.ts:189` 与 `:196`）。
   你的主流程里 thinking 注入在第 5 步（`deepseek_normalize.rs:265`），
   所以新步骤要插在它之前。这是本规格最容易做错的地方 —— 顺序反了，
   上面说的「thinking 注入盲区」就修不掉。
3. **user 与 assistant 都要转**，不要只处理 assistant。

### 配测试

`deepseek_normalize.rs:335` 已有 `mod tests`，按同风格加：

- 字符串 content 被转成单个 text 块
- 已是数组时保持不变（幂等）
- 空字符串**不**被转
- user 与 assistant 都被转
- **回归测试**：`{role:"assistant", content:"文本", tool_calls:[...]}` 这类
  字符串形态的多轮历史，经归一化后应当既转了 content、又注入了 thinking 块
  —— 这条同时守住顺序要求

## 和你正在做的 P0–P5 的关系

- **不冲突。** 本规格只改 `deepseek_normalize.rs`（+ 它的 tests），
  P0–P5 改的是 `passthrough.rs` / `provider.rs` / `http_client.rs`。
- **但优先级相当高。** P0 修的是「连接层间歇失败」，本条修的是「协议层必然失败」。
  两者叠加才是完整可用：连接稳了但 content 形态不对，照样 400。
- 如果要排序：P0 → 本条 → P1/P2 → P3/P4 → P5。

## 可以顺手参考的其他东西

`fuckopencode` 那份实现里，这几处你可能也用得上（不强制，看你判断）：

- **`.claude/docs/DEEPSEEK-QUIRKS.md`** —— 11 条坑位 + 每条的实测复现命令
- **`src/toOpenAI.ts` / `src/toAnthropic.ts`** —— 如果哪天要做「上游只走 OpenAI
  协议」的方案（opencode Zen 的 Anthropic 兼容层工具调用是坏的：返回空 content +
  `stop_reason: null`，实测），这两个文件是完整的双向转换实现
- **`src/keypool.ts`** —— 多 key 池 + 失败分级。特别是 `markFailure` 里对 429 的
  处理：**429 的冷却必须很短**（固定 3 秒上限）。曾用 `cooldownMs/6`（线上 300s
  → 50s），结果 2 个 key 的小池连续两次 429 就整池冷却、50 秒全部 503，客户端
  直接中断。429 是账号级状态，换 key 无益，长冷却纯自伤 —— 这条教训和你
  `provider.rs:42-50` 注释里「一个卡住的请求能把整池压死」是同一类问题
- **`MAX_MESSAGE_CHARS` 的教训** —— 参考实现原本默认 200000 字符，DeepSeek V4 是
  1M token（约 400 万字符），Claude Code 读个大文件就被网关自己拒掉。已改成
  支持 `0 = 不限制`。如果你那边也有类似的单条消息长度上限，检查一下值
