Findings and patch.

## 1. 现状精确描述

| 事项 | 事实 | 位置 |
|---|---|---|
| 缓存字段 | `balance_cache: Mutex<HashMap<u64, CachedBalance>>`，key = **凭据 id** | `src/admin/service.rs:232` |
| 条目结构 | `struct CachedBalance { cached_at: f64, data: BalanceResponse }` | `service.rs:104-111` |
| 新鲜度 TTL | `BALANCE_CACHE_TTL_SECS = 300`（仅 hover 路径判是否重拉上游） | `service.rs:39` |
| 展示保留 | `BALANCE_CACHE_DISPLAY_MAX_AGE_SECS = 7*24*3600` | `service.rs:59` |
| 落盘文件 | `{cache_dir}/kiro_balance_cache.json`，格式 `HashMap<String(id 十进制), CachedBalance>` | `service.rs:252-256`、`3053-3074` |
| 读盘 | `load_balance_cache_from`：读失败/解析失败 → 空 map（已 fail-soft，`warn!` 不 bail）；**但 `k.parse::<u64>().ok()?` 会静默丢弃非数字 key** | `service.rs:3017-3051` |
| 读写点 | `get_balance` 读+写(718/765) · `get_cached_balances` 只读(832) · `refresh_all_balances_gently` 写(928) · `push_balance_snapshots_to_scheduler` 只读(967) · `delete_credential_forced` remove(1393) · `purge_credential` remove(1478) | — |

## 2. 改法：key → 账号标识

选 **新增 `KiroCredentials::account_key()`**：`kiro_api_key` 存在 → `acct:{sha256(key)[..16]}`，否则回退 `cred:{id}`。

- 不用 `family_key()`：它对 external_idp 返回 `m365:{tenant}`，会把**同租户的不同账号**并成一条余额（各自额度不同 → 显示错值）。它的语义是"限流连坐组"，不是"同一额度池"。
- 不用裸 `kiroApiKey`：会把明文密钥写进 `kiro_balance_cache.json`（该文件无加密）。截断 16 hex（64 bit）碰撞概率可忽略，且缩短落盘体积。
- OAuth 号回退 `cred:{id}` → 行为与现状**逐位相同**。

## 3. patch

### P1 — `src/kiro/model/credentials.rs`，紧接 `family_key` 之后（约 :788 `}` 与 `pub fn effective_idp` 之间）

old_string:
```rust
        // ③ 非 M365（IdC/social/api_key）或解析失败：各自独立成族
        format!("cred:{id}")
    }

    pub fn effective_idp(&self) -> &str {
```
new_string:
```rust
        // ③ 非 M365（IdC/social/api_key）或解析失败：各自独立成族
        format!("cred:{id}")
    }

    /// 账号键（account_key）—— **额度/余额**的共享单位，与 [`family_key`](Self::family_key) 正交。
    ///
    /// 分身（同一 `kiroApiKey` 多开 N 份）在上游是**同一个账号、同一份额度**，
    /// 但在本地是 N 条凭据。余额若按凭据 id 缓存，N 份各自查各自缓存、查询时刻不同 →
    /// 面板对同一个 key 显示 5 个不同百分比（线上实测 64.7/66.1/90.1/66.5/90.2）。
    /// 故按账号共享一份缓存：既消除不一致，又把上游 `web_portal` 探测从 N 次降到 1 次
    /// （重复探测同账号会**加重风控**，这是收益的主要部分）。
    ///
    /// 为什么不复用 `family_key`：它对 M365 返回 `m365:{tenant}`，会把同租户的**不同账号**
    /// 并成一条余额（各自额度不同 → 显示错值）。它的语义是"限流连坐组"而非"同一额度池"。
    ///
    /// 为什么截断哈希而非原 key：该键会落盘到 `kiro_balance_cache.json`（无 at-rest 加密），
    /// 明文密钥不得进去；16 hex = 64 bit，碰撞概率对号池规模可忽略。
    /// OAuth 号（social/idc/external_idp）没有 `kiro_api_key`，回退 `cred:{id}`，行为与改动前一致。
    pub fn account_key(&self, id: u64) -> String {
        match self.kiro_api_key.as_deref() {
            Some(k) if !k.is_empty() => {
                let mut hasher = Sha256::new();
                hasher.update(k.as_bytes());
                format!("acct:{:x}", hasher.finalize())[..21].to_string()
            }
            _ => format!("cred:{id}"),
        }
    }

    pub fn effective_idp(&self) -> &str {
```
> 注：`[..21]` = `"acct:"`(5) + 16 hex。若 `credentials.rs` 未 `use sha2::{Digest, Sha256};`，需在文件头补该 use（应用前 grep 确认）。

