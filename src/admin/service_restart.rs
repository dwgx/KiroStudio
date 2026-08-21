//! 一键重启：Windows/macOS 进程自重启，Linux 交 systemd。
//!
//! 由 `service.rs` 以 `#[path]` 接入。`AdminService` 仍在父文件；本文件只持重启簇。
//! `spawn_windows_relaunch_process` 保持 `pub(crate)` 供托盘与 `admin/mod.rs` 再导出。

use std::path::Path;
#[cfg(target_os = "windows")]
use std::path::PathBuf;

use super::AdminServiceError;

impl super::AdminService {
    /// 一键重启本服务：Windows/macOS 下进程自重启（spawn detached 助手拉起新二进制）；
    /// 其余平台（Linux）优雅自退，由 systemd 自动重启。
    ///
    /// **Linux 实现方式：优雅自退，让 systemd 自动重启——不需要任何提权。**
    /// 根因（2026-07-08 定位）：systemd unit 设了 `NoNewPrivileges=true`，它会**永久禁止**
    /// 本进程及其子进程通过 setuid 提权，于是旧实现的 `sudo -n systemd-run ...` 静默失败
    /// （后台收到请求、打了日志，但 sudo 无法提权 → 什么都没发生 = "点了没反应"）。
    /// 由于 unit 配了 `Restart=always` + `RestartSec=3`，进程**只要退出**（任意退出码），
    /// systemd 就会在 3 秒内自动重新拉起。因此这里改为：延迟 1 秒（给 HTTP 200 flush 时间）
    /// 后 `std::process::exit(0)`，完全绕开 sudo/NoNewPrivileges，稳定可靠。
    /// 若将来 unit 去掉 Restart=always，此法失效——但当前部署（见 kirostudio.service）已配置。
    ///
    /// **macOS 没有 systemd**（2026-07-27 定位）：早期实现把"非 Windows"等同于"Linux+systemd"，
    /// macOS 下 `exit(0)` 后没有任何监督者会拉起新进程，一键重启/OTA 更新后服务直接消失、
    /// 端口不再监听。故 macOS 单独拆出一支，复用 Windows 同款思路自行 spawn 重启助手。
    pub fn restart_service(&self) -> Result<(), AdminServiceError> {
        // Windows：用户普遍**裸跑双击 exe**，无 systemd/监督脚本会在 exit(0) 后重拉。
        // 若直接 exit(0),服务就此消失(H1)。故 Windows 下改为**进程自重启**:spawn 一个 detached
        // helper(cmd),让它等本进程退出+端口释放后,用**原 exe 路径**(OTA 已把新二进制放到原路径)
        // 加相同的 --config/--credentials 参数、相同 cwd 重新拉起,再由本进程 exit(0)。
        #[cfg(target_os = "windows")]
        {
            self.spawn_windows_relaunch();
            tokio::spawn(async {
                // 睡 1 秒让本次 HTTP 200 flush 给前端,再退出让出端口,helper 会拉起新进程。
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                tracing::warn!("一键重启(Windows 裸跑):进程退出,已交给 detached helper 拉起新二进制");
                std::process::exit(0);
            });
            return Ok(());
        }

        // macOS：和 Windows 一样没有监督者会在 exit(0) 后自动拉起，且不像 Linux 有 systemd
        // 兜底——同样 spawn 一个 detached 助手，等端口释放后拉起新二进制，再自行退出。
        #[cfg(target_os = "macos")]
        {
            self.spawn_macos_relaunch();
            tokio::spawn(async {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                tracing::warn!("一键重启(macOS):进程退出,已交给 detached 助手拉起新二进制");
                std::process::exit(0);
            });
            return Ok(());
        }

        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        {
            tracing::warn!(
                "收到一键重启请求，约 1 秒后进程自退，由 systemd（Restart=always）在 3 秒内自动拉起"
            );
            // detached 异步任务：睡 1 秒让本次 HTTP 200 响应先 flush 给前端，再退出触发 systemd 重启。
            tokio::spawn(async {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                tracing::warn!("一键重启：进程即将退出，交由 systemd 自动拉起");
                std::process::exit(0);
            });
            Ok(())
        }
    }

