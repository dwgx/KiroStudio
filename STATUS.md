# 当前状态入口（审计快照）

> 更新时间：2026-08-15（v1.1.1 发版 + 代挂严格语义修复 + 线上配置钉死）。
>
> 本文件只记录已经核对过的边界：`HEAD`、未提交工作树、线上观察、以及设计/未验证项。
> 需要继续接手时，读本文件后再读 `docs/TAKEOVER.md` 与 `.claude/state/CURRENT.md`。
> 根目录的 `HANDOFF-*`、`PLAN-*`、`TRACKING-*`、`OPEN-ISSUES-*` 和旧 `STATUS-*` 都是
> **历史档案**，不能覆盖本文件的当前结论。

## 先看结论

- **HEAD** = `1e100a2`（代挂严格语义修复，2026-08-15）。`origin/master`（dwgx/KiroStudio-skiapi 私有）已推，一致。
- 版本号 `1.1.1`（Cargo.toml）；`v1.1.1` 已发版（帮助中心知识库）：public release 5 产物 + sha256 齐，release.yml 全 job 通过。
- **工作树有未提交改动**（多会话并发）：CHANGELOG.md / CLAUDE.md / .github/workflows/release.yml / Cargo.lock / .gitignore 等处于 M 状态——**可能是其他会话的改动，接手时先 `git status` 核对再动**，禁止裸 commit/checkout。
- **线上（nbus 38.244.34.15:8990）**：v1.1.1 + 严格语义修复 build（部署于 2026-08-15 03:40），`/healthz` version 1.1.1 pool 4。公网 api.dwgx.top（FurCDN）全通。
- `public`（dwgx/KiroStudio 公开仓）master 冻结（CLAUDE.md 纪律，停在 7368a70）；只推 tag（v1.0.1/v1.1.0/v1.1.1 发版例外）。
- CI 验证：`cargo test --no-default-features` **1766 passed / 0 failed**（skiapi Docker）+ admin-ui tsc/build 通过。
- 线上凭据配置（2026-08-15 实测，模型边界已钉死）：
  - #1 fuckopencode（opencode 网关 127.0.0.1:8788）`allowed_models=[deepseek-v4-pro, deepseek-v4-flash, deepseek-v4-flash-free]`，启用
  - #2 deepseekapi-dwgx（DeepSeek 官方 api.deepseek.com/anthropic）`allowed_models=[deepseek-v4-pro]`，启用
  - #4 pig code（cdn.pigcode.org）`allowed_models=[gpt-5.6-sol, gpt-image-2, codex-auto-review]`，**启用中**（legacy 迁移自动解禁——它是 gpt 专用号，opus 误伤禁用已消除）
  - #3 cursorapi 禁用

## 证据分层

| 层级 | 当前可写的结论 |
|---|---|
| `HEAD` | `1e100a2` 及其祖先为已提交代码（含波次 1-4、帮助中心、模型黑名单、严格语义修复）。 |
| 工作树 | 有未提交改动（并发会话，见上）。 |
| `origin` | 已推 master（私有，唯一开发仓库）。 |
| `public` | master 冻结（7368a70）；v1.1.1 tag + release 已推（发版例外）。 |
| 线上 | nbus v1.1.1+严格语义 build 运行中（2026-08-15 03:40 部署验证）。 |
| Release | `v1.1.1` = fb682e1；5 产物 + sha256（public）。**注意**：严格语义修复（1e100a2）在 v1.1.1 之后，release 产物不含它——nbus 跑的是修复后 build，若从 release 下载安装会缺修复。 |
| 设计/计划 | `docs/` 中 RFC、spec、plan 只能证明设计或研究存在，除非有当前代码和测试证据，不得写成已实现。 |

## 2026-08-13/14/15 波次与修复（全部 review + CI 验证）

### 波次 1-4（v1.1.0 内容，已发版）
- 选号/性能（rpm 批量/health 读路径去写/族键缓存/模型级限流/排序键 12 键）、config 三写路径同锁、cooldown 持久化、告警 webhook（8 接入点）、403 封禁识别、成本核算（model_pricing）、代理池自动健康调度、诊断快照端点、OTA 自动检查（默认关）、版本伪装、KAM 导出、/healthz、CSP、备份轮换。
- 前端：接入信息页、点击掩码复制完整 Key、画布右键菜单、OTA 进度、设置页三开关、帮助中心。

### v1.1.1（帮助中心，2026-08-14 发版）
- 帮助中心知识库（55 条目 + 架构地图 + 联网搜索 DDG+Bing 兜底 + 接入信息并入），`/help` 直达路由，设置页右上角帮助按钮。
- 三平台打包（linux/macos×2/windows）全过，nbus 已部署 v1.1.1。