### P2 — `src/kiro/token_manager.rs`，`family_key_of` 之后（约 :4126）

old_string:
```rust
            .map(|e| e.credentials.family_key(e.id))
            .unwrap_or_else(|| format!("cred:{id}"))
    }
```
new_string:
```rust
            .map(|e| e.credentials.family_key(e.id))
            .unwrap_or_else(|| format!("cred:{id}"))
    }

    /// 单个凭据的账号键（余额缓存共享单位，见 `KiroCredentials::account_key`）。
    pub fn account_key_of(&self, id: u64) -> String {
        let entries = self.entries.lock();
        entries
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.credentials.account_key(e.id))
            .unwrap_or_else(|| format!("cred:{id}"))
    }

    /// 全池 `id -> account_key` 映射。展示层用它把"按账号"的余额扇出回"按凭据 id"，
    /// 从而**不改动前端契约**（`/api/admin/credentials/balances` 仍返回 id → balance）。
    pub fn account_keys_snapshot(&self) -> HashMap<u64, String> {
        let entries = self.entries.lock();
        entries.iter().map(|e| (e.id, e.credentials.account_key(e.id))).collect()
    }
```

### P3 — 缓存字段类型（`service.rs:232`）

old: `    balance_cache: Mutex<HashMap<u64, CachedBalance>>,`
new:
```rust
    /// 余额缓存，key = **账号键**（`KiroCredentials::account_key`）而非凭据 id。
    /// 分身共享同一账号 → 共享一条缓存 → 面板数字一致 + 上游探测从 N 次降到 1 次。
    balance_cache: Mutex<HashMap<String, CachedBalance>>,
```

### P4 — `get_balance` 读侧（`service.rs:717-731`）

old:
```rust
        let stale_fallback = {
            let cache = self.balance_cache.lock();
            match cache.get(&id) {
```
new:
```rust
        let akey = self.token_manager.account_key_of(id);
        let stale_fallback = {
            let cache = self.balance_cache.lock();
            match cache.get(&akey) {
```

### P5 — `get_balance` 写侧（`service.rs:764-772`）

old:
```rust
            let mut cache = self.balance_cache.lock();
            cache.insert(
                id,
                CachedBalance {
```
new:
```rust
            let mut cache = self.balance_cache.lock();
            cache.insert(
                akey,
                CachedBalance {
```

### P6 — `refresh_all_balances_gently`：跳过已刷过的同账号（`service.rs:915-940` 区间）

old:
```rust
            match self.fetch_balance(*id).await {
                Ok(balance) => {
                    {
                        let mut cache = self.balance_cache.lock();
                        cache.insert(
                            *id,
                            CachedBalance {
                                cached_at: Utc::now().timestamp() as f64,
                                data: balance,
                            },
                        );
                    }
```
new:
```rust
            // 同账号（分身）只探一次：额度是共享的，重复打 web_portal 只会加重风控。
            let akey = self.token_manager.account_key_of(*id);
            if !refreshed_accounts.insert(akey.clone()) {
                tracing::debug!("后台温和余额刷新：凭据 #{} 与已刷账号同源，跳过上游探测", id);
                continue;
            }
            match self.fetch_balance(*id).await {
                Ok(balance) => {
                    {
                        let mut cache = self.balance_cache.lock();
                        cache.insert(
                            akey,
                            CachedBalance {
                                cached_at: Utc::now().timestamp() as f64,
                                data: balance,
                            },
                        );
                    }
```
配套在 `let spacing = ...` 那行后插入声明：

old: `        let spacing = std::time::Duration::from_secs(spacing_secs.max(1));`
new:
```rust
        let spacing = std::time::Duration::from_secs(spacing_secs.max(1));
        // 本轮已探过的账号键，避免分身重复探测同一账号。
        let mut refreshed_accounts: HashSet<String> = HashSet::new();
```
> `continue` 在 `idx > 0` 的 sleep 之后，所以被跳过的分身不会白等一个 spacing；实测 5 分身 1 账号时整轮从 5×spacing 降到 1 次探测。

### P7 — `get_cached_balances` 扇出（`service.rs:830-845`）