    /// Windows 专用：写一个临时 `.bat`，让它等本进程退出+端口释放后重新拉起新二进制。
    ///
    /// 为什么用 .bat 而不是 `cmd /C "start ... "`：Rust `Command::args(["/C", line])` 会对
    /// 整串再加一层引号转义传给 cmd，叠加 `start "" "path"` 的多重引号 + `&`，cmd 解析错乱
    /// 会去找 `\\`（实测 bug:`Windows cannot find '\\'` + `Access is denied`）。批处理**文件**的
    /// 解析规则可预测,把带空格路径的引号写进文件即可,彻底绕开 `/C` 引号地狱。
    ///
    /// 为什么要中间脚本而非当前进程直接 spawn 新 exe：新旧进程抢同一监听端口,当前进程还没退出、
    /// 端口没释放,新 exe 会 bind 失败。脚本先 sleep 等旧进程退出+端口释放,再启动新 exe。
    #[cfg(target_os = "windows")]
    fn spawn_windows_relaunch(&self) {
        // 复用模块级自由函数（托盘「重启服务」项亦共用同一逻辑），传入启动时的
        // config/credentials 路径，让新进程用同一套路径重启。
        let config = self.token_manager.config();
        let config_path = config.config_path().map(|p| p.to_path_buf());
        let credentials_path = self.token_manager.credentials_path();
        spawn_windows_relaunch_process(config_path, credentials_path, &config.host, config.port);
    }

    /// macOS 专用：spawn 一个 detached shell 助手，sleep 后 exec 拉起新二进制。
    ///
    /// 不落地临时脚本文件（Windows 因 cmd `/C` 的多重引号转义问题才需要写 .bat，见
    /// [`spawn_windows_relaunch_process`] 注释）：POSIX shell 用位置参数 `"$0" "$@"` 接收
    /// exe 路径与参数，不做任何字符串拼接/转义，天然规避引号/注入问题。
    /// `trap '' HUP`：若用户是在 Terminal 前台直接跑的（而非 launchd/nohup），关终端触发的
    /// SIGHUP 不该连累刚 spawn、还在 sleep 的助手（及它 exec 顶替出的新进程）。
    /// 不重定向 stdio：助手与 exec 出的新进程沿用当前的 stdout/stderr（终端或已重定向的日志
    /// 文件），保持和重启前一致的日志去向。
    #[cfg(target_os = "macos")]
    fn spawn_macos_relaunch(&self) {
        use std::process::Command;

        // OTA 已把新二进制放到原 exe 路径（rename 旧→.bak、new→原路径）。current_exe 即目标。
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("macOS 自重启:取 current_exe 失败,无法拉起新进程: {e}");
                return;
            }
        };
        // 新进程的工作目录：沿用当前 cwd（config/credentials 相对路径解析依赖它）。
        let cwd = std::env::current_dir().ok();
        let config_path = self
            .token_manager
            .config()
            .config_path()
            .map(|p| p.to_path_buf());
        let credentials_path = self.token_manager.credentials_path();

        // sh -c 'script' 之后的第一个参数是 $0，其余是 $1.. ($@ 不含 $0)——
        // 故把 exe 路径放第一位，"$0" 取到的正是它，"$@" 取到的正是后续的 --config/--credentials。
        let mut args: Vec<std::ffi::OsString> = vec![exe.clone().into_os_string()];
        if let Some(cfg) = &config_path {
            args.push("--config".into());
            args.push(cfg.clone().into_os_string());
        }
        if let Some(cred) = &credentials_path {
            args.push("--credentials".into());
            args.push(cred.clone().into_os_string());
        }

        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(r#"trap '' HUP; sleep 3; exec "$0" "$@""#)
            .args(&args);
        if let Some(dir) = &cwd {
            cmd.current_dir(dir);
        }

        match cmd.spawn() {
            Ok(_) => tracing::warn!(
                "macOS 自重启:已 spawn 重启助手(sleep 3s 后拉起 {exe:?}),本进程退出后由它接管端口"
            ),
            Err(e) => tracing::error!(
                "macOS 自重启:spawn 重启助手失败,OTA/一键重启后服务可能不会自动恢复,请手动重启: {e}"
            ),
        }
    }
}

