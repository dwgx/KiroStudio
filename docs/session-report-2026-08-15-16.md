# KiroStudio 会话全景报告（2026-08-15 → 2026-08-16）

> 给 owner 的总报告。所有数字均来自 `.opencode/state.md`（W1-W13 波次记录）、`.opencode/DONE.md`（已完成+证据）、`.opencode/ISSUES.md`（a-e 问题清单）、`STATUS.md`（状态入口），未经实测的数字一律标注「估算」。
> 报告周期：2026-08-15 早（全仓审计启动）→ 2026-08-16（W13 最终收尾、最终部署）。共 13 个波次（W1-W13，含 W2.5/W11.5 两个子波次）。

---

## 0. 会话概要

| 维度 | 数字 |
|---|---|
| 波次数 | 13（W1-W13，含子波次 W2.5/W11.5） |
| 后端 CI | **1766 → 2020 passed / 0 failed（+254）**，全程 skiapi 服务器 Docker 验证循环 |
| 前端测试 | **46 → 53 pass**（W2 补 2 个 → W11 语言耦合枚举补 5 个），tsc --noEmit 干净 + pnpm build 全过 |
| 线上部署 | **4 次**：52748cf2 → a3ae8874 → edf27204 → **88270616（当前）** |
| 派发 subagent 总数 | 约 90（估算：W1 探查 6 + W2 修复 11 + W3 参考仓库 8 + W4 研究评审 10 + W5 验证修复 ~10 + W6 审计修复 ~10 + W8 smoke 7 + W9 并行 4 + W10-W12 绊脚石 ~12 + W13 收尾 ~8 + 各轮对抗 reviewer；具体数字散落各波次记录，无单一计数器） |
| 核心成果 | 51 项审计修复、4 通道上线、2 参考仓库吃透、9 路对比研究、模拟缓存、错误码可配置（42 key）、模型兼容层、16 项绊脚石+并发工程、性能仪表盘、i18n 三语 2344 键零残留 |

---

## 1. 会话全景时间线（按波次）

