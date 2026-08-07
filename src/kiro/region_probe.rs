//! 上号时自动探测凭据可用的 region，把结果写死进凭据。
//!
//! # 为什么必须有这个
//!
//! `ksk_` token 是**按 region 授权**的：打错 region 上游直接回 403
//! `AccessDeniedException`，一个请求都成功不了。而凭据不带任何 region 字段时，
//! `effective_upstream_region` 会一路回退到 `config.region`（线上是 `us-east-1`）。
//!
//! 2026-08-04 实测（同一个 `kiroApiKey`、同一个账号、只有 region 不同的对照组，约 8 个账号）：
//!
//! | key | eu-central-1 | 回退 us-east-1 |
//! |---|---|---|
//! | `a454081422e2` | 89.5% 成功 / 0% 403 | **100% 403** |
//! | `0dc316cf0323` | 98.3% 成功 | **100% 403** |
//! | `1acc2e8781d3` | 98.9% 成功 | **100% 403** |
//! | `4a3ec3b1141e` | 98.1% 成功 | **100% 403**（4 份全废） |
//!
//! 而推号方给 region 的比例在逐日变化（07-28 是 0%，08-04 是 74%），所以「不带 region」
//! 的号越来越可能是 eu 账号 → 落 `us-east-1` 即废。无 region 号的「上号即废率」实测
//! 08-02 = 0%、08-03 = 27%、08-04 = 30%，正在恶化。
//!
//! 改 `config.region` 只是把赌注押到另一边（反例确实存在：key `e0d95e000d32` 在
//! `us-east-1` 跑 99.1%、在 `eu-central-1` 100% 403）。**探一次写死**才是根治：
//! 从此该号不依赖任何全局默认值。
//!
//! # 🔴 探测必须打「能区分授权与否」的端点，而不是「结论要用的那个」端点（2026-08-06 实测定案）
//!
//! 本模块的结论写进 `api_region`，其**唯一消费者**是 `CliEndpoint::host()` =
//! `q.{region}.amazonaws.com`（`endpoint/cli.rs`）。由此曾推出一条听起来无懈可击的
//! 要求：「必须探该号真正会用的那个域名，否则是探 A 决定 B」。按它把探测从
//! `management.{region}.kiro.dev/getUsageLimits` 换成了 `q.*` 服务根 —— **这是个回归**。
//!
//! 实测（2026-08-06，凭据 `#749` = `ksk_u7Wd…`，已知**只在 `eu-central-1` 授权**、
//! 在 `q.eu-central-1` 实测 98.9% 成功；两组各用与对应实现完全相同的请求）：
//!
//! | 目标 | `eu-central-1`（已授权） | `us-east-1`（未授权） | 能区分？ |
//! |---|---|---|---|
//! | `q.*` 服务根 + `X-Amz-Target`（故意不完整的 body） | `400 ValidationException` `REQUEST_BODY_INVALID` | **`400` 同上** | ❌ **完全不能** |
//! | `management.*/getUsageLimits` | `200` | `403 {"message":"Invalid token","reason":null}` | ✅ 泾渭分明 |
//!
//! 结论：**AWS 先校验请求体格式、后校验 region 授权**。故意不完整的探测体在**任何**区
//! 都先撞 `REQUEST_BODY_INVALID`，授权那一关根本没被求值。于是 `400 = 认证已通过` 这条
//! 判据是假的，而 `PROBE_ORDER` 首项是 `eu-central-1` ⇒ **任何 `api_key` 号第一次探测
//! 都被判「eu-central-1 可用」**，US 号的 `api_region` 被写成 `eu-central-1` → 恒 403。
//!
//! 所以判据回到 `management.*`。它确实是「探 A 决定 B」，但那个 proxy **有实测支撑**：
//! 同一个 key 上 `management.eu` 拿 200 且 `q.eu` 跑 98.9% 成功、`management.us` 拿 403
//! 且该区未授权 —— 两个域名的 region 授权结论一致（都由同一份 `ksk_` 授权范围决定）。
//! 「打真实端点」这个直觉在这里换来的是**失去区分能力**，而区分能力才是探测的全部价值。
//!
//! 若将来要再试「打真实端点」，前置条件是**先实测**：把探测体做完整到能通过 body 校验，
//! 确认未授权区此时回的是 403 —— 而完整 body 意味着每个新号上号真花掉一次对话额度。
//! 不要再凭「AWS 应当先认证后校验」这类推理动它，那正是本回归的成因。
//!
//! # 判据（这是本模块唯一容易写错的地方）
//!
//! | 上游响应 | 含义 | 动作 |
//! |---|---|---|
//! | 2xx | region 正确 | ✅ 采纳 |
//! | **429 限流** | **region 正确**，只是拥堵 | ✅ 采纳（见下） |
//! | 401 | token 本身废了 | ⛔ 整体放弃，别再探 |
//! | **403 + `temporarily suspended`** | **账户级临时风控**，与 region 无关 | ⚠️ 试下一个，但整轮判决另立（见下） |
//! | 403（其余，含 `Invalid token`） | region **错**（token 未在此 region 授权） | ❌ 试下一个 |
//! | 兜底：**400** / 5xx / 网络 / DNS 不解析 | 无结论 | ❌ 试下一个，但不据此判死 |
//!
//! 表的**顺序就是代码里的顺序**，且每一步的位置都是承重的，见 [`classify_probe_result`]。
//!
//! **把 429 当成功是承重的**：上游对 region 正确但拥堵的号回 429，若判成失败就会
//! 跳过一个完全可用的 region、继续探下去，最坏把所有候选都探成「失败」然后回退到
//! `config.region` —— 正是本模块要消除的那个状态。
//!
//! **400 落兜底 `Inconclusive`，绝不再判 `Usable`**：上面对照表的第一行就是它被证否的
//! 实测。也不判 `WrongRegion` —— 400 是格式层的拒绝，不携带任何 region 信息，
//! 据它把区判死同样无依据。走 `management.*` 后这个状态码本不该出现（api_key 号不带
//! `profileArn`，没有 `400 profileArn is required` 那条路），但判据仍显式覆盖它，
//! 因为「万一出现」时唯一不可接受的结果是把坏区判成可用。
//!
//! # 「429 判 Usable」的敞口（回到 `management.*` 后已大幅收窄）
//!
//! 该判据成立的前提是「429 发生在 region 授权校验**之后**」。探 `q.*` 时这个前提敞口很大
//! ——那正是 429 风暴的发生地（近 2h 上游真 429 达 7689 次），若 AWS 在边缘层按调用方
//! 限流，坏区也会回 429 ⇒ 坏区被写死进粘性的 `api_region` ⇒ 该号从此恒 403。
//! 回到 `management.*`（Kiro 自建 REST 服务、量级低、不在风暴路径上）后敞口显著收窄，
//! 但仍未被实测证否。要证否只需一次对照实验：拿已知只授权在 A 区的 key 打 B 区，
//! 在风暴时段看回的是 403 还是 429。拿到结论前别动这条判据的顺序。

use std::sync::Arc;

use crate::http_client::{ProxyConfig, build_client};
use crate::kiro::endpoint::RequestContext;
use crate::kiro::machine_id;
use crate::kiro::model::credentials::KiroCredentials;
use crate::model::config::Config;

/// 单次探测的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbeVerdict {
    /// region 正确（2xx 或 429 —— 后者见模块文档的判据表）。
    ///
    /// ⚠️ 400 **不在**此列：它曾被算进来，实测证否，见 [`classify_probe_result`] 的兜底注释。
    Usable,
    /// region 不对（403：token 未在此 region 授权）。
    WrongRegion,
    /// token 本身失效（401）——继续探其它 region 毫无意义。
    TokenDead,
    /// **账户级临时风控**（403 `temporarily is suspended`）——与 region 毫无关系。
    ///
    /// 必须与 [`Self::WrongRegion`] 分开，理由见 [`classify_probe_result`] 里那段
    /// 「为什么 suspend 不能判 WrongRegion」。
    AccountThrottled,
    /// 无结论（5xx / 网络 / 解析失败）——可以试下一个，但不能据此判死。
    Inconclusive,
}

