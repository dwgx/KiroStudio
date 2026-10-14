//! 模型「首次出现」日期自检
//!
//! # 为什么需要
//! 上游的模型列表不携带发布时间字段，「最近新增」只能由服务自己记：每次成功拉到
//! **池级**模型列表（后台巡检 `probe_model_catalog_round`），就把**之前没见过的**模型 ID
//! 连同当天日期写进 `kiro_model_seen.json`（与 credentials.json 同目录）。后续新增的模型
//! 因此无需改代码即可带上「首次出现」日期。面板消费面（模型列表 NEW 徽章）本仓尚未落地：
//! 日期先如实落盘，接线时无需回填历史。
//!
//! # 首次运行的基线（防升级当天整表亮 NEW）
//! 记录文件不存在（首次升级到本版本，或文件被删/损坏）时，当前已有的全部模型只登记为
//! 「基线」（日期为 `null`），不算新增；基线之后才出现的模型才记当天日期。
//! 没有这一层，升级当天整张模型表都会亮成 NEW。
//!
//! # 手动覆盖
//! `MODEL_RELEASE_DATES` 给「先于本自检机制存在」的模型保留日期：它们会随首次基线被
//! 登记成 null（永久失去徽章资格），想保留徽章只能在这里手动登记。覆盖优先于自检记录。
//!
//! # 只登记池级拉取（刻意不登记单号视图）
//! 与 kiro2cc 参考实现同款判据：**单账号视图只查询、不登记** —— 账号/上游之间模型集合
//! 可能不同，单号列表不构成「池里新增」。本仓唯一登记入口是后台巡检的池级全量拉取
//! （`MultiTokenManager::probe_model_catalog_round` 成功分支）；面板手动探测
//! （`AdminService::probe_upstream_models`）与创建前临时探测（`probe_models_standalone`）
//! 刻意不登记。
//!
//! # 局限（写下来防被当成官方发布日期）
//! 记录的是「本代理第一次看到该模型的日期」，不是官方发布日期；代挂上游之间的模型
//! 集合本就不同，任一上游带来的新 ID 都会按当天登记（参考实现同款已知取舍）。

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::NaiveDate;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// 记录文件名（位于 credentials.json 同目录，与冷却状态同源）。
pub(crate) const SEEN_FILE_NAME: &str = "kiro_model_seen.json";

/// 手动覆盖的「上线日」（模型 ID → `YYYY-MM-DD`），优先于自检记录。
///
/// 只登记「先于本自检机制存在」的模型 —— 首次基线会把它们记成 null，只有这里能保住
/// 徽章资格。日期取该模型在**本代理**可调用的日期（上线日），不是官方发布日期：
/// 5.5 代三档（含 Fable 5.1）随 `c61b296` 交付、当日起对本代理可调用。
const MODEL_RELEASE_DATES: &[(&str, &str)] = &[
    ("claude-opus-5.5", "2026-10-13"),
    ("claude-sonnet-5.5", "2026-10-13"),
    ("claude-fable-5.1", "2026-10-13"),
];

/// 持久化内容：模型 ID → 首次出现日期（`None` = 基线登记，不算新增）。
#[derive(Debug, Default, Serialize, Deserialize)]
struct SeenState {
    models: BTreeMap<String, Option<String>>,
}

impl SeenState {
    /// 登记本次看到的模型，返回本次新登记的 ID（空 = 无变化，调用方不必落盘）。
    ///
    /// `baseline` 为 true 时新模型登记为 `None`（不算新增）—— 首次运行/删文件后的第一次
    /// 同步走这条，防升级当天整表亮 NEW。
    fn observe(&mut self, ids: &[String], today: &str, baseline: bool) -> Vec<String> {
        let mut added = Vec::new();
        for id in ids {
            // 空 ID 与已登记 ID 都跳过：只登记「第一次见到」的，已有日期绝不改写。
            if id.is_empty() || self.models.contains_key(id) {
                continue;
            }
            self.models.insert(
                id.clone(),
                if baseline {
                    None
                } else {
                    Some(today.to_string())
                },
            );
            added.push(id.clone());
        }
        added
    }
}

/// 模型「首次出现」日期追踪器（进程内单例，由 `MultiTokenManager` 持有）。
pub(crate) struct ModelReleaseTracker {
    /// `None` = 纯内存（无凭据路径的场景/测试）：不落盘，避免并行测试共享同一文件
    /// 互相污染（与 `CooldownManager` 的无路径构造同款取舍）。
    path: Option<PathBuf>,
    /// `None` = 尚未建立基线（文件不存在/损坏/首次运行）。
    state: Mutex<Option<SeenState>>,
}

impl ModelReleaseTracker {
    /// 从 `path` 加载记录；文件不存在或损坏 → 视为尚未建立基线（下次 observe 重建）。
    pub(crate) fn load(path: Option<PathBuf>) -> Self {
        let state = path.as_ref().and_then(|p| {
            let content = std::fs::read_to_string(p).ok()?;
            match serde_json::from_str::<SeenState>(&content) {
                Ok(s) => Some(s),
                Err(e) => {
                    tracing::warn!(
                        path = ?p,
                        "模型首次出现记录解析失败，将重建基线（半截 JSON 不得当成有效状态）: {e}"
                    );
                    None
                }
            }
        });
        Self {
            path,
            state: Mutex::new(state),
        }
    }

