# 任务：把 429/瞬态错误的重试兜底做成**内置能力**，并收敛互相冲突的限流配置

> 这份是给下一个 AI 的任务提示词。**先读 `HANDOFF-2026-07-31.md` 第 0 节的六条硬约束再动代码。**
> 配套：`HANDOFF-NEXT-TASKS.md`（其余待办）。
>
> **2026-08-03 修订**：本文整体判断仍成立（"不要简单把 shield 抄进来"、"45s 闸门是刻意设计"、
> "先内置再切 Caddy"），但**四条前提已被实测证否/更正**，动手前必看：
> 1. §0 shield 强度：不是"平均 20 次"，是 **11.6 次重试救回 1 个**；且其中占 22.3% 流量的
>    403 风控误映射成 502 **主线已修**，这条上线后比值应自动改善 → **先观测再决定激进程度**。
> 2. §2 任务 A 位置：吸收层**不能放 `provider.rs`**（admission 与 45s 闸门都在
>    `call_api_with_retry` 内部），正解是三个 `handle_*`。
> 3. §2 任务 B："整形疑似被误改"是**错的** —— 是 `throttle-autotune` 在管；真问题是它算容量的
>    **口径本身是假的**。
> 4. §5 第 4/5 条：`live_snapshot` 的两个前提都错（每号 1 次 SHA256、`LiveCred` 有 7 字段）→ 别做；
>    `hotswap.sh` 已修且原文给的 `cp -a` 修法是错的（ETXTBSY）。

---

## 用户的原始诉求（原话转述）

1. KiroStudio 里有些东西是「垃圾」，该去掉。
2. 现在关于 429 的逻辑策略**互相冲突**，配置项太多，**对人类来说根本没法调**。
3. 重试兜底应该**内置到 KiroStudio**，而不是靠外挂脚本。
4. **不只服务 Cursor —— 所有客户端都要受益。**

---

## 0. 先看清现状：为什么会有外挂脚本

线上现在的真实链路是：

```
客户端 → Caddy → kiro_shield.py(:8993) → KiroStudio(:8990) → Kiro 上游
```

`/opt/kirostudio/bin/kiro_shield.py` 是**别人加的 Python 反向代理**（不在本仓库里），
`kiro-shield.service` 常驻，Caddyfile:485 已把流量指向它。它做的事：

- 收到 `RETRYABLE = {429, 500, 502, 503, 504}` 就**自己等待并重试**，成功才把响应交给客户端
- 不重试 4xx（401/403/400）——「那是配置错误，等 10 分钟也不会好」
- 超总预算（默认 600s / 60 次）返 **503 而不是 429**，因为 Cursor 对 429 会立刻掐会话
- 注释里明写重试安全的依据：**429 发生在流式响应开始之前，此时一个字节都还没发给客户端**

**它真实在工作，而且效率极差**（2026-08-03 累计计数器实测，已替换早先"平均 20 次"的目测）：

```
requests 22448 / retries 19226 / absorbed 1657 / gave_up 325
→ 11.6 次重试才救回 1 个请求，且 325 个最终还是没救回来
```

补充它的真实参数（原文缺）：`MIN_DELAY = 1.0` —— 号池 50ms 就能恢复它也睡满 1 秒；
backoff 有 Retry-After 时 clamp[1,15]，否则 `1.0 * 1.7^(n-1)` clamp 12。
**每次重试是整请求重打 KiroStudio**，而网关内部还有 12 次换号 → 真实放大上限 60×12。

它把「号池严重不足 + 上游风控」掩盖成了「有点慢」。

> 🔴 **11.6:1 这个比值里有很大一块是我们自己造的，已在 2026-08-03 修掉**：
> 上游 403 `Your User ID (...) temporarily is suspended` 占近 2h 流量的 **22.3%**，
> 原先被映射成 **502**。shield 见 502 只能走盲目指数退避，而这类风控窗口开合的实际恢复时刻
> 是可知的。主线已新增 `is_upstream_temporarily_suspended`，把它转成
> **429 + Retry-After: 20**（`UPSTREAM_SUSPENDED_RETRY_AFTER_SECS`）。
> shield 的 backoff 本来就读 Retry-After 并 clamp 到 15s，所以**这一条上线后 11.6:1 应自动改善**。
> ⚠️ 因此：**先观测这条修复的效果，再决定内置吸收层要多激进** —— 否则你会为一个已经变小的
> 问题过度设计。（该 403 是**突发**形态：13:50 一次 928 条、14:50 一次 516 条，中间为 0，
> 即风控窗口开合，**不是真封号**，所以重试是正确策略。）

