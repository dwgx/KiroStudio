//! 上游 trace 埋点（P0-A）
//!
//! ## 为什么需要它
//!
//! 排障史上所有失败的共同点是：**上游原始响应体在网关做出判断之后就被丢弃了**。
//! 日志里只剩网关自己的二次判断（"疑似 region 错配"、"账号级风控"），于是这几个
//! 问题永远答不了：
//!   - 上游到底给没给 `Retry-After`？值多少？
//!   - 同一把 key 在 `us-east-1` 与 `eu-central-1` 的响应差异到底是什么？
//!   - 429 body 里有没有配额剩余量 / 重置时刻字段？
//!   - `reasoningContentEvent` 的原始形状（有没有 `signature`）？
//!
//! 本模块把「上游原始响应」与「网关内部判断」（选了哪个号 / 哪个 region / 第几次重试 /
//! 第几吸收轮 / 哪条分支接住了它）写进**同一条 JSONL 记录**。这是它相对 mitmproxy
//! 方案的唯一优势：外部抓包看得见响应但看不见网关的选号与分支决策，而且改 TLS 指纹
//! 本身可能影响上游风控 ⇒ 污染实验。
//!
//! ## 线程模型：复制 `usage::pipeline` 的范式，而不是挂进它
//!
//! 范式本身照抄（专用 `std::thread` + 有界 `SyncSender::try_send`，满则丢弃并计数），
//! 理由与那边逐字相同：sink 做同步阻塞 IO（`writeln!` + 文件轮转 + `metadata()`），
//! 跑在 tokio worker 上会让慢盘抖动侵蚀线程池、把延迟传导回请求路径。
//!
//! **但刻意不复用 `usage::pipeline` 本身**，三条理由：
//!   1. `UsageSink::on_record(&RequestRecord)` 入参类型是固定的数据契约，
//!      trace 的字段（原始 body / Retry-After 原值 / 分支名）塞不进去，硬塞就是
//!      污染一个被 SQLite + JSONL + 内存聚合三个 sink 共用的结构。
//!   2. 生命周期不同：usage 默认**开**，trace 默认**关**。共用一条通道时
//!      「trace 关着」也要付 usage 的初始化代价，反之 trace 打开会挤占 usage 的额度。
//!   3. usage 那条 10000 容量的通道已被正常流量占满（实测 1214 请求 / 20 分钟）。
//!      trace 是排障期临时开启的高频写，挤进去会让**用量统计**开始丢记录 ——
//!      用一个诊断工具搞坏一个生产度量是最坏的交换。
//!
//! ## 硬红线：脱敏
//!
//! `token` / `kiroApiKey` / `refreshToken` / `Authorization` 头**绝不进 trace**。
//! 请求体整体不落盘（含用户 prompt）。响应 body 只留前 [`BODY_MAX_BYTES`]，
//! 且必须过 [`redact`]。见该函数的文档注释。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;

use serde::{Deserialize, Serialize};

/// 有界通道容量：约 4 千条积压，超出则丢弃并计数。
///
/// 比 usage 的 10000 小是刻意的：trace 是诊断用途，积压 4000 条已经说明盘写不过来，
/// 再攒下去只是让内存背黑锅（每条记录含最多 2KiB body ⇒ 4000 条 ≈ 8MiB 上界）。
const CHANNEL_CAPACITY: usize = 4096;

/// 响应 body 落盘上限（字节）。**先截断再脱敏**，见 [`redact`]。
pub const BODY_MAX_BYTES: usize = 2 * 1024;

/// 轮转时保留的历史文件数（不含当前文件）。
///
/// 超上限时**轮转**而非覆盖：覆盖写会让历史趋势永远拿不到（本仓 ops 侧刚踩过）。
/// 但也不能无限留 —— 本仓有过日志打满磁盘的事故 ⇒ 折中为「保留最近 N 份」，
/// 磁盘占用上界 = `max_bytes * (KEEP_ROTATED + 1)`，是个可算的有限数。
const KEEP_ROTATED: usize = 3;

/// 打码后的替换文案（承重：验收脚本按它判断脱敏是否生效）。
const REDACTED: &str = "[REDACTED]";

