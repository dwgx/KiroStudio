//! 余额缓存：账号键 TTL、展示保留、磁盘持久化、温和刷新。
//!
//! 由 `service.rs` 以 `#[path]` 接入。`AdminService` 仍在父文件；本文件只持缓存簇。
//! `classify_balance_error` / `push_balance_snapshots_to_scheduler` 留在父文件
//! （前者还被 overage 等非缓存路径调用）。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::admin::types::{BalanceResponse, CachedBalanceItem, CachedBalancesResponse};
use crate::kiro::token_manager::MultiTokenManager;

use super::AdminServiceError;

/// 余额缓存【新鲜度】阈值（秒），5 分钟。
/// 仅用于 `get_balance` 的按需（hover）路径：决定是否需要重新向上游拉取。
/// 注意：这【不是】展示缓存的丢弃阈值——展示用 `BALANCE_CACHE_DISPLAY_MAX_AGE_SECS`。
const BALANCE_CACHE_TTL_SECS: i64 = 300;

/// 余额查询等待上游的硬上限（秒）。
///
/// 取 6s 的理由：上游 `web_portal` 自己的 client 超时是 30s/60s，而前端 axios 是 15s ——
/// 若不在这一层设更短的闸门，用户必然先看到前端超时失败。6s 足够正常往返（实测上游
/// 健康时是百毫秒级），又远低于前端超时，使"慢"能被转成"显示上次已知值 + stale 标记"
/// 而不是转圈或报错。
const BALANCE_UPSTREAM_TIMEOUT_SECS: u64 = 6;

/// 余额缓存【展示保留】上限（秒），7 天。
///
/// 关键修复（对齐 Foxfishc 的“重启后余额缓存不丢”目标，但契合我方单一数据源架构）：
/// 展示路径（启动加载 + 批量缓存端点）绝不能用 5 分钟的新鲜度阈值去丢弃条目，
/// 否则会出现两个症状：
///   1. 重启后磁盘缓存几乎必然 >5 分钟 → 被丢弃 → 前端显示“未知”；
///   2. 后台温和刷新间隔为 30 分钟，但展示缓存 5 分钟后即被过滤 →
///      每 30 分钟里有 25 分钟批量端点返回空 → 前端长期“未知”。
/// 因此展示缓存保留最近 7 天的最后已知值，并把 `cached_at` 交给前端判断新鲜度
/// （前端展示“截至 X 分钟前”而非直接抹掉数字）。超过 7 天才丢弃，避免无界陈旧。
const BALANCE_CACHE_DISPLAY_MAX_AGE_SECS: i64 = 7 * 24 * 3600;

/// 缓存的余额条目（含时间戳）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct CachedBalance {
    /// 缓存时间（Unix 秒）
    pub(super) cached_at: f64,
    /// 缓存的余额数据
    pub(super) data: BalanceResponse,
}

