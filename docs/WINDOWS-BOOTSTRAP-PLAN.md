# Windows 引导式启动器 — 实施方案

## 目标
让 KiroStudio 在 Windows 上"开箱即跑"：用户双击一个脚本，若配置缺失/损坏，
启动器**自动检测并生成**带强随机密钥的 config.json，大字打印密钥与面板地址，
然后拉起网关。用户拿密钥登录**现成的 admin 面板**上号即可使用。

## 硬约束（已与用户确认）
- **绝对不改 `src/`**：主线业务代码由另一个 AI 维护，Windows 支持必须是纯附加层，
  只新增 `deploy/windows/` 下的脚本 + 文档。上游怎么更新都零冲突。
- 引导逻辑全部在 **exe 外的 PowerShell 启动器**里，exe 本身一行不改。

## 已验证的技术事实（决定方案可行性）
1. **零凭据可正常启动**：credentials 文件不存在/为空 → 后端返回空数组
   (`credentials.rs:289`)，`MultiTokenManager::new` 接受空列表。所以只要有一份带
   `apiKey`/`adminApiKey` 的有效 config，服务就能起来，用户从面板网页上号。
2. **唯一硬崩溃点**：config 缺 `apiKey` → `main.rs:104` `exit(1)`；缺 `adminApiKey`
   → 面板不挂载。凭据不是障碍。→ 启动器只需保证 config 两个密钥齐全即可。
3. **config.json 必须是纯 JSON 无注释**：后端用 `serde_json::from_str`
   (`config.rs:605`)，不接受 `//` 注释。`config.example.json` 是 JSONC 带注释，
   **不能直接拷贝**——启动器要生成无注释的纯 JSON。
4. **admin 面板即配置 UI**：改密钥、上号（social/idc/微软SSO）、所有开关都在
   `/admin` 里（login-dialog.tsx 已有完整上号向导）。无需自建向导页。
5. exe 已实测在 Windows 原生编译+运行成功（v0.6.2，前端内嵌，admin 200，鉴权 401）。

## 实现：`deploy/windows/start.ps1`（新增，不碰 src）
启动流程：
1. 切到项目根目录（配置/数据相对工作目录解析）。
2. 检查 `target\release\kirostudio.exe`，缺失则提示先跑 build.bat 并退出。
3. **配置自检 + 引导生成**：
   - 若 `config.json` 不存在 → 进入生成流程。
   - 若存在 → 尝试 `ConvertFrom-Json` 解析；解析失败（损坏/JSONC）或
     `apiKey`/`adminApiKey` 为空/占位符 → 也进入生成流程（生成前先备份旧文件为
     `config.json.bak.<时间戳>`，绝不静默覆盖用户数据）。
   - 生成流程：用 `System.Security.Cryptography.RandomNumberGenerator` 生成两个
     强随机密钥（`sk-kiro-<32位>` / `sk-admin-<32位>`），组装成**纯 JSON**对象
     （host=127.0.0.1、port=8990、tlsBackend=rustls、region=us-east-1、
     defaultEndpoint=ide、loadBalancingMode=priority、callbackBaseUrl=""），
     `ConvertTo-Json` 写入 config.json。
   - 缺 credentials.json 时写入 `[]`（空号池，启动后面板上号）。
4. **大字打印**（醒目边框）：adminApiKey、apiKey、面板地址
   `http://127.0.0.1:8990/admin`、上号提示。仅新生成时打印密钥；已有配置只打印地址。
5. 前台拉起 `kirostudio.exe`，日志实时显示。Ctrl-C / 关窗口停止。

## 配套文件（全部新增，不碰 src）
- `deploy/windows/start.ps1` — 引导式启动器（核心）
- `deploy/windows/start.bat` — 双击入口，内部调 PowerShell（绕过执行策略：
  `powershell -ExecutionPolicy Bypass -File start.ps1`）
- 复用已建的 `deploy/windows/build.bat`（构建 exe）
- 更新 `docs/DEPLOY-WINDOWS.md`：把"手动建 config"改为"双击 start.bat 自动引导"

## 安全考量
- 密钥用加密安全随机源（非 `Get-Random`）。
- 生成的 config.json 含明文密钥；Windows 无 0600，文档提示放非共享目录。
- 覆盖前必备份，绝不吞用户已有配置。
- host 默认 127.0.0.1（仅本机），避免无意对局域网裸奔。

## 不做（明确边界）
- 不改 src、不做 UI 内向导页、不动 OTA/一键重启（Windows 下本就不适用，文档已注明）。
- 不自动上号（凭据是敏感操作，交给面板里的现成上号流程）。