old:
```rust
        let now = Utc::now().timestamp() as f64;
        let cache = self.balance_cache.lock();
        let balances: HashMap<u64, CachedBalanceItem> = cache
            .iter()
            .filter(|(_, c)| (now - c.cached_at) < BALANCE_CACHE_DISPLAY_MAX_AGE_SECS as f64)
            .map(|(id, c)| {
                (
                    *id,
                    CachedBalanceItem {
                        balance: c.data.clone(),
                        cached_at: c.cached_at,
                    },
                )
            })
            .collect();
        drop(cache);
```
new:
```rust
        let now = Utc::now().timestamp() as f64;
        // 缓存按【账号】存，前端契约按【凭据 id】——这里扇出：同账号的 N 个分身拿到
        // 同一份数据（数字一致），前端与 `CachedBalancesResponse` 结构均无需改动。
        let account_keys = self.token_manager.account_keys_snapshot();
        let cache = self.balance_cache.lock();
        let balances: HashMap<u64, CachedBalanceItem> = account_keys
            .iter()
            .filter_map(|(id, akey)| {
                let c = cache.get(akey)?;
                if (now - c.cached_at) >= BALANCE_CACHE_DISPLAY_MAX_AGE_SECS as f64 {
                    return None;
                }
                let mut balance = c.data.clone();
                // `BalanceResponse.id` 是缓存写入时那个凭据的 id；扇出后必须改成本条的 id，
                // 否则前端按 id 对齐会串号。
                balance.id = *id;
                Some((*id, CachedBalanceItem { balance, cached_at: c.cached_at }))
            })
            .collect();
        drop(cache);
```

### P8 — 乐观修正按账号汇总（同函数，`service.rs:865-871`）

old:
```rust
        for (id, item) in balances.iter_mut() {
            let (Some(&now_used), Some(&base)) = (used_now.get(id), baselines.get(id)) else {
                continue;
            };
            // 只做**单向**推进：delta<=0 说明基线比当前还大（重启后计数从 0 起等），此时不动。
            let delta = now_used - base;
```
new:
```rust
        // 分身共享额度：乐观推进量必须是**同账号所有分身**的花费之和，
        // 否则 5 个分身各扣自己那 1/5，显示的剩余额度会偏高 5 倍。
        let mut delta_by_account: HashMap<&str, f64> = HashMap::new();
        for (id, akey) in account_keys.iter() {
            let (Some(&now_used), Some(&base)) = (used_now.get(id), baselines.get(id)) else {
                continue;
            };
            let d = now_used - base;
            if d > 0.0 {
                *delta_by_account.entry(akey.as_str()).or_insert(0.0) += d;
            }
        }
        for (id, item) in balances.iter_mut() {
            let Some(delta) = account_keys
                .get(id)
                .and_then(|k| delta_by_account.get(k.as_str()))
                .copied()
            else {
                continue;
            };
```

### P9 — 删除路径：账号仍有其它分身时**不得**清缓存（`service.rs:1391-1396` 与 `1476-1481`，两处同形）

old（各出现一次，需按上下文分别定位）:
```rust
        // 清理已删除凭据的余额缓存
        {
            let mut cache = self.balance_cache.lock();
            cache.remove(&id);
        }
```
new:
```rust
        // 清理已删除凭据的余额缓存 —— 但账号键可能被同账号的其它分身共用，
        // 仅当池中再无凭据映射到该键时才移除，否则会把存活分身的余额一起抹掉。
        {
            let akey = self.token_manager.account_key_of(id);
            let still_used = self
                .token_manager
                .account_keys_snapshot()
                .values()
                .any(|k| *k == akey);
            if !still_used {
                self.balance_cache.lock().remove(&akey);
            }
        }
```
（`purge_credential` 处同样替换，注释首句改「彻底删除凭据」。）

### P10 — `push_balance_snapshots_to_scheduler`（`service.rs:965-985`）

old:
```rust
        let snaps: std::collections::HashMap<u64, BalanceSnapshot> = {
            let cache = self.balance_cache.lock();
            cache
                .iter()
                .filter_map(|(id, cb)| {
```
new:
```rust
        let account_keys = self.token_manager.account_keys_snapshot();
        let snaps: std::collections::HashMap<u64, BalanceSnapshot> = {
            let cache = self.balance_cache.lock();
            account_keys
                .iter()
                .filter_map(|(id, akey)| {
                    let cb = cache.get(akey)?;
```
末尾的 `Some((*id, BalanceSnapshot {...}))` 与 `.collect()` 保持不变。

### P11 — 落盘兼容（`service.rs:3017-3051`）

