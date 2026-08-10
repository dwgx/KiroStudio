# kirostudio 历史档案

从 CLAUDE.md 移出（2026-08-10）。这些是**查档资料，不是当前待办** ——
已知问题的历史记录、与其他分支的对比、逐条 changelog、生态经验。
当前状态看 `STATUS.md`，长期约束看 `CLAUDE.md`。

## ⚠️ 已知问题历史档案（不作当前待办）

> 状态：#1–#19 **在当前工作树有对应修复证据**（各条保留依据备查，但不代表 `HEAD` 或线上已含）。
> **#20–#22 也在当前工作树有对应证据**（2026-08-06 复核；不代表 `HEAD` 或线上已含），
> 见本节末尾。
> ⚠️ 本节整体是**历史档案**，不是待办清单。当前边界看 `STATUS.md` 和 `docs/TAKEOVER.md`；
> `OPEN-ISSUES-2026-08-06.md` 也只作历史证据。
> 与 `TRACKING-2026-08-06.md`。

### 高危

**#1 — 6to4 IPv6 SSRF bypass** (`src/common/ssrf.rs`) ✅ **已修复 2026-07-25**
- 修复：在 `is_forbidden_ipv6` 加入 `seg[0] == 0x2002` 分支，提取嵌入 IPv4 并复用 `is_forbidden_ipv4`
- 同时补充4个6to4测试用例

**#2 — OTA下载无大小限制（OOM风险）** (`src/admin/update.rs`) ✅ **已修复（chunked 缺口已补）**
- 第一轮：新增 `MAX_DOWNLOAD_BYTES = 200MiB` + 两处 Content-Length 预检 —— 但 `Transfer-Encoding:
  chunked` 不带 Content-Length 时预检失效，后续 `resp.bytes()` 仍会无限制读进内存。
- 收口：抽出 `src/common/http_read.rs::read_body_capped`（**流式**按累计字节截断），
  `update.rs:252` 只留一个固定 `MAX_DOWNLOAD_BYTES` 的薄包装，两个下载路径
  （`update.rs:395` 镜像 / `:437` GitHub 直连）都改为走它。
- 抽公共函数的理由写在 `update.rs:246-250`：上一轮漏改就是因为同一逻辑各写了一份。

### 中危

**#3 — OTA客户端超时fallback静默失效** (`src/admin/update.rs`) ✅ **已修复 2026-07-25**
- 修复：`unwrap_or_default()` → `unwrap_or_else(|e| { tracing::warn!(...); default })` 使失败可观测

**#4 — OTA允许降级攻击** (`src/admin/update.rs`) ✅ **已修复 2026-07-25**
- 修复：`target_differs` → `target_is_newer`（只接受 `compare_versions > 0`），降级被显式拒绝

**#5 — SSRF validate/fetch DNS语义不对称** (`src/common/ssrf.rs`) ✅ **已文档化**
- `validate_outbound_url` 文档注释已说明 DNS fail-open 的权衡理由（网络抖动≠攻击，IP字面量仍拦，出站禁重定向兜底）；`build_guarded_client` 保持 fail-closed

**#6 — `extract_client_ip` 不遵守 `trust_forwarded_header` 配置** (`src/anthropic/handlers.rs`) ✅ **已修复**
- **原描述方向错了**：不是"不遵守 =false"，而是**不遵守 =true**。`config.trust_forwarded_header`
  当时没传进 handler 层（`main.rs` 只喂给 `SecurityState`），handler 的 `trusted_client_ip`
  只靠 `is_trusted_proxy_peer(peer)`（对端是否私网/环回）自行判定。
- 真实受害场景：反代在**公网** IP（CDN 直连 / 跨网段 LB）且管理员开了 `trustForwardedHeader=true` 时，
  security 中间件按 XFF 最右段判真实客户端，而 handler 层退回 `peer`＝反代公网 IP →
  业务层 IP 黑名单实际封的是**反代自己**（一封封掉全部用户），且所有客户端共享同一个机器码，
  机器码黑名单同样一封封全部。
