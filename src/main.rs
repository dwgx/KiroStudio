mod admin;
mod admin_ui;

/// /help 帮助中心入口（2026-08-14）：对齐 index_handler 的 async + IntoResponse 包装
/// （axum 0.8 对返回裸 Response<Body> 的 fn item 不直接实现 Handler）。
async fn help_page_handler() -> impl axum::response::IntoResponse {
    admin_ui::serve_help_page()
}
mod anthropic;
mod common;
mod http_client;
mod kiro;
mod model;
mod openai;
pub mod token;
#[cfg(windows)]
mod tray;
mod usage;

use std::collections::HashMap;
use std::sync::Arc;

use clap::Parser;
use kiro::endpoint::KiroEndpoint;
use kiro::model::credentials::{CredentialsConfig, KiroCredentials};
use kiro::provider::KiroProvider;
use kiro::token_manager::MultiTokenManager;
use model::arg::Args;
use model::config::Config;
use usage::{TraceDb, UsageStats};

/// admin 查询侧共享的用量 sink 句柄
#[derive(Clone)]
pub struct UsageHandles {
    pub stats: Arc<UsageStats>,
    pub trace_db: Arc<TraceDb>,
}

/// 生成一个加密安全的随机密钥：`<prefix>-<base64url(32B)>`。
///
/// 用 4 个 UUID v4（各 122 bit 熵，getrandom 后端）拼成 32 字节再 base64url，去掉易混字符。
/// 不引新依赖（uuid 已在用），熵足够做 apiKey / adminApiKey。
fn generate_strong_key(prefix: &str) -> String {
    use base64::Engine;
    let mut bytes = Vec::with_capacity(64);
    for _ in 0..4 {
        bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    }
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes[..24]);
    let cleaned: String = b64.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    format!("{prefix}-{}", &cleaned[..cleaned.len().min(32)])
}

/// Windows 数据隔离根目录：`<exe 同目录>/KiroStudio-data/`。
///
/// 双击 exe 时 cwd 不可控（常是桌面/system32），产物会散落。故把 config.json / credentials.json /
/// trash.json / 用量库统一收进 exe 同目录下一个 `KiroStudio-data/` 文件夹，与 Linux 部署隔离。
/// 仅 Windows 生效；非 Windows 返回 None（走原 cwd/exe 逻辑，systemd 部署用显式路径不受影响）。
/// 不存在则创建；创建失败返回 None（优雅降级到原逻辑，不阻断启动）。
#[cfg(windows)]
fn windows_data_root() -> Option<std::path::PathBuf> {
    let exe_dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let root = exe_dir.join("KiroStudio-data");
    if let Err(e) = std::fs::create_dir_all(&root) {
        tracing::warn!("创建数据目录 {} 失败: {}，回退到默认路径", root.display(), e);
        return None;
    }
    Some(root)
}

/// 解析「默认名」文件的实际落盘路径，兼顾 Windows 数据隔离 + 旧位置兼容 + 源码目录开发。
///
/// 仅当传入是默认名（未显式指定路径）时才重定向；显式路径原样尊重。查找/落盘优先级：
/// 1. cwd 下已有（源码目录开发场景）→ 沿用 cwd，不搬。
/// 2. exe 同目录已有（旧版本落这里的存量配置）→ 沿用，**不强制迁移到 data 目录，避免丢号**。
/// 3. Windows 且能建 data 根 → `<exe>/KiroStudio-data/<name>`（新的隔离位置）。
/// 4. 兜底：exe 同目录（非 Windows 或建 data 失败）。
fn resolve_default_data_path(name: &str) -> std::path::PathBuf {
    use std::path::Path;
    let cwd_path = Path::new(name).to_path_buf();
    if cwd_path.exists() {
        return cwd_path; // 源码目录开发：cwd 已有则沿用
    }
    let exe_dir = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.to_path_buf()));
    if let Some(dir) = &exe_dir {
        let legacy = dir.join(name);
        if legacy.exists() {
            return legacy; // 旧版本落 exe 根目录的存量配置：沿用，不搬（防丢号）
        }
    }
    #[cfg(windows)]
    {
        if let Some(root) = windows_data_root() {
            let in_data = root.join(name);
            // data 目录里已有 → 用它；没有 → 也用它作为新的落盘位置（隔离）。
            return in_data;
        }
    }
    // 非 Windows 或 data 根不可用：回退 exe 同目录（保持原防呆语义）。
    exe_dir.map(|d| d.join(name)).unwrap_or(cwd_path)
}

/// 首次启动自动打开浏览器到 /admin（仅 Windows）。
///
/// 触发条件（全满足）：①本次 bootstrap 新生成了 config（首次运行）②host 是本地回环
/// （127.0.0.1/localhost/::1，避免服务器/公网监听场景乱开）③未设 `KIRO_NO_BROWSER` 环境变量
/// （自动化/测试可关）。用 detached `cmd /C start` 开系统默认浏览器，免新依赖、不阻塞。
#[cfg(windows)]
fn maybe_open_browser_on_first_run(freshly_generated: bool, host: &str, port: u16) {
    if !freshly_generated {
        return;
    }
    if std::env::var("KIRO_NO_BROWSER").map(|v| !v.is_empty()).unwrap_or(false) {
        return;
    }
    let is_loopback = matches!(host, "127.0.0.1" | "localhost" | "::1" | "0.0.0.0");
    // host 为 0.0.0.0（监听所有网卡）时用 127.0.0.1 打开本机面板。
    let browse_host = if host == "0.0.0.0" { "127.0.0.1" } else { host };
    if !is_loopback {
        return;
    }
    let url = format!("http://{}:{}/admin", browse_host, port);
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // `start "" "<url>"`：空标题占位 + URL。用 .bat 无关，单条 start 命令引号简单可靠。
    let mut c = std::process::Command::new("cmd");
    c.args(["/C", "start", "", &url])
        .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW);
    match c.spawn() {
        Ok(_) => tracing::info!("首次启动：已尝试打开浏览器 {}", url),
        Err(e) => tracing::warn!("首次启动打开浏览器失败（不影响服务）: {}", e),
    }
}

/// 非 Windows：不自动开浏览器（服务器部署无 GUI）。
#[cfg(not(windows))]
fn maybe_open_browser_on_first_run(_freshly_generated: bool, _host: &str, _port: u16) {}

/// 解析用量库目录：默认相对值 `"data/usage"` 在 Windows 下前缀到 `KiroStudio-data/`（数据隔离）；
/// 已被用户改成绝对路径或自定义相对值的，原样尊重（不劫持用户显式配置）。
/// 非 Windows / 数据根不可用：原样返回（保持相对 cwd 语义）。
fn resolve_usage_data_dir(configured: &str) -> std::path::PathBuf {
    let p = std::path::PathBuf::from(configured);
    let is_default = configured == "data/usage";
    if !is_default || p.is_absolute() {
        return p;
    }
    #[cfg(windows)]
    {
        if let Some(root) = windows_data_root() {
            return root.join(configured);
        }
    }
    p
}

/// 防呆引导：`config_path` 指向的配置文件不存在时，自动生成一份带强随机密钥的最小 config.json，
/// 并大字打印 adminApiKey / apiKey / 面板地址。已存在则不做任何事（绝不覆盖用户配置）。
///
/// 返回 `(实际配置路径, 是否本次新生成)`。新生成标志供启动后「仅首次自动开浏览器」判断。
/// 路径解析：默认名走 [`resolve_default_data_path`]（Windows 数据隔离 + 旧位置兼容）；
/// 显式 `--config` 指定的路径原样尊重。
fn bootstrap_config_if_missing(config_path: &str) -> (String, bool) {
    use std::path::Path;
    let resolved = if config_path == Config::default_config_path() {
        resolve_default_data_path(config_path)
    } else {
        Path::new(config_path).to_path_buf()
    };
    let resolved_str = resolved.to_string_lossy().to_string();
    if resolved.exists() {
        return (resolved_str, false); // 已有配置，尊重用户，不碰；非首次
    }
    let target = resolved;

    let api_key = generate_strong_key("sk-kiro");
    let admin_key = generate_strong_key("sk-admin");
    // 最小可运行 config：host/port + 两把密钥 + rustls。其余字段走 serde default。
    let cfg = serde_json::json!({
        "host": "127.0.0.1",
        "port": 8990,
        "apiKey": api_key,
        "adminApiKey": admin_key,
        "tlsBackend": "rustls",
        "region": "us-east-1",
        "defaultEndpoint": "ide",
    });
    let body = serde_json::to_string_pretty(&cfg).unwrap_or_default();
    if let Err(e) = std::fs::write(&target, body) {
        // 写失败不阻断：继续走原流程（大概率随后因缺 apiKey 退出并报错），但先告知原因。
        tracing::error!("[引导] 自动生成配置失败({}): {e}；请手动创建 config.json 或用 start.bat", target.display());
        return (resolved_str, false);
    }
    // Unix 收紧权限（含密钥，仅属主可读写）；Windows 依赖 NTFS ACL，此调用 no-op。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600));
    }

    // 大字横幅打印密钥 + 面板地址（用户据此登录 /admin 上号）。用 println! 确保裸双击也能看到。
    println!("\n############################################################");
    println!("#  KiroStudio 首次启动：已自动生成配置（请妥善保存密钥）  #");
    println!("############################################################");
    println!("  配置文件:  {}", target.display());
    println!("  面板密钥 (adminApiKey，登录 /admin 用):");
    println!("     {admin_key}");
    println!("  网关密钥 (apiKey，给 Claude Code / SDK 用):");
    println!("     {api_key}");
    println!("  管理面板:  http://127.0.0.1:8990/admin");
    println!("  登录后到「凭据/号池」页添加 Kiro 账号即可开始使用。");
    println!("############################################################\n");
    tracing::info!("[引导] 已自动生成 {}（首次启动）", target.display());
    (resolved_str, true)
}

