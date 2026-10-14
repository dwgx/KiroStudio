//! 计费事件
//!
//! 处理 meteringEvent 类型的事件，解析上游返回的真实 credit 消耗量。
//!
//! 移植自 BenedictKing/kiro.rs（MIT，Copyright kiro.rs contributors），
//! 用于把上游 `meteringEvent` 携带的真实计费量接入本项目的用量统计链路。

use serde::Deserialize;

use crate::kiro::parser::error::ParseResult;
use crate::kiro::parser::frame::Frame;

use super::base::EventPayload;

/// 计费事件
///
/// 上游在响应流末尾返回本次请求消耗的计费量（当前单位为 `credit`）。
/// 这是唯一携带**真实** credit 消耗的事件，token 估算无法替代。
///
/// ⚠️ 上游 metering 事件可选携带真实的 cache_read/cache_creation token 数
/// （Layer 1 真值，k2cc 同款）：字段缺失（`None`）时降级到本地 prefix 估算。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeteringEvent {
    /// 计费单位，当前固定为 `credit`
    #[serde(default)]
    pub unit: String,
    /// 计费单位复数，当前固定为 `credits`
    #[serde(default)]
    pub unit_plural: String,
    /// 本次请求消耗量（**必填**，见 [`deserialize_usage`]）。
    ///
    /// 与两个 cache 观测字段相反，这里**刻意不写 `#[serde(default)]`**：那会把「字段缺失 /
    /// 上游改名」悄悄变成 `0.0`，而下游是
    /// `credits_used = Some(prev + metering.usage)`（stream.rs / handlers.rs）——于是一次
    /// **真实计费**被记成「上游明确说消耗 0」，请求被静默少计。缺失时唯一正确的语义是
    /// 「上游没给」= `credits_used` 保持 `None`，而这要求本帧解析失败（解码层跳过该帧）。
    #[serde(deserialize_with = "deserialize_usage")]
    pub usage: f64,
    /// 命中缓存读取的 token 数（上游真值，缺失为 None）
    #[serde(default, deserialize_with = "deserialize_optional_token_count")]
    pub cache_read_input_tokens: Option<i32>,
    /// 本次新建缓存写入的 token 数（上游真值，缺失为 None）
    #[serde(default, deserialize_with = "deserialize_optional_token_count")]
    pub cache_creation_input_tokens: Option<i32>,
}

/// `usage` 的**严格**反序列化：缺失 / 非数值 / 负数一律让本帧失败。
///
/// 为什么与上面两个 cache 观测字段的「一律降级为 None」相反：`usage` 是**唯一被消费**的量，
/// 下游 `credits_used = Some(prev + usage)` 累加后写进生命周期 `total_credits_used`
/// （落盘 kiro_stats.json）。缺失走 `default` 会变成「真实消耗 0」（见字段注释）；负数则会把
/// 负数花费写进用量明细与聚合（`add_credits` 只挡 `credits <= 0`，挡不住已经记进
/// `record.credits_used` 的那个数）。本帧失败 ⇒ 解码层跳过该帧并打日志，
/// `credits_used` 保持 `None` = 「上游没给」——比伪造一个数更接近真值。
///
/// `is_finite()` 是纵深防御而非当前可达路径：serde_json 对 `1e400` 这类越界数值返回
/// `NumberOutOfRange`（见 serde_json `de.rs` 的 `f64_from_parts`/`parse_exponent_overflow`），
/// JSON 语法也不允许 `NaN`/`Infinity` 字面量。留着是为了将来换反序列化入口（`from_value`、
/// 手写构造）时不至于把 `inf`/`NaN` 放进累加链。
fn deserialize_usage<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let usage = f64::deserialize(deserializer)?;
    if !usage.is_finite() || usage < 0.0 {
        return Err(serde::de::Error::custom(format!(
            "meteringEvent.usage 非法（{usage}）：必须是有限的非负数"
        )));
    }
    Ok(usage)
}

/// 可选 token 计数的**容错**反序列化：非整数 / 超出 `i32` / 负数一律 `None`。
///
/// 为什么不让 serde 直接失败：这两个字段与 `usage`（本事件唯一真实计费量）同处一个
/// struct，而本帧解析失败 ⇒ **整帧被解码层跳过**（`usage` 一起丢）—— 一个**观测性**字段
/// （缺失时下游会降级到本地 prefix 估算，见字段注释）不该有「拖垮计费真值」这种权力。
/// 负数不是合法 token 数，同样按「没给」处理：它会被下游当 Layer 1 真值用，留着比丢掉更坏。
fn deserialize_optional_token_count<'de, D>(deserializer: D) -> Result<Option<i32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::Number(n) => n
            .as_i64()
            .and_then(|v| i32::try_from(v).ok())
            .filter(|v| *v >= 0),
        _ => None,
    })
}