### 透传选号修复（2026-08-14/15，nbus 在线验证）
1. **模型黑名单**（1e100a2 前身）：上游明确返回模型不支持（model_not_found / no available channel / model not found / unknown model）→ 记 (credential, model) 黑名单 → 选号跳过。
2. **严格语义修复**（1e100a2，对齐 sub2api/newapi）：
   - `effective_model` fallback 兜底**只对 claude-\* 生态**生效——gpt-\* 保持原名（白名单对原名判定，不匹配即过滤）——gpt 请求不再被改写进 deepseek 链
   - 模型黑名单 TTL 60s → **30min**（sub2api 同款：模型不支持是稳定属性）
   - 关键词 + `unknown model`
3. **线上配置钉死**（nbus credentials.json）：fuckopencode/pigcode 补 allowed_models（见上表）。

### 效果（nbus 实测）
- gpt-5.6-sol → pigcode（凭据 4）成功出话；claude-opus-5 → deepseek 链（归一化设计）；交叉请求选号阶段直接过滤，0 白付一跳。
- pigcode 复活：之前被 opus 误伤自动禁用（503），legacy 迁移自动解禁 + 严格语义下 opus 不再打它 → 稳定服务 gpt。

## 已知遗留（当前代码里有 TODO / 半接线 / 未做）

- **opencode 配置（用户侧）**：kirostudio provider 指向 k1ro.skiapi.dev（skiapi，Kiro 池冷却中）——建议切 api.dwgx.top + deepseek-v4-pro（唯一稳的路）。**待用户确认**（key 由用户提供）。
- **gpt-5.6-sol 请求来源**：03:12 两条来自未知客户端（session 41e82a06/7c1b459e，IP 未记录）——用户环境里排查。
- **Console 额度**：fuckopencode 上游 opencode Console 周额度 2 天后重置 + IP 日窗（200 请求/天 UTC 零点）——v4-flash 暂不可用，v4-pro（DeepSeek 官方）可用。
- **P0 上号实测清单**（todo-2026-08-13.md 二-1）：native effort、指纹命中率、error_message 盲区、websearch TTFB、401 判据。
- **native_thinking_effort_enabled 决策**：默认关，上号实测后开。
- **/v1/responses 入站**：有人实现过（openai/handlers.rs post_responses），确认现状即可。
- **客户端 Key 分发 csk_**（P3-35）：未来分享场景，待决。
- **websearch 整轮缓冲改造**：设计文档已备（docs/archive/websearch-buffering-rework.md），等上号实测。
- **upstream_trace DROPPED/WRITTEN 计数器无消费点**。
- **A-5/A-6 低危项**：429 换区后 403 绕圈；select_endpoint 备区只取 PROBE_ORDER 第一项。
- **D 类阈值**：无真实故障分布数据，先修度量再调参。

## 未验证、未做与阻塞

### 未验证
- nbus 功能回归（部署做了健康/端点/选号验证——真实流量下黑名单/严格语义的长周期表现待观察）。
- 前端视觉观感（tsc/build 通过，未做浏览器回归）。
- v1.1.1 release 产物不含严格语义修复（1e100a2）——若走 OTA/从 release 下载需注意。

### 未做（有意，有证据）
- 未动 public master（冻结纪律）。
- 未填 OTA token；OTA 自动检查默认关。
- 未动 fuckopencode 网关（其 IP 日窗限流识别已是正确行为）。

### 需要 owner 决策
- opencode 配置是否切 api.dwgx.top（v4-pro 唯一可用路）。
- gpt-5.6-sol 请求来源排查。
- 是否发 v1.1.2（含模型黑名单 + 严格语义修复，release 产物落后于线上）。
- 是否开启 native_thinking_effort_enabled。

## 下一步（仅 owner 批准后执行）

1. opencode 配置切 api.dwgx.top + deepseek-v4-pro（用户给 key）。
2. 发 v1.1.2（bump → CI → tag origin+public → Actions → nbus）让 release 产物追上线上修复。
3. 上号实测清单 + 压测回归（v1.0.1 基线：QPS 1702 / P99 319ms / 0 击穿）。
4. 后续发版流程照旧：波次实现 → review → CI（skiapi Docker）→ bump → push origin → tag + public release（ALLOW_PUBLIC_PUSH=1）→ nbus 部署验证。

## 历史档案入口

所有 `HANDOFF-*`、`HANDOFF-NEXT*`、`OPEN-ISSUES-*`、`STATUS-2026-08-05.md`、`PLAN-*`、
`TRACKING-*` 均保留为证据与推导材料。历史文档中的数字、线上状态、行号和「已上线/待修」
措辞必须先用本文件和当前代码重新核对。
