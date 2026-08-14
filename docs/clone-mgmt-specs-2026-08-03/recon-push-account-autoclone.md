Now I have everything needed. Producing the report.

## 1. 现状

**4 种请求体格式**（`src/admin/types.rs:1061` `parse_import_keys_request`，优先级互斥、自上而下）：

| # | 形态 | 行号 | 备注 |
|---|---|---|---|
| 1 | `{"items":[{"key","groups","endpoint","disabled"}]}` | types.rs:1082 | `groups` **接受但忽略**（本仓无分组概念，types.rs:1105 注释）；`endpoint` 空串按 None；`disabled` 缺省 false |
| 2 | `{"keys":["ksk_x",…]}` | types.rs:1123 | 元素非空字符串，`endpoint=None`/`disabled=false` |
| 3 | `{"apiKey":"ksk_x"}` | types.rs:1137 | 单条 |
| 4 | `{"kiroApiKey":"ksk_x"}` | types.rs:1138 | 同上（`or_else` 兜底） |

空 items → `Err("没有待导入的 Key")` → 400（handlers.rs:429-438）。

**`concurrencyLimit` 语义**：解析期校验 `0..=IMPORT_CONCURRENCY_LIMIT_MAX`（999），越界 400（types.rs:1071-1080）；随后**只在响应里原样回显，不生效**（service.rs:1280-1288 注释）。真实导入并发是常量 `IMPORT_MAX_IN_FLIGHT = 4`（service.rs:72），Semaphore 在 service.rs:1300 构造。

**两条路径**：
- `/api/admin/import/keys` — router.rs:67，在 `authed` 子树
- `/api/import/keys` — router.rs:177 `create_import_alias_router`，`main.rs:543` nest `/api`

**鉴权与行为完全一致**：同一个 `admin_auth_middleware`（router.rs:179-182）、同一个 handler `import_keys`。唯一差异是**路径**（外部对接方固定，不能改）与"别名树只暴露这一个端点"。

## 2. 「自动分身」配置项设计

`src/model/config.rs`，插在 `balance_refresh_interval_secs` 之后（第 498 行下）：

```rust
old_string:
    #[serde(default = "default_balance_refresh_interval_secs")]
    pub balance_refresh_interval_secs: u64,

new_string:
    #[serde(default = "default_balance_refresh_interval_secs")]
    pub balance_refresh_interval_secs: u64,

    // ============ 批量推号（import/keys）============
    /// 批量推号端点总开关，默认 **true**。
    ///
    /// 默认 true 而非 false：该端点已在线上被外部系统（kiro-accounting）依赖，
    /// 默认关会让升级即断推号链路 —— 这属于"升级造成的功能倒退"，比多一个开关更贵。
    #[serde(default = "default_true")]
    pub import_keys_enabled: bool,

    /// 推号成功后是否自动为新号建分身，默认 **false**。
    ///
    /// 必须默认 false：分身与主号共用同一账号配额，且分身需各自绑独立出口代理才有意义
    /// （没绑代理的分身只是把同一 IP 的放行量放大数倍 → 更早撞上游 429）。
    /// 升级后若默认开，外部推号方的既有协议会凭空多出 N 份未配代理的号。
    #[serde(default)]
    pub import_auto_copies_enabled: bool,

    /// 自动分身份数（含主号自身），默认 1 = 不分身。
    ///
    /// 仅在 `import_auto_copies_enabled=true` 时读取；实际值被 clamp 到
    /// `[1, MAX_CREDENTIAL_COPIES]`（16），与手工多开同一上限。
    #[serde(default = "default_import_auto_copies")]
    pub import_auto_copies: u32,
```

配套（config.rs:845 一带 `default_balance_refresh_interval_secs` 之后）：

