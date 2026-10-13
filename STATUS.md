# KiroStudio status

Public Latest: **v1.2.4**（`d9dd93f`，2026-09-04）· [Release](https://github.com/dwgx/KiroStudio/releases/tag/v1.2.4) · [CHANGELOG](./CHANGELOG.md)

**2026-10-13 复活线（当前纪元）**：

- **master 已合流**：`3300f81` = repo-kit 工具（banner / regeneration workflow）+ v1.2.2–4 产品线。
  此前两条线自 `510e0dd` 分叉（master 只含 repo-kit 4 个提交；发布线 v1.2.2–4 只有 tag 无分支）。
- **HMC 活服务**：工作区 `kirostudio`（容器 8990 → `127.0.0.1:18001`，`/healthz` `ok:true`，v1.2.4），
  镜像 `dwgx/kirostudio:v1.2.4`；部署/升级 = `E:\dwgxhomecloud\ops\kirostudio\deploy.sh`（幂等）。
- **进行中（复活线波次）**：模型表补 5.5 代/Fable（P0-1）· 追赶修复移植（240s 写入防护、建议模式拦截）·
  open-code-review 审计 · 文档矫正（本波次）。
- **未接**：newapi 渠道（等 Owner 点名）；账号池空（等 Owner 载入）。

Do not retarget tags **v1.2.0** / **v1.1.2** / **v1.1.1** — OTA 比较 GitHub tags 与 `CARGO_PKG_VERSION`，二者必须一致。
Maintainer dirty-tree notes stay off this repository.
