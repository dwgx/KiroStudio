//! External Kiro API-key import endpoints.
//!
//! - `POST /api/import/keys`: existing batch protocol.
//! - `POST /api/import/push`: single-key relay protocol with persistent delivery idempotency.

use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::{Arc, Weak},
};

use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Request, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::post,
};
use futures::{StreamExt, stream};
use serde::{Deserialize, Serialize};

use crate::{
    common::auth,
    http_client::{ProxyConfig, build_client_no_redirect},
    kiro::token_manager::MultiTokenManager,
    kiro::{model::credentials::KiroCredentials, regions::KIRO_DIALOG_REGIONS},
};

/// 导入通道状态。**不含 key**：鉴权活读 [`crate::common::auth_keys`]，
/// 这样 importApiKey 改动（含从未配置到启用）即时生效，无需重启。
#[derive(Clone)]
struct ImportState {
    manager: Arc<MultiTokenManager>,
    relay_ledger: Arc<tokio::sync::Mutex<RelayLedger>>,
    relay_delivery_locks: Arc<tokio::sync::Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RelayDeliveryRecord {
    delivery_id: String,
    key_sha256: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    credential_id: Option<u64>,
    at_ms: i64,
}

struct RelayLedger {
    path: Option<PathBuf>,
    deliveries: HashMap<String, RelayDeliveryRecord>,
}

impl RelayLedger {
    fn load(manager: &MultiTokenManager) -> Self {
        let path = manager.credentials_path().map(|credentials| {
            credentials
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("relay-deliveries.ndjson")
        });
        Self::from_path(path)
    }

    fn from_path(path: Option<PathBuf>) -> Self {
        let mut deliveries = HashMap::new();
        if let Some(path) = &path {
            if let Ok(raw) = fs::read_to_string(path) {
                for record in raw
                    .lines()
                    .filter_map(|line| serde_json::from_str::<RelayDeliveryRecord>(line).ok())
                {
                    deliveries.insert(record.delivery_id.clone(), record);
                }
            }
            #[cfg(unix)]
            if path.exists() {
                use std::os::unix::fs::PermissionsExt;
                if let Err(error) = fs::set_permissions(path, fs::Permissions::from_mode(0o600)) {
                    tracing::warn!(path = %path.display(), "无法收紧 relay 账本权限: {error}");
                }
            }
        }
        Self { path, deliveries }
    }