| 波次 | 时间 | 目标 | 交付 / 验证 / 部署 |
|---|---|---|---|
| **W1** | 08-15 早 | 全仓深度审计 | 主线 codegraph 读关键路径 + 6 探查 agent 分模块审计；问题清单落盘：11 MAJOR + 40 MINOR，0 BLOCKER |
| **W2** | 08-15 早-午 | 按清单修复 | 7 修复 agent（A-G）+ 主线 token.rs，51 项全修；CI 1766→**1845**（+79），前端 46→48；协议/安全/健壮性/接线/前端五类全覆盖 |
| **W2.5** | 08-15 午 | 对抗 review | 2 reviewer → 3 MAJOR + 8 MINOR → 2 修复 agent（H 后端 7 项 / I 前端 3 项）+ 主线 3 处守卫同步 → 全绿 |
| **W3** | 08-15 下午 | 上线 + smoke + 参考仓库 | nbus 部署 sha **52748cf2**（19.5MB），4 通道 smoketest 全过（#1 1.1s/#2 2.6s/#3 1.1s/#4 4.7s）；克隆 zyphr（47193 行）+ k2cc（28657 行），8 路总结落盘 ref-*.md，ISSUES.md 五类清单建立 |
| **W4** | 08-15 晚 | 对比研究 + 双路评审 | 8 路对比研究 + token 刷新重跑 + 2 路对抗评审；评审否决 fingerprint 首 miss break/85% cap、改设计跨月配额恢复与模型路由、降级 rotation_bias；认知更新 6 条验证 |
| **W5** | 08-15 深夜 | 模拟缓存 + 修复清单 | 模拟缓存功能（用户要求，mockCacheEnabled/ReadRatio 全链热重载 + 注入 + 前端面板 + 守卫）；修复清单 7 项全量（524 终态/RPM 精确化/refreshToken 校验/web_search AND/跨月恢复/指纹剥除）；双路 review 2 MAJOR + 15 MINOR 全修；CI **1886**；4 路深度验证 |
| **W6** | 08-15 深夜 | 模型兼容波 | 根因修复 predict_passthrough_upstream_model 与 forward 逐位对齐；登记缺口 A/B/C 全修（client_model 参数链/overload 双口径/websearch 回灌 mapped_model）；sub2api 通配符+路径段解析移植；NewAPI 学习；测试矩阵补齐；主线对抗 review 可交付；CI **1921**，未部署 |
| **W6（审计）** | 08-15 深夜 | 模型名链路审计 | 9 项清单逐项查证，范围内零修改；确认已修缺口（provider 预判），登记 A/B/C/D 四处新缺口（后续 W6 修复） |
| **W7-W8** | 08-15 晚 | 错误码配置 + 大规模 smoke | 错误码/提示词可配置化（用户需求 5 点全实现：42 key 表、校验、7 处接入点、矛盾修复、前端弹窗）；三轮对抗 review（B1-B3/M1-M5）；7 路 smoke agent（4 通道 + 透传对比 + 错误码端到端 + 模拟缓存闭环 + SSE + 前端三轮回合 + OpenAI 层）；smoke 抓出 3 bug 全修；CI **1955**，部署 sha **a3ae8874**；诊断出 websearch 结构性缺陷（待决策） |
| **W9 | 08-15 深夜 | 前端 a11y | smoke 发现的 console issues 323 条 → 0：Field div→label 隐式关联、~80 处控件修复、id/name 补全、Dialog aria-describedby、14 键 ×3 语；浏览器实测 0 告警；tsc + 48 pass，未部署 |
| **W9（晚）** | 08-15 晚 | i18n 完美化 + token 交换 | 全站硬编码中文扫尾：528 行 → 335 处理 + 193 豁免，真·UI 残留 0；309 新键 ×3 语，三语键集一致；Token 交换修复（线上 500 排障：warn 日志 + 5xx 重试 + 文案区分 + 6 测试）；CI **1961**，前端 48/0 |
| **W10** | 08-15 深夜 | 绊脚石 #11 | 前 5 大函数行为测试补写：provider.rs 端到端 mock 上游 3 测试（TCP 假上游 + MockEndpoint 注入）+ service.rs 真实更新链路 5 测试；对抗 review 修 2 BLOCKER（测试侧）；零生产改动 |
| **W11** | 08-15 深夜 | 绊脚石 #12/#13 | #12 幽灵承重串「等容量」认知纠错（shield 真实判据仅 3 英文串）；#13 前端语言耦合 5 处改枚举（cooldownCode 9 码 + duplicate_credential + 5 测试）；前端全绿 53/53；**后端 CI 被并发会话 WIP 的 3 个编译错误阻塞未验证** |
| **W11.5** | 08-15 | CI 失败修复 | 修复 #11 update_config 5 测试（helper 缺 create_dir_all 致 panic）+ #13 duplicate_token 1 测试（断言改 duplicate_credential） |
| **W10-W12** | 08-16 凌晨 | 绊脚石全量 16 项 + 并发 | 上号修复（redirect_uri/region/截断）+ 现役 bug 级 1-8 全修 + 结构设计类 9-16 全修 + 并发工程类（count_tokens 可注入/真并发测试/启动播种自检/healthz build_sha/告警扩展/alerting poison 修复）；CI **2020**；部署 sha **edf27204**（踩坑：缓存构建出动态链接坏产物，回滚强制重建）；4 通道模拟测试 |
| **W13** | 08-16 | 最终收尾四线 | ① 核验 blockers 修复真实性 14/16 有效 + 2 瑕疵 4 MINOR 全修；② docs/PERFORMANCE.md（nbus 实测硬证据）；③ 性能仪表盘 PerfDashboard（6 指标卡 + 延迟分布 + 设置开关）；④ I18N 彻底干净（4 组件 bug + ja 精修 34 处 + 术语统一 31 处 + 三语 2344 键）；CI 2020/0 + 前端 53/53；**最终部署 sha 88270616（build_sha=final）**，healthz 全绿 |

---

## 2. 主题归类

### 2.1 审计修复（W1-W2，51 项）