impl super::AdminService {
    /// 获取凭据余额（带缓存 + 上游超时降级）
    ///
    /// # 为什么需要超时降级
    ///
    /// 上游链路是 `fetch_balance` → `token_manager.get_usage_limits_for` →
    /// `kiro::web_portal`（打 app.kiro.dev），而那里的 client 超时是 **30s / 60s**。
    /// 此前中间**没有任何降级**：缓存一过期，面板点余额就干等 30 秒；
    /// 前端 axios 是 15s 超时，所以用户先看到失败、而后端还在等（线上 Caddy 日志里
    /// 该端点有 5 次 502）。
    ///
    /// 现在：上游超过 [`BALANCE_UPSTREAM_TIMEOUT_SECS`] 就放弃，**有旧缓存就返旧缓存并标 stale**，
    /// 让面板显示"上次已知值 + 过期提示"而不是转圈或报错。只有连旧缓存都没有时才报错。
    ///
    /// # `force`：跳过 [`BALANCE_CACHE_TTL_SECS`] 这道新鲜度门
    ///
    /// 用户明确反馈「额度/积分刷新太慢」，而在 `force` 之前**没有任何路径**能让用户
    /// 主动取一次真值：面板列表读的是缓存（30 分钟才由后台刷一次），而本端点在
    /// 5 分钟 TTL 内直接返缓存 ⇒ 连点两次「查看余额」拿到的是同一个数字、零上游往返，
    /// 看起来就是"刷新没反应"。
    ///
    /// 风险边界（封号红线）：`force` **只作用于显式的单号请求**，不存在批量入口
    /// （`get_cached_balances` 恒零上游，后台刷新仍是 30 分钟 + 逐个 4 秒间隔）。
    /// 与既有的 `GET /credentials/{id}/overage`（每次调用都真打上游）同一量级。
    pub async fn get_balance(
        &self,
        id: u64,
        force: bool,
    ) -> Result<BalanceResponse, AdminServiceError> {
        let cache_key = self.balance_cache_key(id);
        // 先查缓存（新鲜即直接返；force 时只取降级值，不早返）
        let stale_fallback = {
            let cache = self.balance_cache.lock();
            match cache.get(&cache_key) {
                Some(cached) => {
                    let now = Utc::now().timestamp() as f64;
                    if !force && (now - cached.cached_at) < BALANCE_CACHE_TTL_SECS as f64 {
                        tracing::debug!("凭据 #{} 余额命中缓存", id);
                        return Ok(cached.data.clone());
                    }
                    // 过期但可用：留作上游失败/超时时的降级值。
                    Some(cached.data.clone())
                }
                None => None,
            }
        };

        // 缓存未命中或已过期，从上游获取 —— 但**绝不为上游慢而无限等**。
        let balance = match tokio::time::timeout(
            std::time::Duration::from_secs(BALANCE_UPSTREAM_TIMEOUT_SECS),
            self.fetch_balance(id),
        )
        .await
        {
            Ok(r) => r?,
            Err(_) => {
                // 超时：有旧值就返旧值（标 stale），没有才报错。
                // 这是"面板可读性优先于数值新鲜度"的刻意取舍 —— 余额只用于展示，
                // 不参与调度决策（balanceWeightEnabled 走的是独立的 BalanceSnapshot 回推）。
                if let Some(mut stale) = stale_fallback {
                    stale.stale = true;
                    tracing::warn!(
                        credential_id = id,
                        timeout_secs = BALANCE_UPSTREAM_TIMEOUT_SECS,
                        "余额上游超时，返回上次已知值并标记 stale（面板显示过期提示而非报错）"
                    );
                    return Ok(stale);
                }
                tracing::warn!(
                    credential_id = id,
                    timeout_secs = BALANCE_UPSTREAM_TIMEOUT_SECS,
                    "余额上游超时且无历史缓存可降级"
                );
                return Err(AdminServiceError::UpstreamTimeout(id));
            }
        };

        // 落缓存 + **同步重置花费基线**（按账号键，于是同 key 的全部分身立刻共享这次结果）。
        // ⚠️ 绝不在这里内联 `cache.insert`：那会漏掉基线重置 → 面板把已含在真值里的花费
        // 再扣一次（见 `commit_fresh_balance` 的算例）。
        self.commit_fresh_balance(cache_key, balance.clone());

        Ok(balance)
    }

    /// 余额缓存的键：**同一个上游账号只有一个键**。
    ///
    /// - api_key 号（`ksk_`）→ `sha256(kiroApiKey)`。同 key 的全部分身共享一条缓存 ⇒
    ///   任一份刷新即全组同步，且上游 `getUsageLimits` 探测从 N 次降到 1 次。
    /// - 其余（OAuth：social / idc / external_idp）→ 十进制 `id`，保持原行为。
    ///
    /// # 为什么 OAuth 必须继续按 id
    ///
    /// 它们没有 `kiroApiKey`，无从算账号指纹。若为了"统一"给它们编一个共享键，
    /// 会把**互不相关的多个 OAuth 账号**的余额混成一条 —— 那是比不同步严重得多的错误
    /// （面板会显示别人的额度）。判据复用 `is_api_key_credential()`，与
    /// `api_key_hash` 字段的算法（`token_manager.rs:5484`：仅 api_key 号才算 sha256）同源。
    ///
    /// # 取不到凭据时
    ///
    /// 回落到 id。这只发生在凭据刚被删除的竞态里，此时缓存键正确与否都无意义。
    pub(super) fn balance_cache_key(&self, id: u64) -> String {
        match self.token_manager.export_credential(id) {
            Some(c) if c.is_api_key_credential() => match c.kiro_api_key.as_deref() {
                Some(k) => crate::kiro::token_manager::sha256_hex(k),
                // api_key 号但 key 为空：配置无效（`InvalidConfig` 会禁用它），
                // 回落 id 而不是拿空串当共享键——空串会把所有这类号混成一条。
                None => id.to_string(),
            },
            _ => id.to_string(),
        }
    }

