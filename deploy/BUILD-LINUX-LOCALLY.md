# 本机出 Linux 二进制（不依赖 GitHub Actions）

> 2026-08-07 实测建立。**换服务器/断网/Actions 不可用时的独立路径。**
> 本机是 macOS arm64，目标是 Linux x86_64（VPS）。

## 一条命令

```bash
cd <worktree>
export PATH="$HOME/.cargo/bin:$PATH"      # ⚠️ 必须，见下「坑 1」
cargo zigbuild --release --no-default-features --target x86_64-unknown-linux-musl
# 产物: target/x86_64-unknown-linux-musl/release/kirostudio
# 实测 3m03s（冷缓存），15.6MB
```

前置（已装好，`brew list` 可查）：`zig` 0.16.0 + `cargo-zigbuild`，
以及 rustup 的 `x86_64-unknown-linux-musl` target。

`cargo zigbuild` 用 zig 当 C 交叉链接器 ⇒ 绕开 `rusqlite`(bundled) 对
`x86_64-linux-musl-gcc` 的需求，**不需要 `musl-cross`**（那个不在 brew 默认 tap，
且要源码编译约 20 分钟）。

## 坑 1：`cargo` 默认指向 Homebrew 的 rust，它没有 musl std

```
which -a cargo  →  /opt/homebrew/bin/cargo      ← 默认命中这个
rustc --print sysroot → /opt/homebrew/Cellar/rust/1.97.1
```

`~/.zshrc` 里**没有** `.cargo/bin` 的 PATH 行，所以 Homebrew 版永远优先。
用它交叉编译会报：

```
error[E0463]: can't find crate for `core`
  = note: the `x86_64-unknown-linux-musl` target may not be installed
```

**这条报错是误导的** —— target 装了，装在 rustup 的 toolchain 下
（`~/.rustup/toolchains/stable-aarch64-apple-darwin/lib/rustlib/x86_64-unknown-linux-musl/`，
`libcore`/`libstd` 都在），只是 Homebrew 的 rustc 看不到它。
⇒ **交叉编译前必须 `export PATH="$HOME/.cargo/bin:$PATH"`。**

（本机测试/clippy 用哪个 cargo 都行，只有交叉编译有这个约束。）

## 坑 2：产物与 CI 的 sha 不同 —— 这是正常的，不是漂移

| 来源 | file 输出 | sha256 前 8 |
|---|---|---|
| CI（Actions, musl-tools） | `ELF ... **static-pie** linked, stripped` | `8d056859` |
| 本机（cargo zigbuild） | `ELF ... **statically** linked, stripped` | `61f2c91d` |

同一份源码、同一个 target，链接方式与工具链不同 ⇒ 字节不同。
**别拿 sha 去比对「本机构建 == CI 构建」，那永远不等。**
sha 的用途只有一个：**证明「上传的 == 本地编译的」**（同一个文件在两端一致）。

要证明「这个二进制含这次改动」，用内容断言：

```bash
deploy/has_literal.py <binary> "$(printf '%s' '你的中文字面量' | base64 | tr -d '\n')"
```

## 坑 3：`target/` 里可能躺着过期产物

实测踩过：`target/x86_64-unknown-linux-musl/release/kirostudio` 是 `205cc0bb`
（交接文档明写「已过期必须重建」的那个），差点被误推上线。

**编译后先看 mtime 与 sha，别假定 cargo 一定重建了。**
`cargo` 报 `Finished` 也可能是复用缓存 —— `verified-deploy.sh` 的
「1/5 断言本地二进制」正是为拦这个而存在（增量构建陷阱）。

## 完整部署流程（含内容断言）

```bash
cd <worktree>
export PATH="$HOME/.cargo/bin:$PATH"
cargo zigbuild --release --no-default-features --target x86_64-unknown-linux-musl

# 四门（在同一个 worktree 跑，用哪个 cargo 都行）
cargo test --no-default-features
cargo clippy --no-default-features --all-targets
cd admin-ui && npx tsc -b && \
  node --import ./tests/tsx-loader-register.mjs --test 'tests/*.test.ts' && cd ..
#   ⚠️ 前端测试必须带 --import ...tsx-loader-register.mjs，否则 fire-candidates
#      会因 .tsx 报 ERR_UNKNOWN_FILE_EXTENSION（假红）。测试文件头注释里的跑法已过期。
#   ⚠️ admin-ui/dist 必须存在（rust-embed 编译期嵌入），缺则 cargo 报 E0599。

deploy/verified-deploy.sh target/x86_64-unknown-linux-musl/release/kirostudio \
  --must '本次改动特有的中文字面量' \
  --must-not '被替换掉的旧文案'
```
