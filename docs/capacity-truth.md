# 容量口径的真相（2026-08-04 只读实测）

数据源：线上 `/opt/kirostudio/data/usage/traces.db`（269668 行，07-28→08-03）、
`config.json` 及其 23 个备份、`/_shield/stats`、VPS 网络实测。**未改任何源码/配置/线上状态。**

## 0. 先看这个：38% 的 "rate_limited" 里有 53% 根本不是限流

近 6h 全量 25216 行：

| outcome | n | % |
|---|---|---|
| success | 15430 | 61.2 |
| rate_limited | 9571 | **38.0** |
| auth_failed | 104 | 0.4 |
| 其余 | 111 | 0.4 |

那 9571 条 `rate_limited` 拆开：

- **5051 条（52.8%）是 `所有凭据均已禁用（0/N）retry_after_secs=10`**，`credential_id IS NULL`。
  出自 `token_manager.rs:3496`，含义是**号池空/全禁用**，一次上游请求都没发。
  N 的分布：0/0=2951、0/1=1718、0/2=382。
- 4520 条是真上游 429，其中 **4146 条集中在 #448/#449 两个号**。

结论：面板上"限流 38%"这个数，一半是**没号**，不是被限流。两者混在同一个 outcome 里，
所有基于 429 率的调参（含 AIMD 与 autotune）都在被这个混淆污染。

## 1. `422` 从哪来：三个配置自乘，零实测输入

`effective_saturation_limit`（`token_manager.rs:3038`）→ `apply_rpm_headroom`（`:3052`）：

```
base = per_cred rpm_limit(>0) ?? credentialRpmLimit(>0) ?? SATURATION_FALLBACK_RPM(30)
阈值 = (base × rpmHeadroomFactor/100) − rpmReserveSlots，下限 1
线上 = 500 × 85/100 − 3 = 422
```

`500` 本身没有任何依据。23 个 config 备份的 mtime 时间线：08-02 14:49 = **80**、
08-03 10:42 = **0**、08-03 17:50 = **200**、08-04 00:10 = **500**。
24 小时内 80→0→200→500，无一次伴随实测。**实测上限见 §5：137。422 是它的 3.1 倍。**

## 2. 三个都叫 "rpm" 的量，互不相等

| 量 | 定义位置 | 计一次的时机 |
|---|---|---|
| `RpmTracker`（面板 `rpm`，就是拿去和 422 比的那个） | `scheduling.rs:81` `record()` ← `commit_selection`（`token_manager.rs:2995`） | **每个 failover 跳一次**（在 `for attempt` 循环内） |
| traces.db 行数 | 每个 KiroStudio HTTP 请求一行 | **每个 shield 重试一行**（shield 整请求重打 → 新 request_id） |
| 客户端真实需求 | shield `requests` 计数 | 实测 requests 6067 / retries 3023 → 尝试量 = 需求 × **1.50** |

且存在正反馈：429 在 **75~167ms** 内返回（实测 #448/#449 的 `MIN(latency_ms)`=75/77），
shield 立刻重打 → 尝试量涨 → 面板 rpm 涨。**429 率越高，被比较的那个 rpm 越虚高。**

## 3. `rpmSaturated` 在 rpm=124 仍是 False：两个独立成因，各自都足够

1. 阈值是 422，124 < 422。
2. **更根本**：`rpm_saturation_gate_active()`（`token_manager.rs:3096`）在 `total_count() <= 1`
   时**无条件返回 false**，而 `service.rs:478` 是 `saturated = raw_saturated && gate_active`。
   现在池里**只有 1 个号**（`credentials.json` 仅 #450）→ 无论阈值设多少，
   `rpmSaturated` 结构上不可能为 True。

`health=0.01 / ewma429=0.99` 却是对的，因为它走另一条轴：`health` 只进 `p_avail`
（`health.rs:441`），而 `p_avail` 是**候选间的排序权重**。1 个候选无从排序 → 健康层同样惰化。
**"健康层知道快死了、RPM 闸门没参与" 的真相是：单号池下两层都没参与。**

## 4. #448/#449 是 (a) 还是 (b)：只读数据已判定 = **(b) 号本身是坏的**，且 region 假说被证伪

判据一 **首个 429 之前的成功数**（决定性）：

| cred | 成功数 | 首个 429 前的成功数 |
|---|---|---|
| 443 | 2508 | **2508**（从未 429） |
| 444 | 3199 | **3199**（从未 429） |
| 445 | 2853 | **2853**（从未 429） |
| 450 | 2287 | 2105 |
| **448** | 1158 | **15** |
| **449** | 541 | **9** |

**速率限制不可能在第 9 个请求上触发。** 这一条单独就把 (a) 排除。

