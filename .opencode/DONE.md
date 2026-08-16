# KiroStudio 已完成工作总清单（含真实证据）

> 更新：2026-08-16（追加 W6-W13 波次记录 + W13 核验结论）。本文件是「做过的 + 真实证据」的权威记录，
> 由 4 路 + 5 路核验 subagent 逐项核对代码/测试/线上状态后落盘。与 .opencode/ISSUES.md、STATUS.md、state.md 同步。
> 验证方法：每项读实际代码（文件:行号）+ 测试名 + 部署状态；CI 数字为 skiapi Docker 实测。
> W1-W5 详细记录见下文一至六节；W6-W13 见七至十二节。会话全景报告：docs/session-report-2026-08-15-16.md。

## 一、模拟缓存功能（W5，✅ 4 路验证全过，16 测试超声明）

| 层 | 证据 | 验证 |
|---|---|---|
| config 字段 mockCacheEnabled/mockCacheReadRatio（默认关/0.7，serde camelCase） | config.rs:756-764 + config.example.json:81/84 | ✅ 实测 |
| TIER3 热重载 9 环全链（config→main 播种→PUT→同步块→双 OR 链→reload→setter→原子镜像→每请求读） | main.rs:584 + types.rs:1306-1356 + service.rs:4132-4147/4791/4929/4880 + handlers.rs:269-288 + passthrough.rs:504 | ✅ 逐环核对 |
| 注入：流式 message_start/message_delta + 非流式 usage（read=round(input×ratio)、creation=0、5m/1h 置 0） | passthrough_think_filter.rs:564-612/:95-137/:671-685 | ✅ 公式推导 |
| 无 input 跳过 + 解析失败 fail-open + debug 日志 + content 缺失仍注入（review 修复） | :116-126（注入在 content 早退之前）+ :118-122 | ✅ |
| Kiro 池四层链隔离（唯一调用方是透传路径） | rg 全仓：mock 引用仅镜像/setter/透传 | ✅ |
| 前端设置区（开关+步进器 0-100%+空串防御+小数取整）+ 三语 5 键 | settings-page.tsx:2826-2845/:1908-1918 + en/zh/ja:1396-1400 | ✅ tsc 实测 EXIT 0 |
| 守卫测试 mock_cache_config_is_fully_wired（删任一处接线必红） | service.rs:7670 | ✅ |
| 行为测试 12 个 + 补充 3 个（sanitize 非有限值/镜像往返/反序列化 None） | passthrough_think_filter.rs:1632-1813 + handlers.rs:6648/6676 + types.rs:2110 | ✅ 超声明（声明 9+，实际 16） |

## 二、修复清单 7 项（W5，✅ 验证全过，1 处 MINOR 建议已补）

| 项 | 证据 | 验证 |
|---|---|---|
| 524 终态渲染（`: 524` 状态行 + `"524 a timeout occurred"` 连续形态，不裸匹配数字） | handlers.rs:1324-1325 + 503+Retry-After :1836-1856 | ✅ 反例 2 + 正向 3（连续形态测试本轮已补） |
| RPM release_index 精确化（kth_oldest_age O(1) + 等待第 fresh-limit+1 个过期） | scheduling.rs:266-275 + token_manager.rs:4253-4261 | ✅ 公式推导 + 2 测试 |
| update_refresh_token 校验（截断+跨凭据去重+api_key 类型闸+trim） | service.rs:1071-1110 + 5 测试（11637-11800） | ✅ |
| 前端「更新 Token」入口（按钮+对话框+横幅+mutation+三语 11 键） | credentials.ts:316-325 + use-credentials.ts:211-220 + credential-card.tsx:851-869/1091-1177 | ✅ 三语集合实测一致 |
| web_search OR→AND（判据单一口径 + tool_choice 闸 + 归一化补 WebSearch 大写 key 缺陷修复） | websearch.rs:279-284/321-346 + converter.rs:2187-2198 + 测试 4 新 2 改 | ✅ |
| 跨月配额恢复（quota_exhausted_at + 12h 月初缓冲 + 懒触发 + 幂等） | token_manager.rs:6445-6500 + credentials.rs:281-285 + 测试 8 个（含闭环/缓冲/时间戳往返） | ✅ |
| system-reminder 指纹剥除（标签对剥除，转发字节不动） | cache_fingerprint.rs:420-441/451-477/498-525 + 2 测试 | ✅ UTF-8 边界推导安全 |

