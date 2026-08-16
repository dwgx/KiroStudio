# 工程/流程层绊脚石清单（blockers-engineering）

> 2026-08-15 工程流程研究员输出。基于本会话真实案例（部署漏 admin-ui ×2、行号漂移 +770、
> 文档滞后、并发会话干扰线上、RUST_LOG=warn 排障受阻、token 交换零日志、观测盲区、OTA 全 403）。
> 每项：位置 + 导致的问题 + 严重度 + 修法建议。全部为现状核实（代码/文档/线上命令实读），
> 非猜测。严重度：BLOCKER（已造成线上事故）/ MAJOR（高概率事故或已造成排障损失）/ MINOR。
> **闭环状态（2026-08-16 核验）**：本文档列出的流程绊脚石已全部修复——B1 快照命令统一 +
> verify-snapshot.sh（W10-W12 #6）+ build.rs KEY_FILES（W13 #6 彻底统一）、双层日志 filter（#7）、
> token 交换日志（W9）、观测盲区告警扩展（W10-W12）。正文保留为审计快照。

---

## 一、部署流程

### B1. 快照命令三份文档互相矛盾，git archive 漏未跟踪文件（BLOCKER，已造成 ×2）

**位置**：`CLAUDE.md:539`（`git add -A -- src` 只加后端）与 `CLAUDE.md:562` / `.opencode/WORKFLOW.md:76`
（`git add -A -- src admin-ui` 两段）与 `.claude/state/CURRENT.md:102`（`git add -A -- src` 又只加后端）
三处不一致。`git archive` 只含快照分支已提交内容，工作树 untracked 文件不进包。

**导致的问题**：`admin-ui/src/components/error-messages-dialog.tsx`（untracked）两次漏进部署 →
前端 UI 组件缺失 → 两轮 BLOCKER。同类风险文件当前仍存在（工作树 186 个未提交文件，含
`conn-page.tsx` / `help-page.tsx` 处于「已删除 + 未跟踪重建」双状态，快照命令漏路径即丢）。
**易漏类型**：① 任何 untracked 新组件/新文件；② 删除后重建的双状态文件；③ 非 src 目录
（docs/、.opencode/、Dockerfile、config 模板——部署机需要但它们不进 `-- src` 路径）。

**修法建议**（按收益排序）：
1. **快照/部署脚本化**：写 `scripts/snapshot.sh`，固定 `git read-tree HEAD && git add -A -- src admin-ui/src`
   （注意 `admin-ui/src` 而非 `admin-ui`——后者会把 `admin-ui/data/*.db-shm`、`scan-ast.tmp.mjs`
   等垃圾带进包）。三份文档统一引用脚本，删除各写各的命令块。
2. **部署前新文件核验**：脚本内快照后对比「快照分支 `git ls-tree -r` 文件清单」vs「本地工作树
   文件清单」，列出部署目标路径下缺失的 untracked 文件并失败退出（不通过不许 archive）。
3. **部署后自检**：部署完成 curl 前端关键资源存在性（组件对应的 JS chunk 或路由可访问），
   防「构建成功但包缺文件」静默上线（注意 strings 查二进制不可靠，见 CURRENT.md 试过不通的路 1）。

---

## 二、状态文件管理

### B2. 六件套全手写同步，守卫行号常态化漂移（MAJOR，已造成 +4~+770）

**位置**：`.opencode/WORKFLOW.md:92-104`（七件套职责表）+ `.claude/state/CURRENT.md:30-56`（守卫清单）。

**导致的问题**：每个波次结束要手动同步 7 个文件；守卫清单按行号记，工作树改动大（一次漂移
+770）后全部过时 → 核验 agent 按旧行号找守卫，守卫「存在性」变成「存在性 × 位置」双重判断。
STATUS.md 记「W2 build」而线上已部署 W8（sha a3ae8874）——文档滞后是同步负担的直接后果。

