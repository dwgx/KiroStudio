//! Portal 独立存储：多用户账号、会话、推送凭据元数据、审计日志。
//!
//! # 为什么独立于 admin
//! `/api/admin` 走单一共享 `adminApiKey`，没有「用户」概念。Portal 是**多用户**、
//! 面向公网的只读查看入口，鉴权/会话/审计与 admin 完全不共享状态——admin 密钥泄露
//! 不影响 portal，portal 用户被撞库也拿不到管理面。
//!
//! # 绝不在这里存明文 key
//! [`ImportKeyMeta`] 只存元数据（指纹、`credential_id`、region、endpoint、时间戳）。
//! 明文在**请求时**按 `credential_id` 从活的凭据池回查，理由：
//! - 磁盘上不出现第二份 key 副本（现有 credentials.json 已有 at-rest 加密，再抄一份等于绕过它）；
//! - 凭据被删除后自动停止外显，不需要额外的级联清理；
//! - 历史元数据仍然查得到，「推过什么、什么时候推的」不丢。
//!
//! # 与 [`crate::common::import_stats`] 的分工
//! 那个模块是进程级、重启归零的运营计数（最近 20 次推送）。本模块是**持久**的按 key
//! 视图，供 portal 用户查历史。两者并存，互不替代。

use std::path::Path;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};

use super::credits::Pricing;

/// Portal 用户（`password_hash` 是 argon2id PHC 串，绝不外发）。
#[derive(Debug, Clone)]
pub struct PortalUser {
    pub id: i64,
    pub username: String,
    pub password_hash: String,
    pub disabled: bool,
    pub created_at_ms: i64,
    pub last_login_ms: Option<i64>,
    /// 角色。登录成功时要把它带进会话响应，让页面知道自己能不能上车。
    pub role: super::role::RoleKind,
}

/// 用户 + 余额，供 admin 列表用。
///
/// # 为何不复用 [`PortalUser`]
/// 那个类型带 `password_hash`——它是 argon2 校验的必需输入，但也是离线爆破的原料。
/// admin 列表要序列化成 JSON 发给浏览器，若沿用带 hash 的类型，「别把这个字段填进
/// 响应」就变成一条要靠人记住的纪律；而纪律会在某次赶时间的改动里失效，且没有
/// 任何编译错误提示。这里换一个**结构上就没有该字段**的类型，泄漏所需的那行代码
/// 根本写不出来。
///
/// 对应的查询也显式列字段而非 `SELECT u.*`——后者在日后给 `portal_users` 加列时
/// 会自动把新列捎带出来，包括不该外发的。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserWithBalance {
    pub id: i64,
    pub username: String,
    pub disabled: bool,
    pub created_at_ms: i64,
    pub last_login_ms: Option<i64>,
    /// 当前余额。无 `portal_balances` 行时为 0（新用户尚未充过值）。
    pub balance: i64,
    pub topup: i64,
    pub spent: i64,
    /// 已上车的车辆数。管理员判断「这个号在用吗」比看最后登录时间更直接。
    pub aboard_count: i64,
    /// 角色。序列化成小写字符串（`"admin"` / `"user"` / `"readonly"`）。
    ///
    /// 【为何列表里要带它】改角色前得先看清现在是什么；而若只在详情接口返回，
    /// 运营者就得逐个点开才知道谁是管理员——那正是最需要一眼看全的信息。
    pub role: super::role::RoleKind,
}

/// 校验会话后拿到的身份（只带下游需要的字段，不带 hash）。
#[derive(Debug, Clone)]
pub struct PortalSession {
    pub user_id: i64,
    pub username: String,
    pub expires_at_ms: i64,
    /// 该用户当前角色。
    ///
    /// # 为何随会话一起查出来，而不是在需要时再查一次库
    /// 越权判断散落在多个 handler 里。若每处自己去查角色，就有处忘了查的可能，
    /// 而「忘了查」的表现是那一处**无权限校验**——最危险的 bug 形态：功能正常、
    /// 测试正常、只是任何人都能调。让角色成为身份的一部分，handler 拿到 session
    /// 就必然拿到角色，漏判需要显式忽略一个已在手上的字段，比忘记查询难得多。
    ///
    /// # 时效性
    /// 这是**会话校验那一刻**的角色。管理员改某人角色后，该用户当前请求仍用旧角色，
    /// 下一个请求就会拿到新值（每次请求都重新 validate）。不缓存跨请求。
    pub role: super::role::RoleKind,
}

/// 一个被推送过来的 key 的**元数据**。明文不在此。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportKeyMeta {
    /// 完整 SHA-256 hex，主键。
    ///
    /// 【为何不用 8 位指纹当主键】指纹只有 32 bit，几百个 key 就有可观的撞车概率，
    /// 撞了会被 `ON CONFLICT` 静默合并成一条、丢掉一个号的记录。全摘要做主键杜绝此事，
    /// 指纹只作展示。
    pub key_hash: String,
    /// SHA-256 前 8 位，与凭据管理页/推送响应同源，肉眼对账用。
    pub fingerprint: String,
    /// 落库后的凭据 ID —— **请求时据此回查明文**。`None` = 当次推送失败，没落库。
    pub credential_id: Option<i64>,
    pub region: Option<String>,
    pub endpoint: Option<String>,
    pub first_seen_ms: i64,
    pub last_seen_ms: i64,
    /// 这个 key 被推送过几次（幂等重推会累加）。
    pub push_count: i64,
    pub ok: bool,
    pub error: Option<String>,
}

/// 审计条目。登录成功/失败、登出、每一次明文外显都记一条。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditEntry {
    pub id: i64,
    pub at_ms: i64,
    pub username: Option<String>,
    pub action: String,
    pub client_ip: Option<String>,
    pub detail: Option<String>,
}

/// 审计查询条件。全部字段为 `None` 即「不筛，按时间倒序取最近的」。
///
/// # 为何用结构体而不是一串函数参数
/// 筛选维度只会变多（日后要按 credential、按结果成败）。七个 `Option` 参数的函数
/// 签名在新增第八个时会让所有调用点重写一遍，而漏改一处的表现是**筛错了但不报错**。
#[derive(Debug, Clone, Default)]
pub struct AuditQuery {
    /// 用户名**精确**匹配。
    ///
    /// 【为何不做子串搜索】子串（`LIKE '%alice%'`）用不上索引，审计表是全表最大的
    /// 表之一，公网撞库还会持续写放大；更要紧的是它有歧义：搜 `alice` 会连
    /// `alice2`、`malice` 一起捞出来，而审计的用途是「这个人干了什么」，多捞一个
    /// 同前缀的人就是把两个人的行为混在一起看。要模糊找人应该先在用户列表里找到
    /// 准确的名字，再来看他的审计。
    pub username: Option<String>,
    /// 动作**精确**匹配（如 `login_ok`）。
    pub action: Option<String>,
    /// 动作**前缀**匹配（如 `login_fail` 匹配全部登录失败原因）。
    ///
    /// 【为何单独一个字段而不是让 `action` 支持 `*` 通配】那要解析魔法字符串，
    /// 而「用户名里正好有个星号」之类的边角会变成一处静默的行为差异。两个字段
    /// 各自语义明确，且**可以同时为 None**（不筛）或分别使用。
    pub action_prefix: Option<String>,
    /// 时间下界（含）。
    pub since_ms: Option<i64>,
    /// 时间上界（含）。
    pub until_ms: Option<i64>,
    /// 跳过多少条。
    pub offset: i64,
    /// 取多少条。由调用方钳到 [`AUDIT_PAGE_MAX`] / [`AUDIT_EXPORT_MAX`]。
    pub limit: i64,
}

impl AuditQuery {
    /// 把筛选条件压成一行人读的说明，写进审计的 `detail`。
    ///
    /// # 为何导出这件事本身要留痕、且要留下筛选条件
    /// 导出会把**含每个用户完整 IP** 的审计表变成一个文件带离系统边界。只记
    /// 「某人导出了审计」而不记筛了什么，事后就回答不了「他带走的是哪一批」——
    /// 而那恰恰是唯一有意义的问题。审计系统不覆盖对审计自身的访问，是原则性缺口。
    ///
    /// # 为何不直接 `format!("{self:?}")`
    /// `Debug` 输出会随字段增删而变，且把 `Some("alice")` 这种 Rust 语法写进
    /// 运营要读的审计里。这里显式拼，只列**用上了的**条件；什么都没筛就说「全部」,
    /// 因为「全部」正是最该看清的那种导出。
    pub fn describe_filter(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(u) = &self.username {
            parts.push(format!("username={u}"));
        }
        if let Some(a) = &self.action {
            parts.push(format!("action={a}"));
        }
        if let Some(p) = &self.action_prefix {
            parts.push(format!("actionPrefix={p}"));
        }
        if let Some(s) = self.since_ms {
            parts.push(format!("sinceMs={s}"));
        }
        if let Some(u) = self.until_ms {
            parts.push(format!("untilMs={u}"));
        }
        if parts.is_empty() {
            // 【为何这里不带 `filter=` 前缀】另一条分支返回的是裸 `username=alice`，
            // 前缀由调用方加。这一支若自带前缀，「什么都没筛」的导出会在审计里
            // 写成 `filter=filter=none(all)`——实测线上就是这么出来的。
            // 两条分支的返回值格式必须同构，否则调用方无论怎么拼都有一半是错的。
            "none(all)".to_string()
        } else {
            parts.join(" ")
        }
    }
}

/// 一页审计结果。
///
/// # 为何要带 `total`
/// 没有总数就只能显示「下一页」而无法显示「共 N 条 / 第 x 页」，而运营看审计时
/// 第一个问题往往是「一共多少条」——比如判断某个 IP 撞了多少次。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditPage {
    pub rows: Vec<AuditEntry>,
    /// 满足筛选条件的**总条数**（不受 offset/limit 影响）。
    pub total: i64,
    pub offset: i64,
    pub limit: i64,
    /// 后面还有没有。
    ///
    /// 【为何服务端算而不是让前端比较 offset+len<total】那个比较写错一次（比如用
    /// `<=`）就会让末页多出一个点不动的「下一页」按钮，而这种错前端测不到。
    pub has_more: bool,
}

/// 一种动作及其出现次数。供筛选下拉用。
///
/// 【为何带上次数】下拉里光有动作名，运营不知道哪个值得看。带上次数，
/// 「login_fail_bad_password 812 次」本身就是信息。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionCount {
    pub action: String,
    pub count: i64,
}

/// 审计分页一次最多返回多少条。
pub const AUDIT_PAGE_MAX: i64 = 200;

/// 导出一次最多多少条。
///
/// 【为何比分页大得多又仍然有上限】导出的用途是拿去 Excel 里慢慢看，几百条不够用；
/// 但无上限意味着一个 GET 就能让服务端把整张表拼成一个字符串驻留在内存里——
/// 那是一条不需要任何权限升级的放大路径（管理员账号被盗即可打满内存）。
pub const AUDIT_EXPORT_MAX: i64 = 5000;

/// 一个时间窗内的运营总量。
///
/// # 为何按 `kind` 分列而不是只给一个净额
/// 净额把「充了 100 花了 100」和「什么都没发生」压成同一个 0。运营要回答的问题
/// （今天发了多少票、进了多少分、退了多少）各自对应不同动作，合并之后无法还原。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardWindow {
    /// 发票数 = 窗内 `portal_unlocks` 行数（一次上车一张票）。
    pub tickets: i64,
    /// 充值总额（`kind='topup'` 的 delta 之和，恒为正）。
    pub topup: i64,
    /// 消费总额（`kind='unlock'` 的 delta 取反之和，恒为正）。
    ///
    /// 存正数而不是原始负 delta：界面上「消费 120 分」比「消费 -120 分」少一次
    /// 心算，而符号信息在 `kind` 里已经有了，不会因此丢失。
    pub spend: i64,
    /// 退款总额（`kind='refund'` 的 delta 之和，恒为正）。
    pub refund: i64,
    /// 管理员调账净额（`kind='admin_adjust'`）。**可正可负**——这一项是唯一
    /// 允许双向的，加分和扣分都走它，合并成净额是有意的：运营关心的是「管理员
    /// 一共动了多少」，而每一笔的方向在流水里逐条可查。
    pub adjust: i64,
}

/// 一辆车（一把 key）的热度。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyHeat {
    pub credential_id: i64,
    /// 乘客数。
    pub passengers: i64,
    /// 当前单价（价格快照算出）。无快照时 `None`——理论上不该出现（有乘客就
    /// 必有快照），出现即说明数据被外部动过，如实报 null 而不是编一个 0。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit_price: Option<i64>,
    /// 当前在册收入 = `SUM(paid)`。
    ///
    /// 【注意这不是历史累计收款】差额退款模型下 `paid` 会随新人上车被下调，
    /// 所以这个值等于「若此刻结算，这辆车一共收了多少」。历史累计要看流水。
    pub revenue: i64,
    pub first_boarded_ms: i64,
    pub last_boarded_ms: i64,
}

/// 用户分层。三档**互斥且穷尽**，`active + broke + zombie == total`。
///
/// # 为何要穷尽
/// 分层的用途是「一眼看出用户都卡在哪一环」。若有用户落不进任何一档，那个数字
/// 就永远对不上总数，而看板上对不上的数字会让人怀疑所有数字。穷尽让它可断言。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserTiers {
    pub total: i64,
    /// 上过车且还有余额。
    pub active: i64,
    /// 上过车但余额已空——**最该被充值提醒的一档**。
    pub broke: i64,
    /// 注册了从未上车。
    pub zombie: i64,
    /// 已停用。**与上面三档交叉而非并列**（停用的号也落在某一档里），
    /// 所以它不参与那条恒等式。
    pub disabled: i64,
}

/// 一个来源 IP 的登录失败次数。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginFailRow {
    /// 来源 IP。审计里可能为空（取不到时），如实报 null。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_ip: Option<String>,
    pub count: i64,
}

/// 账目自检。**看板自己报告这些数字是否可信。**
///
/// # 为何把自检做进产品而不只做进测试
/// 测试证明的是「这份代码在测试数据上自洽」。生产库经历过升级、并发、手工干预，
/// 而对账错误的表现是「某个数字慢慢偏了」——没有任何异常日志。把恒等式算进
/// 响应里，偏差在看板上第一时间可见，不必等到有人对不上账再回头查。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrityCheck {
    /// 所有在册用户的余额之和。
    pub balance_sum: i64,
    /// 在册用户的流水 delta 之和。
    ///
    /// 【为何要按「在册」过滤】`portal_balances` 有 `ON DELETE CASCADE`，删号即
    /// 清余额；而 `portal_ledger` **故意没有外键**（删号后仍要能回答「这个号花了
    /// 多少」）。不过滤的话，删掉任何一个有流水的用户都会让这条恒等式永久失衡，
    /// 于是一个健康的库被报成账目异常——那样的自检会被当成噪音而被忽略，
    /// 比没有自检更糟。
    pub ledger_sum: i64,
    /// `balance != topup - spent` 的用户数。正常恒为 0。
    pub wallet_violations: i64,
    /// 两条恒等式是否都成立。
    pub ok: bool,
}

/// 看板快照。一次查询取全，避免界面为每个区块各发一次请求（那样各区块看到的
/// 是不同时刻的库，人数与收入会互相矛盾）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardSnapshot {
    /// 窗口起点（毫秒）。原样回传，让界面能说清「今日」指的是从哪一刻起。
    pub since_ms: i64,
    pub today: DashboardWindow,
    pub total: DashboardWindow,
    pub keys: Vec<KeyHeat>,
    pub tiers: UserTiers,
    /// 近 24h 登录失败 top IP。
    pub login_fails: Vec<LoginFailRow>,
    pub integrity: IntegrityCheck,
}

/// 流水 `kind` 取值。
///
/// 收敛成常量而不是散落字面量：`adjust_balance` 靠 `kind` 决定钱记进 `topup`
/// 还是冲减 `spent`，一处拼错就是一笔对不上的账，而字符串拼错编译器不会管。
pub const TOPUP_KIND: &str = "topup";
pub const UNLOCK_KIND: &str = "unlock";
pub const REFUND_KIND: &str = "refund";
pub const ADMIN_ADJUST_KIND: &str = "admin_adjust";

/// 上车的四种结果。
///
/// # 为什么是枚举而不是 `Result<..., BoardError>`
/// 余额不足、满员、已上车都**不是错误**，是三种正常业务结局，各自要给用户不同的
/// 话术和不同的 HTTP 状态（402 / 409 / 200）。塞进 `Err` 会迫使上层从错误字符串里
/// 反解原因，那才是真正会出 bug 的设计。枚举让「有几种结局」写在类型里，
/// 上层漏处理一种编译器就会报。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardOutcome {
    /// 上车成功。`refunded` 是本次触发的退款总额（退给车上其他人，不是退给本人）。
    Aboard {
        price: i64,
        balance: i64,
        count: i64,
        refunded: i64,
    },
    /// 本来就在车上。幂等结果，未扣分未写流水。
    AlreadyAboard { paid: i64, balance: i64, count: i64 },
    /// 余额不足。什么都没改。
    NotEnough { needed: i64, balance: i64 },
    /// 满员。什么都没改。
    Full { count: i64, max: i64 },
}

/// 钱包：余额 + 累计充值 + 累计净支出。
///
/// `topup` / `spent` 是纯对账字段，业务判定只看 `balance`。留着它们是为了能回答
/// 「这个号一共充了多少、花了多少」——只靠余额回答不了，因为余额是充值与消费
/// 反复抵消后的结果。退款会让 `spent` 减少（净支出的语义），故它不是单调递增的。
///
/// 恒等式 `balance == topup - spent` 对所有 `kind` 都成立，可用它自检。
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Wallet {
    pub balance: i64,
    pub topup: i64,
    pub spent: i64,
}

/// 一条积分流水。
///
/// `balance_after` 是写入时的余额快照。存它是为了对账时不必从头重放整条流水——
/// 重放要求每一条都不丢不乱，而快照让任意一条都能独立自证。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LedgerEntry {
    pub id: i64,
    pub at_ms: i64,
    /// 正 = 进账（充值/退款），负 = 出账（上车）。
    pub delta: i64,
    pub balance_after: i64,
    /// `topup` / `unlock` / `refund` / `admin_adjust`。
    pub kind: String,
    pub credential_id: Option<i64>,
    pub note: Option<String>,
}

/// 上车（查看某把 key）的结果。
///
/// 四个分支都带上当前 `balance` / `count`，让调用方一次拿全展示所需数据——
/// 若只返回「成功/失败」，前端还得再发一次查询，而那次查询看到的是**另一个
/// 时刻**的人数（别人可能刚上车），页面上就会出现价格与人数互相矛盾的瞬间。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardResult {
    /// 上车成功。`price` 是本人实付，`count` 是含本人的总人数。
    Ok {
        price: i64,
        balance: i64,
        count: u32,
    },
    /// 本人此前已上车。幂等分支：不扣分、不写流水、不触发退款。
    AlreadyOnboard { balance: i64, count: u32 },
    /// 余额不足。`needed` 是当前单价，`balance` 是现有余额。
    NotEnough { needed: i64, balance: i64 },
    /// 已满员。
    Full { count: u32, max: u32 },
}

/// Portal 的 SQLite 存储。
///
/// `rusqlite::Connection` 非 `Sync`，与 [`crate::usage::TraceDb`] 同样用
/// `parking_lot::Mutex` 串行化访问。
pub struct PortalDb {
    conn: Mutex<Connection>,
}

impl PortalDb {
    /// 打开/创建数据库，配置 WAL 并建表（幂等）。父目录需已存在。
    pub fn open(path: &Path) -> Result<PortalDb> {
        let conn = Connection::open(path)
            .with_context(|| format!("打开 Portal SQLite 失败: {}", path.display()))?;

        conn.execute_batch(
            "PRAGMA journal_mode=WAL;\
             PRAGMA synchronous=NORMAL;\
             PRAGMA foreign_keys=ON;",
        )
        .context("配置 Portal SQLite PRAGMA 失败")?;

        Self::init_schema(&conn)?;
        Ok(PortalDb {
            conn: Mutex::new(conn),
        })
    }

    /// 内存库，仅测试用。
    #[cfg(test)]
    pub fn open_in_memory() -> Result<PortalDb> {
        let conn = Connection::open_in_memory().context("打开内存库失败")?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")
            .context("开启外键失败")?;
        Self::init_schema(&conn)?;
        Ok(PortalDb {
            conn: Mutex::new(conn),
        })
    }

