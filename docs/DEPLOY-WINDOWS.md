# KiroStudio · Windows 本机部署指南

在 Windows 上把 KiroStudio 当作**本机常驻网关后台**运行。核心功能（转发到 Kiro
上游、admin 管理面板、凭据/号池管理、调度、限流、用量统计）与 Linux 服务器版
**完全一致**——同一份 Rust 代码，所有 Unix 专属逻辑（文件 0600 权限、SIGTERM
信号）在 Windows 下都有 `#[cfg(unix)]` 兼容分支，无需改动任何 `src/` 代码。

> **设计原则**：Windows 支持是**纯增量层**——只有 `deploy/windows/` 下的脚本、
> `.cargo/config.toml`（仅对 Windows 目标生效）和本文档，绝不修改上游核心代码。
> 因此上游 `dwgx/KiroStudio` 无论怎么更新，`git pull` 合并几乎不会冲突，Windows
> 版永远能跟上主线。

---

## 〇、适用范围 / 系统兼容性（重要）

**运行环境（跑编译好的 exe，最终用户）**：
| 环境 | 支持 | 说明 |
|------|------|------|
| Windows 10 / 11 **x64** | ✅ 原生 | 主要目标，双击即跑 |
| Windows 11 **ARM64**（骁龙笔记本等） | ✅ 模拟 | 靠系统 Prism 层模拟跑 x64，性能有损但可用 |
| Windows Server 2016+ | ✅ | 同 Win10 内核 |
| Windows **32 位**（旧 Win10 及更早） | ❌ | 本项目只出 x64。Win11 已无 32 位版，此为淘汰中的边缘场景 |
| Windows 7 / 8 | ⚠️ 不保证 | UCRT 非自带、TLS 栈差异，未测试 |

**零运行库依赖（已实测）**：编译时通过 `.cargo/config.toml` 的 `+crt-static` 把
C 运行时静态链接进 exe，产物**不依赖 `VCRUNTIME140.dll`**（即不需要用户预装
“Microsoft Visual C++ Redistributable”）。exe 仅依赖 Windows 10+ 自带的系统 DLL
（kernel32 / ntdll / ws2_32 / crypt32 / bcrypt 等），真正做到**任意 Win10+ x64
机器双击即跑**。这解决了 MSVC 默认动态链接下“换台干净电脑报 VCRUNTIME140.dll
缺失”的经典坑。