```rust
old_string:
fn default_balance_refresh_interval_secs() -> u64 {

new_string:
/// 自动分身默认份数：1 = 不分身（与 `import_auto_copies_enabled=false` 双重保险）。
fn default_import_auto_copies() -> u32 {
    1
}

fn default_balance_refresh_interval_secs() -> u64 {
```

`Default for Config`（config.rs:932）：

```rust
old_string:
            balance_refresh_interval_secs: default_balance_refresh_interval_secs(),
            compression: CompressionConfig::default(),

new_string:
            balance_refresh_interval_secs: default_balance_refresh_interval_secs(),
            import_keys_enabled: default_true(),
            import_auto_copies_enabled: false,
            import_auto_copies: default_import_auto_copies(),
            compression: CompressionConfig::default(),
```

**接入点：`import_one_key` 成功分支（service.rs:1383-1387），不是 `import_keys`。**

⚠️ **不能走 `AddCredentialRequest.copies`**：`copies.is_some()` 会把 `allow_dup` 置 true（service.rs:1176），**第 1 份也绕过去重**。而推号是外部系统**周期性重推同一批 key**，去重是唯一防重复入池的门 —— 一旦绕过，每轮推号都整批复制一遍，池子指数膨胀。所以必须：主号照旧走去重的 `add_credential`，**成功后**再单独补分身。

```rust
old_string:
        match self.add_credential(add_req).await {
            Ok(_) => ImportKeyResult {
                ok: true,
                key: masked,
                error: None,
            },

new_string:
        match self.add_credential(add_req).await {
            Ok(resp) => {
                // 自动分身：默认关（import_auto_copies_enabled=false）。
                // 刻意不走 AddCredentialRequest.copies —— 那个字段一旦出现就让第 1 份
                // 也绕过去重，而推号是外部系统周期性重推同一批 key，去重是防重复入池的
                // 唯一门。故主号照旧走去重，成功后才单独补分身。
                let copies_created = self.spawn_import_copies(resp.credential_id).await;
                ImportKeyResult {
                    ok: true,
                    key: masked,
                    error: None,
                    copies_created,
                }
            }
```

失败分支同步补字段：

```rust
old_string:
                ImportKeyResult {
                    ok: false,
                    key: masked,
                    error: Some(e.to_string()),
                }

new_string:
                ImportKeyResult {
                    ok: false,
                    key: masked,
                    error: Some(e.to_string()),
                    copies_created: None,
                }
```

以及 panic 兜底槽（service.rs:1330-1334）：

```rust
old_string:
                slot.unwrap_or_else(|| ImportKeyResult {
                    ok: false,
                    key: "(unknown)".to_string(),
                    error: Some("导入任务异常终止".to_string()),
                })

new_string:
                slot.unwrap_or_else(|| ImportKeyResult {
                    ok: false,
                    key: "(unknown)".to_string(),
                    error: Some("导入任务异常终止".to_string()),
                    copies_created: None,
                })
```

## 3. 关键风险：自动分身失败绝不能拖垮推号

新方法放在 `import_one_key` 之后（service.rs:1398 一带，`delete_credential` 之前）。**签名返回 `Option<u32>` 而非 `Result`** —— 类型层面就不给调用方 `?` 的机会：