    fn init_schema(conn: &Connection) -> Result<()> {
        // username 用 COLLATE NOCASE：唯一约束和查询都大小写不敏感，避免
        // 「Alice」和「alice」注册成两个号（用户自己会记混，也给撞库者多一次机会）。
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS portal_users (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                username      TEXT NOT NULL UNIQUE COLLATE NOCASE,
                password_hash TEXT NOT NULL,
                disabled      INTEGER NOT NULL DEFAULT 0,
                created_at_ms INTEGER NOT NULL,
                last_login_ms INTEGER
            );

            CREATE TABLE IF NOT EXISTS portal_sessions (
                token_hash    TEXT PRIMARY KEY,
                user_id       INTEGER NOT NULL REFERENCES portal_users(id) ON DELETE CASCADE,
                created_at_ms INTEGER NOT NULL,
                expires_at_ms INTEGER NOT NULL,
                client_ip     TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_portal_sessions_user ON portal_sessions(user_id);
            CREATE INDEX IF NOT EXISTS idx_portal_sessions_exp ON portal_sessions(expires_at_ms);

            CREATE TABLE IF NOT EXISTS import_keys (
                key_hash      TEXT PRIMARY KEY,
                fingerprint   TEXT NOT NULL,
                credential_id INTEGER,
                region        TEXT,
                endpoint      TEXT,
                first_seen_ms INTEGER NOT NULL,
                last_seen_ms  INTEGER NOT NULL,
                push_count    INTEGER NOT NULL DEFAULT 1,
                ok            INTEGER NOT NULL,
                error         TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_import_keys_last_seen ON import_keys(last_seen_ms);
            CREATE INDEX IF NOT EXISTS idx_import_keys_cred ON import_keys(credential_id);

            CREATE TABLE IF NOT EXISTS portal_audit (
                id        INTEGER PRIMARY KEY AUTOINCREMENT,
                at_ms     INTEGER NOT NULL,
                user_id   INTEGER,
                username  TEXT,
                action    TEXT NOT NULL,
                client_ip TEXT,
                detail    TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_portal_audit_at ON portal_audit(at_ms);
            -- 筛选用的两个索引。审计是全库最大的表（每次登录、每次明文外显各一条，
            -- 公网撞库下更是写放大目标），而 G4 的界面默认就带筛选——没有索引的话
            -- 「看看 alice 干了什么」是一次全表扫，条数上万后界面会明显卡顿。
            --
            -- 【为何都带上 at_ms】查询永远按 at_ms DESC 排序。只索引 username 的话
            -- SQLite 能用索引筛出行，但还得把结果全部取出来再排序（临时 B 树）；
            -- 把 at_ms 放进索引第二列，筛选和排序一次走完。
            CREATE INDEX IF NOT EXISTS idx_portal_audit_user_at ON portal_audit(username, at_ms);
            CREATE INDEX IF NOT EXISTS idx_portal_audit_action_at ON portal_audit(action, at_ms);

            -- ============ 积分（车队上车）============

            -- 余额单独一张表而非给 portal_users 加列：余额是高频读写、需事务保护的
            -- 字段，与账号信息的生命周期不同。topup/spent 只为对账，不参与计费。
            CREATE TABLE IF NOT EXISTS portal_balances (
                user_id INTEGER PRIMARY KEY REFERENCES portal_users(id) ON DELETE CASCADE,
                balance INTEGER NOT NULL DEFAULT 0,
                topup   INTEGER NOT NULL DEFAULT 0,
                spent   INTEGER NOT NULL DEFAULT 0
            );

            -- 上车记录。复合主键天然保证幂等（同一人同一 key 只能有一行）。
            -- paid = 累计已付，是差额退款模型的核心：退款 = paid - 当前单价。
            -- credential_id 不设外键：凭据池在 credentials.json 而非本库，且凭据
            -- 被删后上车记录应保留（历史仍需可查，与 portal_audit 同理）。
            CREATE TABLE IF NOT EXISTS portal_unlocks (
                user_id        INTEGER NOT NULL REFERENCES portal_users(id) ON DELETE CASCADE,
                credential_id  INTEGER NOT NULL,
                paid           INTEGER NOT NULL,
                unlocked_at_ms INTEGER NOT NULL,
                PRIMARY KEY (user_id, credential_id)
            );
            CREATE INDEX IF NOT EXISTS idx_portal_unlocks_cred ON portal_unlocks(credential_id);

            -- 价格参数快照。一把 key 首次被上车时冻结当时的参数，此后永久按它计价。
            -- 若不冻结，管理员改价会让已有乘客的应付额跳变，只剩两个都不可接受的选择：
            -- 追扣老乘客（用户莫名少钱，余额可能被扣成负），或让同一把 key 上出现两种
            -- 价格（「均摊」当场失效，paid 语义分裂成新旧两套、无法用单一公式校验总账）。
            CREATE TABLE IF NOT EXISTS portal_key_pricing (
                credential_id INTEGER PRIMARY KEY,
                base_count    INTEGER NOT NULL,
                base_price    INTEGER NOT NULL,
                total_price   INTEGER NOT NULL,
                min_price     INTEGER NOT NULL,
                max_unlockers INTEGER NOT NULL,
                frozen_at_ms  INTEGER NOT NULL
            );

            -- 流水。balance_after 存快照，对账时不必重放全部历史。
            -- 不设外键，理由同审计表：用户被删后流水应保留，否则无法回答
            -- 「这个账号一共花了多少」。
            CREATE TABLE IF NOT EXISTS portal_ledger (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                user_id       INTEGER NOT NULL,
                at_ms         INTEGER NOT NULL,
                delta         INTEGER NOT NULL,
                balance_after INTEGER NOT NULL,
                kind          TEXT NOT NULL,
                credential_id INTEGER,
                note          TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_portal_ledger_user ON portal_ledger(user_id, at_ms);",
        )
        .context("初始化 Portal 表结构失败")?;
        Self::migrate(conn)?;
        Ok(())
    }

    /// 增量迁移：给**已存在**的表补列。
    ///
    /// # 为何需要这一层
    /// `CREATE TABLE IF NOT EXISTS` 对已存在的表**完全无效**——它不会补上新加的列。
    /// 于是「在建表语句里加一列」这个看起来最自然的做法，只对全新部署有效：
    /// 老库照旧跑着缺列的表，直到某次查询报 `no such column`。而那个报错出现在
    /// 业务代码里，看起来像业务 bug，不像迁移漏了。
    ///
    /// # 为何用 PRAGMA 探测而不是版本号表
    /// 版本号表要求「每次改 schema 都记得递增版本」，而漏记不会有任何编译错误。
    /// 探测实际列则是**自校验**的：列在就跳过，不在就补，与任何版本号是否准确无关。
    /// 代价是每次开库多几个 PRAGMA 查询——那是微秒级，且只在启动时发生一次。
    ///
    /// # 幂等性
    /// 每条迁移都先查列是否存在。重复开库、老库新库、迁移到一半崩溃后重来，
    /// 结果都相同。`ALTER TABLE ADD COLUMN` 在 SQLite 里是 O(1) 的元数据操作，
    /// 不重写表数据。
    fn migrate(conn: &Connection) -> Result<()> {
        // 每一项：(表名, 列名, 完整的 ADD COLUMN 语句)
        //
        // 【新增列的默认值必须能表达「历史数据不知道」】比如 role 默认 'user'
        // 是安全的（老用户都是普通用户）；而如果某列的默认值会让老数据看起来
        // 像是拥有了某种权限，那就必须显式回填而不是靠默认值。
        const MIGRATIONS: &[(&str, &str, &str)] = &[(
            "portal_users",
            "role",
            "ALTER TABLE portal_users ADD COLUMN role TEXT NOT NULL DEFAULT 'user'",
        )];

        for (table, column, sql) in MIGRATIONS {
            if !Self::has_column(conn, table, column)? {
                conn.execute_batch(sql)
                    .with_context(|| format!("迁移失败：给 {table} 加列 {column}"))?;
                tracing::info!("Portal 库迁移：{}.{} 已补齐", table, column);
            }
        }
        Ok(())
    }

    /// 表里是否已有该列。表不存在时返回 `false`（而非报错）——调用方随后的
    /// `ALTER TABLE` 会给出更准确的错误，不必在这里重复判断。
    fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
        // 表名来自上面的编译期常量，不是外部输入，故可直接内插进 PRAGMA
        // （PRAGMA 的表名位置不接受参数绑定）。
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .with_context(|| format!("查询 {table} 列信息失败"))?;
        let mut rows = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .with_context(|| format!("读取 {table} 列名失败"))?;
        while let Some(name) = rows.next() {
            if name.context("读取列名行失败")? == column {
                return Ok(true);
            }
        }
        Ok(false)
    }

    // ---------------- 用户 ----------------

    /// 创建用户。`password_hash` 必须已是 argon2id PHC 串（本层不做哈希，避免
    /// 「忘了哈希、明文进库」这类调用点错误被静默接受——由 [`super::password`] 统一负责）。
    ///
    /// 用户名已存在时返回 `Ok(None)`，由调用方决定报错文案（不在这层拼用户可见文本）。
    ///
    /// **不写 `role` 列**，落到迁移的 `DEFAULT 'user'`。注册通道由此天然产出普通用户：
    /// 想让自助注册产出管理员，得显式调用 [`Self::create_user_with_role`]——而那个函数
    /// 的调用点是可以数清的。
    pub fn create_user(
        &self,
        username: &str,
        password_hash: &str,
        now_ms: i64,
    ) -> Result<Option<i64>> {
        let conn = self.conn.lock();
        let changed = conn
            .execute(
                "INSERT OR IGNORE INTO portal_users (username, password_hash, disabled, created_at_ms)
                 VALUES (?1, ?2, 0, ?3)",
                params![username, password_hash, now_ms],
            )
            .context("创建 Portal 用户失败")?;
        if changed == 0 {
            return Ok(None); // 用户名已占用
        }
        Ok(Some(conn.last_insert_rowid()))
    }

    /// 创建用户并指定角色。
    ///
    /// # 为何是并行函数而不给 [`Self::create_user`] 加参数
    /// 那个函数有近 20 个调用点（大半在测试里）。加必填参数要逐一改，而改动本身
    /// 没有产出——它们全都想要「默认普通用户」。更要紧的是：注册通道调用的正是
    /// 那个函数，保持它**没有角色参数**意味着自助注册在结构上就无法产出管理员，
    /// 而不是靠调用点传对了值。
    pub fn create_user_with_role(
        &self,
        username: &str,
        password_hash: &str,
        role: super::role::RoleKind,
        now_ms: i64,
    ) -> Result<Option<i64>> {
        let conn = self.conn.lock();
        let changed = conn
            .execute(
                "INSERT OR IGNORE INTO portal_users (username, password_hash, disabled, created_at_ms, role)
                 VALUES (?1, ?2, 0, ?3, ?4)",
                params![username, password_hash, now_ms, role.as_str()],
            )
            .context("创建 Portal 用户（含角色）失败")?;
        if changed == 0 {
            return Ok(None);
        }
        Ok(Some(conn.last_insert_rowid()))
    }

    /// 用户是否存在。
    ///
    /// 【为何不用 `find_user_by_id` 代替】调用方（管理员充值前的校验）只需要一个
    /// 布尔值。取整行会把 `password_hash` 拉进内存，而那是离线爆破的原料——
    /// 让它出现在不需要它的调用栈里，就多了一处可能被日志、Debug 输出或
    /// 将来某次「顺手把这个结构返回给前端」带出去的地方。只查 1 就没有这个风险。
    pub fn user_exists(&self, user_id: i64) -> Result<bool> {
        let conn = self.conn.lock();
        let v: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM portal_users WHERE id = ?1",
                params![user_id],
                |r| r.get(0),
            )
            .optional()
            .context("查用户是否存在失败")?;
        Ok(v.is_some())
    }

    /// 按用户名取用户（大小写不敏感，随表的 COLLATE NOCASE）。
    pub fn find_user_by_name(&self, username: &str) -> Result<Option<PortalUser>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT id, username, password_hash, disabled, created_at_ms, last_login_ms, role
             FROM portal_users WHERE username = ?1",
            params![username],
            |row| {
                let raw: String = row.get(6)?;
                Ok(PortalUser {
                    id: row.get(0)?,
                    username: row.get(1)?,
                    password_hash: row.get(2)?,
                    disabled: row.get::<_, i64>(3)? != 0,
                    created_at_ms: row.get(4)?,
                    last_login_ms: row.get(5)?,
                    role: super::role::RoleKind::parse_logged(&raw, "find_user_by_name"),
                })
            },
        )
        .optional()
        .context("查询 Portal 用户失败")
    }

    /// 列出所有用户（admin 管理页用，按创建时间升序）。
    pub fn list_users(&self) -> Result<Vec<PortalUser>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id, username, password_hash, disabled, created_at_ms, last_login_ms, role
                 FROM portal_users ORDER BY created_at_ms ASC, id ASC",
            )
            .context("准备用户列表查询失败")?;
        let rows = stmt
            .query_map([], |row| {
                let raw: String = row.get(6)?;
                Ok(PortalUser {
                    id: row.get(0)?,
                    username: row.get(1)?,
                    password_hash: row.get(2)?,
                    disabled: row.get::<_, i64>(3)? != 0,
                    created_at_ms: row.get(4)?,
                    last_login_ms: row.get(5)?,
                    role: super::role::RoleKind::parse_logged(&raw, "list_users"),
                })
            })
            .context("执行用户列表查询失败")?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.context("读取用户行失败")?);
        }
        Ok(out)
    }

    /// 列出用户 + 余额，供 admin 管理页用。
    ///
    /// 【为何不复用 [`Self::list_users`] 再逐个查余额】那样每行一次加锁，且返回的
    /// [`PortalUser`] 带着 `password_hash`——admin 接口只要不小心整个序列化出去，
    /// 就把全部用户的爆破原料交了出去。这里用专门的行类型（结构上没有那个字段）
    /// 加一次 LEFT JOIN，两个问题一起解决。
    ///
    /// SQL 里显式列字段而不用 `SELECT u.*`：后者在 `portal_users` 日后加列时会把
    /// 新字段自动带出来，而 `password_hash` 正躺在那张表里。
    pub fn list_users_with_balance(&self) -> Result<Vec<UserWithBalance>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT u.id, u.username, u.disabled, u.created_at_ms, u.last_login_ms,
                        COALESCE(b.balance, 0), COALESCE(b.topup, 0), COALESCE(b.spent, 0),
                        (SELECT COUNT(*) FROM portal_unlocks k WHERE k.user_id = u.id),
                        u.role
                 FROM portal_users u
                 LEFT JOIN portal_balances b ON b.user_id = u.id
                 ORDER BY u.created_at_ms ASC, u.id ASC",
            )
            .context("准备用户余额列表查询失败")?;
        let rows = stmt
            .query_map([], |row| {
                Ok(UserWithBalance {
                    id: row.get(0)?,
                    username: row.get(1)?,
                    disabled: row.get::<_, i64>(2)? != 0,
                    created_at_ms: row.get(3)?,
                    last_login_ms: row.get(4)?,
                    balance: row.get(5)?,
                    topup: row.get(6)?,
                    spent: row.get(7)?,
                    aboard_count: row.get(8)?,
                    role: {
                        let raw: String = row.get(9)?;
                        super::role::RoleKind::parse_logged(&raw, "list_users_with_balance")
                    },
                })
            })
            .context("执行用户余额列表查询失败")?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.context("读取用户余额行失败")?);
        }
        Ok(out)
    }

    /// 改密码。**同时清掉该用户的所有会话**——否则改密后旧 cookie 仍然有效，
    /// 「怀疑被盗号就改密码」这个用户唯一的自救手段会失效。
    pub fn set_password(&self, user_id: i64, password_hash: &str) -> Result<bool> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction().context("开启改密事务失败")?;
        let changed = tx
            .execute(
                "UPDATE portal_users SET password_hash = ?1 WHERE id = ?2",
                params![password_hash, user_id],
            )
            .context("更新密码失败")?;
        tx.execute(
            "DELETE FROM portal_sessions WHERE user_id = ?1",
            params![user_id],
        )
        .context("改密后清理会话失败")?;
        tx.commit().context("提交改密事务失败")?;
        Ok(changed > 0)
    }

    /// 停用/启用。停用时一并清会话，使其立即生效而非等 cookie 自然过期。
    pub fn set_disabled(&self, user_id: i64, disabled: bool) -> Result<bool> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction().context("开启停用事务失败")?;
        let changed = tx
            .execute(
                "UPDATE portal_users SET disabled = ?1 WHERE id = ?2",
                params![i64::from(disabled), user_id],
            )
            .context("更新停用状态失败")?;
        if disabled {
            tx.execute(
                "DELETE FROM portal_sessions WHERE user_id = ?1",
                params![user_id],
            )
            .context("停用后清理会话失败")?;
        }
        tx.commit().context("提交停用事务失败")?;
        Ok(changed > 0)
    }

    /// 删除用户（会话随外键 ON DELETE CASCADE 一起走）。
    pub fn delete_user(&self, user_id: i64) -> Result<bool> {
        let conn = self.conn.lock();
        let changed = conn
            .execute("DELETE FROM portal_users WHERE id = ?1", params![user_id])
            .context("删除 Portal 用户失败")?;
        Ok(changed > 0)
    }

    /// 记一次登录成功的时间戳。
    pub fn touch_last_login(&self, user_id: i64, now_ms: i64) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE portal_users SET last_login_ms = ?1 WHERE id = ?2",
            params![now_ms, user_id],
        )
        .context("更新最后登录时间失败")?;
        Ok(())
    }

    // ---------------- 会话 ----------------

    /// 落一条会话。`token_hash` 必须是 `SHA-256(token)` 的 hex——**库里绝不存原始 token**，
    /// 这样即使 SQLite 文件泄露也不能直接冒充任何已登录用户（等价于密码只存哈希）。
    pub fn create_session(
        &self,
        token_hash: &str,
        user_id: i64,
        now_ms: i64,
        expires_at_ms: i64,
        client_ip: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT OR REPLACE INTO portal_sessions
                 (token_hash, user_id, created_at_ms, expires_at_ms, client_ip)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![token_hash, user_id, now_ms, expires_at_ms, client_ip],
        )
        .context("创建 Portal 会话失败")?;
        Ok(())
    }

    /// 校验会话：未过期、且用户未被停用才返回身份。
    ///
    /// `disabled = 0` 写进 SQL 而不是取回来再判：停用是「立刻失效」语义，
    /// 放在同一条查询里就不存在「查到了但忘了检查」的调用点漏判。
    pub fn validate_session(&self, token_hash: &str, now_ms: i64) -> Result<Option<PortalSession>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT s.user_id, u.username, s.expires_at_ms, u.role
             FROM portal_sessions s JOIN portal_users u ON u.id = s.user_id
             WHERE s.token_hash = ?1 AND s.expires_at_ms > ?2 AND u.disabled = 0",
            params![token_hash, now_ms],
            |row| {
                let raw: String = row.get(3)?;
                Ok(PortalSession {
                    user_id: row.get(0)?,
                    username: row.get(1)?,
                    expires_at_ms: row.get(2)?,
                    // 用宽容解析：库里的 role 若被手工改坏，宁可按普通用户对待也不能
                    // 让这个用户登不进来（详见 RoleKind::parse 的说明）。会打 warn。
                    role: super::role::RoleKind::parse_logged(&raw, "validate_session"),
                })
            },
        )
        .optional()
        .context("校验 Portal 会话失败")
    }

    /// 读某个用户的角色。
    ///
    /// # 为何不用 [`Self::find_user_by_id`]
    /// 那个函数返回带 `password_hash` 的整行。只想知道角色时把爆破原料拉进内存，
    /// 等于给日志/Debug/将来某次「顺手返回这个结构」多留一个泄漏点。
    ///
    /// 用户不存在时返回 `None`，与「存在但角色是 user」区分开——调用方
    /// （改角色前的校验）需要分辨这两种情况。
    pub fn role_of(&self, user_id: i64) -> Result<Option<super::role::RoleKind>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT role FROM portal_users WHERE id = ?1",
            params![user_id],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .context("查询用户角色失败")
        .map(|opt| opt.map(|raw| super::role::RoleKind::parse_logged(&raw, "role_of")))
    }

    /// 设置某个用户的角色。返回是否命中了一行（`false` = 用户不存在）。
    ///
    /// 入参是 [`super::role::RoleKind`] 而非字符串：字符串会让「写进库的值是否合法」
    /// 变成每个调用点各自的责任，而写坏一次就永久留在库里。类型让非法值在
    /// HTTP 入口的 `parse_strict` 处就被挡住，到不了这里。
    pub fn set_role(&self, user_id: i64, role: super::role::RoleKind) -> Result<bool> {
        let conn = self.conn.lock();
        let n = conn
            .execute(
                "UPDATE portal_users SET role = ?2 WHERE id = ?1",
                params![user_id, role.as_str()],
            )
            .context("更新用户角色失败")?;
        Ok(n > 0)
    }

    /// 当前有几个 admin。
    ///
    /// # 为何需要
    /// 「把最后一个管理员降级」会让 portal 内再没有人能管用户——只能回去动
    /// `adminApiKey`，而那正是引入 portal admin 想避免的。改角色前查这个数，
    /// 让「自锁在门外」这件事在入口就被拒绝，而不是发生后才发现。
    pub fn admin_count(&self) -> Result<i64> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT COUNT(*) FROM portal_users WHERE role = ?1 AND disabled = 0",
            params![super::role::RoleKind::Admin.as_str()],
            |r| r.get(0),
        )
        .context("统计管理员数量失败")
    }

    /// 登出：删单条会话。
    pub fn delete_session(&self, token_hash: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "DELETE FROM portal_sessions WHERE token_hash = ?1",
            params![token_hash],
        )
        .context("删除 Portal 会话失败")?;
        Ok(())
    }

    /// 清理过期会话，返回删除条数。定时任务调用——过期行不清会无界堆积。
    pub fn purge_expired_sessions(&self, now_ms: i64) -> Result<usize> {
        let conn = self.conn.lock();
        let n = conn
            .execute(
                "DELETE FROM portal_sessions WHERE expires_at_ms <= ?1",
                params![now_ms],
            )
            .context("清理过期会话失败")?;
        Ok(n)
    }

    /// 只保留该用户最近 `keep` 条会话，删掉更早的，返回删除条数。
    ///
    /// 【为什么需要】会话只在过期后才被清理，中间用户每次登录都新增一行。一个人反复登录
    /// （换设备、清 cookie、脚本轮询）就能让自己的会话行无界增长，而每一行都是一个**当前有效**
    /// 的凭据——攻击者拿到其中任何一条旧 cookie 都能进来。限制并发会话数同时压住两件事：
    /// 存储增长，和「历史 cookie 永远有效」的暴露面。
    ///
    /// 按 `created_at_ms` 倒序保留最近的，故最新登录的那条一定留存（不会把用户自己踢掉）。
    pub fn trim_user_sessions(&self, user_id: i64, keep: usize) -> Result<usize> {
        let conn = self.conn.lock();
        let n = conn
            .execute(
                "DELETE FROM portal_sessions
                 WHERE user_id = ?1 AND token_hash NOT IN (
                     SELECT token_hash FROM portal_sessions
                     WHERE user_id = ?1
                     ORDER BY created_at_ms DESC, token_hash ASC
                     LIMIT ?2
                 )",
                params![user_id, keep as i64],
            )
            .context("裁剪用户会话失败")?;
        Ok(n)
    }

    // ---------------- 推送凭据元数据 ----------------

    /// 记一次 key 推送（幂等 upsert，按 `key_hash`）。
    ///
    /// 冲突时的取值规则，逐字段都有理由：
    /// - `first_seen_ms` **保留旧值**：这是「第一次见到这个号」，重推不该改写它。
    /// - `push_count` 累加：能看出哪个号被反复推。
    /// - `credential_id`/`region`/`endpoint` 用 `COALESCE(新, 旧)`：一次**失败**推送带的是
    ///   `NULL`，若直接覆盖就会把此前成功落库的 id 抹掉，导致明文再也回查不到；
    ///   而号被删后重推会带来新的非空 id，`COALESCE` 自然取新值。
    /// - `ok`/`error` 直接覆盖为最近一次的结果：这两个字段的语义就是「最后一次推送如何」。
    pub fn upsert_import_key(&self, meta: &ImportKeyMeta) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO import_keys
                 (key_hash, fingerprint, credential_id, region, endpoint,
                  first_seen_ms, last_seen_ms, push_count, ok, error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(key_hash) DO UPDATE SET
                 last_seen_ms  = excluded.last_seen_ms,
                 push_count    = import_keys.push_count + 1,
                 credential_id = COALESCE(excluded.credential_id, import_keys.credential_id),
                 region        = COALESCE(excluded.region, import_keys.region),
                 endpoint      = COALESCE(excluded.endpoint, import_keys.endpoint),
                 ok            = excluded.ok,
                 error         = excluded.error",
            params![
                meta.key_hash,
                meta.fingerprint,
                meta.credential_id,
                meta.region,
                meta.endpoint,
                meta.first_seen_ms,
                meta.last_seen_ms,
                meta.push_count.max(1),
                i64::from(meta.ok),
                meta.error,
            ],
        )
        .context("写入推送凭据元数据失败")?;
        Ok(())
    }

    /// 按最近推送时间倒序列出元数据（分页）。
    pub fn list_import_keys(&self, limit: usize, offset: usize) -> Result<Vec<ImportKeyMeta>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT key_hash, fingerprint, credential_id, region, endpoint,
                        first_seen_ms, last_seen_ms, push_count, ok, error
                 FROM import_keys
                 ORDER BY last_seen_ms DESC, key_hash ASC
                 LIMIT ?1 OFFSET ?2",
            )
            .context("准备推送凭据查询失败")?;
        let rows = stmt
            .query_map(params![limit as i64, offset as i64], row_to_import_key)
            .context("执行推送凭据查询失败")?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.context("读取推送凭据行失败")?);
        }
        Ok(out)
    }

    /// 元数据总条数（分页用）。
    pub fn count_import_keys(&self) -> Result<i64> {
        let conn = self.conn.lock();
        conn.query_row("SELECT COUNT(*) FROM import_keys", [], |r| r.get(0))
            .context("统计推送凭据条数失败")
    }

    // ---------------- 积分：余额与流水 ----------------

    /// 读余额。**没有行时返回 0 而非报错**——新用户注册时不预建余额行，
    /// 首次充值才插入。把「没记录」当 0 分处理，语义上完全等价，
    /// 且省掉「注册流程忘了建余额行」这类必然出现的漏接。
    pub fn balance_of(&self, user_id: i64) -> Result<i64> {
        let conn = self.conn.lock();
        let v: Option<i64> = conn
            .query_row(
                "SELECT balance FROM portal_balances WHERE user_id = ?1",
                params![user_id],
                |r| r.get(0),
            )
            .optional()
            .context("查询余额失败")?;
        Ok(v.unwrap_or(0))
    }

    /// 完整钱包视图（余额 + 累计充值 + 累计支出）。无行时全 0。
    pub fn wallet_of(&self, user_id: i64) -> Result<Wallet> {
        let conn = self.conn.lock();
        let w = conn
            .query_row(
                "SELECT balance, topup, spent FROM portal_balances WHERE user_id = ?1",
                params![user_id],
                |r| {
                    Ok(Wallet {
                        balance: r.get(0)?,
                        topup: r.get(1)?,
                        spent: r.get(2)?,
                    })
                },
            )
            .optional()
            .context("查询钱包失败")?;
        Ok(w.unwrap_or(Wallet {
            balance: 0,
            topup: 0,
            spent: 0,
        }))
    }

    /// 充值 / 管理员调账。`delta` 为正是加分，为负是扣分。
    ///
    /// **扣分不允许把余额打成负数**：`delta < 0` 且余额不足时返回 `Ok(None)`，
    /// 调用方据此告知管理员「该用户只剩 X 分，扣不动 Y 分」。允许负余额会让
    /// 「余额 >= 价格」这个前置检查失去意义，后续所有对账都得处理负数分支。
    ///
    /// 返回调账后的余额。整个操作在一个事务里：余额与流水必须同时成立，
    /// 否则会出现「钱变了但查不到为什么」或反之。
    pub fn adjust_balance(
        &self,
        user_id: i64,
        delta: i64,
        kind: &str,
        credential_id: Option<i64>,
        note: Option<&str>,
        at_ms: i64,
    ) -> Result<Option<i64>> {
        let mut conn = self.conn.lock();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .context("开启调账事务失败")?;

        let Some(next) = apply_delta(&tx, user_id, delta, kind, credential_id, note, at_ms)? else {
            // 不 commit 即回滚。显式返回让「余额不足什么都没发生」这件事在代码里看得见。
            return Ok(None);
        };

        tx.commit().context("提交调账事务失败")?;
        Ok(Some(next))
    }

    // ---------------- 上车 ----------------

    /// 上车：扣当前单价、记录上车、按差额退款给已在车上的人。
    ///
    /// # 为什么整个流程必须在一个 `BEGIN IMMEDIATE` 事务里
    /// 单价由**当前人数**决定，所以「读人数 → 算价 → 扣费 → 退款」这四步之间
    /// 若被另一个上车请求插入，两人会按同一个旧人数计价：各付 base_price，
    /// 而正确结果是各付 `unit_price(2)`，且先到的那位本应收到退款。默认的
    /// deferred 事务开头只拿读锁、写时才升级，两个并发写会有一个吃到
    /// `SQLITE_BUSY` 直接失败；IMMEDIATE 开头就拿写锁，第二个请求排队等待，
    /// 醒来后读到的是第一个提交后的真实人数。这是防超卖与错价的唯一防线。
    ///
    /// `cfg` 只在这把 key **尚无价格快照**时使用，用完即冻结进
    /// `portal_key_pricing`。此后改配置不再影响这把 key（见该表的建表注释）。
    pub fn board(
        &self,
        user_id: i64,
        credential_id: i64,
        cfg: Pricing,
        now_ms: i64,
    ) -> Result<BoardOutcome> {
        let mut conn = self.conn.lock();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .context("开启上车事务失败")?;

        // 1. 已上车 → 直接返回，不扣分不写流水。
        //
        // 幂等靠这一步而非靠主键冲突：主键冲突会让「重复点上车」变成一个错误，
        // 而它其实是完全正常的操作（刷新页面、双击按钮）。
        let already: Option<i64> = tx
            .query_row(
                "SELECT paid FROM portal_unlocks WHERE user_id = ?1 AND credential_id = ?2",
                params![user_id, credential_id],
                |r| r.get(0),
            )
            .optional()
            .context("查上车记录失败")?;

        if let Some(paid) = already {
            let count = count_unlockers(&tx, credential_id)?;
            let balance = read_balance(&tx, user_id)?;
            return Ok(BoardOutcome::AlreadyAboard {
                paid,
                balance,
                count,
            });
        }

        // 2. 读价格快照；没有就用当前配置并冻结。
        let pricing = match read_pricing(&tx, credential_id)? {
            Some(p) => p,
            None => {
                let p = cfg.sanitized();
                freeze_pricing(&tx, credential_id, &p, now_ms)?;
                p
            }
        };

        // 3-4. 人数与满员判定。满员不扣分。
        let count = count_unlockers(&tx, credential_id)?;
        if pricing.is_full(count as u32) {
            return Ok(BoardOutcome::Full {
                count,
                max: pricing.max_unlockers as i64,
            });
        }

        // 5. 新人数下的单价——注意是 count + 1（算的是「这位上车后」的价）。
        let price = pricing.unit_price(count as u32 + 1);

        // 6. 余额检查。放在扣费前是为了在余额不足时什么都不改。
        let balance = read_balance(&tx, user_id)?;
        if balance < price {
            return Ok(BoardOutcome::NotEnough {
                needed: price,
                balance,
            });
        }

        // 7. 扣费 + 上车记录。
        let note = format!("上车 #{credential_id}");
        let after = apply_delta(
            &tx,
            user_id,
            -price,
            UNLOCK_KIND,
            Some(credential_id),
            Some(&note),
            now_ms,
        )?
        .context("余额检查已通过却扣费失败，说明事务隔离被破坏")?;

        tx.execute(
            "INSERT INTO portal_unlocks (user_id, credential_id, paid, unlocked_at_ms)
             VALUES (?1, ?2, ?3, ?4)",
            params![user_id, credential_id, price, now_ms],
        )
        .context("写入上车记录失败")?;

        // 8. 给已在车上的人退差额。
        //
        // 先把 (user_id, paid) 收集出来再逐个更新：rusqlite 的语句借用 tx，
        // 边遍历边写会撞借用检查；而且行数上限是 max_unlockers（默认 10），
        // 收集的代价可以忽略。
        let mut olds: Vec<(i64, i64)> = Vec::new();
        {
            let mut stmt = tx
                .prepare(
                    "SELECT user_id, paid FROM portal_unlocks
                     WHERE credential_id = ?1 AND user_id != ?2",
                )
                .context("准备退款查询失败")?;
            let rows = stmt
                .query_map(params![credential_id, user_id], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .context("执行退款查询失败")?;
            for r in rows {
                olds.push(r.context("读取退款目标行失败")?);
            }
        }

        let mut refunded = 0i64;
        for (old_uid, old_paid) in olds {
            let refund = crate::portal::credits::refund_for(old_paid, price);
            if refund <= 0 {
                continue;
            }
            let rnote = format!("退款 #{credential_id}");
            apply_delta(
                &tx,
                old_uid,
                refund,
                REFUND_KIND,
                Some(credential_id),
                Some(&rnote),
                now_ms,
            )?
            .context("退款是加分，不该因余额不足失败")?;

            // paid 同步降到当前单价：差额模型只记「已付」，下一轮退款仍用
            // `paid - 新单价` 计算。忘了这一步会导致同一笔差额被反复退还。
            tx.execute(
                "UPDATE portal_unlocks SET paid = ?1 WHERE user_id = ?2 AND credential_id = ?3",
                params![price, old_uid, credential_id],
            )
            .context("同步已付金额失败")?;
            refunded += refund;
        }

        tx.commit().context("提交上车事务失败")?;

        Ok(BoardOutcome::Aboard {
            price,
            balance: after,
            count: count + 1,
            refunded,
        })
    }

    /// 某把 key 已上车人数。
    pub fn unlocker_count(&self, credential_id: i64) -> Result<i64> {
        let conn = self.conn.lock();
        count_unlockers(&conn, credential_id)
    }

    /// 某用户是否已上车某把 key。明文下发的唯一依据（Task 8）。
    pub fn is_aboard(&self, user_id: i64, credential_id: i64) -> Result<bool> {
        let conn = self.conn.lock();
        let v: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM portal_unlocks WHERE user_id = ?1 AND credential_id = ?2",
                params![user_id, credential_id],
                |r| r.get(0),
            )
            .optional()
            .context("查上车状态失败")?;
        Ok(v.is_some())
    }

    /// 该用户已上车的所有 key → `(实付分数, 上车时刻 ms)`。
    ///
    /// 一次取回而非每行查一次：列表页有几十把 key，逐行查会变成几十次加锁。
    /// 返回 map 而非 set，因为列表页既要判「我上车了吗」（决定是否下发明文），
    /// 也要显示「我这单花了多少」「什么时候上的」——同一张表，分几次查纯属浪费。
    pub fn aboard_map(&self, user_id: i64) -> Result<std::collections::HashMap<i64, (i64, i64)>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT credential_id, paid, unlocked_at_ms FROM portal_unlocks WHERE user_id = ?1",
            )
            .context("准备上车集合查询失败")?;
        let rows = stmt
            .query_map(params![user_id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?),
                ))
            })
            .context("执行上车集合查询失败")?;
        let mut out = std::collections::HashMap::new();
        for r in rows {
            let (k, v) = r.context("读取上车集合行失败")?;
            out.insert(k, v);
        }
        Ok(out)
    }

    /// 每把 key 的上车人数，一次取回（列表页用）。
    pub fn unlocker_counts(&self) -> Result<std::collections::HashMap<i64, i64>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare("SELECT credential_id, COUNT(*) FROM portal_unlocks GROUP BY credential_id")
            .context("准备人数统计失败")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
            .context("执行人数统计失败")?;
        let mut out = std::collections::HashMap::new();
        for r in rows {
            let (k, v) = r.context("读取人数统计行失败")?;
            out.insert(k, v);
        }
        Ok(out)
    }

    /// 该 key 的价格快照，无快照返回 `None`（调用方回退当前配置）。
    pub fn pricing_of(&self, credential_id: i64) -> Result<Option<Pricing>> {
        let conn = self.conn.lock();
        read_pricing(&conn, credential_id)
    }

    /// 所有已冻结的价格快照，一次取回（列表页用）。
    ///
    /// 与 [`Self::pricing_of`] 并存而非取代它：列表页要为几十行各取一次快照，
    /// 逐行调 `pricing_of` 就是几十次加锁 + 几十条 SQL；而 `board` 在事务内
    /// 只关心一把 key，用不着全表。行数上限是「被上过车的 key 数」，很小。
    pub fn all_pricing(&self) -> Result<std::collections::HashMap<i64, Pricing>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT credential_id, base_count, base_price, total_price, min_price, max_unlockers
                 FROM portal_key_pricing",
            )
            .context("准备快照批量查询失败")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    Pricing {
                        base_count: r.get::<_, i64>(1)? as u32,
                        base_price: r.get(2)?,
                        total_price: r.get(3)?,
                        min_price: r.get(4)?,
                        max_unlockers: r.get::<_, i64>(5)? as u32,
                    },
                ))
            })
            .context("执行快照批量查询失败")?;
        let mut out = std::collections::HashMap::new();
        for r in rows {
            let (k, v) = r.context("读取快照行失败")?;
            out.insert(k, v);
        }
        Ok(out)
    }

    /// 读流水（倒序，最近的在前）。
    pub fn ledger_of(&self, user_id: i64, limit: usize) -> Result<Vec<LedgerEntry>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id, at_ms, delta, balance_after, kind, credential_id, note
                 FROM portal_ledger WHERE user_id = ?1
                 ORDER BY at_ms DESC, id DESC LIMIT ?2",
            )
            .context("准备流水查询失败")?;
        let rows = stmt
            .query_map(params![user_id, limit as i64], |r| {
                Ok(LedgerEntry {
                    id: r.get(0)?,
                    at_ms: r.get(1)?,
                    delta: r.get(2)?,
                    balance_after: r.get(3)?,
                    kind: r.get(4)?,
                    credential_id: r.get(5)?,
                    note: r.get(6)?,
                })
            })
            .context("执行流水查询失败")?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.context("读取流水行失败")?);
        }
        Ok(out)
    }

    // ---------------- 运营看板 ----------------

    /// 看板快照。`since_ms` 是「今日」窗口起点，`fail_since_ms` 是登录失败统计
    /// 窗口起点，都由调用方给——存储层不该自己决定「今天」从几点算起（时区、
    /// 是否按自然日、还是滚动 24h，都是上层策略）。
    ///
    /// # 为何整个快照在一把锁里读完
    /// 各区块之间有恒等关系（发票数 == unlocks 行数、余额和 == 流水和）。分多次
    /// 加锁读的话，中间只要有人上车，返回的快照就自相矛盾——而看板上对不上的
    /// 数字会让人怀疑全部数字。一次拿锁读完，快照要么全是 T 时刻的、要么全是
    /// T' 时刻的，横竖自洽。
    ///
    /// 代价是持锁期间挡住写。可接受：这些查询全是有索引的聚合，行数上限是
    /// 「用户数 × 上车数」这个量级（车队场景下是几十到几千），毫秒级。
    pub fn dashboard(
        &self,
        since_ms: i64,
        fail_since_ms: i64,
        top: usize,
    ) -> Result<DashboardSnapshot> {
        let conn = self.conn.lock();
        Ok(DashboardSnapshot {
            since_ms,
            today: window_totals(&conn, Some(since_ms))?,
            total: window_totals(&conn, None)?,
            keys: key_heat(&conn, top)?,
            tiers: user_tiers(&conn)?,
            login_fails: login_fails(&conn, fail_since_ms, top)?,
            integrity: integrity_check(&conn)?,
        })
    }

    // ---------------- 审计 ----------------

    /// 记一条审计。**审计失败绝不阻断主流程**：调用方拿到 `Err` 只记日志，
    /// 不能因为写审计失败就拒绝用户登录或拒绝返回数据。
    pub fn add_audit(
        &self,
        at_ms: i64,
        user_id: Option<i64>,
        username: Option<&str>,
        action: &str,
        client_ip: Option<&str>,
        detail: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO portal_audit (at_ms, user_id, username, action, client_ip, detail)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![at_ms, user_id, username, action, client_ip, detail],
        )
        .context("写入 Portal 审计失败")?;
        Ok(())
    }

    /// 最近的审计条目（adminApiKey 侧的 `/api/admin/portal/audit` 与测试用）。
    ///
    /// # 为何转调 [`Self::audit_page`] 而不自己写一条 SQL
    /// 原先这里有一份独立的 `SELECT … ORDER BY at_ms DESC, id DESC LIMIT ?`，
    /// 与 `audit_page` 构成**同一张表的两个读取路径**。两份 SQL 眼下语义一致
    /// （我逐字核对过排序键），但一致是靠人核对维持的：日后给 `audit_page` 的排序
    /// 或过滤动一次刀，这一份不会跟着走，于是两个入口对「最近 20 条是哪 20 条」
    /// 给出不同答案——而两边各自看都是自洽的，没有任何一处会报错。
    ///
    /// 转调之后排序键、字段映射、错误信息都只有一份。代价是多算一次 `COUNT(*)`
    /// （`audit_page` 要算 total），在 5000 行上限下是覆盖索引上的一次扫描，
    /// 拿来换掉一个慢性漂移风险很划算。
    ///
    /// 【为何不能在这里先拿锁】`audit_page` 内部会 `self.conn.lock()`，而这个
    /// Mutex 不可重入——先锁再转调就是立刻死锁。
    ///
    /// # `limit == 0` 必须提前返回
    /// `audit_page` 把 limit 钳到 `[1, AUDIT_EXPORT_MAX]`——那个钳位对分页是对的
    /// （`limit=0` 会让界面一片空白而不报错，见 `AuditParams` 的说明），但对
    /// 「取最近 0 条」这个直白的请求就是**答非所问**：原先那份 SQL 走 `LIMIT 0`
    /// 老老实实给 0 条，转调之后会给 1 条。
    ///
    /// 实测踩过：改成转调后专门试了 `recent_audit(0)`，返回 1 条。眼下没有调用方
    /// 传 0（都是 200/50/20/10 的常量），所以它是一个**潜伏**的差异——等到某天
    /// 有人写出 `recent_audit(n)` 而 n 可能算出 0，多出来的那一条会被当成真实数据。
    pub fn recent_audit(&self, limit: usize) -> Result<Vec<AuditEntry>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let page = self.audit_page(&AuditQuery {
            limit: limit as i64,
            ..Default::default()
        })?;
        Ok(page.rows)
    }

    /// 带筛选与分页的审计查询。
    ///
    /// # 为何 `total` 与 `rows` 共用一份 WHERE
    /// 两处各写一遍 SQL 的话，日后加一个筛选维度只改了一处，表现是「筛出 3 条却
    /// 显示共 812 条」——而那种不一致看起来像分页坏了，实际是计数没跟着筛。
    /// 这里把 WHERE 拼一次，两条语句共用，从结构上不可能漂移。
    pub fn audit_page(&self, q: &AuditQuery) -> Result<AuditPage> {
        let conn = self.conn.lock();

        // 条件拼接：每个 Some 追加一条 AND 和一个绑定值。
        //
        // 【为何用绑定值而不是把值内插进 SQL】username 与 action 来自 HTTP 查询串，
        // 内插就是 SQL 注入。审计表本身是只读查询，但同一个连接上有余额和会话——
        // 一次注入足够改余额或读走会话哈希。
        let mut where_sql = String::from(" WHERE 1=1");
        let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(u) = &q.username {
            where_sql.push_str(" AND username = ?");
            binds.push(Box::new(u.clone()));
        }
        if let Some(a) = &q.action {
            where_sql.push_str(" AND action = ?");
            binds.push(Box::new(a.clone()));
        }
        if let Some(p) = &q.action_prefix {
            // 前缀匹配用 `LIKE 'x%'`，且把 LIKE 的元字符转义掉。
            //
            // 【为何必须转义】`%` 和 `_` 在 LIKE 里是通配符。不转义时前缀 `login_`
            // 里的下划线会匹配**任意一个字符**，于是 `loginXfail` 也被算进来——
            // 眼下没有这种动作名，但审计筛选静默多捞几条是最难发现的一类错。
            //
            // # 为何不改写成 `action >= 'x' AND action < 'y'`（那样能走索引）
            // 实测过：范围写法确实能让 `idx_portal_audit_action` 变成
            // `SEARCH ... USING COVERING INDEX`，而 LIKE 写法是
            // `SCAN portal_audit USING INDEX idx_portal_audit_at`（全表扫）。
            // 但两者**语义不等价**，且差异对使用者不可见：
            //
            // - SQLite 的 `LIKE` 对 ASCII 默认**大小写不敏感**，范围比较（BINARY
            //   排序）是敏感的。实测 `LIKE 'login%'` 会匹配 `LOGIN_UPPER`、
            //   `LoGiN_mixed`，范围写法不会。生产代码写入的动作名全是小写
            //   snake_case，所以对**现有数据**无差别；但筛选框里的前缀是人手打的，
            //   有人输入 `Login` 时两种写法会给出不同结果。
            // - 上界的构造（末字符 +1）在末字符是 `char::MAX` 时没有下一个码点，
            //   得单独兜底——一条只在极端输入下才踩到的分支。
            //
            // 保留上限是 5000 行（[`crate::portal::MAX_AUDIT_ROWS`]），这个规模下
            // 全表扫是微秒级；用「不可见的语义变化」换一个测不出差别的性能提升
            // 不值得。索引 `idx_portal_audit_action` 服务的是**精确**动作筛选和
            // [`Self::audit_actions`] 的 GROUP BY，这一条前缀筛确实不走它。
            let escaped = p
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            where_sql.push_str(" AND action LIKE ? ESCAPE '\\'");
            binds.push(Box::new(format!("{escaped}%")));
        }
        if let Some(s) = q.since_ms {
            where_sql.push_str(" AND at_ms >= ?");
            binds.push(Box::new(s));
        }
        if let Some(u) = q.until_ms {
            where_sql.push_str(" AND at_ms <= ?");
            binds.push(Box::new(u));
        }

        let bind_refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();

        let total: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM portal_audit{where_sql}"),
                bind_refs.as_slice(),
                |r| r.get(0),
            )
            .context("统计审计条数失败")?;

        // 排序键带上 id：单靠 at_ms 在同一毫秒内的多条记录之间是**不确定顺序**的，
        // 而不确定顺序会让翻页出现「同一条在两页都出现、另一条谁都看不到」。
        // 登录失败风暴正是同毫秒多条的场景，也正是最需要看清的场景。
        let sql = format!(
            "SELECT id, at_ms, username, action, client_ip, detail
             FROM portal_audit{where_sql}
             ORDER BY at_ms DESC, id DESC LIMIT ? OFFSET ?"
        );
        let mut page_binds = bind_refs.clone();
        let limit = q.limit.clamp(1, AUDIT_EXPORT_MAX);
        let offset = q.offset.max(0);
        page_binds.push(&limit);
        page_binds.push(&offset);

        let mut stmt = conn.prepare(&sql).context("准备审计分页查询失败")?;
        let rows = stmt
            .query_map(page_binds.as_slice(), |row| {
                Ok(AuditEntry {
                    id: row.get(0)?,
                    at_ms: row.get(1)?,
                    username: row.get(2)?,
                    action: row.get(3)?,
                    client_ip: row.get(4)?,
                    detail: row.get(5)?,
                })
            })
            .context("执行审计分页查询失败")?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.context("读取审计行失败")?);
        }

        Ok(AuditPage {
            has_more: offset + (out.len() as i64) < total,
            rows: out,
            total,
            offset,
            limit,
        })
    }

    /// 审计里出现过的全部动作及次数，按次数降序。供筛选下拉。
    ///
    /// # 为何从库里查而不是返回代码里的动作常量清单
    /// 清单会漂：新增一种动作忘了加进清单，它就永远不出现在下拉里——而那条动作
    /// 恰恰可能是新加的、最需要观察的。从实际数据里查，只要发生过就一定在。
    /// 代价是「从未发生过的动作」不出现在下拉里，那正确：筛它必然是空结果。
    pub fn audit_actions(&self) -> Result<Vec<ActionCount>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT action, COUNT(*) FROM portal_audit
                 GROUP BY action ORDER BY COUNT(*) DESC, action ASC",
            )
            .context("准备审计动作聚合失败")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(ActionCount {
                    action: r.get(0)?,
                    count: r.get(1)?,
                })
            })
            .context("执行审计动作聚合失败")?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.context("读取动作聚合行失败")?);
        }
        Ok(out)
    }

    /// 审计保留清理：只留最近 `keep` 条，返回删除条数。
    ///
    /// 审计是每次登录 + 每次明文外显都写一条的高频表，公网暴露下更是撞库者的写放大目标，
    /// 不设上限迟早把磁盘写满。按 id 保留最近 N 条，与 traces 的按天清理是同类措施。
    pub fn trim_audit(&self, keep: usize) -> Result<usize> {
        let conn = self.conn.lock();
        let n = conn
            .execute(
                "DELETE FROM portal_audit WHERE id NOT IN (
                     SELECT id FROM portal_audit ORDER BY id DESC LIMIT ?1
                 )",
                params![keep as i64],
            )
            .context("清理 Portal 审计失败")?;
        Ok(n)
    }
}

