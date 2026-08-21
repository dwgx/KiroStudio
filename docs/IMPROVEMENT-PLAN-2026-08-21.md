# KiroStudio 全面质量审查与提升计划

> 2026-08-21 审查基线：当时产品树 == `quality-up/after-v1.1.2`。
> **落地后不要当待办总表。** 完成/剩余：`.agent/HANDOFF.md` + `.agent/BACKLOG-NEXT.md`。
> 活树已在该基线上改过 P0/P1/P2；全量测试 2247；Homecloud 仍 1.1.2（Bug C 在燃，未部署）。
> 方法：5 个独立分区审查员（anthropic 协议层 / kiro 调度层 / admin+前端 / 基础设施 / 架构工程）+ 主线验收 + 联网对照研究。
> 所有发现均给出 文件:行号 证据；行号会随后续编辑漂移，以符号名为准。

---

## 0. TL;DR

**总分 7.4/10**（5 分区 × 6 维 = 30 个维度的平均；发现总量：**1 CRITICAL + 13 MAJOR + 约 44 MINOR/观察**）。

| 分区 | 均分 | 最弱维度 |
|---|---|---|
| 3.1 anthropic 协议层 | 6.3 | 逻辑正确性 5（CRITICAL 拖累）、可维护性 5（9181 行神文件） |
| 3.2 kiro 调度/凭据层 | 7.8 | 可维护性 6（近万行 + marker 协议散布） |
| 3.3 admin + 前端 | 7.7 | 逻辑正确性 7（必现 500 端点） |
| 3.4 基础设施 | 8.5 | —（全仓最高区） |
| 3.5 架构工程 | 6.7 | 神文件治理 5（27 个文件超 1500 行） |

**一句话结论**：调度内核、安全基线、发布链、测试纪律都是同类项目顶尖水平（12 键选号、fail-closed 鉴权、版本一致性闸门、44% 测试占比、"为什么"级注释文化）；被扣住的不是设计能力，而是三类债——①三个各自正确的机制在**组合处**口径错配（本次 CRITICAL 的成因模式，测试恰好漏了唯一暴露组合），②**记账/观测盲区**（断连丢 usage、空响应记成功、JSONL 无限增长、零 span），③**神文件与重复段**持续放大"改一处漏一处"的事故概率（本仓 CLAUDE.md 自己承认的惯性模式）。

**最优先三件事**：

1. **修 Bug C 校验口径错配（CRITICAL，主线已逐环复核属实）**——默认配置下 Write/Edit/Read/Glob/Grep/LS 六个内置工具的流式调用恒被误杀成 `INVALID_TOOL_INPUT`。先花 10 分钟核对线上二进制是否已带该批次改动（`/healthz` build_sha vs v1.1.2）判断"在燃还是待引爆"，再上约 5 行修复 + 8 内置工具端到端回归（→ P0-1/P0-2）。
2. **记账正确性三连修**——客户端断连 Drop 兜底补记、空响应独立 outcome、usage JSONL 保留清理（→ P1-1/P1-2/P1-3）。这三个盲区让面板成功率与生命周期花费系统性失真，对一个以号池健康为核心卖点的网关是暗伤。
3. **凭据池状态完整性三件套**——`persist_credentials` 写串行化（防死号复活）、`quota_exhausted` 告警归位、刷新错误分类结构化（防瞬态烧号）（→ P0-4/P0-5/P1-4）。

主线验收全绿：2171 tests 实跑通过、脏树产品文件与 `quality-up/after-v1.1.2` 75/75 哈希一致、公开 Release v1.1.2 四端资产齐全。两个重量级断言（Bug C CRITICAL、config/import 必现 500）均由主线在活源码里独立复核确认，非审查员孤证。

---

## 1. 本次验收结论（全部通过）

| # | 验收项 | 结果 | 证据 |
|---|---|---|---|
| 1 | `cargo test --no-default-features` | **2171 passed / 0 failed**（26.7s） | 本机 2026-08-21 实跑，日志 `.agent/test-run-2026-08-21.log`；与账本 2026-08-20 记录一致 |
| 2 | 脏树产品文件 vs `quality-up/after-v1.1.2` | **75/75 哈希完全一致**（0 差异 0 缺失） | `git hash-object` 逐文件比对 `src/` + `admin-ui/src/` 全部修改+未跟踪文件 |
| 3 | 公开 Release v1.1.2 | **Latest，四端资产 + sha256 齐全** | GitHub API：linux-x86_64 / macos-aarch64 / macos-x86_64 / windows-x86_64.exe，2026-08-20 发布 |
| 4 | 历史遗留疑点：websearch OR 误吞（ref 文档标"MAJOR 待确认"） | **已修复** | `src/anthropic/websearch.rs:279` `tool_is_web_search` 现为 name+type AND 双判据，注释记录 2026-08-15 对抗审查 MAJOR-1 决策 + 守卫测试 |
| 5 | 编译警告 | 1 条 dead-code | `ErrorResponse::authentication_error` 从未使用（顺手清理项） |

**历史工作复盘（git log + CHANGELOG 交叉）**：0.4.0→1.1.2 的主线是真实的质量爬坡——每版都有"旧代码上会失败"的回归测试、多轮对抗 review、诚实边界声明。0.7.44 的全仓精读修复批次（acquire_context 忙等、令牌桶容量塌陷、AIMD 饿死、OTA 按 OS×ARCH 选资产等）与 1.1.0 的选号临界区优化在本次分区审查中均未发现回退。W1–W6 神文件抽取（`#[path]` 卫星模式）已验证有效且测试数从 2170→2171 无损。

---

## 2. 项目规模快照（2026-08-21 实测）

- 135 个 Rust 文件：生产代码约 78k 行 + 测试约 61k 行（**测试占全仓 44%**）；前端 admin-ui 若干（三语 i18n）。
- 测试函数 **2184 个**（运行 2171，含 ignore/cfg 差异），**零外网依赖**；其中 **181 个**是 `include_str!` 源码守卫测试（245 处引用）。
- 超 1500 行文件 **27 个**（Rust 23 + TSX 4）。Top：`src/admin/service.rs` 13105、`src/kiro/provider.rs` 10841、`src/kiro/token_manager_tests.rs` 10164、`src/kiro/token_manager.rs` 9989、`src/anthropic/handlers.rs` 9181。
- `#[path]` 卫星文件 19 个挂在 5 个父文件（token_manager / stream / converter / provider / service），模式已验证。
- 依赖：Rust 直接 30+4（锁定 425）；前端 19+8（锁定约 237）。
- 生产 `unwrap` 仅 **21 处**；`anyhow!` 167 + `bail!` 106 vs `thiserror` 1；tracing 659 处（**零 span/#[instrument]**）；错误文案 95.6% 中文。

---

## 3. 分区评分与不足点

### 3.1 anthropic 协议层（src/anthropic/）

| 维度 | 评分 | 一句话理由 |
|---|---|---|
| 逻辑正确性 | **5/10** | 绝大多数路径异常严谨（跨 chunk 扣留、UTF-8 边界、merge 决策表有对抗性设计），但存在默认配置下**必现**的 CRITICAL（Bug C 校验口径错配）与一个块序 MAJOR |
| 并发与资源安全 | **7/10** | 所有缓冲有显式上限（invoke hold 256KiB、thinking 256KiB、buffered 64MiB、schema 节点 5 万），流上下文单所有权无锁；扣在断连时整条 usage 记录静默丢失 |
| 协议兼容性 | **6/10** | 事件序列/stop_reason/usage 总体对齐官方且有文档化理由，但多处刻意偏差（0.6657 缩放、prefill 静默丢弃、占位签名）+ 一处真实块交错违规 |
| 错误处理 | **8/10** | CompletionStatus 显式建模贯穿三条收尾路径，in-band 错误/传输中断/解码停止/干净 EOF 全覆盖；瑕疵是空响应发 error 却记 Success |
| 可维护性 | **5/10** | handlers.rs 9181 行神文件、双端点约 200 行近似重复、帧解码循环三份拷贝（仓库注释自认"两份拷贝必然漂移"）；加分在兄弟模块抽取干净、"为什么"级注释全仓罕见 |
| 测试质量 | **7/10** | 330+ 条测试、逐字节切分/EOF 残留/对抗形态远超一般水平；但 CRITICAL 恰好落在唯一没测的组合上（Bug C 测试只用了不改名的 Bash，假阴性） |