    /// 删凭据后清理它的余额缓存 —— **仅当没有别的凭据还共享同一个账号键**。
    ///
    /// # 为什么必须有条件
    ///
    /// 缓存按账号键存（`balance_cache_key`），一条被同 key 的 N 份分身共享。
    /// 无条件 `remove` 会让「删掉一份分身」把**整组**的余额缓存清掉 ⇒ 剩下的份
    /// 面板显示"暂无数据"，直到下次刷新（默认 30 分钟）或用户手点查余额。
    ///
    /// # 调用约定：`key` 必须在删除**之前**算好
    ///
    /// `balance_cache_key` 走 `export_credential`，凭据删掉后它返 `None` ⇒ 回落成 id
    /// 字符串 ⇒ 清的是一个不存在的键，真正那条泄漏在缓存里。所以键由调用方在删除前传入。
    pub(super) fn prune_balance_cache_for_deleted(&self, key: &str) {
        // 还有别的凭据共享这个键吗？（此刻目标凭据已从池中移除）
        let still_shared = self
            .token_manager
            .snapshot()
            .entries
            .iter()
            .any(|e| self.balance_cache_key(e.id) == key);
        if still_shared {
            return;
        }
        {
            let mut cache = self.balance_cache.lock();
            cache.remove(key);
        }
        self.save_balance_cache();
    }

    /// 账号缓存键 → 共享它的**全部**凭据 id。
    ///
    /// 缓存按账号键存（一条），而面板与调度器都按**凭据 id** 消费 —— 所以读回时必须把
    /// 一条展开成 N 条。这就是「同 key 的分身余额必然一致」在 UI 上真正生效的地方：
    /// 它们读的是同一条缓存，不存在各自一份、谁刷谁新的可能。
    ///
    /// 含禁用号：面板要显示禁用号的最后已知余额（判断是不是额度耗尽导致的禁用）。
    pub(super) fn balance_key_to_ids(&self) -> HashMap<String, Vec<u64>> {
        let mut out: HashMap<String, Vec<u64>> = HashMap::new();
        for e in self.token_manager.snapshot().entries {
            out.entry(self.balance_cache_key(e.id))
                .or_default()
                .push(e.id);
        }
        out
    }

    /// 从上游获取余额（无缓存）
    async fn fetch_balance(&self, id: u64) -> Result<BalanceResponse, AdminServiceError> {
        let usage = self
            .token_manager
            .get_usage_limits_for(id)
            .await
            .map_err(|e| self.classify_balance_error(e, id))?;

        // overage（超额）感知：开了 Online Overage 的号 base 耗尽后仍有额度，
        // 用 effective 变体（base + overage cap）计算 remaining/百分比，避免展示失真。
        let overage_enabled = usage.overage_enabled();
        let overage_cap = usage.overage_cap_for(overage_enabled);
        let current_usage = usage.current_usage();
        let usage_limit = usage.usage_limit();
        let effective_limit = usage.effective_usage_limit_for(overage_enabled);
        let remaining = usage.effective_remaining_for(overage_enabled);
        let usage_percentage = if effective_limit > 0.0 {
            (current_usage / effective_limit * 100.0).min(100.0)
        } else {
            0.0
        };

        Ok(BalanceResponse {
            id,
            subscription_title: usage.subscription_title().map(|s| s.to_string()),
            current_usage,
            usage_limit,
            remaining,
            usage_percentage,
            next_reset_at: usage.next_date_reset,
            overage_enabled,
            overage_cap,
            effective_limit,
            // 从上游新取的值 = 新鲜。降级路径在 get_balance 里显式置 true。
            stale: false,
            // 直接从上游取的是真值；乐观修正只发生在 get_cached_balances 的展示路径。
            optimistic: false,
        })
    }

