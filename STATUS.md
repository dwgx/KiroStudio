# KiroStudio status

Public Latest: **v1.2.4**（`d9dd93f`，2026-09-04）· [Release](https://github.com/dwgx/KiroStudio/releases/tag/v1.2.4) · [CHANGELOG](./CHANGELOG.md)

**2026-10-13 复活线（当前纪元）**：

- **master 已合流**：`3300f81` = repo-kit 工具（banner / regeneration workflow）+ v1.2.2–4 产品线。
  此前两条线自 `510e0dd` 分叉（master 只含 repo-kit 4 个提交；发布线 v1.2.2–4 只有 tag 无分支）。
- **HMC 活服务**：工作区 `kirostudio`（容器 8990 → `127.0.0.1:18001`），当前 **`dev-f94c9f7`**
  （`/healthz` `ok:true` · 模型表 32 · `pool_count=0`，等 Owner 载号）；回滚 =
  `KIROSTUDIO_VERSION=v1.2.4 bash ops/kirostudio/deploy.sh`。部署/升级 = `deploy.sh`（幂等）。
- **本轮已交付（2026-10-14 凌晨）**：**代码审计波 11 条高位修复全部落地并上线**——4 路并行子代理审计
  （安全 / 并发 / 流式 / 持久化）出 11 条发现，逐条一笔提交：匿名静态端点任意文件读 ·
  `countTokensApiUrl` 绕过 SSRF（对话原文外流）· 原子写回退截断空凭据 · 首字重开不换号 ·
  统计落盘竞态致健康号被误禁用 · 直播断连丢账 等；本机全量 **2355 passed / 0 failed**，CI 全绿。
  此前同线交付：模型表补 5.5 代/Fable 5.1（`9377b0a`）· 建议模式拦截 / 大写入分块注记 /
  SSE `x-accel-buffering` / idc 推断（`c61b296`）· `.rs` 强制 LF（`d18b835`）· 动态系统块分流
  （`ab7465c`）· **首字超时透明重试**（`46948e5`，默认 30s×2，`0`=关）· **P1-1 从官方 kiro-cli
  只读导入登录**（`f53d07f`，`POST /api/admin/import/kiro-cli`）· P1-6 额度恢复 · P1-4 发布链
  SHA pin · P2-4/11/12。总账：`.dwgx/ops/research-2026-10-13-kiro/dives/CONSOLIDATED-BACKLOG.md`
  （§8 = 审计波，含结论面与待决项）。
- **进行中**：open-code-review 全仓审计扫描（108/112 文件；落盘后按其严重度分诊、与子代理审计去重）。
- **未决（需 Owner 拍板）**：`trustPrivatePeerAsProxy` 默认 `true`（同网段可伪造
  `X-Forwarded-For` 绕过 ipAllowlist/ipBlocklist；属刻意的 LAN 兼容默认，改默认会打断依赖反代的部署）。
- **未接**：newapi 渠道（等 Owner 点名）；账号池空（等 Owner 载入）；skiapi 接入（Owner：以后再做）。

Do not retarget tags **v1.2.0** / **v1.1.2** / **v1.1.1** — OTA 比较 GitHub tags 与 `CARGO_PKG_VERSION`，二者必须一致。
Maintainer dirty-tree notes stay off this repository.