**结论**：这个能力确实该内置 —— 外挂脚本只保护经过 Caddy 那一跳的客户端，
而且它在 KiroStudio 外面，拿不到号池状态、拿不到 Retry-After、无法与选号协同。

---

## 1. ⚠️ 但必须先理解一个**刻意的设计决定**，不要当成 bug 拆掉

`src/kiro/provider.rs:42-50` 明确解释了**为什么 429 目前刻意不在网关内重试**：

> 小号池下，一个卡住的请求会在每次重试时抢到刚出冷却的号、又打 429、又把它冷却，
> 如此在 `acquire_context` 的等待循环（最长 180s）× 多次重试之间反复横跳，
> **一个请求就能把整池长时间压死**（表现为「没有新入站却一直 429/繁忙」）。

配套的两道闸门：
- `MAX_REQUEST_RETRY_BUDGET_SECS = 45`（单请求重试墙钟预算）
- `ABSOLUTE_MAX_TOTAL_RETRIES = 12`（总重试次数硬顶）

注释还记了：叠加 sub2api 侧的 2 次重试 × 10 次账号切换，**单请求最坏放大到约 70~108 次上游调用**。

### 所以真正的问题不是「该不该重试」，而是「在哪一层重试、由谁决定退避」

现在是**三层各自重试且互不知情**：

| 层 | 策略 | 问题 |
|---|---|---|
| `kiro_shield`（外挂） | 429/5xx 死等重试，最多 600s | 在 KiroStudio 外，看不到号池状态 |
| KiroStudio provider | 换号 failover，45s 预算 / 12 次硬顶 | 刻意**不**对 429 做网关内重试 |
| sub2api（更上游） | 2 次重试 × 10 次账号切换 | 与上面两层叠乘 |

**你的任务是把这件事收敛成一层可解释的策略，而不是简单地"把 shield 的逻辑抄进来"。**
抄进来但不动既有闸门，会直接复现注释里描述的「一个请求压死整池」。

---

## 2. 具体任务

### 任务 A：内置「瞬态错误吸收」层（核心）

设计要求：

1. **位置（原文只说了"handlers.rs 入站侧"，这里给精确落点）**：实测入口拓扑已收敛，
   所有四条客户端路径最终都汇进**同样三个函数**：

   ```
   /v1/messages + /v1/chat/completions + /v1/responses → post_messages(handlers.rs:862)
   /cc/v1/messages                                     → post_messages_cc(handlers.rs:1914)
        └─ 两者都分派到 ↓ 这三个（吸收层就放这里）
           handle_stream_request(1087) / handle_stream_request_buffered(2102)
           / handle_non_stream_request(1446)
                └─ provider.call_api_stream(308) / call_api(299) → call_api_with_retry(716)
   ```

   ⚠️ **不要放 `provider.rs`**（原文第 3 条暗示的方向）—— 两个结构性障碍：
   - `acquire_admission()` 在 `provider.rs:783` 一带、**在 `call_api_with_retry` 内部**，
     注释明写「整个客户端请求只过一次(在 failover 循环外)」。在它内层重试等于重复过闸门。
   - `MAX_REQUEST_RETRY_BUDGET_SECS=45` 的闸门相对 `call_started`，**也在 `call_api_with_retry` 内**。
     在内层加重试会被这 45s 一起吃掉，等于没加。

   在三个 `handle_*` 上做，才既覆盖全部四条路径，又在两道闸门**外面**。
   **必须配一个源码级守卫测试**（断言这三个函数都接了吸收层），否则将来新增入口会静默漏覆盖。

2. **只吸收真正瞬态的**：`429` + `503`（号池空窗/全冷却）+ `500/502/504`（上游抖动）。
   **绝不吸收** 4xx（401/403/400/404）—— 配置错误重试无意义，且会掩盖真实故障。
   ⚠️ 我们自己的错误分类已经做得很细（见 `map_provider_error`），**复用它**，不要再写一套字符串匹配。