## 三、W2 全量 51 项（✅ 35 项抽查 + 8 项深验 + 部署 sha 匹配）

### 深验 8 项（全过）
1. websearch 快路径非流式 JSON：websearch.rs:998 build_fast_path_json_body + :1148-1152 stream 分支 + 守卫 :2953
2. subscription_unsupported→404：handlers.rs:1090 + 守卫 :6458/:5033
3. pinned_streaming_client SSRF 固化：http_client.rs:944（单次解析→逐 IP 复验→固化→缓存）127.0.0.1/198.18.0.0/15 本机代挂放行 + 4 测试
4. upstream_trace 接线：kiro/mod.rs:27 + main.rs:519 + provider 6 构造点 + 17 分类 + 2 守卫
5. region 探测窗口先禁后探：service.rs:2866（探测前禁用）+ 守卫 :7334
6. 排序键 12 位含 ramp_tier：token_manager.rs:3891-3904 + 透传池 :3220-3245 + 守卫 :14726/:14824
7. near_empty_response 两路径共用：stream.rs:1270 + handlers.rs:3330
8. PollGuard：admin-ui/lib/poll-guard.ts + login-dialog.tsx:66/98-101 + 8 测试

### 抽查 35 项（34 过，1 处文档未落实——本轮已补）
- 协议 15 项全过（埋点/双计回归/口径/repair/占位/压缩修复/OpenAI id/gzip/URL 收紧/空响应/TTL 30min）
- 安全 5 项全过（redirect none/no-store/500 去细节/fail-closed/回显）
- 健壮性 9 项全过（MCP 墙钟/观测计数/结构化错误/10s 超时/抽样清理/LRU 阈值/ratelimit 观测/tag 跳过/⑨⑩⑪ 守卫）
- 前端 5 项全过（轮询链/快照/SSO i18n/死组件删除/storage）
- docs 3 项：ARCHITECTURE 12 键 ✅ + AuthTransient ✅ + **压缩 4 层 ⚠️ 未落实——本轮已补**（ARCHITECTURE.md:97 + compressor.rs 头注释）

### 部署状态
- **nbus 实测 sha256sum = 52748cf2**（与声明精确匹配，线上跑的是 W2 build）

## 四、参考仓库研究产出（✅ 30+ 条抽查，6 项深验全过，3 处修正）

| 产出 | 证据 | 验证 |
|---|---|---|
| docs/ref-ZyphrZero-kiro.rs.md（13 机制 + 6 我们更优 + 8 问题） | 抽查 15+ 条逐行核实 | ✅ 准确（discovery_rank :1868→:1900 微漂移已容） |
| docs/ref-kiro2cc-proxy.md（四层链/端点桶/sticky/子 Key/h[0] 等） | 抽查 20+ 条逐行核实 | ✅ 2 处修正已落盘（常量时间比较删句、get_k_ref 例子换向） |
| 问题深挖 zyphr 16 项 + k2cc 20 项（进 ISSUES (c)/(e)） | 六项深验全过（三态路由/fingerprint 死代码/跨月恢复/AND 判定/h[0] 冻结/源文件重载） | ✅ k2cc ephemeral MAJOR 降为待复查 |
| 认知更新 6 条（自动禁用已修/0.6657 已实现/sticky 无需/input_schema 兜底/防风暴已具备/web_search 收紧） | 逐条读代码核实 | ✅ 4 条完全成立 + 2 条论据修正已落盘 |

