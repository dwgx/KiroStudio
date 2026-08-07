# 同账号余额同步 + 推号内置（自动分身默认关）—— 可实施方案

> 核实对象：`docs/clone-mgmt-specs-2026-08-03/recon-balance-shared-cache.md`（P1~P11）与
> `recon-push-account-autoclone.md`。本文只记**核实结论 + recon 漏掉的缺口**，不重写 patch。
> 行号按 2026-08-04 工作树；应用前重新定位。

---

## 1. BLOCKER 5 / 6 逐条判定

### BLOCKER 5（`refresh_all_balances_gently` 绕过缓存）→ **部分解**

P6 用 `refreshed_accounts: HashSet<String>` 在 `fetch_balance` 前按 account_key 去重。
**上游探测数确实从 N 降到"不同账号数"** —— 这一条真解了（`service.rs:919-945` 的循环）。

但 P6 有两个必修缺陷：

1. 🔴 **recon 自己的注释写反了。** 它说「`continue` 在 sleep 之后，所以被跳过的分身不会白等一个
   spacing」。事实相反：`if idx > 0 { sleep(spacing) }` 在 `:921-923`，而 P6 的锚点是
   `match self.fetch_balance(*id).await {`（`:925`）→ 插入点在 sleep **之后** →
   **每个被跳过的分身真睡满 4 秒**（spacing 由 `:1031` 硬编码 4）。40 号 / 8 账号：156s vs 应为 28s。
   **修法**：把去重检查移到 `if idx > 0` **之前**（同时 `:916` 的 info 日志改打去重后的数，否则撒谎）。
2. 🔴 **代表号探测失败 → 整账号本轮无余额。** 今天 5 分身 5 次机会，P6 后只剩 1 次。
   Err 分支（`:940-943`）必须 `refreshed_accounts.remove(&akey);`
   （Ok 分支里 akey 已被 `cache.insert` 移走，Err 分支未移走，可直接借用）。

### BLOCKER 6（乐观修正按 id 叠加）→ **真解了**，delta 语义也**正确**

P8 把 `used_now[id] - baselines[id]` 改为按 account_key 求和后整组共用。核实要点：

- **锚点/借用都成立**：old_string `for (id, item) in balances.iter_mut() {` 唯一，在 `service.rs:867`，
  在 `let mut balances = balances;`（`:866`）之后；`account_keys`（P7 引入的 owned map）活过
  `delta_by_account` 的 `&str` 借用，与 `iter_mut` 不冲突。
- **「d>0 才累加」的语义变化是对的。** 原语义是"只单向推进，delta<=0 不动"（`:871-874` 注释）。
  P8 逐 id 先过滤负 delta 再求和 = **逐号单向推进后合账**。若反过来先求和再判正负，一个刚重启、
  基线大于当前计数的分身（`total_credits_used` 从 0 重算）会用负 delta **抵掉兄弟号的真实花费**
  → 剩余额度偏高，正是原注释要防的方向。故 P8 更保守、更符合原意。
- 原 body 里留下的 `if !(delta > 0.0) { continue; }` 成为恒真冗余分支，无害可留。
- 既有测试 `optimistic_adjustment_is_monotonic_and_clamped`（`:3508`）语义不变仍绿。

### 顺带核实：BLOCKER 6(c)「调度侧 5 倍余额」→ **P10 未解，但不是本批引入的**

P10 扇出后 5 个 `BalanceSnapshot` 带同一 `remaining_at_cache`；`balance_factor`
（`token_manager.rs:4093-4116`）每号只减自己的花费 → 账号剩余被高估约 (N-1)/N 的兄弟花费。
**但今天逐 id 缓存时各 entry 的 remaining 也几乎相同**（同账号真值）→ 高估是既有行为，
P10 既没修也没恶化。前端不跨号求和（`StatusBars.tsx:303` 逐号算百分比，无 reduce）。
**结论：不在本批处理，记进已知问题。**

---

## 2. P1 能否编译 / 算术

**能编译。** `format!` 的临时 `String` 经 `Deref<Target=str>` 得到 `str` 位置表达式，`.to_string()`
对其自动取引用；临时值活到语句结束，无悬垂。**算术对**：`"acct:"` 5 字节 + 16 hex = 21；
`{:x}` 对 `Sha256::finalize()` 的 `GenericArray` 有 `LowerHex`（`token_manager.rs:108` 同写法），
产出 64 个 ASCII hex，截 21 恒落在字符边界，不 panic。

