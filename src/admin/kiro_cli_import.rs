//! 官方 kiro-cli 凭据复用（P1-1）：只读打开它的 `data.sqlite3`，从 `auth_kv` 取 token，
//! 转成上号请求走既有 `add_credential` 路径入池。
//!
//! ## 键名与字段拼写的确证来源（两处独立，非猜测）
//!
//! ① 官方 kiro-cli 2.30.0 二进制 strings（本机逆向产物 `evidence/strings-utf8.txt`）：
//!    `kirocli:odic:token` / `kirocli:social:token` / `kirocli:odic:device`；
//! ② kirocc 的 Go 实现（field-tested）：5 组 token key 优先级 + camelCase/snake_case 双拼
//!    + `expiresAt` 既可能是 epoch 也可能是字符串。
//!
//! ## 为什么按优先级「全部取出」而不是只取第一条
//!
//! kirocc 只取优先级最高的那一条（它服务单会话场景）。本仓是号池：DB 里可能存着多个账号
//! 的登录（换号登录会新增行），故按优先级顺序全部取出、按 token 去重后逐条入池；
//! 取不到的号在面板里可见、可删，不会静默丢。
//!
//! ## 只读与不落日志
//!
//! 连接以 `SQLITE_OPEN_READ_ONLY` 打开（不建库、不写 WAL、不动 CLI 状态）；token 原文只进
//! 内存，任何错误信息里都不出现（错误只带 key 名与行号）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// token key 优先级（高 → 低）。`odic` 是官方拼写（不是笔误），`oidc` 变体一并兼容
/// ——两种拼写在生态里都出现过，kirocc 也是两种都试。
pub(super) const TOKEN_KEYS: [&str; 5] = [
    "kirocli:social:token",
    "kirocli:odic:token",
    "kirocli:oidc:token",
    "codewhisperer:odic:token",
    "codewhisperer:oidc:token",
];

/// 官方 IdC device 注册 key（`clientId`/`clientSecret` 的来源）。
///
/// 为什么需要它：本仓 `refresh_idc_token` 要求 clientId + clientSecret（AWS SSO OIDC 的
/// `register_client` 产物），而 token JSON 里没有这两项。取不到时仍导入（access token 有效
/// 期内可用），但在响应里显式标注「刷新会失败」，不装作成功。
///
/// **键名实测修正（2026-10-14，真库取证）**：ACFarm 线上 kiro-cli 写的键是
/// `kirocli:odic:device-registration`（3805 字节，字段 `client_id`/`client_secret`/
/// `client_secret_expires_at`/`oauth_flow`/`region`/`scopes`，全 snake_case）。
/// 原实现只认 `kirocli:odic:device` —— 那来自二进制 strings 的截断（新键名正是它的前缀），
/// 后果是**真库导入一律报「缺 clientId」而拒收**，而单测用同一个错键名自证，长期全绿。
/// 现按「新名 → 旧名 → 前缀兜底」三级取，容忍未来再改名。
const DEVICE_KEYS: [&str; 2] = [
    "kirocli:odic:device-registration",
    "kirocli:odic:device",
];

/// 官方 CLI 的 per-OS 默认库路径（`internal/config/config.go` 的约定）。
pub(super) fn default_db_path() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var_os("LOCALAPPDATA")?;
        Some(PathBuf::from(base).join("kiro-cli").join("data.sqlite3"))
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME")?;
        Some(
            PathBuf::from(home)
                .join("Library/Application Support/kiro-cli/data.sqlite3"),
        )
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let home = std::env::var_os("HOME")?;
        Some(PathBuf::from(home).join(".local/share/kiro-cli/data.sqlite3"))
    }
    #[cfg(not(any(target_os = "windows", unix)))]
    {
        // 官方 CLI 不支持的目标：让调用方显式传 dbPath。
        None
    }
}