// ---------------- 事务内的自由函数 ----------------
//
// 这些函数收 `&Connection` 而不是 `&self`，因为 `PortalDb` 的 Mutex 不可重入：
// `board` 已经持有锁，若它去调 `self.adjust_balance()`（内部又要 lock）会立刻死锁。
// 拆成自由函数让「拿锁开事务」和「事务内做事」彻底分层，两个入口共用同一份逻辑，
// 也就不会出现「充值走一套分类、上车走另一套」这种慢性对账偏差。

/// 事务内读余额，无行按 0。
fn read_balance(conn: &Connection, user_id: i64) -> Result<i64> {
    let v: Option<i64> = conn
        .query_row(
            "SELECT balance FROM portal_balances WHERE user_id = ?1",
            params![user_id],
            |r| r.get(0),
        )
        .optional()
        .context("事务内查余额失败")?;
    Ok(v.unwrap_or(0))
}

/// 事务内数上车人数。
fn count_unlockers(conn: &Connection, credential_id: i64) -> Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM portal_unlocks WHERE credential_id = ?1",
        params![credential_id],
        |r| r.get(0),
    )
    .context("统计上车人数失败")
}

/// 事务内读价格快照。
fn read_pricing(conn: &Connection, credential_id: i64) -> Result<Option<Pricing>> {
    conn.query_row(
        "SELECT base_count, base_price, total_price, min_price, max_unlockers
         FROM portal_key_pricing WHERE credential_id = ?1",
        params![credential_id],
        |r| {
            Ok(Pricing {
                base_count: r.get::<_, i64>(0)? as u32,
                base_price: r.get(1)?,
                total_price: r.get(2)?,
                min_price: r.get(3)?,
                max_unlockers: r.get::<_, i64>(4)? as u32,
            })
        },
    )
    .optional()
    .context("查价格快照失败")
}

