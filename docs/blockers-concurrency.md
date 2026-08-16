# 并发/状态/异步绊脚石清单（blockers-concurrency）

> 2026-08-15 并发专项审计。四路并行探查（token_manager 锁审计 / provider+scheduling+cooldown 锁与 async 陷阱 / 镜像热更接线 / block_in_place+全局态+并发测试盘点），结论经主会话汇总。
> 行号以审计当日工作树为准（token_manager.rs 已漂移至 17324 行）。
> 系列文档：blockers-config.md / blockers-engineering.md / blockers-structure.md。
> **闭环状态（2026-08-16 核验）**：本文档三处绊脚石（count_tokens block_in_place、websearch f3 测试锁、
> cooldown 落盘并发丢失）与镜像热更两断点（COMPRESSION 热更、restore 表）已全部修复
> （W10-W12：count_tokens 可注入+6 测试、websearch 测试锁修复、#8 cooldown save 串行化、
> #1 set_compression 接线、#3 restore 表补 5 项+通用守卫 18/18）。正文保留为审计快照。

## 总判定

**锁纪律执行极严**：全仓无 `static mut`；锁内零网络 IO（唯一持锁跨 await 网络的是 per-credential 刷新锁，且不持 entries 锁）；持久化全部锁外；锁序主序 `entries → 子锁` 单向一致；吸收循环内锁全语句级、sleep 全锁外；无 spawn 泄漏。
**真正的绊脚石不在锁本身**，而在三处：① 远程 count_tokens 的 block_in_place 热路径（零测试覆盖）；② 测试层对全局镜像的锁纪律缺口（websearch f3 无锁改 ERROR_MESSAGES，确定 flaky）；③ cooldown 落盘的并发丢失更新（真实正确性 bug，低概率自愈）。此外镜像热更层有 2 个潜伏接线断点（COMPRESSION 无热更入口、restore 表缺 5 字段）。

---

## 一、锁的使用审计

### entries 锁（token_manager.rs:1967，parking_lot::Mutex<Vec<CredentialEntry>>）——干净

- 全部 90+ 个 `entries.lock()` 点逐一核查：**临界区内零 IO / 零 await / 零 sleep**。
- 写路径（record_passthrough_result :3389 / 自愈块 :4706 / NoCandidate 分诊 :4869/:4979/:5053 / report_failure :5932 / delete_credential :8423）全是「块作用域释放 → 锁外 persist/affinity/save_stats」模式；:3386 注释明示 parking_lot 非可重入，persist 在锁内调用即死锁，此风险被纪律性规避。
- 持久化五处（persist_credentials :5432 / persist_trash :5534 / save_stats :5619 / load_stats / load_trash）快照短锁 clone、写盘锁外，全部 30+ 调用点无一持锁落盘。
- 子模块锁（rpm/cooldown/rate_limiter/health/affinity/model_blocklist/model_blacklist）单向依赖，无反向回调，无锁序环。

### 锁序——1 个无声雷 + 1 个脆弱点

| 位置 | 问题 | 严重度 | 修法 |
|---|---|---|---|
| token_manager.rs:2965-2974 `reserve_clone_seqs`：`clone_seq_hwm.lock()` → `max_clone_seq_in_group`（内部 entries.lock()） | 全仓唯一反序（clone_seq_hwm → entries），与主序 `entries → 子锁` 相反。当前靠「entries 私有 + token_manager 内无反向路径」维持安全（:2956-2960 注释自称全仓仅此一处），但无声无测试：未来在 entries 锁内调它即 ABBA 死锁，且并发时序死锁任何测试都抓不到 | 中（潜伏） | 加测试钉死锁序（在 entries 锁内调用 reserve_clone_seqs 必须编译失败或测试红）；或把发号挪出锁外 |
| token_manager.rs:8523-8529 delete 回滚：trash → entries 顺序锁 | 与主序相反，两块不相交故当前安全，但同属脆弱模式 | 低 | 保持现状 + 注释声明，防将来合块 |
| token_manager.rs:9213-9347 `refresh_token_locked` 持 TokioMutex 跨 await 上游网络（退避最坏 180s+） | 全文件唯一「持锁等网络」；:9347 已显式提前 drop 防队头阻塞；锁序 refresh_lock → entries 单向安全。但任何未来重构在 entries 锁内调它 → 立刻 ABBA 环 | 中（潜伏） | 守卫：注释 + 锁序测试；刷新改「锁外双检」或单飞任务模型 |