3. **重试安全性的边界必须守住**：只能在**尚未向客户端写出任何字节**时重试。
   流式响应一旦开始（`message_start` 已发出），就绝不能重试 —— 否则客户端会收到错乱内容。
   ⚠️ 这一点 shield 靠"429 在流式开始前返回"的观察保证；内置实现要**显式**判断，
   不能依赖巧合。**好消息（已核实）**：`call_api_with_retry`（`provider.rs:716` 一带）返回的是
   **未消费的** `(reqwest::Response, CallMeta)` —— 「读 body 之前」这个重试边界是**结构性成立**的，
   不是巧合。保留这个结构，吸收层就天然落在红线的安全侧。

4. **退避必须由号池状态驱动，而不是固定睡**。这是内置相比外挂的**最大价值**：
   - 号池全冷却时，`acquire_context` 已经知道最短恢复时间（`WaitOutcome::Wait(dur, reason)`）
   - 已有 `retry_after_secs=N` 协议（`map_provider_error` 会把它转成 429 + Retry-After 头）
   - **用真实的恢复时刻退避**，而不是 shield 那种盲等 + 固定下限 1s

5. **必须有总预算且预算耗尽后透传真实错误**，不要变成无限重试。
   建议复用/对齐既有的 `MAX_REQUEST_RETRY_BUDGET_SECS`，别再引入第二个不相干的时间常量。

6. **可观测**：吸收了多少次、每请求重试几次、预算耗尽多少次，都要能在面板看到。
   shield 的重试完全不可见 —— 它把病情藏起来了，内置版不能重复这个错误。
   **已核实的三个现存缺口（做吸收层之前先补，否则你无法验证自己的效果）**：
   - `RequestRecord.retries` 写入点 4 处（`handlers.rs:1093 / 1635 / 1724 / 2253`，全是
     `record.retries = meta.retries`）。**失败侧原先恒为 0**，2026-08-03 已修
     （`provider.rs` 加循环外 `attempts_used`）—— 不要重做这条。
   - 🔴 `Aggregate`（`usage_stats.rs:79-112`）**没有任何 retry 字段**，`add`（:114-137）不读
     `r.retries` → retries 只能逐条看，**画不出趋势**。前端同样只有 2 处逐条详情
     （`usage-page.tsx:785/823`、`ops-detail-dialogs.tsx:683`），无聚合视图；类型有两份独立定义
     （`api.ts:679`、`ops.ts:183`），加字段两边都要改。
   - 🔴 admission 超时（`provider.rs:783` bail）**既不 emit_record 也不 bump 计数器** → 被整形层
     掐掉的请求在面板上不存在。`recovery_metrics.rs:88` 的 `failover_exhausted` 也漏一类：
     bump 点被 `if real_failover_happened` 包着，墙钟预算 break 但只打过 1 个号的路径不计入。

7. **开关 + 默认值**：新增配置项要有 `#[serde(default)]`（线上 config.json 是既有文件，
   缺 default 会导致**加载失败 → 服务起不来**）。默认值要与「不开启此功能时行为不变」一致。

### 任务 B：收敛冲突的限流配置（用户抱怨的「没法调」）

现在有 **79 个配置项**（实测真值，原文写 86），其中与限流/退避/重试相关的至少 4 组：