- 修复：新增进程级镜像 `anthropic::set_trust_forwarded_header`（TIER3 热重载范式，
  `handlers.rs:74-92`），`main.rs:437` 在装配时喂入，`trusted_client_ip` 改为同时看该 flag，
  与 `common::security::client_ip` 口径统一。回归测试 `handlers.rs:2606`
  `test_trusted_client_ip_respects_trust_forwarded_header_config`（true/false 两分支都断言）。

### 低危

**#7 — `snapshot()` 返回过期的熔断器状态** (`src/kiro/health.rs`) ✅ **已修复 2026-07-25**
- 修复：`snapshot()` 改用 `get_mut` + 先调用 `tick_circuit(s, now)` 再读状态

**#8 — `expires_at=None` 时两函数语义不对称** (`src/kiro/token_manager.rs`) ✅ **已修复**
- `is_token_expired` 对 `is_api_key_credential()` 直接返回 false（token_manager.rs:78-83），刷新路径同样跳过 API Key 凭据；两函数的 `unwrap_or` 默认值差异已文档化说明理由

---

### 本轮修复（2026-07-27，全部带回归测试）

**#9 — `acquire_context` CPU 忙等死循环** (`src/kiro/token_manager.rs`) ✅ **已修复** 🔴致命
- 根因：`transient_wait_outcome` 的硬门条件与 `is_entry_selectable` **不对齐**，漏了
  `is_custom_api_credential()` 与 `is_model_blocked()` 两道。于是 select 返 None 而等待判定
  返 `Available`，`WaitOutcome::Available => continue` 分支既不 sleep 也不递增 `attempt_count`
  → 无退出条件的忙等，请求永不返回且烧满一核。
- 触发：① 池中只有 custom_api 代挂号（透传全冷却后回落 Kiro 路径、MCP/WebSearch）；
  ② 某模型被全池加进 model_blocklist（TTL 1800s）后再来同模型请求。
- 修复：补齐两道过滤 + 加 `race_reselect_count`（上限 64）纵深防御，使**将来再次不对齐也只是
  快速失败而非挂死**。回归测试 2 个（旧代码必挂死）。

**#10 — 入站令牌桶容量塌陷** (`src/kiro/throttle.rs`) ✅ **已修复** 🔴致命
- 根因：容量 `=(rpm*1000/60).max(1)*burst`，而取一个令牌需 1000 milli → 隐含要求
  `rpm*burst >= 60`。默认 `burst_secs=2` 时 `rpm<=29` 容量就 <1000，桶**永远攒不满一个令牌**。
  而 AIMD 从默认 100 连降两档即到 25（100→50→25→20=floor）——**默认配置下被上游 429 打两次
  就整体塌陷**：所有请求排满 30s，passthrough=true 时限速彻底失效且每请求白等 30s。
- 修复：`capacity_milli_locked` 与构造函数初始桶都 `.max(ONE_TOKEN_MILLI)`。回归测试 3 个。

**#11 — AIMD 升档饿死（第二条触发路径）** (`src/kiro/throttle.rs`) ✅ **已修复** 🟠高危
- 根因：`report_upstream_429` 在**已达 rpm_min（`next == cur`，本次并未真降档）**时仍无条件
  `last_md_nanos = now`。而 `maybe_step_up` 要求 `since_md >= 20s`，于是上游持续零星 429
  （间隔 >3s 穿过去抖窗、<20s 不到静默期）就让 RPM **永久卡在 floor 再也回不去**。
  注：去抖分支的同类 bug 之前已修，但这条"降不动了"的路径漏了，两者是同一死锁的两条路径。
- 修复：`last_md_nanos` 只在 `next != cur`（真降档）时刷新。回归测试含对照组。

**#12 — 裸 429 的 health 键用 `cred:{id}` 而非 `family_key`** (`src/kiro/token_manager.rs`) ✅ **已修复** 🟠高危
- 根因：`report_rate_limited_with_retry_after` 硬编码 `format!("cred:{}", id)`，而**读侧**
  （选号 `p_avail`/`report_success`/`report_family_suspicious`/`health_snapshots`）全用 `family_key`。
  external_idp(M365) 的 `family_key` 是 `m365:{tenant}` ≠ `cred:{id}` → 裸 429 的
  `ewma_429`/`consecutive_429`/跳闸全写进**从不被读**的影子条目 → **M365 号被 429 打爆也永不熔断**，
  面板 health 恒显示 `consecutive_429=0`。social/idc 因两键恰好相等而正常（现有测试用的正是 social，故测不出来）。
