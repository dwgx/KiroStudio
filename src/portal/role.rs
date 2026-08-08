//! Portal 用户角色。
//!
//! # 为何是独立模块而非一个字符串字段
//! 角色决定「谁能改余额、谁能删号」。若在各处用裸字符串比较（`role == "admin"`），
//! 那么拼错一个字母的后果是**静默降权或静默提权**，且编译期毫无提示。收进一个
//! 枚举后，非法值在解析处就被挡住，越权判断只有一条路径。
//!
//! # 三档的划分依据
//! 不是按「功能多少」分档，而是按**破坏力**分档：
//! - [`RoleKind::Readonly`] 不能花钱、不能取明文 —— 给演示与审计账号
//! - [`RoleKind::User`] 能花自己的钱取明文 —— 现有全部用户的语义
//! - [`RoleKind::Admin`] 能改别人的钱 —— 运营者
//!
//! # 与 adminApiKey 的关系
//! 完全独立。`adminApiKey` 是「部署者」的凭证（能改配置、增删凭据、看全部日志）；
//! 这里的 admin 是「运营者」的身份（只能管 portal 内的用户与余额）。把运营授权给
//! 一个 portal 账号，不必把 `adminApiKey` 交出去 —— 那把钥匙能做的事远超运营需要。

use std::fmt;

/// 用户角色。
///
/// 序列化成小写字符串入库（`portal_users.role`），因为库里已有的行是迁移时
/// 用 `DEFAULT 'user'` 填的字面量 —— 存储表示必须与之一致，否则老行读不出来。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RoleKind {
    /// 只读：能看车队与自己的钱包，**不能上车**（不花钱、不取明文）。
    Readonly,
    /// 普通用户：现有行为，能上车。**这是默认档**。
    User,
    /// 运营管理员：能管 portal 用户与余额。
    Admin,
}

impl RoleKind {
    /// 库里的字符串表示。**改这里等于改存储格式**，会让所有已有行失效。
    pub fn as_str(self) -> &'static str {
        match self {
            RoleKind::Readonly => "readonly",
            RoleKind::User => "user",
            RoleKind::Admin => "admin",
        }
    }

    /// 从库里/请求里读回来。
    ///
    /// # 为何未知值回退到 `User` 而不是报错
    /// 这个函数的调用点是「读一行用户记录」。若未知值让整行读取失败，那么一条被
    /// 手工改坏的 role（或未来版本写入的新角色名，在旧版本回滚后读到）会让这个
    /// **用户彻底无法登录** —— 一个显示问题升级成了拒绝服务。
    ///
    /// # 为何回退到 `User` 而不是 `Readonly` 或 `Admin`
    /// - 回退到 `Admin` 是提权：改坏一个字符就获得管理权，不可接受。
    /// - 回退到 `Readonly` 更"安全"，但会让**现有用户突然不能上车**——而绝大多数
    ///   未知值的来源是打字错误，惩罚不该落在用户的正常使用上。
    /// - `User` 是迁移默认值，也是老库全部行的实际值：回退到它等于「按老行为对待」，
    ///   与本项目「默认关闭、升级后行为不变」的原则一致。
    ///
    /// 未知值会打 warn 日志（在 [`Self::parse_logged`]），不静默。
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "admin" => RoleKind::Admin,
            "readonly" => RoleKind::Readonly,
            _ => RoleKind::User,
        }
    }

    /// 同 [`Self::parse`]，但未知值会打一条 warn。
    ///
    /// 读库路径用这个：静默回退会让「role 被写坏」这件事永远不被发现，
    /// 直到某个运营人员发现自己点不动按钮而没人知道为什么。
    pub fn parse_logged(s: &str, context: &str) -> Self {
        let parsed = Self::parse(s);
        if parsed == RoleKind::User
            && !matches!(s.trim().to_ascii_lowercase().as_str(), "user" | "")
        {
            tracing::warn!(
                "Portal 角色值无法识别，已按普通用户对待：role={:?} 出处={}",
                s,
                context
            );
        }
        parsed
    }

    /// 严格解析，供**入口校验**用（管理员设置某人角色时）。
    ///
    /// 与 [`Self::parse`] 的宽容相反：这里必须拒绝未知值。写入时放过一个错别字，
    /// 等于把它永久留在库里，此后每次读都要靠上面那条回退逻辑兜——而兜住的结果
    /// 未必是管理员当时想设的那一档。
    pub fn parse_strict(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "admin" => Some(RoleKind::Admin),
            "user" => Some(RoleKind::User),
            "readonly" => Some(RoleKind::Readonly),
            _ => None,
        }
    }

    /// 能否上车（花积分换明文）。
    ///
    /// admin 也能上车：运营者本身也是用户，且需要能自测「上车这条路是通的」。
    pub fn can_board(self) -> bool {
        matches!(self, RoleKind::User | RoleKind::Admin)
    }

    /// 能否看到明文 key。
    ///
    /// # 为何不复用 [`Self::can_board`]
    /// 今天两者答案相同（都是 user + admin），但问的是两件不同的事：
    /// `can_board` 是「能不能花分」，这一条是「能不能看到明文」。
    /// **积分关闭时明文是直接下发的**，那条路径上根本没有"上车"这一步——
    /// 若用 `can_board` 兼任，日后有人调整上车权限（比如给 readonly 开放
    /// 免费上车做演示），明文闸门会跟着一起松开，而那是本仓最不能松的一道门。
    /// 两个名字各自表达一件事，改一个不会牵动另一个。
    pub fn can_reveal_plaintext(self) -> bool {
        matches!(self, RoleKind::User | RoleKind::Admin)
    }

    /// 能否使用 portal 内的运营管理端点。
    pub fn can_manage(self) -> bool {
        matches!(self, RoleKind::Admin)
    }

    /// 中文展示名（界面用）。
    pub fn label(self) -> &'static str {
        match self {
            RoleKind::Readonly => "只读",
            RoleKind::User => "普通用户",
            RoleKind::Admin => "运营管理员",
        }
    }

    /// 全部角色，供界面下拉与测试穷举。
    ///
    /// 【为何要有这个】权限矩阵测试必须遍历**所有**角色，而手写列表会在新增
    /// 第四档时漏掉——漏掉的表现是新角色的越权路径根本没被测过。
    pub const ALL: [RoleKind; 3] = [RoleKind::Readonly, RoleKind::User, RoleKind::Admin];
}