## 五、CI / 验证证据（全部实测）

| 项 | 数字 | 方式 |
|---|---|---|
| 后端全量测试 | **1886 passed / 0 failed**（基线 1766 → 1886，+120） | skiapi Docker 验证循环（cargo test --no-default-features） |
| 前端 | tsc --noEmit 干净 + **48 pass / 0 fail** + pnpm build 成功 | 本机实测 |
| 关键测试单跑 | 9+ 项逐个确认执行（mock 注入×2/守卫/跨月/闭环/RPM/web_search/reminder/524/truncated） | 服务器实测 |
| 线上 smoketest（W3） | 4 通道全通（#1 1.1s/#2 2.6s/#3 1.1s/#4 4.7s）+ websearch 非流式 JSON 验证 | nbus 实测 |
| 部署 | sha256 52748cf2 已上线，备份 .bak-1.1.1-fixes，healthz 全绿 pool=4 | nbus 实测 |

## 六、验证过程发现并已修复的问题（闭环）

1. compressor 头注释/ARCHITECTURE「压缩 2 层」与实现 4 层不符 → 已补（本轮）
2. 524 连续形态判据无直接测试 → 已补正向用例（本轮）
3. ref-kiro2cc-proxy.md「常量时间比较」与 ISSUES 自相矛盾（实测普通 ==）→ 已删句
4. ref-kiro2cc-proxy.md get_k_ref 例子（opus-4.7.1 落 sonnet 不成立，opus 命中 2.36 兜底）→ 已换向
5. ISSUES.md (c)/(d) 停在 W4（web_search/524/refreshToken/reminder 还标待做）→ 已同步 W5
6. ISSUES.md fingerprint「误杀真命中」论据自相矛盾（浅段 miss ⟹ 深段不可能匹配）→ 已重写论据
7. k2cc ephemeral「不生效」MAJOR 证据不足 → 降为待复查

## 七、W6 模型兼容波（2026-08-15 深夜，CI 1921/0，未部署）

| 项 | 证据 | 验证 |
|---|---|---|
| 根因修复：透传 mapped_model 预判只算 model_mapping 漏 deepseek fallback | `predict_passthrough_upstream_model`（provider.rs:1385，与 forward 改写链逐位对齐：豁免/顺序/cfg merge/白名单） | ✅ 8 测试 |
| 登记缺口 A/B/C 全修（主路径 requested_model 契约 / overload fallback 双口径 / websearch 回灌 mapped_model） | call_api 系 client_model 参数链 + handlers.rs 4 处成功埋点 + run_round 三元组 + 埋点 | ✅ 主线对抗 review 可交付（无 BLOCKER/MAJOR） |
| sub2api 移植：通配符（末尾 `*` 最长优先 + tie-break）+ 路径段解析（resolve 剥 models/ + 末段，含 [1m] 顺序） | model_mapping.rs + 14 测试 | ✅ |
| 测试矩阵补齐（effective_model 白名单 5 分支 / select 门 / trace_db 双口径 / 黑名单键自洽 + 源码守卫） | 1439 核心零覆盖 → 5 分支 | ✅ F1 通配×normalize 已测试钉住 + 文档警示 |
| 设计文档 | docs/model-compat-plan.md（P0 完成 / P1 巡检+正向路由合并 / P2 家族限流等 + 不做清单 + 8 守卫规划） | ✅ |

## 八、W7-W8 错误码配置系统 + 大规模 smoke（2026-08-15 晚，CI 1955/0，部署 a3ae8874）