- 修复：改用 `self.family_key_of(id)`。

**#13 — 自动禁用不持久化，重启后死号复活** (`src/kiro/token_manager.rs`) ✅ **已修复** 🟠高危
- 根因：`report_quota_exhausted`/`report_account_suspended`/`report_refresh_token_invalid`/
  `report_failure`/`report_refresh_failure` 禁用后只调 `save_stats_debounced()`，而 `StatsEntry`
  **不含 `disabled`/`disabled_reason`**，也都不调 `persist_credentials()`（唯一例外是
  `report_suspicious_activity`）。→ 额度耗尽 / 被封 / refreshToken 失效的号重启后以 enabled
  回池，重新走一遍禁用流程（invalid_grant 号白耗一次刷新往返，配额号多打一次 402）。
- 修复：新增 `persist_disabled_state(id)` 统一收口，5 条路径全部接入。

**#14 — `invoke_sniff_buffer` 无界持有导致整条流停摆** (`src/anthropic/stream.rs`) ✅ **已修复** 🟠高危
- 根因：`partial_invoke_tag_suffix_len` 对"最后一个 `<` 之后没有 `>`"的尾巴**无长度上限**地保留。
  一旦这个 `<` 落到缓冲区首位，`keep=buf.len()`、`emit_len=0` → 此后**整条响应的所有文本都不再
  下发**，全部囤到流结束才 flush，且缓冲无界增长。
- 触发：reclaim 开（默认）+ 请求带工具（CC 常态）+ 模型输出含孤立 `<`
  （中文散文的"条件 a < b 时"、数学式、代码里的比较运算符）。
- 修复：尾巴超过 `MAX_PARTIAL_TAG_BYTES=64` 即判定"不是半个标签"，正常吐出。回归测试含边界。

**#15 — CJK 工具名"越缩越长"且仍超限** (`src/anthropic/converter.rs`) ✅ **已修复** 🟠高危
- 根因：`map_tool_name` 用 `name.len()`（**字节**）判超限，`shorten_tool_name` 用
  `char_indices().nth(54)`（**字符**）截前缀。30 个汉字 = 90 字节 > 63 触发缩短，但 `nth(54)`
  在只有 30 字符时返回 `None` → prefix 取整个名字 → 结果 90+1+8 = 99 字节，**比原名更长且仍
  超限** → 上游回 400 Improperly formed request。（现有测试只用 ASCII 名，覆盖不到。）
- 修复：前缀按**字节预算 54** 逐字符累加截断（UTF-8 安全），结果恒 ≤63 字节 + `debug_assert`。回归测试 2 个覆盖纯 CJK 与混合宽度边界。

**#16 — `/admin/api/bg-img` 与预取池的 MIME/体积缺口** (`src/admin_ui/router.rs`) ✅ **已修复** 🟠高危
- 根因①：`bg_img_proxy_handler` 把上游 `Content-Type` **原样回传**。该端点匿名可达
  （admin_ui 路由树无鉴权 layer），构造让上游返回 `text/html` 即可在 `/admin` **同源执行脚本**；
  而 adminKey 明文存在 localStorage，全仓无 CSP → 完整接管。SSRF 与 10MiB 都防了，唯独 MIME 没限。
- 根因②：`download_bg_bytes`（预取池路径）既不校验 MIME 也**没有体积上限**，而图片 URL 来自
  第三方 JSON 源（api.lolicon.app）的响应＝外部可控数据。污染入池后经**匿名**的
  `/admin/api/bg-cached?idx=N` 原样吐出 → 同一条 XSS，且 `resp.bytes()` 无上限 × 池容量 20 可顶爆内存。
- 修复：两处都加图片 MIME 白名单（非图片拒绝入池 / 覆盖为 image/jpeg）+ 10MiB 流式上限；
  两个响应端点都补 `X-Content-Type-Options: nosniff`（防内容嗅探绕过）。

