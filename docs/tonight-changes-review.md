# 今晚三批改动对抗性复核（2026-08-04）

只读复核，未改任何源码，未跑 cargo/pnpm/git 写操作。
标注约定：**[证据]** = 有代码/配置证据；**[推测]** = 未能证实的推断。

---

## 改动 1：invoke 重组修复（`stream.rs:2440`）

### 你的根因判断：成立 [证据]

字节算术核实无误。`process_content_with_thinking` 的「未见 `<thinking>`」分支
（`stream.rs:1553-1557`）扣留 `len - "<thinking>".len()` = 末尾 **10 字节**；
而 `"</invoke>".len()` = **9 字节** → 完整 invoke 块的闭合标签必然整段滞留 thinking_buffer，
只有前半截进 `invoke_sniff_buffer`。`drain_invoke_sniff_buffer` 里
`find_invoke_block_end`（:3341）找不到闭合 → 走「半块 hold」分支（:1904-1913）→ 永远等不到闭合。

### F1-1 — 同类旁路仍在，且可达（`stream.rs:2118`）🟠 中高

**tool_use 边界**那处没修，仍是裸 `create_text_delta_events`：

```rust
// stream.rs:2112-2119  process_tool_use 内
if self.thinking_enabled && !self.in_thinking_block
    && !self.thinking_extracted && !self.thinking_buffer.is_empty() {
    let buffered = std::mem::take(&mut self.thinking_buffer);
    events.extend(self.create_text_delta_events(&buffered));   // ← 绕过 sniff
}
```

触发条件与 :2440 完全同源，且是**生产常态组合**：thinking 开启 + 未出现内联
`<thinking>` + 同一轮里既有文本化 invoke 又来了真 `toolUseEvent`。两个后果：

1. 被扣留的 ≤10 字节尾巴直接吐成 text_delta，而 sniff 里 hold 的前半截要等到
   `flush_invoke_sniff_buffer`（:2455）才吐 → **文本顺序颠倒**（后到的先出）。
2. `"<invoke"` 只有 7 字节，完全可以整段落在那 10 字节尾巴里（例如 chunk 恰好以
   `\n<invoke` 结束）。此时开标签被直接吐成文本、后续内容进 sniff 时已无开标签 →
   `find_invoke_start` 返 None → **整块当纯文本泄漏，工具不执行**。这正是本次要修的
   同一缺陷，只是换了触发点。

修法：与 :2440 同款，改走 `emit_non_thinking_text`。安全性同理——此处
`in_thinking_block=false`，且 `handle_content_block_start`（:577-592）在 tool_use
开块前会自动 stop 打开的 text 块，顺序合法。

**回退即 FAIL 测试：可写。** thinking=true，先喂 `"...\n<invoke name=\"Bash\">..."`
分片（末尾停在开标签内），再喂一个真 `toolUseEvent`，最后 `generate_final_events()`；
断言 `reclaimed_invoke_count == 1` 且 events 里无裸 `<invoke` 文本。

### F1-2 — `:2404` / `:2104` 确为死代码，你的判断成立 [证据] 🟢 低

`find_real_thinking_end_tag_at_buffer_end`（:293）硬要求
`buffer[after_pos..].trim().is_empty()`。`trim()` 与 `trim_start()` 同用
`char::is_whitespace` → 全空白串的 `trim_start()` 恒为空 ⇒ `remaining` 恒空 ⇒
`:2404`、`:2104` 两处 `create_text_delta_events(&remaining)` **不可达**。
**证伪失败，你是对的**。但它是个潜伏旁路：一旦哪天放宽那个全空白条件，两处立刻变成
活的 reclaim 旁路。建议加 `debug_assert!(remaining.is_empty())` 或干脆也走统一出口。

### V1 — 事件顺序仍合法 [证据]

:2440 只在 `in_thinking_block == false` 的 else 分支执行（:2372 的取反）；而
`in_thinking_block` 被置 false 的三处（:1604、:2083、`close_reasoning_thinking_block`
:2019）**都同时发了 thinking 块的 `content_block_stop`**。若 drain 里
`synthesize_tool_use`（:1740）开 tool_use 块，`handle_content_block_start` 会先自动
stop 打开的 text 块。故不会出现「新块 start → 旧块 stop」交错。