**两处必改（recon 只提了一半）**：
- `credentials.rs` **确认没有** sha2 import（只有 serde/fs/Path/ProxyConfig/Config/regions）
  → 必须补 `use sha2::{Digest, Sha256};`。`sha2 = "0.10"` 在 `Cargo.toml:34`，无需加依赖。
- **更省的替代（建议取这个）**：把 `account_key` 放 `token_manager.rs`，复用已有私有
  `fn sha256_hex`（`:104-109`）→ 零新 import，且与既有 `api_key_hash`（`:4801`、`types.rs:145`）
  **同一个哈希函数**，消掉审查 §7c 的"同一身份两套派生"。写作
  `format!("acct:{}", &sha256_hex(k)[..16])`，算术更直白。代价：`account_keys_snapshot` 内改用
  自由函数形态 `account_key_of_cred(&e.credentials, e.id)`。

---

## 3. 推号内置 + 自动分身

### 3.1 核实结论

| recon 主张 | 判定 |
|---|---|
| 不能走 `AddCredentialRequest.copies` | ✅ **对**。`service.rs:1173` `let allow_dup = req.copies.is_some();` → 带 copies 时第 1 份也绕去重（`:1174-1180`）；推号是周期性重推同一批 key，去重是唯一防重复入池的门。且该语义已被源码级守卫 `explicit_copies_must_bypass_dedup_for_first_copy_too`（`:3382`）钉死，不可改 |
| `parse_import_keys_request` 是手写解析器 | ✅ **对**，`types.rs:1061-1163`，4 格式（items/keys/apiKey/kiroApiKey）。serde default 无效，缺字段行为必须手写并测试 |
| `/api/import/keys` 永不带 autoClone | ✅ **对**，`router.rs:177 create_import_alias_router`。**结论：不要给请求体加 `autoClone` 字段** —— 分身与否只由 `config.import_auto_copies_enabled` 决定（默认 false），外部报文一字不改。这比"手写 `unwrap_or(false)`"更安全：没有字段就没有默认值写错的可能 |
| `import_keys_enabled` 默认 true | ✅ **必须**。线上 kiro-accounting 已在推，默认关＝升级即断链路 |
| 403 而非 404 | ✅ 同意（端点在 `admin_auth_middleware` 之后，`router.rs:179-182`） |
| `import_one_key` 改 `self: &Arc<Self>` | ⚠️ **不需要**。`spawn_import_copies` 只用 `self.token_manager`（`Arc<MultiTokenManager>`），`spawn_initial_refresh` 的 `self: &Arc<Self>` 是 **MultiTokenManager** 的接收者。保持 `&self` 更省。（改了也能编译：`this.import_one_key()` 会自动取 `&Arc<Self>`） |
| `spawn_import_copies` 调 `spawn_initial_refresh` | ⚠️ **是死代码**。`token_manager.rs:6405-6413` 对 `is_api_key_credential()` 直接跳过，而推号恒为 `auth_method="api_key"`（`service.rs:1356`）。删掉这行，别让后人以为它有用 |
| `ImportKeyResult` 加 `copies_created` | ✅ 可行。**编译会断在 4 处**：`types.rs:1337 / 1338 / 1388 / 1393`（测试字面量）+ `service.rs:1330 / 1363 / 1371` |

### 3.2 三处配置镜像的确切位置

`Config` 只有**两处**默认值真相源 + 一处快照字面量（审查说的 "handlers TIER3 static" **对本批不适用**：
这三个字段不在请求热路径，handler 与 `spawn_import_copies` 都活读 `token_manager.config()` 的 ArcSwap）：

| # | 位置 | 内容 |
|---|---|---|
| ① | `src/model/config.rs:497-498` 后 | 三个字段 + `#[serde(default = "default_true")]` / `#[serde(default)]` / `#[serde(default = "default_import_auto_copies")]` |
| ①' | `src/model/config.rs:845` `fn default_balance_refresh_interval_secs` 前 | `fn default_import_auto_copies() -> u32 { 1 }`（`default_true` 已存在于 `:829`） |
| ② | `src/model/config.rs:932` `balance_refresh_interval_secs:` 那行后 | `Default for Config` 三行 |
| ③ | `src/admin/types.rs:742` 后 / `:850` 后 | `ConfigSnapshotResponse` 三个必填 + `UpdateConfigRequest` 三个 `Option` |
| ③' | `src/admin/types.rs:1443`（`balance_refresh_interval_secs: 1800,` 那行）后 | ⚠️ 这里**不是** `impl Default for ConfigSnapshotResponse`（**该 impl 在仓里不存在**，审查 §2 的描述过期），而是测试 `:1420` 的手写字面量 → 加字段会编译失败，必须补 |
| ④ | `src/admin/service.rs:1596` 后 | snapshot 装配三行 |
| ⑤ | `src/admin/service.rs:2168-2173` 那块后 | `if let Some(v) = req.…` 三段。**不进 `restart_fields`**（`:1648` 起那套），活读即时生效 |