/// 把一次单区探测的结果映射成探测结论。
///
/// 单独抽成纯函数是为了能用单测钉死判据表 —— 真实探测要走上游往返，测不了。
/// 判据只看错误**文案里的状态码**，所以与具体上游无关；[`probe_usable_region_endpoint`]
/// 刻意与 `token_manager::fetch_usage_limits_once` 保持同样的文案形状。
///
/// # 分支顺序是承重的
///
/// 每一条的位置都有理由，且都有一条断言顺序（而非分支内部）的守卫测试。改动顺序前
/// 先读那些测试的文档 —— 本仓有过「改三处、四条测试、三次『回退即 FAILED』全过而修复
/// 无效」的事故，成因正是测了分支内部却没测分支顺序。
pub(crate) fn classify_probe_result(result: &Result<(), String>) -> ProbeVerdict {
    let err = match result {
        Ok(()) => return ProbeVerdict::Usable,
        Err(e) => e.as_str(),
    };
    let low = err.to_ascii_lowercase();

    // ⭐ 429 必须判 Usable，且必须排在 403 之前判：上游 429 的响应体里可能同时含
    // "403" 之类的无关数字（例如 requestId），先判 403 会把可用 region 误杀。
    if low.contains("429") || low.contains("too many requests") || low.contains("throttling") {
        return ProbeVerdict::Usable;
    }
    // 401：token 废了。必须排在 403 之前 —— 有些上游响应两个码都提。
    if low.contains("401") || low.contains("认证失败") {
        return ProbeVerdict::TokenDead;
    }
    // ⭐ 账户级临时风控，**必须排在 403 之前**（2026-08-06 加）。
    //
    // # 为什么 suspend 绝不能判 WrongRegion
    //
    // 两者是**同一个 403 `AccessDeniedException`**，只有 body 文案不同：
    // - region 未授权：`{"message":"The bearer token included in the request is invalid."}`
    // - 账户临时风控：`{"message":"Your User ID (1866...) temporarily is suspended.
    //   We've locked your account as a security precaution..."}`
    //
    // 探测判据里没有这道分支时，风控号走的是这条链：
    // 403 suspend → `WrongRegion` → 两候选耗尽 → [`ProbeOutcome::NoUsableRegion`] →
    // `mark_region_probe_failed` 置 `disabled=true` + `RegionProbeFailed`，而该原因
    // **不在** `is_self_healable_reason` 白名单里 ⇒ **临时态被固化成需人工的永久态**，
    // 且归因指向反方向（运维去查 region 授权，真因是账号风控）。分身同批一起禁用。
    //
    // 这条失效路径是「探测从 `management.*/getUsageLimits` 搬到对话端点 `q.*`」**新引入**的：
    // 旧端点不检查 suspend（`token_manager.rs` 的 `deep_verify_credential` 注释写明
    // 「只有真实对话请求才能检测」），所以旧探测对 suspend 是瞎的（返 200 → Usable）。
    // 换到对话端点后 suspend 第一次出现在探测路径上，而判据表里没有它。
    //
    // 严重度：这类 403 占近 2h 流量 22.3%，是常态。本仓对同一类误判有事故史 ——
    // `default_is_account_suspended` 的注释记着 temporarily_suspended 曾被当永久封禁：
    // 12h 内 88 次误禁 + 51 次「凭据用尽」+ 36 次全池自愈活锁、逐小时拒绝率升到 100%。
    // 而 `MAX_CONSECUTIVE_SUSPICIOUS_BEFORE_DISABLE = 6` 存在的唯一理由就是
    // 「见过一次 403 不足以判死」—— 探测这条路径用**一次** 403 就判死，绕过了那道阈值。
    //
    // 判据复用 `endpoint::default_is_temporary_rate_limit`（认 `temporarily is suspended` /
    // `temporarily suspended` / `temporarily_suspended` 三种书写，另有 suspicious+temporary
    // 的组合式）：绝不在这里另写一套，那正是上面那次事故的成因（两处判据分叉、结论相反）。
    //
    // ⚠️ 位置也是承重的，**两侧各有约束**：
    // - 必须在 429 之**后** —— 复用的判据里有 `rate limits applied` / `temporary rate`
    //   这类词，真 429 的 body 若恰好带上就会被这条截走，而 429 判 Usable 是本模块的
    //   头号承重规则（见模块文档）。放在后面即天然不可能截到 429。
    // - 必须在 403 之**前** —— suspend 的 body 里同时有 `403` 与 `AccessDeniedException`，
    //   排在 403 之后就永远走不到，等于这道分支不存在（本仓的「测了分支内部、
    //   没测分支顺序」伪证形态，故守卫测试断言的是最终结论而非中间量）。
    if crate::kiro::endpoint::default_is_temporary_rate_limit(err) {
        return ProbeVerdict::AccountThrottled;
    }
    // 403：region 不对。这是本模块要抓的那个信号。
    //
    // `management.*` 对未授权区的实测原文是 `{"message":"Invalid token","reason":null}`
    // （2026-08-06，凭据 #749 打 us-east-1）—— 文案说 token 无效，但 token 本身是好的
    // （同一秒打 eu-central-1 拿 200），所以**绝不能**据这个文案判 `TokenDead`。
    // 认 `invalid token` 是刻意的：它是本模块要抓的那个信号的真实书写形态。
    // 注意 401 的判据排在本条之前，所以真 401 不会被这条截走。
    if low.contains("403")
        || low.contains("accessdenied")
        || low.contains("权限不足")
        || low.contains("bearer token included in the request is invalid")
        || low.contains("invalid token")
    {
        return ProbeVerdict::WrongRegion;
    }
    // 兜底 `Inconclusive`：5xx / 网络 / DNS —— **以及 400**。
    //
    // # ⭐ 400 落这里是承重的：它曾被判 `Usable`，那是本模块最严重的一次回归
    //
    // 旧判据写「400 = 认证已通过、只是探测体故意不完整 ⇒ region 对」，依据是一条**推理**：
    // 「AWS 先做 Bearer token 的认证/授权，未授权区拿到的是 403 而不是 400」。
    // 实测把它证否了（见模块文档的对照表）：同一个只在 `eu-central-1` 授权的 key，
    // 打 `q.eu-central-1` 与 `q.us-east-1` **都**回 `400 REQUEST_BODY_INVALID` ——
    // AWS 先校验请求体格式、后校验授权，不完整的探测体让授权那一关根本没被求值。
    // 于是 400 恒判 Usable + `PROBE_ORDER` 首项是 eu ⇒ **每个 api_key 号都被写成 eu**，
    // US 号 100% 废。
    //
    // 为什么落 `Inconclusive` 而不是 `WrongRegion`：400 是格式层的拒绝，**不携带任何
    // region 信息** —— 据它把区判死同样无依据。`Inconclusive` 的整轮后果是继续试下一个、
    // 全试完则 `NoUsableRegion`（维持禁用），那是安全的一侧：号可见、可人工填区、
    // 下次重启还会重探。
    //
    // 刻意**不为 400 写一条显式分支**：那条分支的返回值与本兜底逐字相同，属于死代码，
    // 而且会新造一个「400 与 403 谁先判」的顺序问题 —— 落兜底则 403 天然优先，
    // 一个同时含 `403` 与无关 `400` 字样的响应仍被正确判成 `WrongRegion`。
    ProbeVerdict::Inconclusive
}

/// 探测候选顺序（首个 `Usable` 即采纳）。
///
/// # 为什么仍然只有两项（2026-08-06 复核，理由已换）
///
/// 旧理由是「`management.*`/`runtime.*` 只在这两个区解析 DNS」——探测改打
/// `q.{region}.amazonaws.com` 后**那条理由不再成立**（AWS 的 Amazon Q 端点集合与
/// kiro.dev 无关，很可能存在于更多区）。表没有跟着扩，是因为换成了另一条理由：
///
/// **每个候选是一次真实上游往返，而上号路径是串行在用户的「添加凭据」HTTP 请求里的。**
/// 现有依据只支撑这两项：线上实测能用的号 `eu-central-1` 99 个、`us-east-1` 11 个，
/// 其余 region 在本池中**零命中**。凭「AWS 大概也支持 ap-southeast-1」去加候选，
/// 拿到的是每个新号多等一次往返，换来的是一个没有任何观测支撑的假设。
///
/// # 要扩表请按这个门槛
///
/// 每个新增候选必须附一条实测依据（哪个 key、在该区拿到什么状态码），
/// 否则不加。若真出现「只在第三个区授权」的号，它现在的表现是被判
/// [`ProbeOutcome::NoUsableRegion`] 而保持禁用 —— 面板上能看见原因
/// （`RegionProbeFailed`），运维可用 `set_credential_api_region` 手填该区，
/// 那条手填路径压过探测结果（见 `effective_upstream_region` 的 api_key 专属分支）。
/// 即「探不到」是**可见且可人工修**的，不是静默失败 —— 这是不盲目扩表的前提。
pub(crate) const PROBE_ORDER: &[&str] = &["eu-central-1", "us-east-1"];

/// 探测上限：最多试几个候选。
///
/// 与 [`PROBE_ORDER`] 长度一致。此前取 3 而表里有 5 项，于是最后两项**永远探不到**——
/// 上限小于表长等于表尾是死项。现在锁成表长：加候选就一定会被探到，
/// 而"别让表膨胀"这个意图由 [`PROBE_ORDER`] 文档里的依据门槛承担。
pub(crate) const MAX_PROBE_ATTEMPTS: usize = PROBE_ORDER.len();

/// 整轮探测的**判决**（不是单次候选的结论，那是 [`ProbeVerdict`]）。
///
/// # 为什么必须是枚举而不是 `Option<String>`
///
/// 旧签名把**四种语义完全不同**的结果全塌缩成 `None`，于是调用方无法区分
/// 「不用探」与「探了但没探到」——而这两者的正确处置**相反**：
///
/// | 情形 | 旧返回 | 正确处置 |
/// |---|---|---|
/// | 已带 region 字段（推号方给了/运维手填） | `None` | **照常启用**（调用方明确意图） |
/// | 非 api_key 号（OAuth 走 profileArn） | `None` | **照常启用**（本就不该探） |
/// | 探到可用 region | `Some(r)` | 写死 + 启用 |
/// | 全候选 403/无结论 | `None` | **保持禁用**（启用即恒 403 被打死） |
/// | token 已废（401） | `None` | **保持禁用**（且原因不同于上一条） |
///
/// 线上事故：`add_credential` 以**启用态**入池后才探测（要 1~2 秒），窗口里真实流量
/// 打到错区恒回 403，3 次即自动禁用 ⇒ 号在自己的 region 被探出来之前就死了。
/// 实测 #536–550 共 15 个号在两分钟内全部这样死掉（每个只跑 1~6 个请求、**0 成功**），
/// 而 4 分钟后同一批 key 的 #551–556 探到 `eu-central-1` 后 881/881 全成功。
/// 修复要求调用方能按判决决定启用与否 —— 这就是本枚举存在的唯一理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProbeOutcome {
    /// 探到可用 region（应写死进 `api_region` 并启用）。
    Usable(String),
    /// **无需探测**：已带 region 字段，或不是 api_key 号。
    ///
    /// ⚠️ 与「探测失败」必须分开:这条是调用方的明确意图,**绝不能**据此禁用凭据。
    /// 把两者压成同一个值正是旧签名的根本问题。
    Skipped,
    /// 候选全部试过仍无可用 region（403 / 无结论）。启用它等于让它恒 403 被打死。
    NoUsableRegion,
    /// token 本身已失效（401）。与 [`Self::NoUsableRegion`] 分开是因为**处置动作不同**：
    /// 这条要去查 token 来源，那条要去查 region 授权范围。运维看原因就知道该查哪儿。
    TokenDead,
    /// **账户级临时风控**挡住了探测（403 `temporarily is suspended`），region 无从判断。
    ///
    /// # 为什么必须是独立变体，返 `Inconclusive`/`NoUsableRegion` 都不行
    ///
    /// 单次候选的 [`ProbeVerdict::Inconclusive`] 在整轮汇总时仍落
    /// [`Self::NoUsableRegion`] ⇒ 照样被 `mark_region_probe_failed` 禁用成
    /// `RegionProbeFailed`（不在自愈白名单）⇒ 临时态固化成永久态。所以「区分」必须
    /// 一直传到**整轮判决**这一层，调用方才有可能不落那个原因。
    ///
    /// # 语义：这是「探不了」，不是「探过不行」
    ///
    /// 风控是账户级的、与 region 无关，所以这一轮**没有产生任何关于 region 的信息**。
    /// 正确处置是「等风控过去后重探」，而不是禁用。它刻意**不进** `service.rs` 里那个
    /// `matches!(probe_outcome, NoUsableRegion | TokenDead)` 判据（非穷尽匹配，新变体
    /// 天然落 false）⇒ 语义上等于「不禁用」，且不需要改那处代码。
    ///
    /// # 为什么不顺手把该区写死进 `api_region`
    ///
    /// 有一条**诱人但没实测支撑**的推断：AWS 应当先做授权、再检查账户状态，所以
    /// 「拿到 suspend」本身就证明该区授权通过 ⇒ 可以采纳该区。但若这个顺序反了，
    /// 后果是把**首个候选区**（`eu-central-1`）写死给每一个被风控的号，而 `api_region`
    /// 是粘性的（只有运维手填能压过它）⇒ 该号从此恒 403，比不写死糟得多。
    /// 不写死的代价只是「继续靠 `config.region` 回退」，即回到探测接入前的基线，
    /// 且下次重启的存量回填（`main.rs`）会再探一次 —— 那是自动恢复路径，
    /// 而 `disabled=true` + `RegionProbeFailed` 会把号从回填名单里排除
    /// （`ids_needing_region_probe` 过滤 `!e.disabled`），彻底没有自动恢复。
    AccountThrottled,
}