/// 一条从 kiro-cli 库里读出的登录。
#[derive(Debug, Clone)]
pub(super) struct KiroCliToken {
    /// 来源 key（响应里回报，便于排障）。
    pub key: String,
    /// `social` / `idc`（由 key 里是否含 `:social:` 判定）。
    pub auth_method: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// 归一成 RFC3339（库里有 epoch 秒 / epoch 毫秒 / 字符串三种形态）。
    pub expires_at: Option<String>,
    pub region: Option<String>,
    pub profile_arn: Option<String>,
    /// 仅 IdC：来自 device 注册（best-effort）。
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
}

/// camelCase + snake_case 双拼的 token JSON。
#[derive(Debug, Default, serde::Deserialize)]
struct RawToken {
    #[serde(default, rename = "accessToken")]
    access_token: Option<String>,
    #[serde(default, rename = "refreshToken")]
    refresh_token: Option<String>,
    #[serde(default, rename = "expiresAt")]
    expires_at: Option<serde_json::Value>,
    #[serde(default)]
    region: Option<String>,
    #[serde(default, rename = "profileArn")]
    profile_arn: Option<String>,
    #[serde(default, rename = "access_token")]
    access_token_s: Option<String>,
    #[serde(default, rename = "refresh_token")]
    refresh_token_s: Option<String>,
    #[serde(default, rename = "expires_at")]
    expires_at_s: Option<serde_json::Value>,
    #[serde(default, rename = "profile_arn")]
    profile_arn_s: Option<String>,
}

/// camelCase + snake_case 双拼的 device 注册 JSON（形状未经实测，故一律 best-effort）。
#[derive(Debug, Default, serde::Deserialize)]
struct RawDevice {
    #[serde(default, rename = "clientId")]
    client_id: Option<String>,
    #[serde(default, rename = "clientSecret")]
    client_secret: Option<String>,
    #[serde(default, rename = "client_id")]
    client_id_s: Option<String>,
    #[serde(default, rename = "client_secret")]
    client_secret_s: Option<String>,
}

/// epoch（秒或毫秒）或字符串 → RFC3339。
///
/// 三种真实形态：数字 epoch 秒（绝大多数）、数字 epoch 毫秒（JS 侧写入）、
/// RFC3339 字符串。`> 1e12` 视为毫秒（1e12 秒 ≈ 公元 33658 年，不可能是秒）。
fn normalize_expires(v: Option<&serde_json::Value>) -> Option<String> {
    let v = v?;
    let secs = match v {
        serde_json::Value::Number(n) => {
            let f = n.as_f64()?;
            if f > 1e12 {
                f / 1000.0
            } else {
                f
            }
        }
        serde_json::Value::String(s) => {
            let s = s.trim();
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
                return Some(dt.with_timezone(&chrono::Utc).to_rfc3339());
            }
            let f: f64 = s.parse().ok()?;
            if f > 1e12 {
                f / 1000.0
            } else {
                f
            }
        }
        _ => return None,
    };
    if !secs.is_finite() || secs <= 0.0 {
        return None;
    }
    chrono::DateTime::from_timestamp(secs as i64, 0).map(|dt| dt.to_rfc3339())
}