- **做了什么**：6 探查 agent 分模块审计（协议/安全/健壮性/接线/前端/docs 六线）+ 主线交叉验证，产出 11 MAJOR + 40 MINOR 清单；7+2 修复 agent 全量执行，2 轮对抗 review 兜底。
- **成果**：协议 15 项（websearch 非流式 SSE 违约、thinking 双计、subscription_unsupported 502→404、空响应兜底、OpenAI id 自生成）、安全 5 项（SSRF DNS rebinding 固化、gzip 错误体、webhook 重定向、no-store、fail-closed）、健壮性 9 项（MCP 墙钟 1470s、排序键 ramp_tier、结构化错误、count_tokens 300→10s）、接线 2 项（upstream_trace 953 行孤儿文件完整接线、region 探测窗口先禁后探）、前端 5 项（轮询链泄漏、死组件删除、SSO i18n）、docs 3 项。
- **验证证据**：CI 1766→1845（+79）；DONE.md 35 项抽查 + 8 项深验全过；部署 sha 52748cf2 与声明精确匹配。

### 2.2 上线部署与 smoke（W3、W8）

- **做了什么**：W3 首次部署审计修复 build（52748cf2）+ 4 通道 smoketest；W8 部署错误码配置系统（a3ae8874）+ 7 路大规模 smoke agent。
- **成果**：4 通道全通过——#1 fuckopencode（deepseek-v4-flash）200/1.1s、#2 deepseekapi（deepseek-v4-pro）200/2.6s、#3 cursorapi（claude-sonnet-4-6）**已启用** 200/1.1s、#4 pigcode（gpt-5.6-sol）200/4.7s；M1 修复线上验证（websearch 非流式返回 JSON）；微软 SSO start 端点正常；错误码系统端到端（配置→热加载→生效→恢复）实测通过。
- **smoke 抓出并修复 3 个真 bug**：message_start 注入失效（测试结构错误掩盖真实结构）、OpenAI web_search 静默丢弃（保留透传+warn）、max_tokens 超限误判 failover（本地 400 直返）。
- **验证证据**：nbus 实测全部 200 状态码；前端弹窗三轮 smoke（前两轮 BLOCKER：快照漏 admin-ui → 第三轮全 PASS）。

### 2.3 参考仓库吃透（W3，zyphr + k2cc）

- **做了什么**：克隆 ZyphrZero/kiro.rs（v0.7.6，47193 行）+ TsinHzl/kiro2cc-proxy（v2.9.6，28657 行），codegraph 建索引，8 路并行总结 + 问题深挖（zyphr 16 项含 7 MAJOR、k2cc 20 项含 10 MAJOR）。
- **成果**：两份 ref 文档落盘；移植候选 P0 三项（模型感知正向路由、跨月配额恢复+禁用持久化分层、rotation_bias）；新发现我们潜在缺陷（web_search OR 过宽、input_schema 序列化）——后者成为后续波次的直接输入。
- **验证证据**：DONE.md 30+ 条抽查逐行核实 + 6 项深验全过 + 3 处修正（常量时间比较删句、get_k_ref 例子换向、ephemeral 降级待复查）。

### 2.4 对比研究与评审（W4，9 路 + 2 路）

- **做了什么**：8 路对比研究（模型路由/禁用生命周期/选号调度/token 刷新/缓存体系/web_search/客户端 Key/错误翻译）+ token 刷新重跑（第 9 路）+ 2 路对抗评审。
- **成果（评审抓出的关键修正）**：fingerprint 首 miss break + 85% cap **否决**（哈希链铁律证明方向反了）；跨月配额恢复**改设计**（quota_exhausted_at 独立字段 + 200+ 行）；模型路由**改方案**（Unsupported TTL + 预热限频单飞，2-3 人日）；rotation_bias **降级/砍**（三重重叠无边际价值）。认知更新 6 条验证（自动禁用重启变手动**已修复**、0.6657 双轨**已实现**、sticky 无需移植、input_schema 已被兜底、防风暴已具备、web_search 收紧）。
- **验证证据**：ISSUES.md (a)(c)(d)(e) 全面落盘含研究结论；token 刷新研究结论=真实缺口是前端 refreshToken 入口（后续 W5 实现）。

### 2.5 模拟缓存 + 错误码配置（W5、W7-W8）

