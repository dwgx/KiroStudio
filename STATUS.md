# 当前状态入口（审计快照）

> 更新时间：2026-08-14（本次更新，v1.1.0 发版 + nbus 部署完成）。
>
> 本文件只记录已经核对过的边界：`HEAD`、未提交工作树、线上观察、以及设计/未验证项。
> 需要继续接手时，读本文件后再读 `docs/TAKEOVER.md` 与 `.claude/state/CURRENT.md`。
> 根目录的 `HANDOFF-*`、`PLAN-*`、`TRACKING-*`、`OPEN-ISSUES-*` 和旧 `STATUS-*` 都是
> **历史档案**，不能覆盖本文件的当前结论。

## 先看结论

- **HEAD** = `d06b9dc`（v1.1.0，2026-08-14）。`origin/master`（dwgx/KiroStudio-skiapi 私有）已推，一致。
- 版本号 `1.1.0`（Cargo.toml）；**`v1.1.0` 已于 2026-08-14 打 tag 并推送**（origin + public），
  release.yml 全 job 通过，Release 产物齐（linux / macos×2 / windows + sha256，抽验一致）。
- **`public`（dwgx/KiroStudio 公开仓）master 冻结**（CLAUDE.md 纪律，停在 7368a70）——只推了
  `v1.1.0` tag（发版例外，OTA 目标就是 public release）。
- 分支：`master`（HEAD `d06b9dc`）、`ci/verify-adaptive`（波次 CI 快照，= d06b9dc）。
- 已核对：`cargo test --no-default-features` **1760 passed / 0 failed**（skiapi Docker）；
  admin-ui tsc + build 通过。
- **2026-08-14 已部署**：nbus（38.244.34.15:8990，systemd kirostudio.service）运行 v1.1.0，
  `/healthz` 返回 `{"config_loaded":true,"ok":true,"pool_count":3,"sqlite_writable":true,"version":"1.1.0"}`；
  公网 api.dwgx.top（FurCDN）全通。回滚点 `/opt/kirostudio/kirostudio.bak-1.0.0`。

## 证据分层

| 层级 | 当前可写的结论 |
|---|---|
| `HEAD` | `d06b9dc` 及其祖先为已提交代码（v1.1.0）。 |
| `origin` | 已推 master（私有，唯一开发仓库）。 |
| `public` | master 冻结（7368a70）；`v1.1.0` tag + release 已推（发版例外）。 |
| 线上 | nbus v1.1.0 运行中（2026-08-14 部署验证通过）。 |
| Release | `v1.1.0` tag = d06b9dc；5 产物 + sha256 齐（GitHub Release，public）。 |
| 设计/计划 | `docs/` 中 RFC、spec、plan 只能证明设计或研究存在，除非有当前代码和测试证据，不得写成已实现。 |

## 本次实际验证（2026-08-14，skiapi 服务器 Docker + nbus 线上）

- `docker build --target builder`：编译通过（波次 1-4 全部改动）。
- `cargo test --no-default-features`：**1760 passed / 0 failed**。
- `docker build --target frontend-builder`（pnpm install + tsc + vite build）：通过。
- release.yml（public）：test 门禁 success → 4 个构建 job 全 success → 资产上传齐。
- linux 产物下载重算 SHA256 与仓库发布值一致；ELF 静态链接确认。
- nbus 部署后：/healthz 1.1.0、/v1/models 401、/admin 200、公网 api.dwgx.top 全通。
- 三批波次 + 全面 review 均过（对抗式，修复项已落地，见 CURRENT.md）。

## 波次 1-4 内容摘要（2026-08-13/14，全部经 review + CI）

1. **选号/性能**：token_manager 选号临界区优化（custom rpm 批量预取、health 读路径去写、
   report_success 族键缓存、计数器清零对称、refresh_lock 60s 超时）、模型级限流（model_hits
   维度 + 排序键 ⑥whitelist_hit ⑧model_calls_now，12 键）、count_tokens 缓存、SSE 预分配、
   图片 block_in_place、trace_db 批量写 + busy_timeout 5000 + interrupted_bytes。