### 3.3 ⚠️ region：recon 完全没覆盖，必须一起做

`import_one_key`（`service.rs:1355-1361`）构造的 `AddCredentialRequest` **只有 4 个字段**，
region 三件套全 None；而 `inherited`（`:1094`）只在 `req.copies.is_some()` 时才算 → 推号路径
**永远拿不到 region**。ksk_ 号自动路由到 CLI 端点（`credentials.rs:711-726`），host 是
`q.{api_region}.amazonaws.com`（`endpoint/cli.rs:48`），region 缺失 → 回落 config 默认 →
403 bearer token invalid（HANDOFF §2.3 实测 0% vs 继承后 83/45/100/88%）。
`probe_regions_for`（`token_manager.rs:6500-6508`）**明确 bail api_key 号**，没有自动探测兜底。

VPS 上的 `import_compat.py`（`ws-vps/config/opt/kirostudio/bin/import_compat.py`，本地副本 408 行、
**无 region 逻辑 → 线上那份更新，实施前先 scp 回来核对**）今天靠"带 region 的号改走单条添加接口"
绕过。推号内置后必须把这段收进 Rust，否则新号继续丢 region。

**方案（4 处，各自独立可测）**：
1. `types.rs:952-959` `ImportKeyItem` 加 `pub region / auth_region / api_region: Option<String>`。
2. `types.rs:1100` 一带（格式 1 的 endpoint 解析旁）读同名 camelCase：
   `item.get("apiRegion").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)`。
   格式 2/3/4（`:1129 / 1144`）填 `None` —— 那三种格式没有承载位置，这是刻意的。
3. `service.rs:1355` 的 `AddCredentialRequest` 透传三个字段。
4. **兜底（一起做）**：三者皆 None 时用 `find_credential_by_api_key`（`token_manager.rs:2170`）
   查池中同 key 号继承 —— 覆盖"重推已在池中的号"这个最常见场景。新 key 只能靠请求带。

不必校验 region 白名单：`sanitized_region`（`credentials.rs:429-435`）在**读侧**过滤，污染值只会
静默回落 config，拼不出恶意 host。

### 3.4 P9 有一个真 bug（顺带修）

P9 在 `delete_credential_forced`（`service.rs:1386-1398`）删除**之后**才调 `account_key_of(id)`，
而 `token_manager.rs:5995` 已把 entry `remove` 出 `entries` → 落到
`unwrap_or_else(|| format!("cred:{id}"))` → api_key 号拿到**错的键** → `still_used` 恒 false，
`remove` 删了个不存在的键 → **共享缓存永不清理**（不损坏数据，只泄漏条目，7 天自然淘汰）。
`purge_credential`（`:1474`）同理。**修法**：统一孤儿清理，两处都调，与删除顺序无关：
```rust
/// 清掉不再被任何在池凭据引用的余额缓存条目（账号键可被同账号分身共用，
/// 故不能按被删的 id 直接 remove —— 那会抹掉存活分身的余额）。
fn prune_balance_cache_orphans(&self) {
    let live: HashSet<String> = self.token_manager.account_keys_snapshot().into_values().collect();
    let mut cache = self.balance_cache.lock();
    cache.retain(|k, _| live.contains(k));
}
```

### 3.5 落盘（P11 之外）

`save_balance_cache`（`service.rs:3056-3074`）的 `k.to_string()` 对 `String` key 合法 —— recon 对。
若 clippy 报 `to_string` 风格提示，改成 `(k.clone(), v.clone())`。

---

## 4. 「回退即 FAIL」测试清单

`add_credential` 在 `service.rs:1184` 无条件 `await get_usage_limits_for`（真实上游）。
它**不会让测试 panic**（失败只 `warn!`），但每条要吃一次网络超时 → **穿它的行为测试不写**，
改为源码级守卫（本仓已有先例 `service.rs:3382`、`3370`）。

**行为测试（可写，纯内存）**