**#17 — OTA 按 OS 选资产但不看架构** (`src/admin/update.rs`) ✅ **已修复** 🔴致命
- 根因：`ASSET_BIN` 只有 `cfg(windows)` / `cfg(not(windows))` 二选一。macOS（Intel 与 Apple
  Silicon）和 arm64 Linux 全部落到 `kirostudio-linux-x86_64` 分支 → macOS 上会把 Mach-O
  替换成 Linux ELF，sha256 还校验通过（下的和它自己的哈希对得上），随后 `restart_service`
  让进程退出 → **新二进制无法执行，服务当场死亡且无法自愈**（人工恢复 `mv kirostudio.bak kirostudio`）。
- 修复：按 **OS × ARCH** 穷举 6 个组合；未覆盖组合 `compile_error!` 而非静默回退默认值。
  同时 CI 补 macOS 构建 job（aarch64 + x86_64）、`install-binary.sh` 支持 macOS + launchd。

**#18 — tag 与 Cargo.toml 版本无一致性校验** (`.github/workflows/release.yml`) ✅ **已修复** 🟠高危
- 根因：OTA 的 `has_update` 比较"远端最大 tag vs 编译期注入的 `LOCAL_VERSION`"。若打了
  `v0.7.44` 却忘记 bump Cargo.toml（仍 0.7.43），二进制自报旧版 → 升级→重启→**仍认为有新版**
  → 无限升级循环，且每轮都真的重写一次二进制。这是整条发布链上唯一没有自动闸门的地方。
- 修复：test job 加 tag/Cargo.toml 版本一致性门禁（仅 tag 触发时校验，不一致直接 fail）。

**#19 — 测试依赖真实 DNS** (`src/kiro/token_manager.rs`) ✅ **已修复** 🟡中危
- 根因：custom_api 相关测试用 `https://a.example.com` 做 base_url，而 SSRF 校验会真实解析域名。
  在开启 fake-IP 模式代理的机器上（Clash/Surge 等用 `198.18.0.0/16` 作 fake-IP 池，
  而该段正是 `ssrf.rs` 的 RFC2544 benchmark 禁止段）→ 校验正确地拒绝 → 测试 panic。
  即 `cargo test` 对大量国内开发者必然失败，且失败原因与被测逻辑无关。
- 修复：改用 RFC 6761 保留的 `.invalid` TLD（保证永不解析 → 走 DNS 失败 fail-open 分支），
  测试从此与本机 DNS 环境无关。

---

### #20–#22（2026-08-06 复核：当前工作树有证据，不作部署结论）

> 本节此前标题是「当前待修」。三条在当前工作树都有落地证据，**照旧标题去"修"会重复实现**；
> 但它们不因此代表 `HEAD` 或线上已含。
> 各条保留原始描述备查，状态写在末尾。行号会漂，改前用**符号名**重新 grep。

**#20 — admission 超时在面板上完全隐形** (`src/kiro/provider.rs`) ✅ **已修复**
- 原问题：`acquire_admission()` 超时后直接 `bail`，**既不 `emit_record` 也不 bump 任何计数器**
  → 入站被整形层掐掉的请求在面板上不存在，看到的成功率是**偏乐观的**。
- 修复：超时路径 bump `recovery_metrics::bump_inbound_admission_timeout()`（计数器
  `inbound_admission_timeouts`）+ `emit_record`，`error_message` 带
  `inbound_admission_timeout=1` 机器可读标记；`handlers::map_provider_error` 据此单列一条
  429 + Retry-After 分支（文案刻意与全池冷却不同，好让重试层分辨「这是网关背压，不该重试」）。
- 有**源码级守卫测试**（`provider.rs` 内 `include_str!` 自读 + 先剔注释行）钉死：
  注释掉那次 bump 或去掉 error_message 字面量即 FAIL。
- ⚠️ 原本那条警告仍然有效：`acquire_admission()` 在 **`call_api_with_retry` 内部**，
  45s 闸门相对 `call_started` **也在它内部** ⇒ 吸收循环必须在准入闸门**之外**，
  已有守卫测试 `admission_gate_is_outside_absorb_loop` 钉死。