**发现清单（anthropic 层）**

- **[CRITICAL·主线已独立复核确认] `src/anthropic/stream.rs:3097-3116` / `:2881-2888` — Bug C 必填字段校验的参数形态口径错配，默认配置下误杀 6 个内置工具的全部流式调用**。四环证据链（主线逐环验证过）：① 内置工具发上游用合成 Kiro schema，`fs_write` required=`["path","text"]`（tool_compat.rs:406，经 :634 进最终工具表）；② `tool_required_fields` 从该表提取，key/值均 Kiro 形态（converter.rs:835-849）；③ `process_tool_use` stop 分支**先**把参数还原成客户端形态（`path→file_path`、`text→content`，tool_compat.rs:270-273）**再**调 `flush_tool_input` 校验（stream.rs:2881-2888）；④ 两门控默认开（config.rs:1534、1552）。结果：模型发出完全合法的 `{"path","text"}` → 还原成 `{"file_path","content"}` → 按 `["path","text"]` 校验恒缺失 → 整轮置 `INVALID_TOOL_INPUT`、参数不下发、客户端重试再中。**Write/Edit/Read/Glob/Grep/LS 六个必中**；Bash/WebSearch 幸存（字段不改名）。测试假阴性根因：`bug_c_detects_missing_required_tool_fields` 恰好只用了 Bash。非流式路径无 Bug C 校验（不对称侥幸）。修法（按侵入度）：a) 校验挪到出站映射前（对 Kiro 形态校验 Kiro required）；b) required 名单过一遍与 `map_tool_input_from_kiro` 同源的改名表；c) converter 侧存客户端形态 required。任何一种都必须补 8 内置工具端到端回归。
- **[MAJOR] `src/anthropic/stream.rs:2725-2776` — 嗅探型 thinking 块开着时收到 ToolUse，块不关闭直接开 tool_use 块**，违反"先 stop 再 start"契约。`process_tool_use` 只处理两种形态（缓冲末尾恰有 `</thinking>`、或结构化 reasoning 块），第三种——内联 `<thinking>` 开块、闭合未到、上游直接发 toolUseEvent——落空（嗅探路径边到边下发，buffer 常为空）。仓库自己在 :2760 注释里承认此违规会让 CC 解析报错并为 reasoning 分支修过同一问题，嗅探分支漏了。修法：补第三分支（flush 残留 buffer → 空 delta → signature_delta → stop 后再开 tool_use 块），约 10 行。
- **[MAJOR] `src/anthropic/handlers.rs:3306-3459` / `:4707-4851` — 客户端断连时整条 usage 记录（含 credits）静默丢失**。`emit_stream_usage`/`emit_buffered_usage` 只在流自然结束与上游断流两分支调用；客户端主动断开（CC 按 Esc 是高频操作）时 hyper 丢 Body → unfold 状态机连 ctx/meta 一起 Drop，全仓无 Drop 兜底（`impl Drop` 仅存在于测试 guard）。后果：上游已实际消耗的 token/credit 不落库不 report_credits，面板成功率与生命周期花费系统性偏低。修法：`(ctx, meta)` 包进"未 emit 则 Drop 时按 Interrupted outcome 补记"的 guard。
- [MINOR] `src/anthropic/stream.rs:1713` vs `:2554` — 同为"客户端没要而丢弃的思考内容"，内联 `<thinking>` 形态计入 output_tokens（计数在剥离前）、结构化 reasoning 形态不计（有测试钉死），两套口径令 near-empty 判定（<30）随上游形态漂移。统一为"剥掉的不计"。
- [MINOR] `src/anthropic/stream.rs:600-615` — `close_open_blocks` 迭代 HashMap 补发 stop，多块同开时顺序不确定；换 BTreeMap 一行消除。
- [MINOR] `src/anthropic/stream.rs:4340-4346` — `context_input_tokens_from_pct` 下界防御完整（0/NaN/负）但无上界钳制，`pct=1000` 会把计费 input_tokens 写成 10 倍窗口落库。clamp 到 [1,200]% + 超界 warn。
- [MINOR·刻意偏差] `src/anthropic/stream.rs:208` vs `:1028` — 同一条消息内 `usage.input_tokens` 两处口径互斥：message_start 发未缩放值、message_delta 发 ×0.6657 缩放值（操纵 CC compact 触发点）。系数绑死 CC 4.6 的 85% 阈值，客户端升级即漂移。建议至少在 `x-kirostudio-*` 头暴露缩放标记。
- [MINOR] `src/anthropic/handlers.rs:3427` / `:4249` — 空响应给客户端发 error（429/400）记账却是 Success：客户端视角失败、面板视角成功、error_message 为 NULL，空响应高发时段失败率被系统性低估。引入独立 outcome（EmptyResponse）。
- [MINOR·疑似] `src/anthropic/invoke_xml.rs:91-102` — 参数值内字面 `</invoke>` 先到、真闭合在下一 chunk 时提前截断重组（"贪婪取最后闭合"只对已缓冲文本有效）。概率低。
- [MINOR·已文档化偏差] `src/anthropic/converter.rs:741-753` — assistant prefill 静默丢弃（截断到最后一条 user，仅 info 日志）；依赖 prefill 引导结构化输出的客户端行为无提示变化。至少 warn/响应头暴露。
- [MINOR] `src/anthropic/stream.rs:3719-3738` — `estimate_tokens` 只认 U+4E00–9FFF 为 CJK，假名/韩文按 4 字符/token 低估约一半，间接影响 near-empty 判定与面板；`is_leak_glue_char` 反而覆盖更多区段，两处口径不一。
- [MINOR] `src/anthropic/handlers.rs:2780-3099` vs `:4381-4630` — 双端点约 200 行近似重复（历史已因此漂移过一次：/v1 修了 /cc 漏了）+ 帧解码循环三份拷贝，正是本仓 CLAUDE.md 自己总结的"同一判据两份实现只修一份"事故模式的温床。收敛为共享辅助函数 + 内嵌 4276 行测试外提兄弟文件。

**审查员总体印象**：防御密度和文档质量远超均值；CRITICAL 不是设计粗糙，而是三个各自正确的机制（内置工具改名、出站还原、Bug C 校验）在组合处口径未对齐，且测试恰好选了唯一不暴露该组合的工具。建议把"参数形态（Kiro 形态/客户端形态）"显式化为类型或命名约定。

### 3.2 kiro 调度/凭据层（src/kiro/）

| 维度 | 评分 | 一句话理由 |
|---|---|---|
| 逻辑正确性 | **8/10** | 核心调度不变量（排除集三不变量、两趟饱和门、absorb 预算跨轮共享、429 先临时后永久）严密且历史缺陷有回归钉死；扣在告警键错位与刷新错误分类残留 |
| 并发与资源安全 | **8/10** | 锁序单向一致、无锁内 await、InflightGuard RAII、per-credential 刷新锁 + 陈旧快照守卫齐备；扣在 persist_credentials 无写序列化与选号临界区 O(n) 残留 |
| 调度公平性与韧性 | **9/10** | 反饥饿探测键、惩罚衰减半衰期、兜底放行深度档、全余温逃生舱、自愈指数退避互相配套且有实测数据 |
| 错误处理 | **8/10** | 临时风控/永久封禁/配额/模型级四类分类极细、判定顺序有守卫、fail-open 贯彻一致；扣在错误串内嵌 marker 协议（中文文案与机器判据耦合） |
| 可维护性 | **6/10** | 注释质量极高（根因+实测+反向教训），但 token_manager 近万行、13 位排序键耦合、marker 协议散布三处，新人上手成本高 |
| 测试质量 | **8/10** | 约 100 个行为测试 + 96 处源码守卫，守卫有自检设计（标记唯一性、needle 运行时拼接）；扣在个别恒真断言与切片近似 |

