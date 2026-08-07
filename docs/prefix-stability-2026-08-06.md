# 前缀稳定率实测报告

> 2026-08-06 · 作者：前缀稳定专项会话 · 只读分析 `converter.rs` / `compressor.rs`，改动仅落 `src/token.rs`

## 一句话结论

**网关自己造的前缀不稳定源已基本被前几轮修完，剩下的都不在网关可控范围内。**
本轮实际找到并修掉的是**估算侧**的三个错算（不是前缀本身不稳），
并用线上 46 万条 traces 判定了那条卡了两个决策的 A/B 矛盾。

---

## 1. A/B 矛盾的判定结论

### 判定：**不是真矛盾。B 的结论成立，但 B 的方法学是错的；A 是不可复核的孤例观测。**

两条都不该继续当约束用。依据如下。

#### B 的原始记录：找不到，但**可复现**

全仓 grep `表 J` / `窄带` / `0.95833` / `0.95514` / `2795` / `5152`
只有三处命中，且互为转抄：

| 位置 | 形态 |
|---|---|
| `HANDOFF-2026-08-05-NIGHT.md:229` | 原始声明，标 `[实测]` |
| `OPEN-ISSUES-2026-08-06.md:141` | 转抄，已降级标 `[未验，来自 HANDOFF]` |
| `TRACKING-2026-08-06.md:113` | 转抄，同样标 `[未验]` |

**「表 J」在全仓只出现这一次**，没有表 A–I，没有配套数据文件，
`docs/cache-probe-data/` 里只有 EXP-1 的 8 条样本（`observations.jsonl`，
且 `credits_used` 全为 `null`，与 B 无关）。→ **B 的原始记录确实不存在。**

但 B 的方法学可以从那句话反推出来，我照着复现了：

```sql
-- 窄带控制：input 150-250k / output 100-300，按 session 内序号分 first / later
WITH s AS (SELECT session_id, credits_used, input_tokens, output_tokens,
       ROW_NUMBER() OVER (PARTITION BY session_id ORDER BY ts_ms) rn
     FROM traces WHERE ts_ms > <7天前> AND session_id IS NOT NULL AND credits_used IS NOT NULL)
SELECT CASE WHEN rn=1 THEN 'first' ELSE 'later' END g, COUNT(*), AVG(credits_used) ...
```

实测输出 `[实测 2026-08-06]`：

| 组 | n | avg credits | avg input | avg output |
|---|---|---|---|---|
| first | 3152 | 0.949761 | 196079 | 169.7 |
| later | 6662 | 0.931565 | 186561 | 176.9 |

B 报的是 n=2795/5152、0.95833/0.95514。**量级与结构完全吻合**（不同时间窗口口径不同所致）。
所以 B 确实跑过这个查询，不是编的 —— 但**这个查询证明不了它想证明的事**。

#### B 的方法学缺陷：窄带没窄住

`credits_used` 与 `input_tokens` 强相关 `[实测]`：

| input 桶 | n | avg credits |
|---|---|---|
| 0–50k | 45369 | 0.22989 |
| 100–150k | 46163 | 0.80971 |
| 200–250k | 10786 | 1.17756 |
| 500–550k | 4714 | 2.26308 |

150–250k 这个「窄带」**跨了 0.83→1.33 的 credit 区间**，而 B 的两组
avg input 差了 9518 token（196079 vs 186561）—— 组间输入不等，
**差值里混着「输入变小」而不是「缓存折扣」**。B 拿到的 0.3% 是两个反向效应
（later 输入更小 → credits 更小；later output 更大 → credits 更大）相互抵消后的残差。

#### 正确控制后：B 的**方向**是对的

按 10k input 桶 + output 100–300 双控，逐桶对比 `[实测 2026-08-06]`：