**#21 — `retries` 无法聚合** (`src/usage/usage_stats.rs`) ✅ **当前工作树的聚合层和 API 出口均有证据**
- 原问题：`Aggregate` 没有任何 retry 字段、`add` 不读 `r.retries`；写入点 4 处齐全但
  **画不出趋势也算不出分布**。
- 已落地：`Aggregate` 有 `retries_sum` 与 `retried_requests` **两个**字段（配对是承重的：
  绝大多数请求 `retries=0`，只用 `requests` 当分母会把「2 条各重试 6 次」稀释成「平均 0.02 次」）；
  `add` / `merge` 均已累加，并有派生比率（`retries_sum/requests` = 整池放大倍数、
  `retries_sum/retried_requests` = 真重试时重试几次）。有测试断言"旧代码恒 0"。
- 当前工作树的 `usage_handlers.rs` 有 overview/group/timeseries 出口测试；本次本地 Rust 全套测试通过。
  这只证明工作树链路，不证明 `HEAD` 或线上面板已经包含它。
- 相关缺口（原记录，未复核）：`failover_exhausted` 的 bump 点被 `if real_failover_happened`
  包着 → 墙钟预算 break 但只打过 1 个号的路径不计入。

**#22 — 停机注释承诺的行为代码里不存在** (`src/main.rs`) ✅ **已修复**
- 原问题：`shutdown_with_drain_cap()` 实际是「等信号 → `sleep(8s)` → 返回」，
  注释却承诺"未 drain 完的连接被断开"—— 而 `serve().await` **无上限**。
- 修复：竞速已用 `select!` 取先到者（在 `main` 里，**不在** `shutdown_with_drain_cap` 内部——
  函数文档注释已显式写明这一点，避免下一个人以为竞速在函数里而误改），注释改成实际行为。
  `SHUTDOWN_DRAIN_CAP_SECS = 8` 保留，`cap` 由调用方传入以便测试传毫秒级值。

## 与其他kiro.rs分支的对比

| 特性 | KiroStudio (dwgx) | hank9999/kiro.rs (原版) |
|------|-------------------|------------------------|
| 多凭据调度 | ✅ balanced 8键选号+族级连坐 | ❌ 单凭据 |
| 管理面板 | ✅ React SPA内嵌 | ❌ 无 |
| 用量统计 | ✅ SQLite+JSONL+内存聚合 | ❌ 无 |
| 三种上号方式 | ✅ Social/IDC/ExternalIdP | ✅ Social |
| 输入压缩 | ✅ 防上游5MiB限制 | ❌ 无 |
| OTA自更新 | ✅ GitHub一键升级+回滚 | ❌ 无 |
| 入口安全层 | ✅ IP白名单/限流/SSRF | 基础 |
| 工具参数修复 | ✅ JSON repair+截断恢复 | ❌ 无 |
| Windows托盘 | ✅ 系统托盘+开机引导 | ❌ 无 |

## 最近变更

### 2026-08-03 — 风控/重试可观测性修复批次（982 测试全绿 / clippy 0 error / tsc 干净）

| 文件 | 变更 | 依据 |
|------|------|------|
| `token_manager.rs`（两处） | 号池**真耗尽**的 bail 带上 `retry_after_secs` | 原先外层拿不到恢复时刻，只能盲等。+3 测试 |
| `handlers.rs` | 新增 `is_upstream_temporarily_suspended` + `UPSTREAM_SUSPENDED_RETRY_AFTER_SECS`，**403 `temporarily is suspended` → 429 + Retry-After: 20** | 🔴 该错误占近 2h 流量 **22.3%**，原先落 502 → 上游/外挂按 5xx 盲退避。+2 测试 |
| `provider.rs` | `fail_record.retries = attempts_used`（新增循环外计数器） | 所有失败 outcome 原先**无一例外 retries=0**（auth_failed 1487 / rate_limited 1098 / server_error 118 / bad_request 91），失败侧重试数从来没被记录。+源码级守卫测试 |
| `provider.rs` | `compute_max_retries` doc 注释改对 + 定时炸弹测试重写 | 注释与实现分叉 |
| `token_manager.rs` | 排序键里 `inflight` 双读收口成单读 | 同一临界区内两次读可能不一致 |
| `token_manager.rs` | 两处注释漂移修正（排序键概览 6→10 项；删掉已不存在的 jitter 键） | — |
| `deploy/hotswap.sh` | 回滚改用 `install`（`cp -a` 对运行中二进制报 **ETXTBSY**）+ 加 `trap` | 原 `mv "${BIN}.prev"` 用一次就吃掉回滚点；无 trap 时 Ctrl-C 会留孤儿裸实例双写 `kiro_stats.json` |
| `token_manager.rs` | 凭据**多开**：新增 `copies` 字段 / `add_credential_allowing_duplicate` / `MAX_CREDENTIAL_COPIES=16` | +6 测试 |

