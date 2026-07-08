# 贡献指南

感谢你对 KiroStudio 的关注。无论是修复缺陷、完善文档还是提出新特性，都欢迎参与。请在提交前花几分钟阅读本指南，让协作更顺畅。

## 开发环境

- Rust（2024 edition，建议用 `rustup` 安装最新 stable）
- Node.js 20+ 与 pnpm 9+（前端）

前端产物 `admin-ui/dist` 会在编译期通过 `rust-embed` 嵌入二进制，所以改动前端后需要重新 `pnpm build` 才会体现在后端产物里。

```bash
# 后端
cargo run -- -c config/config.json --credentials config/credentials.json
cargo test
cargo fmt         # 提交前格式化
cargo clippy      # 静态检查

# 前端
cd admin-ui
pnpm install
pnpm dev          # 本地热更
pnpm build        # 产出 dist/
```

## 提交流程

1. Fork 本仓库到你自己的账号。
2. 从 `master` 切出功能分支，分支名建议语义化，如 `feat/credential-export`、`fix/stream-timeout`。
3. 完成改动，确保 `cargo fmt`、`cargo clippy`、`cargo test` 均通过。
4. 按下方规范提交 commit。
5. 推送分支并发起 Pull Request，填写 PR 模板说明改动内容、动机与测试情况。
6. 等待 CI 通过与 Review，按反馈调整后合并。

请勿直接向 `master` 推送。

## 提交信息规范

本项目采用 [Conventional Commits](https://www.conventionalcommits.org/) 约定式提交，格式如下：

```
<type>(<scope>): <描述>
```

- **type**：提交类型，见下表。
- **scope**：可选，影响范围，如 `scheduling`、`admin-ui`、`kiro`、`usage`、`docs`。
- **描述**：简明扼要说明本次改动，用中文，动词开头，句末不加句号。

| type | 用途 |
| --- | --- |
| `feat` | 新增功能 |
| `fix` | 修复缺陷 |
| `docs` | 文档改动 |
| `style` | 不影响逻辑的格式调整（空格、缩进、分号等） |
| `refactor` | 重构（既不修复缺陷也不新增功能） |
| `perf` | 性能优化 |
| `test` | 新增或修正测试 |
| `build` | 构建系统或依赖变更 |
| `ci` | CI 配置与脚本变更 |
| `chore` | 杂项（不影响源码与测试的维护性改动） |

示例：

```
feat(scheduling): 均衡模式新增每凭据 RPM 软限流
fix(stream): 修复流式响应缺失 thinking 签名
docs(readme): 补充 Docker 部署示例
refactor(kiro): 抽取协议转换公共逻辑
```

要求：

- 一次提交只做一件事，避免把无关改动混在一起。
- 禁止无意义的提交信息（如 `update`、`fix bug`、`修改`、`123`）。
- 破坏性变更在正文中以 `BREAKING CHANGE:` 段落说明。

## 代码风格

- Rust 代码提交前必须通过 `cargo fmt` 与 `cargo clippy`，不引入新的警告。
- 注释与文档以中文为主，保持清晰、必要即可。
- 新增功能或修复缺陷请附带相应测试；改动不应破坏现有测试。
- 遵循项目既有的模块划分与命名习惯，不随意引入新依赖；确需新增依赖时在 PR 中说明理由并使用固定版本。

## 报告问题

发现缺陷或有功能建议，请通过 [Issues](https://github.com/dwgx/KiroStudio/issues) 提交，并使用对应模板尽量提供复现步骤、环境信息与预期行为。安全类问题请勿公开披露，可通过私下渠道联系维护者。
