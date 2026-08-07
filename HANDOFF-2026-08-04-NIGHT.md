<!-- HISTORICAL-ARCHIVE-MARK -->
> ⚠️ **这是过程记录，不是当前状态。** 当前状态看仓根 `STATUS.md`（唯一真相源）。
>
> 本文件已确证含**过期断言**。历史上多次出现「后来的会话把过期断言当约束」而做出错误决策
> ——最严重的一次：一句无依据的「`q.*` 已停用」注释直接导致了一次错误的架构迁移，
> 改坏 region 探测让 US 号恒 403，上线后才发现并回滚。
>
> 读本文件时：**任何数字（测试数 / 配置值 / 池容量 / 行号）一律现读现验**，
> 结论性断言先按 `STATUS.md` 核一遍。本文件的价值在于**依据与推导过程**，不在于它的结论。

---

# KiroStudio 交接 — 2026-08-04 夜班

> 接续 `HANDOFF-2026-08-04.md`。**先读第 1、2 节。**
> 第 2 节是本轮最重要的结论：**HTTP 429 基本不存在，真实信号是 403 suspended。**
> 硬约束（工作树有其它会话改动、禁 git 写操作、禁全仓 fmt、只用 Edit 改 Rust、
> 每条改动都要「回退即 FAILED」验证）全部沿用上一份第 0 节。

---

## 1. 本轮上线内容

两批，都走 `origin/deploy/vps` + CI + `hotswap.sh`。

### 批 A（`6a79316`，binary `f77760a2a73b36ef`，已验证 1 实例 / 0 panic / 0 禁用）

五条修复针对同一条链：**瞬态错误被计成永久失败 → 号被误禁 → 池掉到 0~1 个 →
剩下的号吃全部流量 → 撞进上游惩罚窗口。**

| # | 改动 | 旧行为为什么烧号 |
|---|---|---|
| 1 | 刷新失败按错误类型分流（新增 `is_refresh_error_credential_level` + `report_refresh_failure_classified`） | `refresh_token_locked` 内部**已经**对 5xx/网络退避重试 3 次才上报一次，所以上报里上游抖动占绝大多数。旧代码无条件计数 ⇒ 一次 30s token 端点抖动 = 3 次计数 = 永久禁用 + 落盘。**429 明确判瞬态**（限流不是号坏） |
| 2 | `TooManyRefreshFailures` 纳入 `is_self_healable_reason` | 它与 `InvalidRefreshToken` 是两个信号，此前一起被排除 ⇒ 全池被它打满后**自愈也救不回来**，必须人工去面板点启用 |
| 3 | `acquire_context_excluding`（排除集） | 此前唯一阻止 failover 重选刚失败号的是 `is_entry_selectable` 里的冷却硬门 ⇒ **`cooldownEnabled=false` 时 failover 事实上不存在**。现在是结构保证 |
| 4 | 停机 `flush_stats_now()` + 首号首次成功绕过 debounce 落盘 | `has_ever_succeeded` 读的是从 stats 恢复的 `success_count`，而它是 provider 判「bearer-invalid 403 = 瞬态 or 真 region 错配」的唯一判据。今日 **41 次 SIGTERM 里 39 次走到 SIGKILL** ⇒ 成功记录丢失 ⇒ 重启后健康号被判「从未成功过」⇒ 三次瞬态 403 即禁用。实测 20:20:30 启动、**20:20:32 打死 93.9% 成功率的 #483** |
| 5 | `region_probe` 接线（`add_credential` + 启动后台串行回填） | 模块早写好、8 测试全绿，但**从未被任何代码调用** |

### 批 B（`779f6f9`）— `autoDisableSuspicious` 补齐接线

该字段此前**只存在于 `config.json` 与 `TokenManager`**：`reload_config` 确实在读它，
但 `admin/types.rs` 无响应字段、无请求字段，`service.rs` 无 TIER1 更新分支。

**后果**：配置 API 读回 `None`，面板既看不到也改不动，只能手改文件 + 重启。
线上因此得出过错误结论 —— 以为「三个自动禁用开关被直连 API 关掉了」，而这一项**改不到**。

补 5 处（types 两处 / service 两处 / 前端三处 + i18n 三语各 2 键，1587 键零差集）。

---

## 2. 🔴 429 的真相：它基本不存在

同一 30 分钟窗口，两边**各自的** DB，按 upstream attempt 口径：