| input 桶 | n_first | cred_first | n_later | cred_later | Δ% |
|---|---|---|---|---|---|
| 100k | 463 | 0.70886 | 2805 | 0.72835 | +2.75 |
| 150k | 353 | 0.83569 | 1495 | 0.84022 | +0.54 |
| 200k | 292 | 0.95310 | 504 | 0.98003 | +2.83 |
| 250k | 213 | 1.15213 | 368 | 1.13550 | −1.44 |
| 290k | 173 | 1.31331 | 329 | 1.30797 | −0.41 |

20 个桶里 Δ 在 −2.6% ~ +8.2% 之间**无方向性地散布**，均值略为正。

再换一个更强的口径：单位输入 credit（`credits/input_tokens`），按会话深度分层，
限 `claude-opus-5` + output 100–300 `[实测 2026-08-06]`：

| input 桶 | n(rn=1) | cred/Mtok first | n(rn≥5) | cred/Mtok deep | Δ% |
|---|---|---|---|---|---|
| 150k | 1534 | 5.1722 | 4092 | 5.2277 | +1.07 |
| 300k | 595 | 4.4033 | 1382 | 4.3246 | −1.79 |
| 500k | 312 | 4.0929 | 865 | 4.1834 | +2.21 |
| 700k | 251 | 4.2397 | 452 | 4.0961 | −3.39 |

**会话越深，单位输入 credit 没有变便宜。** 若存在前缀缓存折扣，
rn≥5 组（前缀已被前几轮建立）应显著低于 rn=1 组，实测没有。

> 结论：**上游没有可观测的隐式前缀 credit 折扣** `[实测]`。B 的结论成立。

#### A 那次 47% 观测：落在正常噪声内，不可复核

A 记的是 `credits 0.141 → 0.075`（比值 0.532），单次观测，无样本数。

我查了「同 model + 同 input_tokens + 同 output_tokens」的分组内 credit 离散度
`[实测 2026-08-06]`：n≥5 的 185 个分组里，**150 个组内有多个不同的 credit 值，
组内极差平均 49.06%**。即：**在所有变量都固定的情况下，credit 本身就有约 50% 的散布。**
A 的 0.532 完全落在这个噪声带内。

具体例子（`claude-opus-4.6`，input=4224，output=1，n=5233）：

| credits | n |
|---|---|
| 0.030468 | 5145 |
| 0.016710 | 88 |

比值 0.548 —— 与 A 的 0.532 几乎相同。我逐项排除了它的成因 `[实测]`：

- **不是会话位置**：两个模式都 ~99% 是 `rn=1`（低模式 89.7%、高模式 98.9% 为首请求）→ 与前缀缓存无关
- **不是凭据档位**：低模式那 56 个 credential_id **全部同时出现在高模式里**
- **不是定价变更**：两个模式在 07-30 ~ 08-05 每一天都同时出现
- **不是重试/流式/结果差异**：两模式均 `is_streaming=0` / `retries=0` / `outcome=success`

→ **成因未确认**（`traces.db` 不记 region，无法验证「同一 credit 阶梯在不同 region 定价不同」这个残留假设）。
但可以确定的是：**它与会话位置无关，因此不是前缀缓存折扣**。

A 的注释（`converter.rs:886-893`）把这个约 45% 的双模态噪声当成了「前缀一致带来的折扣」。

#### 对决策的影响

- `agentContinuationId` 确定性派生、`derive_conversation_id_from_context`
  这些改动的**原始理由（拿 credit 折扣）不成立**。
- 但**不建议回滚**它们：它们同时带来 token 节省与会话亲和稳定（亲和绑定依赖 conversationId），
  这两项收益独立于 credit 折扣且已在线上生效。只是**别再用「47% 折扣」当依据**。
- L0（前缀稳定）的价值退化为纯 token 节省 + 亲和稳定 → 与 RFC 的「分叉 C」一致。

---

## 2. 不稳定源清单

「进入上游 body 的历史部分，且可能逐请求变化」的字段，逐项核实：