| 项 | 证据 | 验证 |
|---|---|---|
| 错误码/提示词可配置化（用户需求 5 点全实现） | config errorMessages 表 42 key + per-key merge + TIER1 热加载 + 校验（status/type 白名单、决策词黑名单、承重串告警、整表拒绝）+ 7 处接入点（map_provider_error 12 分支 + 翻译链 + 透传 + websearch + 双入口）+ 矛盾修复（B4/D10/F3 补 RA）+ 前端弹窗（分页/搜索/编辑/恢复默认/校验回显/defaults 接口） | ✅ 三轮对抗 review（B1-B3/M1-M5 全修） |
| smoke 抓出 3 bug 全修 | message_start 注入失效（真实结构 message.usage 嵌套）、OpenAI web_search 静默丢弃（保留透传+warn）、max_tokens 超限误判 failover（本地 400 直返） | ✅ 7 路 smoke agent 实测 |
| websearch 结构性缺陷诊断 | 快路径 MCP 硬依赖 Kiro 池号，纯透传池透传拦截（静默失效）/无号 502 | ⚠️ 待 owner 决策（D/A/B/C，ISSUES (d)） |
| 部署 | sha a3ae8874，备份 .bak-smoke2 | ✅ nbus 实测 |

## 九、W9 前端 a11y + i18n 完美化 + Token 交换修复（2026-08-15 晚，CI 1961/0 + 前端 48/0，未部署）

| 项 | 证据 | 验证 |
|---|---|---|
| a11y：console issues 323 条 → 0 | Field div→label 隐式关联（~40 控件）+ useId 基元层 + htmlFor+id 补全 + Dialog aria-describedby + 14 键 ×3 语 | ✅ 浏览器实测 0 告警 |
| i18n 完美化：全站硬编码中文扫尾 | 528 行中文 → 335 处理 + 193 豁免，真·UI 残留 0；309 新键 ×3 语（知识库 262 字段键化等） | ✅ 三语键集一致（当时 2309=2309=2309） |
| Token 交换修复（线上用户报 500 Oops） | 根因 = 上游 Kiro auth 500（非我们问题）；修复排障短板：warn 日志（status+body 截断+code 脱敏）、5xx 重试 1 次（500ms）、4xx/5xx 文案区分 | ✅ 6 测试（含本地 TCP mock 重试验证） |

## 十、W10-W12 绊脚石全量修复（2026-08-16 凌晨，CI 2020/0 + 前端 53/53，部署 edf27204）

| 类 | 项 | 验证 |
|---|---|---|
| 上号问题 | redirect_uri/region/截断移植（full_redirect_uri 消费 callback.path / auth_endpoint_for_region / 回调读取截断 / host 头 / Accept / 重试 / 脱敏日志）+ 3 测试 | ✅ 已部署 |
| 现役 bug 级 1-8 | #1 set_compression 接线（reload 播报 + 守卫）、#2 otaAutoCheck 后端补接线（restart-only + 契约测试）、#3 restore 表补 5 项 + 通用守卫 18/18、#4 mock_cache 守卫自证绿修复、#5 buffered 路径补 2 测试、#6 快照命令三文档统一 + verify-snapshot.sh、#7 双层日志 filter（面板 INFO 可见）、#8 cooldown save 串行化 + 版本守卫（4 测试） | ✅ 全部守卫在 |
| 结构设计类 9-16 | #9 model_blocklist/blacklist 合并、#10 禁用四字段双份（3 测试 + 真源注释 + 修 set_disabled 漏清 disabled_at）、#11 超大函数补 8 行为测试（TCP mock 端到端）、#12 幽灵承重串纠错（shield 真实判据 3 词）、#13 前端语言耦合改枚举（cooldownCode 9 码 + duplicate_credential）、#14 子串匹配 3 处结构化、#15 双入口提取 6 公共函数（427/365→323/257 行）、#16 调用环标注纪律 | ✅ 前端 53 测试 |
| 并发工程类 | count_tokens 可注入 + 6 测试（真打网）、websearch 测试锁修复、真并发测试 4 个、启动播种自检（21 镜像标记 + verify）、healthz build_sha（build.rs 注入）、告警扩展（pool_exhausted 5 埋点 + quota_exhausted + stats_stale watchdog）、refresh_loop/select_highest_priority 补测、**alerting poison 修复**（锁 poison 恢复 + 无 runtime 降级——32 测试连锁崩根因） | ✅ CI 2020 |
| 部署 | sha edf27204（static-pie；首次传输缓存构建出动态链接坏产物 → 回滚 + 强制重建 + 校验 sha）；healthz 显示 build_sha | ✅ 4 通道模拟测试全过 |