/// Windows 自重启探活 URL：通配绑定（0.0.0.0 / ::）改走回环，避免 bat 去连不可达地址。
pub(crate) fn windows_healthz_probe_url(host: &str, port: u16) -> String {
    let h = host.trim();
    let loopback = h.is_empty()
        || h == "0.0.0.0"
        || h == "::"
        || h == "[::]"
        || h.eq_ignore_ascii_case("localhost");
    if loopback {
        return format!("http://127.0.0.1:{port}/healthz");
    }
    if h.contains(':') && !h.starts_with('[') {
        return format!("http://[{h}]:{port}/healthz");
    }
    format!("http://{h}:{port}/healthz")
}

/// Windows 自重启 `.bat` 正文：等旧进程退出后拉起新二进制，循环探 `/healthz`，
/// 失败且存在 OTA `.bak` 时回滚旧版再拉起。全平台编译，供单测钉死文案。
pub(crate) fn windows_relaunch_bat(cwd_line: &str, launch: &str, exe: &str, healthz_url: &str) -> String {
    let bak = Path::new(exe)
        .with_extension("bak")
        .to_string_lossy()
        .into_owned();
    format!(
        "@echo off\r\n\
         chcp 65001 >nul\r\n\
         {cwd_line}\
         ping 127.0.0.1 -n 4 >nul\r\n\
         {launch}\r\n\
         REM probe /healthz ~30s; on fail rename .bak and relaunch\r\n\
         set \"KS_HEALTH={healthz_url}\"\r\n\
         set \"KS_EXE={exe}\"\r\n\
         set \"KS_BAK={bak}\"\r\n\
         set KS_PROBE=ps\r\n\
         where curl.exe >nul 2>nul\r\n\
         if not errorlevel 1 set KS_PROBE=curl\r\n\
         set /a KS_N=0\r\n\
         :ks_probe\r\n\
         set /a KS_N+=1\r\n\
         if %KS_N% GTR 30 goto :ks_fail\r\n\
         ping 127.0.0.1 -n 2 >nul\r\n\
         if \"%KS_PROBE%\"==\"curl\" (\r\n\
         curl.exe -fsS --max-time 1 \"%KS_HEALTH%\" >nul 2>nul\r\n\
         ) else (\r\n\
         powershell -NoProfile -Command \"try {{ Invoke-WebRequest -UseBasicParsing -TimeoutSec 1 -Uri $env:KS_HEALTH | Out-Null; exit 0 }} catch {{ exit 1 }}\"\r\n\
         )\r\n\
         if not errorlevel 1 goto :ks_ok\r\n\
         goto :ks_probe\r\n\
         :ks_fail\r\n\
         if exist \"%KS_BAK%\" (\r\n\
         for %%F in (\"%KS_EXE%\") do taskkill /F /IM \"%%~nxF\" >nul 2>nul\r\n\
         ping 127.0.0.1 -n 3 >nul\r\n\
         if exist \"%KS_EXE%\" move /Y \"%KS_EXE%\" \"%KS_EXE%.failed\" >nul\r\n\
         move /Y \"%KS_BAK%\" \"%KS_EXE%\" >nul\r\n\
         {launch}\r\n\
         )\r\n\
         :ks_ok\r\n\
         (goto) 2>nul & del \"%~f0\"\r\n"
    )
}