| # | 字段 / 机制 | 稳定？ | 根因 / 依据 | 进入历史前缀？ |
|---|---|---|---|---|
| 1 | `agentContinuationId` | ✅ 稳定 | `converter.rs:657` SHA256(`"agent-continuation:"` + conversationId)，纯函数 `[代码]` | 否（顶层字段） |
| 2 | `conversationId` | ⚠️ 条件稳定 | 三级回落 `converter.rs:880-885`：① `metadata.user_id` 里的 session UUID → ② `derive_conversation_id_from_context` → ③ **`Uuid::new_v4()`** `[代码]`。仅当 system 与 tools **双双为空**才落到随机分支（`converter.rs:786-788`） | 否（顶层字段） |
| 3 | 派生 conversationId 的输入 | ✅ 稳定 | `converter.rs:775-782`：工具名 `sort_unstable()` 后入哈希；system 先过 `canonicalize_system_text`；**不含 messages** `[代码]` | — |
| 4 | **tools 数组顺序** | ✅ 稳定 | `convert_tools` 用 `tools.iter()`（`converter.rs:1428`）**保持客户端原序**，无 HashMap 迭代 `[代码]` | 是（在 currentMessage.context） |
| 5 | **tool `input_schema` 的键序** | ✅ 稳定 | 源类型是 `HashMap<String, Value>`（`types.rs:248`）→ 本来有序问题。但 **serde_json 未启用 `preserve_order`**：`Cargo.lock:2794-2804` 的 serde_json 1.0.150 依赖只有 itoa/memchr/serde/serde_core/zmij，**无 indexmap** → `Value::Object` 是有序 `BTreeMap`，HashMap 迭代序不外泄 `[代码]` | 是 |
| 6 | 历史占位工具的追加顺序 | ✅ 稳定 | `collect_history_tool_names` 返回 **`Vec`** + `contains` 去重（`converter.rs:812-828`），按历史出现序；消费它的循环（`converter.rs:927`）因此确定。**注意**：同一循环里的 `existing_tool_names` 是 `HashSet`，但只用于 `contains` 判定，不决定顺序 `[代码]` | 是 |
| 7 | 环境噪音剥离 | ✅ 确定性 | `strip_env_noise_lines`（`converter.rs:515-575`）逐行匹配固定字面量 + `collapse_blank_lines`，无随机/无时间 `[代码]` | 是（system 块） |
| 8 | 归因头折叠 | ✅ 确定性 | `canonicalize_billing_header`（`converter.rs:413-419`）整块 → 固定占位符 `[代码]` | 是 |
| 9 | 图片历史去重 | ✅ 确定性 | `image_dedup` 是 `HashSet`（`converter.rs:1586`），但只做「是否首次出现」判定，**结果由消息遍历序决定**，非迭代序 `[代码]` | 是 |
| 10 | `model_id` | ⚠️ 见下 | 嵌在**每一条** history `UserInputMessage`（`conversation.rs:101`）`[代码]` | 是 |
| 11 | **输入压缩触发** | 🔴 **不稳定** | 见下 | 是（重写整个历史） |
| 12 | `chatTriggerType` / `agentTaskType` / `origin` | ✅ 常量 | `"MANUAL"` / `"vibe"` / `"AI_EDITOR"` `[代码]` | — |
| 13 | 时间戳 / UUID / 随机数 | ✅ 无 | `grep now()\|Uuid\|rand::\|timestamp` 在 `src/kiro/model/requests/` **零命中** `[代码]` | — |

### #11 输入压缩：唯一真实的网关侧前缀断裂源

`handlers.rs:481`：

```rust
if compression.enabled && body.len() > compression.trigger_bytes {
    let stats = super::compressor::compress(&mut kiro_request.conversation_state, compression);
```

- 门条件是**整个 body 的序列化后大小**（默认 4MiB，`config.rs:678`），`enabled` 默认 `true`
- 一旦跨过阈值，`compress_whitespace_pass` 会遍历**全部 history 消息**逐条改写
  （`compressor.rs:102-118`），`compress_tool_results_pass` 同样作用于历史 tool_result
