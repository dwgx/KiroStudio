mod admin;
mod admin_ui;
mod anthropic;
mod common;
mod http_client;
mod kiro;
mod model;
pub mod token;
mod usage;

use std::collections::HashMap;
use std::sync::Arc;

use clap::Parser;
use kiro::endpoint::{IdeEndpoint, KiroEndpoint};
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

#[tokio::main]
async fn main() {
    // 解析命令行参数
    let args = Args::parse();

    // 初始化日志
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // 加载配置
    let config_path = args
        .config
        .unwrap_or_else(|| Config::default_config_path().to_string());
    let config = Config::load(&config_path).unwrap_or_else(|e| {
        tracing::error!("加载配置失败: {}", e);
        std::process::exit(1);
    });

    // 加载凭证（支持单对象或数组格式）
    let credentials_path = args
        .credentials
        .unwrap_or_else(|| KiroCredentials::default_credentials_path().to_string());
    let credentials_config = CredentialsConfig::load(&credentials_path).unwrap_or_else(|e| {
        tracing::error!("加载凭证失败: {}", e);
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

    // 获取第一个凭据用于日志显示
    let first_credentials = credentials_list.first().cloned().unwrap_or_default();
    tracing::debug!("主凭证: {:?}", first_credentials);

    // 获取 API Key
    let api_key = config.api_key.clone().unwrap_or_else(|| {
        tracing::error!("配置文件中未设置 apiKey");
        std::process::exit(1);
    });

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

    // 构建端点注册表
    let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
    {
        let ide = IdeEndpoint::new();
        endpoints.insert(ide.name().to_string(), Arc::new(ide));
    }

    // 校验默认端点存在
    if !endpoints.contains_key(&config.default_endpoint) {
        tracing::error!("默认端点 \"{}\" 未注册", config.default_endpoint);
        std::process::exit(1);
    }

    // 校验所有凭据声明的端点都已注册
    for cred in &credentials_list {
        let name = cred
            .endpoint
            .as_deref()
            .unwrap_or(&config.default_endpoint);
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

    let endpoint_names: Vec<String> = endpoints.keys().cloned().collect();

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
    if config.proactive_token_refresh {
        kiro::refresh_loop::spawn(
            token_manager.clone(),
            config.token_refresh_lead_minutes,
            config.token_refresh_interval_secs,
        );
    } else {
        tracing::info!("主动 token 预刷新未启用（proactive_token_refresh=false）");
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
            }
        });
    }

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

    // 构建 Anthropic API 路由（profile_arn 由 provider 层根据实际凭据动态注入）
    let anthropic_app = anthropic::create_router_with_provider(
        &api_key,
        Some(kiro_provider),
        config.extract_thinking,
        config.prompt_cache_enabled,
        config.prompt_cache_ttl_seconds,
        &config.cors_allowed_origins,
        config.max_body_bytes,
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
            let admin_service =
                admin::AdminService::new(token_manager.clone(), endpoint_names.clone());
            let mut admin_state = admin::AdminState::new(admin_key, admin_service);
            // 注入用量查询句柄（未启用统计时为 None，端点返回 503）
            if let Some(handles) = &usage_handles {
                admin_state = admin_state
                    .with_usage(handles.stats.clone(), handles.trace_db.clone());
            }
            let admin_app = admin::create_admin_router(admin_state);

            // 创建 Admin UI 路由
            let admin_ui_app = admin_ui::create_admin_ui_router();

            tracing::info!("Admin API 已启用");
            tracing::info!("Admin UI 已启用: /admin");
            anthropic_app
                .nest("/api/admin", admin_app)
                .nest("/admin", admin_ui_app)
        }
    } else {
        anthropic_app
    };

    // 启动服务器
    let addr = format!("{}:{}", config.host, config.port);
    tracing::info!("启动 Anthropic API 端点: {}", addr);
    tracing::info!("API Key: {}***", &api_key[..(api_key.len() / 2)]);
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

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    // into_make_service_with_connect_info 让中间件可通过 ConnectInfo 拿到对端 IP
    // with_graceful_shutdown：收到 SIGTERM/Ctrl-C 后停止接新连接，等在途请求（含 SSE 流）drain
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .unwrap();

    tracing::info!("服务已优雅停机");
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

    tokio::select! {
        _ = ctrl_c => tracing::info!("收到 Ctrl-C，开始优雅停机…"),
        _ = terminate => tracing::info!("收到 SIGTERM，开始优雅停机…"),
    }
}

/// 装配用量统计管道：打开 SQLite、构造 JSONL 统计、冷启动重放、启动保留清理任务。
///
/// 任一 sink 初始化失败都不致命——记录告警并退化（返回 None 或跳过该 sink），
/// 保证统计侧故障绝不阻断主服务启动。
fn init_usage_pipeline(config: &Config) -> Option<UsageHandles> {
    use std::path::PathBuf;

    let data_dir = PathBuf::from(&config.usage_data_dir);
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
    let retention_days = config.usage_retention_days;
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

    tracing::info!(
        "用量统计已启用：目录={} 保留={}天",
        data_dir.display(),
        retention_days
    );
    Some(UsageHandles {
        stats,
        trace_db,
    })
}