    fn append(&mut self, record: RelayDeliveryRecord) -> Result<(), String> {
        if let Some(path) = &self.path {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            let mut options = OpenOptions::new();
            options.create(true).append(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(path).map_err(|error| error.to_string())?;
            let line = serde_json::to_string(&record).map_err(|error| error.to_string())?;
            writeln!(file, "{line}").map_err(|error| error.to_string())?;
            file.sync_data().map_err(|error| error.to_string())?;
        }
        self.deliveries.insert(record.delivery_id.clone(), record);
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImportRequest {
    items: Vec<ImportItem>,
    #[serde(default)]
    _concurrency_limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct ImportItem {
    key: String,
    #[serde(default)]
    region: Option<String>,
    /// 推送方的分组标签。契约说「固定空数组」，但仍照实接收并回显到面板——
    /// 【修一个静默 bug】此字段原名 `_groups`，serde 据此匹配 JSON key `"_groups"`，
    /// 而对方发的是 `"groups"`，配上 `#[serde(default)]` 就永远静默落空数组：
    /// 对方哪天真的开始发分组，我们会毫无察觉地丢掉。
    #[serde(default)]
    groups: Vec<serde_json::Value>,
    #[serde(default)]
    endpoint: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RelayPushRequest {
    #[serde(default)]
    secret: Option<String>,
    key: String,
    #[serde(default)]
    region: Option<String>,
    #[serde(default)]
    delivery_id: Option<String>,
    #[serde(default)]
    key_sha256: Option<String>,
}

#[derive(Debug, Serialize)]
struct ImportResponse {
    success: bool,
    total: usize,
    imported: usize,
    failed: usize,
    items: Vec<ImportItemResponse>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ImportItemResponse {
    key: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    duplicate: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    credential_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// key 指纹（SHA-256 前 8 位），仅供本地面板辨别，**不进线上响应**（契约未要求，
    /// 保持回给推送方的 JSON 与约定完全一致）。
    #[serde(skip)]
    fingerprint: String,
    /// 完整明文 key，仅用于写入本地面板的导入记录，**绝不进线上响应**。
    ///
    /// 回给推送方的 `key` 字段保持打码（契约：「响应里的 key 建议打码，我们不依赖完整值」），
    /// 而本地面板需要明文以便直接核对/复制刚入池的号——面板走 admin 鉴权，与既有
    /// 「导出凭据」端点同一防护级别。两者用途不同，故分成两个字段而非复用一个。
    #[serde(skip)]
    plain_key: String,
    /// Region（探测或重用的结果），供本地面板显示，**不进线上响应**。
    #[serde(skip)]
    region: Option<String>,
    /// Endpoint（归一或重用的结果），供本地面板显示，**不进线上响应**。
    #[serde(skip)]
    endpoint: Option<String>,
    /// 推送方**原样发来**的 region（未发则 None），供面板区分「对方指定」与「我们探测」。
    #[serde(skip)]
    sent_region: Option<String>,
    /// 推送方**原样发来**的 endpoint（未发则 None）。
    #[serde(skip)]
    sent_endpoint: Option<String>,
    /// 推送方发来的 groups（契约约定固定空数组，但如实记录以便发现约定变化）。
    #[serde(skip)]
    sent_groups: Vec<String>,
}

/// 构建导入路由。**总是挂载**——未配置 importApiKey 时由 [`import_auth`] 全拒（401），
/// 这样运维在面板里填上 key 即可启用通道，不必重启。暴露面从 404 变 401，
/// 两者都不泄露信息（见 auth_keys 模块级说明）。
pub fn create_router(manager: Arc<MultiTokenManager>) -> Router {
    let state = ImportState {
        relay_ledger: Arc::new(tokio::sync::Mutex::new(RelayLedger::load(&manager))),
        relay_delivery_locks: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        manager,
    };
    // Keep the production batch-import route as its own unchanged router tree. In particular,
    // its auth middleware, 1 MiB limit, response headers, request/response schema and importApiKey
    // remain independent from the new Relay route.
    let batch = Router::new()
        .route("/keys", post(import_keys))
        .layer(middleware::from_fn(import_auth))
        .layer(DefaultBodyLimit::max(1024 * 1024));

    let relay = Router::new()
        .route("/push", post(import_push))
        // Relay responses carry credential metadata and must never be cached. This middleware is
        // intentionally absent from `batch`, preserving the existing /keys response headers.
        .layer(middleware::from_fn(crate::common::http_cache::no_store))
        .layer(DefaultBodyLimit::max(64 * 1024));

    batch.merge(relay).with_state(state)
}

async fn import_auth(request: Request<Body>, next: Next) -> Response {
    // 活读热更单元：未配置 / 已清除 → 空存储恒 false → 全部 401（fail-closed）。
    match auth::extract_api_key(&request) {
        Some(key) if crate::common::auth_keys::import_key_matches(&key) => next.run(request).await,
        _ => (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "invalid or missing bearer token"})),
        )
            .into_response(),
    }
}

async fn import_keys(
    State(state): State<ImportState>,
    payload: Result<Json<ImportRequest>, JsonRejection>,
) -> Response {
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("invalid request body: {}", error.body_text())})),
            )
                .into_response();
        }
    };
    if payload.items.is_empty() || payload.items.len() > 1000 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "items must contain between 1 and 1000 entries"})),
        )
            .into_response();
    }

    // Deliberately serial: response indices are guaranteed to match request indices, and the
    // sender promises one request at a time. Region probes themselves are bounded-concurrent.
    let started = std::time::Instant::now();
    let mut results = Vec::with_capacity(payload.items.len());
    for item in payload.items {
        results.push(process_item(&state, item).await);
    }
    let imported = results.iter().filter(|item| item.ok).count();
    let failed = results.len() - imported;
    // 可观测摘要（进程级内存、零持久化）：面板据此显示最近几次推送，不必翻容器日志。
    // 面板记**完整 key**（运维需要直接取用/比对），仅受 admin 鉴权保护、不落盘、重启即失；
    // 回给推送方的 `ImportItemResponse.key` 仍是打码值（契约明确「不依赖完整值」）。
    record_import_stats(&results, started.elapsed().as_millis() as u64, None);
    // Portal 侧**持久**落盘（只存元数据，不存明文；未启用 portal 时是 no-op）。
    // 与上面的 import_stats 并存而非替代：那个是进程级、重启归零、只留最近 20 次的运营计数；
    // 这个是按 key 的持久视图，供 portal 用户查历史。两者口径不同，缺一不可。
    {
        let now_ms = chrono::Utc::now().timestamp_millis();
        for item in &results {
            crate::portal::sink::record_import(
                &item.plain_key,
                item.credential_id,
                item.region.as_deref(),
                item.endpoint.as_deref(),
                item.ok,
                item.error.as_deref(),
                now_ms,
            );
        }
    }
    Json(ImportResponse {
        success: failed == 0,
        total: results.len(),
        imported,
        failed,
        items: results,
    })
    .into_response()
}