- 后果：同一会话在体积**首次跨过 4MiB 的那一刻**，此前所有轮次的历史字节被整体重写
  → 该点之后与之前的前缀完全不同 → 前缀缓存（若存在）在此断一次

**性质**：每会话**只断一次**（跨阈值后持续压缩，压缩本身是确定性的：
`compress_whitespace` / `smart_truncate_by_lines` 都是纯函数，
且对已压缩过的文本幂等 —— 空白已折叠、tool_result 已截断到定长）。
所以这是**有界**的一次性断裂，不是逐请求抖动。

**未验**：我没有实测线上有多少会话真的跨过 4MiB。`traces.db` 不记 body 字节数，
只有 `input_tokens`；4MiB ≈ 1M+ token 量级，而实测 input p90 约 658k token
（引自 `converter.rs:734` 的注释数据，非本轮实测）→ 推测占比很低，但**未量化**。

### #10 model_id：跨模型回退会重写整个历史

`model_id` 写进每条 history `UserInputMessage`（`conversation.rs:101`）。
`overload_fallback_model`（`provider.rs:1946`）在 `MODEL_TEMPORARILY_UNAVAILABLE`
重试耗尽时换模型重试 —— 那一次的整个历史 `modelId` 字段全变。

**性质**：这是**必要的**（换模型就得告知上游），且换模型本身已经使上游缓存失效
（缓存按模型分区）。所以**不是缺陷，不需要修**。列在这里是为了说明
「前缀稳定率」的分母里天然含这一项。

---

## 3. 现状 → 理论上限

### 「前缀稳定率」的可测口径

上游不回传缓存命中量（`docs/CACHE-EXP0-RESULT.md`，EXP-0 已确证），
所以**无法直接测命中率**。可测的是它的两个必要条件：

1. **字节稳定**：同会话连续请求的历史部分逐字节一致
2. **账号稳定**：连续请求落在同一凭据（前缀缓存是账号维度的，换号必然不命中）

### 条件 2 可以用线上数据直接量化 `[实测 2026-08-06]`

```sql
WITH s AS (SELECT session_id, credential_id cid,
       LAG(credential_id) OVER (PARTITION BY session_id ORDER BY ts_ms) prev
     FROM traces WHERE ts_ms > <7天前> AND session_id IS NOT NULL AND outcome='success')
SELECT COUNT(*), SUM(cid=prev) FROM s WHERE prev IS NOT NULL;
```

| 指标 | 值 |
|---|---|
| 有前驱的同会话请求 | 179787 |
| 与前驱同凭据 | 165391 |
| **亲和保持率** | **91.99%** |

→ **8.01% 的后继请求换了号**，这部分无论字节多稳都不可能命中。

补充分布 `[实测]`：有 session 的请求共 404879 条，其中 **31.65%（128134 条）是单请求会话**
—— 这些请求**结构上不可能有缓存收益**（没有第二次请求来命中）。
另有 229 个「会话」各含 100+ 请求、合计 264549 条，且单会话跨 29–117 个 credential
（`client_ip` 恒为服务器自身，无法据此区分真实客户端）→ 这些大概率是
`derive_conversation_id_from_context` 把不同客户端**撞进同一个派生键**的结果，
与 `converter.rs:731-745` 注释里「跨用户撞键是安全的」的设计预期一致。

### 量化：现状 → 可达

**⚠️ 分母口径**：以下百分比的分母是「有前驱的同会话成功请求」（179787 条）
—— 即「理论上有机会命中缓存」的那部分请求。单请求会话不在分母内。

| 项 | 占分母 | 依据 | 可修？ |
|---|---|---|---|
| 亲和被打破（换号） | **8.01%** | `[实测]` Q22 | 部分（调度器权衡，不在本轮范围） |
| 输入压缩跨阈值断裂 | **未量化** | `[未验]` 无 body 字节数记录 | 可（见任务 5 改法 B） |
| 模型回退重写历史 | 未量化 | `[未验]` | 不该修（换模型必然失效） |
| 字节不稳（其它） | **0**（未发现） | 清单 #1–#13 逐项核实 `[代码]` | 已修完 |