// ==================== B7 启动播种集中校验 ====================
// 校验语义：断言「播种执行过」而非「值非空」——error_messages 空表合法、mock_cache
// 默认关合法，用值无法区分「配置没设」与「setter 没被调」。handlers.rs 侧 17 个
// 镜像由 setter 内部置位（unwired_mirrors 汇总）；本表覆盖 main 直接播种、setter
// 在别的模块的 4 个镜像（converter/upstream_trace/token/admin_ui），播种点后显式置位。

/// main 侧播种点位图：第 i 位 = [`MAIN_SEEDED_NAMES`][i] 对应镜像已播种。
static MAIN_SEEDED_BITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// main 侧播种点名字表：新增 main 直接播种的镜像时必须在此登记（漏登记 mark 时 panic 暴露）。
const MAIN_SEEDED_NAMES: [&str; 5] = [
    "login_background_r18",
    "upstream_trace",
    "count_tokens",
    "native_thinking_effort",
    "tool_compat_mapping",
];
const _: () = assert!(MAIN_SEEDED_NAMES.len() <= 64);

/// main 直播种点调用后登记（setter 在其他模块、无法在 setter 内部置位的镜像）。
fn mark_main_seeded(name: &str) {
    let Some(idx) = MAIN_SEEDED_NAMES.iter().position(|n| *n == name) else {
        panic!("main 播种点 {name} 未登记进 MAIN_SEEDED_NAMES");
    };
    MAIN_SEEDED_BITS.fetch_or(1u64 << idx, std::sync::atomic::Ordering::Relaxed);
}

/// 依据播种位图计算缺失的 main 侧镜像名（纯函数，供 B7 告警联动测试）。
fn main_mirrors_missing(bits: u64) -> Vec<&'static str> {
    MAIN_SEEDED_NAMES
        .iter()
        .enumerate()
        .filter(|(i, _)| bits & (1u64 << i) == 0)
        .map(|(_, n)| *n)
        .collect()
}

/// B7 启动播种集中校验：必须在**全部播种点之后**、serve 之前调用。
///
/// 汇总 handlers 侧（setter 内部置位）+ main 侧（调用方置位）两路标记，缺失即
/// warn 醒目告警——**只告警不阻塞**：配置静默不生效比启动失败好诊断，但必须一眼看见。
///
/// 边界（诚实披露）：token_manager 内部镜像（cooldown/rate_limit/affinity/rpm 等）
/// 由 `MultiTokenManager::new` 无条件从 config 播种、失败即 exit(1)，天然保证执行过，
/// 不在本校验范围（setter 在 token_manager.rs，不在本次改动权限内）。
fn verify_runtime_mirrors_wired() {
    let mut missing: Vec<&'static str> = crate::anthropic::handlers::unwired_mirrors();
    let bits = MAIN_SEEDED_BITS.load(std::sync::atomic::Ordering::Relaxed);
    missing.extend(main_mirrors_missing(bits));
    if missing.is_empty() {
        let total = crate::anthropic::handlers::mirror_wired_count() + MAIN_SEEDED_NAMES.len();
        tracing::info!("启动播种自检通过：{} 个进程镜像全部接线", total);
        return;
    }
    // F6/D3-2（scheduling-audit-research）：接线缺失只 warn 的话 webhook 无感知——
    // 「面板改了没反应」要等运维翻日志才发现。直报 bump，reason 携带缺失清单摘要
    // （详细逐条仍在下方日志）；冷却窗口内幂等，未配置 webhook 时零开销 no-op。
    crate::common::alerting::bump_with_dynamic_reason(
        "wiring_incomplete",
        missing.join(", "),
    );
    for m in &missing {
        tracing::warn!(
            "【启动播种缺失】镜像 [{m}] 未被播种：相关配置将静默不生效（面板改了没反应），\
             请检查 main.rs / handlers.rs 的启动接线（B7）"
        );
    }
    tracing::warn!(
        "【启动播种自检】{} 个镜像缺失，服务仍启动；修复前不要依赖这些配置项",
        missing.len()
    );
}

