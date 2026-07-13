//! 模型目录：单一声明式真相源（single source of truth）。
//!
//! # 为什么有这个模块
//! 历史上模型知识散落四处、各自手维、极易漂移:
//! - `converter::map_model`(用 `contains` 子串匹配把 Anthropic 名映射到 Kiro modelId)
//! - `converter::get_context_window_size`(上下文窗口)
//! - `handlers::get_models`(`/v1/models` 广告的清单，还各带 max_tokens)
//! - 前端 `PROBE_MODEL_CATALOG` + `service.rs` 探测默认清单
//!
//! 旧 `contains` 匹配还有三个真漏洞(实测坐实):
//! 1. **Claude 3 老名静默升到贵档**:`claude-3-opus` 不含 `4-x` → 落 else → `claude-opus-4.6`(2.20x),
//!    用户以为用便宜老模型，实际按最贵档计费且拿到不同代际响应，零日志。
//! 2. **高版本静默降级**:`opus-4-9`/`opus-5` 落 else → 降 `4.6`，无告警。
//! 3. **子串误命中**:任何含 `opus` 的串都命中，日期串里的 `4-5` 片段会误判版本。
//!
//! # 本模块的设计(对齐 CLIProxyAPI / LiteLLM / one-api 的业界共识)
//! - 一张 `static CATALOG: &[ModelSpec]`，每个 Kiro 真实 modelId 一行，携带别名/窗口/倍率/能力。
//! - 匹配分层(严格):精确别名反查 → 结构化 family+版本解析 → Claude3 老名显式近似(告警) →
//!   未知/未知版本**显式拒绝**(不静默降级)。
//! - 所有非精确命中打 `tracing::warn!`，把「识别错/降级」从静默变可观测。
//! - `map_model` / `get_context_window_size` / `/v1/models` / 探测清单全部从本表派生，消灭漂移。
//!
//! # 两池隔离铁律
//! 本目录**只服务 Kiro 选号池**。custom_api 透传路径(passthrough.rs)逐字节转发、model 不映射，
//! 绝不经过本模块——registry 与透传是「同一识别入口下的不同 provider 维度」，天然不串池。

use std::collections::HashMap;
use std::sync::OnceLock;

/// 模型家族。国产模型也在内，Kiro 上游直收其原生 modelId(倍率远低于 claude)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Opus,
    Sonnet,
    Haiku,
    DeepSeek,
    Glm,
    Qwen,
    Minimax,
    /// Kiro 的 "Auto" 路由模型(1.0x,由上游按负载自动选目标模型,无固定版本/窗口)。
    Auto,
}

/// 单个模型的权威描述。字段即「关于这个模型的一切」的唯一来源。
#[derive(Debug, Clone)]
pub struct ModelSpec {
    /// Kiro 上游真实 modelId(convert_request 最终注入请求体的值)，如 `claude-opus-4.6`。
    pub kiro_id: &'static str,
    /// 模型家族。
    pub family: Family,
    /// 结构化版本 `(major, minor)`；国产模型无语义版本号 = None。
    pub version: Option<(u16, u16)>,
    /// 精确别名(全小写)：含带日期全名(`claude-opus-4-6-20260204`)与短名(`opus-4.6`)。
    /// 精确匹配走这里，是主路径。
    pub aliases: &'static [&'static str],
    /// `/v1/models` 的 owned_by 字段。
    pub owned_by: &'static str,
    /// `/v1/models` 的展示名。
    pub display_name: &'static str,
    /// 上下文窗口(输入上限)：Kiro 于 2026-03 将 opus/sonnet 4.6 升到 1M。
    pub context_window: i32,
    /// 建议最大输出 tokens(`/v1/models` 的 max_tokens 字段)。
    pub max_output: i32,
    /// 计费倍率(相对基准)。此前只存在记忆文档，现进代码作结构化承载。
    pub credit_mult: f32,
    /// 是否支持 thinking(思考)变体。
    pub supports_thinking: bool,
    /// 是否支持 1M 上下文窗口变体(`claude-xxx[1m]`)。为 true 的模型:
    /// ① `/v1/models` 额外广告一条 `<id>[1m]` 变体;② 请求命中 `[1m]` 后缀时注入
    /// `anthropic-beta: context-1m-2025-08-07` 头。**诚实边界**:Kiro 上游是 CodeWhisperer/Q
    /// 协议(非 Anthropic 直连),该 beta 头是否被上游识别并真放开 1M 窗口未经证实——本字段以
    /// `context_window == 1_000_000` 为内部一致性依据,不代表上游一定认(待旁挂黑盒验证)。
    pub supports_1m: bool,
    /// 是否在 `/v1/models` 中广告(thinking 变体通常只做别名不单列)。
    pub advertised: bool,
}