> 若你**不用** `.cargo/config.toml`（比如被上游合并冲突删了），默认 MSVC 构建的
> exe 会依赖 VCRUNTIME140.dll——分发到未装 VC++ 运行库的机器会打不开。务必保留
> 该文件，或让目标机器安装
> [VC++ Redistributable x64](https://aka.ms/vs/17/release/vc_redist.x64.exe)。

**构建环境（编译 exe，开发/打包者）**：需 Rust + Node（见下节），仅需一次。
最终用户拿到编译好的 exe 则无需任何工具链。

---

## 一、前置要求（一次性）

| 工具 | 用途 | 获取 |
|------|------|------|
| **Rust** (rustc/cargo) | 编译 exe，需 `x86_64-pc-windows-msvc` 目标（rustup 默认） | https://rustup.rs |
| **Node.js** (含 npm) | 构建前端 admin-ui（rust-embed 编译期嵌入 exe） | https://nodejs.org （建议 LTS 20+） |

pnpm 可选：构建脚本会自动探测，有 pnpm 用 pnpm，否则回退 npm。

验证工具链：
```bat
cargo --version
node --version
npm --version
```

---

## 二、从零跑起来（首次）

### 1. 编译
在项目根目录（`D:\Project\KiroStudio`）执行，或直接双击 `build.bat`：
```bat
deploy\windows\build.bat
```
它会：构建前端 `admin-ui\dist` → `cargo build --release` → 产出
`target\release\kirostudio.exe`（约 14MB，前端已打包进 exe，单文件即可运行）。

### 2. 双击启动（自动引导配置）
双击 `deploy\windows\start.bat`（推荐），或在根目录执行：
```bat
deploy\windows\start.bat
```
首次启动时，引导启动器会**自动检测并生成配置**——你**无需手动准备任何文件**：
- 若 `config.json` 不存在（或密钥无效/占位/文件损坏），自动生成一份带**强随机
  密钥**的 config.json（`host=127.0.0.1`、`port=8990`）。覆盖前会先备份旧文件为
  `config.json.bak.<时间戳>`，绝不静默吞掉你已有的配置。
- 若 `credentials.json` 不存在，自动建一个空号池 `[]`（启动后从面板上号）。
- 控制台会用**大字边框**打印生成的 `adminApiKey` / `apiKey` 和面板地址，请复制保存。

第二次起，若配置已有效，启动器不会重新生成、不打印密钥，直接拉起网关（幂等）。

看到 `启动 Anthropic API 端点: 127.0.0.1:8990` 即成功。

> 说明：引导逻辑完全在 `deploy\windows\start.ps1`（PowerShell 启动器）里，**不改
> 任何 `src/` 代码**——它只是在启动 exe 前把配置补齐。上游怎么更新都零冲突。

### 3. 访问
- 管理面板：`http://127.0.0.1:8990/admin`（用打印出的 `adminApiKey` 登录）
- 登录后到「凭据 / 号池」页添加 Kiro 账号（social / IDC / 微软 SSO），即可开始使用。
- 网关地址（给 Claude Code / SDK 用）：`http://127.0.0.1:8990`，请求头带
  `x-api-key: <你的 apiKey>` 或 `Authorization: Bearer <你的 apiKey>`

端口以 config.json 的 `port` 为准（默认 8990）。

> **三个脚本的区别**：
> - `start.bat`（推荐）：**引导式**，自动检测/生成配置 + 打印密钥 + 监督循环启动。适合首次或换机。
> - `run.bat`：**纯启动**，不碰配置，要求你已自备 config.json + 监督循环启动。适合已配置好的日常启动。
> - `update.bat`：**跟随上游更新**，git pull + 重建前端和 exe。见"三、日常运维"。
>
> start.bat 与 run.bat 都内置**监督循环**，让面板「一键重启」/ 崩溃自愈在 Windows 生效。

### 手动准备配置（可选，不想用引导时）
若你想自己写配置：复制 `config.example.json` 为 `config.json`，改 `apiKey` 和
`adminApiKey` 为强随机值。⚠️ **两个坑**：① `config.example.json` 带 `//` 注释，
后端不接受，删掉所有注释行改成纯 JSON；② 保存时必须是 **UTF-8 无 BOM**（记事本
"另存为"选 UTF-8 可能带 BOM 导致后端报 `expected value at line 1 column 1`，
建议用 VSCode 选 "UTF-8" 而非 "UTF-8 with BOM"）。用 `start.bat` 引导则无此问题。

---

## 三、日常运维（手动，不开机自启）

### 停止
- **彻底停服**：关闭 start.bat / run.bat 的日志窗口，或在窗口内按 `Ctrl-C`
  （`run.bat` 会问 `Terminate batch job (Y/N)?`，按 `Y`）。exe 收到 Ctrl-C 会
  触发优雅停机（等在途请求 drain）。

### 重启
两种方式都可用：
- **面板「一键重启」（已支持！）**：start.bat / run.bat 现在内置**监督循环**
  （等价 Linux systemd `Restart=always`）——网关进程干净自退（exit 0）后，
  脚本会自动重新拉起。所以 admin 面板设置页的「一键重启服务」按钮在 Windows
  下**能正常工作**了：点它 → 进程自退 → 脚本 2 秒后重拉，服务自动恢复。
- **手动重启**：关闭窗口 → 重新双击 start.bat / run.bat。

> **监督循环如何区分「重拉」与「停服」**：网关长跑后干净自退（面板重启/OTA）→
> 重拉；用户 Ctrl-C / 关窗口 → 停服不重拉；若网关启动后 10 秒内就退出（多半是
> 配置错/端口占用），脚本会退避重试，连续 5 次仍失败则停止并报错，不会无限刷屏。

### 更新 / 修复（跟随上游）
**推荐：双击 `deploy\windows\update.bat`**（一步到位）：
```bat
deploy\windows\update.bat         REM 检查干净工作树 → git pull → 重建前端+exe
```
它会：确认无未提交改动（有则提示先 commit/stash，绝不丢你的改动）→ 检测网关
是否在运行（在运行则提示先停，因为 Windows 会锁定运行中的 .exe 导致重编覆盖失败）
→ `git pull --ff-only` → 调 `build.bat` 重建。完成后按上面「重启」流程重开窗口
加载新 exe。

> **为什么 update.bat 比面板 OTA 覆盖更全**：`git pull` 能拿到 master 上的**全部
> 最新改动**，而面板 OTA 只能升级到恰好打了 GitHub Release 的 tag 版本。

也可手动分步：
```bat
git pull
deploy\windows\build.bat
```
因为 Windows 支持是纯增量层，`git pull` 通常不会与 `deploy/windows/` 冲突。

> ⚠️ **admin 面板里的"OTA 在线更新"按钮在 Windows 上不可用**：它下载的是
> Linux musl 二进制（`kirostudio-linux-x86_64`，ELF 格式 Windows 跑不了），且
> 依赖 Linux「可 rename 运行中 exe」的特性（Windows 会锁定运行中的 .exe）。
> Windows 请一律用上面的 `update.bat`（或 `git pull` + `build.bat`）更新。
> （注：面板的**一键重启**已可用，只有 **OTA 换二进制**这一步在 Windows 不适用。）

### 查看日志
前台模式日志直接在窗口里。如需留存，可在启动时重定向：
```bat
target\release\kirostudio.exe > kirostudio.log 2>&1
```
或调日志级别：窗口启动前设 `set RUST_LOG=debug`（run.bat 默认 info）。

---

## 四、Windows 与 Linux 服务器版的差异

| 项 | Linux 服务器 | Windows 本机 |
|----|-------------|-------------|
| 核心网关功能 | ✅ | ✅ 完全一致 |
| admin 面板 / 号池 / 调度 / 限流 | ✅ | ✅ 完全一致 |
| 敏感文件权限 | 0600（属主可读写） | 依赖 NTFS ACL（`#[cfg(unix)]` 分支跳过，建议放非共享目录） |
| 优雅停机信号 | Ctrl-C + SIGTERM | 仅 Ctrl-C（关窗口/停进程） |
| 进程守护 / 自动重启 | systemd `Restart=always` | ✅ start.bat/run.bat 内置监督循环（等价） |
| admin "一键重启"按钮 | ✅ 靠 systemd 拉起 | ✅ 靠脚本监督循环拉起 |
| admin "OTA 在线更新"按钮 | ✅ 换 musl 二进制 | ❌ 不适用，用 `update.bat`（git pull + 重编） |
| 开机自启 | systemd enable | 不需要（按需手动启动） |

**结论**：作为本机网关，功能零缺失。唯一 Windows 不适用的是**面板 OTA 换二进制**
（Linux musl 格式 + rename 运行中 exe，Windows 不成立），已由 `update.bat`（git
pull + 重编）等价替代且覆盖更全。**一键重启、崩溃自愈已靠脚本监督循环支持**，
开机自启按你要求不做。

---

## 五、常见问题

**Q：窗口一闪就没了？**
多半是 config.json 缺失或格式错误、或端口被占用。run.bat 末尾有 `pause`，
正常会停住显示错误。若确实闪退，在命令行手动跑 `target\release\kirostudio.exe`
看报错。

**Q：端口 8990 被占用？**
改 config.json 的 `port` 为其他端口（如 8991），重启即可。

**Q：局域网其他设备连不上？**
把 config.json 的 `host` 改为 `0.0.0.0`，并放行 Windows 防火墙对应端口。
注意此时网关对局域网开放，务必保证 `apiKey` 是强随机值。

**Q：编译报错找不到 admin-ui\dist？**
先跑 `build.bat`（它会先构建前端再编译）。不要直接 `cargo build`——
rust-embed 需要 `admin-ui\dist\index.html` 在编译期存在。

