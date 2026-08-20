# 综合兼容分析与升级计划（compat-upgrade-plan.md）

> 日期：2026-08-16。基线：v0.7.43（2026-07-25）→ HEAD（W14）。
> 对照对象：9 个参考仓（ref-zyphr、ref-k2cc、ref-new-kiro-rs-admin、ref-new-Kiro-RS-Tool、
> ref-new-freedom-kirors、ref-new-jsjm-KiroStudio、ref-new-gateway、ref-new-kirogo、ref-new-opencode-auth）。
> 数据来源：`git diff v0.7.43..HEAD`、.opencode/ISSUES.md（a-e）、docs/scheduling-master-plan.md、
> 3 路探查 agent（README + 关键文件头部）、codegraph 现状核验。
> 本文件是冲突裁决的决策记录（ISSUES (d) 补充），后续决策引用这里。

---

## 一、我方 1 个月演进规模（v0.7.43 → HEAD）

**259 文件变更，+112,382 / -7,140 行**（96 个 .rs、57 个 .md、38 个 .tsx、32 个 .ts、9 个 .py 等）。

主线（W2-W14，按 commit 时间序）：

| 领域 | 内容 |
|---|---|
| 协议/正确性 | 透传吸收覆盖（0.7.47）、SSE 多行 data、thinking 过滤收紧、redacted_thinking 漏滤修复、非流式 inline 接线、SSRF 字面量环回放行、gzip 链路修复、`/help` 独立路由 |
| 工具层 | 8 内置工具双向映射（converter.rs:1632+）、repair_tool_json 三层修复（字符级/结构级/glued）、DeepSeek 工具映射合并、TOOL_SCHEMA_INVALID 容错、图片降采样 |
| 调度 | 端点桶重构（q.* 优先 runtime.* 回退）、共享重试预算、ThrottleProfile 三档、cache 指纹、端点健康分、选号黑名单（模型 model_not_found 60s 跳过）、严格语义（gpt 请求不落 deepseek 链） |
| 运维 | 告警体系、成本统计、OTA（restart-only 语义）、回收站分页、日志暂停持久化、代挂创建时模型探测、自愈参数配置化、帮助中心知识库（55 条目） |
| 管理端 | dashboard/usage/settings 扩展、三语 i18n（66 处插值修复）、KAM 导出、接入信息页、conn tab 移除 |
| 质量 | W5 大批修复（cooldownReason 枚举化 9 变体、跨月配额自动恢复、524 终态渲染、refreshToken 更新入口、modelMapping 校验）、W10-W12 守卫清理、W13 性能面板 |
| 文档 | STATUS/CHANGELOG 体系、ref-zyphr/ref-k2cc 研究总结、scheduling 六份研究、PERFORMANCE 实测 |

结论：这 1 个月主要在做**正确性与运维完备性**（吸收层、调度、SSRF、审计、i18n），
功能面已覆盖参考仓的大多数「招牌能力」（见第二节裁决——多处已覆盖且更强）。

---

## 二、冲突点分析与裁决（每点：三方案对比 + 我方判断）

### C1. 工具映射：Kiro-RS-Tool 双向固定表 vs 我们修复式 —— **已消解，我们已覆盖**

| 方案 | 做法 | 优点 | 缺点 |
|---|---|---|---|
| Kiro-RS-Tool | 双向映射固定内置表（converter.rs:930-962：Claude Code Write/Edit/Bash/Read/Glob/Grep/LS/WebSearch ↔ Kiro 原生名 + 参数互转）+ 超长工具名 SHA256 缩短 + 反向 map | 客户端侧零改动 | 只覆盖内置工具，MCP/自定义工具透传 |
| 我们（修复式） | converter.rs:1632+ **同样有 8 内置工具双向映射**（claude_code_tool_name_to_kiro + map_tool_input_to_kiro + tool_name_map 反向恢复）+ **额外 repair_tool_json 三层修复**（非法转义 \U/裸控制符/截断补全/glued 粘连）+ flush_tool_input 半截 JSON 攒全 | 双向映射 + 修复层双保险 | set_tool_compat_mapping 开关未从 main.rs 接线（死旋钮） |
| jsjm | 只做「参数修复」不做双向映射 | 简单 | 不解决工具名不匹配 |