/// 读库里的全部登录（按 [`TOKEN_KEYS`] 优先级排序）。
///
/// 错误一律是可读中文（路由层直接回 400/500 给面板），且**不含 token 原文**。
pub(super) fn read_tokens(db_path: &Path) -> Result<Vec<KiroCliToken>, String> {
    if !db_path.is_file() {
        return Err(format!(
            "kiro-cli 数据库不存在: {}（先在本机用官方 kiro-cli 登录一次，或显式传 dbPath）",
            db_path.display()
        ));
    }
    let conn = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|e| format!("只读打开 kiro-cli 数据库失败: {e}"))?;

    // 一条 IN 查完再按优先级挑（与 kirocc 同款；5 次查询没必要）。
    let placeholders = vec!["?"; TOKEN_KEYS.len()].join(",");
    let sql = format!("SELECT key, value FROM auth_kv WHERE key IN ({placeholders})");
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("读取 auth_kv 表失败（该库不是 kiro-cli 的？）: {e}"))?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(TOKEN_KEYS.iter()), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(|e| format!("查询 auth_kv 失败: {e}"))?;

    let mut by_key: HashMap<String, Vec<String>> = HashMap::new();
    for row in rows {
        let (k, v) = row.map_err(|e| format!("读取 auth_kv 行失败: {e}"))?;
        by_key.entry(k).or_default().push(v);
    }

    // device 注册：best-effort（缺失/形状不符都只是 None，不影响 token 导入）。
    let device = read_device(&conn);

    let mut out = Vec::new();
    let mut seen: Vec<(String, Option<String>)> = Vec::new();
    for key in TOKEN_KEYS {
        let Some(values) = by_key.get(key) else {
            continue;
        };
        for (idx, raw) in values.iter().enumerate() {
            let parsed: RawToken = match serde_json::from_str(raw) {
                Ok(p) => p,
                Err(e) => {
                    // 不打印 value（含 token 原文），只报 key 与行号。
                    tracing::warn!(key, idx, error = %e, "kiro-cli auth_kv 行不是合法 token JSON，跳过");
                    continue;
                }
            };
            let access_token = parsed
                .access_token
                .or(parsed.access_token_s)
                .filter(|s| !s.trim().is_empty());
            let Some(access_token) = access_token else {
                tracing::warn!(key, idx, "kiro-cli token 行缺 accessToken，跳过");
                continue;
            };
            let refresh_token = parsed.refresh_token.or(parsed.refresh_token_s);
            // 同一账号在同一 key 下可能有多行（换号登录的历史行）→ 按 (access, refresh) 去重。
            let sig = (access_token.clone(), refresh_token.clone());
            if seen.contains(&sig) {
                continue;
            }
            seen.push(sig);
            let is_social = key.contains(":social:");
            out.push(KiroCliToken {
                key: key.to_string(),
                auth_method: if is_social { "social" } else { "idc" }.to_string(),
                access_token,
                refresh_token,
                expires_at: normalize_expires(
                    parsed.expires_at.as_ref().or(parsed.expires_at_s.as_ref()),
                ),
                region: parsed.region,
                profile_arn: parsed.profile_arn.or(parsed.profile_arn_s),
                // device 注册只对 IdC 有意义；social 号不需要（刷新只要 refreshToken）。
                client_id: if is_social {
                    None
                } else {
                    device.as_ref().and_then(|d| d.0.clone())
                },
                client_secret: if is_social {
                    None
                } else {
                    device.as_ref().and_then(|d| d.1.clone())
                },
            });
        }
    }
    Ok(out)
}

/// 读 device 注册的 (clientId, clientSecret)（best-effort）。
///
/// 取值顺序：`DEVICE_KEYS`（新名 → 旧名）；都未命中时按 `kirocli:odic:device%` 前缀兜底
/// —— 官方再改键名时仍能捞到，且只接受能解析出 clientId 的行（前缀上的别的 JSON 不会
/// 被误当注册）。
fn read_device(conn: &rusqlite::Connection) -> Option<(Option<String>, Option<String>)> {
    for key in DEVICE_KEYS {
        if let Some(v) = read_device_at(conn, key) {
            return Some(v);
        }
    }
    let mut stmt = conn
        .prepare(
            "SELECT value FROM auth_kv WHERE key LIKE 'kirocli:odic:device%' \
             ORDER BY length(key), key",
        )
        .ok()?;
    let mut rows = stmt.query([]).ok()?;
    while let Ok(Some(row)) = rows.next() {
        let Ok(raw) = row.get::<_, String>(0) else {
            continue;
        };
        if let Ok(d) = serde_json::from_str::<RawDevice>(&raw) {
            if d.client_id.is_some() || d.client_id_s.is_some() {
                return Some((d.client_id.or(d.client_id_s), d.client_secret.or(d.client_secret_s)));
            }
        }
    }
    None
}