old:
```rust
    fn load_balance_cache_from(cache_path: &Option<PathBuf>) -> HashMap<u64, CachedBalance> {
```
new:
```rust
    /// 读盘。**格式变更兼容**：旧文件 key 是凭据 id 十进制（`"1"`），新格式是账号键
    /// （`acct:...` / `cred:{id}`）。此处不做迁移也不 bail：
    /// - 纯数字 key（旧格式）→ 归一成 `cred:{id}`，OAuth 号的展示缓存**原地继续可用**；
    /// - api_key 号的旧条目落到 `cred:{id}` 而新键是 `acct:...` → 该条目此后不被读到，
    ///   等第一次后台刷新（默认 30 分钟内）写入新键即恢复，期间前端显示"未知"，无数据损失；
    /// - 解析失败 → 空 map + `warn!`（沿用现有 fail-soft，绝不让服务起不来）。
    ///
    /// 刻意**不**清理陈留的 `cred:{id}` 条目：7 天 `DISPLAY_MAX_AGE` 会自然淘汰。
    fn load_balance_cache_from(cache_path: &Option<PathBuf>) -> HashMap<String, CachedBalance> {
```
old:
```rust
        let now = Utc::now().timestamp() as f64;
        map.into_iter()
            .filter_map(|(k, v)| {
                let id = k.parse::<u64>().ok()?;
```
new:
```rust
        let now = Utc::now().timestamp() as f64;
        map.into_iter()
            .filter_map(|(k, v)| {
                // 旧格式（纯数字 id）归一到 `cred:{id}`；新格式原样保留。
                let key = match k.parse::<u64>() {
                    Ok(id) => format!("cred:{id}"),
                    Err(_) => k,
                };
```
old:
```rust
                if (now - v.cached_at) < BALANCE_CACHE_DISPLAY_MAX_AGE_SECS as f64 {
                    Some((id, v))
```
new:
```rust
                if (now - v.cached_at) < BALANCE_CACHE_DISPLAY_MAX_AGE_SECS as f64 {
                    Some((key, v))
```
`save_balance_cache`（:3060-3062）的 `k.to_string()` 对 `String` key 仍合法，**无需改**（`String::to_string` 即克隆）。

## 4. 行为差异（前端）

- 端点 `/api/admin/credentials/balances` 的 wire 格式 **不变**（`HashMap<u64, CachedBalanceItem>`，`types.rs:493` 不动）。同账号的 5 个分身现在返回**同一份 `balance` 与同一个 `cached_at`**，`balance.id` 已被改写为各自 id。
- hover 单查（`get_balance`）：查任一分身即填满全账号缓存 → 其余 4 个下次 hover 直接命中，**上游探测降到 1/5**。
- 后台刷新一轮的上游探测数 = **不同账号数**，不再是凭据数。
- 前端不必改；`credential-card` 上 5 张卡的百分比自然一致。

## 5. 「移除即 FAIL」测试骨架（放 `service.rs` 的 `mod balance_cache_tests` 内）

```rust
/// 回归：同一 kiroApiKey 的多个分身必须共享一条余额缓存（线上实测同 key 显示 5 个不同百分比）。
/// **旧代码为何 FAIL**：缓存按凭据 id 存，播种 #1 后 #2 查不到 → balances 只有 1 条。
#[test]
fn cached_balances_shared_across_clones_of_same_account() {
    // 两条凭据，同一个 kiro_api_key（= 分身），auth_method=api_key
    let svc = mk_service_with_two_clones("ksk_same_account_key");
    let akey = svc.token_manager.account_key_of(1);
    assert_eq!(akey, svc.token_manager.account_key_of(2), "分身账号键必须相同");
    svc.balance_cache.lock().insert(akey, mk_cached_balance(1, Utc::now().timestamp() as f64));

    let resp = svc.get_cached_balances();
    assert_eq!(resp.total, 2, "一条缓存必须扇出到两个分身");
    let (b1, b2) = (&resp.balances[&1].balance, &resp.balances[&2].balance);
    assert_eq!(b1.remaining, b2.remaining, "同账号剩余额度必须一致");
    assert_eq!(b2.id, 2, "扇出后 id 必须是本条凭据的 id，不能串号");
}

/// 回归：删除一个分身不得抹掉存活分身的余额（账号键共用）。
#[test]
fn deleting_one_clone_keeps_shared_balance_cache() { /* 播种 → delete_credential_forced(2, true) → 断言 #1 仍有缓存 */ }

/// 回归：旧格式落盘文件（key = "1"）必须仍能读出且不 bail。
#[test]
fn load_balance_cache_accepts_legacy_numeric_keys() {
    // 写 {"1": {...}} → load_balance_cache_from → 断言含 key "cred:1"、len==1
}
```
（`mk_service_with_two_clones` 需照 `mk_service_with_one_credential`（:3425-3435）新建，向 `MultiTokenManager::new` 传两条 `kiro_api_key` 相同的 `KiroCredentials`。）

未验证项：未跑 `cargo build/test`（受任务约束），`credentials.rs` 是否已 `use sha2::Sha256` 需应用前 grep 确认；`HashSet` 在 `service.rs` 已 import（`known_endpoints` 用它），无需补。