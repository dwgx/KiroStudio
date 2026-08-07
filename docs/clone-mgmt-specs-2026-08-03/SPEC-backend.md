Anchors confirmed. Writing the spec.

```
# 「分身管理」后端实施规格 v1

## 0. 两路侦察结论的裁决

| 冲突点 | 侦察 A | 侦察 B | 裁决 |
|---|---|---|---|
| 账号身份派生 | `account_key(id)` → `acct:{sha256(key)[..16]}` 否则 `cred:{id}` | `derive_clone_group(account_key)` 再 SHA256 一次 | **取 A，删 B 的二次哈希**。B 会对已是哈希的串再哈希，前端拿到的 `cloneGroup` 与余额 key 不同源、无法互相 join；且多一个函数就多一处能写错的地方 |
| 余额缓存 key | 改成 `HashMap<String(account_key), _>` | — | **否决改 key**。`CachedBalancesResponse.balances` 是 `HashMap<u64,_>`（types.rs:493，前端两份类型都按 id 取）、`push_balance_snapshots_to_scheduler` 也按 id 组 `BalanceSnapshot`。改 key 要连带改 4 处读侧 + 落盘格式 + 前端。**改为写侧扇出**（见 §5） |
| socks 节点存储 | — | 独立文件 `socks_nodes.json` + at-rest 加密 | **采纳**。理由 B 已给足：config.json 写入是明文、旧版本 save 会抹掉未知字段 |
| `clone_tag` 自由文本 | 不加（已有 `name`） | 同 | **不加** |

`clone_group` 的值 = **父号 `account_key(parent_id)` 原样**，不再哈希。对 API Key 号 = `acct:{16hex}`（同 key 必同组）；对 OAuth 号 = `cred:{父id}`（冻结写入，父号删了分身仍同组）。

---

## 1. 数据模型

### P1-1 `src/kiro/model/credentials.rs` — 两个字段

old_string:
```rust
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}
```
new_string:
```rust
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,

    /// 「分身组」标识：同一上游账号的全部凭据共享同一个值，取值等于父号的
    /// [`account_key`](Self::account_key)。`None` = 非分身 / 旧文件。
    ///
    /// ⚠️ **刻意不用父凭据 id**：id 会随删除/重新导入而变（线上从 #404 跑到 #438），
    /// 用 id 记父子会在换号后留下一批指向不存在凭据的孤儿。
    ///
    /// **写入即冻结**，不随刷新重算：OAuth 号的 `refresh_token` 每次刷新都换，
    /// 若每次现算，同组分身会在各自刷新后分裂成不同组。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clone_group: Option<String>,

    /// 组内序号（2 起，第 1 份是本体故无此字段）。`None` = 本体 / 普通号。
    ///
    /// 「一键删除分身」的唯一判据就是 `clone_seq.is_some()`：本体永远没有它，
    /// 因此即使父号已被删、组里只剩分身，也不会把仅存的可用号一起清掉。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clone_seq: Option<u32>,
}
```
> 两字段都是 `Option` + `skip_serializing_if`，旧版本读新文件时按未知键忽略 → 回滚安全（不引入枚举变体，故无需 `#[serde(other)]`）。