#[tokio::main]
async fn main() {
    // 解析命令行参数
    let args = Args::parse();

    // 初始化日志。两层 filter 独立:
    // - fmt 层(终端/文件):由 RUST_LOG 环境变量控制(默认 info)——生产 unit 写 RUST_LOG=warn,
    //   终端保持精简不刷屏;
    // - LogBufferLayer(面板实时日志流/一键导出,见 common::log_buffer):固定 INFO——
    //   面板排障永远可见 info+ 进度(选号/转发/恢复),不随控制台 filter 一起被压掉。
    {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        use tracing_subscriber::Layer as _;
        let console_filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
        let panel_filter = tracing_subscriber::EnvFilter::new("info");
        tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer().with_filter(console_filter))
            .with(crate::common::log_buffer::LogBufferLayer.with_filter(panel_filter))
            .init();
    }

    // 加载配置
    let config_path = args
        .config
        .unwrap_or_else(|| Config::default_config_path().to_string());

    // 防呆引导（Windows 裸双击 exe 的核心体验）：config 缺失时**不再直接闪退**，而是
    // 自动在合适目录生成带强随机密钥的 config.json + 大字打印密钥/面板地址，再正常启动。
    // 这样下载单个 exe 双击、或首次运行都能开箱即用，无需先跑 start.bat。
    // 已有 config 则完全不碰（绝不覆盖用户配置）。返回 (实际落盘路径, 是否本次新生成)。
    // freshly_generated 供启动后「仅首次自动开浏览器」判断。
    let (config_path, freshly_generated) = bootstrap_config_if_missing(&config_path);

    let config = Config::load(&config_path).unwrap_or_else(|e| {
        tracing::error!("加载配置失败: {}", e);
        std::process::exit(1);
    });

    // 加载凭证（支持单对象或数组格式）
    // 默认名场景走数据隔离解析（Windows→KiroStudio-data/，兼容旧位置）；显式 --credentials 原样尊重。
    let credentials_path = args.credentials.unwrap_or_else(|| {
        resolve_default_data_path(KiroCredentials::default_credentials_path())
            .to_string_lossy()
            .to_string()
    });
    let credentials_config = CredentialsConfig::load(&credentials_path).unwrap_or_else(|e| {
        // 加载失败即 fail-safe 退出(而非空池启动)——**故意如此**:若是 at-rest 密文解不开
        // (密钥文件丢失/来自别机),空池启动后一旦 persist 就会用空池覆盖掉那份仍可恢复的密文
        // = 永久丢号。宁可拒绝启动、保留密文不动,让用户按下方指引恢复(密文本身没坏)。
        tracing::error!("加载凭证失败,拒绝启动以保护现有凭据文件不被覆盖: {}", e);
        tracing::error!(
            "若启用了 at-rest 加密:请确认密钥文件 {:?} 存在且未被移动;跨机器迁移请带上明文导出重新导入。",
            crate::common::secret_store::key_path_for(std::path::Path::new(&credentials_path))
        );
        std::process::exit(1);
    });

    // 判断是否为多凭据格式（用于刷新后回写）
    let is_multiple_format = credentials_config.is_multiple();

    // 转换为按优先级排序的凭据列表
    let mut credentials_list = credentials_config.into_sorted_credentials();

    // 检查 KIRO_API_KEY 环境变量，自动创建 API Key 凭据
    if let Ok(kiro_api_key) = std::env::var("KIRO_API_KEY") {
        if kiro_api_key.is_empty() {
            tracing::warn!("KIRO_API_KEY 环境变量已设置但为空，视为未配置");
        } else {
            tracing::info!("检测到 KIRO_API_KEY 环境变量，添加 API Key 凭据（最高优先级）");
            let api_key_cred = KiroCredentials {
                kiro_api_key: Some(kiro_api_key),
                auth_method: Some("api_key".to_string()),
                priority: 0,
                ..Default::default()
            };
            credentials_list.insert(0, api_key_cred);
        }
    }

    tracing::info!("已加载 {} 个凭据配置", credentials_list.len());

    // 获取第一个凭据用于日志显示。
    // 安全：只打印非敏感可识别字段；KiroCredentials 的 Debug 已在类型层脱敏，
    // 此处再显式收窄，双保险杜绝 refreshToken/clientSecret/kiroApiKey 明文入日志。
    let first_credentials = credentials_list.first().cloned().unwrap_or_default();
    tracing::debug!(
        "主凭证概览: id={:?}, auth_method={:?}, email={:?}, endpoint={:?}",
        first_credentials.id,
        first_credentials.auth_method,
        first_credentials.email,
        first_credentials.endpoint
    );

    // 获取 API Key
    // 安全：不仅要求 apiKey 存在，还要求非空白字符串。
    // 否则 apiKey="" 会导致 auth_middleware 里 constant_time_eq(key, "") 对
    // 任意空 key（如 `x-api-key:` 或 `Authorization: Bearer `）返回 true，
    // 造成整个 /v1 网关 fail-open、匿名可直接消耗上游凭据。
    // 与下方 admin_api_key 的空值防护保持对称。
    let api_key = config.api_key.clone().unwrap_or_else(|| {
        tracing::error!("配置文件中未设置 apiKey");
        std::process::exit(1);
    });
    if api_key.trim().is_empty() {
        tracing::error!("配置文件中 apiKey 为空，拒绝以无鉴权方式启动");
        std::process::exit(1);
    }

    // 播种进鉴权热更单元（common::auth_keys）：后续 admin 改 apiKey 走 setter 即时生效，
    // 无需重启（重启会掐断在途流式请求）。此处已判非空，播种不会失败；若真失败，
    // expect panic（fail-closed）——继续跑会让 /v1 匿名可达。
    crate::common::auth_keys::set_user_key(&api_key)
        .expect("apiKey 为空——拒绝以无鉴权方式提供 /v1（空值会导致鉴权 fail-open）");

    // 构建代理配置
    let proxy_config = config.proxy_url.as_ref().map(|url| {
        let mut proxy = http_client::ProxyConfig::new(url);
        if let (Some(username), Some(password)) = (&config.proxy_username, &config.proxy_password) {
            proxy = proxy.with_auth(username, password);
        }
        proxy
    });

    if proxy_config.is_some() {
        tracing::info!("已配置 HTTP 代理: {}", config.proxy_url.as_ref().unwrap());
    }

    // 构建端点注册表（收口在 endpoint::registry，避免此处与旁路各自维护一份端点清单）：
    // - ide：Kiro IDE 协议（runtime.{region}.kiro.dev/generateAssistantResponse）
    // - cli：Amazon Q CLI 协议（q.{region}.amazonaws.com 服务根 + X-Amz-Target +
    //   tokentype:API_KEY，绝不带 profileArn）。ksk_ 号自动路由至此，也可显式指定。
    let endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = kiro::endpoint::registry();

    // 校验默认端点存在
    if !endpoints.contains_key(&config.default_endpoint) {
        tracing::error!("默认端点 \"{}\" 未注册", config.default_endpoint);
        std::process::exit(1);
    }

    // 校验所有凭据**实际会用**的端点都已注册
    //
    // 口径必须与 provider 的 endpoint_for 一致（同走 effective_endpoint）：显式字段优先、
    // ksk_ 号自动路由到 cli、其余回退默认。若这里只看显式字段，自动路由到未注册端点的
    // 凭据会绕过启动门禁，直到第一个请求打进来才在热路径上炸「未知端点」。
    for cred in &credentials_list {
        let name = cred.effective_endpoint(&config.default_endpoint);
        if !endpoints.contains_key(name) {
            tracing::error!(
                "凭据 id={:?} 指定了未知端点 \"{}\"（已注册: {:?}）",
                cred.id,
                name,
                endpoints.keys().collect::<Vec<_>>()
            );
            std::process::exit(1);
        }
    }

    // CLI body 对齐 kiro-rs 的开关（默认关）。开着时启动期就说清楚"影响几个号"——
    // 这是个拿线上流量换数据的 A/B，事后看日志必须能确定当时是开还是关、覆盖面多大。
    // 不设运行时原子镜像：`CliEndpoint::transform_api_body` 从 `ctx.config` 读，而那份
    // Config 是 provider 每次调用时从 ArcSwap `load_full()` 取的新快照 ⇒ 改配置后下一个
    // 请求即生效，加镜像只会多一份要同步的真值（详见该字段的文档注释）。
    if config.cli_origin_kiro_cli {
        let cli_count = credentials_list
            .iter()
            .filter(|c| {
                c.effective_endpoint(&config.default_endpoint)
                    == kiro::endpoint::cli::CLI_ENDPOINT_NAME
            })
            .count();
        tracing::warn!(
            "cliOriginKiroCli 已开启：{} 个 CLI(ksk_) 号的请求体将按真实 Kiro CLI 形状发送\
             （origin=KIRO_CLI + 去 agentContinuationId + 去 history.modelId）。\
             这是未经线上长期验证的上游协议形状，出现异常先关掉此项。IDE 号不受影响",
            cli_count
        );
    }

    let endpoint_names: Vec<String> = endpoints.keys().cloned().collect();

    // 托盘「重启服务」复用启动时的 config/credentials 路径拉起新进程（Windows）。
    // credentials_path 下面会被 .into() 移动进 TokenManager，config_path 是 String，此处先各克隆一份。
    #[cfg(windows)]
    let tray_relaunch_paths = (
        std::path::PathBuf::from(&config_path),
        std::path::PathBuf::from(&credentials_path),
    );

    // 创建 MultiTokenManager 和 KiroProvider
    let token_manager = MultiTokenManager::new(
        config.clone(),
        credentials_list,
        proxy_config.clone(),
        Some(credentials_path.into()),
        is_multiple_format,
    )
    .unwrap_or_else(|e| {
        tracing::error!("创建 Token 管理器失败: {}", e);
        std::process::exit(1);
    });
    let token_manager = Arc::new(token_manager);

    // 主动 token 预刷新（批次4.4）：后台提前刷将过期的 token，把刷新移出请求热路径。
    // 仅对可刷新凭据生效；未启用则退回请求时按需刷新。
    // TIER2 热重载：spawn 交由 token_manager 的受管任务槽（respawn_refresh_task），
    // 启动即受管，admin 改 proactive/lead/interval 后 abort+respawn 即时生效不重启。
    token_manager.respawn_refresh_task();

    // 存量号 region 回填（后台、串行、绝不阻塞启动）。
    //
    // `add_credential` 里的探测只覆盖**新**号，救不了已经在池里的。而线上真实状态是
    // 池里的 `ksk_` 号没有 region 字段、靠回退 `config.region` **恰好**对 ——
    // 谁改一次全局 region，这些号当场 100% 403，然后被误判成「凭据坏了」。
    //
    // 三条设计约束：
    // ① `spawn` 后台跑 —— 探测是真实上游往返，绝不能进启动关键路径（服务要立刻能收流量）。
    // ② **串行 + 间隔** —— 并发探 N 个号 = 同出口 IP 短时间打一批 management 端点，
    //    那是风控要抓的突发特征。补号场景下这个循环可能有十几个号。
    // ③ 只探「api_key 且完全没有任何 region 字段」的 —— 判据在 probe_api_region 内，
    //    带 region 的是运维/推号方的明确意图，绝不覆盖。
    {
        let region_mgr = token_manager.clone();
        tokio::spawn(async move {
            // 先让启动流程与首批请求跑起来，避免和 token 预刷新抢同一批上游往返。
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            let ids = region_mgr.ids_needing_region_probe();
            if ids.is_empty() {
                return;
            }
            tracing::info!(
                "存量 region 回填：{} 个 api_key 号无 region 字段，后台逐个探测（串行，每个间隔 3s）",
                ids.len()
            );
            for id in ids {
                // ⚠️ 启动回填**刻意忽略判决**，绝不据此禁用 —— 与上号路径相反。
                //
                // 这里面对的是**已在服役**的存量号：它们没有 region 字段，但正靠
                // `config.region` 回退恰好打对（线上实测就有这种号在 90%+ 成功率地出活）。
                // 探测在这种号上返 `NoUsableRegion` 完全可能只是探测那一刻上游抖动，
                // 而据此禁用会把一个正在成功出活的号打掉 —— 那比不回填糟得多。
                //
                // 上号路径必须禁用是因为那里的号**尚未接过任何流量**，禁用的代价只是
                // 「多一次人工确认」；这里的代价是「打断正在服务的号」。同一个判决，
                // 两条路径的正确处置相反，故判决权归调用方（见
                // `probe_and_persist_api_region` 的返回值注释）。
                let _ = region_mgr.probe_and_persist_api_region(id).await;
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
        });
    }

    // 会话亲和性定时清理：affinity map 的 key 是客户端可控的 session id，
    // 仅靠 get() 惰性删除无法回收「不再出现的 session」，长跑会内存泄漏。
    // 每 5 分钟主动 retain 掉超过 TTL 的空闲条目（interval 用 Skip 防唤醒后连刷）。
    {
        let affinity_mgr = token_manager.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(5 * 60));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                affinity_mgr.cleanup_affinity();
                // 顺带回收 RPM 滚动窗口里不再活跃的凭据条目（共用同一 5 分钟 tick）
                affinity_mgr.cleanup_scheduling();
            }
        });
    }

    // 凭据回收站保留清理：软删除的凭据超过 trash_retention_days 后彻底清理。
    // 0 表示永久保留（purge_expired_trash 内部直接短路）。每 6 小时扫描一次。
    {
        let trash_mgr = token_manager.clone();
        let retention_days = config.trash_retention_days;
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(6 * 3600));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                trash_mgr.purge_expired_trash(retention_days);
            }
        });
    }

    // 登录页背景图预取：启动即拉一批到内存池，之后后台定时补充。
    // 请求命中内存字节秒回，不再在登录页热路径实时打图源。关闭时不 spawn。
    // R18 开关先写入运行时镜像（默认 true），预取轮次按此取 r18 参数。
    admin_ui::set_login_background_r18(config.login_background_r18);
    mark_main_seeded("login_background_r18");
    admin_ui::spawn_bg_prefetch(config.login_background_enabled);

    // Kiro IDE 版本伪装：后台每 12h 拉官方稳定版元数据，成功后 UA 版本段用拉到的
    // 版本号，失败静默降级回 config.kiro_version（不阻塞启动）。关闭时不 spawn
    // （缓存恒空，UA 行为零变化）。热更不生效：启动期一次性读取。
    if config.ua_version_fetch {
        kiro::version_mask::spawn_refresher(proxy_config.clone(), config.tls_backend);
    }

    // OTA 自动检查（仅检查 + 打日志，不自动下载替换——见 admin::update::spawn_auto_check）。
    // 默认关：检查本身是 GitHub API 出站往返，多数部署走面板手动升级按钮。
    // 热更不生效：启动期一次性读取。
    if config.ota_auto_check {
        admin::update::spawn_auto_check(config.ota_auto_check_interval_hours);
    }

    // 上游 trace 埋点（P0-A 排障，kiro::upstream_trace）：默认关，关闭时只付一次
    // 原子读（零分配零 IO）。开启后把「上游原始响应 + 网关分支判断」写进 JSONL，
    // 用于回答日志答不了的四个问题（Retry-After 原值 / 两 region 响应差异 / 429 body
    // 配额字段 / reasoningContentEvent 形状）。热更不生效：配置字段不进
    // UpdateConfigRequest，改 config.json 后重启生效（与 trust_forwarded_header 同款
    // 启动期一次性读取的范式，见 515-521 行）。
    kiro::upstream_trace::sync_from_config(
        config.upstream_trace_enabled,
        &config.upstream_trace_path,
        config.upstream_trace_max_bytes,
    );
    mark_main_seeded("upstream_trace");

    // 指纹采集开关：把配置写入热路径运行时镜像（默认 true）。关闭后不采集
    // 下游客户端 device/ip/os/browser。admin 改开关时会立即改写此镜像。
    anthropic::set_collect_client_fingerprint(config.collect_client_fingerprint);
    // ⭐ 修复已知问题 #6：把 trust_forwarded_header 也喂给**业务层**。
    // 此前它只进了下面的 `SecurityState`，handler 层完全看不到 → 两层 IP 判定口径分叉，
    // 反代在公网且开了该开关时，业务层黑名单会封掉反代自己（= 全部用户）。
    // 与 `SecurityState` 同为启动期读取（改该值仍需重启，见 admin 的 restart_fields），
    // 所以这里一次写入即可，不需要 admin 侧的热改钩子。
    anthropic::set_trust_forwarded_header(config.trust_forwarded_header);

    // IP 黑名单业务层镜像(按真实客户端 IP 封禁,反代后也生效;admin 改配置时热更):
    anthropic::handlers::set_ip_blocklist(&config.ip_blocklist);
    // 机器码黑名单业务层镜像(命中即拒;admin 改配置时热更):
    anthropic::handlers::set_machine_code_blocklist(&config.machine_code_blocklist);

    let kiro_provider = KiroProvider::with_proxy(
        token_manager.clone(),
        proxy_config.clone(),
        endpoints,
        config.default_endpoint.clone(),
    );

    // 初始化用量统计管道（可选）：装配 trace_db + usage_stats 两个 sink
    // 返回给 admin 侧共享的实例句柄（未启用时为 None）
    let usage_handles = if config.usage_enabled {
        init_usage_pipeline(&config)
    } else {
        tracing::info!("用量统计未启用（usage_enabled=false）");
        None
    };

    // 初始化 count_tokens 配置
    token::init_config(token::CountTokensConfig {
        api_url: config.count_tokens_api_url.clone(),
        api_key: config.count_tokens_api_key.clone(),
        auth_type: config.count_tokens_auth_type.clone(),
        proxy: proxy_config,
        tls_backend: config.tls_backend,
    });
    mark_main_seeded("count_tokens");

    // 文本化 invoke 重组 + stray 熔断两开关:启动播种进程级镜像(handlers 热路径读),admin 改后即时生效。
    anthropic::handlers::set_tool_reclaim_textified_invoke(config.tool_reclaim_textified_invoke);
    anthropic::handlers::set_tool_stray_repeat_guard(config.tool_stray_repeat_guard);

    // 构建 Anthropic API 路由（profile_arn 由 provider 层根据实际凭据动态注入）
    // prompt cache 记账下发开关：播种进 handlers 的 TIER3 进程镜像。
    //
    // 不走 create_router_with_provider 的参数是刻意的——那个签名已有 14 个参数并挂着
    // #[allow(clippy::too_many_arguments)]，再加只会更难读。这里与下方 respawn_balance_task
    // 同风格：启动即接线，admin 改配置后调同一个 setter 即时生效。
    anthropic::set_prompt_cache_enabled(config.prompt_cache_enabled);

    // Kiro 原生 effort 开关：播种进 converter 的 TIER3 进程镜像（默认关 = 行为逐字节
    // 不变；开 = 白名单模型 + thinking 走 output_config.effort 原生通道而非 XML 标签）。
    anthropic::set_native_thinking_effort_enabled(config.native_thinking_effort_enabled);
    mark_main_seeded("native_thinking_effort");

    // 透传模拟缓存（mockCacheEnabled）：播种进 handlers 的 TIER3 进程镜像（默认关）。
    // 开启后透传响应 usage 注入 cache_read = round(input × ratio) 的伪造值、creation 置 0，
    // 仅供下游（sub2api 等）展示缓存分支；admin 改配置时热更即时改写同一镜像。
    anthropic::handlers::set_mock_cache_config(
        config.mock_cache_enabled,
        config.mock_cache_read_ratio,
    );

    // CC↔Kiro 工具名/参数映射开关（默认 true）：播种进 converter 的进程级原子镜像
    // （默认 true = 现状行为零变化）。关闭后 8 个内置工具（Write→fs_write 等）原样
    // 透传（仅超长缩短），适配非 Claude Code 客户端/同名自定义工具；admin 改配置时
    // 热更改写同一镜像（service.rs update_config 调同一个 setter）。
    anthropic::set_tool_compat_mapping(config.tool_compat_mapping);
    mark_main_seeded("tool_compat_mapping");

    // 模型感知正向路由巡检（2026-08-16 W16 接线）：30min 周期拉取透传池各号模型目录，
    // 三态缓存（Confirmed/Unknown/Unsupported）+ support_rank 排序——混池请求不再
    // 首次打错号（黑名单负向兜底不变）。
    {
        let tm3 = kiro_provider.token_manager_arc();
        tokio::spawn(async move {
            tm3.spawn_model_catalog_probe();
        });
    }

    // stats_stale watchdog（2026-08-16 收尾接线，blockers 17e）：usage JSONL 数据
    // 断更 10 分钟即告警（bump "stats_stale"）。report_if_stale 是幂等的（告警后复位）。
    {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                crate::common::alerting::report_if_stale("usage_jsonl", 600);
            }
        });
    }

    // 错误码/提示词覆盖表：播种进 handlers 的进程镜像（默认空表 = 全部走内置默认，
    // 零行为变化）。⚠️ 播种前校验：管理员手改 config.json 出错时告警 + 降级为空表
    // （不阻塞启动但配置不生效）；admin 热更（update_config）与 reload_config 会改写
    // 同一镜像，错误翻译处每请求读镜像快照（见 handlers::resolve_msg）。
    anthropic::handlers::set_error_messages(sanitize_error_messages_table(
        config.error_messages.clone(),
    ));

    let anthropic_app = anthropic::create_router_with_provider(
        &api_key,
        Some(kiro_provider),
        config.extract_thinking,
        config.cc_auto_buffer,
        &config.cors_allowed_origins,
        config.max_body_bytes,
        config.compression.clone(),
        config.strip_env_noise,
        config.tool_clean_leaked_tokens,
        config.tool_stream_align_failure,
        config.tool_expose_error_to_client,
        config.tool_repair_json,
        config.tool_truncation_recovery,
        config.tool_description_max_chars,
    );

    // 构建 Admin API 路由（如果配置了非空的 admin_api_key）
    // 安全检查：空字符串被视为未配置，防止空 key 绕过认证
    let admin_key_valid = config
        .admin_api_key
        .as_ref()
        .map(|k| !k.trim().is_empty())
        .unwrap_or(false);

    let app = if let Some(admin_key) = &config.admin_api_key {
        if admin_key.trim().is_empty() {
            tracing::warn!("admin_api_key 配置为空，Admin API 未启用");
            anthropic_app
        } else {
            // 播种进鉴权热更单元：admin 改 adminApiKey 走 setter 即时生效，无需重启。
            // 挂载前已判非空；播种失败属「校验被绕过」，expect panic（fail-closed）。
            crate::common::auth_keys::set_admin_key(admin_key)
                .expect("adminApiKey 为空——拒绝挂载无鉴权 Admin API");
            let admin_service =
                admin::AdminService::new(token_manager.clone(), endpoint_names.clone());
            let mut admin_state = admin::AdminState::new(admin_key, admin_service);
            // 注入用量查询句柄（未启用统计时为 None，端点返回 503）
            if let Some(handles) = &usage_handles {
                admin_state =
                    admin_state.with_usage(handles.stats.clone(), handles.trace_db.clone());
            }

            // A6：温和的周期性余额刷新（严格受控）。
            // 为避免触发上游风控：绝不在启动/挂载时批量拉——后台任务首轮也要等满一个
            // 完整间隔才开始，且逐个刷新、每个之间留间隔（分散节奏），只刷未禁用的号，
            // 仅更新缓存供展示，绝不做主动禁用。0 = 禁用（安全默认之一）。
            // TIER2 热重载：spawn 交由 AdminService 的受管任务槽（respawn_balance_task），
            // 启动即受管，admin 改 balanceRefreshIntervalSecs 后 abort+respawn 即时生效不重启。
            admin_state.service.respawn_balance_task();

            // admin 树的 body 上限：axum 默认 2MiB 会卡住批量推号（/import/keys）——
            // 批量导入一次可含上千个 ksk_ key，体量远超 2MiB。与 /v1 同口径复用
            // max_body_bytes（0 = 不限制，与 anthropic/router.rs 的语义一致；若这里
            // 直接 DefaultBodyLimit::max(0) 会把所有非空 body 全 413，故必须走同样的分支）。
            let admin_body_limit = if config.max_body_bytes == 0 {
                axum::extract::DefaultBodyLimit::disable()
            } else {
                axum::extract::DefaultBodyLimit::max(config.max_body_bytes)
            };

            // 兼容别名路由必须在 admin_app 之前建（后者会 move 掉 admin_state）。
            // 只含 POST /import/keys 一个端点，鉴权与 admin 树一致，见
            // create_import_alias_router 的说明。
            let import_alias_app =
                admin::create_import_alias_router(admin_state.clone()).layer(admin_body_limit);

            let admin_app =
                admin::create_admin_router(admin_state).layer(admin_body_limit);

            // 创建 Admin UI 路由
            //（纯 GET：静态资源 + 背景图端点，无请求体，不需要 body limit layer）
            let admin_ui_app = admin_ui::create_admin_ui_router();

            tracing::info!("Admin API 已启用");
            tracing::info!("Admin UI 已启用: /admin");
            anthropic_app
                .nest("/api/admin", admin_app)
                // 外部对接方的固定路径 POST /api/import/keys（改不了），
                // 等价于 /api/admin/import/keys。
                .nest("/api", import_alias_app)
                .nest("/admin", admin_ui_app)
                // 帮助中心直达 URL（2026-08-14）：/help 与 /admin 并列挂载，
                // 同一份 index.html，前端按路径渲染帮助页。
                .route("/help", axum::routing::get(help_page_handler))
        }
    } else {
        anthropic_app
    };

    // 健康探针 /healthz：**未鉴权**端点（auth 中间件只挂在 /v1、/cc/v1 子路由上，
    // 根路由不受影响），供 Docker HEALTHCHECK / 反代主动探测使用。
    //
    // 刻意不返回任何敏感信息（只报 ok/version/号池数/库可写性）；号池数量是运维
    // 观测量，不属密钥级敏感数据。判定口径：
    // - config_loaded：进程能走到 serve 必然 config/凭据加载成功（失败在启动期直接
    //   exit(1)），此处没有更早的信号可读，恒为 true。
    // - sqlite_writable：用量统计未启用（usage_enabled=false 或 SQLite 打开失败 →
    //   usage_handles=None）时 false；启用时以一次 count 探测库可读性。
    // - recent_success_rate **刻意不返回**：recovery_metrics 是单调计数器、无时间
    //   窗口；usage_stats 只有 24h/逐小时聚合窗口，均非「近 N 秒」成功率。为避免
    //   语义欺骗不发明新计数器（需要时应在 recovery_metrics 侧加环形窗口）。
    let healthz_app = {
        let tm = token_manager.clone();
        let handles = usage_handles.clone();
        axum::Router::new().route(
            "/healthz",
            axum::routing::get(move || async move {
                let snapshot = tm.snapshot();
                let sqlite_writable = handles
                    .as_ref()
                    .map(|h| h.trace_db.count().is_ok())
                    .unwrap_or(false);
                axum::Json(serde_json::json!({
                    "ok": true,
                    "version": env!("CARGO_PKG_VERSION"),
                    "build_sha": env!("KIRO_BUILD_SHA"),
                    "config_loaded": true,
                    "pool_count": snapshot.total,
                    "sqlite_writable": sqlite_writable,
                }))
            }),
        )
    };
    let app = healthz_app.merge(app);

    // B7 启动播种集中校验：所有播种点（含 create_router_with_provider 间接播种）之后、
    // serve 之前。缺失只告警不阻塞——日志醒目列出未接线的镜像。
    verify_runtime_mirrors_wired();

    // 启动服务器
    let addr = format!("{}:{}", config.host, config.port);
    tracing::info!("启动 Anthropic API 端点: {}", addr);
    tracing::info!(
        "build: {} (sha {})",
        env!("CARGO_PKG_VERSION"),
        env!("KIRO_BUILD_SHA")
    );
    // 只打印固定短前缀 + 长度指纹，不按比例暴露密钥（半个密钥会显著降低爆破熵）
    {
        let masked = if api_key.len() > 8 {
            format!("{}…{}", &api_key[..4], &api_key[api_key.len() - 2..])
        } else {
            "***".to_string()
        };
        tracing::info!("API Key 已加载: {} (len={})", masked, api_key.len());
    }
    tracing::info!("可用 API:");
    tracing::info!("  GET  /v1/models");
    tracing::info!("  POST /v1/messages");
    tracing::info!("  POST /v1/messages/count_tokens");
    if admin_key_valid {
        tracing::info!("Admin API:");
        tracing::info!("  GET  /api/admin/credentials");
        tracing::info!("  POST /api/admin/credentials/:index/disabled");
        tracing::info!("  POST /api/admin/credentials/:index/priority");
        tracing::info!("  POST /api/admin/credentials/:index/reset");
        tracing::info!("  GET  /api/admin/credentials/:index/balance");
        tracing::info!("Admin UI:");
        tracing::info!("  GET  /admin");
    }

    // 入口安全层（IP 白名单 + 每-IP 限流）。两者都未配置时不挂载中间件，零开销。
    let app = match common::security::SecurityState::from_config(
        &config.ip_allowlist,
        &config.ip_blocklist,
        config.ingress_rate_limit_per_min,
        config.trust_forwarded_header,
    ) {
        Some(sec_state) => {
            if sec_state.allowlist.is_active() {
                tracing::info!(
                    "入口 IP 白名单已启用（{} 条规则）",
                    config.ip_allowlist.len()
                );
            }
            if sec_state.blocklist.is_active() {
                tracing::info!(
                    "入口 IP 黑名单已启用（{} 条规则）",
                    config.ip_blocklist.len()
                );
            }
            if sec_state.rate_limiter.is_active() {
                tracing::info!(
                    "入口限流已启用：{} 请求/分钟/IP",
                    config.ingress_rate_limit_per_min
                );
            }
            if config.trust_forwarded_header {
                tracing::warn!("已信任 X-Forwarded-For：仅当位于可信反代之后才应开启");
            }
            app.layer(axum::middleware::from_fn_with_state(
                sec_state,
                common::security::security_middleware,
            ))
        }
        None => app,
    };

    let listener = match bind_listener(&addr) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("绑定 {} 失败: {:#}", addr, e);
            std::process::exit(1);
        }
    };
    // OTA 回滚兜底（阶段A）：bind 成功即越过 config/凭据/端口三道启动门 → 清零启动计数器
    // （向 systemd ExecStartPre 守卫脚本表明「非 crashloop」），并 spawn 稳定 30s 后写 .health
    // + 删 .bak 回滚点的确认任务。详见 common::health_marker + deploy/rollback-guard.sh。
    common::health_marker::clear_boot_attempts();
    common::health_marker::spawn_health_confirm(env!("CARGO_PKG_VERSION").to_string());
    // 首次启动自动开浏览器（仅 Windows）：本次新生成 config + 本地回环 host + 未设 KIRO_NO_BROWSER
    // 时，bind 成功后打开默认浏览器到 /admin，实现「点击软件直接进面板」。仅首次（新装/首跑），
    // 已有 config 重启不开，避免每次重启骚扰。
    maybe_open_browser_on_first_run(freshly_generated, &config.host, config.port);
    // Windows 系统托盘：另 spawn 一个专用 std 线程跑 win32 消息循环 + 托盘图标（不占 tokio 主线程）。
    // 菜单:打开网页/复制密钥/重启服务/版本/退出。「退出」通过 tray::quit_notify() 通知本进程优雅关闭。
    #[cfg(windows)]
    {
        let admin_key_for_tray = config.admin_api_key.clone().unwrap_or_default();
        let tray_host = config.host.clone();
        let tray_port = config.port;
        let (relaunch_config_path, relaunch_credentials_path) = tray_relaunch_paths;
        // 托盘「重启服务」trigger：spawn detached 重启脚本（用启动时的 config/credentials 路径拉起
        // 新进程）后，通知主线程优雅关闭（drain 在途请求、关 SQLite），主线程随后以退出码 3 退出。
        // run.bat 监督循环见退出码 3 = 用户主动退出、不重拉；由重启脚本单独拉起新进程 → 无双拉。
        // 与面板一键重启同源（复用 admin::service::spawn_windows_relaunch_process）。
        let relaunch_trigger: Box<dyn Fn() + Send> = Box::new(move || {
            tracing::info!("[托盘] 用户点击重启服务，spawn 重启脚本并优雅关闭…");
            admin::spawn_windows_relaunch_process(
                Some(relaunch_config_path.clone()),
                Some(relaunch_credentials_path.clone()),
            );
            tray::quit_notify().notify_one();
        });
        std::thread::Builder::new()
            .name("kiro-tray".into())
            .spawn(move || {
                tray::run(tray::TrayConfig {
                    host: tray_host,
                    port: tray_port,
                    admin_api_key: admin_key_for_tray,
                    relaunch: Some(tray::RelaunchInfo {
                        trigger: relaunch_trigger,
                    }),
                });
            })
            .ok();
    }
    // into_make_service_with_connect_info 让中间件可通过 ConnectInfo 拿到对端 IP
    // with_graceful_shutdown：收到 SIGTERM/Ctrl-C 后停止接新连接，等在途请求（含 SSE 流）drain
    //
    // ⭐ drain 上限必须在**这里**用 select! 竞速，不能只靠 shutdown future 里 sleep：
    // `with_graceful_shutdown` 的语义是「此 future 完成 ⇒ 停止接新连接」，之后
    // `serve().await` 仍会**无上限**等在途请求。于是把 sleep 放在 shutdown future 里
    // 两个承诺一个都不成立 ——
    //   · 在途请求早已 drain 完也白等满 SHUTDOWN_DRAIN_CAP_SECS（部署窗口凭空变长）；
    //   · 真有长流式 SSE 时也**不会**按注释承诺断开（那才是 74s 停服 / 167 次 502 的成因）。
    // 竞速取先到者：drain 完就立刻走；到上限则 drop 掉 serve future ⇒ 监听套接字与
    // 连接任务一并释放 ⇒ 残余连接真的被断开（客户端看到流中断，可重试，好过 502 全量失败）。
    let drain_deadline = std::sync::Arc::new(tokio::sync::Notify::new());
    let serve_fut = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_with_drain_cap(
        token_manager.clone(),
        drain_deadline.clone(),
    ));
    match race_serve_against_drain_cap(
        serve_fut,
        drain_deadline,
        std::time::Duration::from_secs(SHUTDOWN_DRAIN_CAP_SECS),
    )
    .await
    {
        DrainOutcome::Drained(r) => {
            if let Err(e) = r {
                // serve() 的 IO 错误（如监听套接字在停机瞬间失效）不该 panic——
                // panic 会绕过下方托盘退出的 exit(3) 与优雅停机收尾。记录后走统一的失败退出路径。
                tracing::error!("serve 结束但返回错误: {:#}", e);
                std::process::exit(1);
            }
            tracing::info!("在途请求已全部 drain 完毕");
        }
        DrainOutcome::CapReached => {
            tracing::warn!(
                cap_secs = SHUTDOWN_DRAIN_CAP_SECS,
                "drain 宽限期到上限，断开残余连接以尽快让位给新进程"
            );
        }
    }

    tracing::info!("服务已优雅停机");
    // 托盘「退出」触发的停机：以退出码 3 退出,让 start.bat/run.bat 监督循环识别为「用户主动退出」
    // 而不重拉(区别于面板重启/OTA 的 exit 0)。裸跑无脚本时退出码不影响。
    #[cfg(windows)]
    if TRAY_QUIT_REQUESTED.load(std::sync::atomic::Ordering::SeqCst) {
        std::process::exit(tray::TRAY_QUIT_EXIT_CODE);
    }
}

