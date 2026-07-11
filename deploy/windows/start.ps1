# ============================================================================
# KiroStudio - Windows 引导式启动器
# ----------------------------------------------------------------------------
# 作用：检测配置 → 缺失/损坏则自动生成带强随机密钥的 config.json → 大字打印
#       密钥与面板地址 → 前台拉起网关。用户拿密钥登录 /admin 面板上号即可使用。
#
# 设计：本脚本是 exe 外的纯附加层，绝不修改 src/。上游怎么更新都零冲突。
# 用法：双击 deploy\windows\start.bat（推荐），或在项目根执行本脚本。
# 停止：Ctrl-C 或关闭窗口。
# ============================================================================

$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

# ---- 切到项目根目录（配置/数据相对工作目录解析，务必在根运行）----
$root = Resolve-Path (Join-Path $PSScriptRoot '..\..')
Set-Location $root

$Exe        = 'target\release\kirostudio.exe'
$ConfigPath = 'config.json'
$CredPath   = 'credentials.json'

# ---- 生成加密安全的随机密钥（非 Get-Random）----
function New-StrongKey {
    param([string]$Prefix, [int]$Bytes = 24)
    $buf = New-Object 'System.Byte[]' $Bytes
    [System.Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($buf)
    # base64url，去掉易混字符，拼前缀
    $s = [Convert]::ToBase64String($buf) -replace '[+/=]', ''
    return "$Prefix-$s"
}

# ---- 写纯文本文件，UTF-8 无 BOM（关键！）----
# PowerShell 5.1 的 Set-Content -Encoding UTF8 会写入 BOM(EF BB BF)，而后端
# serde_json 不接受 BOM 开头，会报 "expected value at line 1 column 1"。
# 因此配置/凭据文件一律用无 BOM 的 UTF8 写入。
function Write-NoBom {
    param([string]$Path, [string]$Content)
    $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText((Join-Path (Get-Location) $Path), $Content, $utf8NoBom)
}

# ---- 判断某密钥值是否有效（非空、非占位符）----
function Test-KeyValid {
    param($Value)
    if ([string]::IsNullOrWhiteSpace($Value)) { return $false }
    if ($Value -match 'CHANGE-ME|CHANGE_ME|your-|xxxx|placeholder') { return $false }
    return $true
}

# ---- 探测本机 TCP 端口是否已被占用（用于启动前给出友好提示）----
function Test-PortInUse {
    param([int]$Port)
    try {
        $conns = [System.Net.NetworkInformation.IPGlobalProperties]::GetIPGlobalProperties().GetActiveTcpListeners()
        return ($conns | Where-Object { $_.Port -eq $Port }).Count -gt 0
    } catch {
        return $false  # 探测失败不阻断启动，交给 exe 自己报
    }
}

# ---- 友好致命错误处理：打印原因 + 暂停，绝不让窗口静默闪退 ----
function Stop-WithError {
    param([string]$Message)
    Write-Host ''
    Write-Host '============================================================' -ForegroundColor Red
    Write-Host " [启动失败] $Message" -ForegroundColor Red
    Write-Host '============================================================' -ForegroundColor Red
    Write-Host ''
    Read-Host '按回车关闭本窗口'
    exit 1
}

Write-Host ''
Write-Host '============================================================' -ForegroundColor Cyan
Write-Host ' KiroStudio Windows 启动器' -ForegroundColor Cyan
Write-Host " 项目根目录: $root" -ForegroundColor DarkGray
Write-Host '============================================================' -ForegroundColor Cyan
Write-Host ''

# ---- 1) 检查 exe ----
if (-not (Test-Path $Exe)) {
    Write-Host "[错误] 未找到 $Exe" -ForegroundColor Red
    Write-Host '请先运行 deploy\windows\build.bat 完成编译。' -ForegroundColor Yellow
    Write-Host ''
    Read-Host '按回车退出'
    exit 1
}

# ---- 2) 配置自检 ----
$needGenerate = $false
$reason = ''

if (-not (Test-Path $ConfigPath)) {
    $needGenerate = $true
    $reason = 'config.json 不存在'
} else {
    # 尝试解析 + 校验两个密钥
    try {
        $existing = Get-Content $ConfigPath -Raw -Encoding UTF8 | ConvertFrom-Json
        if (-not (Test-KeyValid $existing.apiKey)) {
            $needGenerate = $true; $reason = 'config.json 的 apiKey 无效或为占位符'
        } elseif (-not (Test-KeyValid $existing.adminApiKey)) {
            $needGenerate = $true; $reason = 'config.json 的 adminApiKey 无效或为占位符'
        }
    } catch {
        $needGenerate = $true
        $reason = 'config.json 解析失败（可能损坏或含 JSON 不支持的注释）'
    }
}

$generatedKeys = $null

# ---- 3) 引导生成配置（任何文件操作失败都友好报错，绝不静默闪退）----
if ($needGenerate) {
    Write-Host "[引导] $reason，将自动生成配置。" -ForegroundColor Yellow
    try {
        # 备份已存在但无效的旧配置，绝不静默覆盖
        if (Test-Path $ConfigPath) {
            $bak = "$ConfigPath.bak.$(Get-Date -Format 'yyyyMMdd-HHmmss')"
            Copy-Item $ConfigPath $bak -Force
            Write-Host "  已备份旧配置到 $bak" -ForegroundColor DarkGray
        }

        $apiKey   = New-StrongKey 'sk-kiro'
        $adminKey = New-StrongKey 'sk-admin'

        # 纯 JSON 对象（无注释！后端 serde_json 不接受 // 注释）
        $cfg = [ordered]@{
            host              = '127.0.0.1'
            port              = 8990
            apiKey            = $apiKey
            adminApiKey       = $adminKey
            tlsBackend        = 'rustls'
            region            = 'us-east-1'
            defaultEndpoint   = 'ide'
            loadBalancingMode = 'priority'
            callbackBaseUrl   = ''
        }
        Write-NoBom $ConfigPath ($cfg | ConvertTo-Json -Depth 8)
        Write-Host "  已生成 $ConfigPath（host=127.0.0.1, port=8990）" -ForegroundColor Green

        $generatedKeys = @{ apiKey = $apiKey; adminKey = $adminKey }
    } catch {
        Stop-WithError ("生成配置失败：$($_.Exception.Message)`n" +
            "  常见原因：config.json 被其它程序占用（如已在运行的网关、编辑器），" +
            "或目录只读。请关闭占用程序后重试。")
    }
}

# ---- 4) 缺凭据文件则建空号池（启动后从面板上号）----
if (-not (Test-Path $CredPath)) {
    try {
        Write-NoBom $CredPath '[]'
        Write-Host "  已创建空号池 $CredPath（启动后请到 /admin 面板上号）" -ForegroundColor Green
    } catch {
        Stop-WithError "创建 credentials.json 失败：$($_.Exception.Message)"
    }
}

# ---- 5) 读取实际端口/host 用于打印地址 ----
try {
    $finalCfg = Get-Content $ConfigPath -Raw -Encoding UTF8 | ConvertFrom-Json
    $port = if ($finalCfg.port) { $finalCfg.port } else { 8990 }
    $host_ = if ($finalCfg.host) { $finalCfg.host } else { '127.0.0.1' }
} catch { $port = 8990; $host_ = '127.0.0.1' }
$displayHost = if ($host_ -eq '0.0.0.0') { '127.0.0.1' } else { $host_ }
$adminUrl = "http://${displayHost}:${port}/admin"

# ---- 6) 大字打印密钥与面板地址 ----
Write-Host ''
if ($generatedKeys) {
    Write-Host '################################################################' -ForegroundColor Green
    Write-Host '#                                                              #' -ForegroundColor Green
    Write-Host '#              已为你自动生成配置（请妥善保存）                #' -ForegroundColor Green
    Write-Host '#                                                              #' -ForegroundColor Green
    Write-Host '################################################################' -ForegroundColor Green
    Write-Host ''
    Write-Host '  面板登录密钥 (adminApiKey):' -ForegroundColor Yellow
    Write-Host "     $($generatedKeys.adminKey)" -ForegroundColor White
    Write-Host ''
    Write-Host '  网关调用密钥 (apiKey，给 Claude Code / SDK 用):' -ForegroundColor Yellow
    Write-Host "     $($generatedKeys.apiKey)" -ForegroundColor White
    Write-Host ''
    Write-Host '  这些密钥已写入 config.json，下次启动无需重设。' -ForegroundColor DarkGray
    Write-Host ''
}
Write-Host '  >> 管理面板地址（浏览器打开，用上面的 adminApiKey 登录）:' -ForegroundColor Cyan
Write-Host "     $adminUrl" -ForegroundColor White
Write-Host '  >> 登录后到「凭据 / 号池」页添加 Kiro 账号即可开始使用。' -ForegroundColor DarkGray
if ($host_ -eq '0.0.0.0') {
    Write-Host '  [提示] host=0.0.0.0：网关对局域网开放，请确保 apiKey 为强随机值。' -ForegroundColor Yellow
}
Write-Host ''
Write-Host '------------------------------------------------------------' -ForegroundColor DarkGray
Write-Host ' 启动网关中... 停止服务: Ctrl-C 或关闭本窗口' -ForegroundColor DarkGray
Write-Host '------------------------------------------------------------' -ForegroundColor DarkGray
Write-Host ''

# ---- 7) 启动前探测端口占用（给出比 exe panic 更友好的提示）----
if (Test-PortInUse $port) {
    Write-Host ''
    Write-Host "  [警告] 端口 $port 已被占用！" -ForegroundColor Yellow
    Write-Host '  可能是已有一个网关实例在运行，或其它程序占用了该端口。' -ForegroundColor Yellow
    Write-Host '  若网关随后报错退出，请改 config.json 的 "port" 为其它端口，或关闭占用程序。' -ForegroundColor Yellow
    Write-Host ''
}

# ---- 8) 前台拉起网关（监督循环，等价 systemd Restart=always）----
# 背景：面板「一键重启」和 OTA 更新的实现，都是让进程自退（exit 0），在 Linux 靠
# systemd `Restart=always` 拉起新进程。Windows 前台无守护进程，故这里由脚本充当守护。
#
# 如何区分「该重拉」与「该停服」（关键设计，靠退出方式天然区分，不依赖脆弱的信号捕获）：
#   - 面板一键重启 / OTA：exe 已长跑一段时间后干净自退 → 循环重拉，服务自动恢复。
#   - 用户按 Ctrl-C：OS 把 Ctrl-C 广播给 exe（触发其优雅停机）**同时**广播给本脚本，
#     PowerShell 默认会终止 -File 脚本 → 循环不再执行 → 停服。（保持与旧版单次启动一致的
#     Ctrl-C 语义。）为防极端情况下 PowerShell 仅中断 exe 却继续跑脚本，重拉前留一个
#     可被 Ctrl-C 打断的等待窗口作兜底。
#   - 关闭窗口：整个进程树（pwsh + exe）一起消失，循环自然终止 → 停服。
#
# 崩溃退避：exe 若在 $CrashWindowSec 秒内就退出，多半是配置错/端口占用（而非面板重启）。
# 连续短命达 $MaxCrashLoop 次则停止重拉并报错，避免坏配置下无限重启刷屏。
if (-not $env:RUST_LOG) { $env:RUST_LOG = 'info' }

# 判据用**退出码**（可靠），不用运行时长（会误判长跑后 Ctrl-C）：
#   exit 0   = 干净退出 = 面板一键重启 / OTA 自退 → 重拉恢复。
#              （用户 Ctrl-C 也让 exe 优雅退出 0，但 Ctrl-C 会同时终止本 PowerShell
#               脚本，循环根本走不到重拉——所以 Ctrl-C 天然停服，见下方兜底窗口。）
#   exit !=0 = 崩溃 / 配置错（exit 1）或 panic/端口占用（101）→ 退避重试，
#              连续 $MaxCrashLoop 次仍失败则放弃并报错，避免坏配置无限重启刷屏。
$MaxCrashLoop = 5
$crashStreak  = 0

while ($true) {
    & ".\$Exe"
    $code = $LASTEXITCODE

    if ($code -eq 0) {
        # 干净退出：面板「一键重启」/ OTA 触发的自退，重拉恢复。
        $crashStreak = 0
        Write-Host ''
        Write-Host "[KiroStudio] 网关已干净退出（退出码 0），2 秒后自动重启恢复…" -ForegroundColor Cyan
        Write-Host '  （由面板「一键重启」/ OTA 触发；想彻底停服请现在按 Ctrl-C，或关闭本窗口）' -ForegroundColor DarkGray
        # 2 秒可被 Ctrl-C 打断的窗口：既防 exit0 热循环，也给「此刻想停服」留 Ctrl-C 机会。
        Start-Sleep -Seconds 2
    } else {
        # 非零退出：崩溃 / 配置错 / 端口占用，退避重试，连续多次则放弃。
        $crashStreak++
        Write-Host ''
        Write-Host "[KiroStudio] 网关异常退出（退出码 $code），第 $crashStreak/$MaxCrashLoop 次。" -ForegroundColor Yellow
        if ($crashStreak -ge $MaxCrashLoop) {
            Write-Host '============================================================' -ForegroundColor Red
            Write-Host " [已停止重启] 连续 $MaxCrashLoop 次异常退出，多半是配置错误或端口被占用。" -ForegroundColor Red
            Write-Host ' 请检查上方日志（常见：端口被占用 / config.json 格式错 / apiKey 缺失）。' -ForegroundColor Red
            Write-Host '============================================================' -ForegroundColor Red
            break
        }
        $backoff = [Math]::Min(2 * $crashStreak, 10)
        Write-Host "  ${backoff}s 后重试…（想彻底停止请按 Ctrl-C 或关闭本窗口）" -ForegroundColor DarkGray
        Start-Sleep -Seconds $backoff
    }
}

Write-Host ''
Write-Host '[KiroStudio] 服务已停止。' -ForegroundColor DarkGray
Read-Host '按回车关闭本窗口'
