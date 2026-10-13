# KiroStudio status

Public Latest: **v1.2.4**（`d9dd93f`，2026-09-04）· [Release](https://github.com/dwgx/KiroStudio/releases/tag/v1.2.4) · [CHANGELOG](./CHANGELOG.md)

**2026-10-13 复活线（当前纪元）**：

- **master 已合流**：`3300f81` = repo-kit 工具（banner / regeneration workflow）+ v1.2.2–4 产品线。
  此前两条线自 `510e0dd` 分叉（master 只含 repo-kit 4 个提交；发布线 v1.2.2–4 只有 tag 无分支）。
- **HMC 活服务**：工作区 `kirostudio`（容器 8990 → `127.0.0.1:18001`，`/healthz` `ok:true`，v1.2.4），
  镜像 `dwgx/kirostudio:v1.2.4`；部署/升级 = `E:\dwgxhomecloud\ops\kirostudio\deploy.sh`（幂等）。
- **本轮已交付（2026-10-13 深夜）**：模型表补 5.5 代/Fable 5.1（`9377b0a`）；追赶修复移植——
  建议模式拦截 / 大写入分块注记 / SSE `x-accel-buffering` / idc 字段推断（`c61b296`）；
  `.rs` 强制 LF 检出，修本机 8 个源码扫描守卫测试全红（`d18b835`）。本机全量测试
  **2307 passed / 0 failed**，三笔已推 master。
- **进行中**：open-code-review 全仓审计（扫描进行中，分诊待做）· 7 路深度研究（对手深挖 ×5 +
  官方 kirocli/kirodesktop 逆向 ×2，报告落 `.dwgx/ops/research-2026-10-13-kiro/dives/`）·
  带新功能的构建上 hmc（dev 部署，等 CI 绿）。
- **未接**：newapi 渠道（等 Owner 点名）；账号池空（等 Owner 载入）。

Do not retarget tags **v1.2.0** / **v1.1.2** / **v1.1.1** — OTA 比较 GitHub tags 与 `CARGO_PKG_VERSION`，二者必须一致。
Maintainer dirty-tree notes stay off this repository.
