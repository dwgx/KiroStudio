# 结构绊脚石清单（逻辑问题根源分析）

> 2026-08-15 产出。针对 6 个核心文件（共 61,983 行）的**结构类**问题：巨型文件/函数、
> 复杂度、状态爆炸。每项给出位置、导致的真实问题（引用本仓实测案例或代码推导）、
> 严重度、修法建议与工作量级。工作量级：S=半天，M=1-2 天，L=1 周+，XL=跨波次。
> **闭环状态（2026-08-16 核验）**：本文档列出的结构绊脚石已全部修复——#11 超大函数补 8 行为测试
> （W10，TCP mock 端到端）、#12 幽灵承重串纠错（W11）、#13 前端语言耦合改枚举（W11，前端 53 测试）、
> #14 子串匹配 3 处结构化（W10-W12）、#15 双入口提取 6 公共函数（427/365→323/257 行，W10-W12）、
> #16 调用环标注纪律（W13 勘误）。正文保留为审计快照。

## 0. 文件规模总览

| 文件 | 行数 | 函数数 | 最重函数 |
|---|---|---|---|
| src/kiro/token_manager.rs | 17,324 | 132 | acquire_context_excluding 624 行 |
| src/admin/service.rs | 12,439 | 114 | update_config_locked 1132 行 |
| src/anthropic/stream.rs | 10,601 | 108 | process_tool_use 182 行 |
| src/kiro/provider.rs | 8,098 | 32 | call_api_with_retry 1795 行 |
| src/anthropic/handlers.rs | 7,956 | 56 | handle_non_stream_request 570 行 |
| src/anthropic/converter.rs | 5,565 | 70 | map_tool_input_to_kiro 151 行 |

---

## 1. 超长函数（逻辑分支密集、边界难测）

最长的 10 个函数（估算行数，含签名与空行）：

| 函数 | 位置 | 行数 |
|---|---|---|
| call_api_with_retry | provider.rs:2671-4465 | **1795** |
| update_config_locked | service.rs:4077-5208 | **1132** |
| add_credential_with_intent | service.rs:2673-3512 | **840** |
| try_custom_api_passthrough | provider.rs:1430-2067 | **638** |
| acquire_context_excluding | token_manager.rs:4603-5226 | **624** |
| call_mcp_with_retry | provider.rs:2090-2662 | **573** |
| handle_non_stream_request | handlers.rs:3301-3870 | **570** |
| select_next_credential | token_manager.rs:3493-3993 | **501** |
| post_messages | handlers.rs:2323-2749 | **427** |
| post_messages_cc | handlers.rs:3935-4299 | **365** |

（紧随其后：add_credential_inner 353、token_manager::new 300、refresh_token_locked 261。）

**问题与案例**：

- **call_api_with_retry 1795 行**：全仓最重单函数。内含 failover 循环、吸收层策略快照、
  墙钟/次数闸、每跳错误分类与换号判据、5 条以上 bail 出口。codegraph blast radius 显示
  **无覆盖测试**。错误串的机器可读标记（`pool_permanently_exhausted=1` 等）全靠字符串
  字面量打在下游 `map_provider_error` 上，改一处分支就可能改变整池重试语义而不被测试
  察觉——正是「中文文案一改分类就失效」（handlers.rs:5037 自注）这类缺陷的温床。
- **update_config_locked 1132 行**：单函数内交织「load → 逐字段改 → hot/restart 分类 →
  TIER1/TIER3 镜像接线 → reload_config 触发」五件事，且内部散落 10+ 处「漏掉这行 →
  存盘但热路径读旧值」的注释（service.rs:4126/5021/5039/5044/5164/5177），说明每处
  接线都是手工记忆的。
- **acquire_context_excluding 624 行**：选号 + token 刷新 + 错误分流 + 池耗尽判据 +
  文案区分全在一条大链里，且同一段「any_healable 两情形判据」在函数内联写了**两遍**
  （4984-5018 与 5183-5199），代码自注「与 NoCandidate 那处同款两情形判据」——同一
  语义两份拷贝，改一处忘一处就是「只修 NoCandidate 那处时这里仍会打出旧文案」
  （5188-5190 实测记录）。