**结论**：

- **网关自己造的字节不稳定源，本轮逐项核实后为 0** —— 前几轮（归因头折叠、
  环境噪音剥离、确定性 `agentContinuationId`、派生 conversationId、工具名排序）
  已经把可控部分做满了。用户问的那个「97」我**没有在仓里找到出处**，
  也无法从 traces 反推（不记 body 字节）。
- **字节维度的上限已达到**（除压缩阈值那一次有界断裂）。
- **剩余缺口 91.99% → 100% 的 8.01% 全在账号亲和**，而非前缀构造。
  这一项动的是调度器（`token_manager.rs` / `affinity.rs`），本轮不在我的文件范围内，
  且 CLAUDE.md 明确记载亲和与限流的权衡有实测依据（`rateLimitEnabled=false`
  正是为了不踢开亲和绑定）—— **不要在不做控制实验的情况下动它**。

**不可控部分**（客户端侧，网关无法消除）：

- 客户端每轮改 system prompt（CC 的 `<env>` 块已被我们剥离，但用户自己的
  CLAUDE.md 变更、`# auto memory` 之外的动态内容仍会漂）
- 用户中途增删 tools 定义 → tools 数组变 → 派生键与前缀双变
- 31.65% 的单请求会话（结构上无缓存可言）

---

## 4. `token.rs` 实际改了什么

三处，都是**估算诚实性**问题，不是前缀稳定性问题。
保留估算（用户明确说「假 cacheread 也可以」），只让它别虚报。
与 `CACHE_ESTIMATED_HEADER`（`handlers.rs:1735`，只读确认）的标注机制一致
—— 该头在**实际下发** cache 字段时出现，我的改动只减少「本不该下发却下发了」的情形。

### 改动 1：历史边界与 converter 对齐（`count_prefix_tokens`）

**根因**：`converter.rs` 的真实转发历史是 `messages[..last_user_idx]`
（先 prefill 截断到最后一条 user：`converter.rs:861-871`；再由 `build_history`
去掉末尾那条 user 作 currentMessage：`converter.rs:1583`）。
而原实现直接 `&messages[..messages.len() - 1]` —— 对 prefill 载荷
（末尾是 assistant）会把**当前轮**的 user 消息算成「已缓存前缀」。

改为按 role `rposition` 定位边界，并把「第一轮」判据从 `messages.len() <= 1`
改为**历史切片为空**（`[user, assistant]` 长度 2 但真实历史为空）。
无 user 消息时返回 0（该载荷 converter 会报 `EmptyMessages`）。

### 改动 2：拆掉幽灵 token（`count_all_tokens_local_unfloored`）

`count_all_tokens_local` 末尾的 `.max(1)` 是给「请求输入 token」用的，
但 `count_prefix_tokens` 调它**两次再相加** → 空 system + 空历史被算成 2
→ `estimate_cache_breakdown` 的 `prefix_tokens > 0` 闸门（`handlers.rs:1765`）
被这个幽灵值顶开 → 客户端收到 `cache_read_input_tokens: 2` 而实际零缓存。

拆出无下限私有版本，公开函数保持 `.max(1)` 契约不变（有测试守卫）。

### 测试（7 个，`src/token.rs` 的 `prefix_tokens_tests`）

原先 `token.rs` **零测试**。

```
test token::prefix_tokens_tests::should_keep_floor_of_one_on_public_local_counter ... ok
test token::prefix_tokens_tests::should_return_zero_for_single_turn ... ok
test token::prefix_tokens_tests::should_return_zero_for_prefill_payload_whose_history_is_empty ... ok
test token::prefix_tokens_tests::should_not_invent_phantom_tokens_when_prefix_is_empty ... ok
test token::prefix_tokens_tests::should_count_real_history_and_system ... ok
test token::prefix_tokens_tests::should_return_zero_when_no_user_message_exists ... ok
test token::prefix_tokens_tests::should_cut_history_at_last_user_not_at_last_message ... ok
test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 1269 filtered out
```