/// 优雅停机的 **drain 上限**（秒）。
///
/// ## 为什么需要上限（线上实测）
///
/// `with_graceful_shutdown` 会**无限**等待在途请求 drain，而本网关的在途请求是
/// 长流式 SSE（一次 opus 响应动辄数十秒到数分钟）。于是 `systemctl restart` 实际
/// 要等到 systemd 的 `TimeoutStopUSec`（线上 90s）超时才 SIGKILL。
///
/// 实测：一次部署重启 **02:54:56 → 02:56:10 停了 74 秒**，期间 Caddy 对所有请求
/// 回 502 —— 单次部署就产生 167 次 502（占当日 502 总量的 41%）。
/// 而 502 的 p50 duration 仅 0.01s，即连接被瞬间拒绝，正是"进程不在监听"的特征。
///
/// ## 取 8 秒的理由
///
/// 目标是"绝大多数短请求能正常收尾，但不为个别长流式无限期停服"。
/// 实测非流式与短流式请求 p50 约 2.6s、p90 约 3.4s，8s 覆盖到 p99 量级；
/// 而真正的长响应本来就会被客户端重试（Claude Code 有自身退避重试）。
///
/// 上限到点后 drop 掉 serve future 让进程退出：未 drain 完的连接被断开，客户端看到
/// 流中断而非 502 —— 前者可重试，后者在部署窗口里是全量失败。这个交换明显更好。
///
/// ⚠️ 竞速在 `main` 的 `select!` 里，**不在** [`shutdown_with_drain_cap`] 内部。
/// 后者只是 `with_graceful_shutdown` 的触发器，它返回只意味着"停止接新连接"，
/// `serve().await` 之后仍无上限地等 —— 把 sleep 放在它里面两个承诺都不成立。
const SHUTDOWN_DRAIN_CAP_SECS: u64 = 8;

