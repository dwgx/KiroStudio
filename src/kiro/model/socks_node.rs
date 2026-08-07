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
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
    pub fn normalize(self) -> (Vec<SocksNode>, u64) {
        let (nodes, stored_next) = match self {
            Self::Structured(f) => (f.nodes, f.next_id),
            Self::BareArray(v) => (v, 0),
        };
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
}