    /// 登记一次**池级**成功拉取到的模型列表（内部取当天 UTC 日期）。
    ///
    /// 只应在「上游实时响应成功且非空」时调用：空列表直接跳过 —— 异常空响应若建立出
    /// 空基线，之后所有模型都会被当成新增（与参考实现同款守卫）。
    pub(crate) fn observe(&self, ids: &[String]) {
        self.observe_on(ids, chrono::Utc::now().date_naive());
    }

    /// [`Self::observe`] 的可注入日期版本（测试用固定日期，避免跨午夜抖动）。
    fn observe_on(&self, ids: &[String], today: NaiveDate) {
        if ids.is_empty() {
            return;
        }
        let today = today.format("%Y-%m-%d").to_string();
        let mut guard = self.state.lock();
        let baseline = guard.is_none();
        let state = guard.get_or_insert_with(SeenState::default);
        let added = state.observe(ids, &today, baseline);
        if added.is_empty() {
            return;
        }
        if baseline {
            tracing::info!(
                count = added.len(),
                "模型首次出现自检：建立基线（{} 个模型，日期为 null，不算新增）",
                added.len()
            );
        } else {
            tracing::info!(
                ids = ?added,
                "模型首次出现自检：登记 {} 个新模型（首次出现日期 {}）",
                added.len(),
                today
            );
        }
        self.save(state);
    }

    /// 查询模型「首次出现」日期（`YYYY-MM-DD`）：手动覆盖优先，其次自检记录；
    /// 基线登记的模型与从未见过的模型都返回 `None`（= 不亮 NEW）。
    ///
    /// 消费面（面板模型列表 NEW 徽章）尚未存在；这是刻意保留的查询入口，供消费面
    /// 落地时直接接线，当前仅由测试与未来的面板路径使用。
    #[allow(dead_code)] // 消费面未建（见方法文档）：为未来的面板消费面与测试保留
    pub(crate) fn release_date(&self, model_id: &str) -> Option<String> {
        if let Some((_, date)) = MODEL_RELEASE_DATES.iter().find(|(id, _)| *id == model_id) {
            return Some((*date).to_string());
        }
        self.state
            .lock()
            .as_ref()
            .and_then(|s| s.models.get(model_id))
            .and_then(|d| d.clone())
    }