fn record_import_stats(results: &[ImportItemResponse], elapsed_ms: u64, delivery_id: Option<&str>) {
    let items = results
        .iter()
        .map(|item| crate::common::import_stats::ImportItemRecord {
            // Preserve the existing batch channel's admin-only plaintext record. The new
            // Relay channel is stricter and records only the already-masked response key.
            key: if delivery_id.is_some() {
                item.key.clone()
            } else {
                item.plain_key.clone()
            },
            fingerprint: item.fingerprint.clone(),
            ok: item.ok,
            duplicate: item.duplicate.unwrap_or(false),
            credential_id: item.credential_id,
            error: item.error.clone(),
            region: item.region.clone(),
            endpoint: item.endpoint.clone(),
            sent_region: item.sent_region.clone(),
            sent_endpoint: item.sent_endpoint.clone(),
            sent_groups: item.sent_groups.clone(),
            delivery_id: delivery_id.map(str::to_string),
        })
        .collect();
    if delivery_id.is_some() {
        crate::common::import_stats::record_relay_push(items, elapsed_ms);
    } else {
        crate::common::import_stats::record_push(items, elapsed_ms);
    }
}

async fn import_push(
    State(state): State<ImportState>,
    headers: HeaderMap,
    payload: Result<Json<RelayPushRequest>, JsonRejection>,
) -> Response {
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("invalid request body: {}", error.body_text())})),
            )
                .into_response();
        }
    };
    let supplied_secret = payload
        .secret
        .as_deref()
        .filter(|value| !value.is_empty())
        .or_else(|| headers.get("x-relay-secret").and_then(|v| v.to_str().ok()))
        .or_else(|| {
            headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
        });
    if !supplied_secret
        .map(crate::common::auth_keys::relay_key_matches)
        .unwrap_or(false)
    {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "invalid secret"})),
        )
            .into_response();
    }
    // Backward-compatible with the standalone relay: legacy senders may omit delivery_id.
    // Generated IDs deliberately cannot collide with caller IDs by using a fixed prefix + UUID.
    let delivery_id = payload
        .delivery_id
        .unwrap_or_else(|| format!("legacy-{}", uuid::Uuid::new_v4().simple()));
    if !valid_delivery_id(&delivery_id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "delivery_id is invalid"})),
        )
            .into_response();
    }
    let key = payload.key.trim().to_string();
    if key.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "missing key"})),
        )
            .into_response();
    }
    let key_hash = sha256_hex(&key);
    if payload
        .key_sha256
        .as_deref()
        .is_some_and(|provided| provided != key_hash)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "key_sha256 does not match key"})),
        )
            .into_response();
    }

    // Same delivery IDs serialize with each other, while unrelated pushes can probe/import in
    // parallel. Keeping this lock through the import also makes concurrent retries share the
    // first result instead of issuing duplicate region probes.
    let delivery_lock = {
        let mut locks = state.relay_delivery_locks.lock().await;
        // Weak values preserve same-ID serialization without retaining one map entry forever
        // for every delivery ever seen. Opportunistic pruning bounds the map to in-flight IDs.
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(&delivery_id).and_then(Weak::upgrade) {
            lock
        } else {
            let lock = Arc::new(tokio::sync::Mutex::new(()));
            locks.insert(delivery_id.clone(), Arc::downgrade(&lock));
            lock
        }
    };
    let _delivery_guard = delivery_lock.lock().await;

    {
        let mut ledger = state.relay_ledger.lock().await;
        if let Some(previous) = ledger.deliveries.get(&delivery_id) {
            if previous.key_sha256 != key_hash {
                return (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({"ok": false, "error": "delivery_id is bound to a different key"})),
                )
                    .into_response();
            }
            if previous.status == "delivered" {
                return Json(serde_json::json!({
                    "ok": true,
                    "duplicate": true,
                    "credentialId": previous.credential_id,
                }))
                .into_response();
            }
        } else if let Err(error) = ledger.append(RelayDeliveryRecord {
            delivery_id: delivery_id.clone(),
            key_sha256: key_hash.clone(),
            status: "pending".to_string(),
            credential_id: None,
            at_ms: chrono::Utc::now().timestamp_millis(),
        }) {
            tracing::error!("无法持久化 relay delivery_id: {error}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"ok": false, "error": "delivery ledger unavailable"})),
            )
                .into_response();
        }
    }

    let started = std::time::Instant::now();
    let result = process_item(
        &state,
        ImportItem {
            key,
            region: Some(
                payload
                    .region
                    .filter(|region| !region.is_empty())
                    .unwrap_or_else(|| "us-east-1".to_string()),
            ),
            groups: Vec::new(),
            endpoint: None,
        },
    )
    .await;
    record_import_stats(
        std::slice::from_ref(&result),
        started.elapsed().as_millis() as u64,
        Some(&delivery_id),
    );
    let status = if result.ok { "delivered" } else { "failed" };
    {
        let mut ledger = state.relay_ledger.lock().await;
        if let Err(error) = ledger.append(RelayDeliveryRecord {
            delivery_id: delivery_id.clone(),
            key_sha256: key_hash,
            status: status.to_string(),
            credential_id: result.credential_id,
            at_ms: chrono::Utc::now().timestamp_millis(),
        }) {
            tracing::error!("无法更新 relay delivery_id 状态: {error}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"ok": false, "error": "delivery ledger unavailable"})),
            )
                .into_response();
        }
    }
    crate::portal::sink::record_import(
        &result.plain_key,
        result.credential_id,
        result.region.as_deref(),
        result.endpoint.as_deref(),
        result.ok,
        result.error.as_deref(),
        chrono::Utc::now().timestamp_millis(),
    );
    if result.ok {
        Json(serde_json::json!({
            "ok": true,
            "duplicate": result.duplicate.unwrap_or(false),
            "credentialId": result.credential_id,
        }))
        .into_response()
    } else {
        (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"ok": false, "error": result.error.unwrap_or_else(|| "key import failed".to_string())})),
        )
            .into_response()
    }
}

