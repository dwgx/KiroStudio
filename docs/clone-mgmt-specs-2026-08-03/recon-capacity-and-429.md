## 1. 精确算式与调用点

```
effective_saturation_limit(per_cred)          token_manager.rs:3038-3048
  base = per_cred(>0) ?? global rpm_limit(>0) ?? 30
  → apply_rpm_headroom(base)                  token_manager.rs:3052-3063
      = (base × factor/100).saturating_sub(reserve).max(1)     # factor 0/≥100 视为不打折
线上：200 × 85/100 − 3 = 167 —— **每凭据**
```

| 调用点 | 位置 | 是否选号热路径 | 作用 |
|---|---|---|---|
| `cred_rpm_cap` | `token_manager.rs:2682` | **是（排序键分母）** | ⑦`rpm_usage_permille`(2719) / ⑥`slot_pressure_permille`(2753) 的分母，且喂 `p_avail` 的 rpm_pressure |
| `saturated` → `unusable` | `token_manager.rs:2710` | **是（沉底位 ③）** | 复用批量计数，不重算阈值 |
| `non_saturated` 过滤 | `token_manager.rs:2820-2822` | **是（硬门，第一趟）** | 真正的天花板 |
| `is_sticky_reuse_healthy` | `3109` ← 调用于 `2582` | **是（亲和解绑阈值）** | 绑定号饱和即临时解绑 |
| `transient_wait_outcome` 背压分支 | `2962` | 是（select 返 None 后） | 仅 `rpm_hard_gate_overload_wait=true` 时可达 |
| `is_rpm_saturated`（自带锁版） | `3009` | 否（锁外，测试/外部） | |
| insights 展示 | `admin/service.rs:470` | 否 | UI 口径 |

`RpmTracker` (`scheduling.rs:60-125`)：`hits: HashMap<u64, VecDeque<Instant>>`，`count(id)`(90) / `counts_for(&ids)`(112) 全部**按凭据 id**，无任何账号概念 → 5 份分身 = 5 个独立桶 = 5×167。
`health.rs`：`HealthSnapshot.ewma_429` 是 `pub`（`health.rs:183`，由 `snapshot()` 填充 478-479）；内部 `CredState.ewma_429`(107) 私有。所以账号级观测可以直接读 snapshot，无需改可见性。

## 2. 设计选择：① 按账号聚合 RPM 后再比阈值（读侧聚合，不改 record 路径）

**为什么不选 ②「每份阈值 ÷ 份数」**：它会把 `cred_rpm_cap` 改小，而这个值同时是排序键 ⑥⑦ 的**分母**和 `p_avail` 的 rpm_pressure 输入 → 一改就同时移动三个分流维度，等于掀翻现有分流；且 167/5=33 会让每份分身各自 33 就沉底，突发无法由任一份独吞，实测吞吐会低于 134 RPM。
**为什么不选 ③「新增账号级 RpmTracker」**：需要在 `select_next_credential` 的原子临界区里再 `record` 一次，多一把锁 + 双写一致性风险，而账号总量**完全可以从现有 per-cred 计数求和得到**，零新增写路径。
**①的形态**：账号阈值 = 成员中**最大**的 per-cred 阈值（账号容量是「一份」，绝不随份数相乘），账号已用 = 成员 RPM 之和。禁用号也计入 —— 它过去 60s 烧掉的上游配额一样真实。

**不掀翻现有分流的保证**（三条，缺一条就不该上）：
1. **排序键的分母与数值一字不改**。账号维度只作为**布尔门**加在既有 per-cred 门旁边，`cred_rpm_cap`/`rpm_usage_permille`/`slot_pressure_permille`/`p_avail` 全部原样。
2. **默认关**（`accountSharedRpmLimit: false`）。关时 `account_rpm_state` 返回空表，两个判定函数恒 `false` → 逐字节等价旧行为。
3. **单号池天然无影响**：单凭据时账号总量 ≡ 自身 RPM，账号阈值 ≡ 自身阈值，门的开合与 per-cred 门完全同时 → 只有多开账号才被收紧。

**⚠️ 必须与 #9 的教训对齐**：新门加在 `non_saturated`(2820) 就**必须**同步加进 `transient_wait_outcome`(2962)，否则 select 返 None 而等待判定报 `Available` → `WaitOutcome::Available => continue` 忙等死循环复发。故三处（硬门/亲和/等待）共用同一个 `is_account_rpm_saturated`。

