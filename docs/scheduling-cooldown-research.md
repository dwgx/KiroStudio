# 深度推理：冷却体系研究（现状 / 垃圾 / 根治）

> 状态：研究文档（只读结论，不构成已实施改动）。2026-08-16。
> 范围：`CooldownReason` 9 变体的时长表、触发与语义、自愈体系、排序键交互、
> 持久化正确性、垃圾/低质量点与根治方案。
> 方法：全量读 `src/kiro/cooldown.rs`（1483 行）+ `token_manager.rs` 冷却相关
> 全部调用点 + `provider.rs` 全部触发路径 + 排序键/兜底选号 + `blockers-protocol.md`
> §4 magic number 交叉核对。行号以当前工作树为准（`handlers.rs` 已在 W13 移到
> `src/anthropic/`，blockers-protocol.md 里的旧行号已漂移）。
> 姊妹文档：`docs/scheduling-balance-research.md`（平摊分流体系，12 位排序键）。

## 0. 结论摘要

1. **时长表 9 个裸字面量，其中 3 个变体（AccountSuspended / QuotaExhausted /
   ModelUnavailable）生产路径从未触发** —— 9 变体里实际在用的只有 6 个。
   cooldown.rs:33/:36 的「触发覆盖」注释对 ModelUnavailable 与 AccountSuspended
   两条是**漂移的**（写的触发源在代码里不存在）；QuotaExhausted 一条注释自认未触发。
2. **「同一事件不同路径时长不同」真实存在且分两层**：
   - **池间差异（有意）**：401 在 Kiro 池 = 20s（瞬态）/86400s（永久），透传池 =
     180s；429 在 Kiro 池 = 15s，透传池 = 5s；5xx Kiro 池 = 30s，透传池 = 0。
     两池语义不同（透传号是用户自购、无风控状态），差异本身有注释背书，但
     **透传侧时长 180/5/0 是裸字面量且无常量名**，与 Kiro 侧零交叉引用。
   - **变体内矛盾（无意）**：`ModelUnavailable` 基线 300s 但 `max_short_cooldown_secs`
     =90s，`min()` 后实际永远 ≤90s —— 300s 是一个**不可达的死值**。
3. **透传池 401/402/403/429/5xx 全部复用 `CooldownReason::RateLimitExceeded`**
   （provider.rs:2049-2078 → `cooldown_custom_api`），面板上透传号的 401 显示为
   「速率限制」冷却 180s —— **语义标签错位**（401 是凭据失效，不是限流）。
4. **冷却解除后无爬坡保护**。冷却是候选过滤硬门（`is_entry_selectable_inner`
   :4253-4258），不在排序键里；解除后号立即以满血权重回池。仅有的间接保护：
   SuspiciousActivity 会连带触发 health 熔断半开（admit_prob 渐进），429 有
   health 60s 半衰期惩罚；AuthTransient / ServerError / TokenRefreshFailed
   **解除即满血**，无任何余温。
5. **自愈体系整体健康**（白名单 3 原因、streak 指数退避已配置化、self_heal_revived
   打断判据、W10 持久化串行化 + 版本守卫 + 停机 flush，4 个并发测试钉死），
   本报告未发现新的正确性缺陷；只指出 2 个低危面（内存过期条目不清理、
   冷却到禁用无升级路径）。
6. **勘误（任务前提）**：「401 非瞬态（180s）」在 Kiro 池不成立 —— 401 非瞬态是
   `AuthenticationFailed` = **86400s**；180s 是**透传池** 401/402/403 的调度跳过
   时长（provider.rs:2054）。

---

## 1. 现状分析

### 1.1 时长表（cooldown.rs:102-131 + 计算 :650-681）