    /// 批量读取【已缓存】的凭据余额快照（A10）
    ///
    /// 为降低账号被上游限流的风险：只读 balance_cache，绝不触发任何上游 getUsageLimits 调用。
    ///
    /// 修复：返回最近 7 天内的最后已知值（不再用 5 分钟新鲜度阈值过滤）。
    /// 后台温和刷新间隔为 30 分钟，若这里仍按 5 分钟丢弃，前端每 30 分钟只有 5 分钟
    /// 能看到数字。改为按【展示保留上限】过滤，并把 `cached_at` 交给前端标注新鲜度
    /// （“截至 X 分钟前”），让余额/订阅等级“慢慢自动更新”且重启不丢。
    /// 仅陈旧超过 7 天的条目才不返回（前端可按需单独 hover 拉取）。
    pub fn get_cached_balances(&self) -> CachedBalancesResponse {
        let now = Utc::now().timestamp() as f64;
        // 缓存按**账号**键存，而前端按**凭据 id** 展示 ⇒ 一条展开成共享它的全部 id。
        // 同 key 的分身因此读到**同一条**缓存，余额必然一致（这是同步生效的落点）。
        let key_to_ids = self.balance_key_to_ids();
        let cache = self.balance_cache.lock();
        let mut balances: HashMap<u64, CachedBalanceItem> = HashMap::new();
        for (key, c) in cache.iter() {
            if (now - c.cached_at) >= BALANCE_CACHE_DISPLAY_MAX_AGE_SECS as f64 {
                continue;
            }
            let item = CachedBalanceItem {
                balance: c.data.clone(),
                cached_at: c.cached_at,
            };
            match key_to_ids.get(key) {
                Some(ids) => {
                    for id in ids {
                        balances.insert(*id, item.clone());
                    }
                }
                // 键在缓存里但池中已无对应凭据（号被删）。若键本身是十进制 id（旧格式
                // 或 OAuth 号），仍按它展示，避免刚删号那一刻面板闪空。
                None => {
                    if let Ok(id) = key.parse::<u64>() {
                        balances.insert(id, item);
                    }
                }
            }
        }
        drop(cache);

        // ⭐ dwgx 需求「用了余额之后要刷新额度显示」：用**本地累计的 credit 花费**做乐观修正。
        //
        // 问题：余额真值由后台每 30 分钟温和刷新一次（`refresh_all_balances_gently`），
        // 所以刚跑完一批请求，面板上的额度**最多 30 分钟内都不动** —— 用户以为没生效。
        //
        // 为什么不每次请求都打上游：那是 `web_portal`（app.kiro.dev）探测，会**加重风控**。
        // 线上号池正被风控烧号（单号存活 25~60 分钟），多打探测只会更糟。
        //
        // 做法：`total_credits_used` 是每次请求完成后由 `meteringEvent` 真实计费量累加的
        // （`token_manager::add_credits`）。缓存里存了取值当时的 `credits_used_at_cache` 基线，
        // 两者之差 = **缓存之后新花掉的量**，据此乐观推进 current_usage / remaining / 百分比。
        // 后台刷新到来时用真值覆盖，所以误差不累积、只在两次真值之间起插值作用。
        // 复用**已有**的两套数据，不新造并行链路：
        // - `credits_used_snapshot()`：各号当前的 `total_credits_used`（由 meteringEvent 累加）
        // - `balance_baselines()`：`set_balance_snapshots` 回推时记下的 `credits_used_at_cache`
        //   （余额加权分流已经在用这个基线，见 token_manager 的 balance_factor）
        let used_now = self.token_manager.credits_used_snapshot();
        let baselines = self.token_manager.balance_baselines();
        let mut balances = balances;
        for (id, item) in balances.iter_mut() {
            let (Some(&now_used), Some(&base)) = (used_now.get(id), baselines.get(id)) else {
                continue;
            };
            // 只做**单向**推进：delta<=0 说明基线比当前还大（重启后计数从 0 起等），此时不动。
            let delta = now_used - base;
            if !(delta > 0.0) {
                continue;
            }
            let b = &mut item.balance;
            b.current_usage += delta;
            // remaining 不得为负：额度用超时上游会自己表达（overage/402），这里只保证展示不出负数。
            b.remaining = (b.remaining - delta).max(0.0);
            if b.effective_limit > 0.0 {
                b.usage_percentage = (b.current_usage / b.effective_limit * 100.0).min(100.0);
            }
            // 标记为"含本地推算"：与上游真值区分，前端可据此加"约"字样或提示。
            b.optimistic = true;
        }

        CachedBalancesResponse {
            total: balances.len(),
            balances,
        }
    }