## 十一、W13 最终收尾四线（2026-08-16，CI 2020/0 + 前端 53/53，部署 88270616 = 当前线上）

### ① blockers 修复真实性核验（14/16 真实有效 + 2 瑕疵 + 4 MINOR 全修）

- **14/16 真实有效**：含全部高危项 needle 展开验证（#4 mock_cache / #11 行为测试 / #12 幽灵承重串 /
  #13 语言耦合 / #14 子串结构化——无自证绿、无幻觉）
- **2 瑕疵 + 4 MINOR 全修**：#6 快照命令彻底统一（build.rs 入 KEY_FILES）、#16 标注勘误（写/读引用分清）、
  **stats_stale 接线**（main.rs:658 60s 周期任务，usage JSONL 断更 10min 告警，report_if_stale 幂等）、
  #13 Display 文案（凭据重复/无效分清）；#8 纳秒级残余为观察项

### ② 性能实测（docs/PERFORMANCE.md，nbus 2026-08-16 04:28-04:35 实测）

| 指标 | 值 |
|---|---|
| 机器 | 2 核 Xeon E5-2680 v4 / 2GB（可用 1447MB） |
| 网关 RSS | 空闲 32.5MB（1.6% 内存），并发 20 次零增长 |
| 串行 50 次 | 100% 成功，p50 1185ms / p90 1493ms / p99 1673ms |
| 并发 5×4 | 100% 成功（20/20，0 429/5xx），网关 CPU 0.5% |
| 联网对比 | new-api（45.2k★）/sub2api（37.1k★）均无公开硬基准——我们是稀缺硬证据 |

### ③ 性能仪表盘 PerfDashboard（overview-page）

- 6 指标卡 + 延迟分布 p50/p90/p99 条 + 错误分布 + uptime/RSS 元信息；设置页「显示性能仪表盘」开关
  （localStorage uiLayoutPrefs，默认显示，隐藏即停轮询实测）；三语 21 键；无新依赖；浏览器实测

### ④ I18N 彻底干净

- 浏览器核对抓 4 组件 bug（region-select 中文残留 / useMemo 缺 t / storage 分区 5 处后端 label 直显 →
  storagePartitionLabel 枚举映射）+ ja 精修 34 处（份→件/透传→透過/风控→リスク管理等）+ 术语统一 31 处
  （資格情報→認証情報 AWS 惯例等）+ 联网核对术语符合主流
- **三语键集 2344 = 2344 = 2344 一致**

### ⑤ 部署证据（当前线上）

- sha256sum 实测 = **88270616**（nbus 2026-08-16 实测）；healthz `{"build_sha":"final","ok":true,
  "pool_count":4,"version":"1.1.1"}`；备份 .bak-pre-final；4 通道 smoke 全过

## 十二、部署链核验汇总（nbus，全部 sha256sum 实测）

| sha | 内容 | 波次 | 备注 |
|---|---|---|---|
| 52748cf2 | W2 审计修复全量 | W3 | 备份 .bak-1.1.1-fixes |
| a3ae8874 | W2-W7 全部修复 + 前端 UI + 模拟缓存 + 错误码配置 | W8 | 备份 .bak-smoke2 |
| edf27204 | 绊脚石 16 项 + 并发工程类 | W12 | 首次缓存坏产物回滚重建；healthz 显示 build_sha |
| **88270616** | W13 最终收尾（build_sha=final） | W13 | **当前线上**，healthz 全绿 |