**核对过确认无问题的重点**：12 键排序稳定全序（快照后 min_by_key，历史 family_key 影子条目 bug 已修）；custom_api 与 Kiro 两池隔离铁律成立；absorb 循环无饿死/忙等路径（闸门上移有守卫、预算跨轮共享、退避有下限、min>max clamp 已归一化）；刷新惊群防护完整；Retry-After 钳制 600s、cooldown 持久化带版本号；透传 fail-open + SSRF 运行时复验完整。

**发现清单（kiro 层）**

- **[MAJOR] `src/kiro/token_manager.rs:6730-6735` — `quota_exhausted` 告警键 bump 在错误的函数里**。全仓唯一的 `bump("quota_exhausted")` 出现在 `report_suspicious_activity`（403 风控路径）的禁用分支，而真正处理 402 的 `report_quota_exhausted`（:6926）只 bump `credential_disabled`。后果双向失真：风控整族禁用误报"配额耗尽"（族内 N 份各 bump 一次），真 402 永不触发该告警，运维排障方向被带反。修法：两行连注释移到 `report_quota_exhausted` 禁用块。
- **[MAJOR·疑似（概率低机制确定）] `src/kiro/credential_persist.rs:10-76` — `persist_credentials` 无写序列化，并发下旧快照可能最后落盘**。锁内取快照、放锁后 write_atomic，调用点极多（persist_disabled_state/刷新写回/自愈/admin）；A(旧)B(新) 可交错成 B 先落 A 后落 → 磁盘回退。同仓 `cooldown.rs:791` 已把同形态点名为缺陷并用 save_lock+版本号修掉，credentials.json 这条更关键的路径没有同款保护。风险场景：自动禁用落盘被旧快照覆盖后进程被 SIGKILL（report_success 注释自述线上 41 次 SIGTERM 有 39 次最终 SIGKILL），死号重启复活。修法：照抄 cooldown 的 save_lock 模式。
- **[MAJOR·疑似需复核] `src/kiro/token_manager.rs:1772-1788` — `is_refresh_error_credential_level` 仍用裸子串匹配状态码**（`s.contains("400")`…）。同文件 :195 引入 `RefreshHttpError` 结构化类型的理由正是"contains 会误判 URL 端口/字节数/毫秒数"，`refresh_error_retryable` 已改 downcast，这个方向相反且后果更重（瞬态误判 → refresh_failure_count 3 次 → 不可逆禁用烧号）的判据没跟上。修法：downcast 按 status 精判 4xx（排除 429），无状态码才留字符串兜底。
- [MINOR] `src/kiro/passthrough.rs:243-263` — 透传请求体无条件 parse+re-serialize（serde_json 未开 preserve_order → 键按字典序重排），破坏模块头"字节级透传"承诺——即使映射未命中也重排。对按字节做前缀缓存的上游有实际影响。修法：仅 map_target 命中时重序列化，否则转发 raw_body。
- [MINOR] `src/kiro/passthrough_think_filter.rs:561、594` — SSE `content_block_delta`/`message_delta` 无条件重序列化（`message_start` 却保留原字节，行为不对称）；filter 自身 doc 已标"非字节级"但 passthrough.rs 顶部契约没同步。每 delta 一次 parse+serialize 的 CPU 成本。
- [MINOR] `src/kiro/token_manager.rs:3711-3739` — `select_custom_api_or_wait` 的 Available 竞态分支零 sleep 自旋（上限 64 轮，每轮 3 次 entries 锁）；建议 ≥2 次后加 yield/短 sleep。
- [MINOR·疑似] `src/kiro/token_manager.rs:4050` — opus 订阅硬门判定用 `contains("opus")` 子串；当前命名空间无实害，若允许任意 custom 模型名进 Kiro 路径会误伤 FREE 号。改前缀/精确匹配。
- [MINOR] `src/kiro/cooldown.rs:954-967` — `test_cooldown_incremental` 恒真断言（先 clear 再 set，两次时长必然相等，`d2 >= d1` 恒真）；递增行为实际没被测到。去掉 clear 改断言 `d2 > d1`。
- [MINOR] `src/kiro/token_manager_endpoint_bypass_guard_tests.rs:11-18` — 守卫 `fn_body` 用 `split("\n    }")` 切片近似，函数体内出现同缩进 `}` 会静默截短守卫范围；provider.rs 内守卫已有"标记唯一性自检"缓解，此文件没有。加切片长度下限或同款自检。
- [观察] 选号临界区两处 O(n) 残留：排序键闭包内每候选一次 `family_key` String 分配（:4245，缓存只用在 report_success 侧）+ 每候选一次 health Mutex 获取（:4266）；43 号池一次选号 = 43 次分配 + 43 次锁获取，全在 entries 锁临界区内。与已完成的 rpm 批量化是同一优化的未完成部分。

### 3.3 admin 后端 + admin-ui 前端

| 维度 | 评分 | 一句话理由 |
|---|---|---|
| 安全性 | **8/10** | 鉴权 fail-closed（auth_keys 空值恒拒 + SHA256 定长常时比较）、审计层序有守卫、SSRF/XSS 防线带回归测试、明文导出 no-store；扣在匿名 bg-img 开放代理无频控、无 frame-ancestors、proxyUrl 内嵌账密漏进脱敏导出 |
| 逻辑正确性 | **7/10** | 绝大多数路径异常细致（在途竞态、锁序、lost-update 都有注释+测试钉死）；扣在 config/import 必现 500 且无行为测试、Windows OTA 无自动回滚闭环 |
| API 契约一致性 | **7/10** | camelCase 契约有守卫、错误结构统一、批量"部分失败仍 200"模式一致；扣在三种命名并存（IDC 意外 snake_case），前端被迫 `as any` 适配 |
| 前端代码质量 | **8/10** | React Query 使用地道、fetch-SSE 有 generation 防泄漏+空闲超时+指数退避、登录前置校验；扣在 settings-page 3350 行巨石、185 行手写 diff 默认值双份漂移、四个批量操作 N 次往返 |
| i18n 完整性 | **9/10** | zh/en/ja 各 2367 键**零缺失零空值**（脚本比对），zh==ja 相同值均为合法共用汉字词；扣在后端下发中文-only 展示串使 en/ja 界面夹中文 |
| 可维护性 | **7/10** | 注释文化极佳（决策+历史缺陷+回归守卫三件套）；扣在 service.rs 约 13000 行（`update_config_locked` 单函数约 1240 行）、settings-page 单文件 |

**发现清单（admin+前端）**