| | 成功 | 403 suspended | **HTTP 429** | 总 attempts |
|---|---|---|---|---|
| kiro-rs（`/opt/kiro-rs-test/run/traces.db` 的 `trace_attempts`） | 540 | 1695 | **0** | 2247 |
| KiroStudio | 605 | 1239 | **0** | 2083 |

**24% vs 29% —— 我们略优。**

「kiro-rs 不限速 / 180 RPM 干净」**字面为真但完全误导**：它同期错误率 **69%**
（1737 `auth_failed` + 905 `所有凭据均已禁用（0/3）` + 8 `quota_exhausted`）。
两边**不是限速多少的差异，是记账口径的差异** —— 同一个上游 403，
kiro-rs 记 `auth_failed`，KiroStudio 记 `rate_limited`。
那次 180 RPM 测量是在账号进入 suspend 窗口**之前**做的。

### 唯一站得住的机制：状态型惩罚窗口

同一个号 #481：

```
20:23   96 attempts / 94 ok / 0 429
21:12   90 attempts / 90 429 / 0 ok      ← 同速率，相反结果
```

**不要再用 429 率当信号去调限速。** 这与 `throttle-autotune` 脚本开头的注释一致。

### 我在本轮证伪的五个机制（别再重复）

| 假设 | 为什么错 |
|---|---|
| 429 随请求体增大 | **`input_tokens` 来自上游 usage，429 请求永远拿不到 ⇒ 17550/17550 条 `rate_limited` 的 `input_tokens=0`。** 那张 size-bucket 表只是在区分"失败/成功"，请求大小**无法从该表测量** |
| 并发数决定 429 | 429 行平均在飞 6.45、成功行 3.51，看着很强。但只按**成功**请求算负载后完全反过来：0 个成功在飞时 429 率 81.6%、5–8 个时 3.4%。那个量测的是「该号是否正处于惩罚窗口」，**是结果不是原因** |
| token 量/分钟有天花板 | #481 单分钟 **2270 万** input tokens 零 429；干净分钟均值 990 万 |
| region 决定 429 | 上一份已证伪（控制时间后 eu 号同样 55–62%）。**region 决定 403，不决定 429** |
| 端点（IDE vs CLI）决定 429 | 上一份已证伪，两个都干净 |

---

## 3. `cooldownEnabled=false` 现在可不可以

**基本可以** —— 排除集（批 A #3）已经把最致命那条（下一跳重选刚失败的号）变成结构保证。

但冷却还有三件事排除集覆盖不到（**排除集每请求重置，冷却跨请求**）：

1. 下一个客户端请求仍会先撞它一次 → 白打一次上游拿 403 才 failover。
2. 上游 `Retry-After` 被整个丢弃（`report_rate_limited_with_retry_after` 全函数门控在 `cooldown_enabled`）。
3. 族级熔断（M365 同租户连坐）不生效（同一门控）。

### ⚠️ 危险组合：`autoDisableSuspicious=true` + `cooldownEnabled=false`

`report_suspicious_activity` 里**计数与自动禁用是恒执行的**（注释明写不受冷却/限速
开关影响），只有冷却本身与族级连坐受门控。所以关掉冷却会让自动禁用**更容易**触发：
更多请求打到已被 suspend 的号 → 403 攒得更快 → 更快撞 6 次阈值。

线上现在 `autoDisableSuspicious=false`，组合不成立。**但要开自动禁用就必须同时开冷却。**

---

## 4. 与 kiro-rs v0.7.1 的逐条对比（`~/Documents/Project/_study/kiro-rs`）

VPS 上那份是 `version 0.7.1`，与 upstream master（`2026.3.1`）不同版本；
upstream 用 rust-embed 内嵌面板，VPS 那份是 `src/admin_ui/` + 独立 `admin-ui/dist`。

### 他们对 403 的处理**严格比我们差 —— 别照搬**

`provider.rs:620` 的 `401 | 403` 分支直接 `report_failure`（3 次即禁用），而账号风控
分支在它**之后**且门控在 `status == 429`；`default_account_throttle_kind`
（`endpoint/mod.rs:186`）只认 `suspicious activity`+`temporary limits` 或
`ThrottlingException`/`USER_REQUEST_RATE_EXCEEDED`/`Too many requests` ——
**不认 `temporarily is suspended`**。

