# RPM 分流体系深度研究报告

日期：2026-08-16 · 范围：`src/kiro/scheduling.rs` + `src/kiro/token_manager.rs` 的 RPM 全链路
性质：静态推理 + 源码证据（本机编不过 Rust，未跑基准/实验，见文末自 review）

---

## 1. 现状分析

### 1.1 结构

`RpmTracker`（scheduling.rs:69-73）双 map，各自独立 Mutex：

```rust
hits:       Mutex<HashMap<u64, VecDeque<Instant>>>          // 每凭据精确 60s 滑窗
model_hits: Mutex<HashMap<(u64, String), VecDeque<Instant>>> // 每 (凭据 × 模型) 细分（2026-08-14 新增）
```

- 记账：`record`（:92）/`record_model`（:104），每条 push 一个 `Instant`（8B，macOS 内部 u64）。
- 剔除：`prune`（:236）从队首弹过期前缀——时间戳单调追加，过期项必然是连续前缀，摊还 O(1)，**不是** O(w) 全扫（:227-235 注释 + :499 摊还回归测试钉死）。
- 批量读：`counts_for`（:135）/`ramp_counts_for`（:196）/`model_counts_for`（:159）一次加锁取全候选，锁获取 O(n) → O(1)（:125-134 解释 43 号池一次选号最坏 129 次加锁的历史瓶颈）。
- 背压辅助：`kth_oldest_age`（:266）答「第 k 个名额何时释放」，limit 热调低时精确化 Retry-After。

### 1.2 消费链路（全仓）

**记账点——恰好 2 处**（与 inflight 占位同临界区，都在 `entries` 锁内）：

| 点 | 位置 | 说明 |
|---|---|---|
| Kiro 主路径 `commit_selection` | token_manager.rs:4389-4395 | `rpm.record` + `record_model`（model 非空时） |
| 透传池 `select_custom_api_inner` | token_manager.rs:3367-3371 | 同款，`peek_only` 分支不占位不记账（:3345-3355） |

`record_passthrough_result`（:3468）**刻意不再 record**——2026-08-10 从「上游返回后记」移到「选号占位时记」，消灭选号→记账之间一整个上游 RTT 的惊群窗口（:3461-3479 注释 + 双记警示）。

**读取点——4 组**：

1. **主路径 12 位排序键**（token_manager.rs:3700-3990）：3 个批量预取（`counts_for` :3771、`ramp_counts_for` :3775、`model_counts_for` :3784）喂给键位 ⑤ ramp_tier、⑧ model_calls_now、⑩ rpm_usage_permille，外加 p_avail 内部 rpm_pressure（:3810-3816）与饱和硬门 `is_not_saturated`（:4001）。
2. **透传池 4 位排序键**（:3308-3344）：同款 3 预取（:3293-3307），键序 priority → ramp_tier → rpm → model_calls → inflight。
3. **整池背压** `transient_wait_outcome`（:4337-4361）：`rpm_hard_gate_overload_wait` 开时，RPM 饱和号计为「将恢复的等待候选」，`release_index = fresh - limit + 1`（:4349）→ `kth_oldest_age` → `window - age` 得恢复窗口 → `Wait(RpmRecovery)`。消费在 acquire_context :4998-5022（网关内等，超 `MAX_TRANSIENT_WAIT_SECS`=20s 带 retry_after 报「繁忙」而非「已禁用」）。
4. **观测**：admin/service.rs `ratelimit_insights` 复用 `effective_saturation_limit`（:4444，pub）判饱和——调度与 UI 同一真相源，无口径漂移。

**生命周期**：删号 → `delete_credential_forced:8638` 调 `rpm.remove(id)`（scheduling.rs:285-291 **确认覆盖 model_hits**：`mh.retain(|(cid,_), _| *cid != id)`，测试 :471 钉死）；空闲条目 → `cleanup_scheduling`（token_manager.rs:8988-8991）每 5 分钟清（main.rs:538）。禁用不删号：号不再被选中 → 无新 record → cleanup 兜底。**无残留泄漏路径**。

### 1.3 参考仓对比（k2cc / zyphr）