**裁决：保持现状（我们已融合两家），补一个接线缺口。**
- 我们同时具备 Kiro-RS-Tool 的映射表和 jsjm 的修复层，方案上已是最优组合。
- 唯一缺口：`set_tool_compat_mapping(false)`（converter.rs:1638）零调用半接线——ISSUES (c) MINOR。
  补 main.rs 从 config/env 接线（默认开，兼容现状）。
- Kiro-RS-Tool 的「流式半截 JSON 累积防护（input_json_delta 攒全可解析才发）」我们已有
  flush_tool_input 等价实现；「JSON Schema Draft 2020-12 → 07」我们 normalize_json_schema 已做。
  **无新移植项。**

### C2. 限流：freedom 自适应并发 vs 我们 RPM 精确滑窗 —— **保持 RPM，吸收两个思想**

| 方案 | 做法 | 优点 | 缺点 |
|---|---|---|---|
| freedom 自适应并发 | 按账号闭环控制 limit，429 滑动窗率 ≥10% 才乘性退避 ×0.70，5xx/524 软退避 ×0.80；延迟只做提速闸门永不降速；时间驱动自愈（闲置 15s 回爬 baseline） | 自适应，无需人工调参 | 复杂度高（六态状态机），契约冻结但缺真实故障分布数据支撑 |
| kiro-rs-admin / Kiro-RS-Tool | 429 重试 7 策略档位（failover/turbo/fast/balanced/steady/polite/custom）+ q/runtime 双限流桶 | 档位灵活 | 7 档对 4 号池过度设计 |
| 我们 | RPM 精确滑窗 + ThrottleProfile 三档 + 凭据级 RA 冷却 + 端点桶隔离 + 吸收层重试预算 | scheduling-master-plan 已论证高质量（精确滑窗+摊还 prune，5 并发实测零竞争） | 无自适应 |

**裁决：保持 RPM 体系**（scheduling-master-plan 三、四节已裁决：环形桶/RPM 锁分片/周期豁免均不做）。
**吸收 freedom 两个思想：**
1. **「429 率 ≥ 阈值才乘性退避」**：落地为 S3「最早类型化 429 保留」（scheduling C 批），
   吸收层默认关（config.rs:1559）时两级退避兜底仍成立——先补 S3 再观察是否需乘性退避。
2. **「时间驱动自愈」**：等价物 = S1 starved 位（透传池排序键补近 60s 无请求优先），
   解决同一问题（闲置号饿死）且不打破两池隔离铁律。**不移植六态状态机**（内部实现细节，面板价值低）。

### C3. 端点回退：jsjm endpointFallback vs gateway（无）vs 我们端点桶 —— **我们已超越，不移植**

| 方案 | 做法 | 优点 | 缺点 |
|---|---|---|---|
| jsjm | endpoint/alt.rs：ide/codewhisperer/amazonq 三端点轮转，瞬态 429/5xx 同凭据换端点，不耗预算、不冷却、不扣健康分（开关 endpointFallback 默认开） | 简单直接 | 三端点写死，无自适应 |
| gateway（Python） | 无端点回退 | — | — |
| 我们 | provider.rs:1204 select_endpoint 自适应派发：端点桶（bucket_id 含 host+region+target 去重）+ 429 封桶换桶 + endpoint_health 按凭据成功率 + report_endpoint_outcome 分类（402/403 不记端点失败） | 桶级隔离比 jsjm 强（同构端点去重、region 维度天然在 host 里）；自适应健康分 | 无「换端点不扣健康分」显式开关（但 402/403 已排除，语义等价） |

**裁决：不移植 jsjm**（我们已覆盖且更强）。守卫测试已钉死
（select_endpoint_must_use_endpoint_order_for_bucket_fallback 等）。jsjm 的「端点回退不耗凭据预算」
我们端点桶 429 封桶后 select_endpoint 换桶同样不耗重试预算——语义一致。

