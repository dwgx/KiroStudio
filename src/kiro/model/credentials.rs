//! Kiro OAuth 凭证数据模型
//!
//! 支持从 Kiro IDE 的凭证文件加载，使用 Social 认证方式
//! 支持单凭据和多凭据配置格式

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

use crate::http_client::ProxyConfig;
use crate::model::config::Config;

/// 已知的 AWS/Kiro region 白名单,用于严格校验 profileArn 里解析出的 region
/// (防污染 ARN 拼出坏 host)。含标准分区 + GovCloud + 中国分区。
/// Kiro 对话/余额端点 + profileArn 的合法 region 白名单。
/// 单一真相源见 [`crate::kiro::regions::KIRO_DIALOG_REGIONS`]（此处 re-export，调用点不变）。
use crate::kiro::regions::KIRO_DIALOG_REGIONS as SUPPORTED_KIRO_REGIONS;

/// Kiro OAuth 凭证
///
/// ⚠️ 安全：**刻意不派生 `Debug`**，改为手写脱敏实现（见下方 `impl Debug`）。
/// `access_token`/`refresh_token`/`client_secret`/`kiro_api_key`/`proxy_password`
/// 属可直接复用的活凭证，一旦被 `{:?}` 打进日志即等于泄露。派生 Debug 会输出全部
/// 明文——这里统一在类型层面脱敏，杜绝任何调用点（日志/错误链）意外泄密。
/// 一次「测试可用模型」探测对单个模型的结果记录（持久化，供下次进测试页展示历史）。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestedModel {
    /// 被测的 kiro modelId（如 `deepseek-3.2`）
    pub model: String,
    /// 结果：`supported` / `unsupported` / `unknown`
    pub status: String,
    /// 测试时刻（RFC3339）
    pub tested_at: String,
}

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct KiroCredentials {
    /// 凭据唯一标识符（自增 ID）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,

    /// 访问令牌
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(alias = "access_token")]
    pub access_token: Option<String>,

    /// 刷新令牌
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(alias = "refresh_token")]
    pub refresh_token: Option<String>,

    /// Profile ARN
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(alias = "profile_arn")]
    pub profile_arn: Option<String>,

    /// 过期时间 (RFC3339 格式)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(alias = "expires_at", alias = "expired")]
    pub expires_at: Option<String>,

    /// 认证方式 (social / idc)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(alias = "auth_method")]
    pub auth_method: Option<String>,

    /// OIDC Client ID (IdC 认证需要)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(alias = "client_id")]
    pub client_id: Option<String>,

    /// OIDC Client Secret (IdC 认证需要)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(alias = "client_secret")]
    pub client_secret: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(alias = "token_endpoint")]
    pub token_endpoint: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(alias = "issuer_url")]
    pub issuer_url: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub scopes: Option<String>,

    /// 凭据优先级（数字越小优先级越高，默认为 0）
    #[serde(default)]
    #[serde(skip_serializing_if = "is_zero")]
    pub priority: u32,

    /// 凭据级 RPM 容量上限（每分钟请求数，可选）。
    ///
    /// None/0 = 继承全局 `credential_rpm_limit`。设了就用本号自己的容量:体质好的号(如
    /// Enterprise)可设高(100),弱号设低。用于 balanced 选号的 per-号饱和判定 + 优先级备份溢出:
    /// 高优先级号近 60s RPM 接近**它自己的**容量时才判饱和、溢出分流给下一优先级备份号。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rpm_limit: Option<u32>,

    /// 凭据级「允许模型」白名单（成本安全硬门，可选）。
    ///
    /// None/空 = 不限制（该号可服务任意模型，兼容现有号）。设了就是**硬门**:该号**只**接
    /// 白名单内的模型（值为 map_model 后的规范 kiro modelId，如 `deepseek-3.2`/`claude-opus-4.8`）。
    /// 用途:把便宜模型（国产）的流量锁死在指定便宜号上——即使贵号也能跑国产，只要没在其白名单里
    /// 就绝不会被选中，杜绝便宜请求溢出到贵号按贵号计费。选号唯一收敛点 is_entry_selectable 据此过滤。
    /// ⚠️ 硬门语义:设太窄 + 号不够 → 该模型无号可用返错（防溢出优先于可用性，刻意如此）。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_models: Option<Vec<String>>,

    /// 「测试可用模型」历史结果（探测打的标签，持久化）。下次进测试页可直接看到该号测过什么、
    /// 结果如何，无需重测。每次探测覆盖写入。与 allowed_models 独立：这是"测过的事实"，
    /// allowed_models 是"准用的硬门"；UI 可把测出 supported 的一键设为白名单。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tested_models: Option<Vec<TestedModel>>,

    // ==== 自定义 API「代挂透传」字段（auth_method=custom_api 时用）====
    // 语义:该凭据是一个 Anthropic 兼容上游中转站,Claude Code 打 /v1/messages 时原样透传到
    // base_url + 换用 api_key。不做协议转换(入口=出口=Anthropic)。零 Kiro 字段依赖。
    /// 自定义 API 上游基址（如 https://api.anthropic-proxy.com,透传目标）。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,

    /// 自定义 API 密钥（透传时替换成它;**独立于 kiro_api_key**;已加入 Debug 脱敏清单）。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,

    /// 请求上限：累计请求数达到后自动禁用该凭据（None/0=不限）。通用字段,自定义API主用。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_limit: Option<u64>,

    /// 凭据级 Region 配置（用于 OIDC token 刷新）
    /// 未配置时回退到 config.json 的全局 region
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,

    /// 凭据级 Auth Region（用于 Token 刷新）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_region: Option<String>,

    /// 凭据级 API Region（用于 API 请求）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_region: Option<String>,

    /// 凭据级 Machine ID 配置（可选）
    /// 未配置时回退到 config.json 的 machineId；都未配置时由 refreshToken 派生
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(alias = "machine_id")]
    pub machine_id: Option<String>,

    /// 用户邮箱（从 Anthropic API 获取）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,

    /// 用户自定义别名/备注（管理员在面板设置，用于卡片展示，优先于 email/#id）。
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(alias = "alias")]
    pub name: Option<String>,

    /// 订阅等级（KIRO PRO+ / KIRO FREE 等）
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub subscription_title: Option<String>,

    /// 凭据级代理 URL（可选）
    /// 支持 http/https/socks5 协议
    /// 特殊值 "direct" 表示显式不使用代理（即使全局配置了代理）
    /// 未配置时回退到全局代理配置
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,

    /// 凭据级代理认证用户名（可选）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_username: Option<String>,

    /// 凭据级代理认证密码（可选）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_password: Option<String>,

    /// 凭据是否被禁用（默认为 false）
    #[serde(default)]
    pub disabled: bool,

    /// Kiro API Key（headless 模式）
    /// 格式: ksk_xxxxxxxx
    /// 设置后直接作为 Bearer Token 使用，无需 refreshToken
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kiro_api_key: Option<String>,

    /// 端点名称（可选）
    ///
    /// 决定该凭据走哪套 Kiro API。未配置时回退到 `config.defaultEndpoint`（默认 "ide"）。
    /// 端点名必须在启动时注册的端点 registry 中存在。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

/// 脱敏展示敏感字段：有值 → `"<set:N>"`（标注长度便于排障但不泄露内容），无值 → `None`。
fn mask_secret(v: &Option<String>) -> String {
    match v {
        Some(s) => format!("<set:{}>", s.len()),
        None => "None".to_string(),
    }
}

/// 代理 URL 脱敏：剥掉内嵌 `user:pass@` 段（旧数据/直接输入可能带），仅留 scheme+host。
/// 用 split_proxy_credentials 拆出干净 URL（内部已处理 scheme/最后一个@/IPv6）。
fn mask_proxy_url(url: &str) -> String {
    crate::http_client::split_proxy_credentials(url).0
}