/// 冻结价格快照。用 `OR IGNORE`：并发下两个「首次上车」可能都走到这里，
/// 后到的那个必须沿用先到者冻结的参数，而不是覆盖成自己读到的配置——
/// 否则同一把 key 会因配置刚好被改而出现两种价格。
fn freeze_pricing(conn: &Connection, credential_id: i64, p: &Pricing, now_ms: i64) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO portal_key_pricing
           (credential_id, base_count, base_price, total_price, min_price, max_unlockers, frozen_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            credential_id,
            p.base_count as i64,
            p.base_price,
            p.total_price,
            p.min_price,
            p.max_unlockers as i64,
            now_ms
        ],
    )
    .context("冻结价格快照失败")?;
    Ok(())
}

/// 事务内改余额 + 写流水。返回 `Ok(None)` 表示扣分会导致负余额（调用方回滚）。
///
/// **不 commit**：提交时机归调用方。上车要在同一个事务里连做扣费和多笔退款，
/// 中途 commit 会让「扣了但没退」成为可观测的中间状态。
fn apply_delta(
    conn: &Connection,
    user_id: i64,
    delta: i64,
    kind: &str,
    credential_id: Option<i64>,
    note: Option<&str>,
    at_ms: i64,
) -> Result<Option<i64>> {
    let cur = read_balance(conn, user_id)?;
    let next = cur + delta;
    if next < 0 {
        return Ok(None);
    }

    // topup / spent 按**业务语义**分类，不按 delta 正负号分类。
    //
    // 退款的 delta 是正数，但它绝不是「充值」——差额模型下每有新人上车，前面所有
    // 人都退一次，若按正负号归类，用户看到的「累计充值」会随别人上车无上限地涨。
    // 退款是把之前多收的那部分**冲回去**，因此冲减 spent：
    //   净支出 = spent，且 balance == topup - spent 这个恒等式对退款也成立。
    let (topup_inc, spent_inc) = match kind {
        REFUND_KIND => (0, -delta),
        _ if delta > 0 => (delta, 0),
        _ => (0, -delta),
    };

    conn.execute(
        "INSERT INTO portal_balances (user_id, balance, topup, spent)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(user_id) DO UPDATE SET
           balance = balance + ?5,
           topup   = topup   + ?3,
           spent   = spent   + ?4",
        params![user_id, next, topup_inc, spent_inc, delta],
    )
    .context("写入余额失败")?;

    conn.execute(
        "INSERT INTO portal_ledger (user_id, at_ms, delta, balance_after, kind, credential_id, note)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![user_id, at_ms, delta, next, kind, credential_id, note],
    )
    .context("写入流水失败")?;

    Ok(Some(next))
}

// ---------------- 看板聚合（都收 `&Connection`，由 `dashboard` 在一把锁里串起来）----------------

/// 一个时间窗内的总量。`since = None` 表示累计（不设下界）。
///
/// # 为何用 `i64::MIN` 代替「没有 WHERE 子句」
/// 两套 SQL（带 WHERE / 不带）意味着「今日」和「累计」走**不同的代码路径**，
/// 而这两个数字之间有约束（今日 ≤ 累计）。路径分叉时，只改对其中一条的 bug
/// 恰好会破坏那个约束却仍然「有数字」。`at_ms >= i64::MIN` 恒真，一条 SQL
/// 同时服务两种窗口，今日与累计的差别就只剩参数值。
fn window_totals(conn: &Connection, since: Option<i64>) -> Result<DashboardWindow> {
    let lower = since.unwrap_or(i64::MIN);

    let tickets: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM portal_unlocks WHERE unlocked_at_ms >= ?1",
            params![lower],
            |r| r.get(0),
        )
        .context("统计发票数失败")?;

    // 一次 GROUP BY 取全部 kind，而不是每个 kind 各发一条 SQL。
    let mut w = DashboardWindow {
        tickets,
        ..Default::default()
    };
    {
        let mut stmt = conn
            .prepare(
                "SELECT kind, COALESCE(SUM(delta), 0) FROM portal_ledger
                 WHERE at_ms >= ?1 GROUP BY kind",
            )
            .context("准备流水聚合失败")?;
        let rows = stmt
            .query_map(params![lower], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })
            .context("执行流水聚合失败")?;
        for row in rows {
            let (kind, sum) = row.context("读取流水聚合行失败")?;
            match kind.as_str() {
                TOPUP_KIND => w.topup = sum,
                // 出账的 delta 是负数，取反后存正数（见 DashboardWindow::spend 的说明）。
                UNLOCK_KIND => w.spend = -sum,
                REFUND_KIND => w.refund = sum,
                ADMIN_ADJUST_KIND => w.adjust = sum,
                // 未知 kind 直接忽略。这不是「容错」而是**已知的取舍**：日后新增
                // 一种 kind 若忘了在这里加分支，它的金额会在看板上凭空消失。
                // `dashboard_covers_every_ledger_kind` 这条测试就是为此存在的——
                // 它遍历全部 kind 常量，要求每一种都能被本函数计入某个字段。
                other => {
                    tracing::warn!("看板遇到未计入的流水 kind: {other}");
                }
            }
        }
    }
    Ok(w)
}

/// 车辆热度，按乘客数降序、同数按最近上车时间降序，取前 `top` 辆。
///
/// 单价来自价格快照 + 当前人数，与 `board` 用的是同一个 [`Pricing::unit_price`]——
/// 若在 SQL 里重算一遍两段式公式，改价规则时就得记得同时改两处，而漏改的表现是
/// 看板价与实际扣费价不一致（用户会以为被多扣了）。
fn key_heat(conn: &Connection, top: usize) -> Result<Vec<KeyHeat>> {
    let mut stmt = conn
        .prepare(
            "SELECT u.credential_id, COUNT(*), COALESCE(SUM(u.paid), 0),
                    MIN(u.unlocked_at_ms), MAX(u.unlocked_at_ms)
             FROM portal_unlocks u
             GROUP BY u.credential_id
             ORDER BY COUNT(*) DESC, MAX(u.unlocked_at_ms) DESC
             LIMIT ?1",
        )
        .context("准备车辆热度查询失败")?;
    let rows = stmt
        .query_map(params![top as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })
        .context("执行车辆热度查询失败")?;

    let mut out = Vec::new();
    for row in rows {
        let (cid, passengers, revenue, first, last) = row.context("读取车辆热度行失败")?;
        let unit_price = read_pricing(conn, cid)?.map(|p| p.unit_price(passengers as u32));
        out.push(KeyHeat {
            credential_id: cid,
            passengers,
            unit_price,
            revenue,
            first_boarded_ms: first,
            last_boarded_ms: last,
        });
    }
    Ok(out)
}

/// 用户分层。三档互斥且穷尽（见 [`UserTiers`]）。
///
/// 用一条 SQL 而非三条分别 COUNT：三条各带自己的 WHERE，条件之间的「互斥且穷尽」
/// 就成了要靠人核对的性质——某天改了其中一条的边界，另外两条不会跟着变，于是
/// 三个数字加起来不等于总数，或者某个用户被数两遍。`CASE` 让每一行**恰好**落进
/// 一档，穷尽性由 SQL 本身保证。
fn user_tiers(conn: &Connection) -> Result<UserTiers> {
    conn.query_row(
        "SELECT
           COUNT(*),
           SUM(CASE WHEN aboard > 0 AND bal > 0  THEN 1 ELSE 0 END),
           SUM(CASE WHEN aboard > 0 AND bal <= 0 THEN 1 ELSE 0 END),
           SUM(CASE WHEN aboard = 0              THEN 1 ELSE 0 END),
           SUM(CASE WHEN disabled != 0           THEN 1 ELSE 0 END)
         FROM (
           SELECT u.id, u.disabled,
                  COALESCE(b.balance, 0) AS bal,
                  (SELECT COUNT(*) FROM portal_unlocks k WHERE k.user_id = u.id) AS aboard
           FROM portal_users u
           LEFT JOIN portal_balances b ON b.user_id = u.id
         )",
        [],
        |r| {
            Ok(UserTiers {
                total: r.get(0)?,
                // 空表时 SUM 返回 NULL 而 COUNT 返回 0。取成 Option 再兜 0，
                // 否则空库上整个看板会以类型错误告终——而「刚部署完打开看板」
                // 正是它第一次被访问的时刻。
                active: r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                broke: r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                zombie: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                disabled: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
            })
        },
    )
    .context("统计用户分层失败")
}

/// 近期登录失败的 top IP。
///
/// 匹配 `action LIKE 'login_fail%'` 而不是枚举那四个具体动作：失败原因日后可能
/// 新增（见 [`super::auth`] 的 `audit_action`），而「新增一种失败原因就从异常
/// 统计里漏掉」是最不该发生的漏——异常统计的用途正是发现没预料到的东西。
fn login_fails(conn: &Connection, since_ms: i64, top: usize) -> Result<Vec<LoginFailRow>> {
    let mut stmt = conn
        .prepare(
            "SELECT client_ip, COUNT(*) FROM portal_audit
             WHERE at_ms >= ?1 AND action LIKE 'login_fail%'
             GROUP BY client_ip
             ORDER BY COUNT(*) DESC
             LIMIT ?2",
        )
        .context("准备失败登录统计失败")?;
    let rows = stmt
        .query_map(params![since_ms, top as i64], |r| {
            Ok(LoginFailRow {
                client_ip: r.get(0)?,
                count: r.get(1)?,
            })
        })
        .context("执行失败登录统计失败")?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.context("读取失败登录行失败")?);
    }
    Ok(out)
}

/// 账目自检：两条恒等式（见 [`IntegrityCheck`]）。
fn integrity_check(conn: &Connection) -> Result<IntegrityCheck> {
    let balance_sum: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(balance), 0) FROM portal_balances",
            [],
            |r| r.get(0),
        )
        .context("统计余额总和失败")?;

    // 只算仍在册用户的流水（理由见 IntegrityCheck::ledger_sum）。
    let ledger_sum: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(l.delta), 0) FROM portal_ledger l
             WHERE EXISTS (SELECT 1 FROM portal_users u WHERE u.id = l.user_id)",
            [],
            |r| r.get(0),
        )
        .context("统计流水总和失败")?;

    let wallet_violations: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM portal_balances WHERE balance != topup - spent",
            [],
            |r| r.get(0),
        )
        .context("校验钱包恒等式失败")?;

    Ok(IntegrityCheck {
        balance_sum,
        ledger_sum,
        wallet_violations,
        ok: balance_sum == ledger_sum && wallet_violations == 0,
    })
}

fn row_to_import_key(row: &rusqlite::Row<'_>) -> rusqlite::Result<ImportKeyMeta> {
    Ok(ImportKeyMeta {
        key_hash: row.get(0)?,
        fingerprint: row.get(1)?,
        credential_id: row.get(2)?,
        region: row.get(3)?,
        endpoint: row.get(4)?,
        first_seen_ms: row.get(5)?,
        last_seen_ms: row.get(6)?,
        push_count: row.get(7)?,
        ok: row.get::<_, i64>(8)? != 0,
        error: row.get(9)?,
    })
}

#[cfg(test)]
mod tests {
    use super::super::role::RoleKind;
    use super::*;

    /// 固定的假哈希串：本层不做哈希，测试只关心存取，不需要真 argon2（省掉每次 30ms）。
    const PHC: &str = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$aaaa";

    fn db() -> PortalDb {
        PortalDb::open_in_memory().unwrap()
    }