### C4. 模型路由：我们（黑名单/白名单/排序键）vs freedom customModels 模型缓存路由 —— **已定案，待实施**

| 方案 | 做法 | 优点 | 缺点 |
|---|---|---|---|
| freedom | customModels 别名表（大小写不敏感 + `-thinking` 剥离回退）> 内置 Claude/GPT 规范化 > 原样透传；模型缓存驱动凭据路由（含目标模型的凭据优先） | 正向路由，目录变更自动适应 | 缓存失效/预热风险 |
| kiro-rs-admin | 无显式模型路由（凭据池 + 代理池） | — | — |
| 我们 | 模型黑名单（负向兜底，30min TTL）+ 有效模型白名单 + 排序键 + 通配符映射（map_target + allows_model 末尾星号）+ 模型探测智能剥离候选（/v1、/anthropic 组合） | 负向已成熟，通配符已测钉 | 无正向路由，目录变更靠探测 |

**裁决：实施 ISSUES (d) 已定案的「模型感知正向路由轻量版」（P1）**——透传池三态缓存
（Confirmed/Unknown/Unsupported）+ 黑名单负向兜底；数据源现成 fetch_upstream_models。
已确认的设计约束（ISSUES (d) 原条）：Unsupported 带 TTL、预热限频（单号单飞、绝不放选号热路径
同步 fetch）、空列表不写缓存、全候选 Unsupported 退化放行、deepseek 归一化凭据跳过、
排序键首位前插 support_rank 是有意行为变化。freedom 的 customModels 别名表仅作对照
（我们 model_catalog 归一 + `-thinking` 剥离已覆盖别名需求，无需再引入表结构）。

### C5. 附加冲突（探查发现，非任务点名）

| 冲突点 | 各方 | 裁决 |
|---|---|---|
| 流完整性判定 | kirogo classifyStreamIntegrity（截断重试 2 次+轮换 3 次）vs 我们 CompletionStatus 四态（Ok/UpstreamError/TransportError/DecoderStopped）+ 空响应兜底 | **已覆盖**（我们更强：有 in-band 错误分类 + near_empty_response）。不移植 |
| 请求体超限 | jsjm 900KB 主动截断历史（留最近 4 条插占位）vs 我们 max_body_bytes 硬拒（413） | **差异真实存在**：jsjm 是网关侧修复，我们是拒绝。P1 评估移植（见不足清单 #3） |
| web_search 混合场景 | freedom/Kiro-RS-Tool websearch_loop（本地 agentic loop）vs 我们 web_search 快路径 + 回灌（websearch.rs + emit_websearch_loop_usage） | **已覆盖**。我们快路径 + 回灌 + AND 判据收紧 + 自定义同名工具不误吞（测试钉死） |
| OpenAI 协议 | gateway/kirogo/freedom 均有 /v1/chat/completions + responses | **已覆盖**（openai/convert.rs，含 web_search 工具转换、tool pairing 修复——比 gateway 强） |
| 认证 | kirogo 五认证全家桶 vs 我们 Microsoft SSO + external_idp | 我们 SSO 流程一直可用（用户确认），leg1 fail-closed 已修。无缺口 |
| 在线更新 | kiro-rs-admin GitHub Release 自动更新 vs 我们 OTA（restart-only 已接线） | 已覆盖，差 token 填写（配置项，非代码） |
| 缓存指纹 | k2cc CacheMeter 持久化 vs 我们指纹无持久化 | 8GB 约束下暂不做（ISSUES (c) 已记录） |

---

## 三、不足清单（对照全部参考仓，按「用户实际受益」排序）