另一处正向收益：drain 若真重组出 tool_use，`has_non_thinking_blocks()` 变真 →
:2460 那条「仅 thinking」分支不再误置 `stop_reason=max_tokens` 并补一个空格 text 块。

### V2 — `in_thinking_block` 为真时不会把思考内容并进正文，证伪失败 [证据]

:2440 处在 `if self.in_thinking_block { ... } else { ... }` 的 else 里，真分支
（:2372-2426）走 thinking_delta，两者互斥。且每次 `in_thinking_block → false`
都同时置 `thinking_extracted = true`，此后文本一律走正文出口。**你的判断成立。**

### F1-3 — dsml 尾巴与 sniff 的 flush 顺序倒置 🟢 低

`flush_dsml_tail()`（:2453）在 `flush_invoke_sniff_buffer()`（:2455）之前，且
`flush_dsml_tail` 内部（:1333）也是裸 `create_text_delta_events`。若 sniff 仍 hold
半块而 dsml 尾巴非空 → 后到的尾巴先出。**实际影响很小**：
`dsml_filter_applicable()`（:1216-1221）对 claude/opus/sonnet/haiku 直接返 false，
dsml_tail_buffer 在主力路径恒空，只有国产模型 + 带工具时才可能撞上。

### F1-4 — 新测试与注释的准确性

- `test_reclaim_still_works_when_thinking_enabled`（:4722）**是真回退守卫** [证据]。
  按 :1553 逐步推演：旧代码在 :2440 直接吐 `></invoke>`，随后 :2455 flush 到的只是
  未闭合半块 → 无 tool_use → `assert!(has_tool_use_block)` 失败。用
  `generate_final_events()` 而非直接 `flush_invoke_sniff_buffer()` 的理由**充分**：
  直接调 flush 会跳过 :2371-2443 整个 thinking_buffer 排空块，也就是缺陷本体所在。
- 但该测试的注释有事实错误：写「旧代码在 `if self.thinking_enabled { return
  process_content_with_thinking(..) }` 处提前返回，永远走不到下面的 reclaim 分支」。
  **不成立** [证据]：`emit_non_thinking_text`（:1667）在今晚之前就已存在，
  :1525/:1566/:1572/:1654 四处早已把 thinking 路径的文本喂进 sniff。真实缺陷范围窄得多
  ——只是收尾那一处残留。注释夸大了范围，会误导后来人以为整条路径都曾旁路。
- `test_reclaim_works_for_invoke_after_thinking_close_midstream`（:4753）**不是**本次
  改动的回退守卫 [证据]：它走 :1654（`thinking_extracted` 已置位分支），该分支今晚未改，
  把 :2440 改回 `create_text_delta_events` 它照样绿。作为 :1654 路径的覆盖有价值，
  但别当成本次改动的护栏。它注释里对 :2425 不可达的说明是对的（见 F1-2）。

### V3 — BufferedStreamContext 无独立缺口 [证据]

`BufferedStreamContext` 只是把 `inner: StreamContext`（:2555）的产出攒进
`event_buffer`，`finish_and_get_all_events` 仍调同一个 `generate_final_events`。
故本修复对缓冲模式同样生效，无第二套逻辑。（线上 `ccAutoBuffer=false`，该路径当前不启用。）

---

## 改动 2：准入超时独立标记

### V4 — 新分支位置正确 [证据]

`map_provider_error`（`handlers.rs:690`）里，新分支在 `let err_str = err.to_string();`
之后**第一条**，其前无任何 return。全仓带 `retry_after_secs=` 的 bail 共 6 处
（`provider.rs:822`、`token_manager.rs:3410/3425/3444/3496/3559`），全部落在
:729 那条全池冷却分支之后判定 → 新分支确实抢在所有之前。

### V5 — 源码级守卫的跨越式 needle 稳固 [证据]