/// 一次候选探测的**目标**：把 region 钉死后的凭据副本 + 这一次真正会打的 URL +
/// 该结论将来会作用到的对话 URL。
///
/// 单独成型是为了让「探的到底是哪个 host、结论又用在哪个 host」变成**可单测断言的纯函数
/// 产物**（见 [`probe_target`]）—— 本模块两轮缺陷都是这个关系在代码里无处可看造成的：
/// 第一轮是无意识地探 A 决定 B，第二轮是为了消掉这个不对称而换到 B，结果**失去了区分
/// 能力**（见模块文档实测表）。现在两个 URL 都在结构体里摊开，任何一侧被改动都会被
/// [`probe_target_probes_management_and_pins_dialog_region`] 抓到。
pub(crate) struct ProbeTarget {
    /// 只改了 `api_region` 的凭据副本（不动原凭据，探失败无副作用）。
    pub(crate) candidate: KiroCredentials,
    /// 被探的那个候选 region（`candidate.api_region` 的同值，单独存一份省得再解 Option）。
    pub(crate) region: String,
    /// **这一次真正会打的 URL**：`management.{region}.kiro.dev/getUsageLimits`。
    /// 由 `token_manager::usage_limits_probe_url` 算出 —— 本模块不许手搓 host。
    pub(crate) probe_url: String,
    /// 探测结论（写进 `api_region`）将来会作用到的对话 URL
    /// （`ksk_` 号 = CLI = `https://q.{region}.amazonaws.com/`）。
    ///
    /// 不发请求，只为让「探 A 决定 B」这个**刻意保留**的不对称在代码与日志里可见。
    /// 它有实测支撑（同一 key：`management.eu`=200 且 `q.eu` 98.9% 成功；
    /// `management.us`=403 且该区未授权），但既然是 proxy 就该看得见。
    pub(crate) dialog_url: String,
}

/// 构造「把 region 钉死到 `region` 之后，这个号会打哪个 URL」。
///
/// # 为什么要有 region 一致性校验（返回 `None` 的那条）
///
/// `effective_upstream_region` 的优先级是 `profileArn` > `api_region`(api_key 专属) >
/// `region`/`auth_region` > config。若某个更高优先级的字段已经把 region 钉在别处，
/// 那么**写 `api_region` 是不生效的**：我们会打着 A 区的 host、把结论记成 B 区，
/// 重新制造本模块刚修掉的那个缺陷（探 A 决定 B）。
///
/// 调用方对 `None` 的正确处置是判 [`ProbeVerdict::Inconclusive`]（跳过该候选），
/// **不是**判 region 不可用 —— 它是「探不了」，不是「探过不行」。
///
/// 实际走到这里时 `probe_api_region` 已过滤掉带 region 字段的号，所以只有
/// 「凭据带 profileArn」才可能触发；`ksk_` 号一般无 profileArn，故这是纵深防御。
pub(crate) fn probe_target(
    credentials: &KiroCredentials,
    config: &Config,
    region: &str,
) -> Option<ProbeTarget> {
    let mut candidate = credentials.clone();
    candidate.api_region = Some(region.to_string());
    if candidate.effective_upstream_region(config) != region {
        tracing::warn!(
            region = %region,
            actual = %candidate.effective_upstream_region(config),
            "该凭据有更高优先级的 region 来源（profileArn 等），写 api_region 不生效，跳过此候选"
        );
        return None;
    }

    // 探测 URL 由 `token_manager` 算 —— 那里是 `getUsageLimits` 的**单一真相**，
    // 业务取额度走的是同一个 `(host, url)` 与同一个请求装配函数。绝不在这里手搓 host：
    // 探测请求与业务请求一旦分形，探出来的结论就不适用于业务路径。
    let probe_url = crate::kiro::token_manager::usage_limits_probe_url(&candidate, region);

    // 结论将来作用到的对话 URL，只为可见性（不发请求）。走端点抽象而非手搓，
    // 这样 `q.*` 的形状哪天变了，本模块的断言会跟着变而不是静默过期。
    let machine_id = machine_id::generate_from_credentials(&candidate, config);
    let endpoint = crate::kiro::endpoint::for_credentials(&candidate, &config.default_endpoint);
    let dialog_url = endpoint.api_url(&RequestContext {
        credentials: &candidate,
        // URL 只由 region 决定（见各端点的 `api_url`），token 不参与，故传空串占位。
        token: "",
        machine_id: &machine_id,
        config,
        is_1m: false,
    });
    Some(ProbeTarget {
        candidate,
        region: region.to_string(),
        probe_url,
        dialog_url,
    })
}

/// 对一个候选 region 打一次 `management.{region}.kiro.dev/getUsageLimits`，
/// 返回可交给 [`classify_probe_result`] 的结果。
///
/// # 为什么是这个端点（2026-08-06 实测定案，别再换回对话端点）
///
/// 它是**唯一实测能区分「该 region 是否授权」的探针**：同一个只在 `eu-central-1` 授权的
/// key，这里 `eu-central-1` 拿 200、`us-east-1` 拿 `403 {"message":"Invalid token"}`。
/// 而对话端点（`q.*` 服务根）配故意不完整的请求体时**两个区都回 400**，
/// 授权那一关根本没被求值 —— 完整对照表见模块文档。
///
/// # 成本：零额度
///
/// `getUsageLimits` 是只读的额度查询，不产生对话计费。上号是用户交互路径，
/// 而「把探测体做完整以便让授权成为决定项」意味着每个新号真花掉一次对话额度 ——
/// 这是不选那条路的第二个理由（第一个是它需要一轮实测才敢上）。
///
/// # 只看状态码，**不解析响应体**
///
/// 刻意不复用 `fetch_usage_limits_once`（它会把 200 的 body 解析成
/// `UsageLimitsResponse`）：那样上游哪天改了响应形状，解析失败会被记成
/// `Inconclusive` ⇒ 全候选试完判 `NoUsableRegion` ⇒ **所有新 `ksk_` 号一律被禁用**。
/// 探测只关心「这个区认不认这个 token」，2xx 就是认了，body 长什么样与结论无关。
/// 请求装配仍与业务路径共用 `build_usage_limits_request`（UA/`tokentype`/`profileArn`
/// 一字不差），否则探出来的结论不适用于业务请求。
///
/// 错误文案刻意与 `token_manager::fetch_usage_limits_once` 保持同样的
/// 「中文说明: {状态码} {正文}」形状，使 [`classify_probe_result`] 的判据表对两者同样成立。
async fn probe_usable_region_endpoint(
    target: ProbeTarget,
    config: &Config,
    token: &str,
    proxy: Option<&ProxyConfig>,
) -> Result<(), String> {
    // 30s 与 deep_verify_credential 同口径：探测不是流式对话，不用 720s。
    let client = build_client(proxy, 30, config.tls_backend)
        .map_err(|e| format!("探测客户端构建失败: {e}"))?;

    let response = crate::kiro::token_manager::build_usage_limits_request(
        &client,
        &target.candidate,
        config,
        token,
        &target.region,
    )
    .send()
    .await
    .map_err(|e| format!("region 探测请求失败: {e}"))?;

    let status = response.status();
    if status.is_success() {
        return Ok(());
    }

    let body_text = response.text().await.unwrap_or_default();
    let error_msg = match status.as_u16() {
        401 => "认证失败，Token 无效或已过期",
        403 => "权限不足，该 region 未授权",
        429 => "请求过于频繁，已被限流",
        500..=599 => "服务器错误，AWS 服务暂时不可用",
        _ => "region 探测失败",
    };
    Err(format!("{error_msg}: {status} {body_text}"))
}

/// 对一个凭据探测可用 region。
///
/// 返回 [`ProbeOutcome`]，调用方据此决定「写死 region 并启用」还是「保持禁用」。
/// 判决语义与各分支的理由见该枚举的文档。
///
/// # 只对「没有任何 region 字段」的凭据探测
///
/// 凭据已显式带 region（推号方给了、或运维手填）时直接返回 [`ProbeOutcome::Skipped`]
/// 不探：那是调用方的明确意图，探测不该覆盖它，而且带 region 的号本来就不是本缺陷的受害者。
pub(crate) async fn probe_api_region(
    credentials: &KiroCredentials,
    config: &Arc<Config>,
    token: &str,
    order: &[&str],
) -> ProbeOutcome {
    let effective_proxy = credentials.effective_proxy(None);
    probe_api_region_with(credentials, config, order, |target| {
        let cfg = Arc::clone(config);
        let proxy = effective_proxy.clone();
        async move { probe_usable_region_endpoint(target, &cfg, token, proxy.as_ref()).await }
    })
    .await
}