/// 一条上游 trace 记录。
///
/// 字段集是按「必须能回答那四个问题」倒推的，不是有什么记什么：
///   - `retry_after_header` 直接回答「上游给没给 Retry-After」（`None` = 没给，
///     与「给了但解析失败」区分：后者会落进 `retry_after_raw`）
///   - `url` + `region` + `credential_id` 回答「同一把 key 在两个区的差异」
///   - `body` 回答「429 body 里有没有配额字段」与「reasoningContentEvent 的形状」
///   - `verdict` 回答「网关拿它做了什么判断」—— 这是外部抓包拿不到的那一半
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamTrace {
    /// UTC 时间戳（RFC3339，毫秒精度）
    pub ts: String,
    /// 凭据 id（不是 token，可安全落盘）
    pub credential_id: u64,
    /// 端点实现名（`KiroEndpoint::name()`，如 `ide` / `cli`）
    pub endpoint: String,
    /// **实际**请求 URL（含 region 的那个 host —— 排障的核心事实）
    pub url: String,
    /// 本次请求真正生效的上游 region（`effective_upstream_region`）
    pub region: String,
    /// 请求的模型（可能为 None）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// for 循环内的第几次尝试（0 基）
    pub attempt: u32,
    /// 第几个吸收轮（0 基）
    pub absorb_round: u32,
    /// 本请求累计已打上游次数（含本次）
    pub upstream_calls: u32,
    /// HTTP 状态码。网络错误时为 `None`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// 响应头 `Retry-After` 的**原始字符串**（未解析）。没有该头时为 `None`。
    ///
    /// 刻意存原值而非解析后的 u64：`provider.rs` 只 `parse::<u64>()`，
    /// HTTP-date 形式的 Retry-After（RFC 7231 允许）会被静默丢成 `None` ——
    /// 那正是「上游到底给没给」这个问题被答错的方式。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_raw: Option<String>,
    /// 网关解析出的 Retry-After 秒数（`None` = 头缺失或解析失败）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_secs: Option<u64>,
    /// 响应 body，**已截断至 [`BODY_MAX_BYTES`] 且已脱敏**。成功响应为 `None`
    /// （成功走流式，body 是对话内容，含用户数据，绝不落盘）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// 网络错误文案（`reqwest::Error` 的 Display，已脱敏）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network_error: Option<String>,
    /// 从请求开始到此刻的耗时
    pub latency_ms: u64,
    /// 网关最终判断：哪条分支接住了它。
    ///
    /// `unclassified` 有独立含义：说明这条响应走到了一条**没有埋标签的**分支
    /// ⇒ 要么是新加的分支忘了标，要么是落到了函数末尾的兜底。两者都值得看见。
    pub verdict: String,
    /// 是否已成功过（`has_ever_succeeded`）。承重：bearer-invalid 403 的两条分支
    /// 吃的是**逐字节相同**的上游文案，唯一区分位就是这个 bool。
    /// 没有它，trace 里那两类 403 无法区分。
    pub cred_ever_succeeded: bool,
}

impl UpstreamTrace {
    /// 从已落盘的（已脱敏、已截断的）body 里解析上游账号身份。
    ///
    /// ## 为什么是**派生**而不是一个需要调用方填的字段
    ///
    /// `provider.rs` 有 **6 处** `UpstreamTrace { .. }` 结构体字面量
    /// （网络错误 / 成功 / 失败守卫 ×2 组路径）。加一个必填字段要改 6 处，
    /// 而那个文件此刻**归另一批 agent 所有**，且本仓 §「文件独占」那条规矩正是
    /// 从「并发改同一文件」的历史事故来的。
    ///
    /// 更重要的是：即便能改，「6 个构造点各填一次」正是本仓记录的**主导缺陷形态**
    /// （同一逻辑各写一份 → 改一处漏一处：`update.rs` 的 chunked 缺口、
    /// #6 的 `trust_forwarded_header` 都是这么来的）。`FailureTraceGuard` 的文档注释
    /// 已经为同一理由做过一次这个选择。
    ///
    /// ⇒ 身份从 `body` **单点派生**，新增构造点**不可能忘填**。
    ///
    /// ## 成本落在 writer 线程，不在热路径
    ///
    /// 调用点是 [`Writer::write`]（专用 OS 线程）。`emit` 只做 `try_send`，
    /// 请求路径不付这次扫描的钱。
    ///
    /// ## 只看 body，不看 network_error
    ///
    /// 身份来自**上游应用层错误体**；`network_error` 是 `reqwest::Error` 的 Display
    /// （连接层），不含账号信息。多扫一遍只会增加误命中面。
    pub fn derive_upstream_user_id(&self) -> Option<String> {
        crate::kiro::user_id::parse_upstream_user_id(self.body.as_deref()?)
    }
}

/// 落盘形态 = [`UpstreamTrace`] 全部字段 + 派生的 `upstream_user_id`。
///
/// 用 `flatten` 包一层而**不是**给 `UpstreamTrace` 加字段，理由见
/// [`UpstreamTrace::derive_upstream_user_id`]。JSONL 里两者长得一样（同一层平铺），
/// 离线脚本不需要知道这层包装。
///
/// ⚠️ **User ID 不是密钥，可以落盘** —— 它是账号标识，上游自己在错误体里明文给的，
/// 而且这条 trace 里本来就有整段（已脱敏的）错误体。落它不扩大暴露面，
/// 只是把已在文件里的事实变成**可聚合的一列**。
#[derive(Serialize)]
struct TraceRecord<'a> {
    #[serde(flatten)]
    base: &'a UpstreamTrace,
    /// 上游账号身份。`None` = body 缺失 / 无锚点 / 锚点在但 ID 不可信
    /// （三态区分见 [`crate::kiro::user_id::UserIdSignal`]，落盘只留「有/无」）。
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream_user_id: Option<String>,
}

/// 未分类判据（承重字面量：验收脚本按它找漏标的分支）
pub const VERDICT_UNCLASSIFIED: &str = "unclassified";

struct Pipeline {
    tx: mpsc::SyncSender<UpstreamTrace>,
}

static PIPELINE: OnceLock<Pipeline> = OnceLock::new();
static DROPPED: AtomicU64 = AtomicU64::new(0);
static WRITTEN: AtomicU64 = AtomicU64::new(0);
/// 进程级开关镜像（TIER3 热重载范式，与 `anthropic::set_trust_forwarded_header` 同款）。
///
/// 为什么要镜像而不是每次读 `config()`：埋点在**热路径**上，`token_manager.config()`
/// 是一次 ArcSwap load + Arc clone。开关关着的时候（生产常态）必须是一次
/// `Relaxed` 原子读就短路掉，否则「默认关」这个承诺是假的。
static ENABLED: AtomicBool = AtomicBool::new(false);