/// 手写脱敏 Debug：可识别字段（id/auth/email/endpoint 等）正常显示，
/// 敏感凭证字段一律打码为 `<set:N>`，绝不输出明文。
impl std::fmt::Debug for KiroCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KiroCredentials")
            .field("id", &self.id)
            .field("auth_method", &self.auth_method)
            .field("email", &self.email)
            .field("endpoint", &self.endpoint)
            .field("priority", &self.priority)
            .field("disabled", &self.disabled)
            .field("region", &self.region)
            .field("auth_region", &self.auth_region)
            .field("api_region", &self.api_region)
            .field("expires_at", &self.expires_at)
            .field("subscription_title", &self.subscription_title)
            .field("has_profile_arn", &self.profile_arn.is_some())
            .field("machine_id", &self.machine_id)
            // proxy_url 可能残留内嵌账密（旧数据/直接输入）——Debug 脱敏 userinfo 段防泄漏。
            .field("proxy_url", &self.proxy_url.as_deref().map(mask_proxy_url))
            // —— 敏感字段一律脱敏 ——
            .field("access_token", &mask_secret(&self.access_token))
            .field("refresh_token", &mask_secret(&self.refresh_token))
            .field("client_id", &mask_secret(&self.client_id))
            .field("client_secret", &mask_secret(&self.client_secret))
            .field("token_endpoint", &self.token_endpoint)
            .field("issuer_url", &self.issuer_url)
            .field("scopes", &self.scopes)
            .field("kiro_api_key", &mask_secret(&self.kiro_api_key))
            .field("api_key", &mask_secret(&self.api_key))
            .field("base_url", &self.base_url)
            .field("proxy_username", &mask_secret(&self.proxy_username))
            .field("proxy_password", &mask_secret(&self.proxy_password))
            .finish()
    }
}

/// 判断是否为零（用于跳过序列化）
fn is_zero(value: &u32) -> bool {
    *value == 0
}

/// 回收站条目：软删除的凭据及其删除元数据
///
/// 删除凭据不再物理丢弃，而是包成 `TrashEntry` 移入独立回收站存储，
/// 可原样恢复（含 id/refreshToken 等全部字段）或彻底删除。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrashEntry {
    /// 被删除的完整凭据（含 id，可原样恢复回凭据池）
    pub credentials: KiroCredentials,
    /// 删除时间（RFC3339 格式）
    pub deleted_at: String,
    /// 删除前累计的 API 调用成功次数（恢复时一并还原）
    #[serde(default)]
    pub success_count: u64,
    /// 删除前的生命周期累计 credit 花费（恢复时一并还原；老回收站数据无此字段默认 0）
    #[serde(default)]
    pub total_credits_used: f64,
    /// 删除前最后一次 API 调用时间（恢复时一并还原）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<String>,
}

fn canonicalize_auth_method_value(value: &str) -> &str {
    if value.eq_ignore_ascii_case("builder-id") || value.eq_ignore_ascii_case("iam") {
        "idc"
    } else if value.eq_ignore_ascii_case("api_key") || value.eq_ignore_ascii_case("apikey") {
        "api_key"
    } else if value.eq_ignore_ascii_case("external-idp")
        || value.eq_ignore_ascii_case("externalidp")
        || value.eq_ignore_ascii_case("azure")
        || value.eq_ignore_ascii_case("azuread")
        || value.eq_ignore_ascii_case("azure_ad")
    {
        "external_idp"
    } else if value.eq_ignore_ascii_case("custom_api")
        || value.eq_ignore_ascii_case("customapi")
        || value.eq_ignore_ascii_case("custom-api")
        || value.eq_ignore_ascii_case("passthrough")
    {
        "custom_api"
    } else {
        value
    }
}

/// 凭据配置（支持单对象或数组格式）
///
/// 自动识别配置文件格式：
/// - 单对象格式（旧格式，向后兼容）
/// - 数组格式（新格式，支持多凭据）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CredentialsConfig {
    /// 单个凭据（旧格式）
    Single(KiroCredentials),
    /// 多凭据数组（新格式）
    Multiple(Vec<KiroCredentials>),
}

impl CredentialsConfig {
    /// 从文件加载凭据配置
    ///
    /// - 如果文件不存在，返回空数组
    /// - 如果文件内容为空，返回空数组
    /// - 支持单对象或数组格式
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let path = path.as_ref();

        // 文件不存在时返回空数组
        if !path.exists() {
            return Ok(CredentialsConfig::Multiple(vec![]));
        }

        // 读裸字节:文件可能是明文 JSON 或本机加密的密文信封。maybe_decrypt_to_string 按 magic
        // 前缀自动区分——明文直通、密文用机器绑定密钥解密。**必须先解密再交给 serde**,否则密文会
        // 被当明文解析报一堆看不懂的 JSON 错(且此路径失败会 exit(1))。
        let bytes = fs::read(path)?;

        // 文件为空时返回空数组
        if bytes.iter().all(|b| b.is_ascii_whitespace()) {
            return Ok(CredentialsConfig::Multiple(vec![]));
        }

        let key_path = crate::common::secret_store::key_path_for(path);
        let content = crate::common::secret_store::maybe_decrypt_to_string(&bytes, &key_path)?;

        // 解密/直通后再判空(密文解出来也可能是空数组的 JSON)。
        if content.trim().is_empty() {
            return Ok(CredentialsConfig::Multiple(vec![]));
        }

        let config = serde_json::from_str(&content)?;
        Ok(config)
    }

    /// 转换为按优先级排序的凭据列表
    pub fn into_sorted_credentials(self) -> Vec<KiroCredentials> {
        match self {
            CredentialsConfig::Single(mut cred) => {
                cred.canonicalize_auth_method();
                vec![cred]
            }
            CredentialsConfig::Multiple(mut creds) => {
                // 按优先级排序（数字越小优先级越高）
                creds.sort_by_key(|c| c.priority);
                for cred in &mut creds {
                    cred.canonicalize_auth_method();
                }
                creds
            }
        }
    }

    /// 判断是否为多凭据格式（数组格式）
    pub fn is_multiple(&self) -> bool {
        matches!(self, CredentialsConfig::Multiple(_))
    }
}

impl KiroCredentials {
    /// 特殊值：显式不使用代理
    pub const PROXY_DIRECT: &'static str = "direct";

