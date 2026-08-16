# 深度推理审计体系研究：audit 日志 / trace / usage / 可观测性 / 告警

> 2026-08-16 · kirostudio 调度研究员输出。基于源码实读（usage 五件套 + alerting +
> upstream_trace + usage_handlers + recovery_metrics + provider/handlers 埋点路径 +
> main.rs 装配），每项问题带 `文件:行号` 证据。未跑构建（本机 8GB 编不过 Rust），
> 全部结论是静态审计 + 已有测试/实测证据的交叉验证，非运行时测量。

---

## 一、链路全景（现状）

```
热路径埋点（5 类 record 构造点）
├─ Kiro 主路径成功：emit_stream_usage / emit_buffered_usage / 非流式成功
├─ Kiro 主路径失败：fail_record（provider.rs:4471）
├─ 透传路径：handlers.rs:2729（成功与失败同点，meta.upstream_error 携带）
├─ 准入闸门超时：handlers.rs:2586（假 session "admission-timeout"）
├─ WebSearch：回灌 loop / 快路径（handlers.rs:887/928）
└─ MCP：build_mcp_record（provider.rs:793，8 个出口）
        │
        ▼
usage::pipeline（有界通道 10k，try_send 满则丢+计数，专用 OS 线程，catch_unwind 隔离）
        │  worker 逐个分发
        ├─▶ UsageStats  sink：内存聚合（小时 744 环 / 天 31 环 / by_model×2 / by_credential
        │      / rate 环 / client_agg / throughput 环）+ JSONL 落盘（usage-YYYY-MM-DD.jsonl）
        └─▶ TraceDb   sink：SQLite traces 表（WAL + NORMAL + busy_timeout=5000，
              攒批 50 条/1s，读路径先 flush，Drop 兜底）
        │
        ▼
admin API：overview / timeseries / by-model / by-requested-model / by-credential /
           recent / traces/search / rate / clients / machines / throughput / stream/live
可观测出口：recovery-metrics（进程级原子计数）+ alerting webhook（8 key）+ stats_stale watchdog
诊断工具：upstream_trace（默认关，诊断期 JSONL 落盘，独立管道）
```

**整体结论**：审计体系的「采集 → 聚合 → 查询」三段质量很高（埋点全覆盖、聚合正确性有测试钉死、
API 有分页/上限/防注入/脱敏）。主要缺口集中在**末端**：① 结果细分的聚合缺失（429/配额等
分布画不出）；② 诊断工具（upstream_trace）与告警的消费点缺失；③ 少量注释漂移与埋点口径
不一致。

---

## 二、问题清单（按深度推理清单组织）

### A. RequestRecord 质量

#### A1. error_message 截断策略不一致（空白点，中危）
| 路径 | 截断 | 证据 |
|---|---|---|
| 透传失败 | 有，`upstream_err.chars().take(400)` | provider.rs:2031 |
| Kiro 主路径 fail_record | **无**，`final_error.to_string()` 直落 | provider.rs:4491 |
| 流式/缓冲/非流式失败 | 无（`client_message()`，网关合成文案，长度受控） | handlers.rs:3048 / 4653 / 3852 |
| MCP 失败 | **整个字段缺失**（build_mcp_record 不设 error_message，outcome 失败时也恒 None） | provider.rs:793-812 |

- fail_record 的 `final_error` 在吸收层重试后是 anyhow 链，可携带上游错误原文（吸收层
  每轮 `last_error` 更新），上游 body 有 MiB 级先例（compression 那套为 5MiB 请求体存在）——
  一旦长错误体进入，JSONL 与 SQLite 单条记录无上限膨胀。
- MCP 失败错误信息在 usage 里不可见（只能翻日志），与 2026-08-11 修掉的「失败记录
  error_message 恒 NULL 盲区」（handlers.rs:3045 注释）同型，只是发生在 MCP 路径。

#### A2. outcome 细分无聚合（429/配额/auth 分布画不出，中危）
- `RequestOutcome` 有 10 个变体（record.rs:14-37），但 `Aggregate` 只存 `success/failure`
  二值（usage_stats.rs:83-87）。**429 次数、quota 耗尽次数、auth 失败次数在聚合层不存在**，
  只能 `traces/search?outcome=` 逐条过滤，面板没有「按结果类型的时间序列」。
- 这是「429 分布可观测性」缺口的直接根因：`recovery_metrics` 只有 `cooldown_triggered`
  总数（recovery_metrics.rs:92），`ratelimit_insights` 的 `recent429` 是「连续触发计数」
  当前值（service.rs:332），都不是分布/趋势。