`保护上游)inbound_admission_timeout=1` 在 `provider.rs` 中**只出现在 :822 的 bail
格式串**。:810/:818 的注释里虽提到 `inbound_admission_timeout=1`，但都不与
`保护上游)` 相邻（:810 是反引号包裹、:818 讲的是 `retry_after_secs=`）→ 删掉 bail 里的
标记，needle 必失配。守卫有效。唯一脆弱点：它把中文文案 `保护上游)` 焊进了断言，
改文案（即使保留标记）会误报 FAIL——属可接受代价，但值得在断言消息里写明。

### F2-1 — 另有一条同样不该被吸收的 bail 没处理（`token_manager.rs:3444`）🟠 中

```rust
"整池 RPM 已饱和，等待恢复超时（{}/{}）retry_after_secs={}"
```

这条语义上**也是网关侧背压**（RpmTracker 判定整池 RPM 饱和，等的是网关自己的滑窗），
不是「上游没准备好」。它现在仍落进 :729 全池冷却分支 → 渲染成
`temporarily cooling down` → shield 判 `cool` 并去重试。
你说「只处理了准入超时这一条」，实际漏的就是这条。

### F2-2 — 「全池禁用」被渲染成「冷却」，退避量级差 60 倍 🟠 中

`token_manager.rs:3496` 与 `:3559` 的「所有凭据均已禁用（0/N）」带的是固定
`POOL_EXHAUSTED_RETRY_AFTER_SECS = 10`（:1700，注释自承「未经控制实验」），
经 :729 同样渲染成 `All credentials are temporarily cooling down`。而 shield 注释
（也是你自己写的）明确记载换号空窗实测约 **10 分钟**。于是 shield 判 `cool`、
按 `Retry-After: 10` 起步——真值本身就是错的。目前只靠 `cool_delay` 的 1.6 倍升档
兜住，等于**用本地阶梯去纠正网关给的假真值**，与「听网关真值」的设计意图相悖。
根治要在 `map_provider_error` 给「全池禁用」单列文案（而非复用冷却文案）。

### F2-3 — 改动 2 在当前线上是死代码 [证据] 🟠 中

`ws-vps/config/opt/kirostudio/data/config.json` 实测：

```
inboundThrottleEnabled = False       ← throttle.acquire() 直接 Ok，永不排队
inboundQueueTimeoutPassthrough = False
inboundQueueMaxWaitSecs = 30
inboundTargetRpm = 760
```

`throttle.rs:176-178` 在 `!enabled` 时立即返回 Ok → **准入超时路径当前完全不可达**，
新分支线上零命中。改动本身是正确的前置铺垫（`Passthrough=false` 意味着一旦开启整形，
超时确实会走 Err → 新分支），但不要指望它现在改善任何线上现象。

顺带：`CLAUDE.md` 记的 `inboundThrottleEnabled=true` / 整形设在 133 **已过期**，
实际为 false / 760（大概率 `throttle-autotune` 改的）。文档漂移。

### F2-4 — shield 会把新文案判成普通 `retry`，放大照旧 🟠 中

新文案 `Gateway inbound rate shaping is at capacity ...` 不含任何
`COOLING_MARKERS` / `SWAP_WINDOW_MARKERS` / `PERMANENT_BODY_MARKERS` 命中项 →
`classify()` 落到最后一行 `return "retry" if status in RETRYABLE`（429 在
`RETRYABLE` 内）→ 走 `backoff_delay`，1~15s，**最多 60 次**。
每次重试都是整请求重打网关，而每一发都可能再在准入队列里阻塞至多
`inboundQueueMaxWaitSecs=30s` → 队列占用被显著放大，正是这次改动想避免的形态。
**结论：区分做到了 HTTP 层，但吸收层没接上，端到端行为未变。**
`shield` 侧应把该文案（建议匹配 `gateway-side backpressure`）加进
`PERMANENT_BODY_MARKERS` 或新设一个「立即透传」类别，判成 `pass`。

### F2-5 — OpenAI / Responses 路径丢 `Retry-After` [证据] 🟢 低（既存）

`openai/handlers.rs:296-311` 的 `translate_error_response` 只保留 status 与 body，
`openai_error()` 不复制任何头 → `/v1/chat/completions`、`/v1/responses` 上准入超时
与全池冷却**都拿不到 Retry-After**。既存缺陷，新分支同样受影响。