**修法建议**：
1. **守卫按符号名 + needle 锚，不按行号**：守卫清单改成「符号名 + 唯一 needle 子串」双字段，
   行号变成可自动刷新字段。写 `scripts/refresh-guards.sh`：读 CURRENT.md 守卫清单 → 对每个
   符号名 `rg -n` 当前行号 → 输出刷新后的清单段（人工确认后替换）。核验 agent 用
   `rg <symbol> CURRENT.md` 判守卫存在，行号只作定位参考。
2. **波次收尾脚本**：`scripts/finish-wave.sh` 自动填六件套的机械字段（HEAD、`git status --porcelain`
   计数、CI 数字、新 untracked 清单），人工只写内容段落。脚本只读 git，不违反禁止裸 git 操作纪律。
3. **部署流程强制文档步骤**：部署成功后第一步 = 更新 STATUS「线上现状」节 + Cargo 版本，把
   「文档同步」从纪律变成流程的一部分（W8 部署后 STATUS 仍记 W2 就是缺这一步）。

---

## 三、日志体系

### B3. RUST_LOG=warn 写死在 systemd unit，INFO 全灭（MAJOR，已造成排障受阻）

**位置**：nbus `/etc/systemd/system/kirostudio.service` 的 `Environment=RUST_LOG=warn`
（注释：「WARN 起步，避免刷屏」）。代码侧 `main.rs:228` 默认 `EnvFilter::new("info")`，但被
环境变量覆盖；`main.rs:230-234` 两层（fmt 终端层 + LogBufferLayer 面板环形缓冲层）**共享同一
filter**。

**导致的问题**：websearch 判定「无 INFO 日志」、token 交换失败排障只能靠 warn/error——
大量关键路径的 info 级进度（选号、转发、失败埋点、恢复动作）在线上不可见，排障被迫重启
改环境变量或盲猜。

**修法建议**（最小改动治本）：**双层 filter 拆开**——fmt 层保留严格 filter（warn，终端不刷屏），
LogBufferLayer 用宽松 filter（info，面板实时日志永远可见 info+）。一行级改动，面板排障能力
质变。备选：unit 改成 `RUST_LOG=info,<高频target>=warn` 白名单降噪；或加运行时调级端点
（admin API 改级别不重启），适合低频但需即时生效的场景。

### B4. 日志脱敏是单点修复，无全局纪律（MINOR→MAJOR 若扩散）

**位置**：`src/admin/external_idp_login.rs:835` 等处有 Bearer 头，W9 修了 token 交换日志
（status+body 截断 + code 只记长度/前 4 位），但那是单点修复。

**导致的问题**：zyphr 反面教材就是「日志泄漏 Authorization」（ISSUES (e)）。本仓目前没有
守卫钉住「Authorization/refreshToken/clientSecret 不进 tracing 宏参数」这条纪律，新增日志
代码时全靠自觉。

**修法建议**：加源码守卫——测试段检查生产代码的 `tracing::debug!/info!` 宏参数不含
`Authorization` / `refresh_token` / `client_secret` 等字面量（同现有守卫范式，needle 拼接 +
删目标必红）。日志封装层面可做一个统一脱敏 helper（截断 + 脱敏），新代码强制走 helper。

---

## 四、可观测性缺口

### B5. poolHealth total=0/enabled=0 语义误导（MAJOR）

**位置**：`src/admin/types.rs:1605` `DiagnosticsPoolHealth` —— 它是 **socks 代理池**健康摘要
（代理自动健康调度），不是凭据池。线上未配代理节点 → total=0/enabled=0 恒常显示。

**导致的问题**：面板/诊断快照里这一栏看起来「池死了」，与凭据池实际健康（/healthz pool=4
全绿）矛盾，误导排障方向；且没有「凭据池可用号数」的等价摘要字段（pool_count 只有 healthz
有，且无 enabled 拆分）。

**修法建议**：诊断快照的 pool_health 加字段或新增 `credential_pool` 摘要（total/enabled/
disabled/cooldown/熔断数，来源 token_manager 全量状态，零上游内存数据同现有诊断口径），
代理池为空时返回 `null` + `note: "未配置代理节点"` 而非误导性的 0/0。

### B6. endpoint-health 冷启动/低流量盲区（MAJOR，与 B5 同源）