    /// 温和地周期性刷新所有【未禁用】凭据的余额缓存（A6）
    ///
    /// 为降低账号被上游限流的风险：
    /// - 逐个刷新，每个之间 sleep `spacing_secs` 秒，绝不并发一次性打所有号。
    /// - 只刷未禁用的号。
    /// - 仅更新缓存供展示，绝不因 remaining 低就自动禁用凭据（不做主动禁用）。
    ///
    /// 由 main.rs 的后台任务按长间隔调用（默认 30 分钟）。
    pub async fn refresh_all_balances_gently(&self, spacing_secs: u64) {
        // 取未禁用凭据 id 快照（只读，不持锁跨 await）
        //
        // 🔴 必须排除 custom_api 代挂号（2026-08-10 修）：它们是用户自购的 Anthropic 兼容
        // 中转站，**没有 Kiro 账号**，`get_usage_limits` / `web_portal` 对它们必然失败
        // （`ensure_valid_token` 对代挂号返空 token 后仍会打上游，失败只被 warn 忽略）。
        // ⇒ 改前每轮后台刷新都对每个代挂号白打一次注定失败的上游请求。
        // 这与下面那条「绝不为展示类需求反复打 web_portal（加重风控）」的既定原则同向。
        let all_ids: Vec<u64> = self
            .token_manager
            .snapshot()
            .entries
            .into_iter()
            // 判据与 `KiroCredentials::is_custom_api_credential()` **逐条对齐**
            // （`auth_method == "custom_api"` 或 `base_url` 非空）—— 只判前者会漏掉
            // 「auth_method 未写全但配了 base_url」的历史号，那些同样没有 Kiro 账号。
            .filter(|e| {
                !e.disabled
                    && e.auth_method.as_deref() != Some("custom_api")
                    && e.base_url.is_none()
            })
            .map(|e| e.id)
            .collect();

        // ⭐ 按**账号**去重：同一个 `ksk_` key 的 N 份分身共享一个上游账号与一份配额，
        // 逐份打就是 N 次 `web_portal` 往返拿同一个数字。而 `web_portal` 是上游探测，
        // 调多了会加重风控（本仓调优结论：绝不为展示类需求反复打它）。
        //
        // 缓存现在按账号键（`balance_cache_key`），所以同组只需刷一份 —— 结果自动
        // 覆盖全组。实测线上一组 4 份分身，这一步把 4 次探测降到 1 次。
        //
        // 取组内**第一个**（id 升序，即主份优先）：与前端「查余额只打主份」同口径。
        let ids: Vec<u64> = {
            let mut seen: HashSet<String> = HashSet::new();
            all_ids
                .into_iter()
                .filter(|id| seen.insert(self.balance_cache_key(*id)))
                .collect()
        };

        if ids.is_empty() {
            return;
        }

        tracing::info!("后台温和余额刷新开始：{} 个未禁用凭据", ids.len());
        let spacing = std::time::Duration::from_secs(spacing_secs.max(1));

        for (idx, id) in ids.iter().enumerate() {
            // 分散节奏：从第二个开始，每个之间先 sleep，避免一瞬间并发打多个号
            if idx > 0 {
                tokio::time::sleep(spacing).await;
            }

            match self.fetch_balance(*id).await {
                Ok(balance) => {
                    // usage_limit 先读（commit_fresh_balance 会移动 balance——M4 的门条件
                    // 必须在移动前取值，2026-08-13 编译期修正）。
                    let balance_usage_limit = balance.usage_limit;
                    let exhausted = balance.remaining <= 0.0;
                    let key = self.balance_cache_key(*id);
                    // 落缓存 + 重置该账号基线，走与「查看余额」**同一个**收口
                    // （两条路径各写一份 insert 正是基线漏更新的根源）。
                    // 逐个提交而不是攒到本轮末尾：一轮要走 N×4 秒，早提交的号能早点
                    // 在面板/调度器上生效，且中途进程重启不会白刷。
                    self.commit_fresh_balance(key.clone(), balance);
                    tracing::debug!("后台温和余额刷新：凭据 #{} 已更新缓存", id);
                    // ⭐ 超额自动禁用（2026-08-14 新增）：刚取到的上游真值必然新鲜
                    // （cached_at=now），无需 24h 新鲜度门；语义与手动端点
                    // disable_quota_exceeded 完全一致（report_quota_exhausted 收口）。
                    // 开关默认开，可在面板服务端配置里关闭。
                    // ⚠️ 2026-08-13 对抗审查 M4：空 breakdown 时 usage_limit()=0 → remaining=0，
                    // 会误杀「新号无 usage 记录 / 上游返回空 breakdown」的号（不可逆需人工
                    // 解禁）。必须加 limit>0 门：真额度用尽的号 limit 是正数（remaining=0
                    // 是已用尽），空 breakdown 的号 limit=0（拿不到额度信息）→ 跳过自动禁用。
                    if exhausted
                        && balance_usage_limit > 0.0
                        && self
                            .auto_disable_quota_exceeded
                            .load(std::sync::atomic::Ordering::Relaxed)
                    {
                        self.auto_disable_exhausted_group(&key);
                    }
                }
                Err(e) => {
                    // 单个失败不影响整体节奏；仅更新缓存展示，不做任何禁用动作
                    tracing::warn!("后台温和余额刷新：凭据 #{} 刷新失败（忽略）: {}", id, e);
                }
            }
        }

        // 收尾回推：把**没能刷成功**但缓存里有值的号也补进表（否则调度侧缺表=中性因子 1.0，
        // 余额加权对它们完全失效）。`fresh_keys` 传空 ⇒ 它们保留原基线，不会被误当成
        // "刚取到真值"（见 `push_balance_snapshots_to_scheduler` 的 fresh_keys 文档）。
        // 刷成功的那些已在循环里逐个提交过，这里对它们是幂等的。
        self.push_balance_snapshots_to_scheduler(&HashSet::new());

        tracing::info!("后台温和余额刷新完成");
    }