/// 权威模型目录。新增/改名/调倍率/调窗口 **只改这一处**，全项目派生。
///
/// 数据对齐 Kiro 官方模型表(2026-07):opus 全系 2.2x;sonnet 5/4.6 为 1M、4.5/4.0 为 200K 均 1.3x;
/// haiku 4.5 200K 0.4x;deepseek-3.2 **128K** 0.25x;qwen3-coder-next **256K** 0.05x;
/// minimax m2.5 200K 0.25x / m2.1 200K 0.15x;glm-5 200K 0.5x;Auto 1.0x(路由态无固定窗口)。
pub static CATALOG: &[ModelSpec] = &[
    // ===== Opus 系(2.20x 最贵) =====
    ModelSpec {
        kiro_id: "claude-opus-4.8",
        family: Family::Opus,
        version: Some((4, 8)),
        aliases: &["claude-opus-4-8", "claude-opus-4.8", "opus-4.8", "opus-4-8"],
        owned_by: "anthropic",
        display_name: "Claude Opus 4.8",
        context_window: 1_000_000,
        max_output: 128_000,
        credit_mult: 2.20,
        supports_thinking: true,
        supports_1m: true,
        advertised: true,
    },
    ModelSpec {
        kiro_id: "claude-opus-4.7",
        family: Family::Opus,
        version: Some((4, 7)),
        aliases: &["claude-opus-4-7", "claude-opus-4.7", "opus-4.7", "opus-4-7"],
        owned_by: "anthropic",
        display_name: "Claude Opus 4.7",
        context_window: 1_000_000,
        max_output: 64_000,
        credit_mult: 2.20,
        supports_thinking: true,
        supports_1m: true,
        advertised: true,
    },
    ModelSpec {
        kiro_id: "claude-opus-4.6",
        family: Family::Opus,
        version: Some((4, 6)),
        aliases: &["claude-opus-4-6", "claude-opus-4.6", "opus-4.6", "opus-4-6"],
        owned_by: "anthropic",
        display_name: "Claude Opus 4.6",
        context_window: 1_000_000,
        max_output: 64_000,
        credit_mult: 2.20,
        supports_thinking: true,
        supports_1m: true,
        advertised: true,
    },
    ModelSpec {
        kiro_id: "claude-opus-4.5",
        family: Family::Opus,
        version: Some((4, 5)),
        aliases: &[
            "claude-opus-4-5",
            "claude-opus-4.5",
            "claude-opus-4-5-20251101",
            "opus-4.5",
            "opus-4-5",
        ],
        owned_by: "anthropic",
        display_name: "Claude Opus 4.5",
        context_window: 200_000,
        max_output: 64_000,
        credit_mult: 2.20,
        supports_thinking: true,
        supports_1m: false,
        advertised: true,
    },
    // ===== Sonnet 系(1.30x) =====
    ModelSpec {
        kiro_id: "claude-sonnet-5",
        family: Family::Sonnet,
        version: Some((5, 0)),
        aliases: &["claude-sonnet-5", "claude-sonnet-5-0", "sonnet-5", "sonnet-5-0"],
        owned_by: "anthropic",
        display_name: "Claude Sonnet 5",
        context_window: 1_000_000,
        max_output: 64_000,
        credit_mult: 1.30,
        supports_thinking: true,
        supports_1m: true,
        advertised: true,
    },
    ModelSpec {
        kiro_id: "claude-sonnet-4.6",
        family: Family::Sonnet,
        version: Some((4, 6)),
        aliases: &["claude-sonnet-4-6", "claude-sonnet-4.6", "sonnet-4.6", "sonnet-4-6"],
        owned_by: "anthropic",
        display_name: "Claude Sonnet 4.6",
        context_window: 1_000_000,
        max_output: 64_000,
        credit_mult: 1.30,
        supports_thinking: true,
        supports_1m: true,
        advertised: true,
    },
    ModelSpec {
        kiro_id: "claude-sonnet-4.5",
        family: Family::Sonnet,
        version: Some((4, 5)),
        aliases: &[
            "claude-sonnet-4-5",
            "claude-sonnet-4.5",
            "claude-sonnet-4-5-20250929",
            "sonnet-4.5",
            "sonnet-4-5",
        ],
        owned_by: "anthropic",
        display_name: "Claude Sonnet 4.5",
        context_window: 200_000,
        max_output: 64_000,
        credit_mult: 1.30,
        supports_thinking: true,
        supports_1m: false,
        advertised: true,
    },
    ModelSpec {
        kiro_id: "claude-sonnet-4.0",
        family: Family::Sonnet,
        version: Some((4, 0)),
        aliases: &[
            "claude-sonnet-4-0",
            "claude-sonnet-4.0",
            // 真实 Anthropic 风格无次版本名(如 claude-sonnet-4-20250514)归到 4.0——
            // 否则会走 family-default 被静默升到最新 Sonnet(代际/窗口语义错)。
            "claude-sonnet-4-20250514",
            "sonnet-4.0",
            "sonnet-4-0",
        ],
        owned_by: "anthropic",
        display_name: "Claude Sonnet 4.0",
        context_window: 200_000,
        max_output: 64_000,
        credit_mult: 1.30,
        supports_thinking: true,
        supports_1m: false,
        advertised: true,
    },
    // ===== Haiku 系(0.40x) =====
    ModelSpec {
        kiro_id: "claude-haiku-4.5",
        family: Family::Haiku,
        version: Some((4, 5)),
        aliases: &[
            "claude-haiku-4-5",
            "claude-haiku-4.5",
            "claude-haiku-4-5-20251001",
            "haiku-4.5",
            "haiku-4-5",
        ],
        owned_by: "anthropic",
        display_name: "Claude Haiku 4.5",
        context_window: 200_000,
        max_output: 64_000,
        credit_mult: 0.40,
        supports_thinking: true,
        supports_1m: false,
        advertised: true,
    },
    // ===== 国产模型(Kiro 直收原生 modelId，倍率远低) =====
    ModelSpec {
        kiro_id: "deepseek-3.2",
        family: Family::DeepSeek,
        version: None,
        aliases: &["deepseek-3.2", "deepseek-v3.2", "deepseek"],
        owned_by: "deepseek",
        display_name: "DeepSeek V3.2",
        context_window: 128_000,
        max_output: 64_000,
        credit_mult: 0.25,
        supports_thinking: false,
        supports_1m: false,
        advertised: true,
    },
    ModelSpec {
        kiro_id: "glm-5",
        family: Family::Glm,
        version: None,
        aliases: &["glm-5", "glm5", "glm"],
        owned_by: "zhipu",
        display_name: "GLM-5",
        context_window: 200_000,
        max_output: 64_000,
        credit_mult: 0.50,
        supports_thinking: false,
        supports_1m: false,
        advertised: true,
    },
    ModelSpec {
        kiro_id: "qwen3-coder-next",
        family: Family::Qwen,
        version: None,
        aliases: &["qwen3-coder-next", "qwen3-coder", "qwen"],
        owned_by: "qwen",
        display_name: "Qwen3 Coder Next",
        context_window: 256_000,
        max_output: 64_000,
        credit_mult: 0.05,
        supports_thinking: false,
        supports_1m: false,
        advertised: true,
    },
    ModelSpec {
        kiro_id: "minimax-m2.5",
        family: Family::Minimax,
        version: Some((2, 5)),
        aliases: &["minimax-m2.5", "minimax-m2-5", "minimax"],
        owned_by: "minimax",
        display_name: "MiniMax M2.5",
        context_window: 200_000,
        max_output: 64_000,
        credit_mult: 0.25,
        supports_thinking: false,
        supports_1m: false,
        advertised: true,
    },
    ModelSpec {
        kiro_id: "minimax-m2.1",
        family: Family::Minimax,
        version: Some((2, 1)),
        aliases: &["minimax-m2.1", "minimax-m2-1"],
        owned_by: "minimax",
        display_name: "MiniMax M2.1",
        context_window: 200_000,
        max_output: 64_000,
        credit_mult: 0.15,
        supports_thinking: false,
        supports_1m: false,
        advertised: true,
    },
    // ===== Auto(1.0x,上游按负载路由,无固定版本/窗口) =====
    ModelSpec {
        kiro_id: "auto",
        family: Family::Auto,
        version: None,
        aliases: &["auto", "claude-auto"],
        owned_by: "anthropic",
        display_name: "Auto",
        context_window: 200_000, // 路由态无权威窗口,保守取 200k
        max_output: 64_000,
        credit_mult: 1.00,
        supports_thinking: false,
        supports_1m: false,
        advertised: true,
    },
];