/// 设置埋点开关（热重载入口）。
pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

/// 埋点是否启用（热路径判据，一次 Relaxed 原子读）。
pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// 因通道满而丢弃的记录数。
pub fn dropped_count() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}

/// 已成功写盘的记录数。
pub fn written_count() -> u64 {
    WRITTEN.load(Ordering::Relaxed)
}

/// 按**字节**预算截断，且保证落在 UTF-8 字符边界上。
///
/// 为什么不用 `chars().take(n)`：那是**字符**预算，而上限是给磁盘/内存定的，
/// 单位必须是字节。CJK body 用字符预算会实际写 3 倍字节（本仓 `map_tool_name`
/// 就是被同一个字节/字符混用的缺陷咬过：30 个汉字 = 90 字节）。
fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// 一个字符是否可能是 secret 的组成部分（base64url / JWT / `ksk_` 的字符集）。
fn is_secret_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '='
}

/// 脱敏：把疑似 secret 的串换成 [`REDACTED`]。
///
/// **判据是刻意「宽进严出」的** —— 宁可多打码几个无害串，也不能漏一个 token。
/// 三类前缀：
///   - `ksk_` —— Kiro API Key（`is_api_key_credential` 的判据就是它）
///   - `Bearer ` —— Authorization 头值形态（body 里的回显也算）
///   - `eyJ` —— base64url 编码的 `{"` ⇒ JWT header 的必然起始（access/refresh token）
///
/// ⚠️ **调用顺序是「先截断，再脱敏」，理由是有界扫描开销，不是安全性。**
///
/// 这条注释此前写的是「反过来会让截断切一半的 token 前半段留明文」——
/// **那是错的，实测证否**：`ksk_` / `eyJ` / `Bearer ` 都是**前缀**判据，
/// 无论先截还是先脱，跨边界的 secret 前半段照样命中并被打码。把两种顺序各跑一遍
/// 现有脱敏测试，**15 条全部通过** ⇒ 安全性上两者等价。
///
/// 真正成立的理由只有一条：`redact` 是逐字符扫描，先截断把它的输入**钉死在
/// [`BODY_MAX_BYTES`]**。反过来则要扫完整个 body —— 而上游 body 可以是 MiB 级
/// （本仓 `compression` 那套就是为了对付 5MiB 请求体存在的）⇒ 一条大错误响应
/// 会在**请求路径上**做一次 MiB 级扫描。这与「不阻塞热路径」的承诺直接冲突。
///
/// ⇒ 记这条的教训与本仓 CLAUDE.md 那条同源：**更正一条断言时要像验证原断言那样
/// 验证你的更正**。上面那个错误理由是我自己在写这个函数时凭直觉编的，
/// 回退验证（把顺序改反 → 期待 FAIL）当场证否了它。
///
/// 不用 `regex`：本仓无该依赖，且 CLAUDE.md 明确「不引入新库」。
/// 纯前缀扫描对这三类判据足够，且没有灾难性回溯的风险面。
pub fn redact(input: &str) -> String {
    const PREFIXES: [&str; 3] = ["ksk_", "Bearer ", "eyJ"];
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0usize;
    'outer: while i < bytes.len() {
        for p in PREFIXES {
            if input.is_char_boundary(i) && input[i..].starts_with(p) {
                // 吞掉前缀本身 + 紧随其后的整段 secret 字符
                let mut j = i + p.len();
                while j < bytes.len() {
                    // 逐 char 前进，保证不切断多字节字符
                    let Some(c) = input[j..].chars().next() else {
                        break;
                    };
                    if is_secret_char(c) {
                        j += c.len_utf8();
                    } else {
                        break;
                    }
                }
                out.push_str(REDACTED);
                i = j;
                continue 'outer;
            }
        }
        // 非命中：原样搬一个 char（不是一个 byte —— 按 byte 搬会切断 UTF-8）
        let c = input[i..].chars().next().unwrap_or('\u{FFFD}');
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// 组装一条可安全落盘的 body 字段：**先截断到 [`BODY_MAX_BYTES`]，再脱敏**。
pub fn sanitize_body(body: &str) -> String {
    redact(truncate_utf8(body, BODY_MAX_BYTES))
}

/// 后台 writer：JSONL 追加写 + 超上限轮转。
struct Writer {
    path: PathBuf,
    max_bytes: u64,
}

impl Writer {
    /// 轮转：`upstream_trace.jsonl` → `.1` → `.2` → …，最旧的被删。
    ///
    /// **绝不截断/覆盖当前文件**（那会让历史趋势永远拿不到）。磁盘占用有界：
    /// `max_bytes * (KEEP_ROTATED + 1)`。
    fn rotate(&self) {
        // 从最旧往回搬，避免覆盖：.3 删掉 → .2 变 .3 → .1 变 .2 → 当前变 .1
        let suffixed = |n: usize| -> PathBuf {
            let mut p = self.path.clone().into_os_string();
            p.push(format!(".{n}"));
            PathBuf::from(p)
        };
        let oldest = suffixed(KEEP_ROTATED);
        if oldest.exists() {
            if let Err(e) = fs::remove_file(&oldest) {
                tracing::warn!("上游 trace 轮转：删除最旧文件 {:?} 失败：{e}", oldest);
            }
        }
        for n in (1..KEEP_ROTATED).rev() {
            let from = suffixed(n);
            if from.exists() {
                let to = suffixed(n + 1);
                if let Err(e) = fs::rename(&from, &to) {
                    tracing::warn!("上游 trace 轮转：{:?} → {:?} 失败：{e}", from, to);
                }
            }
        }
        if self.path.exists() {
            let to = suffixed(1);
            if let Err(e) = fs::rename(&self.path, &to) {
                tracing::warn!("上游 trace 轮转：{:?} → {:?} 失败：{e}", self.path, to);
            }
        }
    }

    fn write(&self, trace: &UpstreamTrace) {
        // 身份在**这里**派生（writer 线程），不在 emit 处 —— 请求路径不付扫描的钱。
        let record = TraceRecord {
            base: trace,
            upstream_user_id: trace.derive_upstream_user_id(),
        };
        let line = match serde_json::to_string(&record) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!("上游 trace 序列化失败（丢弃该条）：{e}");
                return;
            }
        };
        if let Some(dir) = self.path.parent() {
            if !dir.as_os_str().is_empty() && !dir.exists() {
                if let Err(e) = fs::create_dir_all(dir) {
                    tracing::warn!("上游 trace 目录 {:?} 创建失败：{e}", dir);
                    return;
                }
            }
        }
        // 轮转判据放在**写之前**：写完再判会让单文件恒定超上限一行，
        // 而「一行」在 body 2KiB 时不是可忽略的量。
        if let Ok(meta) = fs::metadata(&self.path) {
            if meta.len().saturating_add(line.len() as u64 + 1) > self.max_bytes {
                self.rotate();
            }
        }
        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(mut f) => {
                if let Err(e) = writeln!(f, "{line}") {
                    tracing::warn!("上游 trace 写入失败：{e}");
                } else {
                    WRITTEN.fetch_add(1, Ordering::Relaxed);
                }
            }
            Err(e) => tracing::warn!("上游 trace 文件 {:?} 打开失败：{e}", self.path),
        }
    }
}