| 变体 | 基线 | 递增 | 上限 | 可自愈 | 生产触发路径 | 依据 |
|---|---|---|---|---|---|---|
| RateLimitExceeded | 15s | transient 不递增；普通 1.3^n | 90s | 是 | 429 无 Retry-After（瞬态固定 15s）；有 Retry-After 用上游秒数钳 600s；透传池全部复用 | 注释 :104-107（小号池怕整池压死） |
| SuspiciousActivity | 20s | 1.6^n | 1800s | 是 | 403 TEMPORARILY_SUSPENDED（对话 + MCP 两处） | 注释 :108-111 + :653-656（软风控实测，陡增防推真封禁） |
| AuthTransient | 20s | 1.3^n | 90s | 是 | 401/403 bearer-invalid 但 `has_ever_succeeded`；force-refresh 失败但成功过；403 FEATURE_NOT_SUPPORTED + 后台重探 | 注释 :113-118 + :73-90（wire 逐字节相同、语义相反，只能拆变体） |
| TokenRefreshFailed | 60s | 1.3^n | 90s | 是 | 刷新失败判为瞬态（非凭据级） | token_manager.rs:6711-6716 |
| ServerError | 30s | 1.3^n | 90s | 是 | 上游 5xx（absorb 循环内 :4096-4103） | token_manager.rs:6177-6187（曾漏接，实测 500 风暴 408 次/时） |
| ModelUnavailable | **300s（死值）** | 1.3^n | 90s | 是 | **无**（503 MODEL_TEMPORARILY_UNAVAILABLE 走吸收层 TransientCapacity400，handlers.rs:2048） | 注释 :33 声称「全局熔断」——该实现已不存在 |
| AuthenticationFailed | 86400s | — | — | 否 | 401/403 bearer-invalid 且**从未成功** | 注释 :78（refreshToken 真废，等人工） |
| AccountSuspended | 86400s | — | — | 否 | **无**（`report_account_suspended` 只禁用不设冷却，token_manager.rs:6646-6697） | 注释 :36 声称触发 —— **漂移** |
| QuotaExhausted | 86400s | — | — | 否 | **无** | 注释 :48-49 自认「保留 future 分类点」 |

计算规则（calculate_cooldown_duration :650-681）：
- SuspiciousActivity 独立曲线 `20 x 1.6^(n-1)`，上限 1800s，**不受 90s 短冷却上限钳制**；
- 其余可自愈原因 `base x 1.3^(n-1)`，上限 `max_short_cooldown_secs`=90s；
- 不可自愈原因一律走 `long_cooldown_secs`=86400s（`default_duration` 里的 86400 只是文档值）；
- 平静期衰减：距上次冷却结束每过 60s 回退 trigger_count 一级（:520-527 / :452-459）；
- `cooldown_scale_pct` 热更缩放只作用可自愈原因，且限流/风控两类有缩放下限
  （RATE_LIMIT_FLOOR_SECS=8、SUSPICIOUS_FLOOR_SECS=12，防 scalePct=10 把冷却缩没）。

### 1.2 触发点全表（`set_cooldown*` 生产调用点）

| 调用点 | 原因 | 路径 |
|---|---|---|
| token_manager.rs:6135 `report_rate_limited_with_retry_after` | RateLimitExceeded（定制 = min(RA, 600)） | 上游 429 带 Retry-After / 端点桶 30s 封禁 |
| token_manager.rs:6146 同函数 | RateLimitExceeded（transient 固定 15s） | 裸 429（reason:null 无 RA） |
| token_manager.rs:6192 `report_server_error` | ServerError（transient 固定 30s） | 上游 5xx（absorb 循环，同链去重） |
| token_manager.rs:6332 `report_suspicious_activity` | SuspiciousActivity | 403 TEMPORARILY_SUSPENDED（对话 + MCP 双路径） |
| token_manager.rs:6369 `report_auth_cooldown` | AuthenticationFailed（24h） | bearer-invalid 且从未成功（对话 :3799、MCP :2469 两处） |
| token_manager.rs:6396 `report_auth_transient_cooldown` | AuthTransient | 1) bearer-invalid 但成功过（:3841）；2) force-refresh 失败但成功过（:2467/:3797）；3) 403 FEATURE_NOT_SUPPORTED 后台重探中（:3762） |
| token_manager.rs:6731 `report_refresh_failure_classified` | TokenRefreshFailed | 刷新失败判为瞬态（非凭据级 4xx） |
| token_manager.rs:3436 `cooldown_custom_api` | **RateLimitExceeded（复用）** | 透传池 401/402/403 到 180s、429/400/404 到 5s、其余到 0（provider.rs:2049-2078） |