impl ModelSpec {
    /// `/v1/models` 对外广告的 id:优先第一个别名(claude 系为 `claude-opus-4-8` 连字符形,
    /// 与历史广告一致),国产模型第一个别名即其原生 id。回退到 kiro_id。
    pub fn advertised_id(&self) -> &'static str {
        self.aliases.first().copied().unwrap_or(self.kiro_id)
    }
}

/// 精确别名 → CATALOG 下标 的反向索引(惰性构建一次)。key 全小写。
fn alias_index() -> &'static HashMap<String, usize> {
    static IDX: OnceLock<HashMap<String, usize>> = OnceLock::new();
    IDX.get_or_init(|| {
        let mut m = HashMap::new();
        for (i, spec) in CATALOG.iter().enumerate() {
            // kiro_id 本身也作为别名(完整原生 id 直透，映射回自身)。
            m.entry(spec.kiro_id.to_ascii_lowercase()).or_insert(i);
            for a in spec.aliases {
                m.entry(a.to_ascii_lowercase()).or_insert(i);
            }
        }
        m
    })
}

/// 命中方式，用于可观测日志区分「精确 vs 各种回退」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    /// 精确别名命中(主路径，无歧义)。
    Exact,
    /// 结构化 family+版本解析命中(如带日期串的规范名)。
    Structured,
    /// Claude 3 老名近似到最近档(告警级——语义有损)。
    Claude3Approx,
    /// 已知 family 但版本比目录新，映射到同族最新档(需开关，告警)。
    UnknownVersionFallback,
}

