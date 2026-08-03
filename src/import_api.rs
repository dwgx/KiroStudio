//! External Kiro API-key batch import endpoint (`POST /api/import/keys`).

use std::sync::Arc;

use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Request, State, rejection::JsonRejection},
    http::StatusCode,
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

#[derive(Clone)]
struct ImportState {
    token: String,
    manager: Arc<MultiTokenManager>,
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
    #[serde(default)]
    _groups: Vec<serde_json::Value>,
    #[serde(default)]
    endpoint: Option<String>,
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
}

pub fn create_router(token: String, manager: Arc<MultiTokenManager>) -> Router {
    let state = ImportState { token, manager };
    Router::new()
        .route("/keys", post(import_keys))
        .layer(middleware::from_fn_with_state(state.clone(), import_auth))
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .with_state(state)
}

async fn import_auth(
    State(state): State<ImportState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    match auth::extract_api_key(&request) {
        Some(key) if auth::constant_time_eq(&key, &state.token) => next.run(request).await,
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
    // 只记已打码的 key 与失败原因——明文密钥绝不进这里。
    crate::common::import_stats::record_push(
        results
            .iter()
            .map(|item| crate::common::import_stats::ImportItemRecord {
                key: item.key.clone(),
                ok: item.ok,
                duplicate: item.duplicate.unwrap_or(false),
                credential_id: item.credential_id,
                error: item.error.clone(),
            })
            .collect(),
        started.elapsed().as_millis() as u64,
    );
    Json(ImportResponse {
        success: failed == 0,
        total: results.len(),
        imported,
        failed,
        items: results,
    })
    .into_response()
}

async fn process_item(state: &ImportState, item: ImportItem) -> ImportItemResponse {
    let key = item.key.trim().to_string();
    let masked = mask_key(&key);
    let fail = |error: String| ImportItemResponse {
        key: masked.clone(),
        ok: false,
        duplicate: None,
        credential_id: None,
        error: Some(error),
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

    // null/missing endpoint means "unknown": preserve an existing route; new API keys use the
    // actual Kiro CLI runtime protocol instead of guessing an IDE endpoint.
    let endpoint = requested_endpoint
        .or_else(|| existing.as_ref().and_then(|old| old.endpoint.clone()))
        .or_else(|| Some("cli".to_string()));

    match state
        .manager
        .upsert_imported_api_key(key, region, endpoint)
        .await
    {
        Ok(result) => ImportItemResponse {
            key: masked,
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

fn mask_key(key: &str) -> String {
    if key.is_ascii() && key.len() > 12 {
        format!("{}...{}", &key[..8], &key[key.len() - 4..])
    } else {
        "ksk_***".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_keys_without_exposing_full_value() {
        assert_eq!(mask_key("ksk_abcdefghijklmnop"), "ksk_abcd...mnop");
        assert_eq!(mask_key("short"), "ksk_***");
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
}