    /// 新库开出来就必须有 `role` 列，且默认是最低权限。
    ///
    /// 【为何 role 不写在 CREATE TABLE 里】写在建表语句里的话，新库和老库会走两条
    /// 不同的路径拿到这一列（新库靠建表、老库靠迁移），于是迁移那条路只在真实老库上
    /// 才被执行到——本地开发全是新库，迁移写错了也测不出来。让两者都走迁移是**单一路径**：
    /// 每个开发者每次跑测试都在验证迁移本身。
    #[test]
    fn fresh_db_has_role_column_defaulting_to_user() {
        let db = db();
        let id = db.create_user("alice", "hash", 1000).unwrap().unwrap();
        let conn = db.conn.lock();
        let role: String = conn
            .query_row(
                "SELECT role FROM portal_users WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(role, "user", "新建用户必须是最低权限，不能默认成管理员");
    }

    /// **迁移的核心用例：模拟改动之前就存在的老库。**
    ///
    /// 手工建一张**没有 role 列**的 `portal_users`（这正是老部署里的样子）、塞进一个
    /// 老用户，然后跑迁移，验证：列被补上、且老用户拿到的是 `user` 而不是任何更高权限。
    ///
    /// 【为何这一条必须存在】`CREATE TABLE IF NOT EXISTS` 对已存在的表完全无效。
    /// 若只有上面那个新库用例，迁移逻辑整段删掉它也照样绿——因为新库的表是刚建的。
    /// 只有从「缺列的表」出发，才能证明补列这件事真的发生了。
    #[test]
    fn migration_adds_role_to_preexisting_db_as_lowest_privilege() {
        let conn = Connection::open_in_memory().unwrap();
        // 老库的原始形态：没有 role 列。
        conn.execute_batch(
            "CREATE TABLE portal_users (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                username      TEXT NOT NULL UNIQUE COLLATE NOCASE,
                password_hash TEXT NOT NULL,
                disabled      INTEGER NOT NULL DEFAULT 0,
                created_at_ms INTEGER NOT NULL,
                last_login_ms INTEGER
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO portal_users (username, password_hash, created_at_ms)
             VALUES ('legacy', 'hash', 1)",
            [],
        )
        .unwrap();

        // 对照组：迁移前确实没有这一列。缺了这一步，万一 has_column 恒返回 false，
        // 下面的断言仍会通过，而我们其实没验证到任何东西。
        assert!(
            !PortalDb::has_column(&conn, "portal_users", "role").unwrap(),
            "对照组失败：老库不该有 role 列，测试前提已不成立"
        );

        PortalDb::migrate(&conn).unwrap();

        assert!(
            PortalDb::has_column(&conn, "portal_users", "role").unwrap(),
            "迁移后 role 列仍不存在"
        );
        let role: String = conn
            .query_row(
                "SELECT role FROM portal_users WHERE username = 'legacy'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            role, "user",
            "老用户必须落在最低权限。若默认值让他们变成管理员，一次升级就等于给所有历史账号提权"
        );
    }

    /// 迁移必须幂等：连跑多次不报错、结果不变。
    ///
    /// 【为何要测】`ALTER TABLE ADD COLUMN` 对已有列会直接报错。启动时每次开库都会
    /// 跑一遍迁移，所以「第二次启动就崩」这种 bug 只在重启时暴露——而开发时通常
    /// 只启动一次。
    #[test]
    fn migration_is_idempotent() {
        let db = db();
        {
            let conn = db.conn.lock();
            for _ in 0..3 {
                PortalDb::migrate(&conn).expect("重复迁移不该报错");
            }
        }
        // 仍然可用，且列没被改坏。
        let id = db.create_user("bob", "hash", 1000).unwrap().unwrap();
        let conn = db.conn.lock();
        let role: String = conn
            .query_row(
                "SELECT role FROM portal_users WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(role, "user");
    }

    // ---------------- 角色读写 ----------------

    /// 默认路径（`create_user`）建出来的用户必须是普通用户。
    ///
    /// 【为何这条最重要】`create_user` 有近 20 个调用点，注册接口是其中之一。
    /// 若它建出的用户是 admin，任何人自助注册就拿到运营权限。这条锁住
    /// 「不写 role 列 → 落到 DEFAULT 'user'」这个隐式依赖。
    #[test]
    fn create_user_defaults_to_plain_user() {
        let db = db();
        let id = db.create_user("alice", PHC, 1000).unwrap().unwrap();
        assert_eq!(db.role_of(id).unwrap(), Some(RoleKind::User));
        assert!(!db.role_of(id).unwrap().unwrap().can_manage());
    }

    /// 显式指定角色的建号路径：三档都能建出来。
    #[test]
    fn create_user_with_role_honors_each_role() {
        let db = db();
        for (i, r) in RoleKind::ALL.into_iter().enumerate() {
            let name = format!("u{i}");
            let id = db
                .create_user_with_role(&name, PHC, r, 1000)
                .unwrap()
                .unwrap();
            assert_eq!(
                db.role_of(id).unwrap(),
                Some(r),
                "建号时的角色没落库: {r:?}"
            );
        }
    }

    /// 改角色：三档之间任意切换都生效。
    #[test]
    fn set_role_switches_between_all_roles() {
        let db = db();
        let id = db.create_user("alice", PHC, 1000).unwrap().unwrap();
        for r in RoleKind::ALL {
            assert!(db.set_role(id, r).unwrap(), "set_role 没命中行");
            assert_eq!(db.role_of(id).unwrap(), Some(r));
        }
    }

    /// 不存在的用户：`role_of` 返回 None，`set_role` 返回 false。
    ///
    /// 【为何要区分】调用方（改角色前的校验）必须能分辨「用户不存在」与
    /// 「用户存在且是 user」。若两者都返回 User，给不存在的 id 改角色会
    /// 静默成功，运营者以为改了、实际什么都没发生。
    #[test]
    fn missing_user_is_distinguishable_from_plain_user() {
        let db = db();
        assert_eq!(db.role_of(9999).unwrap(), None, "不存在的用户不该有角色");
        assert!(
            !db.set_role(9999, RoleKind::Admin).unwrap(),
            "改了不存在的用户"
        );
        // 对照组：存在的用户两者都必须有结果，否则上面的 None/false 可能只是函数坏了。
        let id = db.create_user("alice", PHC, 1000).unwrap().unwrap();
        assert_eq!(db.role_of(id).unwrap(), Some(RoleKind::User));
        assert!(db.set_role(id, RoleKind::Admin).unwrap());
    }

    /// 会话必须带上角色，且角色变更后**新会话**能看到新角色。
    #[test]
    fn session_carries_role() {
        let db = db();
        let id = db.create_user("alice", PHC, 1000).unwrap().unwrap();
        db.create_session("h1", id, 1000, 9000, None).unwrap();

        let s = db.validate_session("h1", 2000).unwrap().unwrap();
        assert_eq!(s.role, RoleKind::User, "默认会话该是普通用户");
        assert!(!s.role.can_manage());

        db.set_role(id, RoleKind::Admin).unwrap();
        let s2 = db.validate_session("h1", 2000).unwrap().unwrap();
        assert_eq!(s2.role, RoleKind::Admin, "会话角色应随库里的值走");
        assert!(s2.role.can_manage());
    }

    /// 库里的 role 被手工改坏时，用户仍能登录，但**只拿到普通用户权限**。
    ///
    /// 【为何不让它登录失败】一个显示层面的脏数据不该升级成拒绝服务。
    /// 【为何不回退成 admin】那意味着改坏一个字符就提权。
    #[test]
    fn corrupted_role_degrades_to_user_not_admin() {
        let db = db();
        let id = db.create_user("alice", PHC, 1000).unwrap().unwrap();
        db.create_session("h1", id, 1000, 9000, None).unwrap();
        {
            let conn = db.conn.lock();
            conn.execute(
                "UPDATE portal_users SET role = 'SUPERUSER' WHERE id = ?1",
                params![id],
            )
            .unwrap();
        }
        let s = db
            .validate_session("h1", 2000)
            .unwrap()
            .expect("脏 role 不该让用户登不进来");
        assert_eq!(s.role, RoleKind::User);
        assert!(!s.role.can_manage(), "脏 role 提权了");
        assert_eq!(db.role_of(id).unwrap(), Some(RoleKind::User));
    }

    /// admin 计数只数启用的 admin。
    ///
    /// 【为何排除 disabled】「最后一个管理员」的判断若把停用的号也算进去，
    /// 就会允许把唯一可用的管理员降级——此后 portal 内没人能管用户。
    #[test]
    fn admin_count_ignores_disabled_admins() {
        let db = db();
        assert_eq!(db.admin_count().unwrap(), 0);

        let a = db
            .create_user_with_role("boss", PHC, RoleKind::Admin, 1000)
            .unwrap()
            .unwrap();
        let b = db
            .create_user_with_role("boss2", PHC, RoleKind::Admin, 1000)
            .unwrap()
            .unwrap();
        db.create_user("plain", PHC, 1000).unwrap().unwrap();
        assert_eq!(db.admin_count().unwrap(), 2, "普通用户被算成了管理员");

        db.set_disabled(b, true).unwrap();
        assert_eq!(db.admin_count().unwrap(), 1, "停用的管理员仍被计入");

        db.set_role(a, RoleKind::User).unwrap();
        assert_eq!(db.admin_count().unwrap(), 0, "降级后仍被计入");
    }

    /// 四张积分表必须真的建出来，且列名与设计一致。
    ///
    /// 【为何要单独测建表】`execute_batch` 里少一个逗号、表名拼错，编译期都发现不了，
    /// 要等到第一次真正读写才炸。而那时错误信息是「no such table」或「no such column」，
    /// 出现在业务代码里，看起来像业务 bug。这个用例把它锁在建表这一层。
    #[test]
    fn credit_tables_exist_with_expected_columns() {
        let db = db();
        let conn = db.conn.lock();
        for (table, expected) in [
            (
                "portal_balances",
                vec!["user_id", "balance", "topup", "spent"],
            ),
            (
                "portal_unlocks",
                vec!["user_id", "credential_id", "paid", "unlocked_at_ms"],
            ),
            (
                "portal_key_pricing",
                vec![
                    "credential_id",
                    "base_count",
                    "base_price",
                    "total_price",
                    "min_price",
                    "max_unlockers",
                    "frozen_at_ms",
                ],
            ),
            (
                "portal_ledger",
                vec![
                    "id",
                    "user_id",
                    "at_ms",
                    "delta",
                    "balance_after",
                    "kind",
                    "credential_id",
                    "note",
                ],
            ),
        ] {
            let mut stmt = conn
                .prepare(&format!("PRAGMA table_info({table})"))
                .unwrap_or_else(|e| panic!("表 {table} 不存在或无法内省: {e}"));
            let cols: Vec<String> = stmt
                .query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            assert!(!cols.is_empty(), "表 {table} 不存在");
            for c in expected {
                assert!(
                    cols.contains(&c.to_string()),
                    "表 {table} 缺列 {c}（实得 {cols:?}）"
                );
            }
        }
    }

    // ---------------- 钱包 ----------------

    #[test]
    fn wallet_starts_at_zero_for_new_user() {
        let db = db();
        let uid = db.create_user("alice", PHC, 1000).unwrap().unwrap();
        let w = db.wallet_of(uid).unwrap();
        assert_eq!(w.balance, 0, "新用户余额必须是 0，不能凭空有分");
        assert_eq!(w.topup, 0);
        assert_eq!(w.spent, 0);
    }

    #[test]
    fn topup_increases_balance_and_writes_ledger() {
        let db = db();
        let uid = db.create_user("alice", PHC, 1000).unwrap().unwrap();
        let after = db
            .adjust_balance(uid, 50, TOPUP_KIND, None, Some("首充"), 2000)
            .unwrap();
        assert_eq!(after, Some(50), "调账成功返回调账后余额");

        let w = db.wallet_of(uid).unwrap();
        assert_eq!(w.balance, 50);
        assert_eq!(w.topup, 50, "正向调整要累加到 topup");
        assert_eq!(w.spent, 0);

        let rows = db.ledger_of(uid, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].delta, 50);
        assert_eq!(rows[0].balance_after, 50, "流水要带余额快照");
        assert_eq!(rows[0].kind, TOPUP_KIND);
        assert_eq!(rows[0].note.as_deref(), Some("首充"));
    }

    /// 安全红线：余额永不为负。
    ///
    /// 若此用例失败，说明用户可以「透支」看凭据——积分门槛形同虚设，
    /// 且 spent 会算成负数让对账彻底失真。
    #[test]
    fn balance_never_goes_negative() {
        let db = db();
        let uid = db.create_user("alice", PHC, 1000).unwrap().unwrap();
        db.adjust_balance(uid, 10, TOPUP_KIND, None, None, 2000)
            .unwrap();

        // 余额不足返回 Ok(None)：这是正常业务结果（「只剩 10 分，扣不动 25 分」），
        // 不是错误。用 Err 会迫使调用方从错误字符串里解析原因，那才是设计事故。
        let res = db
            .adjust_balance(uid, -25, UNLOCK_KIND, Some(7), None, 3000)
            .expect("查询本身不该失败");
        assert_eq!(res, None, "扣超余额必须被拒，不能扣成负数");

        // 失败后状态完全不变——不能出现「钱扣了但记录没写」这种半途状态。
        let w = db.wallet_of(uid).unwrap();
        assert_eq!(w.balance, 10, "失败的扣费不得改变余额");
        assert_eq!(w.spent, 0);
        assert_eq!(db.ledger_of(uid, 10).unwrap().len(), 1, "失败不得留下流水");
    }

    #[test]
    fn spend_accumulates_into_spent() {
        let db = db();
        let uid = db.create_user("alice", PHC, 1000).unwrap().unwrap();
        db.adjust_balance(uid, 100, TOPUP_KIND, None, None, 1000)
            .unwrap();
        db.adjust_balance(uid, -7, UNLOCK_KIND, Some(9001), None, 2000)
            .unwrap();
        db.adjust_balance(uid, -5, UNLOCK_KIND, Some(9002), None, 3000)
            .unwrap();

        let w = db.wallet_of(uid).unwrap();
        assert_eq!(w.balance, 88);
        assert_eq!(w.topup, 100);
        assert_eq!(w.spent, 12, "两次扣费累加进 spent");
    }

    /// 退款要冲减 spent，否则「累计支出」会虚高。
    ///
    /// 差额模型下退款很频繁（每有新人上车，前面所有人都退一次），
    /// 若退款不冲减 spent，用户看到的「一共花了多少」会远大于实际净支出。
    #[test]
    fn refund_reduces_spent_not_topup() {
        let db = db();
        let uid = db.create_user("alice", PHC, 1000).unwrap().unwrap();
        db.adjust_balance(uid, 100, TOPUP_KIND, None, None, 1000)
            .unwrap();
        db.adjust_balance(uid, -7, UNLOCK_KIND, Some(9001), None, 2000)
            .unwrap();
        db.adjust_balance(uid, 2, REFUND_KIND, Some(9001), None, 3000)
            .unwrap();

        let w = db.wallet_of(uid).unwrap();
        assert_eq!(w.balance, 95);
        assert_eq!(w.topup, 100, "退款不算充值");
        assert_eq!(w.spent, 5, "净支出 = 7 - 2");
    }

    /// 恒等式 `balance == topup - spent` 必须对任意 kind 序列成立。
    ///
    /// 上面几个用例各自钉住一种 kind 的分类，但分类是否**自相一致**要靠恒等式查：
    /// 只要某个 kind 走错分支（比如 admin_adjust 的负值忘了记 spent），
    /// 单看余额是对的、单看 topup 也是对的，只有这个等式会崩。
    #[test]
    fn balance_always_equals_topup_minus_spent() {
        let db = db();
        let uid = db.create_user("alice", PHC, 1000).unwrap().unwrap();

        // 覆盖全部四种 kind，正负都有，并夹一次被拒的扣费（不得留下痕迹）。
        let ops: &[(i64, &str)] = &[
            (100, TOPUP_KIND),
            (-10, UNLOCK_KIND),
            (3, REFUND_KIND),
            (50, ADMIN_ADJUST_KIND),
            (-20, ADMIN_ADJUST_KIND),
            (-7, UNLOCK_KIND),
            (2, REFUND_KIND),
            (-9999, UNLOCK_KIND), // 余额不足，应被拒
            (-1, UNLOCK_KIND),
        ];

        for (i, (delta, kind)) in ops.iter().enumerate() {
            db.adjust_balance(uid, *delta, kind, None, None, 1000 + i as i64)
                .unwrap();
            let w = db.wallet_of(uid).unwrap();
            assert_eq!(
                w.balance,
                w.topup - w.spent,
                "第 {} 步（{delta} / {kind}）后恒等式被破坏：{w:?}",
                i + 1
            );
            assert!(w.balance >= 0, "余额不得为负：{w:?}");
        }

        let w = db.wallet_of(uid).unwrap();
        assert_eq!(w.topup, 150, "只有 topup / 正向 admin_adjust 计入充值");
        assert_eq!(w.spent, 33, "净支出 = 10+20+7+1 - 3 - 2");
        assert_eq!(w.balance, 117);
    }

    #[test]
    fn ledger_is_newest_first_and_capped() {
        let db = db();
        let uid = db.create_user("alice", PHC, 1000).unwrap().unwrap();
        for i in 1..=5 {
            db.adjust_balance(
                uid,
                10,
                TOPUP_KIND,
                None,
                Some(&format!("第{i}笔")),
                1000 + i,
            )
            .unwrap();
        }
        let rows = db.ledger_of(uid, 3).unwrap();
        assert_eq!(rows.len(), 3, "limit 生效");
        assert_eq!(rows[0].note.as_deref(), Some("第5笔"), "最新的在前");
        assert_eq!(rows[2].note.as_deref(), Some("第3笔"));
    }

    /// 删用户后余额清空、流水保留。
    ///
    /// 流水必须留下：否则无法回答「这个账号一共充了多少、花了多少」，
    /// 而这正是删号之后最可能被问到的问题。
    #[test]
    fn deleting_user_clears_balance_but_keeps_ledger() {
        let db = db();
        let uid = db.create_user("alice", PHC, 1000).unwrap().unwrap();
        db.adjust_balance(uid, 30, TOPUP_KIND, None, None, 2000)
            .unwrap();
        db.delete_user(uid).unwrap();

        let conn = db.conn.lock();
        let balances: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM portal_balances WHERE user_id = ?1",
                params![uid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(balances, 0, "余额行应随用户级联删除");

        let ledger: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM portal_ledger WHERE user_id = ?1",
                params![uid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ledger, 1, "流水必须保留（无外键，不级联）");
    }

    #[test]
    fn create_and_find_user() {
        let db = db();
        let id = db.create_user("alice", PHC, 1000).unwrap().unwrap();
        let u = db.find_user_by_name("alice").unwrap().unwrap();
        assert_eq!(u.id, id);
        assert_eq!(u.password_hash, PHC);
        assert!(!u.disabled);
        assert_eq!(u.last_login_ms, None);
        assert!(db.find_user_by_name("nobody").unwrap().is_none());
    }

    /// 用户名唯一且大小写不敏感——否则 Alice/alice 会是两个号。
    #[test]
    fn username_unique_case_insensitive() {
        let db = db();
        assert!(db.create_user("alice", PHC, 1000).unwrap().is_some());
        assert!(
            db.create_user("ALICE", PHC, 1001).unwrap().is_none(),
            "大小写不同应视为同一用户名"
        );
        assert!(db.find_user_by_name("AlIcE").unwrap().is_some());
    }

    #[test]
    fn session_create_validate_delete() {
        let db = db();
        let uid = db.create_user("alice", PHC, 1000).unwrap().unwrap();
        db.create_session("h1", uid, 1000, 5000, Some("1.2.3.4"))
            .unwrap();

        let s = db.validate_session("h1", 2000).unwrap().unwrap();
        assert_eq!(s.user_id, uid);
        assert_eq!(s.username, "alice");

        assert!(
            db.validate_session("h1", 5000).unwrap().is_none(),
            "到点即失效（expires_at 是排他上界）"
        );
        assert!(db.validate_session("nope", 2000).unwrap().is_none());

        db.delete_session("h1").unwrap();
        assert!(db.validate_session("h1", 2000).unwrap().is_none());
    }

    /// 停用必须立即失效，而不是等 cookie 自然过期。
    #[test]
    fn disabled_user_session_rejected_immediately() {
        let db = db();
        let uid = db.create_user("alice", PHC, 1000).unwrap().unwrap();
        db.create_session("h1", uid, 1000, 999_999, None).unwrap();
        assert!(db.validate_session("h1", 2000).unwrap().is_some());

        db.set_disabled(uid, true).unwrap();
        assert!(
            db.validate_session("h1", 2000).unwrap().is_none(),
            "停用后旧会话仍有效 = 停用形同虚设"
        );

        // 重新启用后不该复活旧会话（停用时已删）
        db.set_disabled(uid, false).unwrap();
        assert!(db.validate_session("h1", 2000).unwrap().is_none());
    }

    /// 改密必须踢掉所有旧会话——这是用户怀疑被盗号时唯一的自救手段。
    #[test]
    fn set_password_revokes_all_sessions() {
        let db = db();
        let uid = db.create_user("alice", PHC, 1000).unwrap().unwrap();
        db.create_session("h1", uid, 1000, 999_999, None).unwrap();
        db.create_session("h2", uid, 1000, 999_999, None).unwrap();

        assert!(db.set_password(uid, "$argon2id$new").unwrap());
        assert!(db.validate_session("h1", 2000).unwrap().is_none());
        assert!(db.validate_session("h2", 2000).unwrap().is_none());
        assert_eq!(
            db.find_user_by_name("alice")
                .unwrap()
                .unwrap()
                .password_hash,
            "$argon2id$new"
        );
    }

    /// 删用户时会话必须随外键级联消失。这条同时验证 `PRAGMA foreign_keys=ON`
    /// 真的生效了——SQLite 默认是**关**的，忘开则级联静默失效、留下孤儿会话行。
    #[test]
    fn delete_user_cascades_sessions() {
        let db = db();
        let uid = db.create_user("alice", PHC, 1000).unwrap().unwrap();
        db.create_session("h1", uid, 1000, 999_999, None).unwrap();

        assert!(db.delete_user(uid).unwrap());
        assert!(db.find_user_by_name("alice").unwrap().is_none());
        assert!(
            db.validate_session("h1", 2000).unwrap().is_none(),
            "外键级联未生效，遗留孤儿会话"
        );
    }

    #[test]
    fn purge_expired_sessions_only_removes_expired() {
        let db = db();
        let uid = db.create_user("alice", PHC, 1000).unwrap().unwrap();
        db.create_session("old", uid, 1000, 2000, None).unwrap();
        db.create_session("live", uid, 1000, 999_999, None).unwrap();

        assert_eq!(db.purge_expired_sessions(5000).unwrap(), 1);
        assert!(db.validate_session("live", 5000).unwrap().is_some());
    }

    #[test]
    fn touch_last_login_recorded() {
        let db = db();
        let uid = db.create_user("alice", PHC, 1000).unwrap().unwrap();
        db.touch_last_login(uid, 4242).unwrap();
        assert_eq!(
            db.find_user_by_name("alice")
                .unwrap()
                .unwrap()
                .last_login_ms,
            Some(4242)
        );
    }

    fn meta(hash: &str, cred: Option<i64>, seen: i64, ok: bool) -> ImportKeyMeta {
        ImportKeyMeta {
            key_hash: hash.to_string(),
            fingerprint: hash.chars().take(8).collect(),
            credential_id: cred,
            region: Some("us-east-1".into()),
            endpoint: Some("ide".into()),
            first_seen_ms: seen,
            last_seen_ms: seen,
            push_count: 1,
            ok,
            error: if ok { None } else { Some("boom".into()) },
        }
    }

    #[test]
    fn import_key_insert_and_list() {
        let db = db();
        db.upsert_import_key(&meta("aaaa1111", Some(7), 1000, true))
            .unwrap();
        db.upsert_import_key(&meta("bbbb2222", Some(8), 2000, true))
            .unwrap();

        let rows = db.list_import_keys(10, 0).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].key_hash, "bbbb2222", "应按 last_seen 倒序");
        assert_eq!(rows[0].credential_id, Some(8));
        assert_eq!(
            rows[0].fingerprint,
            "bbbb2222".chars().take(8).collect::<String>()
        );
        assert_eq!(db.count_import_keys().unwrap(), 2);
    }