**未触发变体**：AccountSuspended（report_account_suspended 只禁用）、QuotaExhausted、
ModelUnavailable（503 容量类被吸收层接走）。

### 1.3 冷却 vs 禁用边界

| 维度 | 冷却（cooldown） | 禁用（disabled） |
|---|---|---|
| 语义 | 临时跳过，自动恢复 | 持久，人工/自愈恢复 |
| 存储 | kiro_cooldown.json（`with_data_dir` 才落盘） | persist_disabled_state 落盘 |
| 面板 | 「冷却中」 | 「已禁用」+ disabledReason |
| 触发条件 | 清晰：429/5xx/风控/认证瞬态 | 清晰：连续失败达阈值/封号/配额/黑名单 |
| 恢复 | 到期自动 | 人工面板 / 全池自愈（白名单）/ 跨月配额恢复 |

边界判定**总体清晰**（各自文档详尽），两个值得记录的缝：
- `AuthenticationFailed` 是「不禁用的 24h 冷却」= 面板上「冷却中」的僵尸（比禁用更难
  发现）——文档自认这是刻意的（cooldown.rs:82-86：把瞬态当永久 = 冻健康号 24h，
  且不禁用；AuthTransient 拆分后该变体只剩「从未成功」一类，语义收敛合理）。
- `report_rate_limited_with_retry_after` 对超大 Retry-After 只钳到 600s（:6135-6140），
  注释说「那类应走配额耗尽禁用」，但**没有从冷却升级到禁用的路径** —— 一个反复拿
  到几天级 Retry-After 的号会永远「冷却 600s + 回池再撞」循环，永远走不到禁用判定。
  实际兜底靠 provider 侧 402 配额路径，但冷却侧这条注释描述的升级机制不存在。

### 1.4 自愈体系

| 组件 | 现状 | 证据 |
|---|---|---|
| 可自愈白名单 | `TooManyFailures` / `SuspiciousActivityAuto` / `TooManyRefreshFailures` 3 个；排除 Manual/Unknown/InvalidRefreshToken/InvalidConfig/AccountSuspended/QuotaExceeded/RequestLimitReached/Passthrough* | token_manager.rs:1728-1735 + :1700-1715 文档 |
| 触发条件 | 选号无候选 + 池中存在白名单内禁用号 | :4836-4906 |
| 退避 | `self_heal_base_backoff_secs x 2^streak`（上限 `self_heal_max_backoff_secs`=900s，shift 钳 31 防溢出），已配置化热更 | :4795-4835 |
| 打断判据 | `self_heal_revived` 集合：只有**本次复活的号**成功才清零 streak（修复「任意号成功即清零导致退避从未生效」） | :4872-4882 + :5830-5837 |
| 复活动作 | 清 disabled + clear_transient_counters（清全计数，非只 failure_count）+ 清 cooldown + 重置 rate_limiter + 落盘 | :4858-4903 |
| 与跨月恢复交互 | `recover_expired_quota_disables`（跨自然月 402 号复活）同样清 cooldown + 落盘；顺序在自愈检查之前 | :4779-4789、:6621-6639 |
| 与黑名单交互 | IP/机器码黑名单走禁用（不在白名单）→ 不被自愈。正确 | :1711-1715 文档 |
| 持久化 | `kiro_cooldown.json`：trigger_count 跨重启保持（W10 修复：save_lock 串行化 + 版本守卫条件清 dirty + 停机 flush_now），4 个并发测试钉死 | cooldown.rs:703-794、:1320-1482 |