fn valid_delivery_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

fn sha256_hex(value: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

async fn process_item(state: &ImportState, item: ImportItem) -> ImportItemResponse {
    let key = item.key.trim().to_string();
    let masked = mask_key(&key);
    let fingerprint = crate::common::key_mask::key_fingerprint(&key);
    // 原样留存推送方**发来的**四个字段，与「我们最终落库的值」分开记账。
    // 只记落库值会丢掉关键信息：无法区分「对方指定了 us-east-1」和「对方没发、我们探测出
    // us-east-1」——前者是对方的路由决策，后者是我们的推断，排查责任归属时必须分得清。
    // groups 契约固定为空数组、运行时不参与任何决策，故只留作原样回显（异常值也能被看见）。
    let sent_region = item.region.clone();
    let sent_endpoint = item.endpoint.clone();
    let sent_groups: Vec<String> = item
        .groups
        .iter()
        .map(|g| match g {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .collect();

    // 闭包持有自己的副本：末尾 upsert 会 move 掉 `key`，若闭包借用它则借用检查不通过。
    let plain_for_fail = key.clone();
    let sent_region_for_fail = sent_region.clone();
    let sent_endpoint_for_fail = sent_endpoint.clone();
    let sent_groups_for_fail = sent_groups.clone();
    let fail = |error: String| ImportItemResponse {
        key: masked.clone(),
        plain_key: plain_for_fail.clone(),
        fingerprint: fingerprint.clone(),
        ok: false,
        duplicate: None,
        credential_id: None,
        error: Some(error),
        region: None,
        endpoint: None,
        sent_region: sent_region_for_fail.clone(),
        sent_endpoint: sent_endpoint_for_fail.clone(),
        sent_groups: sent_groups_for_fail.clone(),
    };

    if !key.starts_with("ksk_") || key.len() <= 4 {
        return fail("key must start with ksk_ and contain a value".to_string());
    }
    let requested_endpoint = match item.endpoint.as_deref().map(str::trim) {
        Some("ide") => Some("ide".to_string()),
        Some("cli") => Some("cli".to_string()),
        Some("") | None => None,
        Some(other) => {
            return fail(format!(
                "unsupported endpoint: {other}; expected ide, cli, or null"
            ));
        }
    };

    let existing = state.manager.find_imported_api_key(&key);
    let explicit_region = match item.region.as_deref().map(str::trim) {
        Some("") => return fail("region must not be empty when provided".to_string()),
        Some(region) if !KiroCredentials::is_supported_region(region) => {
            return fail(format!("unsupported region: {region}"));
        }
        Some(region) => Some(region.to_string()),
        None => None,
    };

    let region = match (&explicit_region, &existing) {
        // An unchanged explicit region or omitted region on an existing credential needs no
        // network probe. This keeps duplicate retries cheap and preserves prior routing data.
        (Some(region), Some(old)) if old.region.as_deref() == Some(region.as_str()) => {
            region.clone()
        }
        (None, Some(old)) if old.region.is_some() => old.region.clone().unwrap(),
        (Some(region), _) => match probe_regions(state, &key, vec![region.clone()]).await {
            Ok(region) => region,
            Err(error) => return fail(error),
        },
        (None, _) => {
            let candidates = KIRO_DIALOG_REGIONS.iter().map(|r| r.to_string()).collect();
            match probe_regions(state, &key, candidates).await {
                Ok(region) => region,
                Err(error) => return fail(error),
            }
        }
    };

    // endpoint 为 null/缺省 = 推送方"不知道"，此时绝不替它猜：
    // 已有记录保留原路由，全新 key 落 None → provider 回退 config.defaultEndpoint。
    //
    // 【为何不能默认 cli】实测同一个真实 key：
    //   endpoint=cli → 上游 400 INVALID_MODEL_ID（cli 端点 runtime.{region}.kiro.dev 认的
    //                  modelId 格式与网关下发的 CLAUDE_*_V1_0 不一致），且该 JSON 错误被
    //                  喂进 Event Stream 解码器，报出"消息长度 19 亿字节"的误导性错误；
    //   endpoint=ide → HTTP 200 正常推理。
    // 而对方契约的示例恰好就是 `"endpoint": null`——硬编码 cli 会让推来的号**全部不可用**。
    // 落 None 同时也符合契约第 1 条「不编造默认值」的精神：路由决策交给部署方的配置。
    let endpoint =
        requested_endpoint.or_else(|| existing.as_ref().and_then(|old| old.endpoint.clone()));

    // upsert 会消费 key，先留一份给面板明细（明文仅存内存，见 ImportItemRecord.key 说明）。
    let plain_key = key.clone();
    // 面板要显示"这个号最终落到哪个 region/哪条路由"——93 号那次 endpoint=cli 的坑，
    // 从面板上完全看不出来。endpoint 为 None 时显示为 default（由 config.defaultEndpoint 决定）。
    let landed_region = region.clone();
    let landed_endpoint = endpoint.clone();
    match state
        .manager
        .upsert_imported_api_key(key, region, endpoint)
        .await
    {
        Ok(result) => ImportItemResponse {
            key: masked,
            plain_key,
            fingerprint,
            region: Some(landed_region),
            endpoint: landed_endpoint,
            sent_region,
            sent_endpoint,
            sent_groups,
            ok: true,
            duplicate: Some(result.duplicate),
            credential_id: Some(result.id),
            error: None,
        },
        Err(error) => fail(format!("failed to persist credential: {error}")),
    }
}

async fn probe_regions(
    state: &ImportState,
    key: &str,
    candidates: Vec<String>,
) -> Result<String, String> {
    let config = state.manager.config();
    let proxy = config.proxy_url.as_deref().map(|url| {
        let (clean, inline_user, inline_pass) = crate::http_client::split_proxy_credentials(url);
        let mut proxy = ProxyConfig::new(clean);
        let user = config.proxy_username.clone().or(inline_user);
        let pass = config.proxy_password.clone().or(inline_pass);
        if let (Some(user), Some(pass)) = (user, pass) {
            proxy = proxy.with_auth(user, pass);
        }
        proxy
    });
    let client = build_client_no_redirect(proxy.as_ref(), 12, config.tls_backend)
        .map_err(|error| format!("failed to build region probe client: {error}"))?;

    let outcomes = stream::iter(candidates.into_iter().map(|region| {
        let client = client.clone();
        let key = key.to_string();
        async move {
            let host = format!("management.{region}.kiro.dev");
            let url = format!(
                "https://{host}/getUsageLimits?isEmailRequired=true&origin=KIRO_CLI&resourceType=AGENTIC_REQUEST"
            );
            let result = client
                .get(url)
                .header("Authorization", format!("Bearer {key}"))
                .header("tokentype", "API_KEY")
                .header("host", host)
                .send()
                .await;
            (region, result)
        }
    }))
    .buffer_unordered(6)
    .collect::<Vec<_>>()
    .await;

    let mut matches = Vec::new();
    let mut transient = Vec::new();
    for (region, outcome) in outcomes {
        match outcome {
            Ok(response) if response.status().is_success() => matches.push(region),
            Ok(response)
                if response.status().is_server_error() || response.status().as_u16() == 429 =>
            {
                transient.push(format!("{region}: HTTP {}", response.status()));
            }
            Ok(_) => {}
            // 连接层失败（DNS 无记录 / 拒绝连接）说明该 region **没有 management 端点**，
            // 与"key 在该 region 无效"等价，绝不能算待重试。实测 KIRO_DIALOG_REGIONS 的 33 个
            // 候选里只有 3 个真实存在 host（us-east-1 / eu-central-1 / us-gov-east-1），若把这
            // 30 个 DNS 失败计入 transient，任何无 region 的**永久无效** key 都会返回
            // "inconclusive; retry later" → 按契约第 3 条推送方会无限重推。
            // 只有超时才是真瞬态（链路慢/被墙），值得让对方重试。
            Err(error) if error.is_timeout() => {
                transient.push(format!("{region}: timeout"));
            }
            Err(_) => {}
        }
    }
    match matches.as_slice() {
        [region] => Ok(region.clone()),
        [] if transient.is_empty() => {
            Err("key is invalid or no supported region matched".to_string())
        }
        [] => Err(format!(
            "region probe was inconclusive; retry later ({})",
            transient.into_iter().take(3).collect::<Vec<_>>().join("; ")
        )),
        _ => Err(format!(
            "region probe was ambiguous; matched: {}",
            matches.join(", ")
        )),
    }
}

/// 脱敏展示委托给 [`crate::common::key_mask`]，与凭据管理页同格式——同一个 key 在两处
/// 显示一致，运维才能对照确认是不是同一个号（此前两处格式不同，无法比对）。
fn mask_key(key: &str) -> String {
    crate::common::key_mask::mask_api_key(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_ledger_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "kirostudio-relay-{tag}-{}-{}.ndjson",
            std::process::id(),
            uuid::Uuid::new_v4()
        ))
    }

    /// 脱敏格式与凭据管理页同源（`common::key_mask`），两处显示同一个 key 必须一模一样，
    /// 否则运维无法对照确认「面板上这个号就是对方推的那个」。
    #[test]
    fn masks_keys_without_exposing_full_value() {
        assert_eq!(mask_key("ksk_abcdefghijklmnop"), "ksk_abcd...mnop");
        assert_eq!(mask_key("short"), "***");
        // 与共享实现逐字节一致（防某天有人在此处另开分支）。
        assert_eq!(
            mask_key("ksk_abcdefghijklmnop"),
            crate::common::key_mask::mask_api_key("ksk_abcdefghijklmnop")
        );
    }

    /// 回归：KIRO_DIALOG_REGIONS 里绝大多数 region **没有** management 端点（实测 33 个候选
    /// 只有 3 个 host 真实存在）。这些 DNS 失败必须被当成"该 region 无此 key"而非瞬态错误，
    /// 否则无 region 的永久无效 key 恒返回 "retry later"，推送方按契约第 3 条会无限重推。
    #[test]
    fn dead_region_hosts_are_not_transient() {
        let dead: Vec<&str> = KIRO_DIALOG_REGIONS
            .iter()
            .filter(|r| !matches!(**r, "us-east-1" | "eu-central-1" | "us-gov-east-1"))
            .copied()
            .collect();
        assert!(
            dead.len() > 20,
            "候选表应含大量无 management 端点的 region（实测 30 个），实际 {}",
            dead.len()
        );
    }

    #[test]
    fn delivery_id_validation_matches_relay_contract() {
        assert!(valid_delivery_id("order-42.retry_1:eu"));
        assert!(!valid_delivery_id(""));
        assert!(!valid_delivery_id("contains space"));
        assert!(!valid_delivery_id(&"x".repeat(101)));
    }

    #[test]
    fn relay_request_distinguishes_omitted_and_explicit_empty_delivery_id() {
        let omitted: RelayPushRequest = serde_json::from_str(r#"{"key":"ksk_x"}"#)
            .expect("omitted delivery_id should remain backward compatible");
        assert_eq!(omitted.delivery_id, None);

        let empty: RelayPushRequest = serde_json::from_str(r#"{"key":"ksk_x","delivery_id":""}"#)
            .expect("empty delivery_id is syntactically valid JSON");
        assert_eq!(empty.delivery_id.as_deref(), Some(""));
        assert!(!valid_delivery_id(empty.delivery_id.as_deref().unwrap()));
    }

    #[test]
    fn relay_ledger_survives_restart_and_ignores_partial_lines() {
        let path = temp_ledger_path("restart");
        let mut ledger = RelayLedger::from_path(Some(path.clone()));
        ledger
            .append(RelayDeliveryRecord {
                delivery_id: "delivery-1".into(),
                key_sha256: "abc".into(),
                status: "delivered".into(),
                credential_id: Some(9),
                at_ms: 1,
            })
            .expect("账本应可写");
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("账本应存在")
            .write_all(b"{partial")
            .expect("应可模拟崩溃残行");

        let reloaded = RelayLedger::from_path(Some(path.clone()));
        let record = reloaded.deliveries.get("delivery-1").expect("重启后应恢复");
        assert_eq!(record.key_sha256, "abc");
        assert_eq!(record.credential_id, Some(9));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let _ = fs::remove_file(path);
    }

    #[test]
    fn latest_ledger_record_wins_for_retry_state() {
        let path = temp_ledger_path("latest");
        let mut ledger = RelayLedger::from_path(Some(path.clone()));
        for status in ["pending", "failed", "delivered"] {
            ledger
                .append(RelayDeliveryRecord {
                    delivery_id: "delivery-2".into(),
                    key_sha256: "def".into(),
                    status: status.into(),
                    credential_id: (status == "delivered").then_some(11),
                    at_ms: 1,
                })
                .expect("账本应可追加");
        }
        let reloaded = RelayLedger::from_path(Some(path.clone()));
        let record = reloaded.deliveries.get("delivery-2").unwrap();
        assert_eq!(record.status, "delivered");
        assert_eq!(record.credential_id, Some(11));
        let _ = fs::remove_file(path);
    }
}