- **select_next_credential 501 行 / select_custom_api_inner 158 行**：两套选号链并行
  演进，各自维护白名单感知模型名 + 黑名单 + 排序键（3216-3247 vs 主路径），
  2026-08-09 就修过一次两者分叉（effective_model 判白名单）。

**修法**：把每个 300+ 行函数按「循环体、出口分类、文案渲染」拆成 3-5 个私有方法，
错误出口统一走一个 `fn bail_pool_state(...)` 渲染器（S-M）。工作量 M-L（需守卫测试
同步迁移，WORKFLOW §3 行号漂移常态化）。

---

## 2. token_manager.rs 单文件 11 类职责（改 A 影响 B 的耦合根源）

17,324 行的职责分区（估算行号区间）：

| 职责 | 区间 | 体量 |
|---|---|---|
| 错误类型/分类（RefreshTokenInvalid 等 + retryable 判据） | 118-261 | ~145 |
| token 刷新（social/external_idp/idc 三条链 + apply_refresh_result_fields） | 261-805 | ~545 |
| 用量限额获取 + region 候选 | 806-1157 | ~350 |
| profile 探测/分类（classify/candidate_rank/probe_all_usable_profiles） | 1149-1406 | ~260 |
| CredentialEntry 状态结构（21 字段） | 1412-1574 | ~160 |
| DisabledReason 枚举 + 自愈判据（is_self_healable_reason） | 1576-1790 | ~215 |
| 快照结构（Snapshot/Trash/Manager） | 1791-1960 | ~170 |
| MultiTokenManager 状态结构（39 字段） | 1961-2360 | ~400 |
| new() 加载 + 双份 disabled 同步 + 迁移逻辑 | 2362-2670 | 300 |
| 配置热重载（reload_config，restore 表 + 14 个镜像写点） | 2674-2796 | ~120 |
| 查询/导出/克隆/组管理 | 2797-3011 | ~215 |
| custom_api 池选号（select_custom_api_inner/黑名单/透传结果） | 3038-3492 | ~455 |
| Kiro 池选号（select_next_credential 501 行） | 3493-4047 | ~555 |
| 选号辅助（is_entry_selectable 链/rpm 饱和/admission/AIMD） | 4048-4563 | ~515 |
| 选号主链（acquire_context_excluding 624 行） | 4565-5226 | ~660 |
| current_id 管理/refresh 任务/查询 | 5228-5908 | ~680 |
| 失败报告（failure/suspicious/额度禁用/跨月恢复） | 5909-7157 | ~1250 |
| region probe / ids_needing_region_probe | 7158-8026 | ~870 |
| 凭据增删改查（add 353/delete 169/restore 120） | 8027-9188 | ~1160 |
| token 刷新锁内实现（refresh_token_locked 261） | 9189-9449 | ~260 |

**耦合案例（本会话可引用的实据）**：

- **谓词三处内联**：`kiro_selectable_count` 的过滤谓词在 3078、4984、5054 三处逐字
  内联，代码自注「两处若再分叉，这个 bug 会以另一种形式回来」（4977）——排序/过滤
  键每次改动必须三处同步，这就是「排序键改动影响黑名单判定」的机制：黑名单过滤
  （3163 `is_model_blacklisted`、4123 链内 model_blocklist）与排序键同处一个闭包，
  任一维度（ramp/rpm/balance）改动都要重走整条链。
- **两套 model 黑名单表并存**：`model_blocklist`（2052，Kiro 路径，MODEL_BLOCK_TTL
  1800s）与 `model_blacklist`（2133，custom_api 路径，MODEL_BLACKLIST_TTL_SECS 1800s）
  结构完全相同（都是 `Mutex<HashMap<(u64,String), Instant>>`），命名只差一个字母；
  且 2129 注释写「60s TTL」实际常量 1800s——注释漂移。新写者极易打错表。