/// [`race_serve_against_drain_cap`] 的结果：drain 自然完成，还是撞上宽限期上限。
enum DrainOutcome {
    /// `serve()` 先返回 —— 在途请求全部 drain 完（携带它的返回值）。
    Drained(std::io::Result<()>),
    /// 宽限期到点 —— serve future 被 drop，残余连接断开。
    CapReached,
}

/// 让 `serve()` 与「停机信号后的宽限期」竞速，取先到者。
///
/// 抽成独立函数**只为可测**：竞速逻辑原先内联在 `main` 里，而 `main` 需要真实
/// listener + 真实信号，任何测试都到不了 —— 于是 #22 那种「注释承诺的行为代码里
/// 不存在」的缺陷可以长期无人发现。这里泛化掉 serve future 后，测试能用假 future
/// 覆盖三条路径（提前 drain / 撞上限 / **无信号时永不超时**）。
///
/// 第三条是承重的：宽限期必须从**信号到达**起算，不是从进程启动起算。
/// 若实现成后者，服务会在启动 [`SHUTDOWN_DRAIN_CAP_SECS`] 秒后自己退出。
/// `cap` 由调用方传入（生产是 [`SHUTDOWN_DRAIN_CAP_SECS`]）：测试传毫秒级值即可用
/// 真实时钟跑完，不必引入 tokio 的 `test-util` feature（它不在 `full` 里，
/// 为一个测试新增 dev-dependency 不值得）。
///
/// 泛型是 `IntoFuture` 而非 `Future`：axum 的 `WithGracefulShutdown` 只实现前者。
async fn race_serve_against_drain_cap<F>(
    serve_fut: F,
    drain_deadline: Arc<tokio::sync::Notify>,
    cap: std::time::Duration,
) -> DrainOutcome
where
    F: std::future::IntoFuture<Output = std::io::Result<()>>,
{
    tokio::select! {
        r = serve_fut.into_future() => DrainOutcome::Drained(r),
        _ = async {
            // 先等停机信号到达（由 shutdown future 通知），再从那一刻起算宽限期。
            drain_deadline.notified().await;
            tokio::time::sleep(cap).await;
        } => DrainOutcome::CapReached,
    }
}