- **[MAJOR·主线已独立复核确认] `src/admin/service.rs:5394` + `:5489-5491` — `POST /api/admin/config/import` 必现 500，端点自诞生起从未可用**。复核链：路由已注册（router.rs:181，还有守卫测试防删路由）；`serde_json::from_value::<Config>` 反序列化，而 `Config.config_path` 是 `#[serde(skip)]` 私有字段（config.rs:1168）恒 None；全仓无 `set_config_path`/`save_to`，admin::service 想设也设不了；`imported.save()` 在 None 时直接 Err「配置文件路径未知」（config.rs:1987-1991）→ InternalError 500。讽刺的是函数自己 :5419 就从 token_manager 取到了正确路径（用于 load current），只差回填一步。副作用：`rotate_config_backup` 在 save 之前执行（:5488），每次失败尝试空转一次备份轮换。现有测试只有源码守卫（断言"持锁在写盘前"），无行为测试；前端零调用（grep `config/import` 在 admin-ui 无命中）——所以从未被发现。修法：`Config` 加 `pub(crate) fn set_config_path`（或 `save_to(&Path)`），import 从 current 继承路径；rotate 挪到校验全过之后；补"导入→写盘→重读"行为测试。
- **[MAJOR] Windows 部署无 OTA 自动回滚链 — `src/common/health_marker.rs:21-22`、`src/admin/service.rs:5799-5806`**。crashloop 判定（`.boot_attempts`）+ 自动回滚只与 systemd `ExecStartPre` 守卫脚本闭环（模块文档自认"仅在 Unix 生产部署有意义"）；Windows 裸跑（service.rs:5635 注释自认是主流形态）的重启是一次性 `.bat`（ping 4 次 → start 新 exe → 删自身）——新二进制启动即崩时无人 rename `.bak` 回来，服务死亡需人工恢复。sha256 挡坏包，挡不住"官方包在该机启动崩"（缺运行库/配置不兼容）。前端 update/status 的"升级失败已自动回滚"文案在 Windows 上不成立。修法：`.bat` 加启动探测（循环探 `/healthz` N 秒，失败 rename `.bak` 回原路径再拉起），或面板对 Windows 明示无自动回滚。
- [MINOR] `src/admin_ui/router.rs:784-890` — `/admin/api/bg-img` 是匿名可指挥的出站 GET 代理。防线已完整（DNS 钉死+禁重定向+拒内网、10MiB 截断、MIME 白名单+nosniff，均有测试），但未鉴权客户端仍可让网关拉任意公网 https URL 回吐：带宽放大、出口 IP 被借用、无频控。修法：只接受 `random-bg` 下发的 HMAC+过期戳签名 URL，或收紧图源域白名单。
- [MINOR] `src/admin/service.rs:4814-4825` + `:5350-5366` — 全局 proxyUrl 内嵌账密漏进"脱敏"导出。`update_config` 对 proxyUrl 原样存储（对比：凭据路径/三条登录路径都 `split_proxy_credentials` 拆账密）；`export_config` 脱敏清单只删 `proxyUsername/proxyPassword` 键，URL userinfo 原文照常导出，`get_config_snapshot` 同样全量回显。修法：写入时拆内嵌账密；导出/快照对 userinfo 打码。
- [MINOR] `src/admin/handlers.rs:1373-1418` — IDC 登录端点是 camelCase 契约中唯一的意外 snake_case（`session_id`/`credential_id`…；同文件 external-idp 全套 camelCase），前端被迫 `(data as any)` 双读适配（api/credentials.ts:565 注释自认）。`SetProxyRequest`（:656）同病。修法：`rename_all="camelCase"` + `#[serde(alias)]` 兼容一版后删；顺带删前端适配层。
- [MINOR] `admin-ui/src/api/credentials.ts:68-80` — axios 拦截器把一切 401/403 当密钥失效（清 sessionStorage + reload）。后端用 403 表达业务拒绝（import_keys 开关关闭 handlers.rs:858、IP 黑白名单 security.rs:397/412）——一旦面板挂上这类端点或管理员把自己 IP 写进 blocklist，表现是"静默登出+无限刷新"零提示。修法：仅 401 清 key；403 按响应体 `error.type` 分流。
- [MINOR] `src/admin/insight.rs:24-57` 等 — 后端下发中文-only 展示串（insightText"畅通/冷却中…剩22s"、SuccessResponse"凭据 #5 已禁用"、AdminServiceError 消息），en/ja 用户 toast/抽屉夹中文。项目已有正确范式（`cooldownCode` 稳定枚举码 + 前端 i18n），insightText 未跟进。修法：返回结构码+参数，前端渲染。
- [MINOR] `src/admin_ui/router.rs:618-624` — CSP 缺 `frame-ancestors`，全仓无 X-Frame-Options：登录页可被任意站点 iframe 嵌套做 UI 覆盖钓鱼。修法：CSP 追加 `frame-ancestors 'none'`（一行）。
- [MINOR·疑似] `src/admin/types.rs:1936-1945` — `mask_import_key` 阈值弱：len=13 的 key 泄漏 head8+tail4=12/13 字符。真实 ksk_ key 通常远长于 16，风险低。修法：`len <= 16` 全打码。
- [MINOR] 陈旧安全注释误导后续评审 — handlers.rs:897、admin_ui/router.rs:200/757 仍写"adminKey 明文存 localStorage 且全仓无 CSP"，现状已是 sessionStorage（读取顺带清理 localStorage 残留）+ CSP。这些注释是安全决策的量刑依据。修法：一次性 sweep。
- [MINOR] `admin-ui/src/components/dashboard.tsx:375-537` — batchReset/batchDisable/batchWhitelist/batchRefresh 逐 id 串行 N 次往返；批量删除已有专用端点（自述"2N→1 次往返"），同动机适用。修法：按 `BatchDeleteResponse.results[]` 模式补 batch 端点。
- [正面记录] 匿名可达面全部有意且有防护：`/healthz`（版本指纹，低敏已论证）、`/admin` 静态、bg 三端点（XSS 链闭合有回归测试）、OAuth callback（state 关联 + html_escape + CSRF fail-closed）；admin 鉴权单点收口（活读热更单元），别名路由同一鉴权。

**神文件评估**：service.rs 约 13000 行（生产约 7200 + 测试约 5800），职责混杂（登录门面/凭据 CRUD/余额缓存/config 热更/socks 池/存储统计/诊断/自重启），`update_config_locked` 单函数约 1240 行；`insight.rs`/`ksk_import.rs` 已验证 `#[path]` 兄弟文件下刀模式，干净切线四条：①余额缓存段 ②socks 节点段 ③config 更新+错误表校验 ④自重启助手。settings-page.tsx 3350 行内聚性尚可（8 个独立 Card），要害是 185 行手写 diff 里默认值与 `toForm` 双份维护（`?? 45`、`?? 150` 各写两遍）是将来改默认值的漂移点。

### 3.4 基础设施层（common / usage / config / main / openai / throttle / http_client）

| 维度 | 评分 | 一句话理由 |
|---|---|---|
| 逻辑正确性 | **9/10** | 令牌桶定点数学、滚动窗口口径、事务边界均正确，且历史故障（容量塌陷、AIMD 饿死、零点跳水）都有回归测试钉死 |
| 并发与资源安全 | **8/10** | 锁纪律出色（锁内无 await、锁序注释、config 三写路径同锁有守卫）；扣在 log_buffer seq 锁外分配竞态、usage 管道停机语义 |
| 数据持久化可靠性 | **8/10** | fs_atomic（temp→fsync→rename+Windows 退避）+ WAL/busy_timeout/分批 DELETE 扎实；扣在停机丢失窗口与 rename 后不 fsync 父目录 |
| 安全性 | **8/10** | fail-closed 鉴权三道防线、SHA256+ct_eq、SSRF 多层防线、AEAD at-rest；扣在私网对端自动信任 XFF、代理校验旁路 |
| 可维护性 | **9/10** | "为什么"级注释密度和历史事故存档罕见地高；扣在 convert.rs 2900 行/usage_stats.rs 2600 行单文件过大 |
| 测试质量 | **9/10** | 回归测试直指真实线上故障、"回退即 FAIL"防自弱化守卫、并发/注入式 mock 齐备 |

**发现清单（基础设施）**