`cargo check --no-default-features`：**0 error**（20 个 warning 全部在其它 agent 的文件里，非本轮引入）。
`rustfmt --edition 2024 src/token.rs` 通过（**未**跑全仓 fmt）。

### 回退验证（实际观察到的输出）

**全量回退**（把 `count_prefix_tokens` 改回原实现，测试不动）→ 4 个 FAILED：

```
should_cut_history_at_last_user_not_at_last_message ... FAILED
  assertion `left == right` failed: 边界应落在最后一条 user 之前
  left: 1009   right: 9
should_return_zero_for_prefill_payload_whose_history_is_empty ... FAILED
  assertion `left == right` failed: 末尾 assistant 时真实历史为空，不应把当前轮 user 计入已缓存前缀
  left: 1401   right: 0
should_return_zero_when_no_user_message_exists ... FAILED
  left: 3      right: 0
should_not_invent_phantom_tokens_when_prefix_is_empty ... FAILED
  assertion `left == right` failed: 空前缀不应产出正值（两次 .max(1) 的幽灵值）
  left: 2      right: 0
test result: FAILED. 3 passed; 4 failed
```

`left: 1401` 就是缺陷的实际量级：一个首轮 prefill 请求会虚报 1401 token 的 cache_read。

**部分回退**（针对本仓「测了分支内部没测分支顺序」那个病症）：
只把两次 `count_all_tokens_local_unfloored` 换回 `count_all_tokens_local`、**保留**边界修复
→ 恰好 1 个 FAILED：

```
should_not_invent_phantom_tokens_when_prefix_is_empty ... FAILED
  left: 2      right: 0
test result: FAILED. 6 passed; 1 failed
```

→ 证明两个修复**各自独立承重**，不是一个修复被四条测试重复覆盖。
`should_cut_history_at_last_user_not_at_last_message` 刻意比较**数值**
（1009 vs 9）而非只断言 `> 0` —— 后者在新旧实现下都会通过，抓不住缺陷。

---

## 5. 需要你接的跨文件改法

我没改 `converter.rs` / `compressor.rs` / `handlers.rs`。以下按建议强度排序。

### 改法 A（建议做）：修正 `converter.rs:886-893` 的过期依据注释

**位置**：`src/anthropic/converter.rs:886-893`，`agent_continuation_id` 上方的注释块。

**现状**：

```rust
// 实测(2026-07-07 Phase0):同一大 prompt 前缀一致时上游 credit 折扣约 47%
// （meteringEvent credits 0.141→0.075）。而每请求随机的 continuationId 若进入上游
// 会话/前缀键,会让同一会话的连续请求无法命中上游 prefix 缓存,白白丢掉这份折扣。
```

**建议改为**（保留结论、更正依据）：

```rust
// ⚠️ 2026-08-06 复核：原注释引的「47% credit 折扣」(0.141→0.075) 已被证否。
// 线上 7 天 traces 实测：固定 model+input_tokens+output_tokens 后，credit 组内极差
// 平均 49%（185 组中 150 组多值），0.532 这个比值落在该噪声带内；且按 10k input 桶
// 双控后，会话深度与单位输入 credit 无相关（20 桶 Δ 在 ±8% 无方向散布）。
// 依据：docs/prefix-stability-2026-08-06.md 第 1 节。
// 确定性派生**仍然保留**，但理由改为：① 稳住会话亲和键（affinity 依赖 conversationId）；
// ② 前缀稳定带来的纯 token 节省。不要再用「credit 折扣」当依据。
```

**为什么**：这条注释正是本仓那个系统性病症的活体样本 ——
它把一次不可复核的孤例观测写成 `[实测]`，后续会话（含派生 conversationId 那次改动）
都把它当既定事实当约束。**风险**：纯注释改动，零行为风险。