- **镜像写点跨层**：reload_config（2743）要调 `anthropic::handlers::set_error_messages`
  —— kiro 层反向引用 anthropic 层（见 §6），reload 的语义完整依赖「别忘接线」。

**修法**：按职责拆文件——`token_refresh.rs`、`scheduling.rs`（两池选号 + 排序键 +
黑名单）、`credential_admin.rs`（增删改/回收站）、`persistence.rs`（persist 两兄弟 +
跨月恢复）、`health_report.rs`（失败报告/自愈）。核心难点：39 字段的 MultiTokenManager
需要拆出可组合的子结构（CooldownPool/Scheduler/CredentialStore），工作量 **XL（跨波次）**
，建议先拆「不共享锁的纯函数区」（错误分类、排序键、文案渲染，S-M）。

---

## 3. 状态字段爆炸与同一事实多份拷贝

**字段数量**：

- KiroCredentials（model/credentials.rs:39-315）：**41 字段**（凭据实体 + 12 个
  custom_api 覆盖开关 + 8 个展示/统计 + 4 个禁用状态 + 3 个 region + clone 组 3 项）。
- CredentialEntry（token_manager.rs:1413-1531）：**21 字段**（含与 KiroCredentials
  **重复的 4 项**：disabled/disabled_reason/disabled_at/quota_exhausted_at）。
- MultiTokenManager（1961-2134）：**39 字段**，其中 **13 个配置原子镜像**
  （cooldown/rate_limit/affinity/rpm_limit/headroom/reserve/hard_gate/fast_fail/
  auto_disable/priority_in_balanced/balance_weight_enabled/balance_weight_floor/
  load_balancing_mode）。

**同一事实的多份拷贝**：

1. **禁用状态四重**：`entry.disabled`（运行时真值）+ `entry.credentials.disabled`
   （持久化镜像，仅 persist_credentials 5406-5411 回写）+ `entry.disabled_reason`/
   `credentials.disabled_reason` 双份 + 磁盘文件。每次禁用/恢复必须写 entry 三件套、
   再等 persist 回写 credentials 三件套，漏一步即不一致。
2. **W5 跨月恢复案例（本会话）**：历史上 `credentials` 只有 `disabled: bool`，加载时
   对一切禁用号回填 `Manual` → 自动禁用原因（SuspiciousActivityAuto/QuotaExceeded…）
   重启变 Manual → 以 reason 为判据的自愈逻辑被击穿（1576-1604 注释坐实「整池禁用后
   永久死锁」）。修法（加 disabled_reason 持久化）本身又引入了双份拷贝。
3. **clear_transient_counters 的修复史**（1547-1573）：三条复活路径各自手写清零列表，
   三处都漏了 `consecutive_suspicious` → 复活后一次风控即秒禁。收口成一个方法后才修好
   ——这正是「同一事实多份拷贝」的教科书案例，disabled 双份还在犯同型错误。
4. **失败计数 5 个槽**：failure_count / refresh_failure_count / consecutive_suspicious /
   consecutive_passthrough_failures（自注「历史遗留死槽，不再参与健康判定」1440-1444）/
   inflight，外加 6 个时刻戳（last_selected_at/last_used_at/disabled_at/quota_exhausted_at/
   last_full_reprobe_at/last_usage_403_feature_not_supported）。

**修法**：把 CredentialEntry 的 disabled 三件套 + quota_exhausted_at **并入
KiroCredentials 唯一真值**（entry 只留派生缓存，或反过来），所有禁用/恢复路径走
`set_disabled(reason, at)` 单一收口（类似 clear_transient_counters 的做法，M）。
这直接消除 W5 类「重启变 Manual」与跨月恢复的三处清零遗漏。

---

## 4. 多套镜像/状态并存（配置热更体系）

**现有镜像体系**：