- **[MAJOR] `src/usage/usage_stats.rs:1200-1251` — usage JSONL 无自动保留清理，冷启动重放全部历史**。SQLite traces 有 6h 周期清理（main.rs:1286），但 `usage-YYYY-MM-DD.jsonl` 永久累积、全仓无自动删除；`rebuild_from_logs` 启动时重放目录下所有文件——磁盘无界增长 + 启动时间随历史线性变差（环形桶只保留 31 天，超期记录 apply 后即被覆盖，纯属白算）。修法：6h 清理任务顺带删超期 JSONL；rebuild 按文件名日期只重放最近 31 天。
- **[MAJOR·疑似，视部署形态] `src/common/security.rs:296-341` — 私网对端自动视为可信反代，纯内网直连部署下 XFF 可伪造**。`is_trusted_proxy_peer` 把 RFC1918/环回对端一律当反代并采信 XFF 最右段（A2 修复的副作用）：LAN 直连时任意内网用户发 `X-Forwarded-For: 8.8.8.8` 即可换 IP 绕过每-IP 限流/黑名单（机器码黑名单同源）。修法：加 `trustPrivatePeerAsProxy` 开关（默认现状零回归，LAN 直连部署关闭）。
- [MINOR] `src/usage/trace_db.rs:729-735` vs `src/main.rs:1101-1107` — TraceDb::Drop 停机兜底实际永不执行（pipeline 的 `SyncSender` 在 static OnceLock 里永不 drop，worker 永不退出），两处注释互相矛盾；丢失窗口 = pending 批(≤50 条/1s) + 通道积压(≤10000 条)。修法：提供 `pipeline::shutdown()` 或改正注释。
- [MINOR] `src/common/log_buffer.rs:58-77` — seq 在锁外 `fetch_add`、锁内 push，可乱序入环；增量游标按 `e.seq > s` 过滤会永久跳过迟到的低 seq 行（并发高峰面板丢日志行）。修法：seq 分配挪进锁内（一行）。
- [MINOR] `src/openai/convert.rs:1988-1995、2092-2095` — 非流式 /v1/responses 仍透出上游 `msg_xxx` id，与流式两处（1238、1603）的 MINOR-6 修复（恒自生成）不一致。
- [MINOR] `src/openai/convert.rs:1334-1360` — 流式 usage chunk 无条件下发，未看请求的 `stream_options.include_usage`（偏离 OpenAI 规范，严格客户端可能报错）。
- [MINOR] `src/common/secret_store.rs:9-12` — 模块文档说"机器绑定派生密钥"，实现是 CSPRNG 随机密钥文件；威胁模型描述漂移，误导审计。删过时段落。
- [MINOR] `src/common/fs_atomic.rs:125-144` — rename 后未 fsync 父目录（Linux 崩溃窗口内新内容可能回退旧文件）；崩溃残留 `.tmp` 无启动清扫。
- [MINOR] `Cargo.toml:24` — serde_cbor 0.11 停维护（RUSTSEC-2021-0127），用于 Web Portal rpc-v2-cbor。迁 `ciborium`。
- [MINOR] `Cargo.toml:7` — `default = ["native-tls"]` 与"恒 --no-default-features"纪律相反；默认构建 ≠ 发布构建是长期陷阱。改 `default = []`。
- [MINOR·已知在案] `src/common/ssrf.rs:404-406、446-469` — 域名 DNS 失败 fail-open（文档已自述）；`set_credential_proxy` / `/proxy/test` 两条旁路不经 `validate_proxy_address`。透传路径已有 M8 运行时复验兜底。
- [MINOR] `src/usage/trace_db.rs:708-722` — 低流量下最后一批 pending（≤49 条）无定时器兜底，滞留内存直到下一事件；注释"最迟这么久落一次盘"不准确。
- [观察·无需动作] 吸收层默认关四前置、incremental_vacuum 在 WAL 下不缩文件（靠启动 VACUUM）、AIMD 单向棘轮贴 floor（已承认属另一波次）、healthz 未鉴权暴露 pool_count/build_sha（低敏已论证）。

### 3.5 架构与工程质量（全仓横切）

| 维度 | 评分 | 一句话理由 |
|---|---|---|
| 架构清晰度 | **7/10** | 分层大方向单向清楚（admin→kiro→model、openai→anthropic、model/admin_ui 零出边），但 anthropic↔kiro 生产级双向耦合、common→anthropic 一处越层 |
| 代码组织（神文件治理） | **5/10** | 27 个文件超 1500 行、Top 4 近/超万行是客观重债；`#[path]` 卫星抽取方向正确且已见效，治理在途而非失控 |
| 测试工程 | **7/10** | 2184 测试、44% 测试占比、零外网、生产 unwrap 21 处，纪律优秀；扣在 181 个源码守卫的脆弱维护面、无集成/e2e 层、前端零测试 |
| CI/CD 与发布链 | **7/10** | release 链出色（出厂特性测试门禁、tag↔版本一致性闸门防 OTA 死循环、三端矩阵、sha256）；但无 push/PR 触发 CI、无 clippy/fmt/audit 门禁、工具链三处漂移 |
| 依赖健康 | **7/10** | 直接依赖克制、全主流库、锁文件齐全；serde_cbor 停维护、default 特性与出厂相反 |
| 文档一致性 | **7/10** | README/CHANGELOG/docs 索引新鲜且抽查准确；MODULES.md 自标过期（诚实但旧）、默认端口双重真相 |

**发现清单（架构工程）**

- **[MAJOR] anthropic↔kiro 生产级循环耦合** — `AbsorbClass` 定义在 anthropic 却被 kiro 吸收循环消费（6 处生产引用）；`token_manager` 模型探测直接调 `anthropic::converter::convert_request`。同 crate 能编译，但协议层与调度层互相知晓，拆 crate/重构会卡死。修法：`AbsorbClass` 下沉 `model/`（或 common），探测请求构造由 anthropic 暴露窄接口。
- **[MAJOR] `src/admin/service.rs`（13105 行）最大神文件且测试仍内联**（约 7259 行起 ~5.8k 行测试区）；`handlers.rs`（9181，测试 ~5144 起）、`provider.rs`（生产 5113 + 内联测试 ~5.7k）同理。token_manager 的 `#[path]` 测试外提模式已验证，纯机械搬移。
- **[MAJOR] `.github/workflows/` 无 push/PR 门禁** — 测试只在打 tag 或手动触发时跑；日常提交回归全靠本地自觉。修法：加一个 push 触发轻量 workflow（`cargo test --no-default-features --locked`），复用现有 rust-cache。
- **[MAJOR] serde_cbor 停维护**（同 3.4，两位审查员独立命中）。
- [MINOR] `src/common/security.rs:27` — common 越层引用 `anthropic::types::ErrorResponse`；应下沉 model/ 或 common 定义。
- [MINOR] 默认端口双重真相 — `src/model/config.rs:1264` serde 缺省 8080 vs `main.rs:182` 首启引导/Docker/全部文档 8990；手写最小配置漏 `port` 的用户会落在 8080。serde 缺省对齐 8990。
- [MINOR] 工具链三处漂移 — release.yml 钉 1.97.1、deploy-build.yml 用 `stable`、Dockerfile rust:1.96-alpine；同一产物三条构建路径编译器不同。用 rust-toolchain.toml 一处定义。
- [MINOR] release.yml test job 无 `--locked`（deploy-build.yml 有），发版链理论上可能悄悄解析新依赖版本。
- [MINOR] deploy/ 7 个 shell 脚本 5 个无 `set -e`（bluegreen.sh、hotswap.sh、rollback-guard.sh、verified-deploy.sh、deploy-watchdog.sh；watchdog 类可能有意）。
- [MINOR] 181 个守卫测试维护面 — needle 运行时拼接、剔注释行的坑注释自述"踩过 5 次"；建议提炼共享 helper 到 `common/test_hygiene.rs`（已有雏形）。
- [MINOR] tracing 零 span/#[instrument]——请求级关联靠消息手工带 ID，跨凭据重试链路无法 span 树聚合。
- [MINOR] admin-ui 零测试（无 vitest/testing-library；package.json 无 test script），3377 行 settings-page 全靠手测。
- [MINOR] 测试时序依赖：24 处 sleep（scheduling.rs 9 处最集中），慢 CI 机上有 flake 风险。
- [MINOR] 文档小漂移：CLAUDE.md 架构树缺 `src/openai/`、`src/model/`、`src/admin_ui/`；MODULES.md 行数全过期（已自标）；release.yml 注释版本示例停在 v0.7.44 时代。

---

## 4. 参考仓与生态对照研究（2026-08-21 联网核实）

### 4.1 ZyphrZero/kiro.rs（同源竞品，346 星）

- 最新：release **v0.7.6**（2026-08-13）→ 改用日历版本 tag **v2026.1.6**（之后仅 1 个 websearch trace 修复合并）。仓内 `docs/ref-ZyphrZero-kiro.rs.md`（08-15 研究 @ v0.7.6）**仍然有效**。
- v0.7.5→v0.7.6 增量（对我们的启示）：
  - **GPT-5.6 按模型族生成 reasoning effort**（`additionalModelRequestFields.reasoning.effort` 六档 none~max；Claude 走 `output_config.effort`）——我们仍是 Claude 单通道，GPT 5.6 三变体走 Kiro 时 reasoning 档位没有传达。
  - **OpenAI 入站会话亲和**：依次从 `prompt_cache_key`→`x-session-affinity`→`x-client-request-id`→`session_id` 提取 UUID 复用为 conversationId——我们 openai 路径不认客户端会话头，上游缓存命中率吃亏。
  - **usage 计量以 `metadataEvent.tokenUsage` 服务端精确值优先**、回退本地估算；input_tokens 含未缓存+缓存写+缓存读。
  - Opus 5 纳入 1M 模型族 + 回归测试。
  - 账号级 RPM 主动限流（滑动窗口 + 类型化 429 + 精确 Retry-After，v0.7.5）。