- **做了什么**：模拟缓存功能（用户明确要求，注入伪造值）：config mockCacheEnabled/mockCacheReadRatio + TIER3 热重载 9 环全链 + 透传池流式/非流式注入 + Kiro 池四层链隔离 + 前端设置面板 + 守卫全套。错误码配置：errorMessages 表 42 key + per-key merge + TIER1 热加载 + 校验（status/type 白名单、决策词黑名单、承重串告警）+ 7 处接入点 + 矛盾修复（B4/D10/F3 补 Retry-After）+ 前端弹窗。
- **成果**：模拟缓存 16 测试超声明（9+）；错误码用户需求 5 点全实现；三轮对抗 review 修 8 项（B1 组合校验/B2 billing_error 移除防 CC 重试风暴/B3 key 集 61→42/M1-M5）。
- **验证证据**：CI 1886→1955；smoke 实测模拟缓存注入 0.5/1.0、错误码配置→热加载→生效→恢复闭环。

### 2.6 模型兼容波（W6）

- **做了什么**：修 mapped_model 统计缺口（根因=预判只算 model_mapping 漏 deepseek fallback）；全链路审计登记缺口 A/B/C 全修（主路径 requested_model 契约、overload fallback 双口径、websearch 回灌）；sub2api 移植通配符（最长优先 + tie-break）+ 路径段解析；NewAPI 学习（巡检自动同步等进 P1/P2 分期）。
- **成果**：docs/model-compat-plan.md 分期设计（P0 完成 / P1 巡检+正向路由合并 / P2 家族限流）；测试矩阵补齐（effective_model 白名单 5 分支、select 门、trace_db 双口径、黑名单键自洽 + 源码守卫）；主线对抗 review 可交付（无 BLOCKER/MAJOR）。
- **验证证据**：CI 1886→1921；14 测试（通配符）+ 13+ 用例（预判链）；F1 通配×normalize 陷阱已测试钉住。

### 2.7 绊脚石修复（W10-W12，16 项 + 并发工程类）

- **做了什么**：按结构/协议/并发三册 blockers 文档逐项击破——上号问题（redirect_uri 消费 callback.path、auth_endpoint_for_region、回调截断、host 头、重试、脱敏日志）；现役 bug 级 1-8（set_compression 接线、otaAutoCheck 后端接线、restore 表补 5 项、mock_cache 守卫自证绿、buffered 路径补测、快照三文档统一、双层日志 filter、cooldown save 串行化）；结构设计类 9-16（model_blocklist/blacklist 合并、禁用四字段双份、超大函数补 8 行为测试、幽灵承重串纠错、前端语言耦合改枚举、子串匹配结构化、双入口提取 6 公共函数、调用环标注纪律）。
- **并发工程类**：count_tokens 可注入 + 6 测试（真打网）、websearch 测试锁修复、真并发测试 4 个、启动播种自检 21 镜像标记、healthz build_sha（build.rs 注入）、告警扩展（pool_exhausted 5 埋点 + quota_exhausted + stats_stale watchdog）、**alerting poison 修复**（锁 poison 恢复 + 无 runtime 降级——32 测试连锁崩根因）。
- **验证证据**：CI 1955→**2020**；部署 edf27204（首次缓存构建出动态链接坏产物 → 回滚 + 强制重建 + 校验 sha）；模拟测试 4 通道全过。

### 2.8 性能与面板（W13）

- **做了什么**：docs/PERFORMANCE.md——nbus 实测硬证据（2 核 Xeon E5-2680 v4 / RSS 32.5MB / p50 1185ms / 5 并发 100% 成功 / CPU 0.5%）；联网对比确认 new-api/sub2api 无公开硬基准（我们是稀缺硬证据）；overview-page 加 PerfDashboard（6 指标卡 + 延迟分布 p50/p90/p99 + 错误分布 + uptime/RSS）。
- **成果**：README 性能声明草案 §8；P1 建议（UUIDv4→v7、RPM intern、EndpointHealth 单遍历）记录待做；设置页「显示性能仪表盘」开关（localStorage，隐藏即停轮询）；三语 21 键；无新依赖。
- **验证证据**：浏览器实测；线上 healthz 显示 build_sha。

### 2.9 I18N 完美化（W9 晚 + W13）