2. **可靠/安全**：config 三写路径同锁（update_config/import_config/set_load_balancing_mode）、
   cooldown 持久化（kiro_cooldown.json + SystemTime + 停机 flush）、告警 webhook（alerting.rs，
   8 接入点，冷却去重 + 失败重试上限）、403 封禁识别（裸 suspended 词 + temporar 词族排除）、
   CSP 头 + adminKey sessionStorage、config 备份轮换 .bak×3、导出脱敏/导入掩码继承、
   超额自动禁用空 breakdown 门（limit>0）、websearch 预算耗尽部分结果收尾 + 回灌压缩重试。
3. **新能力**：成本核算（model_pricing 单价表 + usage by-model cost + 前端成本列）、
   socks 代理池自动健康调度（5min 探测/连续 3 次失败禁用）、诊断快照端点、OTA 自动检查
   （默认关）、版本伪装（version_mask 12h 拉 Kiro 版本；刷新接口 UA 刻意固定 config 值）、
   cli_ua amz-sdk-request max=3 对齐、KAM 导出（ids 非法 400 + no-store + region 用
   profileArn 推导）、/healthz、Dockerfile HEALTHCHECK 统一探 /v1/models。
4. **前端便捷**：接入信息页（conn tab：Anthropic/OpenAI 双卡 + curl/env 一键复制）、
   点击掩码即复制完整 Key（卡片/行）、设置页三开关（超额自动禁用/代理池调度/OTA 自动检查）、
   画布右键菜单、OTA 进度状态机、KAM 导出按钮、interrupted 展示。

## 已知遗留（当前代码里有 TODO / 半接线 / 未做）

- **P0 上号实测清单**（todo-2026-08-13.md 二-1）：native effort 触发、指纹命中率、error_message
  盲区闭合、websearch TTFB、401 判据、RPM 验证——需要可用 ksk 号后才能验证。
- **native_thinking_effort_enabled 决策**：默认关是刻意的，上号实测后决定开否。
- **/v1/responses 入站**：有人实现过（openai/handlers.rs post_responses 完整），
  确认现状即可（mod.rs「待补」注释过期待修）。
- **客户端 Key 分发 csk_**（P3-35）：未来分享场景，待决未做。
- **websearch 整轮缓冲改造**：设计文档已备（docs/websearch-buffering-rework.md），等上号实测。
- **告警 bump 点分散**：8 处无重复（冷却去重兜底），未来新增禁用/失败路径时注意统一收口。
- **upstream_trace DROPPED/WRITTEN 计数器无消费点**：确认 trace 排障功能是否还在用后再决定。
- **A-5/A-6 低危项（记录待复核，不修）**：429 换区后 403 绕圈最多浪费一跳；select_endpoint
  备区只取 PROBE_ORDER 第一项。
- **D 类阈值**：无真实故障分布数据，先修度量再调参（项目铁律）。

## 未验证、未做与阻塞

### 未验证
- nbus 上线后**功能回归**（部署只做了健康/端点验证，未打真实流量——需要可用号）。
- 前端视觉观感（tsc/build 通过，未做浏览器回归）。

### 未做（有意，有证据）
- 未动线上 config/credentials（v1.1.0 新字段全 serde default 兼容，零配置改动）。
- 未填 OTA token（update.env）；OTA 自动检查默认关。
- 未把代码推 public master（冻结纪律）。

### 需要 owner 决策
- OTA token 是否填（填后面板 OTA 可用；v1.1.0 tag 已打）。
- 是否开启 `native_thinking_effort_enabled`（默认关；建议上号实测后开）。
- 客户端 Key 分发 csk_ 是否立项（分享场景）。
- 面板需要真实号压测（QPS/429 风暴回归——v1.0.1 基线：QPS 1702 / P99 319ms / 0 击穿）。

## 下一步（仅 owner 批准后执行）

1. 上号后：跑 P0 实测清单 + 压测回归 + websearch TTFB，再决定开关。
2. 若要 OTA 可用：填 `KIROSTUDIO_UPDATE_TOKEN`。
3. 后续发版照此流程：波次实现 → 三批 review → CI（skiapi Docker）→ bump 版本 →
   push origin → tag + public release（ALLOW_PUBLIC_PUSH=1）→ nbus 部署验证。

## 历史档案入口

所有 `HANDOFF-*`、`HANDOFF-NEXT*`、`OPEN-ISSUES-*`、`STATUS-2026-08-05.md`、`PLAN-*`、
`TRACKING-*` 均保留为证据与推导材料。历史文档中的数字、线上状态、行号和「已上线/待修」
措辞必须先用本文件和当前代码重新核对。