| # | 测试 | 断言 | 旧代码为何 FAIL |
|---|---|---|---|
| B1 | `cached_balances_shared_across_clones_of_same_account` | 两条同 `kiro_api_key` 凭据 → 播种 1 条缓存（按 akey）→ `total==2`，两者 `remaining` 相等，`balances[&2].balance.id==2` | 缓存按 id 存，#2 查不到 → total==1 |
| B2 | `optimistic_delta_is_summed_per_account_not_per_id` | #1 花 5、#2 花 3 → 两条 `remaining` 都是 90-8=82 且**彼此相等** | 按 id 叠加 → 85 与 87 → **正是用户截图的发散** |
| B3 | `optimistic_negative_delta_of_one_clone_does_not_offset_sibling` | #1 基线 999（d<0）、#2 花 5 → 组 delta 恒为 5，不是 -994 | 先求和再判正负会得 0 → 余额偏高 |
| B4 | `deleting_one_clone_keeps_shared_balance_cache` | `delete_credential_forced(2,true)` 后 #1 仍有缓存；再删 #1 后缓存清空 | P9 原样写法：第二步不清（键算错） |
| B5 | `load_balance_cache_accepts_legacy_numeric_keys` | 写 `{"1":{...}}` → load 后含 `"cred:1"`、len==1、不 bail | 类型改了但没归一 → 编译过、数据静默全丢 |
| B6 | `import_item_carries_api_region_through_parser` | `{"items":[{"key":"ksk_x","apiRegion":"eu-central-1"}]}` → `items[0].api_region == Some("eu-central-1")` | 解析器不读该字段 → None → 落错 region |
| B7 | `import_legacy_formats_have_none_region` | `{"keys":[...]}` / `{"apiKey":...}` → 三个 region 均 None，且**不报错** | 若给 keys[] 加必填 region 会让外部整批 400 |
| B8 | `import_external_payload_still_parses_without_new_fields` | 复用 `types.rs:1367` 那个对接方固定报文 | 新增字段若忘了 Option → 400 断链路 |
| B9 | `import_auto_copies_disabled_by_default` | `Config::default().import_auto_copies_enabled == false` 且 `import_keys_enabled == true` 且 `import_auto_copies == 1` | 默认写错 = 每个新号 16 份分身 |
| B10 | `import_config_defaults_consistent_across_mirrors` | 三处：`default_*()` == `Config::default().*` == snapshot 字面量 | 抄 `config.rs:1008` 那条已有测试的形式 |
| B11 | `disabled_import_endpoint_returns_403_not_404` | 直接调 `is_import_keys_enabled()` + 断言 handler 里 `StatusCode::FORBIDDEN`（源码级即可） | — |
| B12 | `refresh_ids_are_deduped_by_account` | 3 分身 1 账号 → 去重后 ids 长度 1（把去重抽成纯函数 `dedup_ids_by_account`） | 不去重 → 3 次上游探测 |

**源码级守卫（穿上游的部分只能这么测）**

| # | 测试 | 断言的源码事实 |
|---|---|---|
| S1 | `auto_clone_must_not_use_copies_field` | `import_one_key` 的 `AddCredentialRequest` 字面量**不含** `copies:` —— 否则第 1 份绕过去重、周期重推即池子膨胀 |
| S2 | `spawn_import_copies_never_returns_result` | 函数签名含 `-> Option<u32>`，不含 `-> Result` —— 类型上堵死 `?` 把已入池的号标失败 |
| S3 | `import_one_key_passes_region_fields` | 该字面量包含 `api_region:` |
| S4 | `refresh_error_returns_account_key_to_pool` | `refresh_all_balances_gently` 的 Err 分支包含 `refreshed_accounts.remove(` |

⚠️ 现有 3 处测试会因 P3（key 改 `String`）**编译失败**，必须同批改：
`service.rs:3473`、`:3509`（`insert(1, …)` → `insert(svc.token_manager.account_key_of(1), …)`）、
`:3565-3567`（`contains_key(&1)` → `contains_key("cred:1")`）。

---

## 5. i18n（各 6 键 × 3 语言，按字母序插入）

三份文件各 1523 键、键集完全一致、按键名字母序。插在 `settingspage.hint.restartRequired`
（`zh.json:1143`）与 `settingspage.loadBalance.balanced`（`:1144`）之间；en/ja 同位置。