| 镜像 | 类型 | 写点 | 读点 |
|---|---|---|---|
| config | ArcSwap\<Config\> | reload_config（2782） | 冷/温读点 load() |
| 13 个 Atomic 标量 | AtomicBool/U32 | reload_config（2731-2757） | 热路径 load() |
| throttle/cooldown/rate_limiter/health 内部 | 各自 Atomic/锁 | reload_config（2758-2780） | 各自内部 |
| error_messages | OnceLock\<ArcSwap\> | **main 播种 + reload_config:2743** | 错误翻译处（websearch 7 处 + handlers） |
| mock_cache | AtomicBool+AtomicU64 | **main:584 + service.rs:5120** | passthrough.rs:504 |
| extract_thinking | AtomicBool | **router:60 + service.rs:5104** | handlers 2715/4263 |
| COMPRESSION | OnceLock\<ArcSwap\> | **仅 router.rs:62（启动一次）** | handlers 514/2570/2639/4100/4167 |
| trust_forwarded_header / strip_env_noise | TIER3 镜像 | main 装配（upstream_trace.rs:190 自注同款范式） | 各自 |

**同步靠什么保证**：唯一收口是 reload_config（2682-2785），但它**不写**
mock_cache / COMPRESSION / extract_thinking——后三者靠「想起来就写」的分散 setter
（main 播种 + service.rs 对号入座），每次新增配置项都要记得在 2-4 处接线。

**「忘了接线」的历史（本会话与仓内实据）**：

- error_messages：曾只有 main 播种、reload 不写 → 面板改表后热路径读旧表，靠 2743
  补线（注释 2741-2742「两个入口都齐」是补线痕迹）。
- mock_cache：TIER1（ArcSwap 里读）与 TIER3（setter 镜像）**曾并存**，service.rs:7949
  守卫测试「改后必须调 handlers 的 set_mock_cache_config 写进程镜像，否则热路径读旧值」
  就是防回退的钉。
- reload_config restore 表：proxy 十项（2690-2696）、default_endpoint（2709-2717）、
  三个版本串（2718-2728）都曾漏出 restore 表 → split-brain（对话路径走旧值、登录/
  探测路径走新值），17110/17154 回归测试坐实。
- COMPRESSION 镜像：router.rs:59 注释声称「admin 改配置调对应 setter 即时生效
  （extract_thinking / compression 两项）」，但 admin/service.rs 全文**没有任何
  set_compression 调用**——要么面板不暴露 compression（则注释是空头承诺），要么是
  error_messages 同款「set 无人调」的未接线状态，需要核实面板字段。

**修法**：新增「配置镜像注册表」——reload_config 统一遍历一份 `(config 字段,
  镜像 setter)` 表，消灭分散 setter（M）。短期至少把 update_config_locked 里所有
  setter 调用收口到一个 `apply_runtime_mirrors(new_config)` 私有方法（S）。

---

## 5. 复制粘贴代码

### 5a. /v1 与 /cc/v1 双入口（本会话已踩「复制粘贴双入口」）

| 函数对 | 位置 | 规范化行交集 |
|---|---|---|
| post_messages vs post_messages_cc | handlers.rs:2323-2749 vs 3935-4299 | 157/329（47%）与 157/275（57%） |
| handle_stream_request vs handle_stream_request_buffered | 2752-2808 vs 4305-4358 | 80%/82% |
| create_sse_stream vs create_buffered_sse_stream | 2938-3112 vs 4367-4521 | 42%/46% |

合计约 **250+ 行逐字重复**。代价实据（仓内注释即史）：

- 「/cc/v1 入口曾漏闸，2026-08-11 补上」（2266）——安全闸门只挂在 /v1，漏了另一份。
- 「此前 /cc/v1 只 strip 不重试，随本次补齐一并迁移至此」（4291）、「与 /v1 同款
  压缩重试循环（2026-08-11 审计缺口补齐）」（6616）——同款缺陷在双入口各修一遍。
- 「F1b 同款防泄漏，不得移出循环」（6653）、「双入口同 key」（2373/7275）——守卫
  测试成对出现（7316 vs 7338），每处行为修正都要复制进另一份 + 守卫。

