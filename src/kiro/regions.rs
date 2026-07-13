//! Region 单一真相源（分层管理，杜绝"加一处漏一处"）。
//!
//! # 为何集中在此
//! 历史上 region 表散落三处（credentials.rs 的对话白名单、token_manager.rs 的 profile 探测、
//! idc.rs 的 OIDC 探测），每加一种认证源就各自新增/漏改一张表 → 上号 region 处理反复修补。
//! 此模块把三张表按**用途维度**归拢到一处集中管理。三者**维度不同、不可合并**：
//!
//! - [`KIRO_DIALOG_REGIONS`]：Kiro 对话/余额端点（`runtime.{r}.kiro.dev` / `management.{r}.kiro.dev`）
//!   与 profileArn 第 4 段的**合法 region 白名单**。用于严格校验、防止污染值拼进上游 host。
//! - [`OIDC_PROBE_REGIONS`]：AWS **SSO-OIDC** 端点（`oidc.{r}.amazonaws.com`）的探测候选。
//!   用于 IdC device flow 自动探测实例所在 region（start URL 不含 region）。
//! - [`PROFILE_PROBE_REGIONS`]：external_idp/Enterprise 号动态解析 profileArn 时的多 region 探测候选。
//!
//! 原三处 `const` 已改为从本模块 re-export，调用点不变、真相唯一。

/// Kiro 对话/余额端点 + profileArn 的合法 region 白名单。
///
/// 参考 kiro-account-manager 的 SUPPORTED_KIRO_REGIONS。凡要拼进 `*.kiro.dev` host 或
/// 从 profileArn 解析出的 region，都必须命中此白名单，否则回退可信 config（防污染值攻击）。
pub const KIRO_DIALOG_REGIONS: &[&str] = &[
    "us-east-1", "us-east-2", "us-west-1", "us-west-2",
    "ca-central-1", "ca-west-1",
    "eu-west-1", "eu-west-2", "eu-west-3", "eu-central-1", "eu-central-2",
    "eu-north-1", "eu-south-1", "eu-south-2",
    "ap-south-1", "ap-south-2",
    "ap-southeast-1", "ap-southeast-2", "ap-southeast-3", "ap-southeast-4",
    "ap-northeast-1", "ap-northeast-2", "ap-northeast-3",
    "ap-east-1",
    "sa-east-1",
    "me-south-1", "me-central-1",
    "af-south-1",
    "il-central-1",
    "us-gov-east-1", "us-gov-west-1",
    "cn-north-1", "cn-northwest-1",
];

/// AWS SSO-OIDC（`oidc.{region}.amazonaws.com`）端点的探测候选（主流商业 region）。
///
/// 为何需要：IdC start URL（`d-xxxxxxxxxx.awsapps.com`）是**全局域名、不含 region**，用户
/// 无从得知实例所在 region。填错 → device_authorization 400 invalid_request。此表用于
/// 用户填的 region 不对时自动顺次探测纠正（防呆）。**不含 gov/cn 隔离分区**（其 startUrl 域名不同、
/// OIDC 端点分区隔离，探测无意义）。覆盖 AWS SSO-OIDC 实际有端点的主流商业 region。
pub const OIDC_PROBE_REGIONS: &[&str] = &[
    "us-east-1", "us-east-2", "us-west-2",
    "eu-central-1", "eu-central-2", "eu-west-1", "eu-west-2", "eu-west-3",
    "eu-north-1", "eu-south-1", "eu-south-2",
    "ap-southeast-1", "ap-southeast-2", "ap-southeast-3",
    "ap-northeast-1", "ap-northeast-2", "ap-northeast-3",
    "ap-south-1", "ap-east-1",
    "ca-central-1", "sa-east-1",
    "me-central-1", "il-central-1", "af-south-1",
];

/// external_idp / Enterprise 号动态解析 profileArn 的多 region 探测候选。
///
/// 每个 `management.{region}.kiro.dev` 端点只返回本 region 的 profile（实测）。优先探测号自己
/// 的 region（拿到 region 与 ARN 自洽的 profile），此表作兜底补充。覆盖 Kiro 最普遍开通的 region。
/// 将来账号在更多 region 有 profile 可继续扩充此表。
pub const PROFILE_PROBE_REGIONS: &[&str] = &[
    "us-east-1", "eu-central-1", "us-west-2", "eu-west-1",
    "ap-southeast-1", "ap-northeast-1",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_duplicates_within_each_table() {
        for (name, table) in [
            ("KIRO_DIALOG_REGIONS", KIRO_DIALOG_REGIONS),
            ("OIDC_PROBE_REGIONS", OIDC_PROBE_REGIONS),
            ("PROFILE_PROBE_REGIONS", PROFILE_PROBE_REGIONS),
        ] {
            let mut seen = std::collections::HashSet::new();
            for r in table {
                assert!(seen.insert(*r), "{} 含重复 region: {}", name, r);
            }
        }
    }

    #[test]
    fn test_probe_tables_are_subset_of_dialog_whitelist() {
        // OIDC/PROFILE 探测候选都应是合法 Kiro region（在对话白名单内），否则探到也用不了。
        for r in OIDC_PROBE_REGIONS {
            assert!(KIRO_DIALOG_REGIONS.contains(r), "OIDC 候选 {} 不在对话白名单", r);
        }
        for r in PROFILE_PROBE_REGIONS {
            assert!(KIRO_DIALOG_REGIONS.contains(r), "PROFILE 候选 {} 不在对话白名单", r);
        }
    }
}