### 改法 B（可选，需你判断收益）：让压缩阈值不断裂前缀

**位置**：`src/anthropic/handlers.rs:481`（`build_kiro_request_body` 的门条件）。

**问题**：门条件是整体 body 大小，跨阈值那一刻整个历史被重写一次。

**两个方向**：

1. **无条件压缩**（`trigger_bytes` 设 0 / 移除门）：历史从第一轮起就是压缩态，
   永不发生「某轮突然重写」。代价是对所有请求做有损处理
   —— 与 `config.rs:658-661` 注释里「避免对正常小请求做任何有损处理」的
   保守设计**直接冲突**，需要你拍板。
2. **只压当前轮，不动历史**：`compress_whitespace_pass`（`compressor.rs:102-118`）
   改为跳过 `state.history`、只处理 `state.current_message`。
   前缀永不被重写，但省下的字节大幅减少（大 body 主要是历史堆积），
   **可能压不下 5MiB 硬限** → 会把现在能救回的请求变成 400。

**我的建议：先不做。** 断裂是每会话一次且有界，而两个方向各有明确代价；
更重要的是**上游前缀折扣已被证否**，这个断裂的实际损失只是 token 而非 credit，
优先级应低于 RFC 里的 L3。若要做，先量化「多少会话跨过 4MiB」——
需要在 `handlers.rs:481` 那里加一个计数器（body 字节数当前不落库）。

### 改法 C（不建议做，仅记录）：给 `count_prefix_tokens` 加上下文窗口上限

原任务提到「历史超过上游窗口，被截断的部分不该算进缓存」。
这需要把 model 传进 `count_prefix_tokens` 以调用
`converter::get_context_window_size`，会改动 `handlers.rs:1370` 与 `:2417` 两个调用点
（都在其它 agent 的文件里）。

**且当前已有一层收敛**：`estimate_cache_breakdown`（`handlers.rs:1771`）
已做 `prefix_tokens.min(input_tokens)`，而 `input_tokens` 是同一套本地估算
（system + messages + tools）→ 前缀恒 ≤ 输入。窗口上限只在
「输入本身超窗口」时才额外收紧，那种请求上游会直接 400。**收益极小，不值得动两个热文件。**

---

## 6. 我没能验证的部分

1. **用户口中的「97」出处不明**。全仓没找到这个数字的来源，也无法从 `traces.db`
   反推（不记 body 字节数、不记前缀哈希）。我给的 91.99% 是**账号亲和保持率**，
   与「字节稳定率」是两个不同口径 —— 不要把它当成那个 97 的对应量。
2. **压缩跨阈值的实际发生率未量化**：`traces.db` 无 body 字节数字段。
3. **credit 双模态（约 45% 差）的成因未确认**。已排除会话位置、凭据、时间、
   重试、流式；残留假设是 region 差异，但 `traces.db` 不记 region，无法验证。
   **已确定的是它与会话位置无关**，故不影响前缀缓存的判定。
4. **字节稳定率没有端到端实测**：清单是**代码级**逐项核实（`[代码]`），
   不是抓包比对两个连续请求的实际 body。要做端到端验证需要在
   `build_kiro_request_body` 处落前缀哈希，那是 `handlers.rs`（他人文件）。
5. **B 的原始记录确认不存在**（表 A–I、表 J 均无），我是**反推方法学后复现**的。
   若 B 当时实际跑的不是我复现的那个查询，则我对「B 方法学有缺陷」的判断需要重审
   —— 但我复现出的 n 与均值量级与 B 高度吻合，故这个风险很低。
6. **`input_tokens` 是本地估算而非上游真值**，所有基于它分桶的控制都继承这层误差。
   不过它不影响结论方向：若上游给了缓存折扣，`credits_used`（上游真实计费）会下降
   而我方估算的 input 不变 → 折扣仍应显形。实测没有显形。