/// 停机信号 + drain 上限：收到信号后最多再等 [`SHUTDOWN_DRAIN_CAP_SECS`]。
///
/// axum 的 `with_graceful_shutdown` 语义是"此 future 完成即停止接新连接、
/// 然后等在途完成"。所以上限要加在**信号之后**：先等信号，再给一个封顶的宽限期，
/// 宽限期一到就让 future 返回，axum 随即结束。
async fn shutdown_with_drain_cap(
    token_manager: Arc<crate::kiro::token_manager::MultiTokenManager>,
    drain_deadline: Arc<tokio::sync::Notify>,
) {
    shutdown_signal().await;
    // ⭐ 收到信号后**立刻**强制落盘统计，不等 drain 结束。
    //
    // 必须在 sleep **之前**：线上 `TimeoutStopSec=10`，而 `serve().await` 在长流式 SSE
    // 上会超过它 ⇒ 今天 41 次 SIGTERM 里 39 次走到 SIGKILL ⇒ 放在 sleep 之后的收尾
    // 代码有很大概率压根不执行。放在信号后第一行则只要进程还活着就一定跑到。
    //
    // 为什么这件事重要（不是"统计好看"）：`has_ever_succeeded()` 读的是从 stats 恢复的
    // `success_count`，它是 provider 判「bearer-invalid 403 = 瞬态抖动 or 真 region 错配」
    // 的唯一判据。debounce 窗口内的成功增量被硬杀丢掉 ⇒ 重启后新号变成"从未成功过"
    // ⇒ 瞬态 403 三次即禁用。实测 20:20:30 启动、20:20:32 就把健康号 #483 打死。
    token_manager.flush_stats_now();
    // usage 管道（usage::pipeline）的停机 drain：**无 flush/drain 接口可调**。
    // 它是有界 mpsc（容量 10_000）+ 独立 OS 线程 worker，worker 随进程退出而终止，
    // 通道内残余记录（至多 CHANNEL_CAPACITY 条）会丢失——这是该管道的既定设计
    // （统计数据可容忍丢失，热路径绝不阻塞）。丢失量已被 usagePipelineDropped /
    // usagePipelineWritten 计数覆盖（/api/admin/recovery-metrics），停机那一刻的
    // 丢失还可从这两个数的差值读出。若要严格不丢，应在 pipeline 侧加
    // flush 接口（drop(tx) + join worker），此处不加接口、不改语义。
    tracing::info!(
        "收到停机信号，已强制落盘凭据统计，开始 drain（最多 {}s，超时则断开残余连接以尽快让位给新进程）",
        SHUTDOWN_DRAIN_CAP_SECS
    );
    // 通知 main 的竞速分支从**此刻**开始算宽限期，然后立即返回 ——
    // 返回即触发 axum「停止接新连接、等在途 drain」。
    //
    // ⚠️ 这里**不能**再 sleep：本 future 完成是 axum 开始 drain 的信号，在这里睡
    // 只是延后 drain 开始的时刻（那段时间还在接新连接），而 `serve().await` 之后
    // 依旧无上限。上限只能在外层 select! 里对 `serve()` 本身竞速。
    drain_deadline.notify_one();
}