- 旧研究里 P0/P1 借鉴项（模型感知正向路由、customModels 配置化映射、客户端 Key+分组、token 轮换源文件重载）**仍未落地在我们仓**，依旧成立。

### 4.2 TsinHzl/kiro2cc-proxy（同源竞品，101 星）

- 最新 tag **v2.10.2**（无 GitHub release）。仓内 `docs/ref-kiro2cc-proxy.md`（08-15 研究 @ v2.9.6）之后有 **24 个提交**的实质增量：
  - **/cc/v1 端点改真流式转发、删除缓冲模式**（refactor/streamify-cc-v1-endpoint）——与我们 `ccAutoBuffer` 默认假流式的取舍相反，值得复核我们默认值（我们 0.7.5 已把 ccAutoBuffer 默认改真流式，1.1.x 现状需确认）。
  - **输出 token 上报改"可见输出"口径**，消除取整噪音与 380 固定上限——我们已有 `stream_usage_caliber_tests.rs` 移植对应口径，需对表确认是否同代。
  - **sticky 会话 429 不再立即解绑，改请求内避让**；**端点全封时退避重试其它账号**（不再直接失败/空转耗预算）——与我们的 sticky/封桶终态处理方向一致，细节可对表。
  - 超窗错误文案对齐 Anthropic 官方 `prompt is too long` 格式（利于 CC 自动 compact 触发）。
  - fallback 会话 ID 纳入首条消息防不同会话折叠——我们 1.1.2 已有同修（"conversationId 含首条消息"）。
- 旧研究 P0 借鉴项（跨月配额自动恢复、禁用持久化分层、rotation_bias）状态：**禁用持久化我们已做**（`persist_disabled_state`，0.7.44），跨月配额恢复与 rotation_bias 待评估。

### 4.3 生态扫描（其他同类仓）

| 仓库 | 语言 | 特点（相对我们） |
|---|---|---|
| caidaoli/kiro2api | Go | Anthropic+OpenAI 双协议、顺序负载均衡；token 池状态端点无鉴权（反面教材） |
| quorinex/Kiro-Go | Go | 功能面与我们最接近（Builder ID/IdC/微软 SSO/SSO Token/ksk_ 全认证 + admin 面板 + i18n CN/EN/VI + /v1/responses） |
| Jwadow/kiro-gateway | Python | 强调 free-tier 模型清单跟踪（Opus 4.5 2026-01-17 移出免费档）、VPN/代理路由 |
| d-kuro/kirocc | Go | 读 Kiro CLI SQLite 凭据直连；**内置 OpenTelemetry OTLP tracing**（我们零 span 的对照） |
| Ciyfly/Kiro2api-Node | Node | 账号池 + 面板，功能子集 |
| router-for-me/CLIProxyAPI | Go | 多提供商聚合（Kimi/GPT/Gemini/Claude/Grok + OAuth 池），是"网关之上的网关"生态位 |

**结论**：我们在调度精细度（12 键选号/9 种冷却/EWMA 熔断/族级连坐/吸收层/共享预算）、存储安全（AEAD at-rest）、发布链（版本一致性闸门）上仍领先同类；差距集中在 **模型感知正向路由、客户端 Key 租户隔离、OpenAI 会话亲和、可观测性（span/OTel）** 四点。

---

## 5. 提升计划清单

分级口径：**P0** = 正确性缺陷，立即修（合计约 1-2 天）；**P1** = 记账/安全/运维实害，本月内（约 1-2 周）；**P2** = 工程债与生态借鉴，按切片排入版本周期。
执行纪律（不可弱化）：每项落地必须带**具名测试**（named test），不得以"历史 2171 绿"充当验收；遵守 AGENTS.md 边界（不真实 index 提交、不整树 fmt、快照走临时 GIT_INDEX_FILE）；改 token_manager/stream 等带守卫文件时先读 `.claude/state/CURRENT.md` 防行号漂移。

### P0 — 正确性缺陷（立即）

| # | 事项 | 动机（对应发现） | 改法 | 验收标准 | 规模 |
|---|---|---|---|---|---|
| P0-1 | **Bug C 校验口径错配** | 3.1 CRITICAL：默认配置误杀 6 内置工具全部流式调用 | `find_missing_required_fields` 调用挪到 `map_tool_input_from_kiro` **之前**（stream.rs:2888 的 flush 前先校验 Kiro 形态原文），或 required 名单过一遍同源改名表 | 8 内置工具 × 合法完整参数 → 流式端到端**不置失败态**的回归各一条；Bash 缺参仍判缺；2171 不回退 | ~5 行 + ~10 测试 |
| P0-2 | 核对线上二进制 | 判定 P0-1 是"在燃"还是"待引爆" | 线上 `/healthz` 的 build_sha/version 对比 v1.1.2（Bug C 注入批次） | 结论写入 `.agent/HANDOFF.md` | 10 分钟 |
| P0-3 | **config/import 必现 500** | 3.3 MAJOR：端点从未可用（config_path 恒 None） | `Config` 加 `pub(crate) set_config_path`（或 `save_to(&Path)`），import 从 current 继承；`rotate_config_backup` 挪到校验全过后 | "构造 payload → import → 重读磁盘文件断言字段"行为测试；失败路径不轮换备份 | ~20 行 |
| P0-4 | quota_exhausted 告警归位 | 3.2 MAJOR：唯一 bump 点在 403 风控函数里，402 永不触发 | 两行连注释从 `report_suspicious_activity` 移到 `report_quota_exhausted` 禁用块 | 守卫测试断言 bump 调用点所在函数；402 路径行为测试 | 2 行 |
| P0-5 | persist_credentials 写串行化 | 3.2 MAJOR：并发旧快照后落盘 + SIGKILL = 死号复活 | 照抄同仓 cooldown.rs 已验证的 `save_lock` 模式（快照→写盘全程一把 Mutex，后到者重取快照） | 并发交错回归测试（两写并发，磁盘终态必为新） | ~30 行 |
| P0-6 | 嗅探 thinking 块未关即开 tool_use | 3.1 MAJOR：SSE 块序违规，CC 解析报错 | `process_tool_use` 补第三分支：`in_thinking_block && !reasoning_stream_seen` 且无闭合标签 → flush 残留 → 空 delta → signature_delta → stop → 再开 tool_use | 该形态时序测试（thinking start 后直接 toolUseEvent，断言 stop 先于 tool_use start） | ~10 行 |

### P1 — 记账 / 安全 / 运维（本月）