| # | 不足 | 参考来源 | 用户受益 | 现状证据 |
|---|---|---|---|---|
| 1 | **模型感知正向路由未实施**（透传池三态缓存） | freedom 模型缓存路由 / ISSUES (d) 已定案 | 选号成功率↑、少 429/少失败（线上 4 通道全透传池，目录变更靠探测慢） | ISSUES (d) 第一行，设计已冻结 |
| 2 | **客户端 Key 分发**（csk_* 独立启停 + 按 key/模型/凭据聚合用量） | kiro-rs-admin | 卖 key 商业闭环；用户侧自助 | ISSUES (d) P3-35 已立项（一期 4-5 天） |
| 3 | **请求体超限主动截断**（900KB 丢最旧历史留最近 4 条） | jsjm | 超长对话不 413 失败（客户端按 token 压缩、上游按字节拒，量纲不同） | 我们 max_body_bytes 硬拒（router.rs:108 DefaultBodyLimit） |
| 4 | **最早类型化 429 保留** | zyphr / scheduling S3 | 客户端拿到真实退避语义（429→generic 覆盖丢退避指令） | scheduling C 批已规划 |
| 5 | **starved 维度缺失**（低负载恒选第一个号） | scheduling S1 / freedom 时间自愈 | 多号利用率↑、偏斜↓（线上真实偏斜源） | scheduling B 批已规划 |
| 6 | **透传池冷却标签全复用 RateLimitExceeded**（401 显示「速率限制」） | scheduling S4 | 面板可见性（真实原因） | scheduling B 批已规划 |
| 7 | **上游 RA 透传**（上游 30s 客户端拿 8s） | scheduling S2 | 客户端退避准确 | scheduling C 批已规划 |
| 8 | **多凭据导入格式**（仅 KAM，缺 Kiro-Go/CLiProxyAPIPlus） | kiro-rs-admin（KAM 1.1.2/1.8.3 + Kiro-Go + CLiProxyAPIPlus） | 迁移便利 | admin/types.rs 已对齐 KAM |
| 9 | **凭据目录批量扫描**（配置写目录自动扫 JSON/SQLite） | gateway | 配置便捷（低频场景） | 无 |
| 10 | **会话归一化/outcome 聚合/告警补全**（S6/S7/S8） | scheduling D 批 | 审计能力 | scheduling D 批已规划 |
| 11 | **set_tool_compat_mapping 半接线** | 自查（ISSUES (c) MINOR） | 接非 Claude Code 客户端时可关映射 | converter.rs:1638 零调用 |

不在清单的（已确认无差距）：工具双向映射（已覆盖）、repair_tool_json（已覆盖且更强）、
端点回退（已超越）、流完整性（已覆盖）、OpenAI 端点（已覆盖）、web_search loop（已覆盖）、
图片降采样（已覆盖，ca0cc15）、OTA（已覆盖）、k2cc h[0] 冻结（已覆盖）、continuationId sticky（已覆盖）、
429 重试 7 档（三档已裁决足够）。

---

## 四、改进升级计划（批次）

> 批次内每项：做什么 + 为什么 + 参考来源 + 工作量 + 风险。
> 每批走 WORKFLOW 生命周期：实现 → CI → 对抗 review → 落实核验 → 部署。

### P0 批次（高价值低成本，合计约 2.5-3.5 天）—— 从研究结论与调度计划里挑

| 项 | 做什么 | 为什么 | 参考来源 | 工作量 | 风险 |
|---|---|---|---|---|---|
| P0-1 | **set_tool_compat_mapping 接线**：main.rs 从 config/env 读开关（默认开） | 死旋钮收口，接非 Claude Code 客户端有退路 | 自查 ISSUES (c) | 0.5 天 | 零（默认行为不变） |
| P0-2 | **S4 透传池冷却标签独立**：按原因映射 AuthFailed/ServerError 等，面板显示真实原因 | 401 显示「速率限制」是误导，面板可见性 | scheduling B 批 | 0.5 天 | 低（纯标签映射） |
| P0-3 | **S1 starved 位**：透传池排序键补近 60s 无请求优先 | 线上真实偏斜源（4 通道全透传池） | scheduling B 批 | 0.5-1 天 | 低（不碰 health/family，守隔离铁律） |
| P0-4 | **S3 最早类型化 429 保留**：吸收层重试循环保留首个类型化 429 不被 generic 覆盖 | 客户端退避语义 | scheduling C 批 / zyphr | 0.5-1 天 | 中（吸收层行为变化，需对抗 review） |
| P0-5 | **S5 冷却余温**：冷却解除后短窗口降权 | 防满血回池再被打 | scheduling B 批 | 0.5-1 天 | 低 |
| P0-6 | **Q1-Q9 零风险组**：ramp_tier 抽共享函数 / model_counts 哈希化 / 窗口常量收敛 / 死变体标注 / 时长表常量 / tie-break 契约化 / 注释修正 | 写法质量，顺手 | scheduling A 批 | 1 天 | 零（纯重构+注释） |