于是线上占主导的那 3749 次 403 全部落进凭据失败路径，这正是他们 DB 里
1737 `auth_failed` + 905 全池禁用的来源。**那是我们上一批刚修掉的缺陷。**

| | KiroStudio（现在） | kiro-rs v0.7.1 |
|---|---|---|
| 403 `temporarily is suspended` | 429 + `Retry-After` + 冷却，**不计失败** | `report_failure`，3 次即禁用 |
| bearer-invalid 403（已成功过的号） | 判瞬态，只冷却 | `report_failure` |
| 风控禁用阈值 | 连续 **6** 次且期间零成功 | 任何 403 计 1，共 **3** 次 |
| 池空错误 | 429 + `retry_after_secs` + 可自愈标记 | `所有凭据均已禁用（0/N）` |

### 值得抄的三样

1. **分级冷却**：`RATE_LIMIT_COOLDOWN_SECS = 60` 给速率类，suspicious 走配置的长冷却。
   他们注释写明理由：「刻意不复用 suspicious 的 300s —— 否则 8 个号一起被限时会全部
   长冷却、直接 503」。我们的 `RateLimitExceeded` 基线 15s 会被 `cooldownScalePct`
   缩放（线上 40% ⇒ 6s），**应该给它一个不受缩放影响的下限**。
2. **429 专用退避**：`retry_delay_throttle` 上限 8s，与通用退避（上限 2s）分开。
3. 他们的重试预算保守（`MAX_TOTAL_RETRIES=4` vs 我们 `ABSOLUTE_MAX_TOTAL_RETRIES=12`）。
   **但我们池子大得多，照搬会掐死吞吐 —— 需实测，别直接改。**

---

## 4.5 🔴 429 的真正机制：速率的**跃升**，不是绝对吞吐

这一节是全文最重要的实测结论，晚班后半段才找到。

### 判据与数据

按「凭据 × 分钟」配对，控制**前一分钟完全无 429**（这一步排除了「429 快返回 +
shield 重试 ⇒ 本分钟计数虚高」这个反向因果）：

| 本分钟 / 前一分钟 | 429 率 |
|---|---|
| **≥5x 跃升** | **48.3%** |
| 2–5x | 5.4% |
| **平稳（0.5–2x）** | **0.7%** |
| 下降 | 1.7% |

**69 倍差距。** 与绝对速率交叉制表后，每一档绝对速率内跃升都是主因：

| 绝对速率 | 5x+ 跃升 | 平缓 |
|---|---|---|
| <50 req/min | **36.4%** | 1.3% |
| 50–99 | **51.2%** | 0.5% |
| **100+** | **57.1%** | **2.9%** |

即 **100+ req/min 平缓上量只有 2.9%，而 <50 req/min 突然跃升有 36.4%**。
上游那是 slew-rate 限制，不是吞吐上限。

### 它一次性解释掉之前所有矛盾观测

- **用户报「同号 kiro-rs 180 RPM 干净、KiroStudio 就限速」** —— 那是逐步加压的
  压测，全程落在「平稳」档。
- **#481** 同样 ~90 req/min，20:23 是 0% 429、21:12 是 100% —— 差别在**上一分钟是多少**。
- **#507** 在 1~5 req/min 下跑了 19 分钟、100% 成功；23:08 因池里另一个号掉出而
  瞬间承接 192 req/min，当场 187 个 429。
- **`credentialRpmLimit` 永不触发** —— 它盯的是绝对速率这个错的变量。
- **新号一入池就被打爆** —— 它瞬间承接全部流量。

### ⚠️ 我在这一节之前先错了一次（别重复）

我曾报「调度器把 15× 流量发给 97% 失败的号（507），而 0% 失败的（508）只拿 42 次」。
**不成立** —— 逐分钟拆开看，507 当时是池里**唯一**的号；508 一入池调度器立刻切过去、
507 归零。健康降权本来就在工作。我读的是**跨池子变动的累计数**，这是同一个混淆
今晚第六次咬到我。

但顺着那个错误查下去找到了一个真缺陷（见下）。

---

## 4.6 排序键里的正反馈：失败让号在排序里**变好看**

**429 在 ~1s 返回、成功要 3s+**（实测 507 的 429 平均 1062ms / 508 的成功 3264ms）。
所以正在被打爆的号 `inflight` 反而恒低 —— 实测 **507（97% 429）inflight=1、
508（0% 429）inflight=13**。而 `inflight` 是**升序**排序键 ⇒ 失败越快 → 越显得
空闲 → 越被优先选中。