/// 等待停机信号：Ctrl-C（全平台）或 SIGTERM（Unix，容器编排 docker stop / k8s 用）。
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("安装 Ctrl-C 处理器失败");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("安装 SIGTERM 处理器失败")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    // Windows 托盘「退出」：等托盘线程 notify。非 Windows 永挂（无此源）。
    #[cfg(windows)]
    let tray_quit = async {
        tray::quit_notify().notified().await;
    };
    #[cfg(not(windows))]
    let tray_quit = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("收到 Ctrl-C，开始优雅停机…"),
        _ = terminate => tracing::info!("收到 SIGTERM，开始优雅停机…"),
        _ = tray_quit => {
            tracing::info!("收到托盘退出，开始优雅停机…");
            // 标记托盘退出:优雅停机后 main 以特殊退出码 3 退出,让监督脚本识别「用户主动退出、别重拉」。
            TRAY_QUIT_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

/// 绑定监听端口。**Unix 上开启 `SO_REUSEPORT`**，使新旧进程能同时持有同一端口。
///
/// 为什么需要：升级时 `systemctl restart` 会先 SIGTERM 旧进程、等它退出，才起新进程。
/// 而本服务是优雅停机（`with_graceful_shutdown` 等在途请求 drain，SSE 长流可挂数十秒），
/// 于是端口出现一段**无人监听**的空窗 —— 实测 **20.16 秒**，期间所有入站请求都是
/// 连接拒绝（curl 返回 000），对上游 sub2api 表现为整条通道不可用。
///
/// 开启 `SO_REUSEPORT` 后可做零空窗交接：
///   1. 起新实例（绑同一端口，内核在新旧之间自动分流新连接）
///   2. 健康检查新实例通过
///   3. SIGTERM 旧实例 → 它停止接新连接、继续把在途请求 drain 完才退出
/// 全程端口始终有人监听，空窗为 0。部署脚本据此改造（见 deploy/hotswap.sh）。
///
/// ⚠️ 代价与注意：
/// - 同端口可被多进程绑定，意味着**配置错误时可能悄悄起两个实例**而不再报
///   「端口被占用」。故启动日志显式记录该行为，便于排查「改了配置却没生效」类问题。
/// - 两实例会同时读写同一份 credentials.json。交接窗口很短（秒级）且写盘走
///   `fs_atomic`（temp→fsync→rename）不会写坏文件，但仍应尽快 SIGTERM 旧实例，
///   不要让两个实例长期并存。
/// - Windows 无 `SO_REUSEPORT`（其 `SO_REUSEADDR` 语义不同且不安全），故仅 Unix 生效，
///   Windows 保持原有独占绑定行为。
fn bind_listener(addr: &str) -> anyhow::Result<tokio::net::TcpListener> {
    use std::net::SocketAddr;

    let sock_addr: SocketAddr = addr
        .parse()
        .map_err(|e| anyhow::anyhow!("解析监听地址 {} 失败: {}", addr, e))?;

    let domain = if sock_addr.is_ipv4() {
        socket2::Domain::IPV4
    } else {
        socket2::Domain::IPV6
    };
    let socket = socket2::Socket::new(domain, socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;

    // SO_REUSEADDR：允许立即复用处于 TIME_WAIT 的地址（原 tokio bind 默认行为，保持一致）。
    socket.set_reuse_address(true)?;

    #[cfg(unix)]
    {
        // SO_REUSEPORT：零空窗交接的关键。见函数文档。
        socket.set_reuse_port(true)?;
        tracing::info!(
            "监听 {} 已开启 SO_REUSEPORT（支持零空窗热交接；注意同端口可并存多实例）",
            addr
        );
    }

    socket.set_nonblocking(true)?;
    socket.bind(&sock_addr.into())?;
    // backlog 取 1024：与 tokio 默认量级一致，突发连接不至于被内核丢弃。
    socket.listen(1024)?;

    let std_listener: std::net::TcpListener = socket.into();
    Ok(tokio::net::TcpListener::from_std(std_listener)?)
}

/// 是否由托盘「退出」触发的停机（决定 main 的退出码：3=用户主动退出，监督脚本不重拉）。
static TRAY_QUIT_REQUESTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 启动播种前的错误码表校验（M1）：任一 key 非法 → 告警 + 降级为空表（全部走
/// 内置默认，配置不生效但不阻塞启动）；合法表原样返回。
///
/// 管理员手改 config.json 出错时不阻断服务，但必须告警让用户知道配置没生效
/// （admin 热更路径用同一校验，失败 400 回显——两条路径语义一致）。
fn sanitize_error_messages_table(
    table: std::collections::HashMap<String, model::error_messages::ErrorMessageOverride>,
) -> std::collections::HashMap<String, model::error_messages::ErrorMessageOverride> {
    if let Err(e) = admin::validate_error_messages(&table) {
        tracing::warn!(
            "errorMessages 配置校验失败，已降级为空表（全部走内置默认，配置未生效）: {e}"
        );
        return std::collections::HashMap::new();
    }
    table
}

/// 用量明细保留天数下限钳到 0。
///
/// 负值会让 `trace_db::retention_cleanup` 的 cutoff 落到未来
/// （`DELETE WHERE ts_ms < now - keep_days*86400000`，负 keep_days → cutoff 在未来）
/// → 启动即清空整张 traces 表，且 interval 首 tick 立即触发。与
/// `admin::service::cleanup_traces` 的 `.max(0)` 口径一致。
fn clamp_retention_days(days: i64) -> i64 {
    days.max(0)
}

/// 装配用量统计管道：打开 SQLite、构造 JSONL 统计、冷启动重放、启动保留清理任务。
///
/// 任一 sink 初始化失败都不致命——记录告警并退化（返回 None 或跳过该 sink），
/// 保证统计侧故障绝不阻断主服务启动。
fn init_usage_pipeline(config: &Config) -> Option<UsageHandles> {
    // 用量库目录：默认相对值 "data/usage" 在 Windows 数据隔离下前缀到 KiroStudio-data/，
    // 避免双击时按 cwd 散落。显式改成绝对/自定义路径的：尊重不动。非 Windows：保持原相对 cwd 语义。
    let data_dir = resolve_usage_data_dir(&config.usage_data_dir);
    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        tracing::error!(
            "创建用量数据目录失败 {}: {}，用量统计已禁用",
            data_dir.display(),
            e
        );
        return None;
    }

    // trace_db：SQLite 明细
    let trace_db = match TraceDb::open(&data_dir.join("traces.db")) {
        Ok(db) => Arc::new(db),
        Err(e) => {
            tracing::error!("打开用量 SQLite 失败: {:#}，用量统计已禁用", e);
            return None;
        }
    };

    // usage_stats：JSONL + 内存预聚合，冷启动重放最近日志恢复聚合
    let stats = Arc::new(UsageStats::new(data_dir.clone()));
    stats.rebuild_from_logs();

    // 注册进异步管道（trait 对象，供 worker 分发）
    usage::init_pipeline(vec![
        trace_db.clone() as Arc<dyn usage::UsageSink>,
        stats.clone() as Arc<dyn usage::UsageSink>,
    ]);

    // 保留清理任务：启动清理一次 + 每 6 小时清理一次过期明细
    let retention_days = clamp_retention_days(config.usage_retention_days);
    let cleanup_db = trace_db.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(6 * 3600));
        loop {
            ticker.tick().await;
            match cleanup_db.retention_cleanup(retention_days) {
                Ok(n) if n > 0 => tracing::info!("用量明细保留清理：删除 {} 条过期记录", n),
                Ok(_) => {}
                Err(e) => tracing::warn!("用量明细保留清理失败: {:#}", e),
            }
        }
    });

    // 客户端/窗口聚合定时回收：by_session/by_client/session_meta/client_sessions
    // 的 key 是客户端可控的 session_id（UUID）/ client_ip，原先仅靠概览页查询时
    // 惰性 prune。若长时间无人打开概览页，这些 map 会随不断变化的 session 无界增长
    // （中高危内存泄漏）。每 5 分钟主动回收一次窗口外的条目。
    // interval 用 Skip 防止唤醒后连刷；纯内存操作，零上游调用（不增加上游限流风险）。
    let cleanup_stats = stats.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(5 * 60));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let (sessions, clients) = cleanup_stats.cleanup_client_stats();
            tracing::debug!(
                "用量客户端聚合回收完成：存活 session={} client={}",
                sessions,
                clients
            );
        }
    });

    tracing::info!(
        "用量统计已启用：目录={} 保留={}天",
        data_dir.display(),
        retention_days
    );
    Some(UsageHandles { stats, trace_db })
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;
    use std::time::Duration;

    /// ⭐ 回归：负的 `usage_retention_days` 必须被钳到 0，否则 `retention_cleanup` 的
    /// cutoff 落到未来 → DELETE WHERE ts_ms < 未来 → 启动即清空整张 traces 表。
    /// 回退即 FAIL：把 `clamp_retention_days` 里的 `.max(0)` 去掉（或改负值直接透传）。
    #[test]
    fn clamp_retention_days_never_negative() {
        assert_eq!(clamp_retention_days(-30), 0, "负值必须钳到 0，否则清空全部 traces");
        assert_eq!(clamp_retention_days(-1), 0);
        assert_eq!(clamp_retention_days(0), 0);
        assert_eq!(clamp_retention_days(30), 30, "正常保留天数不得被改动");
        assert_eq!(clamp_retention_days(1), 1);
    }

    /// 测试用宽限期：毫秒级，走真实时钟（避免为一个测试引入 tokio `test-util` feature）。
    const TEST_CAP: Duration = Duration::from_millis(120);

    /// ⭐ 回归（#22）：在途请求 drain 完毕后必须**立刻**返回，不得白等满宽限期。
    ///
    /// 旧实现把 `sleep(SHUTDOWN_DRAIN_CAP_SECS)` 放在 shutdown future 里，而
    /// `with_graceful_shutdown` 的语义是「该 future 完成 ⇒ 停止接新连接」——
    /// 之后 `serve().await` 仍**无上限**地等。于是注释承诺的两件事一件都不成立，
    /// 其中一条就是：在途请求早已 drain 完也白等满 8 秒（部署窗口凭空变长）。
    ///
    /// 把竞速换回「先睡满 cap 再返回」→ 本测试必 FAILED。
    #[tokio::test]
    async fn drained_early_returns_immediately() {
        let notify = Arc::new(tokio::sync::Notify::new());
        notify.notify_one(); // 信号已到（模拟 SIGTERM），宽限期开始计时
        let began = std::time::Instant::now();
        let serve = async { Ok(()) }; // serve 立刻返回：在途请求已 drain 完
        let out = race_serve_against_drain_cap(serve, notify, TEST_CAP).await;
        assert!(
            matches!(out, DrainOutcome::Drained(Ok(()))),
            "应判为自然 drain 完成"
        );
        assert!(
            began.elapsed() < TEST_CAP,
            "drain 完成后必须立刻返回，实际等了 {:?}（>= 宽限期 {:?} 即回归）",
            began.elapsed(),
            TEST_CAP
        );
    }

    /// ⭐ 回归（#22）：长流式 SSE 挂住时必须在宽限期到点后**真的**放弃等待。
    ///
    /// 这是线上 74 秒停服 / 单次部署 167 次 502 的成因：`serve().await` 无上限，
    /// systemd 只能等到 TimeoutStopSec 超时再 SIGKILL。
    #[tokio::test]
    async fn cap_reached_when_serve_hangs() {
        let notify = Arc::new(tokio::sync::Notify::new());
        notify.notify_one();
        // serve 永挂（模拟长流式 SSE 不结束）
        let serve = async {
            std::future::pending::<()>().await;
            Ok(())
        };
        let out = race_serve_against_drain_cap(serve, notify, TEST_CAP).await;
        assert!(
            matches!(out, DrainOutcome::CapReached),
            "serve 挂住时必须在宽限期到点后放弃等待并断开残余连接"
        );
    }

    /// ⭐ 承重回归（#22）：**没有**停机信号时宽限期不得起算。
    ///
    /// 若把竞速分支里的 `notified().await` 去掉（直接 sleep 上限），服务会在启动
    /// `SHUTDOWN_DRAIN_CAP_SECS` 秒后**自己退出** —— 比原缺陷严重得多。
    /// 去掉那个 await → 本测试必 FAILED。
    #[tokio::test]
    async fn cap_does_not_start_without_shutdown_signal() {
        let notify = Arc::new(tokio::sync::Notify::new());
        // 刻意**不** notify：模拟进程正常服务、无人发信号
        let serve = async {
            // 远超宽限期后才返回，模拟"跑了一段时间的正常服务"
            tokio::time::sleep(TEST_CAP * 3).await;
            Ok(())
        };
        let out = race_serve_against_drain_cap(serve, notify, TEST_CAP).await;
        assert!(
            matches!(out, DrainOutcome::Drained(Ok(()))),
            "无停机信号时宽限期不得起算，否则服务会在启动 {SHUTDOWN_DRAIN_CAP_SECS}s 后自杀"
        );
    }
}