| 组 | 配置项 | 线上值（2026-08-03 实测） | 问题 |
|---|---|---|---|
| 入站整形 | `inboundThrottleEnabled` / `inboundRpmAuto` / `inboundTargetRpm` / `inboundBurstSecs` / `inboundQueueMaxWaitSecs` / `inboundQueueTimeoutPassthrough` / `inboundRpmMin` / `inboundRpmMax` | **true** / **false** / **133** / 2 / 30 / true / 50 / 300 | ⚠️ **原文说"疑似误改"是错的**：整形是开着的，`inboundTargetRpm` 由 `throttle-autotune.timer` 每 2 分钟自动管（`inboundRpmAuto=false` 是刻意的，内置 AIMD 单向棘轮问题仍成立）。真问题见下面的 🔴 |
| 每号速率 | `rateLimitEnabled` / `rateLimitMinIntervalMs` / `rateLimitDailyMax` / `rateLimitJitterPct` | false / 1000 / 500 / 20 | 最小间隔 1000ms 会在 241ms 处踢开亲和绑定 → 每次换号 → prompt cache 全丢。**保持 false** |
| 冷却 | `cooldownEnabled` / `cooldownScalePct` | false / 10 | 死号自动禁用**已不依赖它** |
| RPM 硬门 | `credentialRpmLimit` / `rpmHeadroomFactor` / `rpmReserveSlots` / `rpmHardGateOverloadWait` | **200 / 85 / 3 / true** | 原文记的 `80 / 2 / false` 已过期。`overloadWait=true` 与代码默认相反 |

其他相关真值：`affinityEnabled=true`、`ccAutoBuffer=true`、`promptCacheEnabled=true`、
`promptCacheTtlSeconds=3600`、`overloadFallbackModel=None`、`ingressRateLimitPerMin=0`。

### 🔴 用户抱怨"没法调"的真正根因：一个关键数字是假的

`throttle-autotune` 日志：「target=133 已接近建议值 133（池容量 **167**，可用 1 个）」。
那个 167 = `credentialRpmLimit` 200 × `rpmHeadroomFactor` 85%，**是配置自乘出来的数，不是测出来的**。

而实测：**单号 RPM 峰值 144，其中 17.2% 是 rate_limited**。文档里"干净吞吐 25~30"指的是
「429 率为 0 时的 RPM」，和峰值是两个口径，不能混用（原文第 195 条把它当同一口径了）。

推论链：整形阈值 133 < 实测峰值 144 → **整形层实际什么都没限住**，真瓶颈是上游 429；
而所有依赖 167 的自动调节**都在算空气**。所以问题不是"选项太多人类调不动"，
而是**调了也没用，因为反馈信号是假的**。

⚠️ **不要按 25~30 直接改 `credentialRpmLimit`**（会把吞吐掐死一个数量级，
而用户明确说「不能非常非常保守，那都运行不好，我这个就是大型中转」）。
正确顺序是：先让容量口径变成**实测驱动**（用真实 429 率反推有效容量），再谈档位收敛。

**近 2h outcome 基线**（做任何调参前先记住它，改完要能对比）：
success 59.5% / **auth_failed 22.3%**（即上面那条 403，已修）/ rate_limited 17.2% /
server_error 0.8% / bad_request 0.2%；avg latency 4416ms / avg TTFB 6916ms；**号池只剩 1 个号**。

**要做的**：

1. ~~先查清「整形为何被打开」~~ **已查清：不是误改，是 `throttle-autotune.timer` 在管。**
   这一步不用做了。要做的是上面那条：**把容量口径从"配置自乘"改成"实测驱动"**。
2. **给出一个「推荐配置档」**：把这 4 组收敛成 2~3 个人类能理解的档位
   （比如「小号池保守」/「大号池吞吐优先」），底层仍是那些字段，但面板上只暴露档位。
   这直接解决「对人类来说太难调」。
3. **标注哪些配置项是历史包袱可以废弃**（用户说的「垃圾东西」）。
   ⚠️ 判据必须是「有证据表明它无效或有害」，不是「我觉得多余」。
   已知候选：`promptCacheEnabled`（曾是死配置，本轮已接上读取点，需确认现在真的有用）。
   废弃要走 deprecated 而不是直接删 —— 线上 config.json 有这些 key，删了要保证仍能加载。

### 任务 C：内置之后，shield 怎么处理

**不要擅自删掉 `kiro_shield.py` 或停它的服务** —— 那是别人加的、正在保护生产的东西。

正确顺序：
1. 内置版做完并上线
2. 观察内置版的重试统计，确认它确实吸收了同等强度的瞬态错误
3. 把 Caddyfile 从 `:8993`（shield）切回 `:8990`（KiroStudio 直连）
4. 观察一段时间，确认客户端体验不降级
5. 才停 shield 服务

**每一步都要能独立回滚。** 切 Caddy 那一步尤其要注意：改完 `caddy reload` 而不是 restart。