**P0 合计 3.5-5 天**，建议先做 P0-2+P0-3（真实偏斜，收益立现），再 P0-1/P0-5/P0-6，P0-4 单独
批次（吸收层行为变化，需要完整验证循环）。

### P1 批次（功能增强，合计约 10-14 天）

| 项 | 做什么 | 为什么 | 参考来源 | 工作量 | 风险 |
|---|---|---|---|---|---|
| P1-1 | **模型感知正向路由轻量版**：透传池三态缓存（Confirmed/Unknown/Unsupported）+ 黑名单负向兜底，遵守 ISSUES (d) 六条设计约束 | 选号成功率↑，线上全透传池受益最大 | freedom / ISSUES (d) | 2-3 人日 | 中（排序键首位前插 support_rank 是有意行为变化；预热限频纪律必须守） |
| P1-2 | **请求体主动截断**：超限时丢最旧历史（留最近 N 条）插占位说明，替代硬拒；先统计线上 413 事件再定阈值 | 超长对话不失败 | jsjm 900KB | 1-2 天 | 中（截断语义需谨慎：只截历史不截当前/工具结果；有测试钉住） |
| P1-3 | **客户端 Key 分发一期**：ClientKeyManager + sync_system_key 收编主 key 零迁移 + middleware KeyContext + usage 归因 client_key_id + admin API + Key 管理页 | 卖 key 商业闭环第一步 | kiro-rs-admin csk_* | 4-5 天 | 中（安全关键：fs_atomic 原子写、constant_time_eq，不学 k2cc 裸写） |
| P1-4 | **S2 上游 RA 透传**：上游 RA 透传到客户端响应（Retry-After 头 + A5 分类） | 客户端退避准确 | scheduling C 批 | 1-1.5 天 | 低（先统计上游 RA 分布） |
| P1-5 | **S6/S7/S8 会话归一化 + outcome 聚合 + 告警补全** | 审计能力 | scheduling D 批 | 2-3 天 | 低-中（by_session 拆 key、account_uuid 脱敏） |
| P1-6 | **多凭据导入格式扩展**：Kiro-Go / CLiProxyAPIPlus 格式解析（从 JWT 补全邮箱/scopes/issuer） | 迁移便利 | kiro-rs-admin | 1-2 天 | 低（纯解析层） |

### P2 批次（写法/质量吸收，合计约 3-4 天）

| 项 | 做什么 | 为什么 | 参考来源 | 工作量 | 风险 |
|---|---|---|---|---|---|
| P2-1 | **k2cc test-cache.sh 方法论**：真实账单反推缓存计费正确性 | 缓存计费是钱，值得独立验证脚本 | k2cc | 0.5-1 天 | 零（测试工具） |
| P2-2 | **BTreeMap input_schema 验证**：查 OpenAI/透传路径是否绕过 normalize_json_schema，查到收益点才改 | 类型收口 + payload_hash 缓存确定性 | ISSUES (c) | 0.5 天 | 零（先查证） |
| P2-3 | **凭据目录批量扫描**（配置写目录自动扫） | 配置便捷 | gateway | 0.5 天 | 低（低频场景，需目录遍历安全校验） |
| P2-4 | **修复层补测试**：repair_tool_json 三层的反例测试补齐（目前正向覆盖多，反例少） | 修复层是「改坏即线上事故」的高危区 | jsjm INVALID-TOOL-PARAMETERS doc | 0.5 天 | 零 |
| P2-5 | **429 率阈值观察指标**：吸收层记录 429 率（为未来乘性退避决策积累数据） | 决策需数据（D 类阈值纪律） | freedom | 0.5 天 | 零（只加埋点） |
| P2-6 | **图片缩放参数对齐**：对照 freedom 1568px/400KB/JPEG85 复核我们的降采样参数 | 成本与质量平衡 | freedom | 0.5 天 | 低 |

