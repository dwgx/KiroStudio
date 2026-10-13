//! 可复用的代理节点表（「分身管理」页维护）。
//!
//! 与 [`crate::kiro::model::credentials::KiroCredentials`] 的 `proxy_*` 字段的关系：
//! 本表是**候选池**（有哪些节点可用），凭据字段是**绑定结果**（这个号走哪个节点）。
//! 生成分身时从池里取节点写进凭据。
//!
//! 密码随文件走 at-rest 加密（与 credentials/trash 同开关同密钥），故绝不放 config.json。

/// 节点表上限。超过即拒绝新建（而非静默丢弃）。
pub const MAX_SOCKS_NODES: usize = 64;

/// 一个可复用的 SOCKS/HTTP 代理节点。
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SocksNode {
    /// 节点 id（自增，持久化后保持稳定；删除**不复用**——复用会让已绑该 id 的
    /// 分身在节点被删后指向一个无关新节点）。
    pub id: u64,
    /// 展示名（如 "US-West-1"）；为空时前端回落显示 host:port。
    #[serde(default)]
    pub name: String,
    /// 代理 URL：`socks5://host:port` / `http://host:port`。
    /// 内嵌账密在入表时已被拆到下面两字段（避免密码明文留在 URL 里）。
    pub url: String,
    /// 代理用户名（可选）。
    #[serde(default)]
    pub username: Option<String>,
    /// 代理密码（可选，落盘随文件加密）。
    #[serde(default)]
    pub password: Option<String>,
    /// 是否可用于分配（关掉的节点不参与「一键生成分身」，但保留记录）。
    ///
    /// ⚠️ 用 `default = "default_true"` 而非裸 `#[serde(default)]`：后者对 bool 是
    /// `false`，会让**回滚再升级后所有节点变禁用** → 池空 → 一键生成分身全部落直连。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 最近一次测速结果（沿用 `/proxy/test` 的语义；None = 从未测过）。
    #[serde(default)]
    pub last_test: Option<SocksNodeTest>,
    /// 创建时间（Unix 秒）。
    #[serde(default)]
    pub created_at: u64,
}

/// 最近一次代理测活的结果快照（前端在节点卡片上直接渲染）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SocksNodeTest {
    pub ok: bool,
    #[serde(default)]
    pub latency_ms: u64,
    #[serde(default)]
    pub exit_ip: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    /// 测试时刻（Unix 秒），用于前端显示「N 分钟前」。
    #[serde(default)]
    pub tested_at: u64,
}

fn default_true() -> bool {
    true
}

/// 手写脱敏 Debug：`password`（代理密码）与 `url` 里可能残留的 userinfo 都不进 `{:?}`。
///
/// 为什么不能派生：本类型既落盘（`Serialize` 必须保留 `password`，**绝不能**
/// `skip_serializing`，否则下一次保存就把密码抹掉）又会被诊断路径碰到 —— 派生 Debug 会让
/// 任何一条 `{:?}`（日志 / 错误链 / panic 消息）把代理密码原文写出去。与 `credentials.rs`
/// 的 `KiroCredentials` 同款约定：在类型层面脱敏，杜绝调用点意外泄密。
impl std::fmt::Debug for SocksNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SocksNode")
            .field("id", &self.id)
            .field("name", &self.name)
            // 剥 userinfo 再打：`normalize` 已保证入表 URL 干净，这里是第二道保险
            // （直接 `from_str` 构造出的节点不经过 normalize）。
            .field(
                "url",
                &crate::http_client::split_proxy_credentials(&self.url).0,
            )
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<set>"))
            .field("enabled", &self.enabled)
            .field("last_test", &self.last_test)
            .field("created_at", &self.created_at)
            .finish()
    }
}

/// 节点表的**落盘形态**：节点数组 + 已发放 id 的高水位。
///
/// # 为什么不能只存数组
///
/// 只存数组时新 id 只能靠 `max(现有 id) + 1` 现算，而删掉最大 id 的那个节点后
/// 这个算式立刻把该 id 重新发出去。后果是「已绑该 id 的东西指向一个无关新节点」：
/// 面板另一个标签页还持有删除前的列表，点它的「测活」会打到新节点上；
/// 将来若把节点 id 写进凭据，绑定关系会静默错位。
///
/// 高水位只增不减，所以 id 一旦发放就永不复用，重启也不会。
///
/// # 兼容
///
/// `#[serde(untagged)]` 让**裸数组**也能读（本结构引入前的开发期文件形态）。
/// 读到裸数组时高水位按数组里的最大 id 补齐，之后按新形态回写。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SocksNodeFile {
    pub nodes: Vec<SocksNode>,
    /// 下一个要发放的 id（严格大于历史上发放过的任何 id）。
    #[serde(default)]
    pub next_id: u64,
}