#### A3. 双口径（requested vs upstream model）实现质量：高
- 埋点齐全：主路径 success（handlers.rs:3020-3021）、透传（2734-2735）、fail_record
  （provider.rs:4485-4486）、WebSearch（896-899）。聚合回落语义正确（usage_stats.rs:765-787），
  `by_model`/`by_requested_model` 两表独立有界 + 总量守恒有测试钉死（2000-2110）。
- 残留小瑕疵：handlers.rs:2743 注释说「只记截断后的开头」，截断实际发生在
  provider.rs:2031（meta 构造点），两处不在同一文件，注释未指明截断位置——低。

#### A4. 审计盲区：透传中间跳
- 透传 failover 链的中间跳失败只进 `tracing::warn`（provider.rs:2081-2092），usage 只记
  最终 fail_record（`retries = attempts_used` 表达换号次数，provider.rs:4490）。
- 信息不丢（日志可查），但「中间跳失败详情」不可在面板/trace 查询——低危，可接受（与
  `upstream_trace` 诊断工具互补：开 trace 时逐跳有记录）。

#### A5. session_id 魔数 `"admission-timeout"`（中危）
- 准入超时记录用假 session 占位（handlers.rs:2592）→ 所有准入超时汇聚成一个幽灵会话，
  污染 `clients`/`machines` 视图的 session 拆分（usage_stats.rs:521-541 按 session 归组），
  面板会出现「一个会话几万 RPM」的假象。且该假 session 无 client 关联，会单独成行。

### B. 聚合与落库

#### B1. 内存聚合正确性：高
- 环形桶 slot 比对清零（usage_stats.rs:723-736）、滚动窗口修复有测试（1774-1836）、
  prune 惰性 + 5 分钟后台定时（main.rs:1266-1278）、by_model 外部可控 key 有界化
  （MODEL_KEY_CAP/OTHER，usage_stats.rs:674-700，修过真实无界缺陷 741-756）。
- 残留：`by_credential`/`rate` 的 key 是配置受控的凭据 ID，无外部输入风险；`all_time`
  仅覆盖 31 天（天桶数量），文档已注明（usage_stats.rs:882）——接受。

#### B2. trace_db 落库：质量高，有一个量级注意点
- WAL + `synchronous=NORMAL` + `busy_timeout=5000`（trace_db.rs:172-175）、攒批 50 条/1s
  （156-159）、读路径先 flush 保证读写一致（349/381）、Drop 兜底（483-487）。
- 实测驱动决策正确：13.5 万行取 5 万行 42ms/6.5MB 的教训已落成 `MAX_RECENT_LIMIT=2000`
  与「禁止调回万级」的守卫（usage_handlers.rs:202-215, 604-616）。

#### B3. 双写一致性缺口：JSONL 与 SQLite 无对账（中危）
- 两个 sink 从同一 pipeline 分发（main.rs:1240-1243），一致性依赖「都不失败」：
  - trace_db flush 失败丢整批，只 `warn`（trace_db.rs:339-343）
  - JSONL 写入失败丢单条，只 `warn`（usage_stats.rs:1175-1178）
- **无交叉对账**（没有「jsonl 行数 vs sqlite 行数」的校验、无失败计数器）。
- `stats_stale` watchdog 只盯 JSONL 落盘成功（`note_data_activity` 只在 append_line
  成功时刷新，usage_stats.rs:1180-1183）——SQLite 断写不触发任何告警（见 D3-2）。

#### B4. JSONL 无保留清理（低危，长期）
- `retention_cleanup` 只作用于 SQLite traces（trace_db.rs:442-457），usage_stats.rs 全文件
  无删除旧 `usage-*.jsonl` 的逻辑。JSONL 磁盘占用无上界（当前量级小：一条 ~500B，
  一年约十几 MB，但高流量+长期运行会增长）。内存聚合 31 天有界，落盘无界——不对称。

### C. 审计 API

#### C1. 查询质量：高
- 端点齐全（overview/timeseries×2 粒度/by-model/by-requested-model/by-credential/recent/
  traces/search 多维过滤+分页/rate/clients/machines/throughput/stream-live）。
- 分页上限（`MAX_SEARCH_LIMIT=500`，trace_db.rs:28）、错误详情只进日志不进响应体
  （usage_handlers.rs:627-653 守卫）、LIKE 元字符转义（trace_db.rs:127-139）、空串归一
  （usage_handlers.rs:286-291）。前端契约匹配有端点级序列化测试（728-907）。

#### C2. 缺口：与 A2 同源
- 没有「按 outcome 聚合的 GroupStat」（失败按类型分组的视图不存在）；`by-credential`
  没有速率环之外的历史速率曲线。`ratelimit_insights` 是**当前快照**，无时间序列。

### D. 可观测性缺口