```rust
old_string:
    /// 删除凭据
    pub fn delete_credential(&self, id: u64) -> Result<(), AdminServiceError> {

new_string:
    /// 为刚推号成功的凭据补建分身。**永不返回 Err** —— 失败只记日志。
    ///
    /// 返回签名故意是 `Option<u32>` 而非 `Result`：推号的既有约定是"部分失败仍返 200
    /// 并逐条标 ok/error"，而调用方是外部系统（kiro-accounting）。若这里能返 Err，
    /// 将来有人顺手写个 `?` 就会把一个**已成功入池的号**标成导入失败 → 对接方重推 →
    /// 号池重复膨胀。类型上堵死比注释可靠。
    ///
    /// 返回 `None` = 未启用（不在响应里出现该字段）；`Some(n)` = 实际建成的额外份数
    /// （不含主号），失败时可能小于配置值甚至为 0。
    async fn spawn_import_copies(self: &std::sync::Arc<Self>, primary_id: u64) -> Option<u32> {
        let cfg = self.token_manager.config();
        if !cfg.import_auto_copies_enabled {
            return None;
        }
        let copies = effective_copies(Some(cfg.import_auto_copies));
        if copies <= 1 {
            return None;
        }

        // 以主号的当前形态为模板：region/subscription_title 等字段随之继承，
        // 避免分身打错 region host（ksk_ token 按 region 授权，错了必 403）。
        // id 无需清空 —— add_credential_inner 会用自增计数器覆写（token_manager.rs:5789）。
        let Some(template) = self.token_manager.export_credential(primary_id) else {
            tracing::warn!("推号自动分身：主号 #{} 已不在池中，跳过", primary_id);
            return Some(0);
        };

        let mut created = 0u32;
        for seq in 2..=copies {
            let mut copy = template.clone();
            // machineId 置 None：入池时按 kiroApiKey 派生 → 与主号确定性撞车 →
            // 撞车检测轮换成独立随机指纹。这正是"每份机器码不同"的来源。
            copy.machine_id = None;
            match self
                .token_manager
                .add_credential_allowing_duplicate(copy)
                .await
            {
                Ok(id) => {
                    self.token_manager.spawn_initial_refresh(id);
                    created += 1;
                }
                Err(e) => {
                    // 不回滚、不上抛：主号已成功入池，把它标失败会让对接方重推。
                    tracing::warn!(
                        "推号自动分身第 {}/{} 份失败（主号 #{} 已成功，不回滚）: {}",
                        seq,
                        copies,
                        primary_id,
                        e
                    );
                }
            }
        }
        Some(created)
    }

    /// 删除凭据
    pub fn delete_credential(&self, id: u64) -> Result<(), AdminServiceError> {
```

⚠️ `import_one_key` 需改签名为 `self: &std::sync::Arc<Self>`（现为 `&self`，service.rs:1349）—— 调用处 service.rs:1310 已是 `this.import_one_key(item)`，`this: Arc<Self>` 自动 deref，**改签名后调用处无需改动**：

```rust
old_string:
    async fn import_one_key(&self, item: ImportKeyItem) -> ImportKeyResult {

new_string:
    async fn import_one_key(self: &std::sync::Arc<Self>, item: ImportKeyItem) -> ImportKeyResult {
```

**判据（能构造"移除即失败"的测试）**：① `import_auto_copies_enabled=false` 时池计数增量恒为 1；② 开启 `import_auto_copies=3` 时增量为 3 且 `results[i].ok==true`；③ 主号 id 立刻被外部删掉（`export_credential` 返 None）时 `results[i].ok` 仍为 true。三条都可写成 `#[cfg(test)]`。

## 4. 响应体扩展

`src/admin/types.rs:971` `ImportKeyResult`：

```rust
old_string:
    /// 失败原因（成功为 None）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

new_string:
    /// 失败原因（成功为 None）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 该号自动建成的**额外**分身份数（不含主号）。
    ///
    /// `skip_serializing_if` 是硬要求：自动分身未启用时该字段**完全不出现**，
    /// 外部对接方（按 `ok`/`key`/`error` 解析）的报文与现在逐字节一致 ——
    /// 只增可选字段、不改既有字段，故不破坏任何既有解析。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub copies_created: Option<u32>,
}
```

序列化为 `copiesCreated`（结构体已有 `rename_all="camelCase"`，types.rs:973）。

**不加聚合字段**（如 `totalCopiesCreated`）：可由前端对 `results[]` 求和得出，`ImportKeysResponse::new` 已经因 `results`/`items` 双名而有一处冗余真相源，再加一个计数只会多一个"可能不一致"的面。