`trigger_count` 的消费点：冷却时长递增（内部）+ admin API 下发展示（admin/service.rs:310/
389/1149/1165）+ 持久化档位。生产代码**没有**再把它当判据（旧判据 `trigger_count >= 10`
在 :6216-6219 注释里已废弃——`report_success` 的 `clear_cooldown` 会删条目归零，不可达）。

### 1.5 排序键交互（选号）

- **冷却 = 候选过滤硬门**，不是排序键位：`is_entry_selectable_inner`
  （token_manager.rs:4253-4258）在候选收集阶段直接滤掉冷却中的号 —— 冷却中的号在
  12 位排序键里**没有位置**（不是 unusable=1 沉底，而是直接出局）。
- 唯一例外：全池冷却兜底 `select_ignoring_cooldown`（:4141-4192）**故意放行**冷却中的
  号（排序键第一维 `fallback_cooldown_tier`：Ready < Shallow < Deep，档内 id 轮转），
  语义「拿真实上游 429 好过网关自造 429」——设计如此，非缺陷。
- **冷却解除后的行为**：号立即回到候选集，以当前 health p_avail 满血权重参与排序。
  无「解除后短窗口降权」机制。间接保护仅两条：
  1. SuspiciousActivity 会连带 `health.report_family_suspicious`（:6344-6345）→ 熔断
     Open → 冷却硬窗过后 health 半开 admit_prob 渐进放回 —— **只对这一个原因生效**；
  2. 429 的 `health.on_429`（:6174）惩罚半衰期 60s —— 冷却解除后 p_avail 仍略低。
  - **AuthTransient / ServerError / TokenRefreshFailed 完全不碰 health**：解除即满血，
    下个请求可能立刻再撞同一面墙（20-30s 冷却 vs 上游惩罚窗口往往分钟级）。
- 排序键第 5 位 `ramp_tier`（RPM slew-rate 检测，近 10s 速率 vs 60s 均值，:3926-3943）
  与冷却无直接绑定：冷却 15-20s 后 60s RPM 窗口残留有限，保护近似不存在。

### 1.6 持久化与重启恢复

- 落盘格式：credential_id → (reason, started_at, expires_at, trigger_count)，墙钟
  Unix 毫秒（Instant 重启即重置，落盘无意义）；已过期条目落盘时过滤、加载时丢弃。
- 停机路径 `flush_now`（token_manager.rs:5779 自愈/停机收口调用）绕过 debounce 硬写，
  保住 debounce 窗口内的退避档位 —— 这正是本模块要解决的事故形态（风控窗口内重启 =
  反复以短间隔砸风控中的号）。
- W10 修复点（blockers-concurrency.md 三③ 闭环）：save 串行化（save_lock）+ 版本守卫
  条件清 dirty + 锁序（先释放 entries 锁再落盘）。已确认代码在位（cooldown.rs:741-794）。
- 重启恢复正确性：load 静默回退空（文件不存在/损坏不 panic）、过期丢弃、trigger_count
  原样恢复（:798-846）→ 恢复后递增退避档位不回落。**正确。**

## 2. 问题清单（垃圾 / 低质量）