    /// 落盘（仅状态有变化时由 [`Self::observe_on`] 调用）。
    ///
    /// 走 `write_atomic`（temp→fsync→rename）：半截 JSON 会让下次启动回退成「重建基线」，
    /// 把已登记的日期全部抹掉 —— 与「升级当天整表亮 NEW」相对的另一面数据丢失。
    fn save(&self, state: &SeenState) {
        let Some(path) = &self.path else { return };
        let json = match serde_json::to_string_pretty(state) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!("序列化模型首次出现记录失败: {e}");
                return;
            }
        };
        if let Err(e) = crate::common::fs_atomic::write_atomic(path, json.as_bytes()) {
            tracing::warn!(path = ?path, "保存模型首次出现记录失败: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").expect("测试日期字面量必须是合法 YYYY-MM-DD")
    }

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    /// 手动覆盖必须可查（5.5 代三档都在表里）——它们「先于自检存在」，首次基线会把它们
    /// 记成 null，只有覆盖表能保住日期。
    #[test]
    fn manual_overrides_are_queryable() {
        let t = ModelReleaseTracker::load(None);
        assert_eq!(
            t.release_date("claude-opus-5.5").as_deref(),
            Some("2026-10-13")
        );
        assert_eq!(
            t.release_date("claude-sonnet-5.5").as_deref(),
            Some("2026-10-13")
        );
        assert_eq!(
            t.release_date("claude-fable-5.1").as_deref(),
            Some("2026-10-13")
        );
        assert_eq!(
            t.release_date("claude-opus-5"),
            None,
            "未登记模型不得凭猜测给日期"
        );
    }

    /// 覆盖表必须是 `YYYY-MM-DD`：消费面按日期字符串直接解析，格式错了静默失效。
    #[test]
    fn all_overrides_use_iso_date_format() {
        for (id, date) in MODEL_RELEASE_DATES {
            assert_eq!(date.len(), 10, "模型 {id} 的日期格式非法: {date}");
            assert!(
                NaiveDate::parse_from_str(date, "%Y-%m-%d").is_ok(),
                "模型 {id} 的日期无法解析: {date}"
            );
        }
    }

    /// 删文件（或首次升级）后的第一次同步 = 基线：已有模型全部 null，不算新增
    /// （否则升级当天整表亮 NEW）。
    #[test]
    fn first_observation_is_baseline_not_new() {
        let t = ModelReleaseTracker::load(None);
        t.observe_on(&ids(&["a", "b"]), day("2026-10-13"));
        assert_eq!(t.release_date("a"), None);
        assert_eq!(t.release_date("b"), None);
    }

    /// 基线之后新出现的 ID 记当天日期（新增一个 ID → 日期=当天）；基线里的不得被改写。
    #[test]
    fn model_appearing_after_baseline_gets_first_seen_date() {
        let t = ModelReleaseTracker::load(None);
        t.observe_on(&ids(&["a", "b"]), day("2026-10-13"));
        t.observe_on(&ids(&["a", "b", "c"]), day("2026-10-20"));
        assert_eq!(t.release_date("a"), None, "基线里的模型不得被后续观察改写");
        assert_eq!(t.release_date("c").as_deref(), Some("2026-10-20"));
    }

    /// 首次出现日期只记一次：巡检每轮都重复看到同一批模型，日期不得漂移。
    #[test]
    fn first_seen_date_is_not_overwritten() {
        let t = ModelReleaseTracker::load(None);
        t.observe_on(&ids(&["a"]), day("2026-10-13"));
        t.observe_on(&ids(&["a", "c"]), day("2026-10-20"));
        t.observe_on(&ids(&["a", "c"]), day("2026-11-30"));
        assert_eq!(t.release_date("c").as_deref(), Some("2026-10-20"));
    }

    /// 空列表不建立基线：异常空响应之后的第一次真实列表仍算基线（否则全表算新增）。
    #[test]
    fn empty_list_does_not_establish_baseline() {
        let t = ModelReleaseTracker::load(None);
        t.observe_on(&[], day("2026-10-13"));
        t.observe_on(&ids(&["a"]), day("2026-10-14"));
        assert_eq!(t.release_date("a"), None);
    }

    /// 覆盖优先于自检记录（含基线登记的 null）。
    ///
    /// 必须让覆盖 ID **在基线建立之后**才首次出现 —— 否则它只会被记成 `null`，
    /// 此时「先查覆盖、后查自检记录」与相反顺序都会返回覆盖值，测试恒绿、钉不住顺序。
    #[test]
    fn override_takes_priority_over_recorded_baseline() {
        let t = ModelReleaseTracker::load(None);
        // 先让一个非覆盖 ID 建立基线（首调 ⇒ baseline，不产生任何日期）。
        t.observe_on(&ids(&["baseline-only"]), day("2026-10-13"));
        // 覆盖 ID 之后才首次出现 ⇒ 自检记录会是非空的 2026-10-20，与覆盖值不同。
        t.observe_on(&ids(&["claude-opus-5.5"]), day("2026-10-20"));
        assert_eq!(
            t.release_date("claude-opus-5.5").as_deref(),
            Some("2026-10-13")
        );
        assert_ne!(
            t.release_date("claude-opus-5.5").as_deref(),
            Some("2026-10-20"),
            "查表顺序颠倒（先自检记录、后覆盖）会让这里返回自检日期"
        );
    }

    /// 落盘 round-trip：重启（重新 load）不重建基线、已记日期保留、新 ID 记新当天；
    /// 文件内容为消费面契约（`models: {id: 日期|null}`）。
    #[test]
    fn persists_and_reloads_across_restarts() {
        let dir =
            std::env::temp_dir().join(format!("kiro-model-release-rt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(SEEN_FILE_NAME);
        // 清掉上次运行可能残留的记录，保证本测试的基线是本次建立。
        let _ = std::fs::remove_file(&path);

        let t = ModelReleaseTracker::load(Some(path.clone()));
        t.observe_on(&ids(&["a"]), day("2026-10-13"));
        t.observe_on(&ids(&["a", "c"]), day("2026-10-20"));

        let t2 = ModelReleaseTracker::load(Some(path.clone()));
        assert_eq!(
            t2.release_date("a"),
            None,
            "重启不得把基线重建（a 仍为基线）"
        );
        assert_eq!(t2.release_date("c").as_deref(), Some("2026-10-20"));
        t2.observe_on(&ids(&["a", "c", "d"]), day("2026-10-25"));
        assert_eq!(t2.release_date("d").as_deref(), Some("2026-10-25"));

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("\"c\": \"2026-10-20\""),
            "落盘内容应含已登记日期: {raw}"
        );
        assert!(raw.contains("\"a\": null"), "基线条目应为 null: {raw}");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 文件损坏 → 重建基线（不把半截 JSON 当有效状态；损坏是「删文件」的等价物），
    /// 且重建后落盘的是合法 JSON（下一次启动不再走损坏分支）。
    #[test]
    fn corrupted_file_falls_back_to_new_baseline() {
        let dir =
            std::env::temp_dir().join(format!("kiro-model-release-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(SEEN_FILE_NAME);
        std::fs::write(&path, "not json").unwrap();

        let t = ModelReleaseTracker::load(Some(path.clone()));
        t.observe_on(&ids(&["a"]), day("2026-10-13"));
        assert_eq!(t.release_date("a"), None, "损坏文件不得被当成已建基线");
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            serde_json::from_str::<SeenState>(&raw).is_ok(),
            "重建后应落盘合法 JSON: {raw}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