| 维度 | kirostudio | ref-zyphr v0.7.6 | ref-k2cc v2.9.6 |
|---|---|---|---|
| 数据结构 | 中心化双 map，VecDeque<Instant> 精确滑窗 | **per-entry 内嵌** VecDeque（token_manager.rs:904），entries 锁内操作 | **环形秒桶** `[Bucket; 60]`（model/rpm.rs:24），index = now_secs % 60 |
| 时间精度 | 精确（Instant） | 精确 | 秒级近似（同秒累加，≤1s 误差） |
| 记账点 | 选号占位时（防惊群窗口最小） | 选号后 record_request（check-and-reserve 原子：:1817-1847 返回 false 让调用方重选） | handler 入口 + 成功后（:115/:132，窗口最大） |
| RPM 语义 | 软降权 + 饱和硬门（两趟）+ 背压 | **硬门排除**（rpm_exceeded :1732 直接排除出候选） | 监控/观测为主（time_until_slot :148 供等待） |
| 模型级 | **独有**（model_hits） | 无 | 无（三维 = global/cred/api_key） |
| release_index 思路 | kth_oldest_age（k=fresh-limit+1） | rpm_retry_after_secs 同款（:1788，0-based 等价） | 无 |

kirostudio 的三点独特取舍：① 记账点最早（防惊群窗口最小）；② 模型级维度（唯一）；③ 软门 + 硬门混合（排序键降权 + 整池饱和才硬门/背压），而非 zyphr 的纯硬门——单号池或全池爬坡时软门不制造网关自造 503（:3919-3921 注释有理有据）。

---

## 2. 问题清单（证据文件:行号）

### P1 `model_counts_for` 是全表扫描 O(M)，不是哈希查找
scheduling.rs:164-171：`for ((cid, m), v) in map.iter_mut() { if m == model { ... } }`——遍历**全部** (凭据,模型) 条目做字符串比较，而不是按 `(id, model)` 键 O(1) 查找。与 `counts_for`（:141 `map.get_mut(&id)`）不对称。
- 成本：M = 池内所有号 × 每号访问过的模型数（如 43 号 × 20 模型 ≈ 860 条目 × 每次选号 × 长模型名比较），主路径 + 透传池各一次。
- 注释（:156-158）的自辩是「避免逐候选构造 (id, model) 键的开销」——但 `counts_for` 逐候选 `get_mut(&id)` 已经是这个模式，构造 `(u64, &str)` 临时键做 `get_mut` 并不贵（一次 hash + 比较），全扫是 M 次比较 vs 哈希是 n 次（n=候选数，M 通常 ≫ n）。
- 现状不是灾难（模型名集合有限），但这是「O(1) 查询退化 O(M) 扫描」的静默回归，负载上去就是实打实的排序键内扫描。

### P2 `record_model` 每请求一次 String 堆分配 + 长字符串哈希
scheduling.rs:107：`map.entry((id, model.to_string()))`。全仓每请求（有模型语义的）一次堆分配。模型名可长达 30+ 字符（`claude-opus-5-2026-04-30` 类）。相比 `hits` 的 `u64` 键，这是每请求多出的分配 + 更长哈希链。

### P3 ramp_tier 计算逻辑**逐字复制两份**
token_manager.rs:3926-3943（主路径）与 :3319-3336（透传池）——同一段 `recent × (60/RAMP_RECENT_SECS) vs total` 的 5x/2x 分档逻辑，常量内联两处。改档位/改窗口折算时漏改一处即两池分叉（本项目有排序键守卫测试钉顺序，但**没有**钉数值一致性）。透传池曾有同款分叉历史（:15607-15613 注释：主路径 2026-08-04 加爬坡档，透传池漏了，靠源码守卫救回）。

### P4 窗口 60s 与折算系数硬编码，无单一真相源
- scheduling.rs:85：`window: Duration::from_secs(60)` 内联在 `new()`，无常量名。
- token_manager.rs:3326、:3933：`60 / RAMP_RECENT_SECS` 的 `60` 硬编码（必须与窗口同步，注释只说了「必须能整除 60」:2237）。
- 窗口一改三处漂移，且 ramp 折算直接错。

### P5 `is_rpm_saturated`（有锁版）生产死代码
token_manager.rs:4415-4425：生产代码全部走无锁版 `is_rpm_saturated_with_limit`（:4434），有锁版只在测试（:13637/:13961 等 8 处）里消费。带「绝不能在已持 entries 锁时调用」的死亡陷阱警告，留着是给未来调用方埋雷（恰好就是它警告的死锁）。