    /// 把一次**新取到的上游真值**落进缓存，并**同步重置该账号的花费基线**。
    ///
    /// # 为什么必须是一个函数（G-2 修的就是这里）
    ///
    /// 面板列表（`get_cached_balances`）在两次真值之间做**乐观修正**：
    /// `delta = 当前 total_credits_used - credits_used_at_cache`，把 delta 从 remaining 里扣掉。
    /// 这要求「缓存里的真值」与「基线」**成对更新**。
    ///
    /// 而此前只有后台温和刷新那条路径会更新基线（`refresh_all_balances_gently` 末尾那次
    /// 回推），`get_balance`（面板「查看余额」）**只写缓存不动基线** ⇒ 新真值配着旧基线：
    ///
    /// - t0 后台刷新：remaining=100，基线=50 花费
    /// - 期间花掉 20（total=70）→ 面板显示 100-20=80 ✅
    /// - 用户点「查看余额」：上游真值 80（已含那 20），写进缓存，基线仍是 50
    /// - 面板下一次轮询：80-(70-50)=**60** ❌ 那 20 被扣了两次
    ///
    /// 于是「查看余额」拿到 80、而列表显示 60，同一个号两个数字，且**越刷越低**，
    /// 直到 30 分钟后的后台刷新才对上 —— 这正是"额度刷新不对/很慢"的一条实因。
    ///
    /// 收口成一个函数是刻意的：两条路径各写一份 `cache.insert` 正是漏改的根源
    /// （与 `update.rs` 抽 `read_body_capped` 同一理由）。
    pub(super) fn commit_fresh_balance(&self, cache_key: String, balance: BalanceResponse) {
        {
            let mut cache = self.balance_cache.lock();
            cache.insert(
                cache_key.clone(),
                CachedBalance {
                    cached_at: Utc::now().timestamp() as f64,
                    data: balance,
                },
            );
        }
        self.save_balance_cache();
        // 只把**这一个账号**标记为"刚取到真值"。其余账号保留原基线 —— 见
        // `push_balance_snapshots_to_scheduler` 的 `fresh_keys` 文档。
        let mut fresh = HashSet::new();
        fresh.insert(cache_key);
        self.push_balance_snapshots_to_scheduler(&fresh);
    }

    // ============ 余额缓存持久化 ============