| # | 问题 | 证据 | 严重度 |
|---|---|---|---|
| G1 | 时长表 9 个裸字面量，无常量名（blockers-protocol §4 #18 已列） | cooldown.rs:107-129：15/20/60/20/30/300/86400x3 | 高（改一处漏一处不报错） |
| G2 | 20s 散 4 处生产语义各异：SuspiciousActivity 基线 / AuthTransient 基线 / UPSTREAM_SUSPENDED_RETRY_AFTER_SECS / A8 account_throttled RA 兜底 | cooldown.rs:111/:119；anthropic/handlers.rs:1341；model/error_messages.rs:186 | 高（blockers-protocol §4 #2 已列，同值异义） |
| G3 | 透传池冷却时长 180/5/0 裸字面量，与 Kiro 侧零交叉引用（401 180s 与 Kiro 池 401 处置 20s/86400s 无任何联动） | provider.rs:2049-2078 | 中高 |
| G4 | 透传池全部复用 `RateLimitExceeded` 原因 → 面板上透传号 401/402/403 显示「速率限制」；`fallback_cooldown_tier` 若未来涉及会误判 Shallow | provider.rs:2049-2078 + token_manager.rs:3434-3442 | 中（标签错位 + 语义污染） |
| G5 | 3 个变体生产从未触发：AccountSuspended（注释 :36 漂移，report_account_suspended 只禁用）、QuotaExhausted（注释自认）、ModelUnavailable（注释 :33 声称的「全局熔断」已不存在，503 容量类被吸收层接走） | cooldown.rs:28-49 vs 全仓 grep | 中（死代码 + 误导注释） |
| G6 | ModelUnavailable 基线 300s > 短冷却上限 90s，`min()` 后**永远不可达**（未来接触发点实际也只有 90s） | cooldown.rs:121 vs :315/:673 | 低（死值） |
| G7 | 冷却解除后无爬坡保护：AuthTransient/ServerError/TokenRefreshFailed 解除即满血回池 | §1.5 | 中高（反复撞同一面墙） |
| G8 | 冷却到禁用无升级路径：超大 Retry-After 只钳 600s，注释声称的「应走配额禁用」机制不存在 | token_manager.rs:6131-6140 注释 vs 代码 | 低 |
| G9 | 内存过期冷却条目永不清理：`cleanup_expired` 生产路径零调用（`cleanup_scheduling` 只清 rpm/health） | cooldown.rs:614-629 vs token_manager.rs:8988-8991 | 低（小池量级无碍，增长无界） |
| G10 | 401/403 二分（has_ever_succeeded）在对话路径与 MCP 路径**逐字复制两份**，注释自称同款（仓内已知的「同一逻辑各写一份正是漏改成因」模式，MCP 路径历史已漏修过两处） | provider.rs:2466-2469 vs :3796-3799 vs :3822-3841 | 中（一处改漏一处） |
| G11 | 注释漂移：cooldown.rs 头注释的「当前触发覆盖」（:28-49）与实测触发点不符（AccountSuspended/ModelUnavailable 两条）；blockers-protocol.md 行号整体漂移（handlers.rs 已移入 anthropic/） | §1.1/§1.2 对比 | 低（文档维护） |
| G12 | `cooldown_scale_pct` 与时长表双通道并存：admin 已有百分比旋钮，再做时长参数化需防「两套旋钮打架」 | config.rs:500-501 + cooldown.rs:324-327 | 低（设计约束，非缺陷） |

## 3. 根治方案表

