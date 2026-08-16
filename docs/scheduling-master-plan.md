# 调度体系最优方案（scheduling-master-plan.md）

> 2026-08-16 汇总 6 路深度研究（RPM/冷却/429/平摊/会话/审计），全部带文件:行号证据。
> 原则：**根治优先于表面优化**——真实偏斜/断层/误导性标签优先；零风险组顺手；不做有精度倒退或打破隔离的方案。

## 一、真实功能缺口（值得做，按优先级）

| # | 问题 | 证据 | 根治方案 | 工作量 |
|---|---|---|---|---|
| **S1** | **starved 维度缺失**：透传池低负载（请求间隔 >60s，RPM 滑窗归零）时恒选同 priority 第一个号，其余号零流量——线上 4 通道全透传池，**这是当前真实偏斜源** | token_manager.rs:3310-3343（5 键排序）vs 主路径 starved 位 | 透传池排序键补 `starved`（近 60s 无请求的号优先——不碰 health/family，守隔离铁律） | 0.5-1 天 |
| **S2** | **上游显式 Retry-After 断层**：上游说 30s，客户端拿 8s——上游 RA 只进凭据冷却不进客户端响应 | provider.rs:4037→:4105 | 上游 RA 透传到客户端响应（Retry-After 头 + A5 分类），先统计上游 RA 分布（upstream_trace 字段） | 1-1.5 天 |
| **S3** | **最早类型化 429 保留**：吸收层默认关（config.rs:1559），「两级退避兜住」前提不成立 | provider.rs 重试循环 | 保留最早类型化 429（不被后续 generic 覆盖——zyphr 方案评估后实施） | 0.5-1 天 |
| **S4** | **透传池冷却标签全复用 RateLimitExceeded**：401 在面板显示「速率限制」 | provider.rs:2049-2078 → :3434 | 透传池按原因映射独立标签（AuthFailed/ServerError 等），面板显示真实原因 | 半天 |
| **S5** | **冷却解除无爬坡保护**：解除即满血回池（AuthTransient/ServerError 无余温） | token_manager.rs 冷却硬门 | 排序键加「冷却余温」降权位（解除后短窗口 N 次降权） | 0.5-1 天 |
| **S6** | **透传 session_id 归一化**：同一会话跨 Kiro/透传路径 by_session 拆两个 key；account_uuid 明文进 trace；随机 UUID 兜底占 38.8% 历史请求 | provider.rs:1914 vs :4527 | ① 透传埋点用与 Kiro 同源 conversationId ② 随机兜底改 None+归桶 ③ admission-timeout 假会话归桶 ④ account_uuid 脱敏 | 1-1.5 天 |
| **S7** | **outcome 细分无聚合**：429/配额/auth 分布画不出，只能逐条过滤 | usage_stats.rs:83-87 | by_outcome 聚合表（rate_limited/quota_exhausted/auth_failed…），面板可画分布 | 1 天 |
| **S8** | **持久化失败无告警** + **镜像未播种只 warn 不 bump** | trace_db.rs:340 + main.rs:267 | F6 补告警（3 行）+ 镜像未播种联动 bump("mirror_unwired") | 半天 |

## 二、低质量根治（零风险组，顺手做）

| # | 问题 | 证据 | 根治 |
|---|---|---|---|
| **Q1** | ramp_tier 5x/2x 分档逻辑逐字复制两份（透传池已有同款分叉历史） | token_manager.rs:3926-3943 vs :3319-3336 | 抽共享函数 `ramp_tier_of(recent, total)` |
| **Q2** | model_counts_for 全表扫描 O(M)（与 counts_for 不对称） | scheduling.rs:164-171 | 改哈希查找（与 counts_for 同构） |
| **Q3** | 窗口 60s 三处硬编码 | scheduling.rs:85 + :3326/:3933 | 常量 `RPM_WINDOW_SECS` 收敛 |
| **Q4** | 冷却 9 变体中 3 个生产从未触发（AccountSuspended/QuotaExhausted/ModelUnavailable 300s 死值） | cooldown.rs:36 等 | 死变体标注或移除；ModelUnavailable 300s 死值修正 |
| **Q5** | 同事件跨路径时长不一致（401: Kiro 86400s vs 透传 180s；429: Kiro 15s vs 透传 5s）——有意但零交叉引用 | provider.rs:2054 裸字面量 | 时长表常量集中 + 交叉引用注释 |
| **Q6** | is_rate_limit_signal 裸词（stream.rs:126-137 429/quota/exhaust 宽词） | stream.rs:126-137 | 连续形态收窄（等样本再改——先记录） |
| **Q7** | 死代码：is_rpm_saturated（仅测试消费）、remove 双遍历 | scheduling.rs | 删除/合并 |
| **Q8** | tie-break 契约化：末位加 `e.id` 显式（id=创建序，零回归纯契约化，不用随机） | token_manager.rs 排序键 | 末位 e.id |
| **Q9** | 注释漂移 ×2（health.rs:248 写"第③位"实际④；provider.rs:765 写"第三项"实际第 5 项）+ A5 配置 RA 注释与行为矛盾（error_messages.rs:156） | 多处 | 修正注释 + 对齐行为 |

## 三、明确不做（有论证）

| 方案 | 不做理由 |
|---|---|
| 环形桶升级 RPM 窗口 | 秒级近似是精度倒退，当前精确滑窗+摊还 prune 已高质量 |
| RPM 锁分片 | 无竞争证据（5 并发实测零竞争） |
| 排序键两池统一 | 打破两池隔离铁律（health/family 维度语义不适用） |
| RPM 周期性豁免 | 破坏硬门保护 |
| 冷却时长完全参数化 | 无真实故障分布数据支撑调参（先修度量再调参纪律） |

## 四、实施建议批次

- **批次 A（零风险，1 天）**：Q1-Q5、Q7-Q9（共享函数/常量/死码/注释）
- **批次 B（真实偏斜修复，1.5-2.5 天）**：S1 starved + S5 冷却余温 + S4 透传标签独立
- **批次 C（客户端语义，1.5-2.5 天）**：S2 上游 RA 透传 + S3 最早 429 保留
- **批次 D（会话/审计，2-3 天）**：S6 会话归一化 + S7 outcome 聚合 + S8 告警补全
- 每批：实现 → CI → 对抗 review → 落实核验 → 部署（WORKFLOW 生命周期）

## 五、六份研究文档索引

- docs/scheduling-rpm-research.md（9 问题，P0 顺手组 + P1 收益组）
- docs/scheduling-cooldown-research.md（7 根治，R6 过期清理半小时零风险）
- docs/scheduling-429-research.md（根治排序 S1+S2 合并做）
- docs/scheduling-balance-research.md（starved 唯一实质缺口 + tie-break 契约化）
- docs/scheduling-session-research.md（P1 归一化 4 项）
- docs/scheduling-audit-research.md（9 方案，先 F6 补告警）