这与 ZyphrZero v0.7.1 打的补丁是**同一类缺陷**（他们那条是 `success_count`：
被限流的号从没成功过、恒为 0，反而成了全场「最少使用」，注释原话"劫贫济富"）。

**修法**：排序键新增 `ramp_tier`，插在 `health_tier` 之后、`inflight` 之前。
两个位置都有源码守卫钉着（挪到 inflight 之后即 FAILED）。

---

## 4.7 本轮上线的第三、四、五批

| 批 | binary | 内容 |
|---|---|---|
| C | `230882edbd353a54` | 按号爬坡限制（`ramp_tier` + `RpmTracker::ramp_counts_for`） |
| D | — | 限流冷却缩放下限（`scale_floor`：RateLimit 8s / Suspicious 12s） |
| E | `a6f7f5fde259c50a` | REST 端点 US/EU 双候选回退 + 启动 slow-start（D 与 E 一起上线） |

### 爬坡限制的**已验证**行为与**已知缺口**

✅ **多号可用时确实生效**（唯一一次真实观测）：#509 于 23:42 入池，
`2 → 2 → 1 → 6 → 18`，五分钟逐步加量，全程 0 个 429。

❌ **单号可用时无事可做**：23:47 #508 掉出，509 瞬间从 18 跳到 64（3.5x），
当场开始 429，随后到 66/68 时是 37/46 个 429。

这是**刻意的**：它是排序键而非硬门。全池只剩一个号时照常放行，因为拿一个真实 429
好过网关自造 503。用户已明确选择「不加硬限，只加重启后的 slow-start」。

### 启动 slow-start 为什么必要

`RpmTracker` 是**纯内存**的 ⇒ 每次重启爬坡历史清零 ⇒ 重启后每个号 `total=0`
落到「样本不足不判」分支 ⇒ **选号层爬坡在重启后第一分钟完全不设防**。
线上 20:00 起实测 **23 次重启 / 27 次热重载**（用户确认是他自己换号的脚本）。

`boot_ramp_rpm`：有效 RPM 从 25% 线性升到 100%，历时 60s（与 `RpmTracker` 滑窗
对齐，窗口结束那刻选号层刚好攒够一整窗样本，两层无缝接力）。是**读时乘数**，
不写 `target_rpm`，故 AIMD 状态不受影响。

⚠️ **上线后未被验证**：部署时流量已降到 21~39 req/min，而 25% × `inboundTargetRpm=300`
= 75 RPM 高于它 ⇒ slow-start 结构上不会触发（排队日志 0 行）。
**要验证得等一次高流量时段的重启。**

### REST 端点 US/EU 回退（学 ZyphrZero）

实测 `management.{region}.kiro.dev` 与 `runtime.*` **只在 us-east-1 与
eu-central-1 解析**，其余 13 区 DNS 不通。而我们的 `get_usage_limits` 只取
`effective_upstream_region` 一个值、**无任何回退** ⇒ SSO region 是别的区的账号
（Enterprise / IdC 常见）必然 403，且上游文案是 `Invalid token` —— 会让人误判成
token 坏了。

`rest_api_region_candidates`：按前缀选主端点 + 另一个作 403 回退。
**只对 403 回退**（401 是 token 真废、429 是限流，换端点无意义，回退只会把失败的
上游往返翻倍）。

同时把 `region_probe::PROBE_ORDER` 从 5 项收窄到 2 项，`MAX_PROBE_ATTEMPTS`
从写死 3 改为 `PROBE_ORDER.len()`。⚠️ **注意这里不变量方向反转了**：原断言是
「上限 < 表长」（意图：别探满），表收窄后那个意图会变成「永远探不到第二个候选」
⇒ eu 账号拿不到回退机会。测试注释里写明了这次反转的理由。

---

## 5. 待办

### 5.1 ✅ 分级冷却下限（已完成，批 D）

`scale_floor`：`RateLimitExceeded` 8s / `SuspiciousActivity` 12s，不受
`cooldownScalePct` 影响；其余原因（ServerError / ModelUnavailable /
TokenRefreshFailed）仍可被任意缩短（它们是本地可判定的瞬态）。

取 8s 而非对照实现的 60s：我们池子只有 1~3 个号，而排除集已保证单请求内换号
不依赖冷却，冷却只需承担「跨请求别立刻回头撞同一个惩罚窗口」。
60s × 3 个号 = 全池同时冷却 → 网关自造 503，正是对照实现注释里警告的形态。