| # | 问题 | 根治 | 风险 | 工作量 |
|---|---|---|---|---|
| R1 | G1/G2/G3：裸字面量 | 时长表抽常量（`const COOLDOWN_BASE_SECS: [(CooldownReason, u64, &str); 9]` 或每个原因一个 const + 依据注释）；透传侧 180/5 抽常量并与 Kiro 侧注释互引；20s 四处在各定义点注释互引（同值不同义，**不合并**）。不做 config 化：时长是内部策略，admin 已有 scalePct 旋钮，参数化扩大攻击面且与 scale 通道重叠 | 低（纯重构，cooldown.rs 守卫测试已钉住大部分值） | 小（半天） |
| R2 | G4：透传原因错位 | 为透传池冷却引入独立分类（如 `CooldownReason::PassthroughAuth` 或给 `cooldown_custom_api` 加 reason 参数），面板 code 正确显示「认证失败」类而非「速率限制」。改完需同步：fallback_cooldown_tier 的 auto_recoverable 判定、i18n code 表、前端判定（credential-card/row 走 code 已脱钩文案，风险小） | 中（新增枚举变体 → 必须补 code()/description()/i18n 表 + 守卫测试更新；会动 admin 快照展示） | 中（1 天） |
| R3 | G7：冷却解除无爬坡 | 排序键加「冷却余温」位：cooldown 条目记 `last_triggered_at`，解除后 60s 内该号在排序键里降一档（如并入 health_tier 或 ramp_tier 同层的新位）。只降权不硬门（单号池不能自造 503，与 ramp_tier 同哲学）。对 SuspiciousActivity 已有 health 半开，此位只补 AuthTransient/ServerError/TokenRefreshFailed/429 四类 | 低-中（排序键 12 位有测试钉住，新增位放 ramp_tier 之后最安全；需 1-2 个排序测试） | 中（0.5-1 天） |
| R4 | G5/G6/G11：死变体与漂移注释 | 三选一：a) 删掉 3 个未触发变体（AccountSuspended/QuotaExceeded 留禁用路径即可）；b) 接上 ModelUnavailable 触发点（吸收层耗尽时对末次号设冷却，把 300s 与 90s 上限矛盾一并解决——接上后需先定 300s 是否合理）；c) 最小：修正 cooldown.rs:28-49 覆盖注释 + 给死值加「未触发」标注。推荐 **c 起步 + b 视需求**（删变体会动 code() 稳定 API 面与前端 i18n，成本高收益低） | 低（c 纯文档）；b 会动 absorb 热路径（中） | 小（c 半小时；b 另计） |
| R5 | G8：冷却到禁用无升级 | 在 `report_rate_limited_with_retry_after` 的钳制分支补一条观测/告警（RA > 600s 时记录），或把注释改写成实际行为（不实现升级）。真正升级（冷却→禁用）超出冷却模块职责，建议**只改注释** + 面板已有 quota 告警兜底 | 极低 | 极小（半小时） |
| R6 | G9：过期条目清理 | `cleanup_scheduling` 加 `self.cooldown.cleanup_expired()`（main.rs:538 已有周期调用点） | 低（一次性删除 + mark_dirty，落盘自然收敛） | 极小（半小时） |
| R7 | G10：二分复制两份 | 提取 `report_bearer_invalid(id, proven)` 收口函数（内部按 has_ever_succeeded 分流），对话/MCP 两处调用同一函数 | 低-中（热路径微重构，有既有测试覆盖 403 分类） | 小（半天） |

优先级建议：**R6（半小时、零风险）→ R1（半天）→ R3（0.5-1 天）→ R4c → R7 → R2（要动枚举与 i18n，排最后）**。R2 与 R3 若同波次做，先 R3 后 R2（排序键位不依赖枚举形状）。

## 4. 自 review

- **证据可信度**：时长表/触发点/排序键/持久化全部直接读源码核验（非二手）；未触发
  变体用全仓 grep（`CooldownReason::X` 在 cooldown.rs 之外的引用）双通道确认——
  AccountSuspended 仅测试 :10987、ModelUnavailable 零引用、QuotaExhausted 零引用。
- **勘误记录**：任务描述「401 非瞬态（180s）」在 Kiro 池不成立（实际 86400s），180s
  属于透传池且复用 RateLimitExceeded 标签 —— 已在 §0.6 修正，这不影响结论 2/3 的成立。
- **未验证项（诚实披露）**：① 未跑测试（本机 8GB 编不过 Rust，验证循环需服务器
  Docker，本次为纯研究不改码，未触发）；② blockers-protocol.md 中旧行号（handlers.rs
  :1058/:1772 等）已随目录重构漂移，本报告全部使用现读行号；③ `report_rate_limited`
  的 Retry-After 钳制与 402 配额路径的交互（G8）只读到了冷却侧，provider 侧 402 处理
  未深究 —— 标注为低危是因为即使无升级路径，影响也只是「多撞几次上游」，且已有
  quota 告警兜底。
- **与既有文档的关系**：blockers-protocol §4 #2/#18（20s 散点、冷却表裸字面量）与本
  报告 G1/G2 一致，本文补充了触发路径维度（未触发变体、透传复用标签、解除无爬坡）——
  那是 magic number 视角看不到的语义问题。
- **可证伪性**：每条问题的证据都带文件:行号；根治方案表带风险与工作量，便于排期后
  逐条闭环。