### 竞争强度

- 选号热路径嵌套锁链（entries → rpm.hits / cooldown.entries）：稳态可忽略（已从每候选 3 次锁优化为批量 1 次，scheduling.rs:129-134 记录了此前 100 并发压测打爆的历史），但风暴期 setter 与选号争同一把 cooldown 锁，放大全局选号持锁时间。吞吐瓶颈非正确性。
- `select_next_credential` 临界区约 490 行（:3499-3992），与池大小线性相关：200 号池单次选号数百 µs，1000+ RPM 下选号完全串行化；且被 acquire_context 忙等循环（MAX_RACE_RESELECT=64）反复重入放大。未来在此闭包内加任何「重操作」都会直接放大全局阻塞。

## 二、全局可变状态

`static mut` 零处。进程级状态全是 OnceLock<ArcSwap> / OnceLock<Mutex> / static Atomic，无裸全局可变。

### 测试隔离缺口

| 位置 | 问题 | 严重度 | 修法 |
|---|---|---|---|
| websearch.rs:3168 `f3_budget_exhausted_retry_after_defaults_and_overridable` 直接 `set_error_messages`（:3191/:3201）未拿锁 | **全仓唯一「改镜像没拿锁」实证**：ERROR_MESSAGES_TEST_LOCK 是 handlers.rs `error_translation_tests` mod 私有 static（:7124），websearch 结构性拿不到。双向污染：f3 整表替换（只含 mcp_failed）覆盖 handlers 锁内测试的表 → handlers 的 :7195 断言窗口内红；反向 f3 的「默认 Retry-After: 8」依赖空表，handlers 测试临时改表期间误红。当前 CI 1886 通过是调度运气 | **高** | 把 ERROR_MESSAGES_TEST_LOCK 提为 pub(crate) 跨文件共用；f3 测试持锁执行 |
| handlers.rs:7433 COMPRESSION 测试无锁 | 唯一消费者，测试内复位，当前未踩雷；但未进「各测试操作不同镜像」约定清单，范式不一致 | 低（潜伏） | 挂锁或列入约定清单 |
| machine_id.rs:17/:26 双 HashMap 无测试锁，12 个 #[test] 共享 | 写入不清理会跨测试污染；对比 BLOCKLIST_TEST_LOCK 范式是漏网 | 中 | 加串行锁或 per-test 隔离 |
| token.rs COUNT_TOKENS_CONFIG/REMOTE_COUNT_CACHE/REMOTE_COUNT_CLIENT 三件套 OnceLock 一次性、无复位 | 远程 count_tokens 路径**从启动到运行完全不可被测试注入**，零测试覆盖（语义靠注释 + 常量守卫测试维持） | **高** | 改成可注入（如 `#[cfg(test)] set_for_test` 或参数化）并补远程路径测试（含超时、失败降级、缓存命中） |
| usage/pipeline.rs:74/75、upstream_trace.rs:188/189 全局 AtomicU64 DROPPED/WRITTEN | 测试共享，靠 DROP_TEST_LOCK 差值断言 + `>=` 容差，设计已可接受 | 低 | 保持 |
| upstream_trace.rs:467 ONCE_STARTED 一次性 swap 无复位 | 测试开过 writer 线程则关不掉；靠守卫显式传参绕过全局（:513-515 注释记录并行测试实测红过） | 低（已规避） | 保持 |

## 三、async 陷阱

### block_in_place 全清单（8 处调用 + 1 处刻意不用）