### P1-2 同文件 — `account_key`（紧跟 `family_key` 之后）

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

    /// 账号键 —— **额度/余额**的共享单位，与 [`family_key`](Self::family_key) 正交。
    ///
    /// 分身（同一 `kiroApiKey` 多开 N 份）在上游是同一账号、同一份额度，本地却是 N 条凭据。
    /// 余额按凭据 id 缓存时各自查各自的、查询时刻不同 → 面板对同一个 key 显示 5 个不同
    /// 百分比（线上实测 64.7/66.1/90.1/66.5/90.2）。按账号共享后既消除不一致，又把上游
    /// `web_portal` 探测从 N 次降到 1 次 —— **重复探测同账号会加重风控**，这是主要收益。
    ///
    /// 为什么不复用 `family_key`：它对 M365 返回 `m365:{tenant}`，会把同租户的**不同账号**
    /// 并成一条余额（各自额度不同 → 显示错值）。它的语义是"限流连坐组"而非"同一额度池"。
    ///
    /// 为什么截断哈希而非原 key：该值会下发前端、并落盘到 `kiro_balance_cache.json`
    /// （无 at-rest 加密），明文密钥不得进去。16 hex = 64 bit，对号池规模碰撞可忽略。
    pub fn account_key(&self, id: u64) -> String {
        // 已归组的分身直接沿用冻结值：OAuth 号刷新后 refresh_token 会变，现算会分裂。
        if let Some(g) = self.clone_group.as_deref().filter(|g| !g.is_empty()) {
            return g.to_string();
        }
        match self.kiro_api_key.as_deref() {
            Some(k) if !k.is_empty() => {
                use sha2::{Digest, Sha256};
                let hex = format!("{:x}", Sha256::digest(k.as_bytes()));
                format!("acct:{}", &hex[..16])
            }
            // OAuth 号（social/idc/external_idp）无 API Key：各自独立，与改动前逐位相同
            _ => format!("cred:{id}"),
        }
    }

    pub fn effective_idp(&self) -> &str {
```
> 应用前 `grep -n "sha2" src/kiro/model/credentials.rs` —— 当前**没有** sha2 导入，故上面用函数内 `use`（与周边风格一致即可，也可提到文件头）。

### P1-3 `src/kiro/token_manager.rs` — 查询入口

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

    /// 单个凭据的账号键（余额共享单位，见 `KiroCredentials::account_key`）。
    pub fn account_key_of(&self, id: u64) -> String {
        let entries = self.entries.lock();
        entries
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.credentials.account_key(e.id))
            .unwrap_or_else(|| format!("cred:{id}"))
    }

    /// 与 `id` 同账号的全部凭据 id（含自己）。分身余额扇出与「删除分身」都用它。
    /// `id` 不存在时返回空 vec（调用方据此判 404，勿回退成 `vec![id]`）。
    pub fn account_siblings(&self, id: u64) -> Vec<u64> {
        let entries = self.entries.lock();
        let Some(key) = entries
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.credentials.account_key(e.id))
        else {
            return Vec::new();
        };
        entries
            .iter()
            .filter(|e| e.credentials.account_key(e.id) == key)
            .map(|e| e.id)
            .collect()
    }

    /// 组内**分身**（`clone_seq.is_some()`）的 id。本体恒不在结果里。
    pub fn account_clone_ids(&self, id: u64) -> Vec<u64> {
        let entries = self.entries.lock();
        let Some(key) = entries
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.credentials.account_key(e.id))
        else {
            return Vec::new();
        };
        entries
            .iter()
            .filter(|e| e.credentials.clone_seq.is_some())
            .filter(|e| e.credentials.account_key(e.id) == key)
            .map(|e| e.id)
            .collect()
    }
```

### P1-4 `token_manager.rs` — 快照下发（`CredentialSnapshot` 结构体 + 构造处各加 3 行）

结构体加：
```rust
    /// 账号键（余额共享单位；前端据此把同账号的号并成一组显示同一份余额）
    pub account_key: String,
    /// 分身组标识（`None` = 非分身；值等于父号 account_key）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clone_group: Option<String>,
    /// 组内序号（2 起；`None` = 本体）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clone_seq: Option<u32>,
```
构造处（`rpm: self.rpm.count(e.id),` 之后）加：
```rust
                    account_key: e.credentials.account_key(e.id),
                    clone_group: e.credentials.clone_group.clone(),
                    clone_seq: e.credentials.clone_seq,
```
> 前端收到 `accountKey` / `cloneGroup` / `cloneSeq`（`CredentialSnapshot` 已是 camelCase）。

### P1-5 SOCKS 节点：新文件 `src/kiro/model/socks_node.rs`

```rust
//! 「分身管理」页维护的可复用代理节点表。
//!
//! 与 `KiroCredentials.proxy_*` 的关系：这里是**候选池**（有哪些节点可用），
//! 凭据字段是**绑定结果**（这个号走哪个节点）。生成分身时从池里取节点写进凭据。
//!
//! 为什么独立成文件而非塞进 config.json：
//! ① `password` 必须随 at-rest 加密走，而 config.json 的写入路径是**明文**
//!    （`encrypt_credentials_at_rest` 只作用于 credentials/trash）；
//! ② 旧版本的 `Config` 没有该字段，回滚后一次 `save_config` 就把整张节点表抹掉。
//! 独立文件旧版本压根不读 → 回滚后完好。存放位置与 `trash.json` 同级（cache_dir）。

use serde::{Deserialize, Serialize};

fn default_true() -> bool { true }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SocksNode {
    /// 节点 id（自增，删除不复用）
    pub id: u64,
    /// 展示名（为空时前端回落显示 host:port）
    #[serde(default)]
    pub name: String,
    /// 代理 URL：`socks5://host:port` / `http://host:port`。可内嵌账密
    pub url: String,
    #[serde(default)]
    pub username: Option<String>,
    /// 落盘随文件加密，不得进日志
    #[serde(default)]
    pub password: Option<String>,
    /// 关掉的节点不参与「一键生成分身」，但保留记录
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 最近一次测速结果（None = 从未测过）
    #[serde(default)]
    pub last_test: Option<SocksNodeTest>,
    /// 创建时间（Unix 秒）
    #[serde(default)]
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SocksNodeTest {
    pub ok: bool,
    #[serde(default)]
    pub latency_ms: u64,
    #[serde(default)]
    pub exit_ip: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    /// 测速时刻（Unix 秒），前端标注新鲜度
    #[serde(default)]
    pub tested_at: f64,
}
```
`src/kiro/model/mod.rs` 加 `pub mod socks_node;`。

`AdminService` 加两个字段（`new()` 里初始化，抄 `cache_path` 那两行）：
```rust
    socks_nodes: Mutex<Vec<crate::kiro::model::socks_node::SocksNode>>,
    socks_nodes_path: Option<PathBuf>,
```
读写函数照抄 `token_manager::persist_trash`（`token_manager.rs:3852-3887`）：`secret_store::key_path_for` + `encode_for_disk` + `write_atomic`；读侧 `maybe_decrypt_to_string`，解析失败 `warn!` + 空表（fail-soft，绝不 bail —— 节点表坏了不该让服务起不来）。

---

## 2. API 端点全集

**复用，不要新造：**

| 用途 | 端点 | 备注 |
|---|---|---|
| socks 测速 | `POST /api/admin/proxy/test` | 请求 `{proxyUrl, proxyUsername?, proxyPassword?}`，恒 200，靠 `ok` 判成败。前端已有 `ops.ts:227` + `proxy-test-button.tsx` |
| 主凭据查余额 | `GET /api/admin/credentials/{id}/balance` | 改动后同账号共享缓存（§5） |
| 批量余额（列表用） | `GET /api/admin/credentials/balances/cached` | 只读缓存，不打上游 |
| 建分身（手工路径） | `POST /api/admin/credentials` + `copies` | 已实现，含 region 继承 |
| 删分身 | `POST /api/admin/credentials/batch-delete` `{ids, force:true}` | `force` 已有；ids 由 §4 的 GET 拿 |
| 改单号代理 | `POST /api/admin/credentials/{id}/proxy` | 已有 |

**新增 6 个**（全部挂 router.rs 的 `authed` 子树，走同一 `admin_auth_middleware`）：

| # | 方法 路径 | 请求体 | 响应 | 错误码 |
|---|---|---|---|---|
| N1 | `GET /socks/nodes` | — | `{total, nodes:[SocksNode]}`，**password 恒替换为 `null`，另给 `hasPassword: bool`** | — |
| N2 | `POST /socks/nodes` | `SocksNodeUpsertRequest` | `{id, message}` | 400 url 空 / 非 `socks5://\|socks5h://\|http://\|https://` 前缀 / 节点数超 `MAX_SOCKS_NODES=64`；404 `id` 给了但不存在 |
| N3 | `DELETE /socks/nodes/{id}` | — | `{deleted:bool}` | 404 |
| N4 | `POST /socks/nodes/{id}/test` | — | `ProxyTestResponse`（与 N-复用端点同形）+ 写回 `last_test` | 404；恒 200 表达成败 |
| N5 | `GET /clone-groups` | — | §4 | — |
| N6 | `POST /clone-groups/generate` | §3 | §3 | 400/404/409 |

N2 请求体（**camelCase**，前端发 `{"id":null,"name":"US-1","url":"socks5://h:1080","username":"u","password":"p","enabled":true}`）：
```rust
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SocksNodeUpsertRequest {
    /// None = 新建；Some = 按 id 覆盖（不存在则 404，不静默新建）
    #[serde(default)]
    pub id: Option<u64>,
    #[serde(default)]
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub username: Option<String>,
    /// 更新时**省略该字段**表示"不改密码"；给空串表示"清空密码"。
    /// 若不区分这两者，前端每次编辑名字都会把密码抹掉（N1 不回传密码，前端无从回填）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}
```
N4 实现：把 `handlers.rs:225-306` 的探测体抽成 `pub(crate) async fn run_proxy_probe(cfg: ProxyConfig, tls: ...) -> ProxyTestResponse`，`proxy_test` 与 N4 都调它。**不要复制那 80 行**（复制后 `PROXY_TEST_PROBE_URL` 会分叉）。

---

## 3. 一键生成分身（N6）

```rust
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateClonesRequest {
    /// 主凭据 id（分身的字段来源）
    pub credential_id: u64,
    /// 用哪些节点：每个节点建 1 份分身，节点顺序即 clone_seq 顺序。
    /// 空数组 → 走 `count` 分支（建不绑代理的分身，仅用于本机多指纹试探）。
    #[serde(default)]
    pub node_ids: Vec<u64>,
    /// `node_ids` 为空时的份数。与 `node_ids` 同时给值 → 400（语义歧义，不猜）。
    #[serde(default)]
    pub count: Option<u32>,
    /// 建成后是否直接启用，默认 true
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateClonesResponse {
    pub created: usize,
    pub requested: usize,
    pub credential_ids: Vec<u64>,
    /// 逐份失败原因（部分失败不回滚，与 copies/import 既有约定一致）
    pub failures: Vec<String>,
    pub message: String,
}
```
错误：400（`node_ids` 与 `count` 同时给 / 都不给 / 份数 clamp 后 <1）；404（`credential_id` 或某 `node_id` 不存在）；409（该账号现有凭据数 + 请求份数 > `MAX_CREDENTIAL_COPIES`=16）。

实现骨架（`AdminService`）：
```rust
pub async fn generate_clones(&self, req: GenerateClonesRequest)
    -> Result<GenerateClonesResponse, AdminServiceError>
{
    // ① 取父号完整凭据（export_credential 返回的是带敏感字段的克隆）
    let parent = self.token_manager.export_credential(req.credential_id)
        .ok_or(AdminServiceError::NotFound { id: req.credential_id })?;

    // ② 组标识：父号已归组则沿用，否则由父号 account_key 现算并**同时回写父号**
    //    （回写是必要的：不写父号就没有 clone_group，删分身时组边界只能靠现算，
    //     而 OAuth 号刷新后 refresh_token 变 → 现算漂移 → 分身归错组）
    let group = self.token_manager.account_key_of(req.credential_id);

    // ③ 容量门禁：同账号总数 + 新增 <= MAX_CREDENTIAL_COPIES
    let existing = self.token_manager.account_siblings(req.credential_id).len();
    let n = /* node_ids.len() 或 count */;
    if existing + n > MAX_CREDENTIAL_COPIES as usize { return Err(TooMany); }

    // ④ 逐份构建。region 三件套 + subscription_title 直接从 parent 带 —— 这是致命项：
    //    apiRegion 丢了 → ksk_ token 按 region 授权 → 403 bearer invalid → 实测 0% 成功
    let mut clone = parent.clone();
    clone.id = None;
    clone.machine_id = None;          // 置 None 让入池派生+撞车轮换出独立指纹
    clone.disabled = false;           // 父号被禁不传染
    clone.disabled_reason = None;
    clone.clone_group = Some(group.clone());
    clone.clone_seq = Some(next_seq); // 组内已有最大 seq + 1，最小从 2 起
    // 绑节点（node_ids 分支）：url/username/password 从节点表取，password 从加密文件读
    clone.proxy_url = Some(node.url.clone());
    clone.proxy_username = node.username.clone();
    clone.proxy_password = node.password.clone();
    let id = self.token_manager.add_credential_allowing_duplicate(clone).await?;
    self.token_manager.spawn_initial_refresh(id);
}
```
> ④ 之所以不复用 `add_credential(copies=N)`：那条路径从 `AddCredentialRequest` 拼凭据、且**只对 API Key 号**能靠 `find_credential_by_api_key` 继承（service.rs:1094）。这里直接克隆父凭据对象，OAuth 号同样能分身，且天然带全 region。两条路径并存，`copies` 不动。

---

## 4. 一键删除分身（复用 batch-delete）

**不新增删除端点。** N5 提供组视图，前端拿 `cloneIds` 直接打既有 `batch-delete`：

```rust
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloneGroupsResponse {
    pub total: usize,
    pub groups: Vec<CloneGroupItem>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloneGroupItem {
    /// 组标识（= 本体 account_key）
    pub group: String,
    /// 本体 id（`clone_seq.is_none()` 且组内最小 id；组内全是分身时为 null）
    pub primary_id: Option<u64>,
    /// 本体展示信息（email / name / 掩码后的 key 尾 4 位）
    pub primary_label: String,
    /// 全部分身 id（`clone_seq.is_some()`），直接喂给 batch-delete
    pub clone_ids: Vec<u64>,
    /// 组内成员概览：id / seq / 绑定的 proxyUrl（**掩码账密**）/ enabled / rpm
    pub members: Vec<CloneMemberItem>,
    /// 该账号共享的余额快照（缓存值，不触发上游；None = 从未查过）
    pub balance: Option<super::types::CachedBalanceItem>,
}
```
组装：`token_manager.snapshot().entries` 按 `account_key` 分桶 → 每桶按 `clone_seq` 排序 → `clone_ids` 收集 `clone_seq.is_some()` 的 id。

前端两步：`GET /clone-groups` → `POST /credentials/batch-delete {ids: cloneIds, force: true}`。`force:true` 是必需的（分身通常还处于 enabled，非强删会被"必须先禁用"挡下）。删除天然进回收站可恢复（`delete_credentials_batch` 既有行为），因此这个"一键"是可逆的。

---

## 5. 余额共享（写侧扇出，落盘格式不变）

三处写入点（`get_balance` 765、`refresh_all_balances_gently` 928，及未来任何写入）统一走一个新私有方法：

```rust
    /// 把一次余额取值写进**同账号全部凭据**的缓存槽。
    ///
    /// 分身是同一上游账号、同一份额度，但本地是 N 条凭据。此前按 id 各写各的、
    /// 取值时刻不同 → 面板对同一个 key 显示 5 个不同百分比（线上实测
    /// 64.7/66.1/90.1/66.5/90.2）。扇出后 N 条共享同一份数据与同一个 `cached_at`，
    /// 且后续 N-1 次查询都在 TTL 内命中缓存 → **上游 web_portal 探测降到 1 次**
    /// （重复探测同账号会加重风控，这是收益的主要部分）。
    ///
    /// 为什么不把缓存 key 直接换成 account_key：`CachedBalancesResponse.balances`
    /// 与 `push_balance_snapshots_to_scheduler` 都按凭据 id 组织，前端两份类型定义
    /// 也按 id 取值；换 key 要连带改 4 处读侧 + 落盘格式 + 前端。扇出只动写侧。
    fn store_balance_fanout(&self, id: u64, balance: &BalanceResponse) {
        let mut ids = self.token_manager.account_siblings(id);
        if ids.is_empty() {
            ids.push(id); // 号刚被删/查不到时至少写自己，行为与改动前一致
        }
        let now = Utc::now().timestamp() as f64;
        {
            let mut cache = self.balance_cache.lock();
            for sib in ids {
                cache.insert(sib, CachedBalance { cached_at: now, data: balance.clone() });
            }
        }
        self.save_balance_cache();
    }
```
`get_balance` 的读侧不用改：扇出后 id 自己那格已是共享值，TTL 命中即返回。

**旧缓存文件兼容**：格式仍是 `{"438": {...}}`（`HashMap<String(id), CachedBalance>`），`load_balance_cache_from` 一字不改，旧文件照旧加载，只是首次刷新后同组数值会对齐。回滚同样无碍。

> 乐观修正（`get_cached_balances` 里的 `credits_used` 推进）**保持按 id**：那是各分身**本地实际花掉**的量，扇出会重复计数。共享的是上游真值基线，本地增量各算各的 —— 这是刻意的。

---

## 6. 配置项汇总

| 字段 | JSON | 类型 | 默认 | serde | 语义 |
|---|---|---|---|---|---|
| `import_keys_enabled` | `importKeysEnabled` | `bool` | **true** | `#[serde(default = "default_true")]` | 推号端点总开关。默认 true 因线上 kiro-accounting 已依赖，默认关=升级即断链路 |
| `import_auto_copies_enabled` | `importAutoCopiesEnabled` | `bool` | **false** | `#[serde(default)]` | 推号后自动建分身。**必须默认 false**（用户明确要求；且未绑代理的分身只是把同 IP 放行量放大 → 更早撞 429） |
| `import_auto_copies` | `importAutoCopies` | `u32` | **1** | `#[serde(default = "default_import_auto_copies")]` | 自动分身总份数（含本体）。仅 enabled=true 时读，clamp `[1,16]` |
| `import_auto_copies_node_ids` | `importAutoCopiesNodeIds` | `Vec<u64>` | `[]` | `#[serde(default)]` | 自动分身按序绑的节点 id；空=不绑代理 |

四项全部 `#[serde(default)]` 系列 → 线上既有 config.json（无这些键）照旧加载，**服务能起来**。三层热重载：这四项**不进请求热路径**（只在 `import_keys` handler 入口读一次），走 TIER1 的 `ArcSwap` 配置镜像即可，无需新增 TIER2/TIER3 镜像。

shield 内置开关不在本规格范围（另一路负责）。

---

## 7. 实施顺序（7 个独立可回滚 commit）

| # | 主题 | 改动文件 | 新增测试 | 回退什么会让测试 FAIL |
|---|---|---|---|---|
| C1 | `account_key` + 两字段 | `kiro/model/credentials.rs` | `account_key_same_for_same_api_key`；`account_key_differs_for_oauth_by_id`；`account_key_frozen_by_clone_group`；`account_key_not_m365_tenant`（同租户不同账号必须不同键） | 去掉 `clone_group` 优先分支 → 冻结测试挂；改回 `family_key` → M365 测试挂 |
| C2 | `account_siblings` / `account_clone_ids` / 快照字段 | `kiro/token_manager.rs` | `siblings_group_same_key_credentials`；`clone_ids_excludes_primary`；`siblings_empty_for_unknown_id` | `account_clone_ids` 去掉 `clone_seq.is_some()` 过滤 → 第 2 个测试挂（本体被误列入待删） |
| C3 | 余额扇出 | `admin/service.rs` | `balance_fanout_writes_all_siblings`（一次 store 后 5 个 id 的 `cached_at`+数值全等）；`balance_fanout_falls_back_to_self_when_unknown`；`legacy_id_keyed_cache_file_still_loads` | 把 `store_balance_fanout` 换回 `cache.insert(id, …)` → 第 1 个测试挂 |
| C4 | socks 节点表 CRUD + 持久化 | `kiro/model/socks_node.rs`(新)、`kiro/model/mod.rs`、`admin/{service,handlers,types,router}.rs` | `upsert_then_list_roundtrip`；`omitted_password_keeps_existing`（省略 password 不抹密码）；`empty_password_clears`；`reject_non_socks_scheme`；`node_cap_enforced`；`corrupt_node_file_yields_empty_not_panic`；`persisted_file_is_encrypted_when_flag_on` | 把 `password: Option<String>` 改成必填 `String` → 第 2 个测试挂（编辑名字即抹密码）；去掉 scheme 白名单 → 第 4 挂 |
| C5 | `run_proxy_probe` 抽取 + N4 | `admin/handlers.rs`、`admin/router.rs` | `probe_url_is_single_source`（`PROXY_TEST_PROBE_URL` 只出现一次的静态断言/或 `proxy_test` 与 N4 共用同一常量的单测） | 复制探测体到 N4 → 常量出现两次，测试挂 |
| C6 | 生成分身 N6 + 组视图 N5 | `admin/{service,handlers,types,router}.rs` | `clone_inherits_all_regions`（api/auth/region 三项 + subscription_title 全等父号）；`clone_machine_id_is_none`；`clone_seq_starts_at_2_and_increments`；`clone_group_written_to_both_parent_and_clone`；`reject_node_ids_and_count_together`；`cap_16_enforced` | 删掉 region 继承 → 第 1 个测试挂（这正是线上 0% 成功那个缺陷的回归锁） |
| C7 | 4 个 import 配置项 + handler 门禁 | `model/config.rs`、`admin/handlers.rs`、`admin/service.rs` | `import_auto_copies_defaults_to_disabled`（空 JSON `{}` 反序列化后 enabled=false 且 copies=1）；`legacy_config_without_new_keys_loads`；`import_disabled_returns_403`；`auto_copies_clamped_to_16` | 任一字段去掉 `#[serde(default)]` → 第 2 个测试挂（等价于线上服务起不来）；默认改 true → 第 1 挂 |

C1→C2→C3 与 C4→C5 两条链独立，可并行；C6 依赖 C1/C2/C4；C7 依赖 C6（自动分身要调 `generate_clones`）。每个 commit 单独可回滚：C4/C5 回滚只丢节点表 UI，分身功能仍可用（手工填 proxy）；C6 回滚退回 `copies` 手工多开；C3 回滚退回按 id 各查各的（只是显示不一致，不崩）。
```