---

## 3. 硬约束（违反会造成真实损失）

1. **工作树有 71 个其它会话的未提交改动**（2026-08-03 计数）。禁止 `git checkout`/`switch`/`stash`/`reset`/`commit`/`add`、
   禁止全仓 `cargo fmt`。备份只用 `cp` 到 `/tmp`。
   ⚠️ 上一会话用 `git checkout-index` 毁掉别人 **515 行**未提交代码（已恢复，但若没推过就永久丢了）。
2. **只用 Edit 工具改代码。** 禁止 `sed`/`python` 批量替换 Rust
   —— 上一会话用 `sed` 破坏过大括号（模式里含 `|`），历史上还有 python 替换删掉三个函数整段、
   正则把字段插进 `impl` 块造成 209 个编译错误。
3. **每条改动都要有能抓住旧 bug 的回归测试。** 验证方法：把修复处改回旧行为 → 跑测试 →
   **必须 FAILED** → 再还原。**测试"通过"不等于它测到了东西**
   （上一会话有个测试因漏了 struct 级 `#[serde(rename_all)]` 而"通过"，其实什么都没测）。
4. **不做没有证据支撑的改动。** 判据是「能否构造出'移除它即失败'的测试」。
5. **配置项新增必须带 `#[serde(default)]`**，否则线上既有 config.json 加载失败 → 服务起不来。
6. **不要在 VPS 上编译**（4 核会抢死正在服务的 sub2api），走 GitHub Actions。

---

## 4. 验证与上线

```bash
cd admin-ui && pnpm install --frozen-lockfile && pnpm build && cd ..   # rust-embed 编译期需要 dist
cargo test --no-default-features --bin kirostudio      # 当前 982 全绿（原文写 969，已过期）
cargo clippy --no-default-features --bin kirostudio    # 0 error
cd admin-ui && npx tsc --noEmit
```

上线流程（plumbing 快照 + CI + 零空窗 hotswap + 真实流量验证）见
`HANDOFF-NEXT-TASKS.md` 第 3 节，那套我实测走通过，照做即可。

**本任务特有的验证点**：
- 造真实 429（可临时把 `credentialRpmLimit` 调到极低制造饱和），确认内置层吸收且客户端不见 429
- 确认**流式响应已开始后不再重试**（这是正确性红线）
- 面板能看到重试统计（不能像 shield 那样把病情藏起来）
- **四条客户端路径逐个验**：`/v1/messages`、`/v1/chat/completions`、`/v1/responses`、`/cc/v1/*`

---

## 5. 我认为还值得做的（按性价比，非本任务必需）

### 高价值

1. **号池自动补号 / 容量告警**（🔴 当前最痛）。号只剩 1 个时 18:00 那 10 分钟
   **1729 请求只有 1 个成功** —— shield 重试、自愈、cli 端点全都救不了「没号」。
   最小可做：号数低于阈值时发告警（Postfix 已配好，`ws-vps/secrets/aliyun-smtp.env`）。
2. **`credentialRpmLimit` 回归实测值**。现在 200，虚高会让选号以为还有余量、继续往饱和的号上压。
   ⚠️ 但**不是**降到 25~30：那是"429 率为 0 时的 RPM"，而实测峰值 144。两个口径别混。
   详见任务 B 的 🔴 那节。改前必须做控制实验。
3. **TTFB 长尾归因**。埋点已上线并有值：**p50 = 3.7s 但均值 7.8s**，说明有长尾把均值拉高一倍。
   查那批是什么（大 context？failover 重试链？）—— 这是修复前根本看不到的新信息。

### 中价值

4. ~~**瘦 `live_snapshot`**~~ **❌ 原文两个前提都错，别做**：核实后 `snapshot()` 每号只算
   **1 次** SHA256（两个分支由同一个 `is_api_key_credential()` 判定、互斥，不会都走）；
   `LiveCred` 也不是"只用 id/rpm/inflight"，它有 **7** 个字段
   （id / rpm / inflight / cooling_down / cooldown_remaining_ms / circuit_open / health_score）。
   收益远小于原文估计，而它碰的是选号侧数据结构。