    /// 获取默认凭证文件路径
    pub fn default_credentials_path() -> &'static str {
        "credentials.json"
    }

    /// 获取有效的 Auth Region（用于 Token 刷新）
    /// 优先级：凭据.auth_region > 凭据.region > config.auth_region > config.region
    /// 仅接受命中 SUPPORTED_KIRO_REGIONS 白名单的 region 字符串,否则 None。
    ///
    /// 安全(H3/M1):凭据的 region/auth_region/api_region 字段来自不可信来源——本工具核心
    /// 用途就是导入他人分享/第三方的 Kiro 凭据 JSON。这些字符串会被 format! 拼进上游 host
    /// (prod.{region}.auth.desktop.kiro.dev / runtime.{region}.kiro.dev / management.{region}.kiro.dev /
    /// oidc.{region}.amazonaws.com)。若不校验,污染值(如 "evil.com/")会拼出攻击者可控 host,
    /// 刷新时把明文 refresh_token/Bearer POST 到攻击者服务器 = 凭据外泄/中间人。
    /// 与 region_from_profile_arn 同口径:只放行已知 AWS region,不命中即回退 config。
    fn sanitized_region(region: &str) -> Option<&str> {
        if SUPPORTED_KIRO_REGIONS.contains(&region) {
            Some(region)
        } else {
            None
        }
    }

    /// region 是否在白名单内（供 admin 上号入口等外部校验用,如 idc region 参数）。
    pub fn is_supported_region(region: &str) -> bool {
        SUPPORTED_KIRO_REGIONS.contains(&region)
    }

    pub fn effective_auth_region<'a>(&'a self, config: &'a Config) -> &'a str {
        // 凭据 region 字段先过白名单(防污染拼坏 host),不命中回退 config。
        self.auth_region
            .as_deref()
            .and_then(Self::sanitized_region)
            .or_else(|| self.region.as_deref().and_then(Self::sanitized_region))
            .unwrap_or(config.effective_auth_region())
    }

    /// 获取有效的 API Region（用于 API 请求）
    /// 优先级：凭据.api_region（过白名单） > config.api_region > config.region
    pub fn effective_api_region<'a>(&'a self, config: &'a Config) -> &'a str {
        self.api_region
            .as_deref()
            .and_then(Self::sanitized_region)
            .unwrap_or(config.effective_api_region())
    }

    /// 上游 kiro.dev/CodeWhisperer 端点构建时,用于 region 解析优先级的**稳健**版:
    /// profileArn 第 4 段(严格校验) > 凭据 region/auth_region > config。
    ///
    /// 为何要严格校验而非裸 `split(':').nth(3)`:profileArn 可能被污染(存错值 / 非
    /// codewhisperer ARN / 臆造 region),裸取第 4 段会直接拼成坏 host(如
    /// `runtime.{垃圾}.kiro.dev`)导致 DNS 失败/502。故:①先校验 ARN 前缀
    /// (`arn:aws:codewhisperer:`)②再校验第 4 段命中已知 region 白名单,任一不过
    /// 就跳过 profileArn,回退凭据/config region。(白名单校验参考 kiro-account-manager。)
    pub fn effective_upstream_region<'a>(&'a self, config: &'a Config) -> &'a str {
        if let Some(region) = self
            .profile_arn
            .as_deref()
            .and_then(Self::region_from_profile_arn)
        {
            return region;
        }
        // 凭据 region/auth_region 字段先过白名单(H3:防污染值拼坏 host 泄漏 token),
        // 不命中回退 config（config 由部署方掌控,视为可信）。
        self.region
            .as_deref()
            .and_then(Self::sanitized_region)
            .or_else(|| self.auth_region.as_deref().and_then(Self::sanitized_region))
            .unwrap_or_else(|| self.effective_api_region(config))
    }

    // (安全回归测试 test_region_field_rejects_polluted 见 mod tests,验证污染 region 被白名单挡下)

    /// 从 profileArn 严格解析 region:必须形如
    /// `arn:aws:codewhisperer:{region}:{account}:profile/{id}` 且 region 命中白名单,
    /// 否则返回 None(交由上层回退)。
    pub fn region_from_profile_arn(arn: &str) -> Option<&str> {
        let mut segs = arn.split(':');
        if segs.next()? != "arn" {
            return None;
        }
        segs.next()?; // partition: aws / aws-us-gov / aws-cn（不强制,region 白名单已足够约束分区）
        if segs.next()? != "codewhisperer" {
            return None;
        }
        let region = segs.next()?;
        if SUPPORTED_KIRO_REGIONS.contains(&region) {
            Some(region)
        } else {
            None
        }
    }

    /// 【防呆根基】把 `region` / `auth_region` 强制同步成 `profile_arn` 内的 region。
    ///
    /// External IdP 账号可在多 region 各有独立 profile（实测 2026-07-12:同一账号
    /// us-east-1 与 eu-central-1 各一个 ARN,account 都不同）。上游对话端点
    /// `runtime.{region}.kiro.dev` 用的 region **必须**与 profileArn 第 4 段 region 一致,
    /// 否则 400 Improperly formed。故凡设置/写回 profile_arn 处都调本方法,让 region 字段
    /// 与 ARN 物理绑定、不可能错配——这是「导入/刷新不管怎样都自洽」的防呆保证。
    ///
    /// 以 ARN region 为唯一真相源:ARN 是上游权威返回的,region 字段只是本地记录。
    /// profile_arn 缺失或 region 非白名单时不动(交由既有回退逻辑)。返回是否发生了修正。
    pub fn sync_region_from_arn(&mut self) -> bool {
        let arn_region = match self
            .profile_arn
            .as_deref()
            .and_then(|a| Self::region_from_profile_arn(a))
        {
            Some(r) => r.to_string(),
            None => return false,
        };
        let mut changed = false;
        if self.region.as_deref() != Some(arn_region.as_str()) {
            self.region = Some(arn_region.clone());
            changed = true;
        }
        // 【IdC 豁免·502 回归修复】IdC 号的 auth_region = SSO-OIDC 实例所在 region(clientId/secret/
        // refreshToken 在此注册,刷新 token 必须打 oidc.{auth_region}.amazonaws.com),它与 profileArn
        // 的 region(对话/余额用)**物理不同**。若把 auth_region 也同步成 ARN region,刷新就打到错的
        // OIDC 端点 → clientId 跨 region 失配 → AWS 拒 → 网关 502(0.7.12 收口引入的回归)。故 IdC 号
        // **只同步 region(对话/余额),绝不碰 auth_region**。external_idp 的 auth_region 不参与刷新
        // (用微软 token_endpoint)、social 的走 kiro.dev,故仅 IdC 需此豁免。
        if !self.is_idc_credential()
            && self.auth_region.as_deref() != Some(arn_region.as_str())
        {
            self.auth_region = Some(arn_region.clone());
            changed = true;
        }
        changed
    }

    /// 获取有效的代理配置
    /// 优先级：凭据代理 > 全局代理 > 无代理
    /// 特殊值 "direct" 表示显式不使用代理（即使全局配置了代理）
    pub fn effective_proxy(&self, global_proxy: Option<&ProxyConfig>) -> Option<ProxyConfig> {
        match self.proxy_url.as_deref() {
            Some(url) if url.eq_ignore_ascii_case(Self::PROXY_DIRECT) => None,
            Some(url) => {
                // URL 里可能内嵌账密（socks5://user:pass@host:port）——拆出干净 URL 与内嵌账密。
                // 显式 proxy_username/proxy_password 字段优先；缺失时回退用 URL 内嵌的账密。
                // 保证无论用户把账密填在独立字段还是直接写进 URL，都能正确认证。
                let (clean_url, inline_user, inline_pass) =
                    crate::http_client::split_proxy_credentials(url);
                let mut proxy = ProxyConfig::new(clean_url);
                let username = self.proxy_username.clone().or(inline_user);
                let password = self.proxy_password.clone().or(inline_pass);
                if let (Some(username), Some(password)) = (username, password) {
                    proxy = proxy.with_auth(username, password);
                }
                Some(proxy)
            }
            None => global_proxy.cloned(),
        }
    }

    pub fn canonicalize_auth_method(&mut self) {
        let auth_method = match &self.auth_method {
            Some(m) => m,
            None => return,
        };

        let canonical = canonicalize_auth_method_value(auth_method);
        if canonical != auth_method {
            self.auth_method = Some(canonical.to_string());
        }
    }

    /// 是否为「自定义 API 代挂透传」凭据（auth_method=custom_api）。
    ///
    /// 这类凭据不是 Kiro 号:它是一个 Anthropic 兼容上游中转站（base_url + api_key），
    /// /v1/messages 请求原样透传过去。不参与 Kiro 的 token 刷新 / profileArn / region 逻辑。
    /// ⚠️ 判据以 auth_method 为准(canonicalize 已覆盖 custom_api 各种写法);`|| base_url.is_some()`
    /// 是旧数据兜底(早期未写 auth_method 的透传号)。语义脆弱:任何 Kiro 号一旦被写了 base_url
    /// 就会被判成透传号并踢出 Kiro 选号池——故 set_custom_api_config 的 gate 与添加表单都严格只让
    /// custom_api 号写 base_url,不给 Kiro 号写。将来若能确保存量数据都带 auth_method,可收紧掉此兜底。
    pub fn is_custom_api_credential(&self) -> bool {
        self.auth_method.as_deref() == Some("custom_api") || self.base_url.is_some()
    }

    /// 该号是否允许服务给定模型（成本安全白名单硬门）。
    ///
    /// `model` 为 map_model 后的规范 kiro modelId。未设白名单（None/空）→ 允许一切（兼容旧号）。
    /// 设了白名单 → 仅当 model 在其中才允许（大小写不敏感）。用于 is_entry_selectable 过滤，
    /// 确保便宜模型的请求绝不溢出到未列该模型的（更贵的）号。
    pub fn allows_model(&self, model: &str) -> bool {
        match &self.allowed_models {
            None => true,
            Some(list) if list.is_empty() => true,
            Some(list) => list.iter().any(|m| m.eq_ignore_ascii_case(model)),
        }
    }

    /// 检查凭据是否支持 Opus 模型
    ///
    /// Free 账号不支持 Opus 模型，需要 PRO 或更高等级订阅
    pub fn supports_opus(&self) -> bool {
        match &self.subscription_title {
            Some(title) => {
                let title_upper = title.to_uppercase();
                // 如果包含 FREE，则不支持 Opus
                !title_upper.contains("FREE")
            }
            // 如果还没有获取订阅信息，暂时允许（首次使用时会获取）
            None => true,
        }
    }

    /// 检查是否为 API Key 凭据
    ///
    /// API Key 凭据直接使用 kiro_api_key 作为 Bearer Token，无需 refreshToken
    pub fn is_api_key_credential(&self) -> bool {
        self.kiro_api_key.is_some()
            || self
                .auth_method
                .as_deref()
                .map(|m| m.eq_ignore_ascii_case("api_key") || m.eq_ignore_ascii_case("apikey"))
                .unwrap_or(false)
    }

    /// 获取 Web Portal Idp 标识（用于 Cookie: `Idp=<idp>`）
    ///
    /// KiroStudio 的凭据结构没有独立 `idp` 字段，这里按 `auth_method` 推断：
    /// - social（或未标注）→ "Google"（绝大多数 social 用户为 Google 登录）
    /// - idc / api_key → 留空（这两种凭据不参与 app.kiro.dev Web Portal 接口）
    ///
    /// 仅用于 overage 开关等 Web Portal 调用；返回空串表示该凭据不支持。
    pub fn should_send_profile_arn(&self) -> bool {
        self.effective_profile_arn().is_some()
    }

    /// 实际应注入/发送的 profileArn（对话 body / MCP header / 余额 query 统一走这里）。
    ///
    /// 规则(对齐 Kiro IDE + kiro-account-manager,修复新加 idc 号缺 profileArn 报
    /// `400 profileArn is required`):
    /// - external_idp(M365 企业)→ `None`：这类号带 profileArn 反而 403，绝不发。
    /// - 自带真实 profile_arn → 用它。
    /// - idc/social/api_key 缺 profile_arn → 回退默认 BuilderId profileArn（Kiro IDE 公共
    ///   占位 ARN，上游接受）。**根治**:idc 号入池时常没 profileArn(登录未拉),
    ///   而对话/余额端点要求必带,缺了就 400/403。
    ///
    /// 注:更智能的做法是运行时 ListAvailableProfiles 拉真实 ARN 并持久化（见研究备忘），
    /// 此处先用默认回退保证可用,动态解析作为后续增强。
    pub fn effective_profile_arn(&self) -> Option<String> {
        // 有自带/动态解析到的真实 profileArn → 一律用它（含 external_idp）。
        if let Some(arn) = self.profile_arn.as_deref() {
            if !arn.trim().is_empty() {
                return Some(arn.to_string());
            }
        }
        // external_idp（M365 企业）缺真实 arn 时返回 None：绝不给它套 idc 的默认
        // BuilderId 占位 ARN（那是别的租户的公共占位，external_idp 用了会 403）。
        // 它的真实 arn 由 refresh 后的动态 ListAvailableProfiles 解析补上（见
        // refresh_token_locked 的 eligible 判定）。
        //
        // 上游行为变更（kiro.dev 迁移后）：external_idp 号**必须**带自己租户的真实
        // profileArn，缺了直接 `400 profileArn is required`；旧注释"带 profileArn 反而
        // 403"已过时——那是旧 endpoint 时代、且指的是套错的占位 ARN。
        if self.is_external_idp_credential() {
            return None;
        }
        // idc/social/api_key 缺 arn → 回退默认 BuilderId 占位 ARN（上游接受）。
        Some(crate::kiro::token_manager::DEFAULT_BUILDER_ID_PROFILE_ARN.to_string())
    }

    pub fn is_external_idp_credential(&self) -> bool {
        self.auth_method
            .as_deref()
            .map(|m| {
                m.eq_ignore_ascii_case("external_idp")
                    || m.eq_ignore_ascii_case("external-idp")
                    || m.eq_ignore_ascii_case("externalidp")
                    || m.eq_ignore_ascii_case("azure")
                    || m.eq_ignore_ascii_case("azuread")
                    || m.eq_ignore_ascii_case("azure_ad")
            })
            .unwrap_or(false)
    }

    /// 是否 IdC(AWS IAM Identity Center / Builder ID)号。归一化后 == "idc"(含 builder-id/iam）。
    /// IdC 号的**认证 region(SSO-OIDC 实例)与对话/余额 region(profileArn)物理不同**,故
    /// [`sync_region_from_arn`](Self::sync_region_from_arn) 对它豁免 auth_region 改写(见该方法)。
    pub fn is_idc_credential(&self) -> bool {
        self.auth_method
            .as_deref()
            .map(|m| canonicalize_auth_method_value(m) == "idc")
            .unwrap_or(false)
    }

    /// 账户族键（family_key）—— 限流/健康的分组单位（见 docs/DESIGN-M365-FAMILY-RATELIMIT-0708.md）。
    ///
    /// M365 external_idp 号的上游限速是**租户/账户族级连坐**：同一 M365 租户的多个号，
    /// 一个被 suspicious 429 = 整族被限。故这些号共享一个族键（冷却/健康按族统一处置，
    /// 不再逐个号砸），族键取 issuer_url 里的 tenant GUID；解析不到则回退 profileArn 的
    /// AWS 账户号（日志实证同租户号 AWS account-id 同构，可互为兜底）。
    ///
    /// IdC(AWS SSO)/social(Google)/api_key **没有 M365 租户身份、限速模型不同（坚强、独立）**，
    /// 各自 `cred:{id}` 独立成族——M365 的族连坐永不波及它们（键无 `m365:`/`aws:` 前缀），
    /// 雪崩时作坚强兜底号。`id` 传入调用方（entry）的不可变 id。
    pub fn family_key(&self, id: u64) -> String {
        if self.is_external_idp_credential() {
            // ① issuer_url: https://login.microsoftonline.com/{tenant}/v2.0 → m365:{tenant}
            if let Some(issuer) = self.issuer_url.as_deref() {
                if let Some(rest) = issuer.split("login.microsoftonline.com/").nth(1) {
                    let tenant = rest.split('/').next().unwrap_or("").trim();
                    if !tenant.is_empty() {
                        return format!("m365:{tenant}");
                    }
                }
            }
            // ② 兜底: profileArn arn:aws:codewhisperer:region:{account}:profile/xxx → aws:{account}
            if let Some(arn) = self.profile_arn.as_deref() {
                let parts: Vec<&str> = arn.split(':').collect();
                // arn:aws:codewhisperer:{region}:{account}:profile/{id} → 索引 4 = account
                if parts.len() >= 5 && !parts[4].is_empty() {
                    return format!("aws:{}", parts[4]);
                }
            }
        }
        // ③ 非 M365（IdC/social/api_key）或解析失败：各自独立成族
        format!("cred:{id}")
    }

    pub fn effective_idp(&self) -> &str {
        match self
            .auth_method
            .as_deref()
            .map(|s| s.to_ascii_lowercase())
            .as_deref()
        {
            Some("social") | None => "Google",
            _ => "",
        }
    }
}