#### D1. 调度核心可观测性：当前状态有、分布/趋势无
| 维度 | 现状 | 证据 |
|---|---|---|
| 选号分布 | 无显式「每号被选中/被跳过」计数；间接可用 by-credential 请求数 | usage_handlers.rs:162 |
| 冷却分布 | 只有 `cooldown_triggered` 总数；reason 维度、时长分布无 | recovery_metrics.rs:92 |
| 429 分布 | 无聚合（A2）；`recent429` 是连续触发计数当前值 | service.rs:332 |
| 平摊效果 | **有**：retries_sum/retried_requests 双口径（usage_stats.rs:113-131）+ absorb_rounds/recovered/各 Class skipped（recovery_metrics.rs:113-154） | — |
| 每号当前状态 | **有**：ratelimit_insights（rpm/软上限/饱和/在途/冷却明细/近期 429/健康分/推断文案） | service.rs:316-339 |

结论：面板能回答「现在每个号什么样」，回答不了「429 怎么演变的、冷却都因为什么、
哪个号被冷落」。

#### D2. upstream_trace 消费点：确认无（ISSUES.md:78 未闭环）
- 全仓检索 `upstream_trace::dropped_count/written_count`：除 upstream_trace.rs 自身与测试外
  **零非定义读者**。ISSUES.md:78「MINOR：upstream_trace DROPPED/WRITTEN 计数器无消费点」
  至今未动。trace JSONL 本体也没有任何面板/API 消费（默认关是刻意的，但计数器的「开启期间
  丢了多少/写了多少」没有出口）。

#### D3. 告警覆盖盘点（W13 后）
已有 8+1 key：`absorb_retry_quota_exhausted`/`absorb_budget_exhausted`/
`absorb_pool_cooldown`/`failover_exhausted`（provider.rs:4187/4274/4299/4328）、
`pool_exhausted`+reason（token_manager.rs:5343）、`quota_exhausted`（6295）、
`credential_disabled`（6291/6512/6669/6856）、`stats_stale`（main.rs:659-665 已接线）。

仍缺：
1. **持久化失败无告警**（中危）：trace_db flush 失败、JSONL 写失败只 warn。JSONL 断写
   间接被 stats_stale 覆盖（写入失败不刷新 activity → 10 分钟后告警），但 trace_db
   （SQLite）断写**无任何覆盖**——而 SQLite 是面板「最近请求」的唯一数据源。
2. **镜像未播种不 bump**（中危，B8 遗留）：`verify_runtime_mirrors_wired` 缺失时只
   `tracing::warn`（main.rs:267-276），webhook 收不到。B8 建议的「错误码表播种失败
   （B7 联动）」未落地。
3. **usage 管道丢弃无 bump**（低危）：丢弃风暴只有幂次 warn（pipeline.rs:61-67），
   有 recovery-metrics 出口，但 webhook 无感知。
4. **upstream_trace 丢弃无 bump**（低危）：同型。
5. **全局 429 率/成功率阈值告警**（低危，设计层）：absorb 计数有出口，但没有
   「成功率跌破阈值 / 429 率超阈值」类告警——当前全部是事件型（发生即报），无状态型。

### E. 垃圾/低质量

| # | 问题 | 证据 |
|---|---|---|
| E1 | **注释漂移**：「report_if_stale 当前无进程内调用方（检查方在部署侧/未来挂点）」——已过时，main.rs:659-665 已接线 | alerting.rs:229-232 |
| E2 | 格式瑕疵：`/// 提交一条 trace（热路径调用，**非阻塞**）。///` | upstream_trace.rs:469 |
| E3 | `SENT_TOTAL` 生产代码无读者（已 allow(dead_code) 自认，测试用） | alerting.rs:66-67 |
| E4 | A5 的 session 魔数（见上） | handlers.rs:2592 |
| E5 | `Pipeline.dropped` 字段存 `&'static DROPPED` 又全局有 `DROPPED`，双引用冗余（轻微） | pipeline.rs:39-75 |
| E6 | 常量排版：`DAY_MS` 夹在函数定义之间（微不足道） | usage_stats.rs:77 |
| E7 | handlers.rs:2743 注释声称「只记截断后的开头」但截断点在他处（A3 残留） | — |

---

## 三、根治方案表

