# Public takeover

Latest tag: **v1.2.4**（`d9dd93f`，2026-09-04）。见仓根 [STATUS.md](../STATUS.md) 与 [CHANGELOG.md](../CHANGELOG.md)。

- master 已于 2026-10-13 合流（`3300f81`：repo-kit 工具 + v1.2.2–4 产品线），此前「本地脏 master 与公开 squash」的双线时代结束。
- 构建：`cargo test --no-default-features`（先 `cd admin-ui && pnpm install --frozen-lockfile && pnpm build`）。
- 出厂二进制：`--no-default-features`。
- 下一个 release = 新 tag，不 retarget 旧 tag。
- 部署与验证：见 `CLAUDE.md` 与 `STATUS.md`（HMC 工作区 `kirostudio`）。