**分步回滚方案**：
- 步骤 A（本 patch）：加开关 + 三处门，默认关。上线即零行为变化。
- 步骤 B：线上手工置 `true`，看 `/_shield/stats` 与面板 429（判据见 §4）。不对就把开关拨回 false 热重载，**无需回滚二进制**。
- 步骤 C（独立 PR，不在本 patch）：把「去重后的账号级池容量」喂给入站整形 target 与面板，并让 `throttle-autotune` 读它而不是自己重算算式。**注意：步骤 A/B 本身在软门默认（`rpmHardGateOverloadWait=false`）下只影响排序与亲和解绑，不减少放行量；真正压住放行量的是步骤 C 的 target。** 这一点必须写进 PR 说明，否则会误判「开了没效果 = 结论错了」。

### patch

**P1** `src/kiro/model/credentials.rs`（`family_key` 之后，`effective_idp` 之前）
old_string:
```rust
    pub fn effective_idp(&self) -> &str {
```
new_string:
```rust
    /// 账号身份键：**多开分身共享同一上游配额**，故限流要按账号而非按凭据聚合。
    ///
    /// - `kiro_api_key` 存在（多开分身场景）→ `acct:{sha256前16位}`：同 key 的 N 份分身归一账号。
    ///   只哈希不落明文，避免密钥进日志/面板。
    /// - 否则回落 `family_key`：M365 同租户仍归一族，social/IdC 各自独立成账号（与旧行为一致）。
    ///
    /// ⚠️ 刻意**不**用 `refresh_token` 分组：它每次刷新都会轮换，键会在运行期漂移，
    /// 导致账号桶被静默拆开——那比不聚合更坏（看着生效实则没生效）。
    pub fn account_key(&self, id: u64) -> String {
        if let Some(k) = self.kiro_api_key.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            use sha2::{Digest, Sha256};
            let h = Sha256::digest(k.as_bytes());
            return format!("acct:{}", &hex::encode(h)[..16]);
        }
        self.family_key(id)
    }

    pub fn effective_idp(&self) -> &str {
```
（若该文件未引 `hex`，改用 `h.iter().take(8).map(|b| format!("{b:02x}")).collect::<String>()`；`sha2` 已在 Cargo.toml:34。）

**P2** `src/model/config.rs` — 字段（`rpm_hard_gate_overload_wait` 之后，:225 一带）
old_string:
```rust
    #[serde(default)]
    pub rpm_hard_gate_overload_wait: bool,
```
new_string:
```rust
    #[serde(default)]
    pub rpm_hard_gate_overload_wait: bool,

    /// 同账号(多开分身)共享上游配额时，按**账号**聚合 RPM 再比阈值(默认 false=旧的按凭据)。
    /// 关键背景:阈值是每凭据的,5 份分身会让网关自以为有 5×167=835 RPM,而账号实测仅 ~134 RPM
    /// → 放行量虚高 6 倍 → 更早撞上游 429。开启后账号总量达"一份"的阈值即判饱和。
    /// 保守默认关:开关关时账号维度完全不参与判定,分流逐字节等价旧行为。
    #[serde(default)]
    pub account_shared_rpm_limit: bool,
```
default 里（config.rs:894 一带）
old_string: `            rpm_hard_gate_overload_wait: false,`
new_string:
```rust
            rpm_hard_gate_overload_wait: false,
            account_shared_rpm_limit: false,
```
（前端发 `accountSharedRpmLimit`；`UpdateConfigRequest` 加 `pub account_shared_rpm_limit: Option<bool>` + `service.rs` 照 `rpm_reserve_slots`(1971) 的写法接一段。）

**P3** `src/kiro/token_manager.rs` — 原子镜像（:1597）
old_string:
```rust
    rpm_hard_gate_overload_wait: AtomicBool,
```
new_string:
```rust
    rpm_hard_gate_overload_wait: AtomicBool,
    /// 按账号(多开分身共享配额)聚合 RPM 判饱和(默认 false=按凭据)。（原子镜像,reload 热更）
    account_shared_rpm_limit: AtomicBool,
```
构造（:1994）old: `            rpm_hard_gate_overload_wait: AtomicBool::new(rpm_hard_gate_overload_wait),`
new:
```rust
            rpm_hard_gate_overload_wait: AtomicBool::new(rpm_hard_gate_overload_wait),
            account_shared_rpm_limit: AtomicBool::new(config.account_shared_rpm_limit),
```
（同 1942-1944 的取值风格；若此处 `config` 已被移动，则在 1944 附近加 `let account_shared_rpm_limit = config.account_shared_rpm_limit;` 并用局部变量。）
热重载（:2086）old:
```rust
        self.rpm_hard_gate_overload_wait
            .store(new.rpm_hard_gate_overload_wait, Ordering::Relaxed);
```
new:
```rust
        self.rpm_hard_gate_overload_wait
            .store(new.rpm_hard_gate_overload_wait, Ordering::Relaxed);
        self.account_shared_rpm_limit
            .store(new.account_shared_rpm_limit, Ordering::Relaxed);
```