impl Default for RoleKind {
    /// 默认普通用户 —— 与迁移的 `DEFAULT 'user'` 一致。
    fn default() -> Self {
        RoleKind::User
    }
}

impl fmt::Display for RoleKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 存储表示必须与迁移写入的字面量一致。
    ///
    /// 【为何单独测】改 `as_str` 的返回值不会有任何编译错误，但会让库里
    /// 所有已有行读不回来（`parse` 认不出 → 全部退化成 User → 运营者集体失去权限）。
    #[test]
    fn storage_strings_are_stable() {
        assert_eq!(RoleKind::Admin.as_str(), "admin");
        assert_eq!(RoleKind::User.as_str(), "user");
        assert_eq!(RoleKind::Readonly.as_str(), "readonly");
    }

    /// 往返：每一档都能存进去再读回来，且不变。
    #[test]
    fn roundtrip_through_storage_string() {
        for r in RoleKind::ALL {
            assert_eq!(RoleKind::parse(r.as_str()), r, "往返失败: {r:?}");
            assert_eq!(RoleKind::parse_strict(r.as_str()), Some(r));
        }
    }

    /// 未知值按普通用户对待，**绝不提权**。
    ///
    /// 这是本模块最重要的一条：回退到 admin 意味着改坏一个字符就能获得管理权。
    ///
    /// # 「未知值」与「同一个值的不同写法」的界线
    /// 这条界线必须说清，否则测试会自相矛盾（本用例第一版就把 `"admin "` 列进了
    /// 下面的清单，同时另一个用例断言它该被解析成 Admin）。
    ///
    /// - **大小写与首尾空白只是同一个值的书写差异**：`"Admin"`、`"admin "`、`"ADMIN\n"`
    ///   表达的都是 admin 这一档。把它们判成未知反而危险——运营者手工改了库、
    ///   多敲一个空格，就会在无任何提示的情况下失去管理权。
    /// - **除此以外的任何差异都是另一个值**：`"administrator"`、`"root"`、`"adm1n"`、
    ///   `"'admin'"` 与 admin 是不同的字符串，绝不能因为「看起来像」就放行。
    ///
    /// 下面的清单只放后者。前者由 [`parse_tolerates_case_and_whitespace`] 覆盖。
    #[test]
    fn unknown_never_escalates() {
        for bad in [
            "",
            "  ",
            "ADMINISTRATOR",
            "root",
            "superuser",
            "adm1n",
            "owner",
            "user2",
            "read-only",
            "'admin'",
            "admin;--",
            "admin,user",
            "sudo admin",
            "admin\u{200b}", // 零宽空格：trim 不会去掉它，必须判为未知
        ] {
            let got = RoleKind::parse(bad);
            assert_ne!(
                got,
                RoleKind::Admin,
                "未知/畸形值 {bad:?} 被解析成了 Admin —— 这是提权"
            );
            assert!(!got.can_manage(), "未知值 {bad:?} 竟获得管理权限");
        }
    }

    /// 大小写与首尾空白不敏感 —— 那是同一个值的不同写法，不是另一个值。
    ///
    /// 【为何要容忍】库里的 role 可能被运营者用 sqlite3 手工改过。多一个空格、
    /// 首字母大写，都不该让一个管理员静默失去权限——那种故障没有任何提示，
    /// 只表现为「按钮点不动」，排查时几乎不会想到是尾随空格。
    #[test]
    fn parse_tolerates_case_and_whitespace() {
        for variant in ["ADMIN", "Admin", "  admin  ", "admin\n", "\tADMIN\t"] {
            assert_eq!(
                RoleKind::parse(variant),
                RoleKind::Admin,
                "{variant:?} 是 admin 的书写变体，不该被判成未知"
            );
        }
        assert_eq!(RoleKind::parse("ReadOnly"), RoleKind::Readonly);
        assert_eq!(RoleKind::parse(" USER "), RoleKind::User);
        // 对照组：确认上面的通过不是因为一切都被当成 Admin。
        assert_eq!(RoleKind::parse("nonsense"), RoleKind::User);
        assert_eq!(RoleKind::parse("readonly"), RoleKind::Readonly);
    }

    /// 严格解析必须拒绝未知值 —— 写入路径不能放过错别字。
    #[test]
    fn strict_parse_rejects_unknown() {
        for bad in ["", "administrator", "adm1n", "root", "user1", "owner"] {
            assert_eq!(RoleKind::parse_strict(bad), None, "严格解析放过了 {bad:?}");
        }
        // 对照组：合法值必须能过，否则上面全 None 可能只是函数恒返回 None。
        assert_eq!(RoleKind::parse_strict("admin"), Some(RoleKind::Admin));
    }

    /// 权限矩阵：谁能上车、谁能管理。
    ///
    /// 【为何写成穷举表而非逐个断言】新增角色时 `ALL` 会变长，而这张表若漏了
    /// 新角色，下面的长度断言会失败——强制新角色必须在这里明确它的权限。
    #[test]
    fn permission_matrix_is_explicit() {
        // (角色, 能上车, 能管理, 能见明文)
        let table = [
            (RoleKind::Readonly, false, false, false),
            (RoleKind::User, true, false, true),
            (RoleKind::Admin, true, true, true),
        ];
        assert_eq!(
            table.len(),
            RoleKind::ALL.len(),
            "新增角色后必须在本表里明确它能否上车/管理/见明文"
        );
        for (role, can_board, can_manage, can_reveal) in table {
            assert_eq!(role.can_board(), can_board, "{role:?} 的上车权限不符");
            assert_eq!(role.can_manage(), can_manage, "{role:?} 的管理权限不符");
            assert_eq!(
                role.can_reveal_plaintext(),
                can_reveal,
                "{role:?} 的明文权限不符"
            );
        }
    }

    /// **只读账号在任何配置下都拿不到明文。**
    ///
    /// 【为何要单独一条】`can_reveal` 与 `can_board` 目前答案相同，容易被后来者
    /// 合并成一个方法。但它们回答的是不同问题：上车是「能否花钱」，见明文是
    /// 「能否看到 key」。积分**关闭**时没有上车这回事，此时若只有 can_board 这一道，
    /// 只读账号会看到全部明文——而只读的整个用途就是「能看统计但拿不到 key」。
    #[test]
    fn readonly_can_never_reveal_regardless_of_credits() {
        assert!(
            !RoleKind::Readonly.can_reveal_plaintext(),
            "只读账号获得了明文权限——这让「只读」这一档失去意义"
        );
        // 对照组：另外两档必须能见，否则上面的通过可能只是该方法恒 false。
        assert!(RoleKind::User.can_reveal_plaintext());
        assert!(RoleKind::Admin.can_reveal_plaintext());
    }

    /// 默认档必须是 User —— 与迁移默认值一致，保证老用户升级后行为不变。
    #[test]
    fn default_is_plain_user() {
        assert_eq!(RoleKind::default(), RoleKind::User);
        assert_eq!(RoleKind::default().as_str(), "user");
        assert!(RoleKind::default().can_board(), "老用户升级后必须仍能上车");
        assert!(!RoleKind::default().can_manage());
    }

    /// JSON 序列化用小写，与库内表示一致（前端拿到的值可直接回传）。
    #[test]
    fn json_uses_lowercase() {
        assert_eq!(
            serde_json::to_string(&RoleKind::Admin).unwrap(),
            "\"admin\""
        );
        assert_eq!(
            serde_json::from_str::<RoleKind>("\"readonly\"").unwrap(),
            RoleKind::Readonly
        );
    }
}