判据二 **剂量-反应**（每分钟 RPM 分档 → 429 率）：

| 档位 | #445 429% | #448 429% |
|---|---|---|
| 10–19 | — | **17.1** |
| 30–39 | 0.0 | 42.0 |
| 50–59 | 0.0 | 52.8 |
| 100+ | **0.0**（796 req） | 83.8 |

#448 在 10–19 RPM 就被限，#445 在 100+ RPM 干净。**不是同一条曲线上的两点，是两条曲线。**

判据三 **可恢复性**：#448 的 429 率随负载来回摆（272rpm→93%，16rpm→6%），
所以它**不是死号**，是"配额被大幅压缩/账户处于惩罚态"，最终仍服务了 1158 次成功。

region 假说被三重证伪：

1. #450 是 `apiRegion=eu-central-1`，**也 429**（18:07 起，78rpm 时 13 条）。
2. VPS 实测在 **Salt Lake City, US**（`ipinfo`；AS26042 FiberState）。
   `runtime.us-east-1.kiro.dev` conn=**70ms** / `eu-central-1` conn=**150ms** —— us-east-1 是**更近**的。
3. #448/#449 的 429 延迟地板 75/77ms 正好等于 us-east-1 RTT，即它们确实走 us-east-1，
   而**在同一端点上仍拿到 1158/541 次成功**。

RTT 指纹本身可交叉验证（这使 §8.1 的推断可信度大幅提高）：

| cred | 429 延迟地板 | 对应 region | 独立证据 |
|---|---|---|---|
| 448 / 449 | 75 / 77ms | us-east-1（实测 conn 70ms） | 无 `apiRegion` → 吃 `config.region=us-east-1` |
| **450** | **157ms** | **eu-central-1**（实测 conn 150ms） | `apiRegion=eu-central-1`（**直接读到**） |

即：唯一能直接读到 region 的号（#450），其 RTT 地板与该 region 的实测 conn 时间吻合到 7ms。

**与 `docs/region-burn-fix.md`（另一会话）的关系**：那份文档描述的是 `config.region`
被切到 eu-central-1 后 `ksk_` 号拿 403 `bearer token ... invalid` 被 `report_failure×3` 烧掉。
两者**不冲突但不是同一事件**：traces.db 里 #448/#449 的 `bearer token invalid` 计数为 **0**，
该签名只出现在 #421–423（08-03 10:44–10:51）等更早的号上；#448/#449 的 4146 条全是
us-east-1 上的 `ThrottlingException`。故本文档 §4 的结论（号坏，非 region）与那份文档的
结论（region 错配会烧号）各自成立，作用于不同时间窗与不同号。**429 与 403 是两条独立故障链，
不要用一条去解释另一条。**

**注意 error_message 不是判据**：#448/#449 与正常号的 429 都是
`com.amazon.kiro.runtimeservice#ThrottlingException / Too many requests`，字符串完全相同。
"429 vs 403 比例"这个想法行不通（#448 的 403 只有 15 条）。

**可复用的常备判据（只读一条 SQL，不需要控制实验）**：`ok_before_429 < 100` → 号坏；
`> 1000 且 429% 随 rpm 单调上升` → 真限流。

**也不是累计配额**：70 个号的 `ok_before_429` 呈双峰（0–22 与 2100–3200），
不聚在任何单一数字上 → 号"到手即分两种状态"，不是"用满 N 次就限"。

## 5. `credentialRpmLimit` 该设多少

实测天花板：

- 最佳 **5 分钟持续**零 429 = **137 RPM**（#445）
- 单分钟零 429 最高 = **192**（#403）、144（#445/#413）
- #450 在干净跑了 40 分钟 / 2117 请求后，于 78rpm 开始 429

**但先说结论中最重要的一句：在当前单号池上，`credentialRpmLimit` 设任何值都不产生任何效果**
（§3 的 gate_active=false）。这解释了用户"配置根本没法调"的体感——不是难调，是**没接上**。

这也纠正一个方向性误解：`rpm=124 只换到 23 RPM 有效吞吐` **不是"放太高掐了吞吐"**。
124 是尝试量，其中约 80% 在 140ms 内被上游 429 打回；掐吞吐的是**那个坏号**，
不是闸门。把 `credentialRpmLimit` 调低不会改善它，换号才会。

建议值：**120**，同时 `rpmHeadroomFactor=100`、`rpmReserveSlots=0`。
理由是**可审计性**：422 之所以无人能验证，正是因为三个数相乘。一个数、一个含义，
120 相对实测持续上限 137 留 12% 余量。

改后预测：