    /// 重推同一个 key：first_seen 保留、push_count 累加、last_seen 前进。
    #[test]
    fn import_key_repush_is_idempotent_upsert() {
        let db = db();
        db.upsert_import_key(&meta("aaaa1111", Some(7), 1000, true))
            .unwrap();
        db.upsert_import_key(&meta("aaaa1111", Some(7), 5000, true))
            .unwrap();

        assert_eq!(db.count_import_keys().unwrap(), 1, "重推不该产生第二行");
        let r = &db.list_import_keys(10, 0).unwrap()[0];
        assert_eq!(r.first_seen_ms, 1000, "first_seen 必须保留首次值");
        assert_eq!(r.last_seen_ms, 5000);
        assert_eq!(r.push_count, 2);
    }

    /// **最关键的一条**：一次失败推送带的 credential_id 是 NULL，绝不能覆盖掉
    /// 此前成功落库的 id——覆盖了就再也回查不到明文，用户侧表现为「凭据凭空消失」。
    #[test]
    fn failed_repush_must_not_erase_credential_id() {
        let db = db();
        db.upsert_import_key(&meta("aaaa1111", Some(7), 1000, true))
            .unwrap();

        // 失败推送：credential_id / region / endpoint 全为 None
        let mut failed = meta("aaaa1111", None, 2000, false);
        failed.region = None;
        failed.endpoint = None;
        db.upsert_import_key(&failed).unwrap();

        let r = &db.list_import_keys(10, 0).unwrap()[0];
        assert_eq!(
            r.credential_id,
            Some(7),
            "失败推送抹掉了可用的 credential_id"
        );
        assert_eq!(
            r.region.as_deref(),
            Some("us-east-1"),
            "region 被 NULL 覆盖"
        );
        assert_eq!(r.endpoint.as_deref(), Some("ide"), "endpoint 被 NULL 覆盖");
        // ok/error 的语义是「最后一次推送如何」，应反映失败
        assert!(!r.ok);
        assert_eq!(r.error.as_deref(), Some("boom"));
    }

    /// 号被删后重推会带来新的非空 id，COALESCE 必须取新值而非死守旧值。
    #[test]
    fn repush_with_new_credential_id_takes_new_value() {
        let db = db();
        db.upsert_import_key(&meta("aaaa1111", Some(7), 1000, true))
            .unwrap();
        db.upsert_import_key(&meta("aaaa1111", Some(99), 2000, true))
            .unwrap();
        assert_eq!(
            db.list_import_keys(10, 0).unwrap()[0].credential_id,
            Some(99),
            "新的非空 id 应覆盖旧值"
        );
    }

    #[test]
    fn import_key_pagination() {
        let db = db();
        for i in 0..5 {
            db.upsert_import_key(&meta(&format!("hash{i:04}"), Some(i), 1000 + i, true))
                .unwrap();
        }
        let page1 = db.list_import_keys(2, 0).unwrap();
        let page2 = db.list_import_keys(2, 2).unwrap();
        assert_eq!(page1.len(), 2);
        assert_eq!(page2.len(), 2);
        assert_ne!(page1[0].key_hash, page2[0].key_hash, "分页不该重复");
        assert_eq!(db.list_import_keys(10, 10).unwrap().len(), 0, "越界应为空");
    }