**位置**：`src/kiro/endpoint_health.rs:127` EWMA 表**纯内存、不持久化**；
`admin-ui/src/components/ops-page.tsx:418` 空态注释承认「表不持久化，重启后要等第一批请求」。

**导致的问题**：重启后 / 低流量窗口，面板看不到任何端点健康数据（items=[]）；EWMA 基线随
重启清零，冷启动的前 N 个请求在无基线下被派发，健康判定退化。

**修法建议**：EWMA 表落盘（复用 traces.db 或单独 JSONL，启动时恢复），或诊断快照给
「近 N 分钟样本数」兜底字段。至少把「空态 = 重启后无样本」在面板上显式标注（当前只有
通用 emptyDesc 文案，运维会误判为故障）。

### B7. 无「配置接线完整性」启动自检（MAJOR）

**位置**：`main.rs:565-595` 启动播种点散落 ~10 处（set_mock_cache_config / set_error_messages /
set_ip_blocklist / set_trust_forwarded_header / prompt_cache / native_effort / upstream_trace /
collect_fingerprint / error_messages 等），无集中校验。W7 曾踩「reload_config 需调
set_error_messages 否则配置不生效」的接线坑（state.md W7 记录，后修复）。

**导致的问题**：镜像未接线 = 配置静默无效（配置显示已设、行为走默认），热更路径每个新镜像
都需要手动补 setter，漏一个就出现「面板改了没反应」。

**修法建议**：main.rs 启动末尾加 `wiring_self_check()`：逐项断言各进程镜像与配置当前值一致，
不一致打 ERROR 并计入启动告警（走 alerting）。热更 setter 保持「改镜像 + 校验函数」成对模式，
新增镜像时把校验函数加入自检清单（同守卫纪律：删校验必红）。

---

## 五、告警

### B8. 告警覆盖窄：无「号池全灭 / 配额耗尽 / 数据断更」出口（MAJOR）

**位置**：`src/common/alerting.rs` webhook 机制完备（key/冷却/禁重定向/守卫），但全仓 bump
调用点只有 5 个 key：`absorb_retry_quota_exhausted` / `absorb_budget_exhausted` /
`absorb_pool_cooldown` / `failover_exhausted`（provider.rs:4157-4298）+ `credential_disabled`
（token_manager.rs:6168-6723）。STATUS 波次 1-4 记「告警 webhook 8 接入点」与现状不符。

**导致的问题**：① 号池全灭（pool_count=0）无告警——所有请求必失败只能从用户报障发现；
② 配额耗尽（quota_exhausted 首次触发）无告警；③ minutely.jsonl 断更案例（CLAUDE.md 教训：
「systemctl is-active 只证明进程活着，不证明它在产出数据」，断更两天无人发现）说明
「数据新鲜度」类信号没有监控出口。

**修法建议**：
1. 关键信号接入 bump：池全灭（调度层选号时 pool 空 + 首次）、配额耗尽首次触发、
   错误码表播种失败（B7 联动）。
2. 「数据新鲜度」类做成独立 watchdog：systemd timer 或部署侧 cron 检查 usage jsonl mtime /
   healthz 可达 / traces.db 写入量，断更即调 webhook（进程活着≠数据在产，直接监控 mtime）。
3. 面板「运维」页给告警 key 的触发计数视图（recovery_metrics 已有单调计数器，接上即可）。

---

## 六、外部依赖单点

### B9. OTA 依赖 gh-proxy 镜像，镜像全挂 = 全 403（MAJOR，本会话实测）

**位置**：`src/admin/update.rs:131-178` 镜像列表 gh-proxy.org ×4（含 hk/cdn/edgeone），
无 token 时**只走第三方镜像**（token 存在才走 GitHub 直连，避免 PAT 泄露给镜像，
见 update.rs:618 安全注释）。线上 `/etc/kirostudio/update.env` token 为空。

**导致的问题**：OTA「检查更新」按钮必失败（实测全 403）——镜像是我们不可控的第三方，
挂一个就是整条 OTA 不可用；且镜像被劫持的风险在代码注释里已承认。

**修法建议**：
1. 无 token 场景补**直连兜底**：公开仓 release 元数据（`api.github.com`）无需 PAT，
   直连 API 是合法兜底（token 才需要保护，元数据 GET 不泄露任何凭据）。