**本轮明确不做（有证据，别重提）**：`p_avail` 批量化（无实测支撑 + 碰选号热路径 + 行为不可观测
故无法用测试保护）；SQLite `busy_timeout`（rusqlite 已默认 5000ms 且单连接单 Mutex）；
`spawn_blocking` 包 trace_db（争抢在 `parking_lot::Mutex` 上，换线程占锁无用）；
上游 `hank9999/kiro.rs` 逐条对照（28 条 fix 零可修项）；
`ccAutoBuffer` 任一方向（两条实测证据链互相矛盾，**需用户拍板**）。

### 2026-07-26 — v0.7.43 shadow cache 恢复 + 大重构

- `feat(cache)`：经 `continuationId` 恢复 shadow cache 估算，续传请求现在会填充 `cache_read_input_tokens`
- `refactor(token)`：全局刷新 mutex 改为**每凭据独立锁**；瞬态刷新错误加 1s/2s/4s 退避重试
- `fix(provider)`：`MODEL_TEMPORARILY_UNAVAILABLE` 不再计入凭据健康惩罚；overloaded 2s 退避重试 + 新增 `overload_fallback_model` 配置
- `fix(auth)`：external_idp HTTP 401 立即禁用凭据；social 上号回调补 state 参数（CSRF）
- `fix(machine)`：热载凭据 machineId 去重检查；轮换持久化失败输出结构化警告

### 2026-07-25 — v0.7.42 安全修复 + 凭据管理加固

- OAuth state 参数校验防 CSRF（Critical）；无头部署下回调服务器加 5 分钟超时（Critical）
- PKCE code_verifier 改用 getrandom（High）；普通 429 用每凭据健康键而非族键（High）
- IdC HTTP 401 立即禁用凭据（High）；machineId 由 UUID 拼接改为 SHA256 hex（Medium）

### 2026-07-25 — v0.7.41

- `MODEL_TEMPORARILY_UNAVAILABLE` 识别为 overloaded_error（新模型发布容量限制）
- 前端 PROBE_MODEL_CATALOG 补 `claude-opus-5`

### 2026-07-25 — v0.7.40 invoke/cache/cooldown 修复

**已应用的变更（8项）：**

| 文件 | 变更 | 优先级 |
|------|------|--------|
| `src/anthropic/stream.rs` | thinking 模式下非 thinking 文本现在正确路由到 `invoke_sniff_buffer`，修复 bypass | HIGH |
| `src/kiro/token_manager.rs` | API Key 凭据不再触发不必要的 token 刷新（跳过刷新路径） | HIGH |
| `src/kiro/cooldown.rs` | `AuthenticationFailed` 冷却时长明确文档化为 86400s | MEDIUM |
| `src/model/config.rs` | `tool_stream_align_failure`/`tool_expose_error_to_client` 注释更正为默认开 | MEDIUM |
| `src/anthropic/stream.rs` | `repair_json_structure` 现在能处理粘连 JSON 模式（glued pattern） | MEDIUM |
| `src/kiro/cooldown.rs` | `set_cooldown_with_duration` 现在保护已有更长冷却不被缩短 | MEDIUM |
| `src/kiro/throttle.rs` | `maybe_step_up` 改用 `notify_one` 替代 `notify_waiters`，避免惊群 | LOW |
| `src/kiro/token_manager.rs` | token 过期函数补充 `expires_at=None` 语义的明确文档 | LOW |