### 5.2 🟠 爬坡硬限（用户已决定**不做**）

单号可用时爬坡限制无事可做（见 4.7）。要补需要「超出爬坡额度就在入站返 429 +
Retry-After」，是吞吐换成功率的取舍。**用户已明确选择只加重启 slow-start、
不加硬限。** 若将来要做，shield 本来就在吸收 429（4.32x 放大、客户端成功率
98.9%），重试回来时该号已经爬上去了，代价主要是延迟。

### 5.3 🟡 仍未验证的两项

1. **启动 slow-start** 上线时流量已降到 39 req/min < 触发线 75 RPM，
   排队日志 0 行 ⇒ 需等一次高流量时段的重启才能验证。
2. **爬坡限制在单号→多号切换时的表现**：唯一一次观测（#509）证明多号可用时有效，
   但那次之后没再遇到同样的池子变动。

### 5.2 🔴 shield 及三个 unit 不在任何仓库

`kiro_shield.py`（612 行）+ `kiro-shield.service` + `socks-rotator.service` +
`kiro-ratelimit-monitor.service` 全部只在 VPS 上 untracked。
**按现状 clone 重建机器不会有 shield，客户端会直接吃到全部 403。**
它今日 4896 resolved / 21156 attempts（放大 **4.32×**），最近一小时
933 wait / 278 resolved / 3 gave up ⇒ **客户端侧成功率 98.9%** —— 它是目前唯一
在吸收 403 的东西。**不要停它。** 应纳入 ws-vps 仓库。

### 5.3 🟡 已知问题 #20 / #21 仍未修

admission 超时在面板隐形；`retries` 无法聚合。见 `CLAUDE.md`。

### 5.4 需要用户拍板

`inboundQueueTimeoutPassthrough=true` ⇒ 排队后**放行**，整形层 24h 拒绝 0 次。
改 false 是吞吐换成功率。

---

## 6. 线上状态与风险

- **配置有人在抢**：`cooldownEnabled` 今晚被改回 `false` **两次**（20:35、21:5x）；
  21:55 / 21:57 / 22:05 三次热重载不是我做的；22:00:55 有一次外部 `systemctl restart`
  （重启后只加载 2 个凭据）。**21:52–22:04 的池空是池子被外部换掉，不是代码改动。**
  动配置前先确认归谁管。
- **号池已换**：现在是 `#503`（`custom_api` → `rs.skiapi.dev`，**就是 kiro-rs**）
  和 `#505`（`ksk_`，`endpoint: cli` pinned）。**早先基于 #481–483 的分析对应的池子已不存在。**
- ⚠️ **`#503` 返回 200 只代表 kiro-rs 最终成功了** —— 它内部重试吸收的 403
  在 KiroStudio 的 traces 里**看不见**，与 shield 是同一个观测盲区。
  判断真实上游健康只能查 `/opt/kiro-rs-test/run/traces.db` 的 `trace_attempts`。
- **我起的 5 个侦察 agent 全部在启动时就死了**（146 字节占位、零进展）。
  上一份第 7 节警告过：agent 走的正是生产网关。**本轮所有工作都是直接做的，无一来自 agent。**

---

## 7. 验证方法

```bash
cd admin-ui && pnpm install --frozen-lockfile && pnpm build && cd ..   # rust-embed 编译期嵌入，必须先跑
cargo test --no-default-features --bin kirostudio      # 本轮 1093 全绿
cargo clippy --no-default-features --bin kirostudio    # 0 error
cd admin-ui && npx tsc --noEmit                        # 干净
```

**每条改动都做过「改回旧行为 → 测试必须 FAILED → 再还原」验证。**
这个流程本轮抓出我自己**两个纸面测试**：

1. 排除集的行为测试 —— 两道串联 filter 互相掩护，删掉任一条都仍绿。
   已拆成「关亲和隔离 filter」+「亲和旁路改源码守卫」，并在注释里写明为什么
   后者只能是源码守卫（`available` 已被前一道 filter 剔过，`find` 必然 None）。
2. 首次成功落盘 —— 没预热 debounce 时钟，而 `last_stats_save_at=None` 时
   `save_stats_debounced` 本来就会立刻落盘 ⇒ 删掉修复也绿。已加预热。