### P6 record/count 非原子 —— 背压假饱和竞态（软门语义下可接受，但未文档化）
选号先 `counts_for` 快照（:3771），commit 时再 `record`（:4389），两个时刻之间并发请求可能已把号打满 → 第一趟硬门（:4001）可能把「快照时未饱和、提交后饱和」的号排除 → 整池背压多等一次。zyphr 用 check-and-reserve 原子记账（:1817）杜绝此竞态，但那是它硬门语义的前提；kirostudio 是软门（饱和只降权不拒绝），此竞态最坏 = 一次多余的 250ms-2s 等待（:5005-5007），**无正确性影响**。问题在于：没有任何注释向读者解释「为什么不原子」。

### P7 `remove` 对 model_hits 两次全表遍历
scheduling.rs:287-290：`mh.keys().any(...)` 扫一遍 + `mh.retain(...)` 再扫一遍。删号低频，纯浪费，可合并。

### P8 低负载下 rpm 绝对速率维度恒 0（已文档化的已知边界）
token_manager.rs:3882-3890 自述：低负载时 ⑤⑦⑧⑨⑩ 全平局，`min_by_key` 恒选第一个 → 实测 gini 0.378、最热/最冷 6.67x。结构性兜底是 ② starved 反饥饿探测（:3901-3908，STARVATION_PROBE_SECS=180）。这是**已诚实文档化的取舍**，不是隐藏缺陷，但报告里值得显式列出：rpm 维度在低负载不参与分流，公平性靠探测兜底而非 rpm。

### P9 `model_counts_for` 的 out 预填冗余
scheduling.rs:163：`ids.iter().map(|&id| (id, 0)).collect()` 预填 n 个键，即使全 0。与 `rpm_of` 的 `get().unwrap_or(0)` 缺键语义（:3307）相比多 n 次插入。轻微，可改为只插入命中项。

---

## 3. 根治方案表

优先级：**P0** 顺手组（本次可做，零风险）→ **P1** 收益组（值得做）→ **P2** 缓做（先测后做）→ **P3** 不做（有反证）。

| # | 优先级 | 问题 | 根治 | 风险 | 工作量 |
|---|---|---|---|---|---|
| R1 | **P1** | P1 model_counts_for 全扫 | 改按 `(id, model)` 键哈希查找：out 预填 0 后 `for &id in ids { if let Some(v) = map.get_mut(&(id, model)) { prune; out.insert(id, len) } }`——把 M 次字符串比较降为 n 次哈希查找。签名不变，消费侧零改动 | 无（纯查询路径重构；测试 :422-484 已覆盖等价语义，跑一遍即验） | 小（~10 行 + 微调注释） |
| R2 | **P1** | P3 ramp_tier 双份复制 | 抽共享函数 `fn ramp_tier_of(recent: u32, total: u32) -> u8`（放 scheduling.rs 或 token_manager 常量区），两处排序键调用它；5x/2x 阈值与 RAMP_MIN_SAMPLES 一并收进函数 | 无（纯提取，行为逐位等价；透传池守卫测试 :15618 继续钉键序） | 小 |
| R3 | **P0** | P4 窗口硬编码 | scheduling.rs 提 `pub(crate) const RPM_WINDOW_SECS: u64 = 60`，`new()` 与主路径 `60 / RAMP_RECENT_SECS` 的折算都引用它（折算写 `(RPM_WINDOW_SECS / RAMP_RECENT_SECS)`，顺带满足「必须能整除」的编译期约束靠测试兜底） | 无 | 极小 |
| R4 | **P0** | P5 死代码 | 删 `is_rpm_saturated`，8 处测试改调无锁版（测试里本就无锁可传 `entry.credentials.rpm_limit` 之外的值，需小改）或标注 `#[cfg(test)]` | 无 | 极小 |
| R5 | **P0** | P7 remove 双遍历 | 合并：`let had_model = { let before = mh.len(); mh.retain(...); mh.len() != before }` | 无 | 极小 |
| R6 | **P0** | P6 非原子竞态 | 只在 `commit_selection`/背压分支注释补一段「为何不 check-and-reserve」（软门语义 + 最坏代价一次多余等待），不写代码 | 无 | 极小 |
| R7 | **P0** | P9 out 预填 | 与 R1 合并解决（哈希查找版天然只插命中项） | 无 | 并入 R1 |
| R8 | **P2** | P2 String 分配 | 模型名 intern：RpmTracker 内 `Mutex<HashMap<String, u32>>`，`record_model` 先查 intern 表取 u32 id，model_hits 键变 `(u64, u32)`；模型名集合有限（白名单/目录），cleanup 同步清空表即可 | 中（改数据结构 + 消费侧签名；intern 表无界需与 cleanup 联动） | 中 |
| R9 | **P3 不做** | 环形计数桶（k2cc 方案） | 不换。精确滑窗已摊还 O(1)，环形秒桶是**秒级近似**（同秒累加、60s 边界 ±1s），换过去是精度倒退；且 ramp 的 10s 窗口在桶方案下要扫 10 个桶，收益仅常数级内存（且当前 60s 内存上界 = 活跃速率 × 60，远非问题）。**结论：保持 VecDeque** | — | — |
| R10 | **P3 不做** | 锁分片（hits 按 id 分片） | 不换。批量读已把选号热路径锁获取降到 O(1)（:3293/:3771），record 每次 1 锁临界区微秒级；1000 RPM 下无竞争证据。锁分片引入顺序死锁风险换不到可测量的收益。真到 10k+ RPM 再议 | — | — |