- **做了什么**：全站硬编码中文扫尾（528 行 → 335 处理 + 193 豁免，真·UI 残留 0；309 新键 ×3 语；知识库 262 字段键化）；W13 浏览器核对抓 4 组件 bug（region-select 中文残留、useMemo 缺 t、storage 分区 5 处后端 label 直显 → storagePartitionLabel 枚举映射）；ja 精修 34 处 + 术语统一 31 处 + 联网核对术语符合主流。
- **成果**：三语键集 2344=2344=2344 一致；a11y 323 条 console issues → 0（~80 处控件修复）；浏览器实测 0 表单字段无 label、0 缺 id/name、无 React a11y 告警。
- **验证证据**：tsc + 48 pass（W9）→ 53 pass（W11 后）+ vite build 全过；`stats_stale` 等后端改动均带测试。

### 2.10 上号修复（W10-W12，redirect_uri）

- **做了什么**：移植用户调试修复——full_redirect_uri 消费 callback.path、auth_endpoint_for_region、回调读取截断、host 头、Accept、重试、脱敏日志。
- **成果**：微软 SSO 上号链路问题全量修复 + 3 测试；已随 edf27204 部署。
- **验证证据**：部署后模拟测试全过；完整登录流程（用户操作侧）仍待实测一次。

---

## 3. 数字总览

### CI 测试数变化（后端，skiapi Docker 验证循环实测）

| 阶段 | passed | 增量 |
|---|---|---|
| 基线（W1 前） | 1766 | — |
| W2 审计修复 | 1845 | +79 |
| W5 模拟缓存+修复清单 | 1886 | +41 |
| W6 模型兼容波 | 1921 | +35 |
| W7-W8 错误码配置 | 1955 | +34 |
| W9 token 交换修复 | 1961 | +6 |
| W10-W12 绊脚石全量 | **2020** | +59 |
| W13 最终收尾 | **2020** | +0 |

**合计 +254**（79+41+35+34+6+59=254，与 1766→2020 自洽）。全程 0 failed。

### 前端测试数变化

| 阶段 | 测试数 |
|---|---|
| W2 前 | 46 |
| W2-W9 | 48 |
| W11 语言耦合枚举 +5 | **53** |
| W13 | **53** |

（tsc --noEmit 干净 + pnpm build 成功全程保持）

### 部署链（nbus 38.244.34.15:8990，全部 systemd kirostudio.service）

| sha（前缀） | 内容 | 部署波次 | 备注 |
|---|---|---|---|
| 52748cf2 | W2 审计修复全量（19.5MB） | W3 | 备份 .bak-1.1.1-fixes |
| a3ae8874 | W2-W7 全部修复 + 前端 UI + 模拟缓存 + 错误码配置 | W8 | 备份 .bak-smoke2 |
| edf27204 | 绊脚石 16 项 + 并发工程类 | W12 | 首次缓存构建坏产物 → 回滚强制重建；healthz 显示 build_sha |
| **88270616** | W13 最终收尾（build_sha=final） | W13 | **当前线上**，healthz 全绿 |

### 线上通道状态（最新实测）

| 通道 | 模型 | 状态 |
|---|---|---|
| #1 fuckopencode | deepseek-v4-flash | 正常（周配额曾耗尽 24h 恢复；端口已固化 8787） |
| #2 deepseekapi | deepseek-v4-pro | 正常 |
| #3 cursorapi | claude-sonnet-4-6 | 已启用，但**号池空**（502 持续，sonnet 已 fallback 到 #1） |
| #4 pigcode | gpt-5.6-sol | 正常（W12 模拟测试时上游 502 非网关问题） |

### 派发 subagent 估算

约 **90** 个（W1 探查 6、W2 修复 11、W3 参考仓库 8、W4 研究评审 10、W5 验证修复 ~10、W6 审计修复 ~10、W8 smoke 7、W9 并行 4、W10-W12 绊脚石 ~12、W13 收尾 ~8，各轮对抗 reviewer 约 8-10）。非精确计数，各波次记录中无单一总计数器。

---

## 4. 交付物清单

### docs/ 新增文档