- 池 = 1 号 → **零变化**（闸门惰化）。
- 池 ≥ 2 号 → 亲和解绑与排序在 120 尝试/分/号处开始动作，而非"永不动作"。
- **不会减少放行量**：软门（`rpmHardGateOverloadWait`）与排序键都不拒绝请求。
  放行量只由入站整形 target 决定（见 `recon-capacity-and-429.md` §3 末段的同一结论）。

## 6. 按账号聚合 RPM（recon-capacity 方案）在单号池下没有意义

三条独立理由：

1. 该方案自己的 §2 已写明：单凭据时账号总量 ≡ 自身 RPM、账号阈值 ≡ 自身阈值，逐字节等价旧行为。
2. 它把新门挂在 per-cred 硬门旁边，而那个门在 `total_count()<=1` 时本身就不生效。
3. 现在 `credentials.json` 里确实只有 1 个号。

**何时才有意义**：≥2 个条目共享同一上游账号时——即 `copies` 多开
（`MAX_CREDENTIAL_COPIES=16`，2026-08-03 加入）或同一 `kiroApiKey` 被推两次。
那时 N 份分身 = N×422 的幻觉容量。**可测的前置条件**：池内存在 ≥2 条 `kiroApiKey` 相同的记录。
当前 push-manager 约每小时换一次号且池长期为 1，故这不是当前瓶颈，**建议推迟**。

## 7. `throttle-autotune` 与 Rust 双实现：现在是"算了个不生效的数"

autotune 第 44–52 行用 Python 重写了 `effective_saturation_limit` + `apply_rpm_headroom`，
再 `WANT = CAP × 80/100`（:72），clamp 到 30..1200（:73-74）。

当前实际发生的事：

```
1 个可用号 → CAP = 422 → WANT = 337 → 写进 config.inboundTargetRpm = 337
Rust 手动挡再 clamp：target_rpm.clamp(inboundRpmMin=50, inboundRpmMax=300)  ← throttle.rs:317-321
→ 真正生效的是 300，config 里存的是 337
autotune 下一轮回读 inboundTargetRpm = 337（存的那个，不是生效的那个）
→ |337 − 337| = 0 ≤ 死区 → "已接近建议值"，永不修正
```

即**它在和一个从不生效的数做收敛判断**，且它 status 里打印的"池容量 422"就是 §1 那个自乘数。

收口方案，按可行性排序：

1. **零代码、立刻可做**：让 `inboundRpmMax ≥ inboundTargetRpm`，使"存的"与"生效的"一致。
   现在两者静默相差 37 RPM，任何调参都在此之上叠加误差。
2. **消除重复**：删掉 autotune 44–52 行，改读 Rust 已 `pub` 的同一真相源
   （`effective_saturation_limit`，已被 `service.rs:470` 用于 insights）。
   注意这**只消除双实现，不消除虚假**——读到的仍是 422。
3. **真修复需要一个不存在的输入**：全仓没有任何代码路径记录"观测到的干净 RPM 上限"。
   autotune 的决策信号应改为实测干净 RPM（traces.db 已有数据，算法见 §5），而非配置算术。
   这是决策规则重写，**需要拍板，我不替你定**。

## 8. 我无法从代码与只读数据确定的部分

1. **#448/#449 的 `apiRegion` 字段值**。凭据 ID 每次重写文件都重新分配，
   现存 `credentials.json` 只有 #450，且没有覆盖 08-03 16:00–17:20 的备份。
   §4 的 region 是从 429 RTT 地板（75ms）**推断**的，不是读出来的 ——
   但该指纹已用 #450（157ms ↔ 直接读到的 `apiRegion=eu-central-1`）交叉验证，
   故这条不确定性**很低**。真正读不到的只是字段本身。
2. **#448/#449 与 #443–445 是否同一个上游 AWS 账号**。`kiroApiKey` 在可读备份里缺失，
   traces.db 无账号列。若其实是同账号，则"池大小"远小于凭据数，§6 的结论会反转。
3. **单账号的绝对上限**。192（单分钟）/137（5 分钟）都是**下界**——从来没有一个健康号
   被推到 272 尝试/分以上。真上限未测出。
4. **#450 在 18:07 的 429 是负载触发还是时钟触发**（如整点配额窗）。
   要分辨必须跨边界保持 RPM 恒定，属控制实验。
5. **每号的客户端真实需求**。traces.db 无 shield-attempt 标记，需求只能从
   `/_shield/stats` 全局估（÷1.50），无法拆到号或分钟。
6. **`rateLimitEnabled=false` 在本轮是否有影响**——未测。
7. **5051 条 "0/0" 的成因**（号池为空的那些时刻是补号间隙、还是被自动禁用清空）
   需要 journal，而 `journalctl -u kirostudio` 在本次查询窗口内返回空。