### F2-6 — 已知问题 #20 未被本次改动修掉 [证据] 🟢 低

`provider.rs:820-826` 的 bail 在 `for attempt` 循环**之前**，而 `fail_record` +
`emit_record` 在循环之后（:1463-1477）→ 准入超时仍**不产生任何用量记录、不 bump
任何计数器**，面板上依旧隐形。本次只解决了「客户端能否区分」，可观测性缺口原样保留。

---

## 改动 3：`kiro_shield.py`（`~/Documents/WorkSpace/ws-vps/config/opt/kirostudio/bin/`）

### F3-1 — `cool` 与 `swap` 共用 `swap_attempt`，把刚修的 bug 放了回来 🟠 高

`_proxy()` 里两种判决共用同一个 `swap_attempt` 计数器（`verdict in ("swap","cool")`
分支内 `swap_attempt += 1`），且**判决切换时不重置**。混合序列后果：

- **先 swap 后 cool**（号被封 → 补号 → 转入冷却）：`swap_attempt` 已被 swap 累到 4~5，
  `cool_delay(5, 10)` = `10 × 1.6⁴` = 65.5 → clamp 到 **60s**。
  网关明说 `Retry-After: 10`，实际睡 60s ——**正是本次要修的「把 Retry-After 丢掉、
  等真实恢复时间 6 倍」那个 bug，换个入口重现**。
- **先 cool 后 swap**：`swap_delay` 起点被 cool 抬高，首轮就接近 60s 上限。

修法：`cool_attempt` 与 `swap_attempt` 分开计数（各自独立升档），或在判决类别变化时归零。

### F3-2 — 共用 `swap_deadline` / `SWAP_BUDGET`，且放弃文案误导 🟡 中

`swap_deadline` 由先到的那个判决设定（`if swap_deadline is None`），之后 cool 与 swap
共吃同一份 900s。纯冷却场景耗尽预算时，返回的仍是
`"credential swap in progress, please retry"` 和日志 `swap window did not recover`
——把「池子在冷却」误报成「正在换号」，会直接误导排障方向。
`STATS` 侧也只有 `swap_gave_up`，没有 `cool_gave_up`，事后分不清是哪类耗尽的。

### F3-3 — 池子真空时 `cool_delay` 升档会「白转」，但方向是对的 🟡 中

真空（0/0）时网关恒回 `Retry-After: 10`。序列为 10 → 16 → 25.6 → 41 → 60 → 60…，
900s 预算约 17 次打完 → 503。**不会**无限空转，量级也合理（不是每 10s 打 90 次）。
代价是：从第 5 轮起就完全脱离网关真值，此后 `Retry-After` 事实上只在**首轮**被尊重。
这是 F2-2 那个假真值（固定 10s）逼出来的补偿，根因在网关侧而非 shield。

### F3-4 — 移走 `"All credentials"` 后，`SWAP_WINDOW_MARKERS` 三条全部失效 [证据] 🟡 中

逐条核实客户端**实际收到的 body**：

| marker | 是否还会命中 |
|---|---|
| `no available credential` | **全仓 0 命中**（`grep -rni` 遍 `src/` 无此串）→ 本就是死条目 |
| `TEMPORARILY_SUSPENDED` | `handlers.rs:555` 拦下后返回**中文**文案（:815-825），英文标记不出现 → 死 |
| `AccessDeniedException` | `map_provider_error` 末尾（:838-848）明确「原文只进日志不回客户端」→ 死 |

即 `map_provider_error` 会把所有上游原文改写成中文/固定英文文案，**Kiro 主路径上
swap 判决已不可能触发**，账号被封现在只会落 `cool` 或 `retry`（短退避），而 shield 注释
里那套「绝不能用限速那套 1 秒退避去打已被封的账号」的保护**实际已失效**。
唯一残存入口是 `passthrough.rs:130-133`（custom_api 透传原样回传上游 status/body）——
线上 `customApiFirst=false`，基本不走。
你问的「剩下的 `no available credential` 还有没有命中场景」：**没有，一次都没有。**
建议：要么给「全池禁用/被封」在 `map_provider_error` 里单列可识别文案（推荐，同时解决
F2-2），要么承认 swap 类别已死并删掉，别留着假保护。