5. ~~**`hotswap.sh` 两个真 bug**~~ **✅ 2026-08-03 已修，别重做。**
   ⚠️ 并且原文给的修法是错的：`cp -a` 对**正在运行**的二进制会报 **ETXTBSY**。
   实际改用 `install`（它先 unlink 再写，绕开 ETXTBSY）。`trap` 也已加。
6. **关机固定睡 8 秒**（`main.rs:~704` 的 `shutdown_with_drain_cap()` = 等信号 →
   `sleep(SHUTDOWN_DRAIN_CAP_SECS=8)` → 返回，是裸 `sleep` 不是 `select!`）——
   即使在途请求早已 drain 完也白等 8 秒。改成竞速取先到者。
   ⚠️ 顺带修注释：`~702-703` 与 `~714` 承诺"未 drain 完的连接被断开"，**代码里不存在这个行为**
   （`serve().await` 无上限）。注释比睡 8 秒更危险 —— 它让人以为有超时保护。

### 需要产品判断

7. **`defaultEndpoint` 现在是 `ide`**（429 高的那个）。对 `ksk_` 号无影响（自动路由到 `cli`），
   但将来加 social/idc 号会走 `ide`。要不要改全局默认，取决于那些号类型在
   `q.{region}.amazonaws.com` 上能否认证 —— **没验过，不要拍脑袋改**。
8. **OTA 的 PAT 是 `gho_` 型**（`gh auth token`），用户下次 `gh auth login` 会让它轮换 → OTA 失效。
   换成 fine-grained PAT（只勾 `dwgx/KiroStudio-skiapi`，Contents: read）。
9. **0.7.46 还没打 tag** —— OTA 升不到这一版（Cargo.toml 已是 0.7.46，仍未 tag）。
   在此之前用 `kirostudio-update` 脚本，功能等价。

### 明确不要做（已有证据，别重做）

- `busy_timeout`：rusqlite **已无条件默认设 5000ms**（`inner_connection.rs:118`），
  且 `TraceDb` 是单连接单 Mutex，SQLite 层压根不争锁 —— 该 PRAGMA 是死代码
- `spawn_blocking` 包 trace_db 查询：真正的争抢在那把 `parking_lot::Mutex` 上，换线程占锁无用
- 上游 `hank9999/kiro.rs` 逐条对照：28 条 fix **零可修项**，我们还有 4 处比它更严
- `p_avail` 批量化：无实测支撑 + 碰选号热路径 + 行为不可观测**故构造不出"移除即失败"的测试**
- `ccAutoBuffer` 往任一方向改：两条实测证据链**互相矛盾**，**需用户拍板**，不要自行决定

### 已修，不要重做（2026-08-03 主线）

1. 号池真耗尽 bail 带 `retry_after_secs`（`token_manager.rs` 两处）+3 测试
2. **403 `temporarily suspended` → 429 + Retry-After: 20** +2 测试 ← 与本任务关系最大
3. `fail_record.retries = attempts_used`（`provider.rs` 循环外计数器）+ 源码级守卫测试
4. `compute_max_retries` doc 注释改对 + 定时炸弹测试重写
5. sort key 里 `inflight` 双读收口成单读
6. 两处注释漂移（排序键概览 6→10 项；已删除的 jitter 键）
7. `hotswap.sh`（见上面第 5 条）
8. 凭据多开：`copies` / `add_credential_allowing_duplicate` / `MAX_CREDENTIAL_COPIES=16` +6 测试

---

## 6. 三条最容易踩的坑（上一会话都踩了）

1. **测试"通过"不代表它执行到了被测路径。** 唯一可靠的验证是「回退修复 → 必须 FAILED」。
2. **同一前提失败两次就停下来量，不要继续改测试。** G1 元测试失败三次才对，
   前两次都是「机制推理正确但复现方式错」；第三次写诊断脚本把两种实现**并排实测**才找到真因。
3. **fan-out subagent 时注意**：agent 调用走的正是 `k1ro.skiapi.dev`（我们自己的网关），
   号池紧张时 8 路并行必然打穿它 —— 上一轮 fleet 被 502 打死 **5 次**。
   建议 agent 只做只读侦察、并发 ≤3，写代码由主线串行做。