### 2026-07-25 — 安全修复 + Opus 5 支持

**已应用的变更（6项）：**

| 文件 | 变更 | 类型 |
|------|------|------|
| `src/common/ssrf.rs` | 修复6to4 IPv6 SSRF bypass（`2002::/16`分支）+ 补4个测试用例 | 安全 |
| `src/admin/update.rs` | 新增 `MAX_DOWNLOAD_BYTES=200MiB` + 两处Content-Length预检 | 安全 |
| `src/admin/update.rs` | `http_client()` 失败时改为 `warn!` 日志而非静默fallback | Bug |
| `src/admin/update.rs` | `target_differs` → `target_is_newer`，拒绝降级 | 安全 |
| `src/kiro/health.rs` | `snapshot()` 先调 `tick_circuit()`，消除过期熔断器假象 | Bug |
| `src/anthropic/model_catalog.rs` | 新增 `claude-opus-5`（5,0），别名含带日期版 `claude-opus-5-20260715` + 测试 | 功能 |

**待后续处理（2项）：**
- `#5` ssrf.rs DNS fail-open 文档化
- `#8` token_manager.rs `expires_at=None` API Key语义对齐

### 2026-07-25 — 生态调研会话（无代码变更）

本次会话为纯研究阶段，未对代码库做任何修改。所有已知问题（#1–#8）仍为待修复状态。

研究内容：
- 审阅 hank9999/kiro.rs（Rust，axum）的模型映射、token刷新、工具名截断、流式SSE实现
- 审阅 d-kuro/kirocc（Go）的凭据生命周期、Gate Writer模式、prompt cache局限
- 搜集生态项目（jwadow/kiro-gateway、aliom-v/KiroGate、aleck31/open-kiro 等）的凭据结构和模型ID现状
- 确认 Claude Opus 5（发布日 2026-07-24）和 Claude Sonnet 5 已进入 Kiro，Sonnet 5 已在目录，Opus 5 于后续修复会话中补入

## 从生态中学到的

以下模式来自本次对 hank9999/kiro.rs、d-kuro/kirocc 及周边项目的研究，可酌情在 KiroStudio 中采用或对照验证。

**1. BufferedStreamContext 回补 input_tokens**
kiro.rs 在缓冲模式下把所有 SSE 事件先攒到 `event_buffer`，等 `ContextUsage` 事件在流末尾到达后，再回写 `message_start.usage.input_tokens`。这解决了"流开始时 token 数未知"的问题，无需两次上游请求。KiroStudio 当前 `stream.rs` 是否已实现此回补值得核查。

**2. 双重检测锁防刷新惊群**
kiro.rs 用独立的 `TokioMutex` 作为刷新串行锁，与存数据的 `parking_lot::Mutex` 分离。临界区内二次检查凭据是否已被他人刷新，避免并发请求同时触发刷新。KiroStudio `token_manager.rs` 中 `refresh_lock` 已有类似机制，可对照确认二次检查逻辑完整。

**3. 全局失败后自愈重置**
kiro.rs 的 `acquire_context` 中：若所有凭据全部因 `TooManyFailures` 被禁用，则一次性重置计数并重新启用所有凭据，等效于免重启的进程恢复。KiroStudio 的 `health.rs` + `cooldown.rs` 体系可评估是否缺少此兜底路径。

**4. Gate Writer：流式层透明重试**
kirocc 在 SSE 流中缓冲输出，检测到"仅有 thinking 无实际输出"时在流层面静默重试，客户端无感知。这比在 HTTP 层重试更精细，适合处理 Kiro 上游偶发的 thinking-only 响应。

**5. kiro-cli SQLite token 不持久化（上游 bug #4847）**
kiro-cli 刷新 token 后只保存在内存，不写回 SQLite。若 KiroStudio 将来支持从 kiro-cli SQLite 读取凭据，需在内存中缓存刷新后的 token，而非每次从磁盘重读。当前 KiroStudio 使用自有 `credentials.json`，不受此 bug 影响。