| # | 问题 | 根治 | 风险 | 工作量 |
|---|---|---|---|---|
| F1 | A2/D1：outcome 细分无聚合（429/配额/auth 分布不可见） | `Aggregate` 增加 `outcome_counts: [u64; 10]`（或按 `as_str` 的数组），`add`/`merge` 同步；`WindowSummary`/`SeriesPoint`/`GroupStat` 下发；新 API 或扩展现有 `timeseries`。serde default + 历史 JSONL 兼容（新增字段缺省 0）。前端 W13 性能面板加「失败归因」堆叠图 | 低（纯增量字段，旧数据缺省 0） | 中（结构体×3 + 序列化 + 前端图表） |
| F2 | A1：error_message 截断不一致 | fail_record 与 MCP 失败出口统一走「UTF-8 安全截断 helper」（如 500 字符，仿 upstream_trace::truncate_utf8 范式）；MCP 失败补 error_message（截断版）；给 `build_mcp_record` 加 error_message 参数 | 低（只缩不扩，行为兼容） | 小 |
| F3 | D1：429/冷却分布 | ① per-credential 429 计数环（复用 CredRateRing 模式，记录 outcome=RateLimited 的 30 秒桶）；② 冷却 reason 分布计数（bump_cooldown_triggered 处按 reason 拆，或 cooldown_snapshot 聚合）；③ `ratelimit_insights` 加 `recent429_series`（最近 20×30 秒） | 低（纯内存增量） | 中 |
| F4 | D2：upstream_trace 计数无消费点 | 镜像进 recovery_metrics：`emit` 满时 bump `upstream_trace_dropped`、writer 成功 bump `upstream_trace_written`（同 usage pipeline 范式，pipeline.rs:57/115）；面板运维页显示 | 低（原子计数，零结构改动） | 小 |
| F5 | D3-1：持久化失败无告警 | `flush_pending` 失败处 bump `trace_db_write_failed`；JSONL 写失败 bump `usage_jsonl_write_failed`（stats_stale 是间接覆盖，直报更快）；可选：每 N 条对账 jsonl/sqlite 行数，偏差 bump `usage_dual_write_diverged` | 低（bump 是零开销 no-op 未配置时） | 小 |
| F6 | D3-2：镜像未播种不 bump | `verify_runtime_mirrors_wired` 的缺失分支补 `bump_with_reason("wiring_incomplete", ...)`（B8 遗留，B7 已就位只差告警） | 极低 | 极小（3 行） |
| F7 | B4：JSONL 无保留清理 | `usage_retention_days` 周期清理：删除早于保留期的 `usage-*.jsonl`（与 SQLite retention 同周期、同配置；注意保留期=0 时的语义与 trace_db 对齐） | 低（删除文件前校验日期前缀，防误删） | 小 |
| F8 | A5：session 魔数 | 准入超时记录 `session_id` 改 None（或真实唯一 id），按 outcome=rate_limited + error_message 特征可查；同时给 `clients`/`machines` 视图加「session_id 为 None 的请求不计入 session 拆分」的既有语义确认 | 低（记录字段变动，历史数据无影响） | 极小 |
| F9 | E1 注释漂移 | 更新 alerting.rs:229-232 注释（report_if_stale 已有进程内调用方） | 无 | 极小 |

**优先级建议**：F6（极小改动补告警盲区）→ F2（防长错误体膨胀）→ F5（持久化失败直报）
→ F4（诊断工具计数出口）→ F8/F9（极小事）→ F1（中量级，随 W13 性能面板下一迭代）
→ F3（依赖 F1 的部分语义）→ F7（量级低，可排后）。

---

## 四、自 review

1. **证据可信度**：全部问题均为源码实读 + 行号；「无消费点」「无聚合」类结论经全仓
   rg 检索确认（检索范围 src/ 全部 .rs，含测试）。未跑构建：本机 8GB 编不过 Rust
   （项目硬约束），但本任务零代码改动，无编译验证需求。运行时数字（trace_db 42ms 等）
   引用 PERFORMANCE.md 既有实测，未重测。
2. **诚实边界**：
   - A1 的「final_error 可能携带 MiB 级上游 body」是推断（吸收层 last_error 更新的
     具体内容未逐行追踪），标为「风险中」而非「已发生」；至少截断是无损防御。
   - F1 的 `outcome_counts` 用数组还是 HashMap 未定（10 变体数组即可，`OtherError`
     兜底未知串），方案级不锁实现。
   - F7 JSONL 保留期语义（保留期=0 是否删除全部）未对齐 trace_db 细节，实施时确认。
3. **与既有修复的关系**：B7 启动自检已落地（main.rs:218-277，只差告警联动 = F6）；
   B8 主体已落地（pool_exhausted/quota_exhausted/stats_stale 均在），本文档只补遗留；
   双口径（A3）、滚动窗口（B1）、丢弃计数出口（C1）是既有高质量项，不重复动。
4. **未覆盖**：前端 admin-ui 的 W13 性能面板数据源匹配只从 API 侧确认（端点级序列化
   测试存在），未读前端组件代码——若需要可另开任务核前端消费字段。