| # | 位置 | 热路径 | 问题 | 严重度 |
|---|---|---|---|---|
| 1 | token.rs:129 count_all_tokens | **是** | **唯一「网络 IO + block_in_place + 每请求」组合**：miss 即 block_in_place 同步等待远程 count_tokens，10s 超时（:168，2026-08-15 从 300s 收窄，守卫测试 :589 钉死 1..=10s）。每个 miss 请求同步占 worker + blocking 线程最多 10s，并发 miss 耗尽 blocking 池，上游 RTT 直接叠进 TTFB。零测试覆盖（见全局态）。默认关闭（count_tokens_api_url 为 None），配置即踩 | **高** | 改异步：不进 block_in_place，直接 async 调用 + timeout；或彻底移到后台任务 + 缓存预热，热路径只读缓存 |
| 2 | token_manager.rs:5619 save_stats | 部分 | 从 report_success/failure 计费路径调，文件写在请求路径上；有 debounce 缓解 | 中 | 可接受，风暴期监控 fsync 延迟 |
| 3 | converter.rs:1434 图片（image_resize.rs:258 spawn_blocking） | 是 | 每请求 CPU 重活（含图片时），无超时但纯 CPU 有界；已用 spawn_blocking 正确让位 | 中 | 保持；注意上游图片尺寸上限 |
| 4-6 | config.rs:1821 / token_manager.rs:5432/:5534 | 否 | admin 存盘路径，锁外，无超时必要 | 低 | 保持 |
| 7 | cooldown.rs:728-733 | — | 刻意不用 block_in_place（Handle::try_current() 在 spawn_blocking 线程也返回 Ok，会 panic，M5 审查结论）。代价：**同步 write_atomic（含 fsync）在 tokio worker 上执行，fsync 延迟无上界**，触发时机恰是 429 风暴（:4043 吸收循环 → set_cooldown → save 链） | 中 | 单飞 + 在 spawn_blocking 中写盘（用 try_current 判线程）；或保持现状并接受 |

### cooldown 落盘并发竞态——真实正确性 bug

- cooldown.rs:670-741：`mark_dirty` → `save()` **无串行化**。两线程可同时通过 should_flush 并发 save：T2 的 set_cooldown 发生在 T1 快照后、T1 rename 前时，T1 旧快照最后落盘且 `dirty=false`（:737）无条件清除 → 最新冷却变更（含 trigger_count 退避档位）丢盘，进程在下次全量 save 前死亡则重启回退。429 风暴 + 30s debounce 边界触发。低概率、自愈，但机制真实存在、零防护 | **中**（正确性） | save 入口加互斥/单飞（AtomicBool CAS 或 tokio Mutex）；dirty 清除改为「仅当本次快照是最新」的条件清除 |

### 干净项（已核实）

- 吸收循环（provider.rs:2870-4288）：锁全语句级、退避 sleep（最长 15s）全锁外、AIMD/埋点/兜底全在循环外或 absorb_round==0 门内，与守卫清单 :6761/:6850/:6889/:6924/:6970 一致。
- provider.rs:1029-1031 明示纪律「持锁 await 是硬错误」：upstream_per_credential_gates 返回 Arc 后锁外 acquire，实测相符。
- 大 await 跨锁持有：三文件零处（唯一跨 await 的是 client_for 锁内同步构建 reqwest client，仅缓存 miss 一次，无网络）。
- tokio::spawn：三文件零 spawn，无泄漏面；全仓 spawn 全是生产代码，后台任务（refresh_loop/version_mask/alerting）有幂等/JoinHandle 管理。
- retry_pressure / endpoint_buckets / SharedRetryBudget：全语句级微秒临界区。

## 四、镜像/状态一致性（热更接线）

### 完整链路（已核实，无断点）