2. 或把 OTA 元数据端点搬到自有服务（api.dwgx.top 挂一个 release.json），彻底摆脱第三方镜像。
3. 镜像可用性做启动自检（B7 联动）：全部镜像不可达时告警，而不是按钮点了才报 403。

### B10. 上游单点（Kiro auth 500）——已修复，保留降级观察（已解决 / 观察）

**位置**：token 交换链路（external_idp_login / idc_login），W9 修：服务端 warn 日志
（status+body 截断+code 脱敏）+ 5xx 重试 1 次（500ms）+ 4xx/5xx 文案区分 + 6 测试。

**导致的问题（历史）**：上游 Kiro auth 服务 500 时**零日志**，用户报「Token 交换失败 500 Oops」
无法排障——根因是上游 500 非我们问题，但当时连「确认不是我们问题」都做不到。

**现状**：已修复。保留项：Kiro auth 是单点依赖（4 凭据中 #3 cursorapi 依赖本机 8008、
#4 pigcode 依赖 cdn.pigcode.org），上游故障时降级路径依赖 failover 链，建议把「上游 5xx
连续 N 次」也计入告警（B8 联动）。

---

## 七、版本/发版

### B11. Cargo.toml 1.1.1 与线上二进制漂移，版本无法回答「跑的是哪个 commit」（BLOCKER 级隐患）

**位置**：`Cargo.toml:3` `version = "1.1.1"`；线上 nbus 跑 W8 build（sha a3ae8874，含 W2-W9
全部修复）；`/healthz` 返回 `"version":"1.1.1"`——编译期版本，与线上实际代码状态无对应关系。
release 产物（v1.1.1）落后线上全部修复（STATUS.md:32 已标注：从 release 下载安装会缺全部
修复）。

**导致的问题**：① 「线上跑什么」只能靠 sha256 人工比对（部署时记，之后无人知道）；
② STATUS/CLAUDE.md 版本断言全部滞后（记 W2 实际 W8）；③ 发版决策（v1.1.2）悬置多日，
release 产物与线上永久漂移，任何人按 release 安装都是旧版。

**修法建议**：
1. **healthz/诊断快照加 build 标识**：编译期注入 git 短 sha（`build.rs` 或环境变量），
   healthz 返回 `"version":"1.1.1","build":"<sha>"`——「线上跑哪个 commit」从此可机械回答，
   B2 的文档滞后根因去掉一半。
2. **部署即 bump**：部署脚本成功后将 Cargo.toml patch 版本 +1（1.1.1→1.1.2）并记录
   sha→版本映射到 STATUS；v1.1.2 发版流程照旧（bump → CI → tag → Actions → nbus）。
3. 发版产物加「线上 sha 匹配」校验步骤：release 产物构建 sha 与线上 sha 一致才算发布完成。

---

## 最值得先动的 3 个工程绊脚石

| # | 绊脚石 | 为什么先动 | 改动量 |
|---|---|---|---|
| 1 | **B1 快照命令统一 + 新文件核验**（部署漏 admin-ui ×2 的根因） | 已造成两轮 BLOCKER，修复=一个脚本 + 文档统一，收益立竿见影；顺手清掉 `admin-ui/data/*.db-shm` 等入包垃圾 | 小（1 脚本 + 3 处文档引用替换） |
| 2 | **B3 双层日志 filter**（RUST_LOG=warn 全灭 INFO） | 面板环形缓冲层改 info filter，fmt 层保持 warn——排障能力质变（websearch「无 INFO」、token 交换零日志同根因），一行级改动无风险 | 极小（main.rs:228-234） |
| 3 | **B11 healthz 加 build sha + 部署即 bump** | 「线上跑什么版本」从不可回答变可回答——版本漂移、STATUS 滞后、release 落后三者共享此根因；发 v1.1.2 前先有准确基线 | 小（build.rs + healthz 一行 + 部署脚本一步） |

三者共同点：都是「流程/观测基建」而非功能代码，修复后所有后续波次的排障与部署成本同步下降。