### 5b. 错误分类判据三套并行（B 表翻译链 vs D 类本地错误）

- translate_upstream_error 链（handlers.rs:1158-1331）：裸 `contains()` 字符串匹配
  上游关键词（`MONTHLY_REQUEST_COUNT`/`QUOTA`/`MODEL_TEMPORARILY_UNAVAILABLE`/
  `INSUFFICIENT_MODEL_CAPACITY`…）。
- endpoint/mod.rs `default_is_*` 家族（189-263 trait + 265-615 实现）：**另一套**
  关键词判据（default_is_model_temporarily_unavailable:584 与 handlers:1260 匹配同一
  语义、不同字面量集合），handlers.rs:1743 又调 endpoint 这份。
- token_manager/provider 打的机器可读标记串（`pool_permanently_exhausted=1` 等）：
  第三套「判据」，靠字符串字面量契约对接。
- map_provider_error_for_websearch（521-595）自注「新写一套字符串匹配」（1647）——
  第四套。

**修法**：双入口合并——post_messages 与 post_messages_cc 共享核心（参数化
`cc_mode: bool` 或提取 `dispatch_messages(req, ctx, cc_mode)`），流式分发同理
（L）。错误判据统一到 endpoint 分类器 + 标记串两套、删裸串（M，注意 1231-1248 的
「不能直接删」警告：MCP/透传路径冒泡的裸串仍需兜底）。

---

## 6. 循环依赖/调用层次

**层次环**（本会话实拍）：

```
anthropic/handlers ──call_api_with_retry──▶ kiro/provider
      ▲                                        │
      │                                        ▼
      │                             token_manager.acquire_context
      │                                        │
      └──set_error_messages (2743) ◀───────────┘
```

kiro 层反向引用 anthropic 层共 4 处：token_manager.rs:2743（set_error_messages）、
passthrough.rs:504/756（mock_cache_config/resolve_msg）、endpoint/mod.rs:1372
（is_upstream_temporarily_suspended）、deepseek_schema.rs:16（converter 复用）。

**后果**：

- kiro 模块无法独立编译/单测，任何 anthropic 层改动都可能波及 kiro 层（反向边）。
- reload_config 的镜像接线因此跨层（§4），「kiro 层 → anthropic 层 → 镜像」的链路
  每次都要跨模块读代码。
- 错误分类跨 3 层协作（token_manager 打标记 → handlers 字符串匹配 → endpoint
  分类器），任何一层改判据都要三层对齐（§5b 的根）。

**修法**：把镜像/错误表下沉到独立 `runtime_state.rs` 模块（不含 handlers 依赖），
kiro 层只依赖它（M）。短期可把 set_error_messages 等 setter 迁到 middleware/AppState
持有，消除直接反向引用（S-M）。

---

## 7. 最值得先动的 3 个结构绊脚石

1. **token_manager.rs 拆文件**（§2，XL）：11 类职责、132 个函数、39 字段状态、
   谓词三处内联——「改 A 影响 B」的耦合全部出自这里。先拆无锁纯函数区
   （错误分类/排序键/文案渲染）热身，再拆 scheduling 与 persistence。
   前置条件：把 §3 的 disabled 单一事实源做掉，否则拆文件时双份状态会散得更开。

2. **disabled 状态单一事实源**（§3，M）：消除 CredentialEntry 与 KiroCredentials
   的 4 项双份拷贝，全部走 `set_disabled` 收口。直接消灭 W5「禁用原因重启变
   Manual」、跨月恢复三处清零遗漏、clear_transient_counters 同型 bug 的再发生。
   这是所有「多份状态」问题里**影响面最真实**的一个（本会话已踩）。

3. **/v1 与 /cc/v1 双入口合并**（§5a，L）：250+ 行重复 + 成对守卫 + 多次漏闸/
   漏 strip/漏重试回归史。合并后行为修正只做一次，守卫测试数量减半。
   可与 §1 的 post_messages/post_messages_cc 拆分（427/365 行）同步进行。