    pub(super) fn load_balance_cache_from(
        cache_path: &Option<PathBuf>,
        token_manager: &Arc<MultiTokenManager>,
    ) -> HashMap<String, CachedBalance> {
        let path = match cache_path {
            Some(p) => p,
            None => return HashMap::new(),
        };

        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return HashMap::new(),
        };

        // 文件中使用字符串 key 以兼容 JSON 格式
        let map: HashMap<String, CachedBalance> = match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("解析余额缓存失败，将忽略: {}", e);
                return HashMap::new();
            }
        };

        let now = Utc::now().timestamp() as f64;

        // ⭐ 旧格式迁移：**按凭据 id 键 → 按账号键**。
        //
        // # 为什么必须迁移（而不是"接受失效"）
        //
        // 缓存键从 `id` 改成 `sha256(apiKey)` 之后，旧文件里的十进制 id 键**永远不会被
        // 命中** ⇒ 升级后 api_key 号的余额全部显示为空 ⇒ 面板集体转圈打
        // `getUsageLimits`。那是 `web_portal` 上游探测，本仓调优结论是绝不为展示类需求
        // 反复打它（线上号池正被风控烧号）。
        //
        // 实测规模：线上 5 条缓存 / 5 个 api_key 号 / **只有 1 个不同的 key** ⇒
        // 迁移后并成 1 条。量级小，但方向是"少打一次上游探测"，且迁移只需十几行。
        //
        // # 并组时取最新的那条
        //
        // N 个 id 映射到同一个账号键时，按 `cached_at` 取最新 —— 它们描述的是同一个账号
        // 同一份配额，旧的那些本来就是冗余副本（这正是本次改动要消除的东西）。
        //
        // # 无法映射的键原样保留
        //
        // OAuth 号的键本来就是 id（`balance_cache_key` 对非 api_key 号回落 id），
        // 以及"号已被删但缓存还在"的残留 —— 两者都原样留着，由展示层的 7 天上限自然淘汰。
        let mut migrated: HashMap<String, CachedBalance> = HashMap::new();
        for (key, v) in map {
            if (now - v.cached_at) >= BALANCE_CACHE_DISPLAY_MAX_AGE_SECS as f64 {
                // 修复：启动恢复用【展示保留上限】(7 天)，而非 5 分钟新鲜度阈值。
                // 这样重启后仍能立刻显示上次的余额数字（前端据 cached_at 标注新鲜度），
                // 而不是因为磁盘缓存 >5 分钟就整批丢成“未知”。只有陈旧到 7 天才丢弃。
                continue;
            }
            // 旧格式判定：键能 parse 成 u64 且该 id 是 api_key 号 ⇒ 需要迁移成账号键。
            // （新格式的账号键是 64 位 hex，parse::<u64> 必然失败，所以不会被误迁。）
            let target = match key.parse::<u64>() {
                Ok(id) => match token_manager.export_credential(id) {
                    Some(c) if c.is_api_key_credential() => match c.kiro_api_key.as_deref() {
                        Some(k) => crate::kiro::token_manager::sha256_hex(k),
                        None => key.clone(),
                    },
                    // 非 api_key 号（OAuth）或号已不在池里 ⇒ 键保持 id，与
                    // `balance_cache_key` 的回落一致。
                    _ => key.clone(),
                },
                Err(_) => key.clone(),
            };
            match migrated.get(&target) {
                // 已有更新的条目 ⇒ 丢弃这条旧副本
                Some(existing) if existing.cached_at >= v.cached_at => {}
                _ => {
                    migrated.insert(target, v);
                }
            }
        }
        migrated
    }

    fn save_balance_cache(&self) {
        let path = match &self.cache_path {
            Some(p) => p,
            None => return,
        };

        // 持有锁期间完成序列化和写入，防止并发损坏
        let cache = self.balance_cache.lock();
        let map: HashMap<String, &CachedBalance> =
            cache.iter().map(|(k, v)| (k.to_string(), v)).collect();

        match serde_json::to_string_pretty(&map) {
            Ok(json) => {
                if let Err(e) = std::fs::write(path, json) {
                    tracing::warn!("保存余额缓存失败: {}", e);
                }
            }
            Err(e) => tracing::warn!("序列化余额缓存失败: {}", e),
        }
    }
}