#[cfg(test)]
impl KiroCredentials {
    fn from_json(json_string: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json_string)
    }

    fn to_pretty_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::config::Config;

    #[test]
    fn test_allows_model_whitelist() {
        // 无白名单 → 允许一切（兼容旧号）
        let c = KiroCredentials::from_json(r#"{"refreshToken":"x"}"#).unwrap();
        assert!(c.allows_model("claude-opus-4.8"));
        assert!(c.allows_model("deepseek-3.2"));

        // 空白名单 → 允许一切
        let c = KiroCredentials::from_json(r#"{"refreshToken":"x","allowedModels":[]}"#).unwrap();
        assert!(c.allows_model("claude-opus-4.8"));

        // 设了白名单 → 仅白名单内允许（成本安全硬门）
        let c = KiroCredentials::from_json(
            r#"{"refreshToken":"x","allowedModels":["deepseek-3.2","glm-5"]}"#,
        )
        .unwrap();
        assert!(c.allows_model("deepseek-3.2"), "白名单内应允许");
        assert!(c.allows_model("glm-5"));
        assert!(!c.allows_model("claude-opus-4.8"), "白名单外的贵模型绝不允许(防溢出)");
        // 大小写不敏感
        assert!(c.allows_model("DeepSeek-3.2"));
    }

    #[test]
    fn test_region_from_profile_arn_valid() {
        assert_eq!(
            KiroCredentials::region_from_profile_arn(
                "arn:aws:codewhisperer:us-east-1:123456789012:profile/ABC"
            ),
            Some("us-east-1")
        );
        assert_eq!(
            KiroCredentials::region_from_profile_arn(
                "arn:aws-us-gov:codewhisperer:us-gov-west-1:1:profile/X"
            ),
            Some("us-gov-west-1")
        );
    }

    #[test]
    fn test_region_from_profile_arn_rejects_polluted() {
        // 非 codewhisperer ARN → None（不会误取第 4 段当 region）
        assert_eq!(
            KiroCredentials::region_from_profile_arn("arn:aws:s3:::my-bucket/key"),
            None
        );
        // region 不在白名单（臆造/垃圾）→ None
        assert_eq!(
            KiroCredentials::region_from_profile_arn(
                "arn:aws:codewhisperer:not-a-region:1:profile/X"
            ),
            None
        );
        // 非 arn 前缀 → None
        assert_eq!(
            KiroCredentials::region_from_profile_arn("garbage:aws:codewhisperer:us-east-1:1:x"),
            None
        );
        // 空/残缺 → None（不 panic）
        assert_eq!(KiroCredentials::region_from_profile_arn(""), None);
        assert_eq!(KiroCredentials::region_from_profile_arn("arn:aws"), None);
    }

    #[test]
    fn test_sync_region_from_arn_overrides_mismatched_region() {
        // 防呆核心:region 字段与 ARN region 不符时,以 ARN 为准强制同步。
        // 场景:导入存了 us-east-1,但实际选的 profile 是 eu-central-1 的 ARN。
        let mut c = KiroCredentials::default();
        c.region = Some("us-east-1".to_string());
        c.auth_region = Some("us-east-1".to_string());
        c.profile_arn =
            Some("arn:aws:codewhisperer:eu-central-1:155119901513:profile/ACPYXKUPYE3H".to_string());
        let changed = c.sync_region_from_arn();
        assert!(changed, "region 不符时应发生修正");
        assert_eq!(c.region.as_deref(), Some("eu-central-1"), "region 应被 ARN region 覆盖");
        assert_eq!(c.auth_region.as_deref(), Some("eu-central-1"), "auth_region 同步");
    }

    #[test]
    fn test_sync_region_from_arn_idc_exempts_auth_region() {
        // 【502 回归修复】IdC 号:auth_region=SSO-OIDC 实例 region(刷新 token 用),与 profileArn
        // region(对话/余额)物理不同。sync 只同步 region,**绝不改 auth_region**(改了刷新打错端点→502)。
        let mut c = KiroCredentials::default();
        c.auth_method = Some("idc".to_string());
        c.region = Some("us-east-1".to_string());
        c.auth_region = Some("us-east-1".to_string()); // = R_sso(SSO-OIDC 注册 region)
        c.profile_arn =
            Some("arn:aws:codewhisperer:eu-central-1:1:profile/X".to_string()); // R_arn 不同
        let changed = c.sync_region_from_arn();
        assert!(changed, "region 不符仍应同步(返回 true)");
        assert_eq!(c.region.as_deref(), Some("eu-central-1"), "IdC 对话 region 应同步为 ARN region");
        assert_eq!(
            c.auth_region.as_deref(),
            Some("us-east-1"),
            "IdC 的 auth_region 必须保留 R_sso,绝不被 ARN region 覆盖(否则刷新 502)"
        );
        // builder-id / iam 归一化也算 IdC,同样豁免。
        let mut c2 = KiroCredentials::default();
        c2.auth_method = Some("builder-id".to_string());
        c2.auth_region = Some("us-west-2".to_string());
        c2.profile_arn = Some("arn:aws:codewhisperer:eu-central-1:1:profile/X".to_string());
        c2.sync_region_from_arn();
        assert_eq!(c2.auth_region.as_deref(), Some("us-west-2"), "builder-id 也豁免 auth_region");
    }

    #[test]
    fn test_sync_region_from_arn_noop_when_consistent_or_missing() {
        // 已一致 → 不改（返回 false）
        let mut c = KiroCredentials::default();
        c.region = Some("eu-central-1".to_string());
        c.auth_region = Some("eu-central-1".to_string());
        c.profile_arn = Some("arn:aws:codewhisperer:eu-central-1:1:profile/X".to_string());
        assert!(!c.sync_region_from_arn(), "已一致不应改动");
        // profile_arn 缺失 → 不动 region（交由既有回退）
        let mut c2 = KiroCredentials::default();
        c2.region = Some("ap-southeast-1".to_string());
        c2.profile_arn = None;
        assert!(!c2.sync_region_from_arn());
        assert_eq!(c2.region.as_deref(), Some("ap-southeast-1"), "无 ARN 不碰 region");
        // ARN region 非白名单 → 不动（不会把 region 改成垃圾值）
        let mut c3 = KiroCredentials::default();
        c3.region = Some("us-east-1".to_string());
        c3.profile_arn = Some("arn:aws:codewhisperer:not-a-region:1:profile/X".to_string());
        assert!(!c3.sync_region_from_arn());
        assert_eq!(c3.region.as_deref(), Some("us-east-1"), "非法 ARN region 不覆盖");
    }

    #[test]
    fn test_effective_profile_arn_idc_falls_back_to_default() {
        // 根治新加 idc 号缺 profileArn 报 400:idc 缺 profileArn 应回退默认 BuilderId ARN
        let mut idc = KiroCredentials::default();
        idc.auth_method = Some("idc".to_string());
        idc.profile_arn = None;
        let arn = idc.effective_profile_arn();
        assert_eq!(
            arn.as_deref(),
            Some(crate::kiro::token_manager::DEFAULT_BUILDER_ID_PROFILE_ARN),
            "idc 缺 profileArn 应回退默认 BuilderId ARN(否则对话 400 profileArn is required)"
        );
        assert!(idc.should_send_profile_arn(), "idc 应发送 profileArn");
    }

    #[test]
    fn test_effective_profile_arn_uses_real_when_present() {
        let mut idc = KiroCredentials::default();
        idc.auth_method = Some("idc".to_string());
        idc.profile_arn = Some("arn:aws:codewhisperer:us-east-1:111:profile/REAL".to_string());
        assert_eq!(
            idc.effective_profile_arn().as_deref(),
            Some("arn:aws:codewhisperer:us-east-1:111:profile/REAL")
        );
    }

    #[test]
    fn test_effective_profile_arn_external_idp() {
        // kiro.dev 迁移后 external_idp 号**必须**带自己租户的真实 profileArn
        //（缺了 400 profileArn is required）。
        // ① 有真实 arn → 用它。
        let mut ext = KiroCredentials::default();
        ext.auth_method = Some("external_idp".to_string());
        ext.profile_arn = Some("arn:aws:codewhisperer:us-east-1:222:profile/X".to_string());
        assert_eq!(
            ext.effective_profile_arn().as_deref(),
            Some("arn:aws:codewhisperer:us-east-1:222:profile/X"),
            "external_idp 有真实 arn 应使用它"
        );
        assert!(ext.should_send_profile_arn());

        // ② 缺 arn → 返回 None（绝不套 idc 的默认 BuilderId 占位 ARN，那会 403）；
        //    真实 arn 由 refresh 后动态 ListAvailableProfiles 解析补上。
        let mut ext_no_arn = KiroCredentials::default();
        ext_no_arn.auth_method = Some("external_idp".to_string());
        assert_eq!(
            ext_no_arn.effective_profile_arn(),
            None,
            "external_idp 缺真实 arn 应返回 None（不套默认占位 ARN）"
        );
    }

    #[test]
    fn test_effective_upstream_region_fallback() {
        let config = Config::default();
        // profileArn 污染 → 回退凭据 region（不拼坏 host）
        let mut c = KiroCredentials::default();
        c.profile_arn = Some("arn:aws:s3:::bucket".to_string());
        c.region = Some("eu-central-1".to_string());
        assert_eq!(c.effective_upstream_region(&config), "eu-central-1");
        // profileArn 合法 → 优先用 ARN region（压过凭据 region）
        c.profile_arn =
            Some("arn:aws:codewhisperer:ap-northeast-1:1:profile/X".to_string());
        assert_eq!(c.effective_upstream_region(&config), "ap-northeast-1");
    }

    /// 安全回归(H3):被污染的凭据 region/auth_region 字段(来自不可信导入的凭据 JSON)
    /// 不得原样进入上游 host——必须过 SUPPORTED_KIRO_REGIONS 白名单,不命中回退可信 config。
    #[test]
    fn test_region_field_rejects_polluted() {
        let mut config = Config::default();
        config.region = "us-east-1".to_string();
        config.auth_region = Some("us-east-1".to_string());
        config.api_region = Some("us-east-1".to_string());

        let mut c = KiroCredentials::default();
        // 攻击者投毒:把 region 拼成能劫持 host 的字符串
        c.region = Some("evil.attacker.com/".to_string());
        c.auth_region = Some("169.254.169.254".to_string());
        c.api_region = Some("x-injected".to_string());

        // 三个入口都不得吐出污染值,一律回退可信 config
        assert_eq!(c.effective_upstream_region(&config), "us-east-1");
        assert_eq!(c.effective_auth_region(&config), "us-east-1");
        assert_eq!(c.effective_api_region(&config), "us-east-1");
        // 合法 region 仍正常放行
        c.region = Some("eu-west-1".to_string());
        assert_eq!(c.effective_upstream_region(&config), "eu-west-1");
        assert!(KiroCredentials::is_supported_region("eu-west-1"));
        assert!(!KiroCredentials::is_supported_region("evil.com/"));
    }

    /// 安全回归：Debug 输出绝不含敏感凭证明文（防 HIGH-1 日志泄露复发）。
    #[test]
    fn test_debug_masks_secrets() {
        let mut c = KiroCredentials::default();
        c.id = Some(7);
        c.email = Some("u@example.com".to_string());
        c.access_token = Some("AT_SECRET_PLAINTEXT_123".to_string());
        c.refresh_token = Some("RT_SECRET_PLAINTEXT_456".to_string());
        c.client_secret = Some("CS_SECRET_PLAINTEXT_789".to_string());
        c.kiro_api_key = Some("ksk_SECRET_PLAINTEXT".to_string());
        c.proxy_password = Some("PROXY_PASS_SECRET".to_string());

        let dbg = format!("{:?}", c);

        // 明文密钥绝不出现
        for leaked in [
            "AT_SECRET_PLAINTEXT_123",
            "RT_SECRET_PLAINTEXT_456",
            "CS_SECRET_PLAINTEXT_789",
            "ksk_SECRET_PLAINTEXT",
            "PROXY_PASS_SECRET",
        ] {
            assert!(
                !dbg.contains(leaked),
                "Debug 输出泄露了敏感明文 {leaked}: {dbg}"
            );
        }
        // 非敏感可识别字段仍可见，便于排障
        assert!(dbg.contains("u@example.com"), "email 应可见");
        assert!(dbg.contains("id: Some(7)"), "id 应可见");
        // 敏感字段以脱敏形式标注（<set:N>）
        assert!(dbg.contains("<set:"), "敏感字段应以 <set:N> 脱敏标注");
    }

    #[test]
    fn test_from_json() {
        let json = r#"{
            "accessToken": "test_token",
            "refreshToken": "test_refresh",
            "profileArn": "arn:aws:test",
            "expiresAt": "2024-01-01T00:00:00Z",
            "authMethod": "social"
        }"#;

        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.access_token, Some("test_token".to_string()));
        assert_eq!(creds.refresh_token, Some("test_refresh".to_string()));
        assert_eq!(creds.profile_arn, Some("arn:aws:test".to_string()));
        assert_eq!(creds.expires_at, Some("2024-01-01T00:00:00Z".to_string()));
        assert_eq!(creds.auth_method, Some("social".to_string()));
    }

    #[test]
    fn test_from_json_with_unknown_keys() {
        let json = r#"{
            "accessToken": "test_token",
            "unknownField": "should be ignored"
        }"#;

        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.access_token, Some("test_token".to_string()));
    }

    #[test]
    fn test_to_json() {
        let creds = KiroCredentials {
            id: None,
            access_token: Some("token".to_string()),
            refresh_token: None,
            profile_arn: None,
            expires_at: None,
            auth_method: Some("social".to_string()),
            client_id: None,
            client_secret: None,
            token_endpoint: None,
            issuer_url: None,
            scopes: None,
            priority: 0,
            rpm_limit: None,
            allowed_models: None,
            tested_models: None,
            base_url: None,
            api_key: None,
            request_limit: None,
            region: None,
            auth_region: None,
            api_region: None,
            machine_id: None,
            email: None,
            name: None,
            subscription_title: None,
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            disabled: false,
            kiro_api_key: None,
            endpoint: None,
        };

        let json = creds.to_pretty_json().unwrap();
        assert!(json.contains("accessToken"));
        assert!(json.contains("authMethod"));
        assert!(!json.contains("refreshToken"));
        // priority 为 0 时不序列化
        assert!(!json.contains("priority"));
    }

    #[test]
    fn test_default_credentials_path() {
        assert_eq!(
            KiroCredentials::default_credentials_path(),
            "credentials.json"
        );
    }

    #[test]
    fn test_priority_default() {
        let json = r#"{"refreshToken": "test"}"#;
        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.priority, 0);
    }

    #[test]
    fn test_priority_explicit() {
        let json = r#"{"refreshToken": "test", "priority": 5}"#;
        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.priority, 5);
    }

    #[test]
    fn test_credentials_config_single() {
        let json = r#"{"refreshToken": "test", "expiresAt": "2025-12-31T00:00:00Z"}"#;
        let config: CredentialsConfig = serde_json::from_str(json).unwrap();
        assert!(matches!(config, CredentialsConfig::Single(_)));
    }

    #[test]
    fn test_credentials_config_multiple() {
        let json = r#"[
            {"refreshToken": "test1", "priority": 1},
            {"refreshToken": "test2", "priority": 0}
        ]"#;
        let config: CredentialsConfig = serde_json::from_str(json).unwrap();
        assert!(matches!(config, CredentialsConfig::Multiple(_)));
        assert_eq!(config.into_sorted_credentials().len(), 2);
    }

    #[test]
    fn test_credentials_config_priority_sorting() {
        let json = r#"[
            {"refreshToken": "t1", "priority": 2},
            {"refreshToken": "t2", "priority": 0},
            {"refreshToken": "t3", "priority": 1}
        ]"#;
        let config: CredentialsConfig = serde_json::from_str(json).unwrap();
        let list = config.into_sorted_credentials();

        // 验证按优先级排序
        assert_eq!(list[0].refresh_token, Some("t2".to_string())); // priority 0
        assert_eq!(list[1].refresh_token, Some("t3".to_string())); // priority 1
        assert_eq!(list[2].refresh_token, Some("t1".to_string())); // priority 2
    }

    // ============ Region 字段测试 ============

    #[test]
    fn test_region_field_parsing() {
        // 测试解析包含 region 字段的 JSON
        let json = r#"{
            "refreshToken": "test_refresh",
            "region": "us-east-1"
        }"#;

        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.refresh_token, Some("test_refresh".to_string()));
        assert_eq!(creds.region, Some("us-east-1".to_string()));
    }

    #[test]
    fn test_region_field_missing_backward_compat() {
        // 测试向后兼容：不包含 region 字段的旧格式 JSON
        let json = r#"{
            "refreshToken": "test_refresh",
            "authMethod": "social"
        }"#;

        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.refresh_token, Some("test_refresh".to_string()));
        assert_eq!(creds.region, None);
    }

    #[test]
    fn test_region_field_serialization() {
        let creds = KiroCredentials {
            id: None,
            access_token: None,
            refresh_token: Some("test".to_string()),
            profile_arn: None,
            expires_at: None,
            auth_method: None,
            client_id: None,
            client_secret: None,
            token_endpoint: None,
            issuer_url: None,
            scopes: None,
            priority: 0,
            rpm_limit: None,
            allowed_models: None,
            tested_models: None,
            base_url: None,
            api_key: None,
            request_limit: None,
            region: Some("eu-west-1".to_string()),
            auth_region: None,
            api_region: None,
            machine_id: None,
            email: None,
            name: None,
            subscription_title: None,
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            disabled: false,
            kiro_api_key: None,
            endpoint: None,
        };

        let json = creds.to_pretty_json().unwrap();
        assert!(json.contains("region"));
        assert!(json.contains("eu-west-1"));
    }

    #[test]
    fn test_region_field_none_not_serialized() {
        let creds = KiroCredentials {
            id: None,
            access_token: None,
            refresh_token: Some("test".to_string()),
            profile_arn: None,
            expires_at: None,
            auth_method: None,
            client_id: None,
            client_secret: None,
            token_endpoint: None,
            issuer_url: None,
            scopes: None,
            priority: 0,
            rpm_limit: None,
            allowed_models: None,
            tested_models: None,
            base_url: None,
            api_key: None,
            request_limit: None,
            region: None,
            auth_region: None,
            api_region: None,
            machine_id: None,
            email: None,
            name: None,
            subscription_title: None,
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            disabled: false,
            kiro_api_key: None,
            endpoint: None,
        };

        let json = creds.to_pretty_json().unwrap();
        assert!(!json.contains("region"));
    }

    // ============ MachineId 字段测试 ============

    #[test]
    fn test_machine_id_field_parsing() {
        let machine_id = "a".repeat(64);
        let json = format!(
            r#"{{
                "refreshToken": "test_refresh",
                "machineId": "{machine_id}"
            }}"#
        );

        let creds = KiroCredentials::from_json(&json).unwrap();
        assert_eq!(creds.refresh_token, Some("test_refresh".to_string()));
        assert_eq!(creds.machine_id, Some(machine_id));
    }

    #[test]
    fn test_machine_id_field_serialization() {
        let mut creds = KiroCredentials::default();
        creds.refresh_token = Some("test".to_string());
        creds.machine_id = Some("b".repeat(64));

        let json = creds.to_pretty_json().unwrap();
        assert!(json.contains("machineId"));
    }

    #[test]
    fn test_machine_id_field_none_not_serialized() {
        let mut creds = KiroCredentials::default();
        creds.refresh_token = Some("test".to_string());
        creds.machine_id = None;

        let json = creds.to_pretty_json().unwrap();
        assert!(!json.contains("machineId"));
    }

    #[test]
    fn test_multiple_credentials_with_different_regions() {
        // 测试多凭据场景下不同凭据使用各自的 region
        let json = r#"[
            {"refreshToken": "t1", "region": "us-east-1"},
            {"refreshToken": "t2", "region": "eu-west-1"},
            {"refreshToken": "t3"}
        ]"#;

        let config: CredentialsConfig = serde_json::from_str(json).unwrap();
        let list = config.into_sorted_credentials();

        assert_eq!(list[0].region, Some("us-east-1".to_string()));
        assert_eq!(list[1].region, Some("eu-west-1".to_string()));
        assert_eq!(list[2].region, None);
    }

    #[test]
    fn test_region_field_with_all_fields() {
        // 测试包含所有字段的完整 JSON
        let json = r#"{
            "id": 1,
            "accessToken": "access",
            "refreshToken": "refresh",
            "profileArn": "arn:aws:test",
            "expiresAt": "2025-12-31T00:00:00Z",
            "authMethod": "idc",
            "clientId": "client123",
            "clientSecret": "secret456",
            "priority": 5,
            "region": "ap-northeast-1"
        }"#;

        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.id, Some(1));
        assert_eq!(creds.access_token, Some("access".to_string()));
        assert_eq!(creds.refresh_token, Some("refresh".to_string()));
        assert_eq!(creds.profile_arn, Some("arn:aws:test".to_string()));
        assert_eq!(creds.expires_at, Some("2025-12-31T00:00:00Z".to_string()));
        assert_eq!(creds.auth_method, Some("idc".to_string()));
        assert_eq!(creds.client_id, Some("client123".to_string()));
        assert_eq!(creds.client_secret, Some("secret456".to_string()));
        assert_eq!(creds.priority, 5);
        assert_eq!(creds.region, Some("ap-northeast-1".to_string()));
    }

    #[test]
    fn test_region_roundtrip() {
        // 测试序列化和反序列化的往返一致性
        let original = KiroCredentials {
            id: Some(42),
            access_token: Some("token".to_string()),
            refresh_token: Some("refresh".to_string()),
            profile_arn: None,
            expires_at: None,
            auth_method: Some("social".to_string()),
            client_id: None,
            client_secret: None,
            token_endpoint: None,
            issuer_url: None,
            scopes: None,
            priority: 3,
            rpm_limit: None,
            allowed_models: None,
            tested_models: None,
            base_url: None,
            api_key: None,
            request_limit: None,
            region: Some("us-west-2".to_string()),
            auth_region: None,
            api_region: None,
            machine_id: Some("c".repeat(64)),
            email: None,
            name: None,
            subscription_title: None,
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            disabled: false,
            kiro_api_key: None,
            endpoint: None,
        };

        let json = original.to_pretty_json().unwrap();
        let parsed = KiroCredentials::from_json(&json).unwrap();

        assert_eq!(parsed.id, original.id);
        assert_eq!(parsed.access_token, original.access_token);
        assert_eq!(parsed.refresh_token, original.refresh_token);
        assert_eq!(parsed.priority, original.priority);
        assert_eq!(parsed.region, original.region);
        assert_eq!(parsed.machine_id, original.machine_id);
    }

    // ============ auth_region / api_region 字段测试 ============

    #[test]
    fn test_auth_region_field_parsing() {
        let json = r#"{
            "refreshToken": "test_refresh",
            "authRegion": "eu-central-1"
        }"#;
        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.auth_region, Some("eu-central-1".to_string()));
        assert_eq!(creds.api_region, None);
    }

    #[test]
    fn test_api_region_field_parsing() {
        let json = r#"{
            "refreshToken": "test_refresh",
            "apiRegion": "ap-southeast-1"
        }"#;
        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.api_region, Some("ap-southeast-1".to_string()));
        assert_eq!(creds.auth_region, None);
    }

    #[test]
    fn test_auth_api_region_serialization() {
        let mut creds = KiroCredentials::default();
        creds.refresh_token = Some("test".to_string());
        creds.auth_region = Some("eu-west-1".to_string());
        creds.api_region = Some("us-west-2".to_string());

        let json = creds.to_pretty_json().unwrap();
        assert!(json.contains("authRegion"));
        assert!(json.contains("eu-west-1"));
        assert!(json.contains("apiRegion"));
        assert!(json.contains("us-west-2"));
    }

    #[test]
    fn test_auth_api_region_none_not_serialized() {
        let mut creds = KiroCredentials::default();
        creds.refresh_token = Some("test".to_string());
        creds.auth_region = None;
        creds.api_region = None;

        let json = creds.to_pretty_json().unwrap();
        assert!(!json.contains("authRegion"));
        assert!(!json.contains("apiRegion"));
    }

    #[test]
    fn test_auth_api_region_roundtrip() {
        let mut original = KiroCredentials::default();
        original.refresh_token = Some("refresh".to_string());
        original.region = Some("us-east-1".to_string());
        original.auth_region = Some("eu-west-1".to_string());
        original.api_region = Some("ap-northeast-1".to_string());

        let json = original.to_pretty_json().unwrap();
        let parsed = KiroCredentials::from_json(&json).unwrap();

        assert_eq!(parsed.region, original.region);
        assert_eq!(parsed.auth_region, original.auth_region);
        assert_eq!(parsed.api_region, original.api_region);
    }

    #[test]
    fn test_backward_compat_no_auth_api_region() {
        // 旧格式 JSON 不包含 authRegion/apiRegion，应正常解析
        let json = r#"{
            "refreshToken": "test_refresh",
            "region": "us-east-1"
        }"#;
        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.region, Some("us-east-1".to_string()));
        assert_eq!(creds.auth_region, None);
        assert_eq!(creds.api_region, None);
    }

    // ============ effective_auth_region / effective_api_region 优先级测试 ============

    #[test]
    fn test_effective_auth_region_credential_auth_region_highest() {
        // 凭据.auth_region > 凭据.region > config.auth_region > config.region
        let mut config = Config::default();
        config.region = "config-region".to_string();
        config.auth_region = Some("config-auth-region".to_string());

        let mut creds = KiroCredentials::default();
        creds.region = Some("eu-central-1".to_string());
        creds.auth_region = Some("eu-west-1".to_string());

        assert_eq!(creds.effective_auth_region(&config), "eu-west-1");
    }

    #[test]
    fn test_effective_auth_region_fallback_to_credential_region() {
        let mut config = Config::default();
        config.region = "config-region".to_string();
        config.auth_region = Some("config-auth-region".to_string());

        let mut creds = KiroCredentials::default();
        creds.region = Some("eu-central-1".to_string());
        // auth_region 未设置

        assert_eq!(creds.effective_auth_region(&config), "eu-central-1");
    }

    #[test]
    fn test_effective_auth_region_fallback_to_config_auth_region() {
        let mut config = Config::default();
        config.region = "config-region".to_string();
        config.auth_region = Some("config-auth-region".to_string());

        let creds = KiroCredentials::default();
        // auth_region 和 region 均未设置

        assert_eq!(creds.effective_auth_region(&config), "config-auth-region");
    }

    #[test]
    fn test_effective_auth_region_fallback_to_config_region() {
        let mut config = Config::default();
        config.region = "config-region".to_string();
        // config.auth_region 未设置

        let creds = KiroCredentials::default();

        assert_eq!(creds.effective_auth_region(&config), "config-region");
    }

    #[test]
    fn test_effective_api_region_credential_api_region_highest() {
        // 凭据.api_region > config.api_region > config.region
        let mut config = Config::default();
        config.region = "config-region".to_string();
        config.api_region = Some("config-api-region".to_string());

        let mut creds = KiroCredentials::default();
        creds.api_region = Some("ap-southeast-1".to_string());

        assert_eq!(creds.effective_api_region(&config), "ap-southeast-1");
    }

    #[test]
    fn test_effective_api_region_fallback_to_config_api_region() {
        let mut config = Config::default();
        config.region = "config-region".to_string();
        config.api_region = Some("config-api-region".to_string());

        let creds = KiroCredentials::default();

        assert_eq!(creds.effective_api_region(&config), "config-api-region");
    }

    #[test]
    fn test_effective_api_region_fallback_to_config_region() {
        let mut config = Config::default();
        config.region = "config-region".to_string();

        let creds = KiroCredentials::default();

        assert_eq!(creds.effective_api_region(&config), "config-region");
    }

    #[test]
    fn test_effective_api_region_ignores_credential_region() {
        // 凭据.region 不参与 api_region 的回退链
        let mut config = Config::default();
        config.region = "config-region".to_string();

        let mut creds = KiroCredentials::default();
        creds.region = Some("cred-region".to_string());

        assert_eq!(creds.effective_api_region(&config), "config-region");
    }

    #[test]
    fn test_auth_and_api_region_independent() {
        // auth_region 和 api_region 互不影响
        let mut config = Config::default();
        config.region = "default".to_string();

        let mut creds = KiroCredentials::default();
        creds.auth_region = Some("eu-west-2".to_string());
        creds.api_region = Some("ap-northeast-2".to_string());

        assert_eq!(creds.effective_auth_region(&config), "eu-west-2");
        assert_eq!(creds.effective_api_region(&config), "ap-northeast-2");
    }

    // ============ 凭据级代理优先级测试 ============

    #[test]
    fn test_family_key_m365_by_tenant() {
        // 同一 M365 租户的两个 external_idp 号 → 同一族键（整族连坐）
        // 注：租户 GUID 用占位假值（脱敏，不含真实账户标识）。
        let mut a = KiroCredentials::default();
        a.auth_method = Some("external_idp".to_string());
        a.issuer_url = Some("https://login.microsoftonline.com/00000000-0000-0000-0000-000000000001/v2.0".to_string());
        let mut b = KiroCredentials::default();
        b.auth_method = Some("external_idp".to_string());
        b.issuer_url = Some("https://login.microsoftonline.com/00000000-0000-0000-0000-000000000001/v2.0".to_string());
        assert_eq!(a.family_key(53), "m365:00000000-0000-0000-0000-000000000001");
        assert_eq!(a.family_key(53), b.family_key(54), "同租户号必须同族键");
    }

    #[test]
    fn test_family_key_m365_falls_back_to_aws_account() {
        // issuer 缺失但有 profileArn → aws:{account}（AWS 账户号用占位假值，脱敏）
        let mut c = KiroCredentials::default();
        c.auth_method = Some("external_idp".to_string());
        c.profile_arn = Some("arn:aws:codewhisperer:us-east-1:000000000000:profile/EXAMPLE".to_string());
        assert_eq!(c.family_key(53), "aws:000000000000");
    }

    #[test]
    fn test_family_key_idc_independent() {
        // IdC 号无 issuer → cred:{id} 独立成族（坚强兜底,不并入 m365 连坐）
        let mut idc = KiroCredentials::default();
        idc.auth_method = Some("idc".to_string());
        assert_eq!(idc.family_key(61), "cred:61");
        // social 同理独立
        let mut soc = KiroCredentials::default();
        soc.auth_method = Some("social".to_string());
        assert_eq!(soc.family_key(70), "cred:70");
    }

    #[test]
    fn test_effective_proxy_credential_overrides_global() {
        let global = ProxyConfig::new("http://global:8080");
        let mut creds = KiroCredentials::default();
        creds.proxy_url = Some("socks5://cred:1080".to_string());

        let result = creds.effective_proxy(Some(&global));
        assert_eq!(result, Some(ProxyConfig::new("socks5://cred:1080")));
    }

    #[test]
    fn test_effective_proxy_inline_credentials_in_url() {
        // 账密内嵌 URL（虚构样例）：effective_proxy 应拆出账密走 with_auth，
        // 干净 URL 不含账密。
        let mut creds = KiroCredentials::default();
        creds.proxy_url = Some("socks5://proxyuser:proxypass@127.0.0.1:1080".to_string());

        let result = creds.effective_proxy(None).unwrap();
        assert_eq!(result.url, "socks5://127.0.0.1:1080");
        assert_eq!(result.username, Some("proxyuser".to_string()));
        assert_eq!(result.password, Some("proxypass".to_string()));
    }

    #[test]
    fn test_effective_proxy_explicit_fields_override_inline() {
        // 独立账密字段优先于 URL 内嵌值。
        let mut creds = KiroCredentials::default();
        creds.proxy_url = Some("socks5://inlineuser:inlinepass@host:1080".to_string());
        creds.proxy_username = Some("explicit".to_string());
        creds.proxy_password = Some("explicitpass".to_string());

        let result = creds.effective_proxy(None).unwrap();
        assert_eq!(result.url, "socks5://host:1080");
        assert_eq!(result.username, Some("explicit".to_string()));
        assert_eq!(result.password, Some("explicitpass".to_string()));
    }

    #[test]
    fn test_effective_proxy_credential_with_auth() {
        let global = ProxyConfig::new("http://global:8080");
        let mut creds = KiroCredentials::default();
        creds.proxy_url = Some("http://proxy:3128".to_string());
        creds.proxy_username = Some("user".to_string());
        creds.proxy_password = Some("pass".to_string());

        let result = creds.effective_proxy(Some(&global));
        let expected = ProxyConfig::new("http://proxy:3128").with_auth("user", "pass");
        assert_eq!(result, Some(expected));
    }

    #[test]
    fn test_effective_proxy_direct_bypasses_global() {
        let global = ProxyConfig::new("http://global:8080");
        let mut creds = KiroCredentials::default();
        creds.proxy_url = Some("direct".to_string());

        let result = creds.effective_proxy(Some(&global));
        assert_eq!(result, None);
    }

    #[test]
    fn test_effective_proxy_direct_case_insensitive() {
        let global = ProxyConfig::new("http://global:8080");
        let mut creds = KiroCredentials::default();
        creds.proxy_url = Some("DIRECT".to_string());

        let result = creds.effective_proxy(Some(&global));
        assert_eq!(result, None);
    }

    #[test]
    fn test_effective_proxy_fallback_to_global() {
        let global = ProxyConfig::new("http://global:8080");
        let creds = KiroCredentials::default();

        let result = creds.effective_proxy(Some(&global));
        assert_eq!(result, Some(ProxyConfig::new("http://global:8080")));
    }

    #[test]
    fn test_effective_proxy_none_when_no_proxy() {
        let creds = KiroCredentials::default();
        let result = creds.effective_proxy(None);
        assert_eq!(result, None);
    }
}