/// 初始化 trace 管道并启动后台 writer 线程。
///
/// 幂等：重复调用被忽略（沿用 `usage::pipeline::init` 的语义）。
/// `enabled=false` 时**也建管道**但把开关置 false —— 这样面板热改开关能立即生效
/// 而不需要重启（管道本身空转时零成本：`emit` 在 [`is_enabled`] 处就短路了）。
pub fn init(path: impl AsRef<Path>, max_bytes: u64, enabled: bool) {
    set_enabled(enabled);
    let writer = Writer {
        path: path.as_ref().to_path_buf(),
        max_bytes,
    };
    let (tx, rx) = mpsc::sync_channel::<UpstreamTrace>(CHANNEL_CAPACITY);
    if PIPELINE.set(Pipeline { tx }).is_err() {
        tracing::warn!("上游 trace 管道已初始化，忽略重复初始化");
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("upstream-trace".into())
        .spawn(move || {
            tracing::info!(
                path = ?writer.path,
                max_bytes = writer.max_bytes,
                "上游 trace writer 启动"
            );
            while let Ok(trace) = rx.recv() {
                writer.write(&trace);
            }
            tracing::info!("上游 trace writer 退出（通道关闭）");
        });
    if let Err(e) = spawned {
        tracing::error!("上游 trace writer 线程启动失败：{e}");
    }
}

/// 按当前配置同步开关与管道（幂等，**当前只在启动期调用一次**）。
///
/// ## 接线现状（2026-08-15 对齐）
///
/// `main.rs` 启动装配时调用一次，之后**没有**热更入口：三个配置字段
/// （`upstream_trace_enabled` / `upstream_trace_path` / `upstream_trace_max_bytes`）
/// 不进 `UpdateConfigRequest`，改 `config.json` 后须重启生效（与
/// `trust_forwarded_header` 同款「启动期一次性读取」范式）。函数形态刻意保持
/// 幂等 + 可重复调：将来若把字段接进配置热重载（TIER1 ArcSwap 镜像），
/// 热路径上每轮调用即可就地生效，无需改本函数。
///
/// ## 为什么做成可热调形态而不是只有 init
///
/// 幂等快路径让「无热更入口」时也零额外成本，且把「将来接热更」的路留好：
/// 本函数自带 `enabled` 快照比对，状态没变直接返回 —— 不需要调用方去查重。
///
/// 代价是它在（假想的）热路径上被调用。开销做了两道限：
///   1. `enabled=false`（生产常态）时只做一次 `Relaxed` load + 一次 store 判等，
///      **不碰 `OnceLock`、不建线程、不 clone 字符串**。
///   2. `enabled=true` 时管道只在首次真正建起来（`ONCE_STARTED` 的 CAS 保证），
///      之后每次调用同样只有两次原子操作。
///
/// ⇒ 关闭时零分配、零 IO；这正是「默认关」这个承诺的兑现处。
pub fn sync_from_config(enabled: bool, path: &str, max_bytes: u64) {
    // 快路径：状态没变就直接返回（生产常态 false→false，两次原子操作）
    if ENABLED.load(Ordering::Relaxed) == enabled && (!enabled || ONCE_STARTED.load(Ordering::Relaxed))
    {
        return;
    }
    if enabled && !ONCE_STARTED.swap(true, Ordering::Relaxed) {
        // 首次开启：建管道 + 起 writer 线程。`init` 自身也幂等（OnceLock）。
        init(path, max_bytes, true);
        tracing::warn!(
            path = %path,
            max_bytes,
            "上游 trace 埋点已开启（诊断模式：每条失败响应写一行 JSONL，含最多 {} 字节脱敏 body）",
            BODY_MAX_BYTES
        );
        return;
    }
    set_enabled(enabled);
}

/// writer 线程是否已启动（一次性，关掉再开不重建线程）。
static ONCE_STARTED: AtomicBool = AtomicBool::new(false);

/// 提交一条 trace（热路径调用，**非阻塞**）。///
/// 开关关闭 / 未初始化 / 通道满 → 静默丢弃（满时计数，见 [`dropped_count`]）。
pub fn emit(trace: UpstreamTrace) {
    if !is_enabled() {
        return;
    }
    let Some(pipeline) = PIPELINE.get() else {
        return;
    };
    if pipeline.tx.try_send(trace).is_err() {
        let n = DROPPED.fetch_add(1, Ordering::Relaxed) + 1;
        // 降低日志噪音：仅在 2 的幂次时告警（同 usage::pipeline）
        if n.is_power_of_two() {
            tracing::warn!("上游 trace 管道积压，已累计丢弃 {} 条", n);
        }
    }
}

/// 失败响应的 trace 守卫（RAII）。
///
/// ## 为什么是 Drop 守卫，而不是在每条分支各写一次 `emit`
///
/// `provider.rs` 的失败路径在读完 body 之后分成 **11 条分支**（client_validation /
/// temporary_rate_limit / monthly_limit / account_suspended / invalid_model_id /
/// capacity_400 / generic_400 / 401|403（内含 5 个子出口）/ model_unavailable /
/// 408|429|5xx / other_4xx），每条各自 `break` 或 `continue`。
///
/// 在每个出口各写一次 emit ⇒ 12+ 处重复的 12 字段组装代码。本仓 §7 记录的**主导
/// 缺陷形态**恰好是这个：同一逻辑各写一份，改一处漏一处（`update.rs` 的 chunked
/// 缺口、#6 的 trust_forwarded_header 都是这么来的）。
///
/// 守卫把「组装」做一次、「打标签」留给分支一行，`Drop` 负责发出去。
/// 副作用是它**天然全覆盖**：新加的分支哪怕忘了打标签，记录仍会落盘、
/// 只是 `verdict` 是 [`VERDICT_UNCLASSIFIED`] —— 从「静默丢失」变成「可被查询发现」。
///
/// ⚠️ 守卫**不覆盖成功路径**：成功分支 `return Ok(...)` 时 body 还没读（也不该读，
/// 那是对话内容），故成功侧用独立的 [`emit`] 直接发一条 `verdict="success"`。
pub struct FailureTraceGuard {
    inner: Option<UpstreamTrace>,
}

impl FailureTraceGuard {
    /// 建守卫。
    ///
    /// `enabled` **显式传入**而不是内部读 [`is_enabled`]，两个理由：
    ///   1. 让守卫不依赖进程全局态 ⇒ 测试可确定性地驱动两种分支
    ///      （否则并行测试会在同一个 `ENABLED` 静态上互相打断，实测就是这么红的）。
    ///   2. 调用方 `provider.rs` 那一侧本来就要判一次开关，传进来避免读两遍。
    ///
    /// `build` 是**闭包而非现成值**，这条是承重的：记录的组装含
    /// [`sanitize_body`]（最多 2KiB 的逐字符扫描）+ 五六次 `String` clone。
    /// 传现成值的话，开关关着时这些代价**照付**——「默认关零开销」就是假的。
    /// 闭包让它在 `enabled=false` 时根本不被调用。
    pub fn new(enabled: bool, build: impl FnOnce() -> UpstreamTrace) -> Self {
        if !enabled {
            return Self { inner: None };
        }
        Self {
            inner: Some(build()),
        }
    }

    /// 标注「哪条分支接住了它」。多次调用时**最后一次生效**
    /// （401/403 那条大分支里有 5 个子出口，外层先标 `auth_4xx`、
    /// 子出口再覆盖成更精确的名字，这个语义是需要的）。
    pub fn verdict(&mut self, v: &str) {
        if let Some(t) = self.inner.as_mut() {
            t.verdict = v.to_string();
        }
    }
}

impl Drop for FailureTraceGuard {
    fn drop(&mut self) {
        if let Some(t) = self.inner.take() {
            emit(t);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> UpstreamTrace {
        UpstreamTrace {
            ts: "2026-08-07T00:00:00.000Z".into(),
            credential_id: 1,
            endpoint: "cli".into(),
            url: "https://q.eu-central-1.amazonaws.com/generateAssistantResponse".into(),
            region: "eu-central-1".into(),
            model: Some("claude-opus-5".into()),
            attempt: 0,
            absorb_round: 0,
            upstream_calls: 1,
            status: Some(429),
            retry_after_raw: Some("30".into()),
            retry_after_secs: Some(30),
            body: Some("{}".into()),
            network_error: None,
            latency_ms: 12,
            verdict: VERDICT_UNCLASSIFIED.into(),
            cred_ever_succeeded: true,
        }
    }

    // ============ 脱敏（硬红线）============

    #[test]
    fn should_redact_kiro_api_key_in_body() {
        let body = r#"{"error":"bad key ksk_AbCd1234EfGh5678 rejected"}"#;
        let out = sanitize_body(body);
        assert!(
            !out.contains("ksk_AbCd1234EfGh5678"),
            "ksk_ 明文泄漏：{out}"
        );
        assert!(out.contains(REDACTED), "应打码：{out}");
        // OVER-REACH CONTROL：非 secret 部分必须保留，否则 trace 就没有诊断价值了
        assert!(out.contains("rejected"), "过度打码，丢了诊断信息：{out}");
    }

    #[test]
    fn should_redact_bearer_header_value() {
        let out = sanitize_body("Authorization: Bearer abc.def.ghi failed");
        assert!(!out.contains("abc.def.ghi"), "Bearer 值泄漏：{out}");
        assert!(out.contains("failed"), "过度打码：{out}");
    }

    #[test]
    fn should_redact_jwt_looking_token() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.sIgNaTuRe";
        let out = sanitize_body(&format!("{{\"accessToken\":\"{jwt}\"}}"));
        assert!(!out.contains(jwt), "JWT 泄漏：{out}");
        // 键名可以留（它不是秘密，而且是排障需要的形状信息）
        assert!(out.contains("accessToken"), "键名不该被吃掉：{out}");
    }

    #[test]
    fn should_redact_secret_split_by_truncation_boundary() {
        // 承重：跨截断边界的 secret 必须仍被打码。
        //
        // ⚠️ 已实测：这条测试对**顺序**不敏感（先截后脱 / 先脱后截都通过）——
        // 因为三个判据都是前缀匹配。保留它是为了守「边界处不漏」这个性质本身，
        // 顺序由下面那条 `sanitize_body_must_bound_redact_input_size` 守。
        let mut s = "x".repeat(BODY_MAX_BYTES - 10);
        s.push_str("ksk_SECRETVALUE1234567890");
        let out = sanitize_body(&s);
        assert!(!out.contains("SECRETVALUE"), "截断边界处 secret 泄漏：{out}");
        assert!(out.contains(REDACTED), "边界处应打码：{out}");
    }

    #[test]
    fn sanitize_body_must_bound_redact_input_size() {
        // 顺序的真实约束：`redact` 的**输入**必须已被截断到 BODY_MAX_BYTES，
        // 否则一条 MiB 级错误 body 会在请求路径上做一次 MiB 级逐字符扫描。
        //
        // 判据用一个「脱敏后会变长」的输入把两种顺序区分开：
        // 每个 `ksk_A` (5B) → `[REDACTED]` (10B)，即打码是**扩张**的。
        //   · 先截断再脱敏：截到 2KiB 后再扩张 ⇒ 结果**可以 > 2KiB**
        //   · 先脱敏再截断：整体扩张后被截回 ⇒ 结果**恒 ≤ 2KiB**
        // 于是「结果长度 > BODY_MAX_BYTES」这个可观测量就把顺序钉死了。
        //
        // 回退即 FAIL：把 sanitize_body 改成 truncate(redact(body)) 这条必红。
        let unit = "ksk_A "; // 6 字节 → "[REDACTED] " 11 字节
        let n = (BODY_MAX_BYTES / unit.len()) + 100; // 输入远超 2KiB
        let big = unit.repeat(n);
        assert!(big.len() > BODY_MAX_BYTES, "输入需超上限才能区分顺序");
        let out = sanitize_body(&big);
        assert!(
            out.len() > BODY_MAX_BYTES,
            "sanitize_body 似乎是先脱敏再截断（结果 {} ≤ {}）⇒ redact 扫了整个 body，\
             MiB 级错误响应会在请求路径上做 MiB 级扫描",
            out.len(),
            BODY_MAX_BYTES
        );
        // 且仍必须真的打码（不能靠"没脱敏"来通过上面那条）
        assert!(!out.contains("ksk_A"), "未打码：{}", &out[..80.min(out.len())]);
    }

    #[test]
    fn should_not_redact_ordinary_upstream_error_text() {
        // OVER-REACH CONTROL：真实 429 body 必须原样可读，否则埋点答不了
        // 「429 body 里有没有配额字段」这个问题（= 本埋点的存在理由之一）
        let body = r#"{"reason":"OVERAGE_REQUEST_LIMIT_EXCEEDED","resetsAt":"2026-09-01"}"#;
        let out = sanitize_body(body);
        assert_eq!(out, body, "正常错误 body 被误打码：{out}");
    }

    // ============ 截断（字节口径）============

    #[test]
    fn should_truncate_body_by_bytes_not_chars() {
        // CJK：每字 3 字节。字符口径会写 3 倍字节（本仓 map_tool_name 的历史缺陷形态）
        let body = "限".repeat(BODY_MAX_BYTES); // 3 * 2048 字节
        let out = sanitize_body(&body);
        assert!(
            out.len() <= BODY_MAX_BYTES,
            "截断按字符而非字节：{} > {}",
            out.len(),
            BODY_MAX_BYTES
        );
    }

    #[test]
    fn should_truncate_on_utf8_char_boundary() {
        // 边界恰好切在一个 3 字节字符中间 → 必须回退到边界，不能 panic 也不能产生非法 UTF-8
        let mut s = "a".repeat(BODY_MAX_BYTES - 1);
        s.push('限');
        let out = sanitize_body(&s);
        assert!(out.len() <= BODY_MAX_BYTES);
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    // ============ 默认关（不阻塞热路径 / 不产生副作用）============

    #[test]
    fn should_be_disabled_by_default_and_emit_nothing() {
        // 源码级守卫（本仓既有范式，约 103 处 include_str!）：直接钉住那个 static 的初值。
        //
        // 为什么不读 `ENABLED.load(...)`：那是**进程全局**态，并行跑的其它测试会改它
        // ⇒ 这条测试会随机红（实测红过两条）。也不复制一个 `const ENABLED_DEFAULT`：
        // 复制出来的字面量与真 static 会各自漂移，而"默认关"必须由**真 static** 保证。
        let src = include_str!("upstream_trace.rs");
        // 剔掉注释行，避免注释里出现同样字面量造成假通过
        let code: String = src
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        // ⚠️ needle **必须运行时拼接**（本仓库踩过四次的 include_str! 自匹配坑）：
        // 写成整条字面量时，它自己就出现在本断言这一行里，而本行是**代码行**、
        // 剔注释那步剔不掉 ⇒ `contains` 命中自己，把初值改成 true 测试照样绿。
        // 实测过：回退验证跑出 `修复态=PASS | 回退态=PASS` 才发现这条测试是装饰品。
        let needle = format!(
            "static ENABLED: AtomicBool = AtomicBool::new({});",
            "false"
        );
        assert_eq!(
            code.matches(needle.as_str()).count(),
            1,
            "上游 trace 必须默认关闭：`static ENABLED` 的初值不是 false（或该声明出现了多处）。\
             生产 config.json 是既有文件，且这是诊断用途的高频写埋点"
        );
    }

    #[test]
    fn guard_should_be_inert_when_disabled() {
        let mut g = FailureTraceGuard::new(false, || panic!("关闭时不该组装记录"));
        g.verdict("rate_limited");
        assert!(g.inner.is_none(), "关闭时守卫应为空，不分配也不发送");
    }

    #[test]
    fn guard_should_not_build_record_when_disabled() {
        // 承重：`new` 收闭包而非现成值。若改回收现成值，调用点就必须先组装
        // （含 2KiB sanitize_body 扫描），「关闭时零开销」立刻变成假的。
        let mut built = false;
        let _g = FailureTraceGuard::new(false, || {
            built = true;
            sample()
        });
        assert!(!built, "关闭时不该调用组装闭包（否则默认关仍付全部代价）");
    }

    // ============ 守卫语义 ============

    #[test]
    fn guard_verdict_last_write_wins() {
        let mut g = FailureTraceGuard::new(true, sample);
        g.verdict("auth_4xx");
        g.verdict("region_mismatch_403");
        assert_eq!(
            g.inner.as_ref().unwrap().verdict,
            "region_mismatch_403",
            "401/403 子出口需要能覆盖外层的粗标签"
        );
        // 阻止 Drop 真的 emit（管道未初始化时 emit 是 no-op，但别依赖那个）
        g.inner = None;
    }

    #[test]
    fn guard_defaults_to_unclassified() {
        // ⚠️ 这条**曾经是同义反复**：`sample()` 用 `VERDICT_UNCLASSIFIED` 填 verdict，
        // 断言又拿 `VERDICT_UNCLASSIFIED` 去比 —— 把常量改成 `""` 两边一起变，测试照样绿。
        // 实测发现（回退验证跑出 `修复态=PASS | 回退态=PASS`）。
        // ⇒ 改为断言**字面值**，常量被改动即 FAIL。
        assert_eq!(
            VERDICT_UNCLASSIFIED, "unclassified",
            "未分类判据的字面值是承重的：验收脚本 trace-audit.py 按它统计\
             「有多少失败分支漏打标签」。改这个字面量会让那张表恒为 0"
        );
        let mut g = FailureTraceGuard::new(true, sample);
        assert_eq!(
            g.inner.as_ref().unwrap().verdict,
            "unclassified",
            "没打标签的分支必须落 unclassified（这是发现漏标分支的唯一信号）"
        );
        g.inner = None;
    }

    /// 承重：`provider.rs` 的两处守卫组装都必须用 [`VERDICT_UNCLASSIFIED`] 做初值。
    ///
    /// 上面那条只管本模块的常量与守卫语义；真正决定「漏标分支能否被发现」的是
    /// **调用点**填了什么。若哪天有人在 provider.rs 里把初值写成 `"unknown"`，
    /// 验收脚本那张 `unclassified` 表就恒为 0，而漏标分支继续静默。
    #[test]
    fn provider_guards_must_default_verdict_to_unclassified() {
        let src = include_str!("provider.rs");
        let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..cut];
        // needle 运行时拼接（include_str! 自匹配坑）
        let needle = format!(
            "verdict: crate::kiro::upstream_trace::VERDICT_UNCLASSIFIED{}",
            ".to_string(),"
        );
        assert_eq!(
            prod.matches(needle.as_str()).count(),
            2,
            "对话路径与 MCP 路径的守卫都必须以 VERDICT_UNCLASSIFIED 为初值（当前 {} 处）——\
             否则漏标的失败分支在 trace 里查不出来",
            prod.matches(needle.as_str()).count()
        );
    }

    // ============ 落盘 + 轮转（不覆盖）============

    #[test]
    fn writer_should_rotate_instead_of_overwriting() {
        let dir = std::env::temp_dir().join(format!(
            "kg-trace-rot-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("upstream_trace.jsonl");
        // max_bytes 故意设得极小，让每条记录都触发一次轮转
        let w = Writer {
            path: path.clone(),
            max_bytes: 200,
        };
        for i in 0..3u64 {
            let mut t = sample();
            t.credential_id = i;
            w.write(&t);
        }
        // 当前文件存在 + 至少一个轮转文件存在（= 没有覆盖掉历史）
        assert!(path.exists(), "当前文件应存在");
        let rotated = dir.join("upstream_trace.jsonl.1");
        assert!(
            rotated.exists(),
            "超上限应轮转出 .1 文件，而不是覆盖/截断当前文件"
        );
        // 上界成立：文件数 ≤ KEEP_ROTATED + 1
        let count = fs::read_dir(&dir).unwrap().count();
        assert!(
            count <= KEEP_ROTATED + 1,
            "轮转文件数无上界（磁盘打满风险）：{count}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn writer_should_emit_valid_jsonl_line() {
        let dir = std::env::temp_dir().join(format!(
            "kg-trace-jsonl-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("upstream_trace.jsonl");
        let w = Writer {
            path: path.clone(),
            max_bytes: 64 * 1024 * 1024,
        };
        w.write(&sample());
        let content = fs::read_to_string(&path).unwrap();
        let lines: Vec<_> = content.lines().collect();
        assert_eq!(lines.len(), 1, "应恰好一行");
        let v: serde_json::Value = serde_json::from_str(lines[0]).expect("必须是合法 JSON");
        // 那四个问题各自对应的字段必须在
        assert_eq!(v["retry_after_raw"], "30", "Retry-After 原值缺失");
        assert_eq!(v["url"], "https://q.eu-central-1.amazonaws.com/generateAssistantResponse");
        assert_eq!(v["region"], "eu-central-1");
        assert_eq!(v["status"], 429);
        assert!(v["verdict"].is_string(), "缺网关判断字段");
        assert!(v["cred_ever_succeeded"].is_boolean());
        let _ = fs::remove_dir_all(&dir);
    }

    /// ⭐ 落盘的 JSONL 必须带 `upstream_user_id`，且它必须**平铺在同一层**
    /// （离线脚本按顶层字段读，包在子对象里等于零产出）。
    ///
    /// 回退即 FAIL：把 `Writer::write` 里的 `TraceRecord` 换回直接序列化 `trace`
    /// —— 该字段消失，本条断言红。
    #[test]
    fn writer_should_emit_flattened_upstream_user_id() {
        let dir = std::env::temp_dir().join(format!(
            "kg-trace-uid-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("upstream_trace.jsonl");
        let w = Writer {
            path: path.clone(),
            max_bytes: 64 * 1024 * 1024,
        };

        let mut t = sample();
        t.body = Some(
            r#"{"__type":"com.amazon.aws.codewhisperer#AccessDeniedException","message":"Your User ID (898055051935) temporarily is suspended."}"#
                .into(),
        );
        w.write(&t);

        // 无身份的一条：普通 429，body 不带 User ID（这是真实的多数形态）
        let mut t2 = sample();
        t2.body = Some(
            r#"{"message":"Too many requests, please wait before trying again.","reason":"USER_REQUEST_RATE_EXCEEDED"}"#
                .into(),
        );
        w.write(&t2);

        let content = fs::read_to_string(&path).unwrap();
        let lines: Vec<_> = content.lines().collect();
        assert_eq!(lines.len(), 2);

        let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(
            v["upstream_user_id"], "898055051935",
            "身份必须平铺在顶层：{}",
            lines[0]
        );
        // flatten 没有吃掉原有字段
        assert_eq!(v["credential_id"], 1);
        assert_eq!(v["verdict"], VERDICT_UNCLASSIFIED);
        assert_eq!(v["retry_after_raw"], "30");

        let v2: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert!(
            v2.get("upstream_user_id").is_none(),
            "无身份时应整个字段缺省（而不是 null 或空串）：{}",
            lines[1]
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// 成功侧 body 恒为 `None` ⇒ 派生必然 `None`，不该 panic 也不该瞎猜。
    #[test]
    fn derive_upstream_user_id_should_be_none_without_body() {
        let mut t = sample();
        t.body = None;
        assert_eq!(t.derive_upstream_user_id(), None);
    }

    #[test]
    fn trace_record_must_not_carry_request_body_or_auth_fields() {
        // 硬红线的结构级守卫：记录类型里**根本不存在**请求体/token 字段。
        // 靠"注意别写进去"是靠不住的（本仓的主导缺陷形态就是注释对、实现漏），
        // 这条在序列化产物上钉死：加一个 request_body / token 字段即 FAIL。
        let json = serde_json::to_string(&sample()).unwrap();
        for banned in [
            "request_body",
            "requestBody",
            "\"token\"",
            "refresh_token",
            "refreshToken",
            "kiro_api_key",
            "kiroApiKey",
            "authorization",
            "Authorization",
        ] {
            assert!(
                !json.contains(banned),
                "trace 记录含禁止字段 `{banned}`：{json}"
            );
        }
    }
}