### V6 — `Retry-After` 取值健壮性 OK [证据]

`resp_headers.get("Retry-After")` 走 `email.message.Message.get`，**大小写不敏感**
（内部 `lower()` 比较）→ 大小写无风险。`float()` 对 HTTP-date 抛 `ValueError`，
已被 `except (TypeError, ValueError)` 捕获并退到 `COOL_DELAY_FALLBACK`；负数/0 被
`max(MIN_DELAY, ...)` 兜住，超大值被 `min(..., SWAP_DELAY_MAX)` 截断。无缺口。
小瑕疵：`cool_delay` 首行 `base = MIN_DELAY` 是死赋值（两个分支都会覆盖）。

### F3-5 — 新准入文案 shield 该判什么 🟠 中（与 F2-4 同一件事）

- **现在判成**：`retry`（纯状态码兜底）→ 1~15s 退避 × 最多 60 次。
- **该判成**：`pass`（直接透传给客户端，让客户端自己退避）。理由就是网关文案自己写的
  ——`retrying immediately will not help`：重试只是把同一请求塞回同一个满桶，且每次
  重试可能再吃 30s 队列等待。
- 两侧配套才成立：KiroStudio 已给出可区分信号，shield 必须加对应 marker，否则
  改动 2 的收益为零。建议 marker 取 `gateway-side backpressure`（文案里唯一稳定短语，
  且与 cooling 文案无交集）。

### F3-6 — `request_queue_size = 1024` 与线程模型 🟢 低

`Server` 用 `ThreadingMixIn` + `daemon_threads`，**无线程上限**。把 accept 队列从 5
提到 1024 修掉了 RST（注释里 300 并发 61.3%→100% 的实测可信），但同时把「同时存活线程数」
的上限也放大到 1024 量级。当前 300 并发实测无碍，属放大既存风险而非新缺陷；
真要收口需换 `ThreadPoolExecutor` 或加 `Semaphore`。

---

## 可写「回退即 FAIL」测试的项

| 项 | 可测 | 说明 |
|---|---|---|
| F1-1（:2118 旁路） | ✅ | thinking=true + 尾巴含 `<invoke` + 真 toolUseEvent，断言 `reclaimed_invoke_count==1` |
| F1-2（:2404 死代码） | ⚠️ | 只能加 `debug_assert`；行为测试写不出（分支不可达） |
| F2-1（RPM 饱和文案） | ✅ | 与今晚那条同款：喂 `整池 RPM 已饱和…retry_after_secs=3`，断言 body 不含 `cooling down` |
| F2-2（全池禁用文案） | ✅ | 喂 `所有凭据均已禁用（0/3）retry_after_secs=10`，断言 body 与冷却文案不同 |
| F2-6（#20 无记录） | ✅ | 源码级守卫：断言准入 bail 之前存在 `emit_record` 调用 |
| F3-1（shield 计数器混用） | ✅ | pytest：先喂 swap 响应 3 次再喂 cool + `Retry-After: 10`，断言 sleep ≈10 而非 60 |
| F3-4（死 marker） | ✅ | 源码级：断言每条 `SWAP_WINDOW_MARKERS` 都能在 KiroStudio 客户端文案里找到 |
| F3-5（新文案判 pass） | ✅ | pytest：`classify(429, 新文案)` 应返 `"pass"` |

## 优先级建议

1. **F1-1**（:2118）—— 与今晚修的是同一缺陷，且生产常态可达，工具静默不执行。
2. **F2-4 / F3-5**（shield 加 marker）—— 不做则改动 2 端到端零收益。
3. **F3-1**（shield 计数器分离）—— 已修的 bug 会从混合序列重现。
4. **F2-2 / F3-4**（全池禁用单列文案）—— 一处修改同时解决假真值与死 marker。
5. F2-1、F1-4（注释纠偏）、F2-3（CLAUDE.md 漂移）、F1-2/F1-3/F2-5/F2-6 收尾。