**模型级维度（P2 之外的总评）**：**值得保留**。消费点 = 主路径排序键 ⑧（:3971-3975，排在 inflight ⑦ 之后做同档细分）+ 透传池第 4 位（:3341）；饱和判定刻意不消费（scheduling.rs:68「阈值/上限刻意不新增」）。有效场景 = 多模型混跑、中高负载、inflight 平局时把爆款模型摊整池——参考仓均无此维度，是差异化能力。但它的价值高度依赖「inflight 平局」这个前提，单模型池/低负载下是纯成本。**结论：保留 + R1 降查询成本；R8 缓做，先给线上负载做个 profile 再决定要不要 intern**。

---

## 4. 自 review

### 验证过的断言（有源码证据）
- remove 覆盖 model_hits：scheduling.rs:285-291 + 测试 :471-484 ✓
- 记账恰好 2 处：:3367-3371 与 :4389-4395，注释 + 双记警示（:3461-3479）✓
- ramp_tier 双份复制：:3926-3943 vs :3319-3336 逐字对比 ✓
- release_index 数学正确：fresh=5, limit=2 → k=4 即第 4 老（t4）过期时 t1-t3 已过、剩 t5=1 条 < limit ✓（与 zyphr :1788 的 0-based 等价）
- headroom 默认：config.rs:1379-1381 `default_rpm_headroom_factor() = 85`，:1603 `rpm_reserve_slots = 0`，:1604 hard_gate 默认 false ✓
- cleanup 周期：main.rs:538 每 5 分钟 ✓
- is_rpm_saturated 生产无调用：rg 全仓仅测试消费 ✓

### 诚实披露（未验证/边界）
1. **全部是静态推理，没有跑过实验**。本机 8GB 编不过 Rust，R1/R2 的「收益」是量级分析不是基准数据；P1 的 O(M) 全扫在线上 4 号池（M ≈ 4×模型数，几十条）实际**无感**——问题清单成立，但优先级是按「池子会变大」的假设排的。
2. `Instant` 8B/条是平台假设（macOS u64 内部），未实测；但这不影响量级结论。
3. P8 的 6.67x 偏斜数字是代码注释里的历史实测（:3880），不是我复测的。
4. R4 删死代码时 8 处测试的改法我没逐行确认过（测试大多能直接传 `Some(limit)` 给无锁版，但个别断言 `is_rpm_saturated(2)` 需要先取 entries——动手前要过一遍）。
5. 未检查 admin-ui 侧是否直接调 `RpmTracker` 的其它方法（观测走 service.rs 的 ratelimit_insights，已确认复用真相源，但面板上 RPM 数字的来源未追到前端）。
6. 验证循环：改动需走 skiapi Docker「验证循环」（CLAUDE.md 命令），本机无法编译确认。

### 结论
RpmTracker 整体是高质量实现：精确滑窗 + 摊还 O(1) prune + 批量读 + 删号清理覆盖 + 守卫测试，都是对的。问题集中在**模型维度**（P1/P2：查询退化 + 每请求分配）与**可维护性**（P3/P4 双份复制/硬编码）。P0+P1 组（R1-R7）是一下午的活、零行为风险；R8 intern 值得做但先测；R9/R10 明确不做（有反证）。