/// 解析结果。
pub struct Resolved {
    pub spec: &'static ModelSpec,
    pub kind: MatchKind,
    /// 客户端请求的是 1M 上下文变体(`claude-xxx[1m]`)**且**该 spec 支持 1M。
    /// 客户端给了 `[1m]` 但 spec 不支持 → 此处为 false(后缀被忽略并告警),不发 beta 头。
    pub is_1m: bool,
}

/// 从原始模型名剥离受控的 `[1m]` 后缀,返回 (去后缀名, 是否带 [1m])。
///
/// 与 [`strip_thinking`] 对称的范式。客户端(如某些只能传纯模型名的客户端)可用
/// `claude-opus-4-6[1m]` 显式请求 1M 上下文变体。在 [`resolve`] 最前面剥离,剥完走既有
/// 精确/thinking/结构化全流程,`[1m]` 只作为 is_1m 布尔信号,不影响 kiro_id 映射。
fn strip_1m_suffix(model_lower: &str) -> (String, bool) {
    if let Some(base) = model_lower.strip_suffix("[1m]") {
        (base.to_string(), true)
    } else {
        (model_lower.to_string(), false)
    }
}

/// 从原始模型名剥离受控的 `-thinking` 后缀，返回 (去后缀名, 是否带 thinking)。
fn strip_thinking(model_lower: &str) -> (String, bool) {
    if let Some(base) = model_lower.strip_suffix("-thinking") {
        (base.to_string(), true)
    } else if let Some(base) = model_lower.strip_suffix("-think") {
        (base.to_string(), true)
    } else {
        (model_lower.to_string(), false)
    }
}

/// 从名字里结构化提取第一个 `(major, minor)` 版本(接受 `4-6` 或 `4.6`)。
/// 只取「族名之后」的版本，避免命中日期串(如 `20250929`)——通过要求版本紧跟在已知族名后实现。
fn parse_family_version(model_lower: &str) -> Option<(Family, Option<(u16, u16)>)> {
    // 识别族名(子串，但只用来定位，不直接决定映射)。
    let family = if model_lower.contains("opus") {
        Family::Opus
    } else if model_lower.contains("sonnet") {
        Family::Sonnet
    } else if model_lower.contains("haiku") {
        Family::Haiku
    } else if model_lower.contains("deepseek") {
        Family::DeepSeek
    } else if model_lower.contains("glm") {
        Family::Glm
    } else if model_lower.contains("qwen") {
        Family::Qwen
    } else if model_lower.contains("minimax") {
        Family::Minimax
    } else {
        // ⚠️ **不**用 `contains("auto")` 探测 Family::Auto:`auto` 是极常见英文子串,
        // 任何含它的未知名(gpt-4-auto / autopilot / autocomplete-model)都会被静默映射到
        // Kiro Auto(1.0x)并真实发上游,既不拒绝也不告警,与「未知即拒 + 非精确必可观测」的
        // 设计目标直接矛盾、掩盖客户端拼写错误还产生真实计费。Auto 只经精确别名命中
        // (`auto`/`claude-auto`,resolve 步骤 1/2 的 alias_index 已覆盖),到不了这里。
        return None;
    };

    // 无语义版本的族(国产 deepseek/glm/qwen):直接返回族、版本 None，
    // 交由 resolve 选族内唯一/默认档(精确别名命中通常已在 resolve 更前面拦下)。
    // Auto 不在此列——它已从上面的子串探测移除,只经精确别名命中。
    if matches!(family, Family::DeepSeek | Family::Glm | Family::Qwen) {
        return Some((family, None));
    }

    // 对 claude 系与 minimax:提取族名后紧跟的 major.minor（如 `opus-4-6` / `minimax-m2.1`）。
    let anchor = match family {
        Family::Opus => "opus",
        Family::Sonnet => "sonnet",
        Family::Haiku => "haiku",
        Family::Minimax => "minimax",
        _ => return Some((family, None)),
    };
    let rest = model_lower.split(anchor).nth(1).unwrap_or("");
    // 扫描 rest 里第一个 `<digits><sep><digits>` 模式。
    let bytes: Vec<char> = rest.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let mut j = i;
            let mut major = 0u16;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                major = major.saturating_mul(10).saturating_add((bytes[j] as u8 - b'0') as u16);
                j += 1;
            }
            if j < bytes.len() && (bytes[j] == '-' || bytes[j] == '.') {
                let sep = j;
                let mut k = sep + 1;
                if k < bytes.len() && bytes[k].is_ascii_digit() {
                    let mut minor = 0u16;
                    let mut minor_digits = 0u8;
                    while k < bytes.len() && bytes[k].is_ascii_digit() {
                        minor = minor.saturating_mul(10).saturating_add((bytes[k] as u8 - b'0') as u16);
                        minor_digits += 1;
                        k += 1;
                    }
                    // 真实 minor 版本是 1-2 位(4.5 / 4.6 / 2.1);>2 位几乎必是日期串
                    // (`4-20250514` 的 `20250514`)——不当版本,视作「族 major 无 minor」交由 family-default。
                    // major 同样限制在 1-2 位,防把日期起始误当 major。
                    if minor_digits <= 2 && (i..j).len() <= 2 {
                        return Some((family, Some((major, minor))));
                    }
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
    Some((family, None))
}

/// 在 CATALOG 里找某族确切版本的 spec。
fn find_family_version(family: Family, ver: (u16, u16)) -> Option<&'static ModelSpec> {
    CATALOG
        .iter()
        .find(|s| s.family == family && s.version == Some(ver))
}