/// [`probe_api_region`] 的可注入内核：把「打一次上游」抽成参数。
///
/// # 为什么要注入
///
/// 本仓铁律禁止测试依赖网络，而本模块历史上出过的三个缺陷全都在**循环与判决**这一层
/// （首个 Usable 即返 / 401 不停 / 「不用探」与「探失败」同值），不在 HTTP 那一层。
/// 抽出内核后这些都能用假 prober 直接测行为，而不是只能靠源码断言。
/// 注意 prober 收到的是 [`ProbeTarget`]，所以测试还能顺带断言**探的是哪个 URL**、
/// 以及**按授权情况返回不同结果时结论跟不跟着走** —— 后者正是本轮回归漏掉的那条覆盖
/// （见 [`us_only_key_must_resolve_to_us_east_1`]）。
///
/// prober 收**所有权**而非引用：闭包返回的 future 若借用参数就要 HRTB，
/// 而闭包的返回类型推不出高阶生命周期（实测编译不过）。给所有权最省事，
/// 调用方需要 URL 的话在调用前自己留一份（本函数就是这么做的）。
pub(crate) async fn probe_api_region_with<F, Fut>(
    credentials: &KiroCredentials,
    config: &Arc<Config>,
    order: &[&str],
    mut prober: F,
) -> ProbeOutcome
where
    F: FnMut(ProbeTarget) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    // 已有任一 region 字段 → 不探（见函数文档）。
    if credentials.region.is_some()
        || credentials.auth_region.is_some()
        || credentials.api_region.is_some()
    {
        tracing::debug!("凭据已带 region，跳过自动探测");
        return ProbeOutcome::Skipped;
    }
    // 只对 api_key（`ksk_`）号探：它们是「按 region 授权」的那一类。
    // OAuth 号的 region 由 profileArn 决定（`effective_upstream_region` 第一优先），
    // 探测既无必要也会与那条路径打架。
    if !credentials.is_api_key_credential() {
        return ProbeOutcome::Skipped;
    }

    // 整轮汇总要的两个事实。**不能只看最后一个候选的 verdict**：
    // ① 实际打出去过几次 —— 0 次意味着这一轮没产生任何关于 region 的信息（见循环后）。
    // ② 是否见过账户级风控 —— 它与 region 无关，不该被汇总成「region 不可用」。
    let mut probed = 0usize;
    let mut saw_account_throttled = false;

    for region in order.iter().take(MAX_PROBE_ATTEMPTS) {
        // 「这个号打哪个 URL」由端点抽象算，探的与将来跑的是同一个 host。
        let Some(target) = probe_target(credentials, config, region) else {
            // 更高优先级字段把 region 钉在别处 ⇒ 探不了（不是探过不行），跳过。
            // 注意这条**不递增 `probed`**：P3-b 的整轮汇总要靠它区分两种「都没成」。
            continue;
        };
        probed += 1;
        // prober 拿走所有权（见本函数文档），日志要的两个 URL 先各留一份。
        // 两个都记：排查时「探了哪儿」与「结论用在哪儿」缺一个就说不清（本模块两轮缺陷
        // 都是这个关系看不见造成的）。
        let probe_url = target.probe_url.clone();
        let dialog_url = target.dialog_url.clone();
        let outcome = prober(target).await;

        let verdict = classify_probe_result(&outcome);
        tracing::info!(
            region = %region,
            probe_url = %probe_url,
            pins_dialog_url = %dialog_url,
            ?verdict,
            "region 自动探测"
        );
        match verdict {
            ProbeVerdict::Usable => return ProbeOutcome::Usable((*region).to_string()),
            ProbeVerdict::TokenDead => {
                tracing::warn!("token 本身失效（401），停止 region 探测");
                return ProbeOutcome::TokenDead;
            }
            // 风控命中后**仍试下一个候选**：风控是账户级的，但下一个候选完全可能回
            // 2xx/400/429（那就是确定的可用区，直接采纳，比记一个「探不了」有用得多）。
            // 代价是对被风控的账号多打一次 —— 上限 MAX_PROBE_ATTEMPTS=2，可接受。
            ProbeVerdict::AccountThrottled => {
                saw_account_throttled = true;
                continue;
            }
            ProbeVerdict::WrongRegion | ProbeVerdict::Inconclusive => continue,
        }
    }

    // P3-b：一次都没真打出去 ⇒ 这一轮**没有任何关于 region 的信息**，不是「探过不行」。
    // 走到这里只有一种成因：`probe_target` 对每个候选都返 None，即凭据有更高优先级的
    // region 源（profileArn）把区钉在别处 —— 而那与「凭据已带 region 字段」是同一类事实
    // （调用方/上游已经定了区），故与它同判 `Skipped`。
    // 判 `NoUsableRegion` 的后果是**一个完全能用的号在上号时被禁用**，与 `probe_target`
    // 文档里写明的「`None` 是探不了、不是探过不行」直接矛盾。
    if probed == 0 {
        tracing::warn!(
            "region 自动探测一个候选都没能打出去（凭据有更高优先级的 region 源），\
             判 Skipped —— 这是「探不了」，据此禁用会误杀一个能用的号"
        );
        return ProbeOutcome::Skipped;
    }

    // P3-a：见过账户级风控 ⇒ 整轮判决必须与「region 不可用」分开，否则临时态被固化成永久态。
    if saw_account_throttled {
        tracing::warn!(
            probed,
            "region 自动探测被账户级临时风控挡住（403 temporarily suspended，与 region 无关）\
             —— 判 AccountThrottled 而非 NoUsableRegion：据后者禁用会把临时风控固化成\
             需人工处理的永久态，且归因指向错误方向（该查账号风控，不是 region 授权）"
        );
        return ProbeOutcome::AccountThrottled;
    }

    tracing::warn!(
        tried = MAX_PROBE_ATTEMPTS,
        probed,
        "region 自动探测未得出可用结论 —— 该号不应被启用（启用即恒 403 被打死）"
    );
    ProbeOutcome::NoUsableRegion
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ⭐ 承重：**429 必须判 Usable**。
    ///
    /// 回退即 FAIL：把 429 那条分支删掉或移到 403 之后 —— 上游对「region 正确但拥堵」
    /// 的号回 429，判成失败就会跳过一个完全可用的 region、继续探，最坏把所有候选
    /// 都探成失败然后回退 `config.region`，正是本模块要消除的状态。
    #[test]
    fn throttled_means_region_is_correct() {
        for s in [
            "请求过于频繁，已被限流: 429 Too Many Requests {\"__type\":\"com.amazon.kiro.runtimeservice#ThrottlingException\"}",
            "429 Too Many Requests",
            "ThrottlingException",
        ] {
            assert_eq!(
                classify_probe_result(&Err(s.to_string())),
                ProbeVerdict::Usable,
                "429 说明请求到了正确 region，必须采纳: {s}"
            );
        }
    }

    /// 403 = region 不对，这是本模块要抓的信号。
    #[test]
    fn forbidden_means_wrong_region() {
        for s in [
            "权限不足，无法获取使用额度: 403 Forbidden {\"__type\":\"com.amazon.kiro.runtimeservice#AccessDeniedException\"}",
            "403 Forbidden",
            "The bearer token included in the request is invalid.",
        ] {
            assert_eq!(
                classify_probe_result(&Err(s.to_string())),
                ProbeVerdict::WrongRegion,
                "403/bearer-invalid 是 region 错配的签名: {s}"
            );
        }
    }

    /// 探测路径上会看到的**真实**上游 403 suspend 串。
    ///
    /// 形状 = `probe_dialog_endpoint` 的 `format!("{error_msg}: {status} {body_text}")`，
    /// 其中 `error_msg` 是 403 那条中文、`status` 是 reqwest 的 `403 Forbidden`、
    /// body 取自 `endpoint/mod.rs` 与 `handlers.rs` 注释里记录的线上实测原文
    /// （两种书写：message 正文的 `temporarily is suspended` 与 reason 字段的
    /// `TEMPORARILY_SUSPENDED`）。刻意不用合成串 —— 真实链路不产生的串测不出东西。
    const REAL_SUSPEND_BODIES: &[&str] = &[
        "权限不足，该 region 未授权: 403 Forbidden {\"__type\":\"com.amazon.aws.codewhisperer#AccessDeniedException\",\"message\":\"Your User ID (1866...) temporarily is suspended. We've locked your account as a security precaution. To restore access, please contact our support team...\"}",
        "权限不足，该 region 未授权: 403 Forbidden {\"__type\":\"com.amazon.kiro.runtimeservice#AccessDeniedException\",\"message\":\"Your User ID is temporarily suspended. We detected unusual user activity and locked it as a security precaution...\",\"reason\":\"TEMPORARILY_SUSPENDED\"}",
    ];

    /// ⭐ must_fix：403 账户级临时风控**绝不能**判 `WrongRegion`。
    ///
    /// 回退即 FAIL：删掉 `classify_probe_result` 里那道
    /// `default_is_temporary_rate_limit` 分支 —— 两个真实 body 都落 403 分支判
    /// `WrongRegion` ⇒ 两候选耗尽 ⇒ `NoUsableRegion` ⇒ `disabled=true` +
    /// `RegionProbeFailed`（不在自愈白名单）⇒ 临时风控被固化成需人工的永久态。
    ///
    /// 同时钉住**顺序**：这道判据必须在 403 之前。两个 body 都含 `403` 与
    /// `AccessDeniedException`，先判 403 就永远走不到 suspend 分支 —— 那正是本仓
    /// 「测了分支内部、没测分支顺序」这种伪证形态的靶子，故此处断言的是最终结论。
    #[test]
    fn account_suspend_must_not_be_read_as_wrong_region() {
        for body in REAL_SUSPEND_BODIES {
            let verdict = classify_probe_result(&Err(body.to_string()));
            assert_eq!(
                verdict,
                ProbeVerdict::AccountThrottled,
                "403 temporarily suspended 是账户级临时风控（与 region 无关），\
                 判 WrongRegion 会让号被 RegionProbeFailed 永久禁用: {body}"
            );
            assert_ne!(
                verdict,
                ProbeVerdict::WrongRegion,
                "顺序守卫：suspend 判据必须排在 403 之前"
            );
        }
    }

    /// 2026-08-06 线上实测的**原文** body，凭据 `#749`（`ksk_u7Wd…`，已知只在
    /// `eu-central-1` 授权）。这是本轮回归的全部依据，测试夹具刻意用它们而非合成串。
    ///
    /// ```text
    /// GET management.eu-central-1.kiro.dev/getUsageLimits -> HTTP 200
    /// GET management.us-east-1.kiro.dev/getUsageLimits    -> HTTP 403
    ///     {"message":"Invalid token","reason":null}
    /// POST q.eu-central-1.amazonaws.com/ (探测体) -> HTTP 400
    ///     {"__type":"com.amazon.aws.codewhisperer#ValidationException",
    ///      "message":"Improperly formed request.","reason":"REQUEST_BODY_INVALID"}
    /// POST q.us-east-1.amazonaws.com/ (探测体)    -> HTTP 400
    ///     {"__type":"com.amazon.kiro.runtimeservice#ValidationException",
    ///      "message":"Improperly formed request.","reason":"REQUEST_BODY_INVALID"}
    /// ```
    const REAL_UNAUTHORIZED_REGION_BODY: &str = r#"{"message":"Invalid token","reason":null}"#;
    const REAL_MALFORMED_400_BODIES: &[&str] = &[
        r#"{"__type":"com.amazon.aws.codewhisperer#ValidationException","message":"Improperly formed request.","reason":"REQUEST_BODY_INVALID"}"#,
        r#"{"__type":"com.amazon.kiro.runtimeservice#ValidationException","message":"Improperly formed request.","reason":"REQUEST_BODY_INVALID"}"#,
    ];

    /// ⭐⭐ must_fix 本轮回归的核心断言：**未授权区必须被判 `WrongRegion`。**
    ///
    /// # 这条测试为什么必须存在
    ///
    /// 回归能上线的唯一原因是：**此前没有任何测试断言过「未授权区必须被判 WrongRegion」**。
    /// 既有的 `forbidden_means_wrong_region` 用的是合成串与 `q.*` 的文案，
    /// 而真实链路上「未授权」的书写是 `{"message":"Invalid token","reason":null}` ——
    /// 那个文案说 token 无效，但 token 是好的（同一秒打 eu-central-1 拿 200）。
    ///
    /// 两种形态都测：带完整状态码前缀的（真实 prober 产出的形状）与裸 body
    /// （防判据只靠 `"403"` 这个数字、而上游哪天不带状态码文本时整条失效）。
    #[test]
    fn real_unauthorized_region_body_must_be_wrong_region() {
        let full =
            format!("权限不足，该 region 未授权: 403 Forbidden {REAL_UNAUTHORIZED_REGION_BODY}");
        for s in [full.as_str(), REAL_UNAUTHORIZED_REGION_BODY] {
            let verdict = classify_probe_result(&Err(s.to_string()));
            assert_eq!(
                verdict,
                ProbeVerdict::WrongRegion,
                "2026-08-06 实测：#749 打 management.us-east-1 得到这个 403 body，\
                 而该 key 只在 eu-central-1 授权 —— 判不出 WrongRegion 则该区不会被排除，\
                 US 号会被写死成错的区: {s}"
            );
            assert_ne!(
                verdict,
                ProbeVerdict::Usable,
                "承重：未授权区**绝不能**被判可用，那正是本轮回归的形态"
            );
            assert_ne!(
                verdict,
                ProbeVerdict::TokenDead,
                "文案是 Invalid token，但 token 是好的（同一秒 eu-central-1 拿 200）—— \
                 判 TokenDead 会让探测在首个候选就整体放弃，eu 号也一起废"
            );
        }
    }

    /// ⭐⭐ must_fix：`400 REQUEST_BODY_INVALID` **绝不能**判 `Usable`。
    ///
    /// # 这就是回归本身
    ///
    /// 旧判据「400 = 认证已通过 ⇒ region 对」是**推理**，实测证否：同一个只在
    /// `eu-central-1` 授权的 key 打 `q.eu-central-1` 与 `q.us-east-1` **都**回这个 400
    /// （AWS 先校验请求体格式、后校验 region 授权）。而 `PROBE_ORDER[0]` 是 `eu-central-1`
    /// ⇒ 每个 api_key 号首次探测都被判「eu 可用」⇒ US 号的 `api_region` 恒被写成 eu ⇒ 恒 403。
    ///
    /// 走 `management.*` 后这个状态码本不该出现，但判据仍必须覆盖它：**唯一不可接受的
    /// 结果是把坏区判成可用**。判 `Inconclusive`（而非 `WrongRegion`）是因为 400 是格式层
    /// 拒绝、不携带 region 信息。
    ///
    /// 回退即 FAIL：在 `classify_probe_result` 里加一条 `if low.contains("400") { Usable }`，
    /// 或把 prober 的成功判据改回 `status.is_success() || code == 400`（后者由
    /// [`probe_must_not_treat_400_as_success_in_real_prober`] 抓）。
    #[test]
    fn malformed_request_400_must_not_be_read_as_usable() {
        for body in REAL_MALFORMED_400_BODIES {
            let full = format!("region 探测失败: 400 Bad Request {body}");
            for s in [full.as_str(), *body] {
                let verdict = classify_probe_result(&Err(s.to_string()));
                assert_ne!(
                    verdict,
                    ProbeVerdict::Usable,
                    "400 REQUEST_BODY_INVALID 在**授权与未授权两个区都会出现**（2026-08-06 \
                     实测），判它可用等于把首个候选无条件采纳 ⇒ US 号恒 403: {s}"
                );
                assert_eq!(
                    verdict,
                    ProbeVerdict::Inconclusive,
                    "400 不携带 region 信息 ⇒ 无结论（继续试下一个候选，全试完维持禁用）: {s}"
                );
            }
        }
    }

    /// ⭐ 顺序守卫：403 判据必须排在 400 兜底之**前**。
    ///
    /// 本仓「纸面测试」第 8 种形态是「测了分支内部、没测分支顺序」，真实事故：改三处、
    /// 四条测试、三次「回退即 FAILED」全过而修复无效。故本条显式构造一个**同时含
    /// `403` 与无关 `400`** 的响应（requestId 里带 400 是真实可能的），断言它仍判
    /// `WrongRegion` —— 若哪天有人给 400 加一条显式分支并放在 403 之前，
    /// 那个真正的「region 未授权」信号会被吞成「无结论」，坏区不再被排除。
    #[test]
    fn wrong_region_403_wins_over_incidental_400_text() {
        let s = format!(
            "权限不足，该 region 未授权: 403 Forbidden \
             {{\"requestId\":\"a-400-b\",\"message\":\"Invalid token\",\"reason\":null}}"
        );
        assert_eq!(
            classify_probe_result(&Err(s.clone())),
            ProbeVerdict::WrongRegion,
            "403 判据必须排在 400 之前（400 现在落兜底，故天然满足；\
             给 400 加显式分支时务必放在 403 之后）: {s}"
        );
    }

    /// ⭐ 顺序守卫（全表一次过）：把判据顺序当**表**来断言，而不是逐条测分支内部。
    ///
    /// 每一行是「一个真实/可能的响应 → 期望结论」，并在注释里写明它钉的是哪一条相邻
    /// 顺序约束。四条相邻约束（429 > 401 > suspend > 403 > 兜底400）都必须有一行覆盖，
    /// 因为**只测分支内部时，任意两条相邻分支交换位置都可能仍全绿**。
    #[test]
    fn verdict_precedence_table_is_ordered() {
        let suspend_403 = REAL_SUSPEND_BODIES[0];
        let cases: &[(&str, ProbeVerdict, &str)] = &[
            // 429 > 403：429 body 里带无关 "403" 时 429 必须赢（可用区不得被误杀）。
            (
                "请求过于频繁，已被限流: 429 Too Many Requests {\"requestId\":\"x-403-y\"}",
                ProbeVerdict::Usable,
                "429 必须排在 403 之前",
            ),
            // 429 > suspend：复用的 suspend 判据认 "temporary rate" 一类词，
            // 真 429 的 body 若恰好带上，先判 suspend 会把可用区记成「探不了」。
            (
                "请求过于频繁，已被限流: 429 Too Many Requests {\"message\":\"temporary rate limits applied\"}",
                ProbeVerdict::Usable,
                "429 必须排在 suspend 之前",
            ),
            // 401 > suspend：token 废了就是废了，继续探是纯浪费。
            (
                "认证失败，Token 无效或已过期: 401 Unauthorized {\"message\":\"temporarily is suspended\"}",
                ProbeVerdict::TokenDead,
                "401 必须排在 suspend 之前",
            ),
            // suspend > 403：suspend body 里同时有 403 与 AccessDeniedException，
            // 排在 403 之后等于这条分支不存在 ⇒ 风控号被 RegionProbeFailed 永久禁用。
            (
                suspend_403,
                ProbeVerdict::AccountThrottled,
                "suspend 必须排在 403 之前",
            ),
            // 403 > 兜底 400：403 body 里带无关 400 时仍须判 WrongRegion（坏区要被排除）。
            (
                "权限不足，该 region 未授权: 403 Forbidden {\"requestId\":\"a-400-b\",\"message\":\"Invalid token\"}",
                ProbeVerdict::WrongRegion,
                "403 必须排在 400 之前",
            ),
        ];
        for (input, expected, why) in cases {
            assert_eq!(
                classify_probe_result(&Err((*input).to_string())),
                *expected,
                "判据顺序被破坏（{why}）: {input}"
            );
        }

        // 兜底：纯 400 无 region 信息 ⇒ 无结论（**不是** Usable，那是本轮回归）。
        //
        // ⚠️ 必须带状态码前缀。裸 body 里根本没有 `400` 这三个字符
        // （`REQUEST_BODY_INVALID` 不含数字），拿裸 body 测「400 怎么判」是**纸面测试**：
        // 它无论如何都落兜底，把 400 判据改成 `Usable` 也照样绿。
        // 这个坑在本轮的回退验证里真的被抓到过一次。
        let real_400 = format!(
            "region 探测失败: 400 Bad Request {}",
            REAL_MALFORMED_400_BODIES[0]
        );
        assert!(
            real_400.contains("400"),
            "夹具自检：这个串必须真的含 400，否则测不到 400 判据"
        );
        assert_eq!(
            classify_probe_result(&Err(real_400.clone())),
            ProbeVerdict::Inconclusive,
            "400 落兜底 Inconclusive，绝不判 Usable: {real_400}"
        );
    }

    /// 真正的 region 错配（`bearer token ... is invalid`）**不得**被 suspend 判据吃掉。
    ///
    /// 与上一条是对照组：两者是同一个 403 `AccessDeniedException`，只有 body 文案不同。
    /// 若 suspend 判据写宽了（例如只认 `suspended` 或干脆认 `AccessDeniedException`），
    /// 坏区就会被判「探不了」⇒ 号带着错的 `config.region` 回退入池，
    /// 正是本模块存在的那个缺陷。
    #[test]
    fn real_wrong_region_body_is_not_mistaken_for_suspend() {
        let body = "权限不足，该 region 未授权: 403 Forbidden {\"__type\":\"com.amazon.aws.codewhisperer#AccessDeniedException\",\"message\":\"The bearer token included in the request is invalid.\"}";
        assert_eq!(
            classify_probe_result(&Err(body.to_string())),
            ProbeVerdict::WrongRegion,
            "region 未授权的真实 body 必须仍判 WrongRegion —— suspend 判据不能写宽"
        );
    }

    /// 401 = token 废了，继续探其它 region 无意义。
    #[test]
    fn unauthorized_stops_probing() {
        assert_eq!(
            classify_probe_result(&Err("认证失败，Token 无效或已过期: 401 Unauthorized".into())),
            ProbeVerdict::TokenDead
        );
    }

    /// 5xx / 网络 = 无结论，不能据此把 region 判死。
    #[test]
    fn transient_is_inconclusive() {
        for s in [
            "服务器错误，AWS 服务暂时不可用: 500 Internal Server Error",
            "error sending request for url (https://management.eu-central-1.kiro.dev/)",
            "operation timed out",
        ] {
            assert_eq!(
                classify_probe_result(&Err(s.to_string())),
                ProbeVerdict::Inconclusive,
                "瞬态错误不该被当成 region 错配（否则一次网络抖动就把好 region 排除）: {s}"
            );
        }
    }

    #[test]
    fn success_is_usable() {
        assert_eq!(classify_probe_result(&Ok(())), ProbeVerdict::Usable);
    }

    /// ⭐ 同时含 429 与 403 字样时，429 必须赢（顺序守卫）。
    ///
    /// 回退即 FAIL：把 403 判据移到 429 之前 —— 上游 429 的响应体里带上无关的
    /// "403"（如 requestId 片段）就会让一个可用 region 被误杀。
    #[test]
    fn throttling_wins_over_incidental_403_text() {
        let s = "429 Too Many Requests {\"requestId\":\"abc-403-def\",\"__type\":\"ThrottlingException\"}";
        assert_eq!(
            classify_probe_result(&Err(s.to_string())),
            ProbeVerdict::Usable,
            "429 判据必须排在 403 之前"
        );
    }

    /// 探测顺序按实测命中率排，且候选表**只含有实测依据的两个区**。
    ///
    /// # 「上限 == 表长」的理由
    ///
    /// 曾经表里有 5 项而上限 3，于是表尾两项**永远探不到** —— 上限小于表长等于表尾是死项。
    /// 现在锁成表长：加候选一定会被探到。
    ///
    /// # 表长断言为什么改了理由（2026-08-06）
    ///
    /// 旧理由是「其余 13 个区 DNS 不解析」，那是 `management.*`/`runtime.*`（kiro.dev）
    /// 的约束。探测已改打 `q.*.amazonaws.com`，**那条理由对新 host 不成立**。
    /// 现在的理由是成本 + 证据：每个候选是一次串行在用户上号请求里的上游往返，
    /// 而只有这两个区有实测命中（99 / 11 个号），其余零命中。
    /// 要扩表请按 [`PROBE_ORDER`] 文档里的依据门槛来，别只改这个数字。
    #[test]
    fn probe_order_starts_with_measured_winners_and_is_capped() {
        assert_eq!(
            PROBE_ORDER[0], "eu-central-1",
            "实测能用的号 eu-central-1 有 99 个、us-east-1 有 11 个，应先探命中率高的"
        );
        assert_eq!(PROBE_ORDER[1], "us-east-1");
        assert_eq!(
            PROBE_ORDER.len(),
            2,
            "候选表只该有这两个有实测依据的区：每多一项都是一次串行在用户上号请求里的上游往返，\
             加候选必须附实测依据（见 PROBE_ORDER 文档）"
        );
        assert_eq!(
            MAX_PROBE_ATTEMPTS,
            PROBE_ORDER.len(),
            "上限必须覆盖整张表，否则最后一个候选永远探不到 → eu 账号拿不到回退机会"
        );
        // 候选必须全部在合法白名单内，否则会拼出坏 host。
        for r in PROBE_ORDER {
            assert!(
                crate::kiro::regions::KIRO_DIALOG_REGIONS.contains(r),
                "{r} 不在 KIRO_DIALOG_REGIONS 白名单里，会拼出非法 host"
            );
        }
    }

    /// ⭐ P0 回归：「不用探」必须返 [`ProbeOutcome::Skipped`]，**绝不能**与「探失败」同值。
    ///
    /// # 这条测试在防什么
    ///
    /// 旧签名是 `Option<String>`，把**五种语义完全不同**的结果全塌缩成 `None`。
    /// 修复后调用方会据判决**禁用凭据**（`service.rs` 的 `mark_region_probe_failed`），
    /// 于是这个区分从"代码整洁"变成**承重**：
    ///
    /// - 已带 region 字段 / 非 api_key 号 ⇒ 调用方的明确意图，必须照常启用
    /// - 探不到可用 region / token 已废 ⇒ 必须保持禁用
    ///
    /// 若两者仍同值，后果是**把推号方明确指定了 region 的号、以及全部 OAuth 号
    /// 一律禁用** —— 那比原缺陷（15 个号被误禁）严重得多，是全池级别的。
    ///
    /// 把任一 `Skipped` 改回 `NoUsableRegion` → 本测试必 FAILED。
    ///
    /// # 为什么这两条能在单测里跑
    ///
    /// 两条跳过路径都在函数**开头**、任何上游往返之前 return，所以 token / config / order
    /// 全部用不到（传占位值即可）。本仓铁律禁止测试依赖网络，而这正是可测的那部分。
    #[tokio::test]
    async fn skipped_must_be_distinguishable_from_probe_failure() {
        use crate::kiro::model::credentials::KiroCredentials;
        use crate::model::config::Config;
        use std::sync::Arc;

        let cfg = Arc::new(Config::default());

        // ① 已显式带 region（推号方给了 / 运维手填）—— 三个字段各测一次，
        //    因为判据是三者的 or，漏掉任一个都会让那种号被误禁。
        for (name, mutate) in [
            (
                "region",
                (|c: &mut KiroCredentials| c.region = Some("eu-central-1".into()))
                    as fn(&mut KiroCredentials),
            ),
            ("auth_region", |c: &mut KiroCredentials| {
                c.auth_region = Some("eu-central-1".into())
            }),
            ("api_region", |c: &mut KiroCredentials| {
                c.api_region = Some("eu-central-1".into())
            }),
        ] {
            let mut cred = KiroCredentials::default();
            cred.kiro_api_key = Some("ksk_test".into()); // 是 api_key 号，只靠 region 字段跳过
            mutate(&mut cred);
            assert_eq!(
                probe_api_region(&cred, &cfg, "dummy-token", PROBE_ORDER).await,
                ProbeOutcome::Skipped,
                "带 {name} 的号必须判 Skipped（调用方明确意图），判成失败会把它禁用"
            );
        }

        // ② 非 api_key 号（OAuth 走 profileArn，region 由它决定）
        let oauth = KiroCredentials::default(); // 无 kiro_api_key、无 auth_method ⇒ 非 api_key
        assert!(
            !oauth.is_api_key_credential(),
            "前提校验：这个构造必须不是 api_key 号，否则本条测不到目标路径"
        );
        assert_eq!(
            probe_api_region(&oauth, &cfg, "dummy-token", PROBE_ORDER).await,
            ProbeOutcome::Skipped,
            "OAuth 号必须判 Skipped —— 判成失败会把全部 OAuth 号禁用"
        );

        // ③ 承重：`Skipped` 与两种失败必须是不同的值（若枚举被改成同值，上面全部失效）
        assert_ne!(ProbeOutcome::Skipped, ProbeOutcome::NoUsableRegion);
        assert_ne!(ProbeOutcome::Skipped, ProbeOutcome::TokenDead);
    }

    /// ⭐ P0 源码守卫：上号路径必须**按判决**禁用，且分身要继承禁用态。
    ///
    /// 行为测试到不了 `add_credential`（它会调 `get_usage_limits_for`，那是真实上游网络
    /// 往返，本仓铁律禁止测试依赖网络），故用源码断言钉住三件事：
    ///
    /// 1. 判决被**接住**（不是丢弃返回值）—— 丢弃则整个 P0 修复无效
    /// 2. 失败时调 `mark_region_probe_failed`（写可归因的原因，而非停在 `Manual`）
    /// 3. 失败时置 `new_cred.disabled = true` —— 分身继承父号的 `api_region`，父号探不到时
    ///    它们继承到 `None` ⇒ 回退 `config.region` ⇒ 与父号同样恒 403。
    ///    历史事故正是这个形态（父号 #525 探到 eu-central-1 而 4 个分身 `api_region=None`，
    ///    24 秒内全部被禁用）。**必须在入池时就禁用**，不能建完再批量禁用（那有中间窗口）。
    ///
    /// 锚点切掉注释行：本仓踩过五次「needle 命中注释里的散文」。
    #[test]
    fn add_credential_must_act_on_probe_verdict() {
        let src = include_str!("../admin/service.rs");
        let prod: String = src
            .split("#[cfg(test)]")
            .next()
            .expect("生产段应存在")
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");

        // needle 运行时拼接，避免 include_str! 自匹配。
        let capture = ["let probe_outcome = self", "\n            .token_manager"].concat();
        assert!(
            prod.contains(capture.as_str()) || prod.contains("let probe_outcome"),
            "探测判决必须被接住 —— 丢弃返回值则整个 P0 修复无效（号仍以启用态接流量）"
        );
        let mark = ["mark_region_probe_failed", "(credential_id"].concat();
        assert!(
            prod.contains(mark.as_str()),
            "探测失败必须调 mark_region_probe_failed 写可归因原因，\
             否则号停在 Manual 上 —— 运维看到「手动禁用」而没人手动禁过它"
        );
        let inherit = ["new_cred.disabled", " = true"].concat();
        assert!(
            prod.contains(inherit.as_str()),
            "探测失败必须置 new_cred.disabled=true —— 否则分身继承 api_region=None、\
             回退 config.region、与父号同样恒 403（历史事故：4 个分身 24 秒内全部被禁用）"
        );
    }

    /// ⭐ 承重（源码级）：探测**不得**走带 403 换区回退的额度查询，也不得手搓 host。
    ///
    /// # 这条测试上一轮把约束写反了，值得记一笔
    ///
    /// 它此前禁的是**全部** `get_usage_limits*`，理由「那是探 A 决定 B」。实测证明
    /// `management.*` 才是唯一能区分授权的探针（模块文档对照表），于是这条 needle
    /// 把代码锁在了一个**没有区分能力**的实现上 —— 一条本意防回归的断言反过来钉死了回归。
    /// 现在只禁真正有害的那一个：**带 403 换区回退的 `get_usage_limits`**。
    ///
    /// 它为什么有害（与域名无关，是另一个缺陷）：`token_manager::get_usage_limits` 在
    /// `eu-central-1` 拿 403 时会**静默改打** `us-east-1`，成功即 `return Ok(_)`，
    /// 而返回值里**不含「实际生效的是哪个区」**。探测拿这个 `Ok` 当「候选区可用」的判据 ⇒
    /// 一个真实授权在 `us-east-1` 的 key，探 `eu-central-1` → 内部回退成功 →
    /// 把 **`eu-central-1`** 写死进 `api_region` ⇒ 该号此后恒 403，分身还继承这个错值。
    /// 即"US 的 key 添加后显示成 EU"的**另一条**成因，与 400 误判各自独立。
    ///
    /// 手搓 host 同样仍禁：URL 必须来自 `token_manager` 的单一真相
    /// （`usage_limits_probe_url` / `build_usage_limits_request` 共用一个 `(host, url)`），
    /// 否则探测请求与业务请求会静默分形，而判据的有效性完全建立在两者同形之上。
    #[test]
    fn probe_must_not_use_region_falling_back_usage_limits() {
        let src = include_str!("region_probe.rs");
        let prod: String = src
            .split("#[cfg(test)]")
            .next()
            .expect("生产段应存在")
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");

        // needle 运行时拼接，避免 include_str! 自匹配。
        // 只禁**带换区回退**的那个入口；`build_usage_limits_request` /
        // `usage_limits_probe_url` 不含此子串，是刻意的（它们不换区）。
        let falling_back = ["get_usage_limits", "(&client"].concat();
        assert!(
            !prod.contains(falling_back.as_str()),
            "探测不得用带 403 换区回退的 get_usage_limits：它会静默改打另一个区并返回 Ok，\
             把错的区写死进 api_region（US key 显示成 EU 的成因之一）"
        );
        let falling_back2 = ["token_manager::get_usage_limits", "("].concat();
        assert!(
            !prod.contains(falling_back2.as_str()),
            "同上：换区回退版本的额度查询绝不能当 region 判据"
        );
        for host in ["management.", "runtime.", "q."] {
            let literal = ["\"https://", host].concat();
            assert!(
                !prod.contains(literal.as_str()),
                "探测不得手搓 host（发现 {literal}）—— URL 必须来自 token_manager 的单一真相，\
                 否则探测请求与业务请求会静默分形"
            );
        }
        let shared = ["build_usage_limits_request", "("].concat();
        assert!(
            prod.contains(shared.as_str()),
            "探测必须复用业务路径的请求装配（UA/tokentype/profileArn 一字不差），\
             否则换来的可能是另一个状态码，结论对业务路径不成立"
        );
    }

    /// ⭐ 行为回归：探的是**能区分授权的** `management.*`，钉的是 `q.*` —— 两者都必须
    /// 带**当前候选区**。
    ///
    /// # 为什么断言的是这个不对称，而不是「两者相同」
    ///
    /// 上一轮的同名测试断言「探的 host == 将来跑的 host」，看起来更干净，但它把代码锁在
    /// `q.*` 上，而 `q.*` 配不完整请求体时**两个区都回 400**（模块文档实测表）⇒ 探测
    /// 失去全部区分能力。所以这个不对称是**刻意的、有实测支撑的** proxy，测试要钉的是
    /// 「proxy 的两端都指向同一个候选区」，而不是「两端是同一个域名」。
    ///
    /// 三个断言各钉一件事：
    /// 1. 探的是 `management.{候选区}.kiro.dev/getUsageLimits` —— 唯一实测能区分授权的探针；
    /// 2. 结论钉住的对话 URL 是 `q.{候选区}.amazonaws.com` —— `api_region` 的唯一消费者；
    /// 3. 两个 URL 带的都是**候选自己那个 region**，不是 `config.region`。第 3 条防的是
    ///    「候选换了、host 没换」这种最难看出来的形态（那会让两次探测其实打同一个 host、
    ///    结论却记给两个不同的区）—— 故 `cfg.region` 刻意设成第三个区做诱饵。
    #[test]
    fn probe_target_probes_management_and_pins_dialog_region() {
        let mut cfg = Config::default();
        // 诱饵：若哪个 URL 是从 config 回退算的（而非候选），下面的断言会当场抓到。
        cfg.region = "ap-northeast-1".to_string();
        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some("ksk_probe".into());
        assert!(
            cred.is_api_key_credential(),
            "前提：必须是 ksk_ 号，否则端点解析不到 CLI"
        );

        for region in PROBE_ORDER {
            let t = probe_target(&cred, &cfg, region).expect("无更高优先级 region 源，应能构造");
            assert!(
                t.probe_url.starts_with(&format!(
                    "https://management.{region}.kiro.dev/getUsageLimits"
                )),
                "探测必须打 management.{region}（唯一实测能区分 region 授权的探针）\
                 —— 实际打的是 {}；换回 q.* 会让两个区都回 400 ⇒ 零区分能力",
                t.probe_url
            );
            assert_eq!(
                t.dialog_url,
                format!("https://q.{region}.amazonaws.com/"),
                "结论钉住的对话 URL 必须是候选区的 CLI 服务根（api_region 的唯一消费者是 \
                 CliEndpoint::host）"
            );
            assert_eq!(
                t.candidate.api_region.as_deref(),
                Some(*region),
                "候选副本必须把 region 钉在被探的那个区"
            );
            assert_eq!(t.region, *region, "region 字段必须与被探的候选一致");
            assert!(
                !t.probe_url.contains("ap-northeast-1") && !t.dialog_url.contains("ap-northeast-1"),
                "两个 URL 都不得从 config.region 回退算出"
            );
        }
    }

    /// 更高优先级的 region 源（profileArn）存在时不探该候选 —— 否则又是「探 A 决定 B」。
    ///
    /// profileArn 把 region 钉在 `us-west-2`，此时写 `api_region=eu-central-1`
    /// **不生效**（`effective_upstream_region` 里 profileArn 是第一优先），
    /// 于是会打着 us-west-2 的 host 把结论记成 eu-central-1。必须跳过。
    #[test]
    fn probe_target_refuses_when_region_is_pinned_elsewhere() {
        let cfg = Config::default();
        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some("ksk_probe".into());
        cred.profile_arn =
            Some("arn:aws:codewhisperer:us-west-2:123456789012:profile/ABCDEFGH".into());
        assert!(
            probe_target(&cred, &cfg, "eu-central-1").is_none(),
            "profileArn 已把 region 钉在 us-west-2，写 api_region 不生效 —— \
             继续探会打 us-west-2 的 host 却把结论记成 eu-central-1"
        );
    }

    /// ⭐ 循环行为：403 继续试下一个、429 立刻采纳，且**采纳的是被探的那个区**。
    ///
    /// 用假 prober（不走网络）同时断言「顺序」与「归因」：第一个候选回 403
    /// 必须继续，第二个回 429 必须判 Usable 并返回 **第二个** 区。
    /// 若哪天有人把「首个 Usable 即返」改成「首个响应即返」，本条会抓到。
    #[tokio::test]
    async fn probe_tries_next_region_on_403_and_accepts_throttled_one() {
        let cfg = Arc::new(Config::default());
        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some("ksk_probe".into());

        let order = ["eu-central-1", "us-east-1"];
        let seen = std::cell::RefCell::new(Vec::<String>::new());
        let outcome = probe_api_region_with(&cred, &cfg, &order, |t| {
            seen.borrow_mut().push(t.probe_url.clone());
            let n = seen.borrow().len();
            async move {
                if n == 1 {
                    Err("权限不足，该 region 未授权: 403 Forbidden".to_string())
                } else {
                    Err("请求过于频繁，已被限流: 429 Too Many Requests".to_string())
                }
            }
        })
        .await;

        assert_eq!(
            outcome,
            ProbeOutcome::Usable("us-east-1".to_string()),
            "第一个候选 403 应继续试第二个，第二个 429 说明打到了正确的区 ⇒ 采纳 us-east-1"
        );
        let urls = seen.borrow();
        assert_eq!(urls.len(), 2, "两个候选都该被探到");
        for (i, region) in order.iter().enumerate() {
            assert!(
                urls[i].starts_with(&format!("https://management.{region}.kiro.dev/")),
                "第 {} 次探测必须打候选区 {region} 自己的探针（顺序也必须按 order），实际 {}",
                i + 1,
                urls[i]
            );
        }
    }

    /// ⭐⭐ must_fix 端到端：**只在 `us-east-1` 授权的号必须探出 `us-east-1`。**
    ///
    /// 这是用户提的那句「确保 us 号导入进来跑的效果跟 eu 一样」的直接编码。夹具用
    /// 2026-08-06 实测的真实 body：未授权区回 `403 {"message":"Invalid token","reason":null}`、
    /// 授权区回 200。
    ///
    /// # 回归时它为什么会 FAIL
    ///
    /// 回归实现探 `q.*` 且判 `400 => Usable`，而 `q.*` 对两个区都回 400 ⇒ 首个候选
    /// `eu-central-1` 立刻被判可用 ⇒ 返回 `Usable("eu-central-1")`，与本条期望相反。
    /// 用 `assert_ne!` 显式钉住那个错值，让失败信息直接指向回归形态。
    #[tokio::test]
    async fn us_only_key_must_resolve_to_us_east_1() {
        let cfg = Arc::new(Config::default());
        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some("ksk_us_only".into());

        let seen = std::cell::RefCell::new(Vec::<String>::new());
        let outcome = probe_api_region_with(&cred, &cfg, PROBE_ORDER, |t| {
            seen.borrow_mut().push(t.region.clone());
            let is_authorized_region = t.region == "us-east-1";
            async move {
                if is_authorized_region {
                    Ok(()) // 实测：授权区 HTTP 200
                } else {
                    // 实测原文：未授权区 HTTP 403
                    Err(format!(
                        "权限不足，该 region 未授权: 403 Forbidden {REAL_UNAUTHORIZED_REGION_BODY}"
                    ))
                }
            }
        })
        .await;

        assert_eq!(
            outcome,
            ProbeOutcome::Usable("us-east-1".to_string()),
            "只在 us-east-1 授权的号必须探出 us-east-1 —— 探到的是 {outcome:?}"
        );
        assert_ne!(
            outcome,
            ProbeOutcome::Usable("eu-central-1".to_string()),
            "回归形态：首个候选被无条件判可用（400 恒判 Usable / 判据失去区分能力）\
             ⇒ US 号的 api_region 被写成 eu-central-1 ⇒ 该号恒 403"
        );
        assert_eq!(
            *seen.borrow(),
            vec!["eu-central-1".to_string(), "us-east-1".to_string()],
            "必须真的按 PROBE_ORDER 逐个探过来（首个候选未授权 ⇒ 继续试第二个）"
        );
    }

    /// 对照组：只在 `eu-central-1` 授权的号仍必须探出 `eu-central-1`，且**只打一次**。
    ///
    /// 与上一条成对存在。单有 US 那条时，一个「恒返回 `PROBE_ORDER` 末项」的坏实现
    /// 也能让它通过 —— 两条一起才钉住「结论跟着授权走」而不是跟着位置走。
    #[tokio::test]
    async fn eu_only_key_must_resolve_to_eu_central_1_in_one_roundtrip() {
        let cfg = Arc::new(Config::default());
        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some("ksk_eu_only".into());

        let calls = std::cell::Cell::new(0usize);
        let outcome = probe_api_region_with(&cred, &cfg, PROBE_ORDER, |t| {
            calls.set(calls.get() + 1);
            let is_authorized_region = t.region == "eu-central-1";
            async move {
                if is_authorized_region {
                    Ok(())
                } else {
                    Err(format!(
                        "权限不足，该 region 未授权: 403 Forbidden {REAL_UNAUTHORIZED_REGION_BODY}"
                    ))
                }
            }
        })
        .await;

        assert_eq!(outcome, ProbeOutcome::Usable("eu-central-1".to_string()));
        assert_eq!(
            calls.get(),
            1,
            "首个候选就授权 ⇒ 只该有一次上游往返（上号是用户交互路径，多一次都是白等）"
        );
    }

    /// 401 必须**立刻停**：继续探其它 region 是纯浪费的上游往返。
    #[tokio::test]
    async fn probe_stops_at_first_401() {
        let cfg = Arc::new(Config::default());
        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some("ksk_probe".into());

        let order = ["eu-central-1", "us-east-1"];
        let calls = std::cell::Cell::new(0usize);
        let outcome = probe_api_region_with(&cred, &cfg, &order, |_t| {
            calls.set(calls.get() + 1);
            async { Err("认证失败，Token 无效或已过期: 401 Unauthorized".to_string()) }
        })
        .await;

        assert_eq!(outcome, ProbeOutcome::TokenDead);
        assert_eq!(calls.get(), 1, "401 之后不该再打第二个候选");
    }

    /// 全候选 403 ⇒ `NoUsableRegion`（调用方据此**保持禁用**）。
    ///
    /// 这条与上面两条一起钉住「探不出可用 region 就维持禁用」：实测事故是 15 个号
    /// 以启用态入池、打到错区恒 403、3 次即自动禁用、每个只跑 1~6 请求 0 成功。
    #[tokio::test]
    async fn probe_all_403_yields_no_usable_region() {
        let cfg = Arc::new(Config::default());
        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some("ksk_probe".into());

        let order = ["eu-central-1", "us-east-1"];
        let calls = std::cell::Cell::new(0usize);
        let outcome = probe_api_region_with(&cred, &cfg, &order, |_t| {
            calls.set(calls.get() + 1);
            async { Err("权限不足，该 region 未授权: 403 Forbidden".to_string()) }
        })
        .await;

        assert_eq!(outcome, ProbeOutcome::NoUsableRegion);
        assert_eq!(calls.get(), order.len(), "两个候选都该被探到");
    }

    /// ⭐ must_fix 整轮汇总：全候选 403 suspend ⇒ `AccountThrottled`，**不是** `NoUsableRegion`。
    ///
    /// 这是本条修复的承重断言。单看 `classify_probe_result` 判对了还不够 ——
    /// 只要整轮仍汇总成 `NoUsableRegion`，`service.rs` 的
    /// `matches!(probe_outcome, NoUsableRegion | TokenDead)` 依旧命中 ⇒
    /// `mark_region_probe_failed` 置 `disabled=true` + `RegionProbeFailed` ⇒
    /// 而该原因不在 `is_self_healable_reason` 白名单 ⇒ 号永久出局，还连带分身。
    ///
    /// 回退即 FAIL：把循环后那段 `if saw_account_throttled` 删掉（或把
    /// `ProbeVerdict::AccountThrottled` 的分支并回 `WrongRegion | Inconclusive`）。
    #[tokio::test]
    async fn all_candidates_suspended_yields_account_throttled_not_region_failure() {
        let cfg = Arc::new(Config::default());
        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some("ksk_probe".into());

        let order = ["eu-central-1", "us-east-1"];
        let calls = std::cell::Cell::new(0usize);
        let outcome = probe_api_region_with(&cred, &cfg, &order, |_t| {
            let n = calls.get();
            calls.set(n + 1);
            async move { Err(REAL_SUSPEND_BODIES[n.min(REAL_SUSPEND_BODIES.len() - 1)].to_string()) }
        })
        .await;

        assert_eq!(
            outcome,
            ProbeOutcome::AccountThrottled,
            "账户级临时风控挡住探测时必须判 AccountThrottled —— 判 NoUsableRegion 会让\
             service.rs 把号禁用成 RegionProbeFailed（不可自愈），把临时态固化成永久态"
        );
        assert_ne!(
            outcome,
            ProbeOutcome::NoUsableRegion,
            "承重：这两个判决在调用方处的处置相反（不禁用 / 禁用）"
        );
        assert_eq!(calls.get(), order.len(), "风控命中后仍应试下一个候选");
    }

    /// 风控只挡住第一个候选、第二个探到可用区 ⇒ 仍必须采纳该区（风控不该盖掉真结论）。
    ///
    /// 防的是「见过一次 suspend 就整轮短路成 AccountThrottled」这种过度修复：
    /// 那会让一个**已经探到可用区**的号拿不到 `api_region`，白白退回 `config.region` 轮盘。
    #[tokio::test]
    async fn usable_region_wins_over_earlier_suspend() {
        let cfg = Arc::new(Config::default());
        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some("ksk_probe".into());

        let order = ["eu-central-1", "us-east-1"];
        let calls = std::cell::Cell::new(0usize);
        let outcome = probe_api_region_with(&cred, &cfg, &order, |_t| {
            calls.set(calls.get() + 1);
            let first = calls.get() == 1;
            async move {
                if first {
                    Err(REAL_SUSPEND_BODIES[0].to_string())
                } else {
                    Ok(())
                }
            }
        })
        .await;

        assert_eq!(
            outcome,
            ProbeOutcome::Usable("us-east-1".to_string()),
            "第二个候选可用时必须采纳它 —— 先前的 suspend 只说明「那次没探出信息」"
        );
    }

    /// 401 必须压过 suspend：token 废了就是废了，继续探毫无意义。
    ///
    /// 顺序守卫的另一半 —— suspend 判据插在 401 **之后**、403 之前。若插到 401 之前，
    /// 一个 body 里同时提 401 与 suspend 的响应会被判成「可重探」，白耗后续往返。
    #[test]
    fn token_dead_still_wins_over_suspend_text() {
        let body = "认证失败，Token 无效或已过期: 401 Unauthorized {\"message\":\"Your User ID temporarily is suspended.\"}";
        assert_eq!(
            classify_probe_result(&Err(body.to_string())),
            ProbeVerdict::TokenDead,
            "401 必须压过 suspend 文案"
        );
    }

    /// ⭐ P3-b：一个候选都没真打出去 ⇒ `Skipped`，**不是** `NoUsableRegion`。
    ///
    /// `probe_target` 的文档写明「`None` 是探不了，不是探过不行」，但整轮汇总此前无条件落
    /// `NoUsableRegion` ⇒ 一个**完全能用**的号在上号时被 `RegionProbeFailed` 禁用。
    ///
    /// 夹具走的是真实成因：凭据带 profileArn 把 region 钉在 `us-west-2`（不在 `PROBE_ORDER`
    /// 内），于是每个候选的 `effective_upstream_region` 都 != 候选区 ⇒ 全部返 None。
    ///
    /// 回退即 FAIL：删掉循环后那段 `if probed == 0`。
    #[tokio::test]
    async fn no_probeable_candidate_is_skipped_not_region_failure() {
        let cfg = Arc::new(Config::default());
        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some("ksk_probe".into());
        cred.profile_arn =
            Some("arn:aws:codewhisperer:us-west-2:123456789012:profile/ABCDEFGH".into());

        let calls = std::cell::Cell::new(0usize);
        let outcome = probe_api_region_with(&cred, &cfg, PROBE_ORDER, |_t| {
            calls.set(calls.get() + 1);
            async { Ok(()) }
        })
        .await;

        assert_eq!(
            calls.get(),
            0,
            "前提：所有候选都不可探，prober 一次都不该被调用"
        );
        assert_eq!(
            outcome,
            ProbeOutcome::Skipped,
            "全候选不可探是「探不了」 —— 判 NoUsableRegion 会把一个能用的号禁用掉"
        );
    }

    /// prober 判可用时，采纳的**必须**是当前候选区（归因不漂）。
    ///
    /// ⚠️ 本条只覆盖「prober 说行 ⇒ 采纳该候选」这半段（注入的是 `Ok(())`）。
    /// 「哪些 HTTP 状态码才配得到 `Ok`」在网络里，由源码守卫
    /// [`probe_must_not_treat_400_as_success_in_real_prober`] 钉住 —— 那条现在禁的正是
    /// 曾经把 400 也算成 `Ok` 的特判。
    #[tokio::test]
    async fn probe_usable_verdict_credits_the_probed_region() {
        let cfg = Arc::new(Config::default());
        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some("ksk_probe".into());

        let order = ["eu-central-1"];
        let outcome = probe_api_region_with(&cred, &cfg, &order, |_t| async { Ok(()) }).await;
        assert_eq!(
            outcome,
            ProbeOutcome::Usable("eu-central-1".to_string()),
            "prober 判可用时必须采纳**被探的那个候选区**，不能记成 order 里别的区"
        );
    }

    /// ⭐ must_fix 源码守卫：真实 prober **只能**把 2xx 判可用，绝不能把 400 也放进去。
    ///
    /// # 这条测试是上一条的反面，而上一条正是回归本身
    ///
    /// 此前这里断言的是 `status.is_success() || code == 400` **必须存在**，理由「400 是
    /// 预期响应，判它失败会把全部新 ksk_ 号禁用」。实测证否（模块文档对照表）：那个
    /// `|| code == 400` 让**每个** api_key 号都在首个候选 `eu-central-1` 上被判可用，
    /// 不管它真实授权在哪 ⇒ US 号的 `api_region` 恒被写成 `eu-central-1` ⇒ 恒 403。
    ///
    /// 所以现在反过来钉：`code == 400` 这类特判**不得出现**在 prober 的成功判据里。
    /// 回退即 FAIL —— 把 `|| code == 400` 加回去，本条立刻挂。
    ///
    /// 用源码断言而非行为测试：HTTP 状态码到 `Result` 的那一步在网络里，本仓铁律禁止
    /// 测试依赖网络。判据表那一层（400 文案 ⇒ 不是 Usable）另有行为测试
    /// [`malformed_request_400_must_not_be_read_as_usable`]。
    #[test]
    fn probe_must_not_treat_400_as_success_in_real_prober() {
        let src = include_str!("region_probe.rs");
        let prod: String = src
            .split("#[cfg(test)]")
            .next()
            .expect("生产段应存在")
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");

        // needle 运行时拼接，避免 include_str! 自匹配。
        for needle in [
            ["code =", "= 400"].concat(),
            ["as_u16() =", "= 400"].concat(),
        ] {
            assert!(
                !prod.contains(needle.as_str()),
                "prober 不得把 400 特判成成功（发现 {needle}）：实测同一个只在 eu-central-1 \
                 授权的 key 在两个区都拿 400（AWS 先校验 body 格式、后校验授权），\
                 该特判会让每个 api_key 号都被判「首个候选可用」⇒ US 号恒 403"
            );
        }
        // 正向：成功判据必须只有 2xx 这一条。
        assert!(
            prod.contains("if status.is_success() {"),
            "prober 的成功判据必须只认 2xx"
        );
    }
}