/// 磁盘上可能是新形态（对象）也可能是旧形态（裸数组）。
#[derive(serde::Deserialize)]
#[serde(untagged)]
pub enum SocksNodeFileCompat {
    Structured(SocksNodeFile),
    BareArray(Vec<SocksNode>),
}

impl SocksNodeFileCompat {
    /// 归一化成 `(nodes, next_id)`。`next_id` 至少是 `max(id) + 1`。
    ///
    /// 顺带把每个节点的 `url` 里可能内嵌的账密拆到 `username`/`password`（见
    /// [`SocksNode::normalized_url`]）—— 这是**加载边界**，也是唯一能覆盖「手改文件 /
    /// 旧版本数据」的时机。
    pub fn normalize(self) -> (Vec<SocksNode>, u64) {
        let (nodes, stored_next) = match self {
            Self::Structured(f) => (f.nodes, f.next_id),
            Self::BareArray(v) => (v, 0),
        };
        let nodes: Vec<SocksNode> = nodes
            .into_iter()
            .map(SocksNode::normalized_url)
            .collect();
        let floor = nodes.iter().map(|n| n.id).max().unwrap_or(0) + 1;
        let next = stored_next.max(floor);
        (nodes, next)
    }
}

impl SocksNode {
    /// 前端展示用标签：`name` 优先，空则回落 `url`。
    pub fn display_label(&self) -> &str {
        if self.name.trim().is_empty() {
            &self.url
        } else {
            &self.name
        }
    }