/// 某族「最新」档(version 最大者；无版本族取第一个)。用于未知版本回退。
fn newest_in_family(family: Family) -> Option<&'static ModelSpec> {
    CATALOG
        .iter()
        .filter(|s| s.family == family)
        .max_by_key(|s| s.version.unwrap_or((0, 0)))
}

/// 某族默认档:国产无版本族取该族唯一/首个 spec。
fn default_in_family(family: Family) -> Option<&'static ModelSpec> {
    CATALOG.iter().find(|s| s.family == family)
}

/// 是否为 Claude 3/2 老名(不属本代 4.x)。
fn is_legacy_claude(model_lower: &str) -> bool {
    model_lower.contains("claude-3") || model_lower.contains("claude-2")
}

/// 是否允许「已知 family 但未知(更新)版本」回退到同族最新档。默认关(strict)。
/// 环境变量 `KIRO_ALLOW_UNKNOWN_VERSION=1` 开启(类比 LiteLLM 的 pass_through_all_models)。
fn allow_unknown_version() -> bool {
    std::env::var("KIRO_ALLOW_UNKNOWN_VERSION")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// 解析客户端模型名 → CATALOG spec + 命中方式。这是**唯一**的模型识别入口。
///
/// 先剥 `[1m]` 后缀(若有),用剥后名走完整解析,再据 spec.supports_1m 决定最终 is_1m:
/// spec 支持 → is_1m=true(触发 beta 头);spec 不支持 → 忽略后缀 + 告警(不拒绝,更宽容)。
pub fn resolve(model: &str) -> Option<Resolved> {
    let lower = model.to_ascii_lowercase();
    let (base_name, requested_1m) = strip_1m_suffix(&lower);
    // 用剥掉 [1m] 的名字走既有全流程(精确/thinking/结构化)。
    let mut resolved = resolve_inner(&base_name)?;
    if requested_1m {
        if resolved.spec.supports_1m {
            resolved.is_1m = true;
        } else {
            tracing::warn!(
                request_model = %model,
                resolved_kiro_id = %resolved.spec.kiro_id,
                "模型识别:客户端请求 [1m] 变体但该模型不支持 1M 上下文,忽略后缀按普通请求处理(不注入 beta 头)"
            );
        }
    }
    Some(resolved)
}

/// 内部解析(不含 `[1m]` 处理):入参已是剥掉 `[1m]` 的名字。所有 `Resolved` 的 `is_1m` 恒 false,
/// 由外层 [`resolve`] 据 supports_1m 回填。
fn resolve_inner(model: &str) -> Option<Resolved> {
    let raw_lower = model.to_ascii_lowercase();
    let idx = alias_index();

    // 1) 原名精确匹配(主路径，无告警)。
    if let Some(&i) = idx.get(&raw_lower) {
        return Some(Resolved { spec: &CATALOG[i], kind: MatchKind::Exact, is_1m: false });
    }

    // 2) 剥 thinking 后缀再精确匹配。
    let (base, _had_thinking) = strip_thinking(&raw_lower);
    if base != raw_lower {
        if let Some(&i) = idx.get(&base) {
            return Some(Resolved { spec: &CATALOG[i], kind: MatchKind::Exact, is_1m: false });
        }
    }

    // 3) 结构化 family+版本解析。
    if let Some((family, ver_opt)) = parse_family_version(&base) {
        if let Some(ver) = ver_opt {
            // 3a) 确切版本目录里有 → Structured(无损)。
            if let Some(spec) = find_family_version(family, ver) {
                return Some(Resolved { spec, kind: MatchKind::Structured, is_1m: false });
            }
            // 3b) Claude 3/2 老名:历史被静默升到 4.x 贵档——显式近似+告警。
            if is_legacy_claude(&raw_lower) {
                if let Some(spec) = newest_in_family(family) {
                    tracing::warn!(
                        request_model = %model,
                        resolved_kiro_id = %spec.kiro_id,
                        credit_mult = spec.credit_mult,
                        match_kind = "claude3-approx",
                        "模型识别:Claude 老名近似到最近档(语义有损、计费按目标档)——请客户端改用明确新模型名"
                    );
                    return Some(Resolved { spec, kind: MatchKind::Claude3Approx, is_1m: false });
                }
            }
            // 3c) 更新的未知版本(opus-4.9/opus-5):默认拒绝(strict);开关开则回退最新档+告警。
            if allow_unknown_version() {
                if let Some(spec) = newest_in_family(family) {
                    tracing::warn!(
                        request_model = %model,
                        resolved_kiro_id = %spec.kiro_id,
                        credit_mult = spec.credit_mult,
                        match_kind = "unknown-version-fallback",
                        "模型识别:未知版本回退到同族最新档(KIRO_ALLOW_UNKNOWN_VERSION 已开)——可能非用户预期版本"
                    );
                    return Some(Resolved { spec, kind: MatchKind::UnknownVersionFallback, is_1m: false });
                }
            }
            tracing::warn!(
                request_model = %model,
                family = ?family,
                "模型识别:已知家族但目录无此版本，strict 拒绝(设 KIRO_ALLOW_UNKNOWN_VERSION=1 可回退最新档)"
            );
            return None;
        }

        // 3d) 无版本号。国产族取族内默认档(=Structured，国产本就无语义版本)；
        //     claude/minimax 无版本回退族内最新档+告警。
        match family {
            Family::DeepSeek | Family::Glm | Family::Qwen | Family::Auto => {
                if let Some(spec) = default_in_family(family) {
                    return Some(Resolved { spec, kind: MatchKind::Structured, is_1m: false });
                }
            }
            _ => {
                if let Some(spec) = newest_in_family(family) {
                    // Claude 3/2 老名(如 claude-3-opus,无 4-x 版本结构)也走这里。它语义最危险
                    // ——老模型被映射到最新贵档,故单独标 Claude3Approx 并用更醒目的告警。
                    if is_legacy_claude(&raw_lower) {
                        tracing::warn!(
                            request_model = %model,
                            resolved_kiro_id = %spec.kiro_id,
                            credit_mult = spec.credit_mult,
                            match_kind = "claude3-approx",
                            "模型识别:Claude 老名近似到最近档(语义有损、计费按目标档)——请客户端改用明确新模型名"
                        );
                        return Some(Resolved { spec, kind: MatchKind::Claude3Approx, is_1m: false });
                    }
                    tracing::warn!(
                        request_model = %model,
                        resolved_kiro_id = %spec.kiro_id,
                        credit_mult = spec.credit_mult,
                        match_kind = "family-default",
                        "模型识别:无显式版本号，回退到同族最新档——建议客户端带明确版本"
                    );
                    return Some(Resolved { spec, kind: MatchKind::Structured, is_1m: false });
                }
            }
        }
    }

    // 4) 完全未知 → 拒绝。
    None
}

/// 便捷:仅要 Kiro modelId(map_model 语义)。
pub fn resolve_kiro_id(model: &str) -> Option<&'static str> {
    resolve(model).map(|r| r.spec.kiro_id)
}