**P4** 两个新函数（插在 `apply_rpm_headroom` 之后，:3063 一带）
old_string:
```rust
    /// RPM 硬门在当前配置下是否**真的**对调度生效(而非仅仅是一个数字)。
```
new_string:
```rust
    /// 账号级 RPM 聚合状态:`account_key → (该账号名下全部凭据近 60s RPM 之和, 账号阈值)`。
    ///
    /// 账号阈值取成员中**最大**的 per-cred 阈值——账号容量是"一份",绝不随分身份数相乘。
    /// 禁用号也计入总量:它过去 60s 烧掉的上游配额一样真实存在。
    /// 开关关时返回空表 → 下游判定恒 false,零回归。
    ///
    /// 调用约定:调用方已持 `entries` 锁并把切片传进来(本函数不重入 entries,只锁 RpmTracker)。
    fn account_rpm_state(
        &self,
        entries: &[CredentialEntry],
    ) -> std::collections::HashMap<String, (u32, u32)> {
        let mut out = std::collections::HashMap::new();
        if !self.account_shared_rpm_limit.load(Ordering::Relaxed) {
            return out;
        }
        let ids: Vec<u64> = entries.iter().map(|e| e.id).collect();
        let counts = self.rpm.counts_for(&ids);
        for e in entries.iter() {
            let lim = self.effective_saturation_limit(e.credentials.rpm_limit);
            let slot = out.entry(e.credentials.account_key(e.id)).or_insert((0u32, 0u32));
            slot.0 = slot.0.saturating_add(counts.get(&e.id).copied().unwrap_or(0));
            slot.1 = slot.1.max(lim);
        }
        out
    }

    /// 该凭据所属账号是否已达账号级 RPM 上限(空表/无记录=false)。
    ///
    /// ⚠️ 硬门(non_saturated 过滤)、亲和复用(is_sticky_reuse_healthy)、背压等待
    /// (transient_wait_outcome) **三处必须都调它**。只加前者会让 select 返 None 而等待判定
    /// 报 Available → `WaitOutcome::Available => continue` 忙等死循环(已知问题 #9 同型)。
    fn is_account_rpm_saturated(
        &self,
        acct: &std::collections::HashMap<String, (u32, u32)>,
        e: &CredentialEntry,
    ) -> bool {
        if acct.is_empty() {
            return false;
        }
        matches!(acct.get(&e.credentials.account_key(e.id)), Some(&(t, l)) if t >= l)
    }

    /// RPM 硬门在当前配置下是否**真的**对调度生效(而非仅仅是一个数字)。
```

**P5** 硬门（:2820-2822）
old_string:
```rust
                        rpm_of(e.id)
                            < self.effective_saturation_limit(e.credentials.rpm_limit)
```
new_string:
```rust
                        rpm_of(e.id)
                            < self.effective_saturation_limit(e.credentials.rpm_limit)
                            // 账号级门:同账号分身共享上游配额,总量达"一份"阈值即整账号让路
                            && !self.is_account_rpm_saturated(&acct_rpm, e)
```
并在 `let rpm_of = ...`（:2675 一带）之后插入：
old_string:
```rust
        let rpm_of = |id: u64| rpm_counts.get(&id).copied().unwrap_or(0);
```
new_string:
```rust
        let rpm_of = |id: u64| rpm_counts.get(&id).copied().unwrap_or(0);
        // 账号级聚合:一次算好(含禁用号),供硬门与亲和判定共用,临界区内不重复加锁。
        let acct_rpm = self.account_rpm_state(&entries);
```
（`rpm_of` 那行原文缩进为 16 空格，套用时按文件实际缩进对齐。）

**P6** 亲和（:3108-3111 + 调用点 :2582）
old_string:
```rust
    fn is_sticky_reuse_healthy(&self, entry: &CredentialEntry) -> bool {
        if self.is_rpm_saturated_with_limit(entry.id, entry.credentials.rpm_limit) {
            return false;
        }
```
new_string:
```rust
    fn is_sticky_reuse_healthy(
        &self,
        entry: &CredentialEntry,
        acct: &std::collections::HashMap<String, (u32, u32)>,
    ) -> bool {
        if self.is_rpm_saturated_with_limit(entry.id, entry.credentials.rpm_limit) {
            return false;
        }
        // 账号级饱和同样要解绑:否则会话死粘一份分身,而整账号配额已被其它分身吃满。
        if self.is_account_rpm_saturated(acct, entry) {
            return false;
        }
```
调用点 old: `                        if self.is_sticky_reuse_healthy(entry) {`
new: `                        if self.is_sticky_reuse_healthy(entry, &self.account_rpm_state(&entries)) {`
（亲和分支在 :2582，早于 P5 的 `acct_rpm`，故就地算一次；命中亲和是快路径、池规模下成本可忽略。）