#[cfg(test)]
mod error_messages_boot_tests {
    use super::*;

    /// M1：启动播种校验——非法表必须降级为空表（不阻塞启动但配置不生效）。
    /// 用例① status 白名单外（不依赖默认表形态，最稳）；② 渲染值组合违例
    /// （B1 同款 status-only 绕过，动态取默认表 key，抗并行重写）。
    #[test]
    fn boot_sanitize_drops_invalid_table_to_empty() {
        let mut bad = std::collections::HashMap::new();
        bad.insert(
            "quota_exhausted".to_string(),
            model::error_messages::ErrorMessageOverride {
                status: Some(418),
                r#type: None,
                message: None,
                retry_after_secs: None,
            },
        );
        let out = sanitize_error_messages_table(bad);
        assert!(out.is_empty(), "白名单外 status 必须降级为空表");

        let base = model::error_messages::default_error_messages()
            .iter()
            .find(|(_, s, t, ..)| *s == 429 && *t == "rate_limit_error")
            .map(|(k, ..)| k.to_string())
            .expect("默认表必须保留至少一个 429+rate_limit_error 的 key（启动校验基线）");
        let mut bad2 = std::collections::HashMap::new();
        bad2.insert(
            base,
            model::error_messages::ErrorMessageOverride {
                status: Some(401),
                r#type: None,
                message: None,
                retry_after_secs: None,
            },
        );
        let out2 = sanitize_error_messages_table(bad2);
        assert!(out2.is_empty(), "渲染值组合违例必须降级为空表");
    }

    /// M1：合法表必须原样保留（只改 message 等合法姿势不误伤）。
    #[test]
    fn boot_sanitize_keeps_valid_table() {
        let mut good = std::collections::HashMap::new();
        good.insert(
            "quota_exhausted".to_string(),
            model::error_messages::ErrorMessageOverride {
                status: Some(429),
                r#type: Some("rate_limit_error".to_string()),
                message: Some("配额已耗尽，请稍后重试。".to_string()),
                retry_after_secs: None,
            },
        );
        let out = sanitize_error_messages_table(good.clone());
        assert_eq!(out, good, "合法表必须原样保留");
    }
}

// ==================== B7 main 侧播种点：源码守卫 ====================
#[cfg(test)]
mod main_seeding_guard {
    // 守卫纪律（CLAUDE.md 教训 #9）：本模块注释里不得出现带引号括号的完整调用字面量。

    /// 每个 main 直播种点（setter 在别的模块）必须在调用后登记接线标记。
    /// 删掉任一登记行 / 名字不同步 → 红。
    #[test]
    fn main_seeding_source_guard() {
        let full = include_str!("main.rs");
        let prod = full.split("\n#[cfg(test)]").next().unwrap_or(full);
        for name in super::MAIN_SEEDED_NAMES {
            let needle = format!("mark_main_seeded(\"{}\"{}", name, ")");
            assert!(
                prod.contains(&needle),
                "main 播种点登记缺失: {needle} 不存在于生产代码（MAIN_SEEDED_NAMES 与播种点必须同步）"
            );
        }
        let mut sorted: Vec<&str> = super::MAIN_SEEDED_NAMES.to_vec();
        sorted.sort_unstable();
        let deduped = sorted.clone();
        sorted.dedup();
        assert_eq!(sorted.len(), deduped.len(), "MAIN_SEEDED_NAMES 含重复名字");
    }

    /// 校验函数必须真的被 main 调用（删调用即红）。带缩进 + 分号的调用形态，
    /// 避免与函数定义行（fn 前缀无分号）误匹配。
    #[test]
    fn verify_called_in_main() {
        let full = include_str!("main.rs");
        let prod = full.split("\n#[cfg(test)]").next().unwrap_or(full);
        assert!(
            prod.contains("    verify_runtime_mirrors_wired();"),
            "main 必须调用启动播种自检"
        );
    }
}

// ==================== F6/D3-2 接线缺失告警联动 ====================
#[cfg(test)]
mod mirror_wiring_alert_tests {
    use super::*;

    /// 纯函数判定：播种位图与缺失清单一一对应。
    #[test]
    fn main_mirrors_missing_matches_bits() {
        assert_eq!(
            main_mirrors_missing(0),
            MAIN_SEEDED_NAMES.to_vec(),
            "位图全 0 时 4 个 main 镜像全部缺失"
        );
        assert!(
            main_mirrors_missing(u64::MAX).is_empty(),
            "位图全 1 时无缺失"
        );
        // 只播第 0 个：其余全部缺失、且已播的不在缺失清单里
        let got = main_mirrors_missing(1u64 << 0);
        assert_eq!(got.len(), MAIN_SEEDED_NAMES.len() - 1);
        assert!(
            !got.contains(&MAIN_SEEDED_NAMES[0]),
            "已播种的镜像不得出现在缺失清单"
        );
    }

    /// 端到端：启动自检发现接线缺失必须 bump `wiring_incomplete`，且 reason
    /// 携带缺失清单摘要。本地 HTTP server 收 payload 断言（同 alerting 测试模式）。
    ///
    /// 确定性依据：测试进程里 `main()` 不跑 → MAIN_SEEDED_BITS 恒 0 → 4 个 main
    /// 镜像恒缺失 → 必走缺失分支 → 必 bump（与 handlers 侧镜像的测试间状态无关）。
    ///
    /// 防自弱化：删掉 verify 缺失分支的 bump 行 → 本测试收不到投递 → 超时红。
    #[tokio::test]
    async fn verify_missing_mirrors_bumps_wiring_incomplete() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let _guard = crate::common::alerting::test_lock();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (body_tx, mut body_rx) = tokio::sync::mpsc::channel::<String>(4);
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 8192];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                if let Some(pos) = text.find("\r\n\r\n") {
                    let _ = body_tx.send(text[pos + 4..].to_string()).await;
                }
                let resp = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
                let _ = socket.write_all(resp.as_bytes()).await;
            }
        });

        crate::common::alerting::init(
            Some(format!("http://{addr}/hook")),
            3600,
            "test".to_string(),
        );

        verify_runtime_mirrors_wired();

        let body = tokio::time::timeout(std::time::Duration::from_secs(5), body_rx.recv())
            .await
            .expect("应收到告警投递")
            .expect("channel 不应关闭");
        let v: serde_json::Value = serde_json::from_str(&body).expect("payload 应为 JSON");
        assert_eq!(
            v["key"], "wiring_incomplete",
            "接线缺失必须 bump wiring_incomplete"
        );
        let reason = v["reason"].as_str().expect("reason 必须携带缺失清单摘要");
        for name in MAIN_SEEDED_NAMES {
            assert!(
                reason.contains(name),
                "reason 摘要必须包含缺失镜像名 {name}"
            );
        }
    }
}