| 类别 | 文件 | 来源波次 |
|---|---|---|
| blockers ×6 | blockers-structure / blockers-protocol / blockers-config / blockers-concurrency / blockers-engineering / blockers-testing | W10-W13 系列 |
| error-codes ×4 | error-codes-inventory / error-codes-config-design / error-codes-config-mechanism / error-codes-client-behavior | W7-W8 |
| ref ×2 | ref-ZyphrZero-kiro.rs.md（13 机制 + 6 我们更优 + 8 问题）/ ref-kiro2cc-proxy.md（四层链/端点桶/sticky/子 Key/h[0]） | W3-W4 |
| 性能 | PERFORMANCE.md（nbus 实测 + README §8 草案 + P1 建议） | W13 |
| 设计/分期 | model-compat-plan.md（P0/P1/P2）、model-forward-routing-design.md、quota-402-design.md、cooldown-reason-i18n-design.md、p2-family-rl-response-model.md | W6/W4 系列 |

### .opencode/ 六件套

- **state.md**：W1-W13 全波次记录（最全，本次报告主来源）
- **DONE.md**：已完成工作 + 真实证据（模拟缓存 16 测试/51 项抽查深验/参考仓库 30+ 条）
- **ISSUES.md**：a-e 五类问题清单（历史已修/本波已修/现存/待做/研究中）
- **CHANGES-W2.md**：W2 修复明细
- **WORKFLOW.md** + **todo-2026-08-13.md**：流程与遗留

---

## 5. 当前状态

- **线上**：sha **88270616**，healthz 全绿；build_sha=final。
- **工作树**：203 个未提交文件（多会话并发，禁止裸 git add/commit/checkout；提交走临时 index 快照）。HEAD 仍为 1e100a2，未提交改动覆盖 W2-W13 全部工作。
- **遗留待决策项**：
  1. **websearch 结构性缺陷**（W7-W8 诊断）：快路径 MCP 硬依赖 Kiro 池号，纯透传池时透传拦截（静默失效）或无号 502。修复选项 D 补 Kiro 号（零代码，推荐）/ A 判定前移 / B 无号降级转发 / C 大工程——**待 owner 拍板**。
  2. **v1.1.2 发版**：release 产物（v1.1.1）落后线上全部 W2-W13 修复，从 release 下载安装会缺全部修复。
  3. **opencode 配置切换**：kirostudio provider 仍指向 k1ro.skiapi.dev（冷却中），建议切 api.dwgx.top + deepseek-v4-pro（需用户 key）。
  4. gpt-5.6-sol 请求来源排查（03:12 两条未知客户端）。
  5. native_thinking_effort_enabled 是否开启（默认关，待上号实测）。
  6. OTA token 未填、OTA 自动检查默认关；upstream_trace 默认关未开。
  7. 微软 SSO 完整登录流程实测一次（回调路径已修，待用户操作）。
  8. 生图三层不通（路由 404 + catalog 400 + 上游 key 无 image 权限）。

## 6. 风险与关注

- **并发会话干扰（最高关注）**：W11 后端 CI 被并发会话 WIP 的 3 个编译错误阻塞（handlers.rs:2627/4100 i32↔u32、cooldown.rs:743 丢类型注解）；W12 记录并发会话踩踏配置（建议错峰）；工作树 203 未提交文件本身就是多会话并发证据——**建议单会话推进 + 错峰跑 CI**。
- **fuckopencode 周配额**：Go 侧限额，24h 恢复；IP 日窗（200 请求/天 UTC 零点）仍在。属上游行为，识别正确。
- **cursorapi 号池空**：502 持续，sonnet 已 fallback 到 #1，但长期依赖单通道有单点风险。
- **pigcode 502**：W12 模拟测试确认上游 502 非网关问题（上游侧），随时可能复发。
- **部署构建缓存坑**：edf27204 首传缓存构建出动态链接坏产物（static-pie 正常形态被破坏），已回滚强制重建——后续部署需校验 sha + 确认产物形态。
- **本机 8GB 编不过 Rust**：全部验证依赖 skiapi 服务器 Docker 循环，CI 存在排队窗口（尤其并发会话时）。
- **数据口径**：subagent 总数 90 为估算；部分波次（W10 后端 CI、W11 后端 CI）受并发阻塞未在当波验证，最终由 W10-W12 合并验证兜底。

---

*报告生成：2026-08-16。数据来源：.opencode/state.md、.opencode/DONE.md、.opencode/ISSUES.md、STATUS.md。*