**P7** 背压等待（:2960-2962，与 P5 对齐）
old_string:
```rust
            if self.rpm_hard_gate_overload_wait.load(Ordering::Relaxed)
                && self.is_rpm_saturated_with_limit(entry.id, entry.credentials.rpm_limit)
```
new_string:
```rust
            if self.rpm_hard_gate_overload_wait.load(Ordering::Relaxed)
                && (self.is_rpm_saturated_with_limit(entry.id, entry.credentials.rpm_limit)
                    || self.is_account_rpm_saturated(&acct_rpm, entry))
```
并在 `let entries = self.entries.lock();`（:2908）后插 `let acct_rpm = self.account_rpm_state(&entries);`。

## 3. 能否测试

能，三个「移除即 FAIL」的骨架（放 `token_manager.rs` 测试模块，仿 9176/9540 的构造）：

```rust
// A. 账号级门拦下分身（旧代码必 PASS→改后 FAIL 的对照）
fn test_account_shared_rpm_gate_blocks_clone_overrun() {
    let mut config = Config::default();
    config.credential_rpm_limit = 3;
    config.rpm_headroom_factor = 100;          // 隔离 headroom,阈值恰为 3
    config.account_shared_rpm_limit = true;
    config.rpm_hard_gate_overload_wait = true; // 让 transient_wait_outcome 的背压分支可达
    // 3 个凭据，同一个 kiro_api_key("ksk_same") → 同 account_key
    // 各 record 1 次 → per-cred 1 < 3（旧口径全不饱和），账号总量 3 >= 3
    assert!(matches!(manager.transient_wait_outcome(None), WaitOutcome::Wait(_, WaitReason::RpmRecovery)));
    // 旧代码：immediate_available=true → WaitOutcome::Available（断言失败）
}

// B. 开关关 = 零回归（防"顺手改默认"）
fn test_account_gate_off_keeps_legacy_behavior() { /* 同 A 但 account_shared_rpm_limit=false → Available */ }

// C. 亲和解绑
fn test_account_saturation_releases_affinity() {
    // 会话绑 #1（自身 rpm=1 未饱和），#2/#3 各 record 1 → 账号总量 3
    // is_sticky_reuse_healthy(#1) == false；关开关则 true
}
// D. account_key：同 kiro_api_key 归一 / 不同 key 分开 / 无 key 回落 family_key
```

**构造不出测试、因此不该改行为的部分**：软门默认（`rpmHardGateOverloadWait=false`）下整账号饱和会回退「选最不坏」，`select_next_credential` 仍返 `Some` —— 放行量不减。这是刻意保留的旧行为，任何试图在软门下也拦住请求的改动都会变成「整池饱和即 503」，我不建议做。**放行量的收紧只能靠入站整形 target 用去重后的账号容量**（步骤 C），那部分属展示/整形口径，patch 不在本轮。

## 4. 与 shield 的关系：上线后怎么看出生效

基线（已知事实）：shield 吸收 178 次 429 中的 167，比 1.07:1；直连 72% 成功 / 84 个 429。

判据（同一 300 并发脚本，开关 `true` 前后各跑一次）：
1. **`/_shield/stats` 的 429 计数应显著下降**（吸收量是"网关放行过量"的直接读数）。若从 ~167 降到 <60，即账号级门确实在源头拦住了本该被上游拒的那批。
2. **shield 平均重试次数下降** → p50 应从 73.2s 明显回落（每次重试至少 `MIN_DELAY=1.0s`，少一次重试就少一秒以上）。
3. **有效吞吐不应低于 ~134 RPM**。若吞吐掉下 134，说明账号阈值取小了（检查是否误按 ÷N 生效），应回拨开关。
4. **反向证伪**：若 429 计数**没变**，先确认不是"软门吞掉了效果"（见 §3 末段）——查面板 insights 的 `rpmSaturated` 是否已在多分身号上亮起。亮了但 429 未降 = 账号聚合正确但放行量由入站 target 决定，需要走步骤 C；没亮 = `account_key` 未把分身归一（最可能是分身的 `kiroApiKey` 实际不同）。

未能确认：`throttle-autotune`（VPS 上，不在本仓）的算式改法 —— 禁网禁 ssh，只能从已知事实推断它需要改读去重容量，具体行号无法给出。