- **mockCache（Atomic 镜像）**：面板 flag :4341 → sanitize 双端一致 :4351/:302 → 双 OR 链 :5027/:5168 → TIER3 setter :5119 → handlers.rs:288 写序 ratio→enabled（关优先）、读序 enabled→ratio（:310，无注入残留窗口）→ main.rs:584 播种 → 唯一读点 passthrough.rs:504。**事故案例（并发会话热改 0.5→0.8→1）的 lost update 已被 config_write_lock（service.rs:4061-4073，2026-08-14 修）串行化**。
- OR 链 18 项 changed flag 全在 hot_or_display_changed（service.rs:5012-5045），无漏项；R18/背景图历史问题（AtomicBool 只存盘没 reload）已修复。
- IP/机器码黑名单：setter 双写（service.rs:4822/:4847）+ reload 双刷，读点 handlers.rs:97/:130 每请求即取。
- reload_config（token_manager.rs:2682-2785）：解析失败零副作用（:2689 先 load 再动状态）；镜像逐项 store 先于 config.store（毫秒级混合窗口，字段级独立，消费点分布决定无实际危害）；全程 config_write_lock 串行（update_config :4067 / import_config :5261 双路径都在锁内，守卫 :12138 钉死）。
- 读路径每请求 ArcSwap load_full 即取即用，唯一跨 await 持有是 compression_cfg 跨 compress_retry loop（最长 90-135s）——符合「下个请求生效」语义，非缺陷。

### 断点与缺口

| 位置 | 问题 | 严重度 | 修法 |
|---|---|---|---|
| COMPRESSION 镜像（handlers.rs:422）无热更入口，reload_config 不刷新（:2743 只有 set_error_messages） | 只由 router.rs:62 启动播种，admin/types.rs 无 compression 字段。直接改 config.json 的 compression + 同批改任一热字段触发 reload → ArcSwap 拿新值、COMPRESSION 镜像保持启动旧值；读点全走镜像（handlers.rs:514/:2639/:4100/:4167）→ 行为旧值、快照新值。与 proxy split-brain 同型（:2690-2696 修法未覆盖此镜像）；将来面板加字段忘接 setter 立即踩中，无守卫 | **中高**（潜伏） | reload_config 加 set_compression 刷新；或让读点改走 config ArcSwap 单一来源 |
| restore 表（token_manager.rs:2697-2729）缺 5 个 restart-only 字段：corsAllowedOrigins / ipAllowlist / maxBodyBytes / ingressRateLimitPerMin / trustForwardedHeader | 与 default_endpoint 历史事故（:2709-2716 承认）同型。当前消费点全部启动固化（无活读点）→ 只造成 reload 后 ArcSwap 脏值 + 面板快照与行为不一致（有「需重启」提示兜底）。表与 restart_fields 清单无对齐守卫，字段读点形态一变即 split-brain | 中 | 补全 restore 表；加守卫测试钉死「restart_fields ⊆ restore 表」 |
| service.rs:5155-5182 immediate_changed 缺 self_heal_changed | 只改自愈退避三字段时 reload 正常触发但响应回「无改动。」，文案与实际不符 | 低 | 补进 immediate_changed |
| TRUST_FORWARDED_HEADER（main.rs:533 播种，admin 无 setter） | 有意 restart-only（「不需要 admin 侧热改钩子」），但 restore 表漏（见上）→ 同批热字段触发 reload 时 ArcSwap 持脏值，仅快照显示不一致 | 低 | 随 restore 表一并处理 |
| service.rs:4495-4518 auto_disable_quota_exceeded / socks_auto_health（内存 Atomic 不落盘） | 重启回默认，响应却回「已保存并立即生效」，语义有差；注释已承认 | 低 | 改文案或落盘 |
| service.rs:4124-4126 注释声称「消费点每请求读 config ArcSwap 查表」 | 实际消费点全走 handlers 镜像，注释过时（实现完整） | 低 | 修注释防误导 |
| service.rs:5047-5049 reload 失败降级：镜像已写（新）ArcSwap 未换（旧） | 异常路径，重启生效语义明确 | 低 | 保持 |

## 五、竞态窗口（热更 vs 并发读）

- reload_config 无数据竞态窗口：换入顺序（镜像 → ArcSwap）与字段级独立消费点使毫秒级混合窗口无实际危害；读点每请求即取，无跨 await 长持有（唯一例外见上，语义可接受）。
- 配置热更与请求读的 split-brain 防护：**主防线 = restore 表**（reload 时把 restart-only 字段从磁盘值还原回 ArcSwap 旧值），该表当前缺 5 字段（见四）；**次防线 = restart_fields 提示**（面板标「需重启」）。缺对齐守卫是结构性风险。

## 六、并发测试盘点