/// 单个键取值 + 解析；解析失败返回 `None`，让调用方继续试下一级。
fn read_device_at(
    conn: &rusqlite::Connection,
    key: &str,
) -> Option<(Option<String>, Option<String>)> {
    let mut stmt = conn.prepare("SELECT value FROM auth_kv WHERE key = ?").ok()?;
    let raw: String = stmt.query_row([key], |r| r.get(0)).ok()?;
    let d: RawDevice = serde_json::from_str(&raw).ok()?;
    Some((d.client_id.or(d.client_id_s), d.client_secret.or(d.client_secret_s)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 只含 `auth_kv` 的临时库；drop 时自清（含断言失败 panic 的路径）。
    struct TempDb {
        path: PathBuf,
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            if let Some(dir) = self.path.parent() {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }

    fn make_db(rows: &[(&str, &str)]) -> TempDb {
        let dir = std::env::temp_dir().join(format!("kiro-cli-import-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.sqlite3");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute("CREATE TABLE auth_kv (key TEXT PRIMARY KEY, value TEXT)", [])
            .unwrap();
        for (k, v) in rows {
            conn.execute("INSERT INTO auth_kv (key, value) VALUES (?, ?)", [k, v])
                .unwrap();
        }
        drop(conn);
        TempDb { path }
    }

    #[test]
    fn reads_social_token_with_camel_case() {
        let db = make_db(&[(
            "kirocli:social:token",
            r#"{"accessToken":"a1","refreshToken":"r1","expiresAt":1800000000,"region":"us-east-1","profileArn":"arn:aws:codewhisperer:us-east-1:1:profile/X"}"#,
        )]);
        let toks = read_tokens(&db.path).unwrap();
        assert_eq!(toks.len(), 1);
        let t = &toks[0];
        assert_eq!(t.auth_method, "social");
        assert_eq!(t.access_token, "a1");
        assert_eq!(t.refresh_token.as_deref(), Some("r1"));
        assert_eq!(t.region.as_deref(), Some("us-east-1"));
        assert!(t.expires_at.as_deref().unwrap().starts_with("2027-01-"));
        assert_eq!(t.key, "kirocli:social:token");
    }

    #[test]
    fn reads_snake_case_and_epoch_millis() {
        let db = make_db(&[(
            "kirocli:odic:token",
            r#"{"access_token":"a2","refresh_token":"r2","expires_at":1800000000000}"#,
        )]);
        let toks = read_tokens(&db.path).unwrap();
        assert_eq!(toks.len(), 1);
        assert_eq!(toks[0].access_token, "a2");
        assert_eq!(toks[0].auth_method, "idc");
        // 毫秒被归一成与秒同一天（2027-01-15T08:00:00Z）
        assert_eq!(
            toks[0].expires_at.as_deref(),
            Some("2027-01-15T08:00:00+00:00")
        );
    }

    #[test]
    fn priority_order_social_before_odic() {
        let db = make_db(&[
            ("kirocli:odic:token", r#"{"accessToken":"idc-a","refreshToken":"idc-r"}"#),
            ("kirocli:social:token", r#"{"accessToken":"soc-a","refreshToken":"soc-r"}"#),
        ]);
        let toks = read_tokens(&db.path).unwrap();
        assert_eq!(toks.len(), 2);
        assert_eq!(toks[0].access_token, "soc-a", "social 优先级更高");
        assert_eq!(toks[1].access_token, "idc-a");
    }

    #[test]
    fn idc_token_gets_device_registration() {
        let db = make_db(&[
            ("kirocli:odic:token", r#"{"accessToken":"a","refreshToken":"r"}"#),
            (
                "kirocli:odic:device",
                r#"{"clientId":"cid","clientSecret":"csec"}"#,
            ),
        ]);
        let toks = read_tokens(&db.path).unwrap();
        assert_eq!(toks[0].client_id.as_deref(), Some("cid"));
        assert_eq!(toks[0].client_secret.as_deref(), Some("csec"));
    }

    /// **真库形状**（2026-10-14 从 ACFarm 线上 kiro-cli 的 `data.sqlite3` 抄回）：
    /// 键名是 `kirocli:odic:device-registration`、字段全 snake_case、带 extra 字段。
    /// 此前的单测用 `kirocli:odic:device` + camelCase 自证，与真实契约不符 ——
    /// 线上表现为「导入一律报缺 clientId 而拒收」。本测试钉住真实形状。
    #[test]
    fn idc_token_gets_device_registration_real_world_shape() {
        let db = make_db(&[
            (
                "kirocli:odic:token",
                r#"{"access_token":"a","refresh_token":"r","expires_at":1800000000,"region":"us-east-1","scopes":["sso:account:access"],"start_url":"https://view.awsapps.com/start"}"#,
            ),
            (
                "kirocli:odic:device-registration",
                r#"{"client_id":"cid-real","client_secret":"csec-real","client_secret_expires_at":1900000000,"oauth_flow":"device_code","region":"us-east-1","scopes":["sso:account:access"]}"#,
            ),
        ]);
        let toks = read_tokens(&db.path).unwrap();
        assert_eq!(toks.len(), 1);
        assert_eq!(toks[0].access_token, "a");
        assert_eq!(toks[0].refresh_token.as_deref(), Some("r"));
        assert_eq!(toks[0].region.as_deref(), Some("us-east-1"));
        assert_eq!(toks[0].client_id.as_deref(), Some("cid-real"));
        assert_eq!(toks[0].client_secret.as_deref(), Some("csec-real"));
    }

    /// 官方再改键名时，前缀兜底仍能捞到注册（且只认能解析出 clientId 的行）。
    #[test]
    fn device_registration_prefix_fallback() {
        let db = make_db(&[
            ("kirocli:odic:token", r#"{"accessToken":"a","refreshToken":"r"}"#),
            // 同前缀但无 clientId 的行：不应被误当注册。
            ("kirocli:odic:device-code", r#"{"userCode":"u","deviceCode":"d"}"#),
            (
                "kirocli:odic:device-registration-v2",
                r#"{"client_id":"cid-v2","client_secret":"csec-v2"}"#,
            ),
        ]);
        let toks = read_tokens(&db.path).unwrap();
        assert_eq!(toks[0].client_id.as_deref(), Some("cid-v2"));
        assert_eq!(toks[0].client_secret.as_deref(), Some("csec-v2"));
    }

    #[test]
    fn social_token_never_gets_device_registration() {
        let db = make_db(&[
            ("kirocli:social:token", r#"{"accessToken":"a","refreshToken":"r"}"#),
            ("kirocli:odic:device", r#"{"clientId":"cid","clientSecret":"csec"}"#),
        ]);
        let toks = read_tokens(&db.path).unwrap();
        assert!(toks[0].client_id.is_none(), "social 刷新不需要 client 注册");
    }

    #[test]
    fn duplicate_tokens_are_deduped() {
        let db = make_db(&[
            ("kirocli:social:token", r#"{"accessToken":"dup","refreshToken":"r"}"#),
            ("kirocli:odic:token", r#"{"accessToken":"dup","refreshToken":"r"}"#),
        ]);
        let toks = read_tokens(&db.path).unwrap();
        assert_eq!(toks.len(), 1, "同 (access, refresh) 只入池一次");
    }

    #[test]
    fn missing_db_is_readable_error() {
        let db = make_db(&[]);
        let gone = db.path.parent().unwrap().join("nope.sqlite3");
        let err = read_tokens(&gone).unwrap_err();
        assert!(err.contains("不存在"), "错误必须可读: {err}");
    }

    #[test]
    fn non_kiro_db_reports_auth_kv_missing() {
        let dir = std::env::temp_dir().join(format!("kiro-cli-import-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.sqlite3");
        rusqlite::Connection::open(&path).unwrap();
        let err = read_tokens(&path).unwrap_err();
        assert!(err.contains("auth_kv"), "应点名缺表: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_row_is_skipped_not_fatal() {
        let db = make_db(&[
            ("kirocli:social:token", "not-json"),
            ("kirocli:odic:token", r#"{"accessToken":"ok"}"#),
        ]);
        let toks = read_tokens(&db.path).unwrap();
        assert_eq!(toks.len(), 1);
        assert_eq!(toks[0].access_token, "ok");
    }

    #[test]
    fn row_without_access_token_is_skipped() {
        let db = make_db(&[("kirocli:social:token", r#"{"refreshToken":"r"}"#)]);
        let toks = read_tokens(&db.path).unwrap();
        assert!(toks.is_empty());
    }
}