    /// 把 `url` 里可能内嵌的账密拆到 `username` / `password`，返回规范化后的节点。
    ///
    /// 为什么必须在**加载边界**做：`url` 字段的契约是「内嵌账密在入表时已被拆到下面两字段
    /// （避免密码明文留在 URL 里）」，面板视图（`admin::types::SocksNodeView`）据此把 `url`
    /// 原样外传、同时只给 `has_password` —— 若磁盘上残留 `socks5://user:pass@host:port`
    /// （手改文件 / 旧版本数据），密码就会从 `url` 字段漏到面板，`has_password` 的脱敏
    /// 形同虚设。
    ///
    /// 优先级与写入路径（`set_credential_proxy` / `upsert_socks_node`）**逐字一致**：
    /// 显式字段优先，缺失（或空白）的一侧才用 URL 内嵌值补。
    fn normalized_url(mut self) -> Self {
        let (clean, inline_user, inline_pass) =
            crate::http_client::split_proxy_credentials(&self.url);
        if clean == self.url {
            // 绝大多数节点：URL 本就没有 userinfo，零改动（含 `direct` / 空串）。
            return self;
        }
        self.url = clean;
        if self.username.as_deref().unwrap_or("").is_empty() {
            self.username = inline_user;
        }
        if self.password.as_deref().unwrap_or("").is_empty() {
            self.password = inline_pass;
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ⭐ `enabled` 缺字段时必须是 **true**。
    ///
    /// 回退即 FAIL：把 `default = "default_true"` 改成裸 `#[serde(default)]`
    /// → 旧节点文件（或回滚再升级后写下的文件）反序列化出 `enabled=false`
    /// → 可分配节点池为空 → 「一键生成分身」静默全部落直连（无代理），
    /// 而用户以为每份都有独立出口 IP。
    #[test]
    fn enabled_defaults_to_true_when_absent() {
        let json = r#"{"id":1,"url":"socks5://h:1080"}"#;
        let n: SocksNode = serde_json::from_str(json).expect("缺字段必须能反序列化");
        assert!(
            n.enabled,
            "enabled 缺字段时必须默认 true，否则节点池会静默变空"
        );
        assert_eq!(n.id, 1);
        assert!(n.name.is_empty());
        assert!(n.last_test.is_none());
    }

    /// 显式 false 必须被尊重（否则「禁用节点」这个功能不存在）。
    #[test]
    fn explicit_disabled_is_respected() {
        let json = r#"{"id":2,"url":"socks5://h:1080","enabled":false}"#;
        let n: SocksNode = serde_json::from_str(json).unwrap();
        assert!(!n.enabled);
    }

    #[test]
    fn display_label_falls_back_to_url_when_name_blank() {
        let mut n: SocksNode =
            serde_json::from_str(r#"{"id":1,"url":"socks5://h:1080","name":"  "}"#).unwrap();
        assert_eq!(n.display_label(), "socks5://h:1080");
        n.name = "JP-1".into();
        assert_eq!(n.display_label(), "JP-1");
    }

    /// 序列化必须是 camelCase（与面板其余端点同口径）。
    #[test]
    fn wire_format_is_camel_case() {
        let n = SocksNode {
            id: 7,
            name: "JP".into(),
            url: "socks5://h:1080".into(),
            username: Some("u".into()),
            password: Some("p".into()),
            enabled: true,
            last_test: Some(SocksNodeTest {
                ok: true,
                latency_ms: 12,
                exit_ip: Some("1.2.3.4".into()),
                error: None,
                tested_at: 100,
            }),
            created_at: 50,
        };
        let s = serde_json::to_string(&n).unwrap();
        assert!(s.contains("\"lastTest\""), "应为 camelCase: {s}");
        assert!(s.contains("\"latencyMs\""));
        assert!(s.contains("\"exitIp\""));
        assert!(s.contains("\"testedAt\""));
        assert!(s.contains("\"createdAt\""));
    }

    /// 加载边界必须把 URL 里的账密拆出来：`url` 字段的契约是「内嵌账密已被拆到
    /// username/password」，而面板视图（`SocksNodeView`）原样外传 `url`、只给
    /// `has_password` —— 不拆就等于把代理密码从 `url` 漏到面板。
    ///
    /// 回退即 FAIL：去掉 `normalize` 里的 `normalized_url`，`nodes[0].url` 会带着明文账密。
    #[test]
    fn normalize_splits_inline_credentials_out_of_url() {
        let raw = r#"{"nodes":[
            {"id":1,"url":"socks5://inlineuser:inlinepass@h:1080"},
            {"id":2,"url":"socks5://inlineuser:inlinepass@h:1080","username":"explicit","password":"explicitpass"}
        ],"nextId":9}"#;
        let file: SocksNodeFile = serde_json::from_str(raw).expect("解析应成功");
        let (nodes, next) = SocksNodeFileCompat::Structured(file).normalize();
        assert_eq!(next, 9, "高水位不受影响");
        assert_eq!(nodes[0].url, "socks5://h:1080", "userinfo 必须从 url 拆走");
        assert_eq!(nodes[0].username.as_deref(), Some("inlineuser"));
        assert_eq!(nodes[0].password.as_deref(), Some("inlinepass"));
        // 显式字段优先：URL 内嵌值不得覆盖已有账密（与写入路径同口径）。
        assert_eq!(nodes[1].url, "socks5://h:1080");
        assert_eq!(nodes[1].username.as_deref(), Some("explicit"));
        assert_eq!(nodes[1].password.as_deref(), Some("explicitpass"));
    }

    /// Debug 不得泄露代理密码（`url` 里残留的 userinfo 同样不得输出）。
    ///
    /// 回退即 FAIL：把 `impl Debug` 换回 `#[derive(Debug)]`，两个断言立刻变红。
    #[test]
    fn debug_masks_password_and_url_userinfo() {
        let n: SocksNode = serde_json::from_str(
            r#"{"id":3,"url":"socks5://u:SECRETPASS@h:1080","password":"SECRETPASS2"}"#,
        )
        .unwrap();
        let dbg = format!("{n:?}");
        assert!(!dbg.contains("SECRETPASS"), "url 内嵌账密不得进 Debug: {dbg}");
        assert!(!dbg.contains("SECRETPASS2"), "password 不得进 Debug: {dbg}");
        assert!(dbg.contains("socks5://h:1080"), "干净 URL 仍应可见: {dbg}");
        assert!(dbg.contains("id: 3"), "id 仍应可见: {dbg}");
    }
}