### 不做清单（有理由）

| 项 | 来源 | 不做理由 |
|---|---|---|
| 批发号池/CDK/利润统计（wholesale/profit/key_supplier） | kiro-rs-admin | 无卖 key 生意；若启动可复用 P1-3 客户端 Key 的基建，但属二期 |
| 429 重试 7 策略档位 | kiro-rs-admin / Kiro-RS-Tool | 4 号池过度设计；ThrottleProfile 三档已裁决 |
| freedom 六态限流状态机快照 | freedom | 内部实现细节，面板价值低；我们 RPM 已高质量 |
| 自适应并发限流器整体 | freedom | RPM 精确滑窗已论证更优（scheduling-master-plan 三节） |
| rotation_bias | k2cc | 与 5s 调度冷却 + ramp_tier 三重叠加，小号池无边际价值（ISSUES (d) 已裁） |
| must_wait_for_upstream | zyphr | 我们多号多账号模型下换号真实有效，前提不同（ISSUES (d) 已裁） |
| continuationId sticky 移植 | k2cc | UserAffinityManager 已覆盖核心语义（ISSUES (d) 已裁） |
| h[0] 冻结三件套 | k2cc | 已覆盖（cch 归一 + 指纹层剥除已做，ISSUES (d) 已裁） |
| 指纹首 miss break + 85% cap | k2cc | 85% cap 无依据，L3 精确哈希链无高估问题（ISSUES (d) 已裁） |
| CacheMeter 指纹持久化 | zyphr | 8GB 约束 + 重启后计费短暂退化可接受（ISSUES (c)） |
| 端点回退（jsjm endpointFallback） | jsjm | 我们端点桶 + select_endpoint 自适应已超越（C3 裁决） |
| kirogo 流完整性分类移植 | kirogo | CompletionStatus 四态已覆盖（C5 裁决） |
| OpenAI 协议端点建设 | gateway 等 | 已覆盖（openai/convert.rs）且更强 |
| 在线更新机制重做 | kiro-rs-admin | OTA 已接线，差 token 填写（配置项） |
| openspec WHEN/THEN 规范驱动 | k2cc | 我们守卫测试体系已覆盖核心价值；仅在复杂模块重构时参考 |

---

## 五、裁决记录汇总（决策记录，ISSUES (d) 补充）

| 冲突点 | 裁决 | 理由（一句话） |
|---|---|---|
| C1 工具映射（双向表 vs 修复式） | **保持融合现状** | 我们同时具备映射表 + 修复层，唯一缺口是开关接线（P0-1） |
| C2 限流（自适应 vs RPM） | **保持 RPM** | 已论证高质量；吸收 freedom 两个思想（S3 保留最早 429 / S1 starved 时间自愈） |
| C3 端点回退（jsjm vs 我们） | **不移植，我们已超越** | 端点桶 + 健康分自适应强于三端点轮转，402/403 已排除端点失败 |
| C4 模型路由（正向 vs 负向） | **实施已定案轻量版**（P1-1） | 线上全透传池，正向路由受益最大；freedom customModels 表不引入 |
| C5 流完整性 / OpenAI 端点 / web_search loop / 认证 / OTA | **均不移植** | 已覆盖或更强（逐条见 C5 表） |
| 请求体超限（截断 vs 拒绝） | **P1 评估移植截断** | 超长对话用户真实受益；先统计线上 413 再定阈值 |
| 客户端 Key（csk_*） | **P1-3 实施一期** | 商业闭环第一步；安全纪律（原子写/常量时间比较） |
| 多格式导入 / 目录扫描 | **P1-6 / P2-3 做** | 迁移便利，成本低 |

**跟踪**：本文件裁决项进入 .opencode/ISSUES.md (d) 表；实施时按 WORKFLOW 生命周期 + 服务器验证循环。