types.rs:1377 一带那个固定请求体测试会因新字段编译失败（若它构造了 `ImportKeyResult` 字面量）—— 应用 patch 时按编译器提示补 `copies_created: None`。

## 5. 推号总开关：关闭时返 403

**403，不是 404。** 理由：该端点在 `admin_auth_middleware` **之后**，能走到 handler 的一定是已通过 adminKey 的调用方。对已鉴权的运维返 404 是撒谎 —— 排障时会往"路径写错了 / 版本没这个端点"方向找，而真因是自己在设置里关了开关。403 + 明确 message 是可行动的信号。（404 只在"隐藏端点存在性"有安全价值时才划算，而这里存在性已经不是秘密。）

Handler 层收口（**一处覆盖两条路由**，因为二者共用同一 handler），`src/admin/handlers.rs:423`：

```rust
old_string:
pub async fn import_keys(
    State(state): State<AdminState>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    // 手工解析而非 #[derive(Deserialize)]：4 种互斥格式 + 越界校验要区分「字段缺失」
    // 与「类型/范围非法」，serde 的 untagged 无法给出可读的 400 原因。

new_string:
pub async fn import_keys(
    State(state): State<AdminState>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    // 总开关（默认开）。门放在 handler 而非 router：两条路径
    // （/api/admin/import/keys 与外部对接方固定的 /api/import/keys）共用本 handler，
    // 一处即全覆盖；且读的是 ArcSwap 现值，设置里改完即时生效，不需重启。
    //
    // 返 403 而非 404：能走到这里的调用方已通过 adminKey，对已鉴权方谎称"不存在"
    // 会把排障引向"路径/版本不对"，而真因是开关被关。
    if !state.service.is_import_keys_enabled() {
        return (
            axum::http::StatusCode::FORBIDDEN,
            Json(super::types::AdminErrorResponse::new(
                "feature_disabled",
                "批量推号已在设置中关闭（importKeysEnabled=false）",
            )),
        )
            .into_response();
    }

    // 手工解析而非 #[derive(Deserialize)]：4 种互斥格式 + 越界校验要区分「字段缺失」
    // 与「类型/范围非法」，serde 的 untagged 无法给出可读的 400 原因。
```

`service.rs`（放在 `get_config_snapshot` 附近，如 service.rs:1494 `tls_backend` 那个 getter 旁）：

```rust
old_string:
        self.token_manager.config().tls_backend

new_string:
        self.token_manager.config().tls_backend
    }

    /// 批量推号端点是否启用（默认 true，见 `Config::import_keys_enabled`）。
    pub fn is_import_keys_enabled(&self) -> bool {
        self.token_manager.config().import_keys_enabled
```

⚠️ 上面这段 patch 依赖 tls_backend getter 的收尾花括号形态，应用时按实际缩进对齐。

**snapshot / update 三处补字段**（前端读写用，均 camelCase）：
- `types.rs:742` `ConfigSnapshotResponse` 后加 `pub import_keys_enabled: bool` / `pub import_auto_copies_enabled: bool` / `pub import_auto_copies: u32`
- `types.rs:850` `UpdateConfigRequest` 后加同名 `Option<...>` 三个
- `service.rs:1592` 一带 snapshot 装配、`service.rs:2185` 一带 `if let Some(v) = req.…` 三段（全部**立即生效**，无需进 `restart_fields`：handler 与 `spawn_import_copies` 都活读 ArcSwap）
- `types.rs:1478` 那个测试用 snapshot 字面量需补三个字段

**前端应发**：`{"importKeysEnabled":true,"importAutoCopiesEnabled":false,"importAutoCopies":4}`（`UpdateConfigRequest` 有 `rename_all="camelCase"`，与 `SetProxyRequest` 的 snake_case 例外无关）。

**未确认**：`ImportKeyResult` 是否在 `types.rs` 测试模块外还有其它字面量构造点（只 grep 了 service.rs/handlers.rs）；应用 patch 时靠编译错误兜住。