总量：**1769 #[test] + 208 tokio::test，其中 multi_thread 42 个**。但**真并发测试只有 4 个**：

| 位置 | 测试 | 覆盖 |
|---|---|---|
| token_manager.rs:10538 | concurrent_import_of_same_api_key_must_insert_only_one | 8 任务并发 import 同 key 只插 1 条（注释：单线程 runtime 下竞态不可复现会假绿，必须 multi_thread） |
| token_manager.rs:16912 | concurrent_clone_seq_reservations_never_overlap | 8×4 并发预留分身序号，无重号无空洞 |
| throttle.rs:1020 | test_concurrent_no_overadmit | 100 并发令牌桶不超发 |
| token_manager.rs:10573 | allow_duplicate_still_permits_multiple_copies_after_recheck | 名义 multi_thread 实际串行（对照） |

其余 38 个 multi_thread 全是基础设施需要（admin 回写凭据文件走 block_in_place 26 个 / 图片路径 11 个 / 真实 HTTP server 2 个），非并发测试。

### 并发逻辑测试空白（高风险区）

| 逻辑 | 现状 | 严重度 | 修法 |
|---|---|---|---|
| **选号并发**（entries 锁临界区 + RPM 批量取回 + inflight 记账 + 惊群散热） | 零直接测试。:3481-3482 注释背书「候选读取、选中、inflight+=1 同一临界区」；:3150 记载的 100 并发压测是历史人工行为未转回归；test_low_load_selection_does_not_always_pick_first_entry 是串行 120 次 | **高** | 仿 concurrent_import 先例（multi_thread 可稳定复现），并发 N 任务同时选号断言：总数不超池、无重号、inflight 记账守恒 |
| **refresh 锁惊群并发**（per-credential TokioMutex + 条件重检） | 只有单线程模拟（:10188 直接调 refresh_token_locked 验证条件重检）；N 凭据并发刷新、N 请求排队 + 条件重检路径零测试 | **高** | 并发任务同时触发同凭据刷新，断言只发 1 次上游请求 |
| **镜像热更并发读写** | 只有 setter→getter 往返单线程测试（handlers.rs:7422/:7433）；「改配置瞬间在途请求读旧值」语义无测试 | 中 | 并发读写 ArcSwap/Atomic 镜像断言不撕裂、最终一致 |
| cooldown save 并发（见三） | 无测试 | 中 | 随 save 单飞修复一起补 |

---

## 最值得先动的 3 个并发绊脚石

1. **token.rs:129 远程 count_tokens 热路径（高）**——唯一「网络 + block_in_place + 每请求」组合：10s 超时下并发 miss 同时占 worker + blocking 池，TTFB 直线上涨，正是背景案例（count_tokens 10s 超时阻塞热路径）的现场；且 COUNT_TOKENS_CONFIG OnceLock 不可注入导致**该路径零测试覆盖**。修法：改异步调用（不进 block_in_place）+ 超时 + 失败缓存退避；配置了 count_tokens_api_url 的线上必须立即评估。**配套**：把三件套改成可注入并补远程路径测试。

2. **websearch.rs:3168 f3 测试无锁改 ERROR_MESSAGES 镜像（高）**——全仓唯一「改镜像没拿锁」实证，与 handlers 3 个锁内测试双向污染，CI 1886 通过是调度运气，flaky 风险确定；锁 mod 私有导致结构性拿不到。修法最小：ERROR_MESSAGES_TEST_LOCK 提 pub(crate) + f3 持锁。这是最便宜的高价值修复。

3. **cooldown.rs:670-741 save() 并发丢失更新（中，正确性）**——save 无串行化，429 风暴 + debounce 边界下 stale 快照最后落盘 + dirty 无条件清除，进程死亡即丢冷却与退避档位（正是该模块当初要解决的事故形态）。修法最小：save 入口 CAS 单飞 + dirty 条件清除，并补并发测试。

次选：COMPRESSION 镜像无热更入口（潜伏接线断点，动面板字段前必修）、restore 表缺 5 字段（补全 + 对齐守卫测试）、选号/刷新锁并发测试空白（两块最该补测试的并发逻辑）。