/// Windows 专用自由函数：写一个临时 `.bat`，让它等本进程退出+端口释放后重新拉起新二进制。
///
/// 从 [`AdminService::spawn_windows_relaunch`] 抽出为模块级函数，供**面板一键重启**与
/// **系统托盘「重启服务」**共用同一套久经验证的自重启逻辑（不依赖 `AdminService` 实例，
/// 托盘线程也能调）。`config_path` / `credentials_path` 由调用方传入（启动参数），让新进程
/// 用同一套路径。`listen_host` / `listen_port` 用于启动后探 `/healthz`；失败且存在 OTA
/// `.bak` 时回滚旧二进制。为何用 .bat + 中间脚本 + `CREATE_BREAKAWAY_FROM_JOB` 的完整原因见函数体注释。
#[cfg(target_os = "windows")]
pub(crate) fn spawn_windows_relaunch_process(
    config_path: Option<PathBuf>,
    credentials_path: Option<PathBuf>,
    listen_host: &str,
    listen_port: u16,
) {
    {
        use std::io::Write;
        use std::os::windows::process::CommandExt;
        use std::process::Command;

        // OTA 已把新二进制放到原 exe 路径（rename 旧→.bak、new→原路径）。current_exe 即目标。
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("Windows 自重启:取 current_exe 失败,无法拉起新进程: {e}");
                return;
            }
        };
        // 新进程的工作目录：沿用当前 cwd（config/credentials 相对路径解析依赖它）。
        let cwd = std::env::current_dir().ok();

        // 组装批处理里的 exe 调用行:每个含空格/特殊字符的路径用双引号包裹(bat 内引号规则简单可靠)。
        let q = |s: &str| format!("\"{}\"", s);
        let mut launch = format!("start \"KiroStudio\" {}", q(&exe.to_string_lossy()));
        if let Some(cfg) = &config_path {
            launch.push_str(&format!(" --config {}", q(&cfg.to_string_lossy())));
        }
        if let Some(cred) = &credentials_path {
            launch.push_str(&format!(" --credentials {}", q(&cred.to_string_lossy())));
        }

        // 批处理内容:等 ~3 秒(ping 当 sleep)→ 起新 exe → 循环探 /healthz →
        // 失败且存在 OTA .bak 则回滚旧版再拉起 → 删自身。
        // `start "标题" "exe" args` 让新 exe 独立于本 .bat 存活;`chcp 65001` 防中文路径乱码。
        let cwd_line = cwd
            .as_ref()
            .map(|d| format!("cd /d \"{}\"\r\n", d.to_string_lossy()))
            .unwrap_or_default();
        let health = windows_healthz_probe_url(listen_host, listen_port);
        let bat = windows_relaunch_bat(&cwd_line, &launch, &exe.to_string_lossy(), &health);

        // 写进临时目录的唯一 .bat。
        let bat_path = std::env::temp_dir()
            .join(format!("kirostudio-relaunch-{}.bat", uuid::Uuid::new_v4()));
        {
            let mut f = match std::fs::File::create(&bat_path) {
                Ok(f) => f,
                Err(e) => {
                    tracing::error!("Windows 自重启:创建重启脚本失败,请手动重启: {e}");
                    return;
                }
            };
            if let Err(e) = f.write_all(bat.as_bytes()) {
                tracing::error!("Windows 自重启:写重启脚本失败,请手动重启: {e}");
                return;
            }
        }

        // DETACHED_PROCESS(0x8) | CREATE_NEW_PROCESS_GROUP(0x200) | CREATE_NO_WINDOW(0x8000000)
        // + CREATE_BREAKAWAY_FROM_JOB(0x1000000):脱离父进程的 job object。
        // 【根因】若本进程被放进一个 job(如某些启动器/终端/服务包装把子进程装进 job,且 job 设了
        // KILL_ON_JOB_CLOSE),主进程 exit(0) 会**连带杀掉** detached 子进程 → 重启脚本还没 ping 完
        // 就被杀 → 新 exe 起不来(实测:Bash `&` 后台起的实例点重启即复现)。BREAKAWAY 让 cmd 脱离
        // 该 job,主进程退出不再牵连它。但 job 若禁止 breakaway,带此 flag 会 spawn 失败——故**先带
        // breakaway 尝试,失败再回退不带**(不在 job / 双击场景本就不需要,回退等价原行为)。
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        let base_flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW;

        let bat_str = bat_path.to_string_lossy().to_string();
        let spawn_with = |flags: u32| {
            let mut c = Command::new("cmd");
            c.args(["/C", &bat_str]).creation_flags(flags);
            if let Some(dir) = &cwd {
                c.current_dir(dir);
            }
            c.spawn()
        };
        // 先带 breakaway;失败(job 禁止 breakaway / 其它)则回退到原 flags。
        let result = spawn_with(base_flags | CREATE_BREAKAWAY_FROM_JOB)
            .or_else(|_| spawn_with(base_flags));
        match result {
            Ok(_) => tracing::warn!(
                "Windows 自重启:已 spawn 重启脚本({:?}),将在本进程退出后拉起 {exe:?}",
                bat_path
            ),
            Err(e) => tracing::error!(
                "Windows 自重启:spawn 重启脚本失败,OTA 后服务可能不会自动恢复,请手动重启: {e}"
            ),
        }
    }
}