| # | 事项 | 动机 | 改法 | 验收标准 | 规模 |
|---|---|---|---|---|---|
| P1-1 | 断连 usage Drop 兜底 | 3.1 MAJOR：CC 按 Esc 中断即整条 usage+credits 丢失 | `(ctx, meta)` 包进 guard 结构，Drop 时未 emit 则按 `Interrupted` outcome 补记 + report_credits | 模拟客户端提前断开，断言记录落库、credits 上报 | ~60 行 |
| P1-2 | 空响应独立 outcome | 3.1 MINOR：客户端收 error 面板记 Success，失败率系统性低估 | 新增 `EmptyResponse` outcome（不污染熔断/健康信号的初衷保留） | 空响应路径记账断言测试 | ~30 行 |
| P1-3 | usage JSONL 保留清理 | 3.4 MAJOR：磁盘无界 + 冷启动重放全史 | 6h 清理任务顺带删 31 天外 `usage-*.jsonl`；`rebuild_from_logs` 按文件名日期只放最近 31 天 | 过期文件删除 + rebuild 跳过断言 | ~40 行 |
| P1-4 | 刷新错误分类结构化 | 3.2 MAJOR：子串匹配 4xx，瞬态误判 → 不可逆烧号 | `downcast_ref::<RefreshHttpError>()` 按 status 精判 4xx（排除 429），无状态码才留字符串兜底 | 端口含 4xx 数字的瞬态错误不入 failure_count 的回归 | ~30 行 |
| P1-5 | XFF 私网信任开关 | 3.4 MAJOR·疑似：LAN 直连部署可伪造 IP 绕限流/黑名单 | `trustPrivatePeerAsProxy` 配置项，默认 true（现状零回归），直连部署关闭 | 开关两态行为测试 | ~30 行 |
| P1-6 | Windows OTA 回滚链 | 3.3 MAJOR：主流部署形态升级崩 = 服务死亡 | `.bat` 启动后循环探 `/healthz` N 秒，失败 rename `.bak` 回原路径重拉；面板文案对齐真实语义 | 坏包演练（手测）+ bat 内容守卫测试 | ~40 行 bat |
| P1-7 | axios 401/403 分流 | 3.3 MINOR：业务 403 → 静默登出死循环（未爆雷） | 仅 401 清 key；403 按 `error.type === 'authentication_error'` 分流 | 前端手测两态 | ~5 行 |
| P1-8 | proxyUrl 拆内嵌账密 | 3.3 MINOR：脱敏导出/快照漏账密 | `update_config` 的 proxyUrl 分支复用 `split_proxy_credentials`；导出/快照对 userinfo 打码 | 导出脱敏断言测试 | ~20 行 |
| P1-9 | CSP frame-ancestors + 安全注释 sweep | 3.3 MINOR×2：登录页 iframe 钓鱼 + 陈旧注释误导评审 | CSP 追加 `frame-ancestors 'none'`；sweep "localStorage/无 CSP"过期注释为现状 | 响应头断言测试 | ~10 行 |
| P1-10 | push/PR CI 门禁 | 3.5 MAJOR：日常提交回归全靠本地自觉 | 新增 push 触发轻量 workflow：`cargo test --no-default-features --locked`，复用 rust-cache | 提交后 Actions 可见红绿 | 1 个 yml |
| P1-11 | 透传字节级承诺恢复 | 3.2 MINOR×2：无条件重序列化破坏契约、影响上游字节前缀缓存 | 请求侧仅 `map_target` 命中才重序列化，否则转发 `raw_body`；SSE delta 无改动回原字节（与 message_start 对齐）；passthrough.rs 顶部契约文档同步 | 未命中映射时输入输出字节一致断言 | ~20 行 |

### P2 — 工程债与生态借鉴（版本周期）

| # | 事项 | 动机 | 改法 / 切片 | 规模 |
|---|---|---|---|---|
| P2-1 | 神文件继续拆 | 3.5 MAJOR：Top4 近/超万行；3.3 神文件评估 | service.rs 四刀（余额缓存/socks 池/config 更新/自重启）+ handlers.rs/provider.rs/service.rs 内嵌测试外提 `#[path]` 兄弟文件（token_manager 模式已验证）+ settings-page 8 卡片拆文件 | 机械搬运，分批 |
| P2-2 | 双端点重复段收敛 | 3.1 MINOR：约 200 行重复 + 帧解码三份拷贝，历史已漂移过一次 | 提取 `decode_frames_into(ctx, chunk)` 与 `prepare_kiro_dispatch(payload)` 共享辅助 | 中 |
| P2-3 | 层间耦合修正 | 3.5 MAJOR：anthropic↔kiro 循环耦合；3.5 MINOR：common 越层 | `AbsorbClass` 下沉 `model/`；模型探测走 anthropic 暴露的窄接口；`ErrorResponse` 引用下沉 | 小-中 |
| P2-4 | serde_cbor → ciborium | 3.4/3.5 双报：RUSTSEC-2021-0127 停维护 | 替换 Web Portal rpc-v2-cbor 编解码 | 小-中 |
| P2-5 | 构建口径统一 | 3.5 MINOR×3：default 特性与出厂相反、工具链三处漂移、release test 无 --locked | `default = []`；rust-toolchain.toml 一处定义；release.yml 补 `--locked` | 小 |
| P2-6 | tracing span 引入 | 3.5 MINOR：零 span/#[instrument]，跨凭据重试链无法聚合（对照 kirocc 已内置 OTel） | 请求级 root span + 关键路径 `#[instrument]`，为将来 OTLP 导出留口 | 中 |
| P2-7 | 生态借鉴四点（4.1/4.3 结论） | 竞品已领先：模型感知正向路由、客户端 Key 租户隔离、OpenAI 会话亲和（prompt_cache_key→conversationId）、GPT 族 reasoning effort 通道 | 各自独立设计切片，先出一页设计再动手（走 grill 流程） | 大，分四片 |
| P2-8 | 选号临界区批量化收尾 | 3.2 观察：43 号池每次选号 43 次 String 分配 + 43 次 health 锁，全在 entries 临界区 | `p_avail_batch` 一次锁取全候选；排序键用 family_key 缓存 | 中 |
| P2-9 | insightText 结构码化 | 3.3 MINOR：en/ja 界面夹中文 | 返回 `{code, params}`，前端 i18n 渲染（cooldownCode 范式已验证） | 中 |
| P2-10 | admin 批量端点 | 3.3 MINOR：四个批量操作 N 次往返 | 按 `BatchDeleteResponse.results[]` 模式补 batchReset/Disable/Whitelist/Refresh | 小-中 |
| P2-11 | admin-ui 测试引入 | 3.5 MINOR：前端零测试，3350 行 settings 全靠手测 | vitest + testing-library；首批覆盖手写 diff 构造器（默认值双份漂移点）与 use-credentials hooks | 中 |
| P2-12 | MINOR 清扫批 | 各区剩余 MINOR | 每项独立小改动+具名测试，可搭车任意批次：dead-code `authentication_error`、默认端口 serde 缺省对齐 8990、estimate_tokens CJK 扩区、context pct 上界 clamp、close_open_blocks 换 BTreeMap、cooldown 恒真断言修正、fn_body 切片自检、opus 前缀匹配、零 sleep 自旋加 yield、mask_import_key 阈值、log_buffer seq 锁内分配、TraceDb Drop 注释改正、fs_atomic fsync 父目录+tmp 清扫、msg_xxx id 自生成对齐、include_usage 尊重请求、secret_store 文档改正、bg-img 签名 URL、IDC camelCase 迁移、prefill 丢弃 warn/头暴露、0.6657 缩放头标记、deploy 脚本 set -e、守卫 helper 提炼、sleep 测试去时序化、文档漂移 sweep（CLAUDE.md 架构树补 openai/model/admin_ui、MODULES.md 行数、ref 文档版本戳） | 小 × N |

**排序依据**：P0 全部是"结论确定的正确性缺陷"且修复面小（合计约百行级）；P1 是有真实运营影响但有 workaround 或触发条件的项；P2 按"防再犯 >新能力"排——神文件与重复段收敛（P2-1/2/3）优先于借鉴项（P2-7），因为本次 CRITICAL 的成因模式（组合处口径错配 + 测试盲区）正是这类结构债的直接产物。

---

## 6. 附录：全部发现汇总表

级别：C=CRITICAL，M=MAJOR，m=MINOR，O=观察/正面记录。"疑似"= 审查员标注需复核。计划列指向第 5 节条目。

