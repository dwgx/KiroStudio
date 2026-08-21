//! Held-lock token refresh (`refresh_token_locked`).
//! Child of `token_manager` (`#[path]`) so it can reach private fields.
//! Does not own select / report / AIMD.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration as StdDuration;

use crate::kiro::model::credentials::KiroCredentials;

use super::{
    apply_refresh_result_fields, is_token_expiring_within, refresh_error_retryable,
    refresh_token, resolve_profile_arn_multi_region, DiagnosedError,
    RefreshNotSupportedError, RefreshOutcome, RefreshTokenInvalidError,
    REPROBE_ALL_BAD_COOLDOWN, REFRESH_LOCK_TIMEOUT_SECS,
};

impl super::MultiTokenManager {
    /// 刷新错误的可重试性判定已抽为模块级自由函数 [`refresh_error_retryable`]
    /// （定义在 RefreshHttpError 类型旁），供本方法与测试共用。

    /// 持锁刷新的共享实现。`conditional_lead` 为 `Some(min)` 时，拿锁后二次确认
    /// token 仍将在 `min` 分钟内过期才刷新，否则返回 [`RefreshOutcome::Skipped`]；
    /// 为 `None` 时无条件刷新（admin 强刷）。
    pub(super) async fn refresh_token_locked(
        &self,
        id: u64,
        conditional_lead: Option<i64>,
    ) -> anyhow::Result<RefreshOutcome> {
        // 快速存在性检查（无锁），同时取出该凭据的 per-credential 刷新锁。
        // Arc clone 出来后无需持有 entries 锁，await 期间不阻塞其他凭据的 entries 读写。
        let cred_refresh_lock = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| Arc::clone(&e.refresh_lock))
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        // 获取该凭据专属的刷新锁——仅串行化同一凭据的并发刷新，不影响其他凭据。
        // ⚠️ 加**等待超时**（2026-08-14）：该号上一次刷新是跨 .await 的网络往返
        // （含退避最坏可到 180s+），期间并发请求会在锁后无限排队（实测最坏 15s+）。
        // 超时后按瞬态错误返回，让请求路径换号重试（`report_refresh_failure_classified`
        // 对不含 4xx/invalid_grant 的普通错误一律按瞬态处置：只冷却、不计数、不禁用），
        // 而不是死等一个可能已经卡住的刷新。等待上限远小于一次真实刷新的耗时，
        // 正常刷新几乎不可能超时，只在「刷新卡死/超长」时兜底。二次确认（下方
        // conditional_lead 拿锁后重查 token 是否仍将过期）不受影响，防惊群语义保留。
        let _guard = tokio::time::timeout(
            StdDuration::from_secs(REFRESH_LOCK_TIMEOUT_SECS),
            cred_refresh_lock.lock(),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "凭据 #{} 刷新锁等待超时（{}s）：该号上一次刷新耗时异常或卡死，\
                 本次按瞬态错误处置，请求将换号/重试",
                id,
                REFRESH_LOCK_TIMEOUT_SECS
            )
        })?;

        // 拿锁后读取当前凭据：请求路径或其它预刷新可能在等锁期间已刷新
        let credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        // 陈旧刷新守卫：快照发起刷新时的 refresh_token。刷新是跨 .await 的网络调用,
        // 期间请求路径的 try_ensure_token 等可能已用同一 refresh_token 换到新 token 并写回。
        // 若写回时发现 entry 的 refresh_token 已不等于本次快照,说明别的路径抢先刷新成功,
        // 本次结果已陈旧 → 丢弃写回(否则会把已轮换的新 token 覆盖回旧的,导致下次刷新用废弃
        // 的 refresh_token 而失败)。参考 kiro-account-manager tasks/token_refresh.rs 的守卫。
        let refresh_token_snapshot = credentials.refresh_token.clone();

        // 条件刷新：拿锁后二次确认 token 仍将在 lead 内过期才刷,否则跳过(避免惊群重刷)。
        // unwrap_or(**true**):expires_at 缺失/不可解析时视为"需刷新"不跳过,与热路径进入判定
        // is_token_expired(unwrap_or=true) 同口径——否则 A3/C2 修复会把 expiry 未知的凭据误跳过
        // 导致该刷不刷、返回陈旧 token。(后台预刷新在 :4900 已用 unwrap_or(false) 预筛,expiry
        // 未知的凭据根本不会进预刷新,故此处改 true 不会引发后台重刷。)
        if let Some(lead) = conditional_lead {
            if !is_token_expiring_within(&credentials, lead).unwrap_or(true) {
                return Ok(RefreshOutcome::Skipped);
            }
        }

        let effective_proxy = credentials.effective_proxy(self.proxy.as_ref());
        let cfg = self.config.load_full();

        // Change 2: 瞬态错误重试退避（3 次，1s/2s/4s）。
        // 仅对 5xx 状态码和网络/连接错误重试；400/401/403/429/invalid_grant/DiagnosedError
        // 属永久性或策略性失败，直接透传给上层（保留 RefreshTokenInvalidError 语义不变）。
        let new_creds = {
            const MAX_ATTEMPTS: u32 = 3;
            let mut last_err: anyhow::Error = anyhow::anyhow!("unreachable");
            let mut succeeded = false;
            let mut result_creds = None;
            for attempt in 0..MAX_ATTEMPTS {
                match refresh_token(&credentials, &cfg, effective_proxy.as_ref()).await {
                    Ok(c) => {
                        result_creds = Some(c);
                        succeeded = true;
                        break;
                    }
                    Err(e) => {
                        // 永久/策略失败：不重试，立即透传。
                        // RefreshNotSupportedError（api_key 契约 bail）也在此列：
                        // 结构上永不可能成功，退避重试满 3 次（1s+2s 白等）纯损耗，
                        // 且每轮都计一次失败 → 加速把号判成死号。根因已在 provider.rs
                        // 的两处 force-refresh 调用点堵住（api_key 号不再进入刷新）；
                        // 这里是**纵深防御**：任何将来新增的调用方即使漏判，
                        // 也只会失败一次而不是被退避重试三次。
                        if e.downcast_ref::<RefreshTokenInvalidError>().is_some()
                            || e.downcast_ref::<DiagnosedError>().is_some()
                            || e.downcast_ref::<RefreshNotSupportedError>().is_some()
                        {
                            return Err(e);
                        }
                        // ⭐ 结构化可重试性判定（2026-08-15 替换裸子串匹配）：带状态码的
                        // 错误按码分（5xx 瞬态可重试；403/429 等策略性失败不重试）；
                        // 无状态码的错误（reqwest 连接/超时/JSON 解析）视为网络层瞬态
                        // 可重试。此前 `contains("500")` 会把错误串里的 URL 端口
                        // （:5000）/错误体数字/毫秒时间误判成 5xx，把策略性失败当瞬态
                        // 重试；黑名单式 is_network 又把不含状态码的真 5xx 判成网络错误。
                        if refresh_error_retryable(&e) && attempt + 1 < MAX_ATTEMPTS {
                            let backoff_secs = 1u64 << attempt; // 1, 2
                            tracing::warn!(
                                "凭据 #{} 刷新瞬态错误（第 {}/{}），{}s 后重试: {}",
                                id,
                                attempt + 1,
                                MAX_ATTEMPTS,
                                backoff_secs,
                                e
                            );
                            tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
                            last_err = e;
                        } else {
                            last_err = e;
                            break;
                        }
                    }
                }
            }
            if succeeded {
                result_creds.unwrap()
            } else {
                return Err(last_err);
            }
        };

        // 更新 entries 中对应凭据（写回前校验 refresh_token 未被其它路径抢先轮换）
        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                if entry.credentials.refresh_token != refresh_token_snapshot {
                    // 别的路径已刷新成功,本次结果陈旧,不覆盖(避免用旧 refresh_token 覆盖新的)
                    tracing::debug!(
                        "凭据 #{} 刷新结果已陈旧(refresh_token 期间被其它路径轮换),丢弃本次写回",
                        id
                    );
                    return Ok(RefreshOutcome::Skipped);
                }
                // 逐字段合并（白名单，见 apply_refresh_result_fields 文档）：只搬运刷新链路
                // 真正拥有的 4 个 token 字段，其余字段（如 subscription_title）原地保留，
                // 不被本次刷新发起前的陈旧快照回退。
                apply_refresh_result_fields(&mut entry.credentials, &new_creds);
                // 白名单含 profile_arn（族键兜底输入），刷新可能换 arn ⇒ 失效族键缓存。
                entry.family_key = None;
                entry.refresh_failure_count = 0;
            }
        }

        // 队头阻塞根治（Medium 1）：token 已刷新并写回，refresh_lock 的职责（串行化 token 轮换，
        // 防并发用同一 refresh_token 重复换取）到此结束。下面的 profileArn 动态解析 / 验活重选
        // 是**独立的纯网络探测**（只改 profile_arn，与 refresh_token 轮换正交），若继续持锁，一个
        // 全坏 external_idp 号 reprobe 一整轮 getUsageLimits 会把所有号的刷新全堵在锁后（队头阻塞）。
        // 故在此显式释放 refresh_lock，让 arn/reprobe 在锁外并发进行。写回 profile_arn 时另用
        // entries 短临界区 + 值比对，无需 refresh_lock 保护。
        drop(_guard);

        // 动态解析 profileArn:idc/Enterprise 号常无 profileArn(oidc 刷新不回传),而对话/余额
        // 端点要求真实 profileArn(占位 ARN 对 Enterprise 号会被判 Invalid token/403)。刷新成功后
        // 若该号仍缺 profileArn 且非 external_idp(它不带),运行时调 management ListAvailableProfiles
        // 拿真实 arn 写回,一次解析后持久化缓存,后续对话/余额直接用真实值。失败仅告警不阻断。
        let (needs_arn, needs_reprobe, arn_creds, arn_token) = {
            let entries = self.entries.lock();
            match entries.iter().find(|e| e.id == id) {
                Some(e) => {
                    let c = &e.credentials;
                    let missing = c
                        .profile_arn
                        .as_deref()
                        .map(|s| s.trim().is_empty())
                        .unwrap_or(true);
                    // external_idp 也纳入动态解析:上游迁 kiro.dev 后 external_idp 号
                    // 必须带自己租户的真实 profileArn（缺了 400 profileArn is required），
                    // 而 resolve_profile_arn_via_management 本就为它设了 TokenType:EXTERNAL_IDP。
                    // 仅 api_key 号无 profile 概念,排除。
                    let eligible = missing && !c.is_api_key_credential();
                    // 验活重选（D）：external_idp 号当前 arn 上次 getUsageLimits 返回过
                    // 403 FEATURE_NOT_SUPPORTED（该 region profile 未开通）→ 需要 reprobe 换可用 region。
                    // 只对**确认坏的号**触发（健康号 flag=false 不动，省成本）。missing 优先走解析路径。
                    //
                    // 成本护栏（Medium 2）：全 region 都坏的号，上次全坏探测若在 REPROBE_ALL_BAD_COOLDOWN
                    // 冷却期内则跳过——否则余额环每 ~30min 重置 403 flag，每 token TTL 都白跑一整轮探测。
                    let in_reprobe_cooldown = e
                        .last_full_reprobe_at
                        .lock()
                        .map(|t| t.elapsed() < REPROBE_ALL_BAD_COOLDOWN)
                        .unwrap_or(false);
                    let needs_reprobe = !missing
                        && c.is_external_idp_credential()
                        && e.last_usage_403_feature_not_supported
                            .load(Ordering::Relaxed)
                        && !in_reprobe_cooldown;
                    (
                        eligible,
                        needs_reprobe,
                        c.clone(),
                        c.access_token.clone().unwrap_or_default(),
                    )
                }
                None => (false, false, KiroCredentials::default(), String::new()),
            }
        };
        // 验活重选（D）：确认坏的 external_idp 号——枚举全部候选、真验活、选 usable 的 arn 写回。
        // 用 probe_all_usable_profiles（而非 resolve_profile_arn_multi_region 的「取第一个」），
        // 否则可能再次选中同一个 FEATURE_NOT_SUPPORTED 的坏 arn。
        if needs_reprobe && !arn_token.is_empty() {
            // 抽成 helper 供刷新路径 + 对话路径异步任务共用(逻辑单一真相源)。
            self.reprobe_and_correct_region_with(id, &arn_creds, &arn_token)
                .await;
        }
        if needs_arn && !arn_token.is_empty() {
            let cfg2 = self.config.load_full();
            let proxy2 = arn_creds.effective_proxy(self.proxy.as_ref());
            // 优先探测号自己的 region（拿到 region 与 ARN 自洽的 profile），无则候选兜底。
            let preferred_region = arn_creds.effective_upstream_region(&cfg2).to_string();
            match resolve_profile_arn_multi_region(
                &arn_creds,
                &cfg2,
                &arn_token,
                proxy2.as_ref(),
                &preferred_region,
            )
            .await
            {
                Ok(Some(arn)) => {
                    let mut entries = self.entries.lock();
                    if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                        entry.credentials.profile_arn = Some(arn.clone());
                        // 族键在 issuer_url 解析失败时退化为 profileArn 兜底，变更后须失效缓存。
                        entry.family_key = None;
                        // 防呆铁律:profile_arn 一变,region/auth_region 立即同步成 ARN 内 region,
                        // 杜绝「解析到 X region 的 ARN 却留着 Y region」错配 → 400 Improperly formed。
                        if entry.credentials.sync_region_from_arn() {
                            tracing::info!(
                                "凭据 #{} region 已随 profileArn 同步为 {}",
                                id,
                                entry.credentials.region.as_deref().unwrap_or("?")
                            );
                        }
                    }
                    tracing::info!(
                        "凭据 #{} 动态解析到 profileArn（ListAvailableProfiles）",
                        id
                    );
                }
                Ok(None) => tracing::warn!("凭据 #{} ListAvailableProfiles 无可用 profile", id),
                Err(e) => tracing::warn!("凭据 #{} 动态解析 profileArn 失败（不阻断）: {}", id, e),
            }
        }

        // 持久化
        if let Err(e) = self.persist_credentials() {
            tracing::warn!("刷新 Token 后持久化失败: {}", e);
        }

        tracing::info!("凭据 #{} Token 已刷新", id);
        Ok(RefreshOutcome::Refreshed)
    }
}