```json
  "settingspage.import.autoCopies.count.label": "自动分身份数",
  "settingspage.import.autoCopies.hint": "默认关闭。开启后推号成功会自动为新号建分身（含本体最多 16 份）。⚠️ 未给分身绑独立出口代理时，它只是把同一个 IP 的放行量放大，会更早撞上游 429；且外部推号方的既有协议不会带这个意图，开启即改变上号行为。",
  "settingspage.import.autoCopies.label": "推号后自动分身",
  "settingspage.import.enabled.hint": "关闭后 /api/import/keys 与 /api/admin/import/keys 一律返回 403（已鉴权调用方看到明确原因，不谎称路径不存在）。⚠️ 外部推号系统正依赖此端点，关闭即断链路。保存即时生效。",
  "settingspage.import.enabled.label": "批量推号端点",
  "settingspage.import.sharedBalance.hint": "同一个 Key 的多份分身在上游是同一个账号、同一份额度，面板按账号共享一条余额缓存：数字一致，且后台探测按账号只打一次（重复探测会加重风控）。"
```

```json
  "settingspage.import.autoCopies.count.label": "Auto-clone copies",
  "settingspage.import.autoCopies.hint": "Off by default. When on, each successfully pushed key also gets clones (up to 16 including the original). ⚠️ Without a dedicated egress proxy per clone this only multiplies traffic from the same IP and hits upstream 429 sooner; external pushers never signal this intent, so turning it on changes onboarding behavior.",
  "settingspage.import.autoCopies.label": "Auto-clone after push",
  "settingspage.import.enabled.hint": "When off, both /api/import/keys and /api/admin/import/keys return 403 (an authenticated caller gets the real reason instead of a fake 404). ⚠️ External push systems depend on this endpoint; turning it off breaks them. Applies immediately on save.",
  "settingspage.import.enabled.label": "Bulk key push endpoint",
  "settingspage.import.sharedBalance.hint": "Clones of the same key are one upstream account sharing one quota. Balances are cached per account, so the numbers match and background probing hits upstream once per account (repeat probes increase throttling risk)."
```

```json
  "settingspage.import.autoCopies.count.label": "自動分身の数",
  "settingspage.import.autoCopies.hint": "既定でオフ。オンにすると、プッシュ成功した新規キーに分身を自動作成します（本体を含め最大 16 個）。⚠️ 各分身に個別の送信プロキシを割り当てない限り、同一 IP からの通過量が増えるだけで上流 429 に早く到達します。外部プッシュ側の既存プロトコルはこの意図を送らないため、オンにすると登録の挙動が変わります。",
  "settingspage.import.autoCopies.label": "プッシュ後に自動分身",
  "settingspage.import.enabled.hint": "オフにすると /api/import/keys と /api/admin/import/keys は 403 を返します（認証済み呼び出し元には偽の 404 ではなく実際の理由を返す）。⚠️ 外部プッシュ系がこのエンドポイントに依存しています。オフにすると連携が切れます。保存で即時反映。",
  "settingspage.import.enabled.label": "キー一括プッシュ",
  "settingspage.import.sharedBalance.hint": "同一キーの分身は上流では同一アカウント・同一枠です。残量は アカウント単位でキャッシュされるため表示が一致し、バックグラウンド探索もアカウントごとに 1 回だけ実行されます（重複探索は制限リスクを高めます）。"
```

**前端挂载**（4 处，照 `loginBackgroundR18` 的既有形态）：
`settings-page.tsx:285` `FormState` 加 3 字段 → `:375` `toForm` 加
`importKeysEnabled: c.importKeysEnabled ?? true` / `importAutoCopiesEnabled: c.importAutoCopiesEnabled ?? false` /
`importAutoCopies: String(c.importAutoCopies ?? 1)` → `:1616` diff 三行（缺省基线必须与 toForm 一致）→
JSX 用 `<Switch>` + `<NumberStepper min={1} max={16}>`（份数 Switch 关时 `disabled`，同 R18 的
`disabled={!form.loginBackgroundEnabled}` 写法）。类型两处：`api.ts:413` 后（Snapshot）、`:487` 后（Update）。

---

## 6. 实施顺序（每步独立可回滚）

① `account_key` / `account_key_of` / `account_keys_snapshot`（放 token_manager 复用 `sha256_hex`）→
② P3~P11 + §1 的两处 P6 修正 + §3.4 的 `prune_balance_cache_orphans` + 改 3 处既有测试 →
③ 三配置项（§3.2 的 6 处）+ handler 403 门 → ④ **region 透传单独一个提交**（它是今晚 #448/#449
的直接修复）→ ⑤ `spawn_import_copies` + `copies_created` → ⑥ 前端 3 开关 + i18n 18 键 →
⑦ `cargo test/clippy --no-default-features` + `tsc` → ⑧ 确认 §3.3 覆盖了线上 shim 的 region
逻辑后才可下线 `import_compat.py`。