| 区 | 级 | 位置 | 摘要 | 计划 |
|---|---|---|---|---|
| anthropic | **C** | stream.rs:3097/:2881 | Bug C 校验在参数还原后用 Kiro 形态 required，6 内置工具流式恒败（主线已复核） | P0-1/2 |
| anthropic | **M** | stream.rs:2725-2776 | 嗅探 thinking 块未关即开 tool_use，违反块序契约 | P0-6 |
| anthropic | **M** | handlers.rs:3306/:4707 | 客户端断连整条 usage+credits 静默丢失（无 Drop 兜底） | P1-1 |
| anthropic | m | stream.rs:1713 vs :2554 | 丢弃 thinking 两形态计数口径相反，near-empty 判定漂移 | P2-12 |
| anthropic | m | stream.rs:600-615 | close_open_blocks 迭代 HashMap，stop 顺序不确定 | P2-12 |
| anthropic | m | stream.rs:4340-4346 | context pct 无上界钳制，脏值 10 倍窗口入账 | P2-12 |
| anthropic | m | stream.rs:208 vs :1028 | 同消息 input_tokens 两口径（0.6657 缩放，刻意），无头标记 | P2-12 |
| anthropic | m | handlers.rs:3427/:4249 | 空响应发 error 记 Success，失败率低估 | P1-2 |
| anthropic | m疑似 | invoke_xml.rs:91-102 | 参数值内字面 `</invoke>` 跨 chunk 可提前截断 | 观察 |
| anthropic | m | converter.rs:741-753 | assistant prefill 静默丢弃（已文档化偏差） | P2-12 |
| anthropic | m | stream.rs:3719-3738 | estimate_tokens 只认 U+4E00-9FFF，日韩低估一半 | P2-12 |
| anthropic | m | handlers.rs:2780/:4381 | 双端点 200 行重复 + 帧解码三份拷贝 | P2-2 |
| kiro | **M** | token_manager.rs:6730 | quota_exhausted 告警 bump 在 403 风控函数，402 永不触发 | P0-4 |
| kiro | **M**疑似 | credential_persist.rs:10-76 | persist_credentials 无写串行化，旧快照可后落盘 | P0-5 |
| kiro | **M**疑似 | token_manager.rs:1772 | 刷新错误子串匹配 4xx，瞬态可误判烧号 | P1-4 |
| kiro | m | passthrough.rs:243-263 | 请求体无条件重序列化，破坏字节级承诺 | P1-11 |
| kiro | m | passthrough_think_filter.rs:561/594 | SSE delta 无条件重序列化，与 message_start 不对称 | P1-11 |
| kiro | m | token_manager.rs:3711 | custom_api 竞态分支零 sleep 自旋（有界） | P2-12 |
| kiro | m疑似 | token_manager.rs:4050 | opus 硬门 contains 子串匹配 | P2-12 |
| kiro | m | cooldown.rs:954-967 | test_cooldown_incremental 恒真断言 | P2-12 |
| kiro | m | token_manager_endpoint_bypass_guard_tests.rs:11 | 守卫 fn_body 切片近似可静默弱化 | P2-12 |
| kiro | O | token_manager.rs:4245/:4266 | 选号临界区每候选 String 分配 + health 锁（O(n) 残留） | P2-8 |
| admin | **M** | service.rs:5394/:5489 | config/import 必现 500，端点从未可用（主线已复核） | P0-3 |
| admin | **M** | health_marker.rs:21/service.rs:5799 | Windows OTA 无自动回滚链，升级崩=服务死亡 | P1-6 |
| admin | m | admin_ui/router.rs:784-890 | bg-img 匿名出站 GET 代理无频控/签名 | P2-12 |
| admin | m | service.rs:4814/:5350 | proxyUrl 内嵌账密漏进脱敏导出/快照 | P1-8 |
| admin | m | handlers.rs:1373-1418 | IDC 登录唯一 snake_case，前端 as any 适配 | P2-12 |
| admin | m | credentials.ts:68-80 | axios 401/403 混判 → 业务 403 静默登出死循环 | P1-7 |
| admin | m | insight.rs:24-57 | 后端中文-only 展示串夹进 en/ja 界面 | P2-9 |
| admin | m | admin_ui/router.rs:618-624 | CSP 缺 frame-ancestors，登录页可被 iframe 钓鱼 | P1-9 |
| admin | m疑似 | types.rs:1936-1945 | mask_import_key len=13 泄漏 12/13 字符 | P2-12 |
| admin | m | handlers.rs:897 等 | 陈旧安全注释（localStorage/无 CSP）误导评审 | P1-9 |
| admin | m | dashboard.tsx:375-537 | 四个批量操作逐 id N 次往返 | P2-10 |
| admin | O | — | 匿名面全部有防护；鉴权单点收口；i18n 三语 2367 键零缺失 | — |
| infra | **M** | usage_stats.rs:1200-1251 | usage JSONL 无保留清理，冷启动重放全史 | P1-3 |
| infra | **M**疑似 | security.rs:296-341 | 私网对端自动信任 XFF，LAN 直连可伪造 IP | P1-5 |
| infra | m | trace_db.rs:729 vs main.rs:1101 | TraceDb Drop 停机兜底永不执行，注释矛盾 | P2-12 |
| infra | m | log_buffer.rs:58-77 | seq 锁外分配可乱序，增量游标永久跳行 | P2-12 |
| infra | m | openai/convert.rs:1988/2092 | 非流式仍透出上游 msg_xxx id，与流式修复不一致 | P2-12 |
| infra | m | openai/convert.rs:1334-1360 | usage chunk 不看 include_usage，偏离 OpenAI 规范 | P2-12 |
| infra | m | secret_store.rs:9-12 | 模块文档与实现漂移（机器绑定 vs 随机密钥文件） | P2-12 |
| infra | m | fs_atomic.rs:125-144 | rename 后不 fsync 父目录；崩溃残留 .tmp 无清扫 | P2-12 |
| infra | m | Cargo.toml:24 | serde_cbor 停维护（RUSTSEC-2021-0127） | P2-4 |
| infra | m | Cargo.toml:7 | default=native-tls 与恒 --no-default-features 纪律相反 | P2-5 |
| infra | m在案 | ssrf.rs:404/446 | DNS 失败 fail-open（自述）；proxy 两旁路不经校验 | P2-12 |
| infra | m | trace_db.rs:708-722 | 低流量最后一批 pending 无定时器兜底 | P2-12 |
| infra | O | — | 吸收层默认关四前置、AIMD 单向棘轮、healthz 低敏暴露（均已论证/在案） | — |
| arch | **M** | absorb_policy/converter | anthropic↔kiro 生产级循环耦合（AbsorbClass 6 处 + 探测直调 converter） | P2-3 |
| arch | **M** | service.rs 等 Top4 | 神文件近/超万行，测试仍内联 | P2-1 |
| arch | **M** | .github/workflows/ | 无 push/PR CI 门禁，回归靠本地自觉 | P1-10 |
| arch | **M** | Cargo.toml | serde_cbor（与 3.4 双报，同一项） | P2-4 |
| arch | m | security.rs:27 | common 越层引用 anthropic::types | P2-3 |
| arch | m | config.rs:1264 vs main.rs:182 | 默认端口双重真相 8080/8990 | P2-12 |
| arch | m | release.yml/deploy-build.yml/Dockerfile | 工具链三处漂移（1.97.1/stable/1.96） | P2-5 |
| arch | m | release.yml | test job 无 --locked | P2-5 |
| arch | m | deploy/*.sh | 7 脚本 5 个无 set -e | P2-12 |
| arch | m | 全仓 181 守卫 | 守卫测试维护面大，helper 未共享 | P2-12 |
| arch | m | 全仓 tracing | 零 span/#[instrument]，链路无法聚合 | P2-6 |
| arch | m | admin-ui | 前端零测试 | P2-11 |
| arch | m | 24 处 sleep | 测试时序依赖，慢 CI flake 风险 | P2-12 |
| arch | m | CLAUDE.md/MODULES.md 等 | 文档漂移（架构树缺三目录、行数过期） | P2-12 |

---

## 7. 方法与可信度声明

- 5 个分区审查员均为只读审查（未改文件、未跑 cargo、未做 git 写操作），输出统一要求"文件:行号 证据 + 为什么是问题 + 修法"；无法逐行覆盖的区域已在各节如实标注。
- 主线对两个最重断言做了独立复核：Bug C CRITICAL（四环证据链逐环验证：合成 schema required→提取口径→还原时序→默认开关）与 config/import 必现 500（路由注册→serde skip 私有字段→无 setter→save 报错）。其余"疑似"项按审查员原标注保留，落地前需按 P 级计划复核。
- 行号基于 2026-08-21 的 `master` 脏树（= `quality-up/after-v1.1.2`），后续编辑会漂移，定位以符号名为准。
- 联网研究（第 4 节）核实时间 2026-08-21，竞品版本以当日 GitHub API 为准。
