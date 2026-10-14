# KiroStudio status

Public Latest: **v1.2.5**（`4dee202`，2026-10-14；四端产物齐 + OTA 闭环 `has_update=false`）· [Release](https://github.com/dwgx/KiroStudio/releases/tag/v1.2.5) · [CHANGELOG](./CHANGELOG.md)

**2026-10-13 复活线（当前纪元）**：

- **master 已合流**：`3300f81` = repo-kit 工具（banner / regeneration workflow）+ v1.2.2–4 产品线。
  此前两条线自 `510e0dd` 分叉（master 只含 repo-kit 4 个提交；发布线 v1.2.2–4 只有 tag 无分支）。
- **HMC 活服务**：工作区 `kirostudio`（容器 8990 → `127.0.0.1:18001`），当前 **`v1.2.5`**（官方发布版，`4dee202`；此前 dev 版 `dev-a6a1ba8`）
  （`/healthz` `ok:true` · 模型表 32 · `pool_count=11`，其中 **8 个可用、3 个因上游账户级风控本地停用**）；
  回滚 = `KIROSTUDIO_VERSION=v1.2.4 bash ops/kirostudio/deploy.sh`。部署/升级 = `deploy.sh`（幂等）。
- **本轮已交付（2026-10-14，v1.2.5 窗口，109 笔提交）**：① **合流**（`3300f81`，repo-kit + v1.2.2–4
  发布线并入 master，唯一线、单 remote）；② **ocr 全仓审计**（`src/` 347 条分诊 112 修 / 41 deferred /
  124 误报 / 70 设计 + 覆盖缺口六片 + `admin-ui/` 33k 行首次覆盖 27 修）+ 多路独立验证收口；
  ③ **对手对照**（`ZyphrZero/kiro.rs` + `hj01857655/kiro-account-manager` + 146 仓更新扫描）——
  429 毫秒级限流头、根级 union 扁平化合并、无参工具收口、反饥饿、配额自动回池、等头超时 504 等；
  ④ **复活/启用路径统一收口**（四条运行时路径 + 第五条旧版迁移路径清三层惩罚）；
  ⑤ **P2**：模型「首次出现」日期自检、cc-switch 一键导入、sticky 晚解绑（同会话连续 429≥3）；
  ⑥ **观测面**：用量端点维度、隐藏上下文计量（只观测）、cache 真值可观测面、风控盲区修复、
  `messageMetadataEvent` 别名容忍；⑦ **号池 3 → 11**（ACFarm 产出导入 + 三个曾被误诊「token 作废」
  的号按正确 client 配对复活）。本机全量 **2639 passed / 0 failed**，CI 全绿（一次编译红被 CI 抓到后
  下一笔修复）。总账：`.dwgx/ops/research-2026-10-13-kiro/dives/CONSOLIDATED-BACKLOG.md`（§8–§21）。
- **已交付（2026-10-14 凌晨，同窗口早期）**：代码审计波 11 条高位修复上线——匿名静态端点任意文件读 ·
  `countTokensApiUrl` 绕过 SSRF · 原子写回退截断空凭据 · 首字重开不换号 · 统计落盘竞态致健康号被误禁用 ·
  直播断连丢账；另有模型表补 5.5 代/Fable 5.1（`9377b0a`）· 首字超时透明重试（`46948e5`）·
  P1-1 从官方 kiro-cli 只读导入登录（`f53d07f`）等。
- **未决（需 Owner 拍板）**：`trustPrivatePeerAsProxy` 默认 `true`（同网段可伪造
  `X-Forwarded-For` 绕过 ipAllowlist/ipBlocklist；属刻意的 LAN 兼容默认，改默认会打断依赖反代的部署）。
- **未接**：newapi 渠道（等 Owner 点名）；skiapi 接入（Owner：以后再做）；3 个被上游风控的号等解除后复启用。

Do not retarget tags **v1.2.0** / **v1.1.2** / **v1.1.1** — OTA 比较 GitHub tags 与 `CARGO_PKG_VERSION`，二者必须一致。
Maintainer dirty-tree notes stay off this repository.