impl EventPayload for MeteringEvent {
    fn from_frame(frame: &Frame) -> ParseResult<Self> {
        let event: Self = frame.payload_as_json()?;
        // C-2 观测（解析层唯一埋点，见 cache_truth.rs）：本事件的 cache 字段是四层
        // 降级链的 Layer 1 真值来源，带字段时打 debug + 计数。解析失败（Err）时整帧
        // 被跳过，不观测 —— 分母口径 = 解析成功的帧。
        super::cache_truth::observe_metering(frame.event_type().unwrap_or("meteringEvent"), &event);
        Ok(event)
    }
}

impl std::fmt::Display for MeteringEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.unit.is_empty() {
            write!(f, "{:.6}", self.usage)
        } else {
            write!(f, "{:.6} {}", self.usage, self.unit)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metering_event_deserialize() {
        let json = r#"{"unit":"credit","unitPlural":"credits","usage":1.5}"#;
        let event: MeteringEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.unit, "credit");
        assert_eq!(event.unit_plural, "credits");
        assert_eq!(event.usage, 1.5);
        assert_eq!(event.cache_read_input_tokens, None);
        assert_eq!(event.cache_creation_input_tokens, None);
    }

    #[test]
    fn test_metering_event_defaults() {
        // `{}` 缺 `usage` → 必须**报错**，不得默认成 0.0。
        // 回退即 FAIL：`#[serde(default)]` 会把「字段缺失 / 上游改名」记成「真实消耗 0」
        // （下游 `credits_used = Some(prev + usage)`），请求被静默少计且无任何报错。
        assert!(
            serde_json::from_str::<MeteringEvent>("{}").is_err(),
            "缺 usage 的 meteringEvent 必须解析失败：宁可不计（None），也不能伪造 0 消耗"
        );

        // 其余字段（单位）仍可缺省；`usage` 显式给 0 是**上游确实说 0**，照收。
        let event: MeteringEvent = serde_json::from_str(r#"{"usage":0}"#).unwrap();
        assert_eq!(event.unit, "");
        assert_eq!(event.unit_plural, "");
        assert_eq!(event.usage, 0.0);
        assert_eq!(event.cache_read_input_tokens, None);
        assert_eq!(event.cache_creation_input_tokens, None);
    }

    /// `usage` 是唯一被消费的计费量：非法值必须让**本帧**失败（解码层跳过、`credits_used`
    /// 保持 `None`），绝不能带着坏数进入下游累加。
    ///
    /// 回退即 FAIL：`-5` 会被记成负数花费并写进用量库 / 聚合；`1e400` 由 serde_json 以
    /// `NumberOutOfRange` 拒绝（见 `deserialize_usage` 的注释）。
    #[test]
    fn test_metering_event_rejects_bad_usage() {
        for bad in [r#"{"usage":-5}"#, r#"{"usage":1e400}"#, r#"{"usage":"1.5"}"#] {
            assert!(
                serde_json::from_str::<MeteringEvent>(bad).is_err(),
                "{bad} 必须被拒绝：坏计费量不得进入下游累加链"
            );
        }
        // 整数形态必须照收（上游可能发 `usage: 2`）。
        let event: MeteringEvent = serde_json::from_str(r#"{"usage":2}"#).unwrap();
        assert_eq!(event.usage, 2.0);
    }

    #[test]
    fn test_metering_event_parses_cache_fields() {
        // 上游 metering 事件可选携带 cache 真值（Layer 1）——必须能解析。
        let json = r#"{"usage":1.5,"cacheReadInputTokens":600,"cacheCreationInputTokens":200}"#;
        let event: MeteringEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.cache_read_input_tokens, Some(600));
        assert_eq!(event.cache_creation_input_tokens, Some(200));
    }

    /// 观测性字段坏了不得拖垮整条事件：非整数 / 超 i32 / 负数一律降级为 `None`，`usage` 照旧。
    ///
    /// 回退即 FAIL：改回裸 `#[serde(default)]` 后 `600.5` 会让**整个** `MeteringEvent`
    /// 反序列化失败 —— 真实计费量 `usage` 被一起丢掉（且经 `?` 传播成整条流报错）。
    #[test]
    fn test_metering_event_bad_cache_fields_degrade_to_none() {
        let json =
            r#"{"usage":1.5,"cacheReadInputTokens":600.5,"cacheCreationInputTokens":3000000000}"#;
        let event: MeteringEvent = serde_json::from_str(json)
            .expect("坏 cache 字段不得让整个 metering 事件解析失败（会丢掉 usage）");
        assert_eq!(event.usage, 1.5);
        assert_eq!(event.cache_read_input_tokens, None, "非整数应降级为 None");
        assert_eq!(
            event.cache_creation_input_tokens, None,
            "超出 i32 应降级为 None"
        );

        // 负数不是合法 token 数：同样按「没给」处理（否则会被下游当 Layer 1 真值用）。
        let event: MeteringEvent =
            serde_json::from_str(r#"{"usage":1.0,"cacheReadInputTokens":-5}"#)
                .expect("负数也不该报错");
        assert_eq!(event.cache_read_input_tokens, None);
    }

    #[test]
    fn test_metering_event_display() {
        let event = MeteringEvent {
            unit: "credit".to_string(),
            unit_plural: "credits".to_string(),
            usage: 2.25,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
        };
        assert_eq!(format!("{event}"), "2.250000 credit");
    }
}