/// 便捷:该模型名是否请求了受支持的 1M 上下文变体(`[1m]` 后缀且 spec.supports_1m)。
/// 未识别或非 1M → false。供 handler 决定是否给上游注入 `anthropic-beta: context-1m` 头。
pub fn resolve_is_1m(model: &str) -> bool {
    resolve(model).map(|r| r.is_1m).unwrap_or(false)
}

/// 便捷:上下文窗口。未识别默认 200k(与旧行为一致)。
pub fn context_window(model: &str) -> i32 {
    resolve(model).map(|r| r.spec.context_window).unwrap_or(200_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kid(m: &str) -> Option<&'static str> {
        resolve_kiro_id(m)
    }

    #[test]
    fn test_exact_alias_and_dated_names() {
        // 精确别名 + 带日期全名都应稳定命中,不受日期串干扰。
        assert_eq!(kid("claude-opus-4-6"), Some("claude-opus-4.6"));
        assert_eq!(kid("claude-opus-4.6"), Some("claude-opus-4.6"));
        assert_eq!(kid("claude-sonnet-4-5-20250929"), Some("claude-sonnet-4.5"));
        assert_eq!(kid("claude-opus-4-5-20251101"), Some("claude-opus-4.5"));
        assert_eq!(kid("claude-haiku-4-5-20251001"), Some("claude-haiku-4.5"));
        // thinking 后缀剥离
        assert_eq!(kid("claude-opus-4-8-thinking"), Some("claude-opus-4.8"));
    }

    #[test]
    fn test_matchkind_exact_no_warn_path() {
        let r = resolve("claude-opus-4.7").unwrap();
        assert_eq!(r.spec.kiro_id, "claude-opus-4.7");
        assert_eq!(r.kind, MatchKind::Exact);
    }

    #[test]
    fn test_1m_suffix_supported_models() {
        // [1m] 后缀:映射到干净 kiro_id(后缀不进 modelId)+ is_1m=true。
        let r = resolve("claude-opus-4-6[1m]").unwrap();
        assert_eq!(r.spec.kiro_id, "claude-opus-4.6");
        assert_eq!(r.kind, MatchKind::Exact);
        assert!(r.is_1m, "opus-4.6 支持 1M,[1m] 应置 is_1m");
        // kiro_id 干净,不含 [1m]
        assert_eq!(kid("claude-opus-4-6[1m]"), Some("claude-opus-4.6"));
        // sonnet 系 1M 变体
        assert!(resolve("claude-sonnet-5[1m]").unwrap().is_1m);
        assert!(resolve("claude-sonnet-4-6[1m]").unwrap().is_1m);
        assert!(resolve("claude-opus-4-8[1m]").unwrap().is_1m);
        assert!(resolve("claude-opus-4-7[1m]").unwrap().is_1m);
    }

    #[test]
    fn test_1m_without_suffix_is_not_1m() {
        // 无后缀:即便模型支持 1M,is_1m 也为 false(未请求)。
        let r = resolve("claude-opus-4.6").unwrap();
        assert!(!r.is_1m);
        assert!(!resolve_is_1m("claude-sonnet-5"));
    }

    #[test]
    fn test_1m_suffix_unsupported_model_ignored() {
        // 不支持 1M 的模型加 [1m]:后缀被忽略(不拒绝),kiro_id 正常,is_1m=false。
        let r = resolve("claude-opus-4-5[1m]").unwrap();
        assert_eq!(r.spec.kiro_id, "claude-opus-4.5");
        assert!(!r.is_1m, "opus-4.5 不支持 1M,[1m] 应被忽略");
        assert_eq!(kid("claude-sonnet-4-5[1m]"), Some("claude-sonnet-4.5"));
        assert!(!resolve_is_1m("claude-haiku-4-5[1m]"));
    }

    #[test]
    fn test_1m_suffix_unknown_model_still_none() {
        // 未知模型加 [1m]:剥后仍未知 → None(不因后缀假命中)。
        assert_eq!(kid("gpt-4[1m]"), None);
        assert!(!resolve_is_1m("gpt-4[1m]"));
    }

    #[test]
    fn test_1m_and_thinking_combined() {
        // [1m] 与 thinking 后缀共存:先剥 [1m] 再走 thinking 剥离,两者都识别。
        let r = resolve("claude-opus-4-6-thinking[1m]").unwrap();
        assert_eq!(r.spec.kiro_id, "claude-opus-4.6");
        assert!(r.is_1m, "[1m] 应被识别");
    }

    #[test]
    fn test_resolve_is_1m_helper() {
        assert!(resolve_is_1m("claude-opus-4-6[1m]"));
        assert!(!resolve_is_1m("claude-opus-4-6"));
        assert!(!resolve_is_1m("claude-opus-4-5[1m]")); // 不支持 1M
    }

    #[test]
    fn test_claude3_legacy_is_approx_not_silent_upgrade() {
        // 回归:Claude 3 老名过去被静默映射到贵档且零信号。现在必须命中,且标记为 Claude3Approx
        // (调用侧据此打 warn),而非 Exact/Structured 混淆。
        let r = resolve("claude-3-opus-20240229").unwrap();
        assert_eq!(r.spec.family, Family::Opus);
        assert_eq!(r.kind, MatchKind::Claude3Approx);
        let r2 = resolve("claude-3-5-sonnet-20241022").unwrap();
        assert_eq!(r2.spec.family, Family::Sonnet);
        // claude-3-5-sonnet 无 4-x 版本结构 → family-default 路径,但因是 legacy claude 名,
        // 也应标 Claude3Approx(语义最危险:老名→最新贵档,必须可观测)。
        assert_eq!(r2.kind, MatchKind::Claude3Approx);
    }

    #[test]
    fn test_unknown_version_strict_reject_by_default() {
        // 回归:更新的未知版本(opus-4-9 / opus-5)默认 strict 拒绝,不静默降级到 4.6。
        // (未设 KIRO_ALLOW_UNKNOWN_VERSION,默认关)
        assert_eq!(kid("claude-opus-4-9"), None);
        assert_eq!(kid("claude-opus-5-0"), None);
    }

    #[test]
    fn test_unknown_model_rejected() {
        assert_eq!(kid("gpt-4"), None);
        assert_eq!(kid("gemini-2.0"), None);
        assert_eq!(kid(""), None);
    }

    #[test]
    fn test_official_table_models_present() {
        // 对齐 Kiro 官方模型表:补全的 Sonnet 5 / Sonnet 4.0 / Auto 必须能识别(不再被 strict 拒)。
        assert_eq!(kid("claude-sonnet-5"), Some("claude-sonnet-5"));
        assert_eq!(kid("sonnet-5"), Some("claude-sonnet-5"));
        assert_eq!(kid("claude-sonnet-4-0"), Some("claude-sonnet-4.0"));
        assert_eq!(kid("claude-sonnet-4-20250514"), Some("claude-sonnet-4.0"), "旧带日期名归 4.0 不再静默升级");
        assert_eq!(kid("auto"), Some("auto"));
        assert_eq!(kid("claude-auto"), Some("auto"));
    }

    #[test]
    fn test_auto_substring_not_silently_matched() {
        // 回归(对抗 review #1):`auto` 是极常见英文子串,含它的未知名不得被静默映射到 Kiro Auto,
        // 否则既不拒绝也不告警、还产生真实计费,与「未知即拒」矛盾。这类名字必须 strict 拒绝(None)。
        assert_eq!(kid("gpt-4-auto"), None, "含 auto 子串的未知名应拒绝,不静默映射到 Auto");
        assert_eq!(kid("autopilot-v2"), None);
        assert_eq!(kid("autocomplete-model"), None);
        assert_eq!(kid("gemini-auto"), None);
        // 精确别名仍必须命中(不能误伤 Auto 本身)。
        assert_eq!(kid("auto"), Some("auto"), "精确 auto 仍应命中");
        assert_eq!(kid("claude-auto"), Some("auto"), "精确 claude-auto 仍应命中");
    }

    #[test]
    fn test_official_context_windows() {
        // 窗口对齐官方表:DeepSeek 128K、Qwen 256K、Sonnet 5 1M。
        assert_eq!(context_window("deepseek-3.2"), 128_000);
        assert_eq!(context_window("qwen3-coder-next"), 256_000);
        assert_eq!(context_window("claude-sonnet-5"), 1_000_000);
        assert_eq!(context_window("claude-sonnet-4.0"), 200_000);
    }

    #[test]
    fn test_sonnet_newest_is_5() {
        // 无版本 sonnet 回退应指向族内最新=Sonnet 5。
        assert_eq!(resolve("claude-sonnet").unwrap().spec.kiro_id, "claude-sonnet-5");
    }

    #[test]
    fn test_national_models() {
        assert_eq!(kid("qwen"), Some("qwen3-coder-next"));
        assert_eq!(kid("glm"), Some("glm-5"));
        assert_eq!(kid("deepseek"), Some("deepseek-3.2"));
        assert_eq!(kid("minimax-m2.1"), Some("minimax-m2.1"));
        assert_eq!(kid("minimax"), Some("minimax-m2.5")); // 无版本回退最新
    }

    #[test]
    fn test_context_window_from_spec_not_map_inference() {
        // 窗口直接来自 spec,opus/sonnet 4.6 为 1M,4.5 为 200k,国产 200k。
        assert_eq!(context_window("claude-opus-4.6"), 1_000_000);
        assert_eq!(context_window("claude-opus-4.8"), 1_000_000);
        assert_eq!(context_window("claude-opus-4.5"), 200_000);
        assert_eq!(context_window("claude-sonnet-4.6"), 1_000_000);
        assert_eq!(context_window("qwen"), 256_000); // 官方 256K
        assert_eq!(context_window("totally-unknown"), 200_000);
    }

    #[test]
    fn test_catalog_no_cross_model_alias_collision() {
        // registry 卫生:同一个别名/id 绝不能同时指向两个不同 spec(否则解析有歧义)。
        // 允许 kiro_id 自身出现在该 spec 的 aliases 里(冗余但无害);只禁跨 spec 撞键。
        let mut owner: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for (i, spec) in CATALOG.iter().enumerate() {
            let mut keys: Vec<String> = vec![spec.kiro_id.to_ascii_lowercase()];
            keys.extend(spec.aliases.iter().map(|a| a.to_ascii_lowercase()));
            keys.sort();
            keys.dedup(); // 同一 spec 内部重复无害,去掉再查跨 spec
            for k in keys {
                if let Some(&prev) = owner.get(&k) {
                    assert_eq!(prev, i, "别名/id `{}` 跨模型撞键(spec#{} 与 #{})", k, prev, i);
                } else {
                    owner.insert(k, i);
                }
            }
        }
    }
}