    #[test]
    fn audit_append_and_read_desc() {
        let db = db();
        db.add_audit(
            1000,
            Some(1),
            Some("alice"),
            "login_ok",
            Some("1.2.3.4"),
            None,
        )
        .unwrap();
        db.add_audit(
            2000,
            Some(1),
            Some("alice"),
            "reveal",
            Some("1.2.3.4"),
            Some("cred=7"),
        )
        .unwrap();

        let rows = db.recent_audit(10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].action, "reveal", "应按时间倒序");
        assert_eq!(rows[0].detail.as_deref(), Some("cred=7"));
        assert_eq!(rows[1].action, "login_ok");
    }

    /// 登录失败要能在用户不存在时也记上（username 有值、user_id 为 None），
    /// 否则撞库时审计里看不到被尝试的用户名。
    #[test]
    fn audit_records_failure_for_unknown_user() {
        let db = db();
        db.add_audit(
            1000,
            None,
            Some("no-such-user"),
            "login_fail",
            Some("9.9.9.9"),
            None,
        )
        .unwrap();
        let rows = db.recent_audit(10).unwrap();
        assert_eq!(rows[0].username.as_deref(), Some("no-such-user"));
        assert_eq!(rows[0].client_ip.as_deref(), Some("9.9.9.9"));
    }

    /// 审计是高频写表（每次登录 + 每次明文外显），不清理会写满磁盘。
    #[test]
    fn audit_trim_keeps_most_recent() {
        let db = db();
        for i in 0..10 {
            db.add_audit(1000 + i, None, Some("alice"), "login_ok", None, None)
                .unwrap();
        }
        let deleted = db.trim_audit(3).unwrap();
        assert_eq!(deleted, 7);

        let rows = db.recent_audit(100).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].at_ms, 1009, "应保留最新的那批");
        assert_eq!(rows[2].at_ms, 1007);

        // 幂等：再清一次不该删多余的行
        assert_eq!(db.trim_audit(3).unwrap(), 0);
    }

    /// 删用户时审计**必须保留**——审计的用途正是事后追查已被删除的账号做过什么。
    /// portal_audit 故意不加外键，本用例锁住这个决定，防止日后有人「顺手补上」CASCADE。
    #[test]
    fn audit_survives_user_deletion() {
        let db = db();
        let uid = db.create_user("alice", PHC, 1000).unwrap().unwrap();
        db.add_audit(1000, Some(uid), Some("alice"), "login_ok", None, None)
            .unwrap();

        assert!(db.delete_user(uid).unwrap());
        let rows = db.recent_audit(10).unwrap();
        assert_eq!(rows.len(), 1, "审计不该随用户删除而消失");
        assert_eq!(rows[0].username.as_deref(), Some("alice"));
    }

    // ---------------- 审计筛选与分页（G4）----------------

    /// 铺一批可预期的审计数据。
    ///
    /// 时间戳刻意用 1000/2000/… 的整数，让「时间窗筛选」的边界断言能写成确定值；
    /// 其中两条**共用同一个 at_ms**，专门用来验证同毫秒下的翻页稳定性。
    fn seed_audit(db: &PortalDb) {
        // (at_ms, username, action, ip)
        let rows: &[(i64, &str, &str, Option<&str>)] = &[
            (1000, "alice", "login_ok", Some("1.1.1.1")),
            (2000, "alice", "login_fail_bad_password", Some("1.1.1.1")),
            (3000, "bob", "login_ok", Some("2.2.2.2")),
            (4000, "bob", "login_fail_unknown_user", Some("2.2.2.2")),
            (5000, "bob", "login_fail_throttled", Some("2.2.2.2")),
            (6000, "alice", "board_ok", Some("1.1.1.1")),
            // 同毫秒两条：翻页时不得重复也不得漏。
            (7000, "carol", "reveal_keys", None),
            (7000, "carol", "board_fail_full", None),
        ];
        for (at, user, action, ip) in rows {
            db.add_audit(*at, None, Some(user), action, *ip, None)
                .unwrap();
        }
    }

    fn q(limit: i64) -> AuditQuery {
        AuditQuery {
            limit,
            ..Default::default()
        }
    }

    /// 不带条件时按时间倒序给最近的，`total` 是全量。
    #[test]
    fn audit_page_defaults_to_newest_first() {
        let db = db();
        seed_audit(&db);
        let p = db.audit_page(&q(3)).unwrap();

        assert_eq!(p.total, 8, "total 应是满足条件的全部条数，不受 limit 影响");
        assert_eq!(p.rows.len(), 3);
        assert!(p.has_more);
        // 最新的在最前。同毫秒的两条之间按 id 降序，故 board_fail_full（后插入）在前。
        assert_eq!(p.rows[0].action, "board_fail_full");
        assert_eq!(p.rows[1].action, "reveal_keys");
        assert_eq!(p.rows[2].action, "board_ok");
    }

    /// 用户名精确匹配，不做子串。
    ///
    /// 【对照组不可省】只断言「筛 alice 出来的都是 alice」的话，一个恒返回空集的
    /// 实现也能通过。必须同时断言**该出现的出现了**。
    #[test]
    fn audit_filter_by_username_is_exact() {
        let db = db();
        seed_audit(&db);
        // 同前缀的另一个用户：子串实现会把它一起捞出来。
        db.add_audit(8000, None, Some("alice2"), "login_ok", None, None)
            .unwrap();

        let p = db
            .audit_page(&AuditQuery {
                username: Some("alice".to_string()),
                ..q(100)
            })
            .unwrap();

        assert_eq!(
            p.total, 3,
            "alice 有 3 条（login_ok/bad_password/board_ok）"
        );
        assert!(
            p.rows
                .iter()
                .all(|r| r.username.as_deref() == Some("alice")),
            "筛出了别人的记录：{:?}",
            p.rows.iter().map(|r| &r.username).collect::<Vec<_>>()
        );
        assert!(
            !p.rows
                .iter()
                .any(|r| r.username.as_deref() == Some("alice2")),
            "alice2 被当成 alice 捞出来了——用户名筛选退化成了子串匹配"
        );
    }

    /// 动作精确匹配只出这一种动作，且该出现的都在。
    #[test]
    fn audit_filter_by_action_is_exact() {
        let db = db();
        seed_audit(&db);
        let p = db
            .audit_page(&AuditQuery {
                action: Some("login_ok".to_string()),
                ..q(100)
            })
            .unwrap();

        assert_eq!(p.total, 2, "login_ok 恰好 2 条");
        assert!(p.rows.iter().all(|r| r.action == "login_ok"));
        // 对照组：精确匹配不得把前缀相同的 login_fail_* 带进来。
        assert!(
            !p.rows.iter().any(|r| r.action.starts_with("login_fail")),
            "精确匹配退化成了前缀匹配"
        );
    }

    /// 前缀匹配把一族动作一起筛出来。
    #[test]
    fn audit_filter_by_action_prefix_catches_the_whole_family() {
        let db = db();
        seed_audit(&db);
        let p = db
            .audit_page(&AuditQuery {
                action_prefix: Some("login_fail".to_string()),
                ..q(100)
            })
            .unwrap();

        assert_eq!(p.total, 3, "三种登录失败原因各一条");
        let mut got: Vec<&str> = p.rows.iter().map(|r| r.action.as_str()).collect();
        got.sort_unstable();
        assert_eq!(
            got,
            [
                "login_fail_bad_password",
                "login_fail_throttled",
                "login_fail_unknown_user"
            ]
        );
        // 对照组：成功的登录不能被前缀 login_fail 捞进来。
        assert!(!p.rows.iter().any(|r| r.action == "login_ok"));
    }

    /// **LIKE 的元字符必须被转义。**
    ///
    /// 前缀 `login_` 里的 `_` 在 LIKE 里是「任意一个字符」。不转义时 `loginXfail`
    /// 也会被匹配——眼下没有这种动作名，所以这条测试自己造一个。不造的话这个 bug
    /// 在真实数据上永远不显形，直到某天有人加了个带下划线的相似动作名。
    #[test]
    fn audit_action_prefix_escapes_like_wildcards() {
        let db = db();
        db.add_audit(1000, None, Some("a"), "login_ok", None, None)
            .unwrap();
        // 与 `login_` 只差第 6 个字符：未转义的 `_` 会把它也算进来。
        db.add_audit(2000, None, Some("a"), "loginXok", None, None)
            .unwrap();

        let p = db
            .audit_page(&AuditQuery {
                action_prefix: Some("login_".to_string()),
                ..q(100)
            })
            .unwrap();

        assert_eq!(
            p.total,
            1,
            "`_` 被当成通配符了，loginXok 被误纳入：{:?}",
            p.rows.iter().map(|r| &r.action).collect::<Vec<_>>()
        );
        assert_eq!(p.rows[0].action, "login_ok");
    }

    /// `recent_audit(0)` 必须给 0 条。
    ///
    /// 【为何专门测这个 0】这是把 `recent_audit` 转调 `audit_page` 时唯一可能
    /// 静默改掉的语义：`audit_page` 会把 limit 钳到 `[1, MAX]`（因为 limit=0
    /// 对分页界面意味着「一片空白但不报错」），而裸 `LIMIT 0` 是老老实实给 0 条。
    /// 转调之后若不处理，`recent_audit(0)` 会返回 1 条——一个「要 0 条却给了 1 条」
    /// 的接口，调用方拿它做「要不要显示审计区块」的判断时就会显示一个只有一行的区块。
    #[test]
    fn recent_audit_zero_limit_returns_nothing() {
        let db = db();
        seed_audit(&db);
        let rows = db.recent_audit(0).unwrap();
        assert!(
            rows.is_empty(),
            "recent_audit(0) 给了 {} 条——limit 被钳成 1 了",
            rows.len()
        );
        // 对照组：非 0 的正常值必须照常工作，否则这条测试可能是「恒返回空」的假绿。
        assert_eq!(
            db.recent_audit(2).unwrap().len(),
            2,
            "对照组：要 2 条应给 2 条"
        );
    }

    /// `describe_filter` 的两个分支必须同一形状：都**不带** `filter=` 前缀。
    ///
    /// 【为何专门测这个】实测踩过：空条件那支返回 `"filter=none(all)"`（自带前缀），
    /// 非空那支返回 `"username=alice"`（不带），而调用方自己也拼了一个 `filter=`。
    /// 结果是「没筛任何条件」的那次导出在审计里写成 `filter=filter=none(all)`。
    /// 这种错不影响任何功能、不报错，只是让审计记录看起来像是坏的——而审计的
    /// 全部价值就是事后被人相信。
    #[test]
    fn describe_filter_never_carries_its_own_prefix() {
        let empty = AuditQuery::default().describe_filter();
        assert!(
            !empty.contains("filter="),
            "空条件那支自带了 filter= 前缀，调用方再拼一次就成了 filter=filter=…: {empty}"
        );
        assert_eq!(empty, "none(all)", "空条件应明确说「全部」而不是空字符串");

        let filtered = AuditQuery {
            username: Some("alice".to_string()),
            action_prefix: Some("login_".to_string()),
            ..AuditQuery::default()
        }
        .describe_filter();
        assert!(
            !filtered.contains("filter="),
            "非空那支也不该带前缀: {filtered}"
        );
        // 对照组：用上的条件都要出现，没用上的不能凭空出现。
        assert!(
            filtered.contains("username=alice"),
            "漏了 username: {filtered}"
        );
        assert!(
            filtered.contains("actionPrefix=login_"),
            "漏了前缀: {filtered}"
        );
        assert!(
            !filtered.contains("sinceMs"),
            "没筛时间却写了 sinceMs: {filtered}"
        );
    }

    /// 时间窗是闭区间，边界那两条必须**在**结果里。
    ///
    /// 【为何专门测边界】`>` 与 `>=` 写错一个字符，表现是「按今天筛，今天零点那条
    /// 不见了」——数量只差一条，肉眼对不出来，而审计恰恰是要拿它当证据的。
    #[test]
    fn audit_time_window_is_inclusive_on_both_ends() {
        let db = db();
        seed_audit(&db);
        let p = db
            .audit_page(&AuditQuery {
                since_ms: Some(2000),
                until_ms: Some(4000),
                ..q(100)
            })
            .unwrap();

        assert_eq!(p.total, 3, "2000/3000/4000 三条，两端都含");
        let ats: Vec<i64> = p.rows.iter().map(|r| r.at_ms).collect();
        assert!(ats.contains(&2000), "下界那条被漏掉了（`>` 写成了 `>=`？）");
        assert!(ats.contains(&4000), "上界那条被漏掉了");
        assert!(!ats.contains(&1000));
        assert!(!ats.contains(&5000));
    }

    /// 翻页把每条恰好覆盖一次：不重复、不遗漏。
    ///
    /// 【为何用「收集全部页再比对集合」而不是逐页看长度】长度对得上但内容重复
    /// （同毫秒排序不稳定的典型症状）时，只看长度的测试会通过。
    #[test]
    fn audit_pagination_covers_every_row_exactly_once() {
        let db = db();
        seed_audit(&db);

        let mut seen: Vec<i64> = Vec::new();
        let mut offset = 0;
        loop {
            let p = db.audit_page(&AuditQuery { offset, ..q(3) }).unwrap();
            seen.extend(p.rows.iter().map(|r| r.id));
            if !p.has_more {
                break;
            }
            offset += 3;
            assert!(offset < 100, "翻页没有终止——has_more 恒为真");
        }

        let mut uniq = seen.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(seen.len(), 8, "翻完全部页应恰好 8 条，实得 {}", seen.len());
        assert_eq!(uniq.len(), 8, "有条目在两页里重复出现：{seen:?}");
    }

    /// **同一毫秒的多条记录被翻页切开时，不得重复也不得遗漏。**
    ///
    /// # 为何单独一条（上面那条测不到）
    /// `audit_pagination_covers_every_row_exactly_once` 用 limit=3，而同毫秒的那两条
    /// 恰好都落在第一页里——页边界从没切开它们，于是排序键里有没有 `id` 都一样绿。
    /// 去掉 `ORDER BY` 的 `id DESC` 后那条测试照旧通过，是变异注入才发现的。
    ///
    /// # 为何这件事要紧
    /// 只按 `at_ms` 排序时，同毫秒记录之间的顺序在 SQL 里是**未定义**的：两次查询
    /// （第 1 页和第 2 页是两次查询）可以给出不同的内部顺序，于是同一条在两页都出现、
    /// 另一条谁都看不到。而同毫秒多条的典型场景正是**登录失败风暴**——最需要看全的
    /// 那批数据，恰恰是最容易漏的。
    #[test]
    fn audit_pagination_is_stable_when_a_page_boundary_splits_one_millisecond() {
        let db = db();
        // 六条全在同一毫秒：limit=1 逐页翻，每个页边界都切在同毫秒记录中间。
        for i in 0..6 {
            db.add_audit(
                9000,
                None,
                Some("storm"),
                "login_fail_bad_password",
                Some("9.9.9.9"),
                Some(&format!("n={i}")),
            )
            .unwrap();
        }

        let mut seen: Vec<i64> = Vec::new();
        let mut offset = 0;
        loop {
            let p = db.audit_page(&AuditQuery { offset, ..q(1) }).unwrap();
            seen.extend(p.rows.iter().map(|r| r.id));
            if !p.has_more {
                break;
            }
            offset += 1;
            assert!(offset < 50, "翻页没有终止");
        }

        let mut uniq = seen.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(
            seen.len(),
            6,
            "逐页翻应恰好 6 条，实得 {}：{seen:?}",
            seen.len()
        );
        assert_eq!(uniq.len(), 6, "同毫秒记录在翻页时重复或遗漏了：{seen:?}");
        // 顺序必须是 id 降序：这是「稳定」的可断言形式。
        let mut expect = uniq.clone();
        expect.sort_unstable_by(|a, b| b.cmp(a));
        assert_eq!(seen, expect, "同毫秒记录的翻页顺序不是 id 降序：{seen:?}");
    }

    /// 超界 offset 给空页且 `has_more` 为假——不能报错，也不能绕回第一页。
    #[test]
    fn audit_pagination_out_of_range_is_empty_not_wrapped() {
        let db = db();
        seed_audit(&db);
        let p = db
            .audit_page(&AuditQuery {
                offset: 999,
                ..q(10)
            })
            .unwrap();

        assert!(p.rows.is_empty(), "超界应给空页，实得 {} 条", p.rows.len());
        assert_eq!(p.total, 8, "total 与 offset 无关");
        assert!(
            !p.has_more,
            "空页还说后面有更多，会让界面卡在一个点不动的下一页"
        );
    }

    /// 末页的 `has_more` 必须为假。
    ///
    /// 【为何单独一条】`offset + len < total` 里的 `<` 写成 `<=` 时，只有**恰好末页**
    /// 这一种情形会错，其它页全对。而末页正是「还能不能点下一页」唯一有分歧的地方。
    #[test]
    fn audit_has_more_is_false_exactly_on_last_page() {
        let db = db();
        seed_audit(&db);

        let first = db.audit_page(&q(8)).unwrap();
        assert!(!first.has_more, "一页装下全部 8 条时不该说还有更多");

        let p = db.audit_page(&AuditQuery { offset: 6, ..q(2) }).unwrap();
        assert_eq!(p.rows.len(), 2);
        assert!(!p.has_more, "末页（6+2==8）说还有更多");

        let mid = db.audit_page(&AuditQuery { offset: 5, ..q(2) }).unwrap();
        assert!(mid.has_more, "对照组：非末页应为真");
    }

    /// 筛选与分页**共用同一份条件**：total 必须是「筛过之后」的总数。
    ///
    /// 【这条挡的是什么】count 与 rows 各写一份 WHERE、只改了一处的经典后果：
    /// 筛出 3 条却显示「共 8 条」，界面据此画出 3 个页码，后两页全空。
    #[test]
    fn audit_total_respects_filters_not_just_rows() {
        let db = db();
        seed_audit(&db);
        let p = db
            .audit_page(&AuditQuery {
                username: Some("bob".to_string()),
                limit: 1,
                ..Default::default()
            })
            .unwrap();

        assert_eq!(p.rows.len(), 1, "limit=1 应只给一条");
        assert_eq!(p.total, 3, "total 应是 bob 的 3 条，而不是全表 8 条");
        assert!(p.has_more);
    }

    /// 多个条件同时生效（AND 语义）。
    #[test]
    fn audit_filters_combine_with_and() {
        let db = db();
        seed_audit(&db);
        let p = db
            .audit_page(&AuditQuery {
                username: Some("bob".to_string()),
                action_prefix: Some("login_fail".to_string()),
                since_ms: Some(4000),
                until_ms: Some(4000),
                ..q(100)
            })
            .unwrap();

        assert_eq!(p.total, 1, "四个条件交出来应恰好一条");
        assert_eq!(p.rows[0].action, "login_fail_unknown_user");
        assert_eq!(p.rows[0].at_ms, 4000);
    }

    /// limit 被钳进合法区间：0 或负数不能变成「取全表」或让 SQL 报错。
    #[test]
    fn audit_limit_is_clamped() {
        let db = db();
        seed_audit(&db);

        let zero = db.audit_page(&q(0)).unwrap();
        assert_eq!(zero.limit, 1, "limit=0 应被钳到 1，而不是当成无限制");
        assert_eq!(zero.rows.len(), 1);

        let neg = db.audit_page(&q(-5)).unwrap();
        assert_eq!(
            neg.limit, 1,
            "负 limit 应被钳到 1（SQLite 里 LIMIT -1 是无限制）"
        );

        let huge = db.audit_page(&q(999_999)).unwrap();
        assert_eq!(huge.limit, AUDIT_EXPORT_MAX, "超大 limit 应被钳到导出上限");
        assert_eq!(huge.rows.len(), 8, "钳过之后仍要正常返回全部数据");
    }

    /// 负 offset 当 0 处理。
    #[test]
    fn audit_negative_offset_is_treated_as_zero() {
        let db = db();
        seed_audit(&db);
        let p = db.audit_page(&AuditQuery { offset: -3, ..q(2) }).unwrap();
        assert_eq!(p.offset, 0, "负 offset 会让 SQLite 的 OFFSET 行为不可预期");
        assert_eq!(p.rows.len(), 2);
    }

    /// 空库不崩，给零。
    #[test]
    fn audit_page_on_empty_db_is_empty() {
        let db = db();
        let p = db.audit_page(&q(10)).unwrap();
        assert_eq!(p.total, 0);
        assert!(p.rows.is_empty());
        assert!(!p.has_more);
        assert!(db.audit_actions().unwrap().is_empty());
    }

    /// 动作下拉按次数降序，且只列**实际发生过**的动作。
    #[test]
    fn audit_actions_are_counted_and_sorted_by_frequency() {
        let db = db();
        seed_audit(&db);
        let acts = db.audit_actions().unwrap();

        assert_eq!(acts[0].action, "login_ok");
        assert_eq!(acts[0].count, 2, "login_ok 出现 2 次，应排第一");
        // 其余都是 1 次，按 action 名升序（稳定输出，避免界面下拉每次刷新都换序）。
        let ones: Vec<&str> = acts[1..].iter().map(|a| a.action.as_str()).collect();
        let mut sorted = ones.clone();
        sorted.sort_unstable();
        assert_eq!(ones, sorted, "同次数的动作应按名字升序，输出才稳定");

        assert_eq!(
            acts.iter().map(|a| a.count).sum::<i64>(),
            8,
            "各动作次数之和应等于总条数"
        );
        // 对照组：没发生过的动作不该出现。
        assert!(
            !acts.iter().any(|a| a.action == "admin_delete_user"),
            "列出了从未发生过的动作——说明这份清单来自代码常量而非实际数据"
        );
    }

    /// 用户名筛选走 SQL 绑定值，注入串只会查无此人。
    ///
    /// 【为何要有这一条】审计查询与余额、会话在同一个连接上。一次注入足够改余额
    /// 或读走会话哈希——这不是「只读接口所以无所谓」的场景。
    #[test]
    fn audit_filters_are_injection_safe() {
        let db = db();
        seed_audit(&db);
        let before = db.audit_page(&q(100)).unwrap().total;

        for evil in [
            "alice' OR '1'='1",
            "'; DELETE FROM portal_audit; --",
            "alice'--",
        ] {
            let p = db
                .audit_page(&AuditQuery {
                    username: Some(evil.to_string()),
                    ..q(100)
                })
                .unwrap();
            assert_eq!(p.total, 0, "注入串 {evil:?} 匹配到了数据");
        }

        assert_eq!(
            db.audit_page(&q(100)).unwrap().total,
            before,
            "注入串把审计数据删掉了"
        );
    }

    // ---------------- 上车 ----------------

    /// 建号并充值，返回 user_id。上车测试几乎都要这两步。
    fn rich(db: &PortalDb, name: &str, credits: i64) -> i64 {
        let uid = db.create_user(name, PHC, 1000).unwrap().unwrap();
        db.adjust_balance(uid, credits, TOPUP_KIND, None, None, 1000)
            .unwrap()
            .unwrap();
        uid
    }

    /// 默认参数：前 2 人各 10 分，之后 ceil(20/N)，最多 10 人。
    fn pr() -> Pricing {
        Pricing::default()
    }

    #[test]
    fn board_first_user_pays_base_price() {
        let db = db();
        let uid = rich(&db, "alice", 100);

        let out = db.board(uid, 7, pr(), 2000).unwrap();
        match out {
            BoardOutcome::Aboard {
                price,
                balance,
                count,
                refunded,
            } => {
                assert_eq!(price, 10, "第 1 人按 base_price");
                assert_eq!(balance, 90);
                assert_eq!(count, 1);
                assert_eq!(refunded, 0, "车上没别人，无退款");
            }
            other => panic!("应当上车成功，实际 {other:?}"),
        }

        assert!(db.is_aboard(uid, 7).unwrap());
        assert_eq!(db.unlocker_count(7).unwrap(), 1);
    }

    /// 重复上车必须幂等：不扣分、不写流水、不改人数。
    ///
    /// 【为何是硬要求】刷新页面、双击按钮、客户端重试都会重放这个请求。若第二次
    /// 也扣费，用户会因为网络抖动被重复收费——而且他自己无法察觉，只看到余额少了。
    #[test]
    fn board_twice_is_idempotent() {
        let db = db();
        let uid = rich(&db, "alice", 100);

        db.board(uid, 7, pr(), 2000).unwrap();
        let ledger_before = db.ledger_of(uid, 100).unwrap().len();

        let out = db.board(uid, 7, pr(), 3000).unwrap();
        match out {
            BoardOutcome::AlreadyAboard {
                paid,
                balance,
                count,
            } => {
                assert_eq!(paid, 10);
                assert_eq!(balance, 90, "第二次不得再扣分");
                assert_eq!(count, 1);
            }
            other => panic!("应当返回 AlreadyAboard，实际 {other:?}"),
        }

        assert_eq!(
            db.ledger_of(uid, 100).unwrap().len(),
            ledger_before,
            "幂等重放不得留下流水"
        );
        assert_eq!(db.unlocker_count(7).unwrap(), 1, "人数不得涨");
    }

    /// 第 3 人上车触发退款：前两人各退到 ceil(20/3)=7。
    ///
    /// 这是差额模型的核心断言。若失败，说明退款额或 `paid` 的同步写错了，
    /// 后果是每个人的净支出不再等于当前单价，「均摊」当场失效。
    #[test]
    fn board_third_user_triggers_refund_to_earlier_two() {
        let db = db();
        let a = rich(&db, "alice", 100);
        let b = rich(&db, "bob", 100);
        let c = rich(&db, "carol", 100);

        db.board(a, 7, pr(), 2000).unwrap();
        db.board(b, 7, pr(), 3000).unwrap();
        // 前两人都按 base_price=10，此时无退款
        assert_eq!(db.balance_of(a).unwrap(), 90);
        assert_eq!(db.balance_of(b).unwrap(), 90);

        let out = db.board(c, 7, pr(), 4000).unwrap();
        match out {
            BoardOutcome::Aboard {
                price,
                count,
                refunded,
                ..
            } => {
                assert_eq!(price, 7, "第 3 人 = ceil(20/3) = 7");
                assert_eq!(count, 3);
                assert_eq!(refunded, 6, "前两人各退 10-7=3，合计 6");
            }
            other => panic!("应当上车成功，实际 {other:?}"),
        }

        // 三人净支出必须相等——这正是「均摊」的定义。
        assert_eq!(db.balance_of(a).unwrap(), 93, "alice 退 3");
        assert_eq!(db.balance_of(b).unwrap(), 93, "bob 退 3");
        assert_eq!(db.balance_of(c).unwrap(), 93, "carol 直接付 7");

        for uid in [a, b, c] {
            let w = db.wallet_of(uid).unwrap();
            assert_eq!(w.spent, 7, "净支出都等于当前单价");
        }
    }

    /// 连续上到 10 人，全程每人净支出恒等于当前单价。
    ///
    /// 【为何要逐步断言而不只看终态】差额模型每一步都要重算退款，某一步少退或
    /// 多退，后面几步可能又「碰巧」抹平。只查最后一步会漏掉中间的错账。
    #[test]
    fn board_up_to_ten_keeps_everyone_equal() {
        let db = db();
        let p = pr();
        let uids: Vec<i64> = (0..10).map(|i| rich(&db, &format!("u{i}"), 1000)).collect();

        for (i, &uid) in uids.iter().enumerate() {
            let n = i as u32 + 1;
            let out = db.board(uid, 42, p, 2000 + i as i64).unwrap();
            let price = match out {
                BoardOutcome::Aboard { price, .. } => price,
                other => panic!("第 {n} 人应上车成功，实际 {other:?}"),
            };
            let expect = p.unit_price(n);
            assert_eq!(price, expect, "第 {n} 人单价");

            // 当前车上每一位的净支出都必须是同一个数。
            for &done in &uids[..=i] {
                let w = db.wallet_of(done).unwrap();
                assert_eq!(w.spent, expect, "{n} 人时，已上车者净支出应统一为 {expect}");
            }
        }

        assert_eq!(db.unlocker_count(42).unwrap(), 10);
    }

    /// 满员必须拒绝，且不扣分。
    #[test]
    fn board_rejects_when_full() {
        let db = db();
        // max_unlockers=2，方便造满员
        let p = Pricing {
            base_count: 1,
            base_price: 5,
            total_price: 10,
            min_price: 1,
            max_unlockers: 2,
        };
        let a = rich(&db, "alice", 100);
        let b = rich(&db, "bob", 100);
        let c = rich(&db, "carol", 100);

        db.board(a, 7, p, 2000).unwrap();
        db.board(b, 7, p, 3000).unwrap();

        let out = db.board(c, 7, p, 4000).unwrap();
        match out {
            BoardOutcome::Full { count, max } => {
                assert_eq!(count, 2);
                assert_eq!(max, 2);
            }
            other => panic!("应当满员拒绝，实际 {other:?}"),
        }

        assert_eq!(db.balance_of(c).unwrap(), 100, "满员被拒不得扣分");
        assert!(!db.is_aboard(c, 7).unwrap(), "被拒者不得留下上车记录");
        assert_eq!(db.ledger_of(c, 10).unwrap().len(), 1, "只有那笔充值流水");
        assert_eq!(db.unlocker_count(7).unwrap(), 2, "人数不得因失败而变");
    }

    /// 余额不足必须整体回滚：不扣分、不上车、不给别人退款。
    ///
    /// 【最容易写错的地方】若把退款放在余额检查之前，一次失败的上车会白送
    /// 别人一笔退款——车上人数没变，钱却少了，总账再也对不平。
    #[test]
    fn board_insufficient_balance_changes_nothing() {
        let db = db();
        let a = rich(&db, "alice", 100);
        let poor = rich(&db, "bob", 3);

        db.board(a, 7, pr(), 2000).unwrap();
        let a_before = db.balance_of(a).unwrap();

        let out = db.board(poor, 7, pr(), 3000).unwrap();
        match out {
            BoardOutcome::NotEnough { needed, balance } => {
                assert_eq!(needed, 10, "第 2 人价 = base_price");
                assert_eq!(balance, 3);
            }
            other => panic!("应当余额不足，实际 {other:?}"),
        }

        assert_eq!(db.balance_of(poor).unwrap(), 3, "失败不得扣分");
        assert!(!db.is_aboard(poor, 7).unwrap());
        assert_eq!(db.unlocker_count(7).unwrap(), 1);
        assert_eq!(
            db.balance_of(a).unwrap(),
            a_before,
            "失败的上车不得给车上的人退款"
        );
    }

    /// 首次上车冻结价格快照，之后改配置不影响这把 key。
    ///
    /// 【为何必须冻结】不冻结的话，管理员调价会让已在车上的人的「应付额」跳变，
    /// 只剩两个都不可接受的选择：追扣老乘客，或让同一把 key 上并存两种价格。
    #[test]
    fn board_freezes_pricing_on_first_boarding() {
        let db = db();
        let a = rich(&db, "alice", 100);
        let b = rich(&db, "bob", 100);

        db.board(a, 7, pr(), 2000).unwrap();

        let snap = db.pricing_of(7).unwrap().expect("首次上车应写入快照");
        assert_eq!(snap, pr().sanitized(), "快照 = 当时的配置");

        // 管理员把价格翻倍后，第 2 人仍按快照价（10）而不是新价（20）。
        let changed = Pricing {
            base_count: 2,
            base_price: 20,
            total_price: 40,
            min_price: 1,
            max_unlockers: 10,
        };
        let out = db.board(b, 7, changed, 3000).unwrap();
        match out {
            BoardOutcome::Aboard { price, .. } => {
                assert_eq!(price, 10, "老 key 按快照价，不受改价影响");
            }
            other => panic!("应当上车成功，实际 {other:?}"),
        }

        // 而一把新 key 用的是新配置。
        db.board(a, 99, changed, 4000).unwrap();
        let snap99 = db.pricing_of(99).unwrap().unwrap();
        assert_eq!(snap99.base_price, 20, "新 key 按新配置冻结");
    }

    /// 不同 key 之间互不影响：人数、价格、上车记录都按 key 独立。
    #[test]
    fn board_is_per_credential() {
        let db = db();
        let a = rich(&db, "alice", 100);

        db.board(a, 7, pr(), 2000).unwrap();
        db.board(a, 8, pr(), 3000).unwrap();

        assert_eq!(db.unlocker_count(7).unwrap(), 1);
        assert_eq!(db.unlocker_count(8).unwrap(), 1);
        assert_eq!(db.balance_of(a).unwrap(), 80, "两把 key 各扣 10");

        // aboard_map 一次回答三件事：上了哪些车、每单付了多少、什么时候上的。
        // 时间戳按 key 分别记录（这里 7 号是 2000、8 号是 3000），不是同一个值——
        // 若实现里错用了「最近一次上车时间」，两把 key 会读到同一个数，本断言会抓住。
        let mine = db.aboard_map(a).unwrap();
        assert_eq!(mine.get(&7), Some(&(10, 2000)));
        assert_eq!(mine.get(&8), Some(&(10, 3000)));
        assert_eq!(mine.get(&9), None);

        let counts = db.unlocker_counts().unwrap();
        assert_eq!(counts.get(&7), Some(&1));
        assert_eq!(counts.get(&8), Some(&1));
        assert_eq!(counts.get(&9), None, "没人上车的 key 不出现在统计里");
    }

    /// 上车与退款都要写流水，且带 credential_id。
    ///
    /// 【为何 credential_id 不能省】用户看到「-7 分」时必须能知道是哪把 key。
    /// 没有这一列，客诉只能靠时间戳猜，而同一秒内可能有多笔。
    #[test]
    fn board_writes_traceable_ledger() {
        let db = db();
        let a = rich(&db, "alice", 100);
        let b = rich(&db, "bob", 100);
        let c = rich(&db, "carol", 100);

        db.board(a, 7, pr(), 2000).unwrap();
        db.board(b, 7, pr(), 3000).unwrap();
        db.board(c, 7, pr(), 4000).unwrap();

        // alice：充值 + 上车 + 退款
        let rows = db.ledger_of(a, 100).unwrap();
        let kinds: Vec<&str> = rows.iter().map(|r| r.kind.as_str()).collect();
        assert!(kinds.contains(&UNLOCK_KIND), "应有上车流水");
        assert!(kinds.contains(&REFUND_KIND), "应有退款流水");

        for r in &rows {
            if r.kind == UNLOCK_KIND || r.kind == REFUND_KIND {
                assert_eq!(r.credential_id, Some(7), "上车/退款流水必须带 key id");
            }
        }

        let unlock = rows.iter().find(|r| r.kind == UNLOCK_KIND).unwrap();
        assert_eq!(unlock.delta, -10);
        let refund = rows.iter().find(|r| r.kind == REFUND_KIND).unwrap();
        assert_eq!(refund.delta, 3, "10 → 7 退 3");
    }

    /// 删用户后上车记录随之消失，其余乘客不受影响。
    ///
    /// 【注意这里不退款】删号是管理操作，不触发重新计价。若要求删号后自动给
    /// 剩下的人涨价，那就得追扣，而追扣可能把余额扣成负数——这条边界留在
    /// 「删号只清记录」这一侧，是有意的取舍。
    #[test]
    fn deleting_user_removes_their_boarding() {
        let db = db();
        let a = rich(&db, "alice", 100);
        let b = rich(&db, "bob", 100);

        db.board(a, 7, pr(), 2000).unwrap();
        db.board(b, 7, pr(), 3000).unwrap();
        assert_eq!(db.unlocker_count(7).unwrap(), 2);

        db.delete_user(a).unwrap();
        assert_eq!(db.unlocker_count(7).unwrap(), 1, "删号清掉其上车记录");
        assert!(db.is_aboard(b, 7).unwrap(), "其余乘客不受影响");
        assert_eq!(db.balance_of(b).unwrap(), 90, "不追扣、不退款");
    }

    // ---------------- 并发 ----------------
    //
    // 【这些用例实际在验什么】`board` 的串行化有两层：外层是 `PortalDb` 的
    // `parking_lot::Mutex`（单连接，先到先得），内层是 `BEGIN IMMEDIATE`。当前
    // 实现下 Mutex 已经保证了互斥，IMMEDIATE 是为「日后换成连接池 / 多连接」
    // 留的第二道防线——那时 Mutex 不再管用，事务隔离级别就是唯一防线。
    //
    // 所以这些用例测的是**端到端不变量**（人数不超卖、价格一致、总账自洽），
    // 不区分是哪一层挡住的。这样写的好处：将来有人为了吞吐把单连接换成池，
    // 只要这几条还绿，就说明换法是安全的；一旦红，就是真的漏了。
    //
    // 用 `Barrier` 让线程真的同时冲进 `board`，而不是靠 spawn 顺序碰运气。

    use std::sync::{Arc, Barrier};

    /// 同时抢一把空 key：两人都该上车，各付 base_price（默认 base_count=2）。
    #[test]
    fn board_concurrent_two_users_same_key() {
        let db = Arc::new(db());
        let a = rich(&db, "alice", 100);
        let b = rich(&db, "bob", 100);

        let gate = Arc::new(Barrier::new(2));
        let hs: Vec<_> = [a, b]
            .into_iter()
            .map(|uid| {
                let db = Arc::clone(&db);
                let gate = Arc::clone(&gate);
                std::thread::spawn(move || {
                    gate.wait();
                    db.board(uid, 7, pr(), 2000).unwrap()
                })
            })
            .collect();

        let outs: Vec<_> = hs.into_iter().map(|h| h.join().unwrap()).collect();
        for o in &outs {
            assert!(
                matches!(o, BoardOutcome::Aboard { .. }),
                "两人都该成功上车，实际 {o:?}"
            );
        }

        assert_eq!(db.unlocker_count(7).unwrap(), 2, "不能多也不能少");
        // base_count=2，所以前两人都是 10 分，谁先谁后不影响结果。
        assert_eq!(db.balance_of(a).unwrap(), 90);
        assert_eq!(db.balance_of(b).unwrap(), 90);
    }

    /// 满员后并发抢：都该被拒，且**人数不能被抢超**。
    ///
    /// 这条是超卖的直接检测。若 `board` 的「查人数 → 判满员 → 写记录」不在同一个
    /// 事务里，两个线程会都读到 9 人、都认为没满、都写进去 → 变成 11 人，
    /// 而 max_unlockers 就成了一句空话。
    #[test]
    fn board_concurrent_at_capacity() {
        let db = Arc::new(db());
        let p = pr(); // max_unlockers = 10

        // 先塞满 10 人。
        for i in 0..10 {
            let uid = rich(&db, &format!("u{i}"), 100);
            assert!(matches!(
                db.board(uid, 7, p, 2000 + i).unwrap(),
                BoardOutcome::Aboard { .. }
            ));
        }
        assert_eq!(db.unlocker_count(7).unwrap(), 10);

        let late_a = rich(&db, "late_a", 100);
        let late_b = rich(&db, "late_b", 100);

        let gate = Arc::new(Barrier::new(2));
        let hs: Vec<_> = [late_a, late_b]
            .into_iter()
            .map(|uid| {
                let db = Arc::clone(&db);
                let gate = Arc::clone(&gate);
                std::thread::spawn(move || {
                    gate.wait();
                    db.board(uid, 7, p, 9000).unwrap()
                })
            })
            .collect();

        for o in hs.into_iter().map(|h| h.join().unwrap()) {
            assert!(
                matches!(o, BoardOutcome::Full { .. }),
                "满员必须拒绝，实际 {o:?}"
            );
        }

        assert_eq!(db.unlocker_count(7).unwrap(), 10, "名额被抢超了（超卖）");
        assert_eq!(db.balance_of(late_a).unwrap(), 100, "被拒不该扣分");
        assert_eq!(db.balance_of(late_b).unwrap(), 100);
    }

    /// 同时首次上车：价格快照只能有一份，两人价格必须相同。
    ///
    /// 若快照写入不在事务内，两个线程会各写一次 `portal_key_pricing`（主键冲突
    /// 报错，或后写覆盖先写），于是同一把 key 出现两套参数，「均摊」当场失效。
    #[test]
    fn board_concurrent_first_unlock_snapshot() {
        let db = Arc::new(db());
        let a = rich(&db, "alice", 100);
        let b = rich(&db, "bob", 100);

        let gate = Arc::new(Barrier::new(2));
        let hs: Vec<_> = [a, b]
            .into_iter()
            .map(|uid| {
                let db = Arc::clone(&db);
                let gate = Arc::clone(&gate);
                std::thread::spawn(move || {
                    gate.wait();
                    db.board(uid, 7, pr(), 2000).unwrap()
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }

        let conn = db.conn.lock();
        let rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM portal_key_pricing WHERE credential_id = 7",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 1, "价格快照必须唯一");

        let distinct_paid: i64 = conn
            .query_row(
                "SELECT COUNT(DISTINCT paid) FROM portal_unlocks WHERE credential_id = 7",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(distinct_paid, 1, "同一把 key 上所有人的已付必须相等");
    }

    /// 10 人同时抢同一把 key：最终所有人已付相同，且总账自洽。
    ///
    /// 这是最强的一条：并发下退款要发生 8 轮（第 3 到第 10 个人各触发一次，
    /// 每次退给之前所有人），任何一次算错或漏算都会让「已付」分叉。
    /// 顺序执行时 `board_up_to_ten_keeps_everyone_equal` 已覆盖同样的不变量，
    /// 这里把它放到真并发下再验一次。
    #[test]
    fn board_concurrent_ten_users_stay_equal() {
        let db = Arc::new(db());
        let uids: Vec<i64> = (0..10).map(|i| rich(&db, &format!("u{i}"), 100)).collect();

        let gate = Arc::new(Barrier::new(uids.len()));
        let hs: Vec<_> = uids
            .iter()
            .copied()
            .map(|uid| {
                let db = Arc::clone(&db);
                let gate = Arc::clone(&gate);
                std::thread::spawn(move || {
                    gate.wait();
                    db.board(uid, 42, pr(), 5000).unwrap()
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }

        assert_eq!(db.unlocker_count(42).unwrap(), 10);

        // 10 人时单价 = ceil(20/10) = 2，且 min(base_price=10) 不生效。
        let want = pr().unit_price(10);
        assert_eq!(want, 2, "参数变了就该更新这个期望值，别默默跟着改");

        for uid in &uids {
            let w = db.wallet_of(*uid).unwrap();
            assert_eq!(
                w.balance,
                100 - want,
                "每人净支出必须等于当前单价（user {uid}）"
            );
            assert_eq!(w.spent, want, "spent 要反映净支出而非累计扣款");
            assert_eq!(w.topup, 100, "退款不得算进充值");
        }
    }

    // ---------------- 运营看板 ----------------

    /// 看板的默认取数参数（测试里统一用，免得每处各写一遍魔法数）。
    const TOP: usize = 5;

    /// **空库不能崩，且必须显示 0 而不是报错。**
    ///
    /// 【为何这条排在最前】刚部署完、还没有任何用户的那一刻，正是看板第一次被打开
    /// 的时刻。而聚合 SQL 在空表上的坑很具体：`SUM` 返回 NULL 而非 0，取成 `i64`
    /// 会直接是类型错误。若只在有数据的库上测，这个分支永远走不到。
    #[test]
    fn dashboard_on_empty_db_is_all_zeros_and_reports_ok() {
        let db = db();
        let d = db.dashboard(0, 0, TOP).unwrap();

        assert_eq!(d.today, DashboardWindow::default());
        assert_eq!(d.total, DashboardWindow::default());
        assert_eq!(d.tiers, UserTiers::default());
        assert!(d.keys.is_empty());
        assert!(d.login_fails.is_empty());
        assert!(d.integrity.ok, "空库是自洽的，不该报账目异常");
        assert_eq!(d.integrity.balance_sum, 0);
        assert_eq!(d.integrity.ledger_sum, 0);
    }

    /// 每个聚合数字都用**独立 SQL** 复算比对。
    ///
    /// 【为何要独立复算而不是写死期望值】写死期望值证明的是「这段代码今天输出了
    /// 这些数」，而它们是不是**这些表里真实存在的数**没人验证——聚合 SQL 漏个
    /// WHERE、把 SUM 写成 COUNT，只要我把期望值也跟着改成新输出，测试照旧绿。
    /// 用另一条路径把同一个事实算第二遍，两条路径同时错到一处的概率低得多。
    #[test]
    fn dashboard_totals_match_independently_computed_sql() {
        let db = db();
        let a = rich(&db, "alice", 100);
        let b = rich(&db, "bob", 100);
        // 第三个人是**必需的**，不是凑数：前 2 人都按 base_price 付，价格不变则
        // 无差额可退，`refund` 恒为 0，下面那条「四项都非零」的护栏就会红。
        // 第 3 人把单价压到 ceil(20/3)=7，前两人各退 3 分。
        let c = rich(&db, "carol", 100);
        db.board(a, 7, pr(), 5000).unwrap();
        db.board(b, 7, pr(), 6000).unwrap();
        db.board(c, 7, pr(), 6500).unwrap();
        db.board(a, 8, pr(), 7000).unwrap();
        db.adjust_balance(a, -5, ADMIN_ADJUST_KIND, None, None, 8000)
            .unwrap()
            .unwrap();

        let d = db.dashboard(0, 0, TOP).unwrap();
        let conn = db.conn.lock();

        let tickets: i64 = conn
            .query_row("SELECT COUNT(*) FROM portal_unlocks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(d.total.tickets, tickets, "发票数必须等于 unlocks 行数");

        let want = |kind: &str| -> i64 {
            conn.query_row(
                "SELECT COALESCE(SUM(delta), 0) FROM portal_ledger WHERE kind = ?1",
                params![kind],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(d.total.topup, want(TOPUP_KIND));
        assert_eq!(d.total.spend, -want(UNLOCK_KIND), "消费存正数");
        assert_eq!(d.total.refund, want(REFUND_KIND));
        assert_eq!(d.total.adjust, want(ADMIN_ADJUST_KIND), "调账保留符号");

        // 这几个数字必须真的非零，否则上面全是 0 == 0 的空断言。
        assert!(d.total.topup > 0 && d.total.spend > 0 && d.total.refund > 0);
        assert!(d.total.adjust < 0, "扣分调账应为负");
    }

    /// 「今日」窗口必须真的过滤时间，且带**对照组**证明过滤器本身没坏。
    ///
    /// 【为何一定要有对照组】只断言「窗外的不出现」的话，一个恒返回 0 的坏聚合
    /// 会让测试全绿——它确实没把窗外的算进来，因为它什么都没算进来。所以同一份
    /// 数据必须同时断言「窗内的出现了」，两个方向一起才能定位到过滤边界。
    #[test]
    fn dashboard_today_window_excludes_older_and_includes_newer() {
        let db = db();
        let a = rich(&db, "alice", 100);
        let b = rich(&db, "bob", 100);

        // 窗外（旧）：alice 在 1000 上车 #7。
        db.board(a, 7, pr(), 1_000).unwrap();
        // 窗内（新）：bob 在 9000 上车 #8。
        db.board(b, 8, pr(), 9_000).unwrap();

        let d = db.dashboard(5_000, 0, TOP).unwrap();

        assert_eq!(d.today.tickets, 1, "窗内只有 bob 那一张票");
        assert_eq!(d.total.tickets, 2, "对照组：累计必须两张，否则是聚合坏了");
        assert!(
            d.today.spend < d.total.spend,
            "窗内消费必须严格小于累计（今日 {} 累计 {}）",
            d.today.spend,
            d.total.spend
        );
        // 充值全发生在 1000（rich 里），因此窗内不该有充值——而累计有。
        assert_eq!(d.today.topup, 0);
        assert_eq!(d.total.topup, 200);
    }

    /// 车辆热度：排序、乘客数、收入、单价都要对，且单价与 `board` 同源。
    #[test]
    fn dashboard_key_heat_ranks_by_passengers_and_matches_pricing() {
        let db = db();
        let a = rich(&db, "alice", 100);
        let b = rich(&db, "bob", 100);
        let c = rich(&db, "carol", 100);

        // **热的那辆车故意用更大的 credential_id。**
        //
        // 【为何这个细节是刻意的】GROUP BY 的隐式输出顺序就是按分组键升序。若让
        // #7 既是最热又是最小 id，那么「按乘客数排序」和「压根没排序」会给出
        // 完全相同的结果——删掉 ORDER BY 子句测试照旧绿。实测正是如此：这条
        // 变异第一次注入时活了下来。把热车放到更大的 id 上，两种顺序才可区分。
        db.board(a, 8, pr(), 1000).unwrap();
        db.board(b, 8, pr(), 2000).unwrap();
        db.board(c, 8, pr(), 3000).unwrap();
        db.board(a, 7, pr(), 4000).unwrap();

        let d = db.dashboard(0, 0, TOP).unwrap();
        assert_eq!(d.keys.len(), 2);

        let hot = &d.keys[0];
        assert_eq!(
            hot.credential_id, 8,
            "乘客多的车必须排第一（#8 三人 vs #7 一人），拿到 {} 说明排序没生效",
            hot.credential_id
        );
        assert_eq!(hot.passengers, 3);
        assert_eq!(hot.first_boarded_ms, 1000);
        assert_eq!(hot.last_boarded_ms, 3000);

        // 单价用同一个公式复算：3 人时 ceil(20/3) = 7。
        assert_eq!(hot.unit_price, Some(pr().unit_price(3)));
        assert_eq!(hot.unit_price, Some(7), "参数变了就更新这里，别默默跟着走");

        // 收入 = SUM(paid)。差额退款后三人各付 7 分。
        assert_eq!(hot.revenue, 21, "在册收入 = 每人当前 paid 之和");

        assert_eq!(d.keys[1].credential_id, 7);
        assert_eq!(d.keys[1].passengers, 1);
    }

    /// 乘客数相同时，**最近有人上车的排前面**（同数按 `MAX(unlocked_at_ms)` 降序）。
    ///
    /// 【为何单独一条】上面那条只能证明「按乘客数降序」；同数时的次级排序是另一段
    /// 逻辑，删掉它上面那条不会红。而这个次级键有实际用途：一堆各有 1 人的车里，
    /// 运营想先看到刚刚有人上的那辆。这里同样把「最近的」放在**更小**的 id 上，
    /// 让它与隐式 id 升序不可混淆。
    #[test]
    fn dashboard_key_heat_breaks_ties_by_recency() {
        let db = db();
        let a = rich(&db, "alice", 100);

        // #5 上车更晚（9000），#6 更早（1000）。各 1 人。
        db.board(a, 6, pr(), 1000).unwrap();
        db.board(a, 5, pr(), 9000).unwrap();

        let keys = db.dashboard(0, 0, TOP).unwrap().keys;
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].passengers, keys[1].passengers, "前置条件：人数相同");
        assert_eq!(
            keys[0].credential_id, 5,
            "同人数时最近上车的该排前面，拿到 {} 说明次级排序没生效",
            keys[0].credential_id
        );
    }

    /// `top` 参数必须真的限制条数（否则大库上一次拉出全表）。
    #[test]
    fn dashboard_key_heat_respects_top_limit() {
        let db = db();
        let a = rich(&db, "alice", 100);
        for cid in 1..=4 {
            db.board(a, cid, pr(), 1000 + cid).unwrap();
        }
        assert_eq!(db.dashboard(0, 0, 4).unwrap().keys.len(), 4, "对照组");
        assert_eq!(db.dashboard(0, 0, 2).unwrap().keys.len(), 2);
    }

    /// 用户分层三档**互斥且穷尽**：`active + broke + zombie == total`。
    ///
    /// 这条恒等式是分层唯一的正确性判据。任何一档的边界写错，都会让某个用户
    /// 落进两档（和 > 总数）或落不进任何一档（和 < 总数）。
    #[test]
    fn dashboard_user_tiers_are_mutually_exclusive_and_exhaustive() {
        let db = db();

        // active：上过车且还有余额。
        let active = rich(&db, "active_one", 100);
        db.board(active, 7, pr(), 1000).unwrap();

        // broke：上过车且余额恰好花光。首人价 10 分，就充 10 分。
        let broke = rich(&db, "broke_one", 10);
        db.board(broke, 8, pr(), 1000).unwrap();
        assert_eq!(db.balance_of(broke).unwrap(), 0, "前置条件：余额确实为 0");

        // zombie：注册了从未上车。
        db.create_user("zombie_one", PHC, 1000).unwrap().unwrap();
        // zombie 且有余额的也必须算 zombie（充过值但没上车）。
        rich(&db, "zombie_rich", 50);

        // disabled 与三档交叉：这个号是 active 且被停用。
        let off = rich(&db, "off_one", 100);
        db.board(off, 9, pr(), 1000).unwrap();
        db.set_disabled(off, true).unwrap();

        let t = db.dashboard(0, 0, TOP).unwrap().tiers;

        assert_eq!(t.total, 5);
        assert_eq!(
            t.active + t.broke + t.zombie,
            t.total,
            "三档必须互斥且穷尽：active={} broke={} zombie={} total={}",
            t.active,
            t.broke,
            t.zombie,
            t.total
        );
        assert_eq!(t.active, 2, "active_one + off_one（停用不改变分层）");
        assert_eq!(t.broke, 1);
        assert_eq!(t.zombie, 2);
        assert_eq!(t.disabled, 1, "disabled 是交叉维度，不参与上面那条恒等式");
    }

    /// 登录失败统计：按 IP 聚合、按次数降序、只算窗内、只算失败动作。
    #[test]
    fn dashboard_login_fails_group_by_ip_and_exclude_successes() {
        let db = db();
        // 窗内（>= 5000）：1.1.1.1 失败 3 次，2.2.2.2 失败 1 次。
        for at in [5000, 6000, 7000] {
            db.add_audit(
                at,
                None,
                Some("x"),
                "login_fail_bad_password",
                Some("1.1.1.1"),
                None,
            )
            .unwrap();
        }
        db.add_audit(
            6500,
            None,
            Some("y"),
            "login_fail_unknown_user",
            Some("2.2.2.2"),
            None,
        )
        .unwrap();
        // 窗外的失败：不该被算进来。
        db.add_audit(
            1000,
            None,
            Some("z"),
            "login_fail_bad_password",
            Some("9.9.9.9"),
            None,
        )
        .unwrap();
        // 成功登录：不该被算进来，哪怕来自同一个 IP。
        db.add_audit(6600, None, Some("x"), "login_ok", Some("1.1.1.1"), None)
            .unwrap();

        let rows = db.dashboard(0, 5000, TOP).unwrap().login_fails;

        assert_eq!(rows.len(), 2, "窗外 IP 与成功登录都不该出现");
        assert_eq!(rows[0].client_ip.as_deref(), Some("1.1.1.1"));
        assert_eq!(rows[0].count, 3, "成功那条不能被算进失败数");
        assert_eq!(rows[1].client_ip.as_deref(), Some("2.2.2.2"));
        assert_eq!(rows[1].count, 1);
        assert!(
            rows.iter()
                .all(|r| r.client_ip.as_deref() != Some("9.9.9.9")),
            "窗外的失败泄漏进来了"
        );
    }

    /// **未来新增一种失败原因也必须被统计到**（`LIKE 'login_fail%'` 的意义）。
    ///
    /// 若哪天改成枚举具体动作，这条会红——而它该红：异常统计的用途正是发现
    /// 没预料到的东西，「新增一种失败就从统计里消失」是最不该有的漏。
    #[test]
    fn dashboard_login_fails_catch_unforeseen_failure_kinds() {
        let db = db();
        db.add_audit(
            5000,
            None,
            Some("x"),
            "login_fail_some_future_reason",
            Some("3.3.3.3"),
            None,
        )
        .unwrap();

        let rows = db.dashboard(0, 0, TOP).unwrap().login_fails;
        assert_eq!(rows.len(), 1, "没见过的失败原因也必须进统计");
        assert_eq!(rows[0].count, 1);
    }

    /// 账目自检在健康库上必须 ok，且两条恒等式都真的被算了。
    #[test]
    fn dashboard_integrity_holds_on_healthy_db() {
        let db = db();
        let a = rich(&db, "alice", 100);
        let b = rich(&db, "bob", 60);
        db.board(a, 7, pr(), 1000).unwrap();
        db.board(b, 7, pr(), 2000).unwrap();

        let i = db.dashboard(0, 0, TOP).unwrap().integrity;
        assert!(i.ok, "健康库不该报异常: {i:?}");
        assert_eq!(i.wallet_violations, 0);
        assert_eq!(
            i.balance_sum,
            db.balance_of(a).unwrap() + db.balance_of(b).unwrap()
        );
        assert_eq!(i.ledger_sum, i.balance_sum, "余额和必须等于流水和");
        assert!(i.balance_sum > 0, "别让上面几条变成 0 == 0 的空断言");
    }

    /// **自检必须真的能发现被篡改的账。**
    ///
    /// 直接 UPDATE 余额（模拟手工干预 / 未来某个 bug），不动流水，自检必须报警。
    /// 没有这一条的话，一个恒返回 `ok: true` 的实现也能通过上面那个用例。
    #[test]
    fn dashboard_integrity_detects_tampered_balance() {
        let db = db();
        let a = rich(&db, "alice", 100);
        {
            let conn = db.conn.lock();
            conn.execute(
                "UPDATE portal_balances SET balance = balance + 999 WHERE user_id = ?1",
                params![a],
            )
            .unwrap();
        }

        let i = db.dashboard(0, 0, TOP).unwrap().integrity;
        assert!(!i.ok, "余额被改大 999 分却报账目正常");
        assert_ne!(i.balance_sum, i.ledger_sum);
        assert_eq!(
            i.wallet_violations, 1,
            "balance != topup - spent 必须被数到"
        );
    }

    /// 删号不该让自检永久报警（`ledger_sum` 按在册用户过滤的理由）。
    ///
    /// 【为何这条重要】删号后余额随 CASCADE 消失，而流水故意留着。若不过滤，
    /// 任何删过号的库都会永久显示「账目异常」——那样的告警会被当成噪音忽略，
    /// 比没有告警更糟。
    #[test]
    fn dashboard_integrity_survives_user_deletion() {
        let db = db();
        let a = rich(&db, "alice", 100);
        let b = rich(&db, "bob", 50);
        db.board(a, 7, pr(), 1000).unwrap();

        assert!(db.dashboard(0, 0, TOP).unwrap().integrity.ok, "前置条件");

        db.delete_user(a).unwrap();

        let i = db.dashboard(0, 0, TOP).unwrap().integrity;
        assert!(i.ok, "删号后自检误报: {i:?}");
        assert_eq!(i.balance_sum, db.balance_of(b).unwrap(), "只剩 bob 的余额");
    }

    /// **每一种流水 kind 都必须被看板计入某个字段。**
    ///
    /// 【这条测试要拦的是什么】`window_totals` 用 `match kind` 分派，未知 kind 只
    /// 打一条 warn 就丢掉。日后新增一种 kind（比如「过期扣回」）若忘了加分支，
    /// 那笔钱会在看板上凭空消失——没有报错、没有崩溃，只是数字对不上，而且
    /// 对不上的方式很难反推。这里遍历全部 kind 常量，逐个写一笔再要求它出现在
    /// 某个字段里，漏掉分支时在 `cargo test` 阶段就红。
    #[test]
    fn dashboard_covers_every_ledger_kind() {
        for kind in [TOPUP_KIND, UNLOCK_KIND, REFUND_KIND, ADMIN_ADJUST_KIND] {
            let db = db();
            // 先垫底再按 kind 记账：出账类 kind 需要余额，否则 apply_delta 直接拒绝。
            let uid = rich(&db, "alice", 1000);
            let base = db.dashboard(0, 0, TOP).unwrap().total;

            // 出账为负、进账为正——符号由 kind 的业务语义决定。
            let delta = if kind == UNLOCK_KIND { -7 } else { 7 };
            db.adjust_balance(uid, delta, kind, None, None, 2000)
                .unwrap()
                .unwrap();

            let now = db.dashboard(0, 0, TOP).unwrap().total;
            assert_ne!(
                now, base,
                "kind={kind} 的金额没有出现在看板的任何字段里——新增 kind 后请在 window_totals 里加分支"
            );
        }
    }
}
