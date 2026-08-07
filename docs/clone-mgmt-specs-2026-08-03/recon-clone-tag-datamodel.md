Design + patch。

## 决策

**字段：两个扁平字段，不用枚举**（`clone_group: Option<String>` + `clone_seq: Option<u32>`）。理由：枚举变体在 `credentials.json` 里是新形状，回滚时旧版没有 `#[serde(other)]` 兜底会解析失败 → `CredentialsConfig::load` → `process::exit(1)`。两个 `Option` + `skip_serializing_if` 则旧版直接忽略未知键。**socks 节点不新增字段** —— 已有 `proxy_url`/`proxy_username`/`proxy_password`，构造不出「移除新字段即失败」的测试。

**主凭据标识：账号 key 的 SHA256 前 16 位 hex，而非 id**。id 会随删除/重新导入变（#404→#438），分身会挂在不存在的父上；而组身份的本质是「同一个账号」= 同一个 `kiroApiKey`。取前 16 位是因为它要下发给前端做分组，不需要全长 64。

**`clone_group` 持久化而非每次现算**：OAuth 号的 `refresh_token` 刷新后会变，现算会导致同组分身分组漂移；写死一次则冻结。API Key 号两者等价。

**不加 `clone_tag`**：`name` 字段已是自由文本备注（`credential-card.tsx` 已展示），前端可用 `clone_seq` + `name` 组合出标签。加它构造不出承重测试。

---

### 1. `src/kiro/model/credentials.rs` — 新增字段

old_string:
```rust
    /// 端点名称（可选）
    ///
    /// 决定该凭据走哪套 Kiro API。未配置时回退到 `config.defaultEndpoint`（默认 "ide"）。
    /// 端点名必须在启动时注册的端点 registry 中存在。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}
```
new_string:
```rust
    /// 端点名称（可选）
    ///
    /// 决定该凭据走哪套 Kiro API。未配置时回退到 `config.defaultEndpoint`（默认 "ide"）。
    /// 端点名必须在启动时注册的端点 registry 中存在。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,

    /// 「分身组」标识：同一个上游账号的全部凭据共享同一个值（账号 key 的 SHA256 前 16 位 hex）。
    ///
    /// ⚠️ **刻意不用父凭据 id**：id 会随删除/重新导入而变（线上从 #404 跑到 #438），
    /// 用 id 记父子关系会在换号后留下一批指向不存在凭据的孤儿，分身再也归不了组。
    /// 组身份的本质是「同一个账号」，而账号身份就是 `kiro_api_key`（或 OAuth 的 refreshToken）。
    ///
    /// **写入即冻结**，不随刷新变化：OAuth 号的 `refresh_token` 每次刷新都会换，
    /// 若每次现算哈希，同组分身会在各自刷新后分裂成不同组。
    ///
    /// `None` = 非分身 / 旧文件（前端回退用 `apiKeyHash` 分组）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clone_group: Option<String>,

    /// 该凭据在分身组内的序号（1 起）。`None` = 不是分身（本体或普通号）。
    ///
    /// 「一键删除分身」判据就是 `clone_seq.is_some()`：本体永远没有它，
    /// 因此即使父号已被删除、组里只剩分身，也不会把仅存的可用号一并清掉。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clone_seq: Option<u32>,
}

/// 由账号 key 派生「分身组」标识（SHA256 前 16 位 hex）。
///
/// 只取前 16 位：这个值要下发给前端做分组，不需要抗碰撞到 64 位；
/// 而 16 位 hex = 64 bit，在几十个凭据的量级上碰撞概率可忽略。
pub fn derive_clone_group(account_key: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(account_key.as_bytes());
    format!("{:x}", digest)[..16].to_string()
}
```

`impl Debug` 手写脱敏实现不含这两个字段亦无妨（它们非敏感）；若周边 Debug 逐字段列举，加两行 `.field("cloneGroup", &self.clone_group)` 同理。

---

### 2. `src/kiro/token_manager.rs` — 快照下发

old_string:
```rust
    /// 最近 60 秒滚动窗口内的请求数（RPM 观测）
    pub rpm: u32,
}
```
new_string:
```rust
    /// 最近 60 秒滚动窗口内的请求数（RPM 观测）
    pub rpm: u32,
    /// 分身组标识（同账号共享；`None` = 非分身或旧凭据，前端回退按 `apiKeyHash` 分组）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clone_group: Option<String>,
    /// 分身组内序号（1 起；`None` = 本体/普通号）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clone_seq: Option<u32>,
}
```

old_string:
```rust
                    inflight: e.inflight.load(Ordering::Acquire),
                    rpm: self.rpm.count(e.id),
                })
                .collect(),
```
new_string:
```rust
                    inflight: e.inflight.load(Ordering::Acquire),
                    rpm: self.rpm.count(e.id),
                    clone_group: e.credentials.clone_group.clone(),
                    clone_seq: e.credentials.clone_seq,
                })
                .collect(),
```

---

### 3. `src/admin/types.rs` — 请求与响应

`AddCredentialRequest` 加一个字段（前端发 `cloneTag`？**不需要** —— 组标识由后端派生，调用方不传）。只加响应侧。

old_string（`CredentialStatusItem`）：
```rust
    /// 最近 60 秒滚动窗口内的请求数（RPM 观测）
    pub rpm: u32,
    /// 用户自定义别名/备注（卡片展示优先于 email/#id）
```
new_string:
```rust
    /// 最近 60 秒滚动窗口内的请求数（RPM 观测）
    pub rpm: u32,
    /// 分身组标识（同一上游账号共享。`None` = 非分身/旧凭据，前端回退按 `apiKeyHash` 分组）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clone_group: Option<String>,
    /// 分身组内序号（1 起。`None` = 本体，「一键删除分身」不会碰它）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clone_seq: Option<u32>,
    /// 用户自定义别名/备注（卡片展示优先于 email/#id）
```

---

### 4. `src/admin/service.rs` — 映射 + 写入

**4a 映射**（`credentials_status` 一带，line ~423）

old_string:
```rust
                inflight: entry.inflight,
                rpm: entry.rpm,
                name: entry.name,
```
new_string:
```rust
                inflight: entry.inflight,
                rpm: entry.rpm,
                clone_group: entry.clone_group,
                clone_seq: entry.clone_seq,
                name: entry.name,
```

**4b 写入 `add_credential`**：`new_cred` 构造处（line ~1147），在 `endpoint: req.endpoint,` 之后。

old_string:
```rust
            kiro_api_key: req.kiro_api_key,
            endpoint: req.endpoint,
        };
```
new_string:
```rust
            kiro_api_key: req.kiro_api_key,
            endpoint: req.endpoint,
            // 分身标识：只有显式多开（`copies` 给值）才打，普通上号保持 None。
            //
            // 组 key 取**账号**（kiroApiKey，退化到 refreshToken），不取父凭据 id ——
            // id 会随删号/重导而变（线上 #404→#438），用 id 记父子会留下指向不存在
            // 凭据的孤儿。见 `derive_clone_group` 的说明。
            //
            // ⚠️ 第 1 份也打 `clone_seq`：多开的常态是「已有号再加 N 份」，此时池里本体
            // 已存在（本函数正是为它建分身），这 N 份**全都**是分身。若第 1 份不打，
            // 「一键删除分身」会漏掉它。
            clone_group: clone_group.clone(),
            clone_seq: clone_group.as_ref().map(|_| next_seq),
        };
```

紧接 `inherited` 计算之后（`let inherit = |mine: ...` 之前）插入组 key 与起始序号：

old_string:
```rust
        let inherit = |mine: Option<String>, pick: fn(&KiroCredentials) -> Option<String>| {
            mine.or_else(|| inherited.as_ref().and_then(pick))
        };
```
new_string:
```rust
        let inherit = |mine: Option<String>, pick: fn(&KiroCredentials) -> Option<String>| {
            mine.or_else(|| inherited.as_ref().and_then(pick))
        };

        // 分身组 key：仅多开路径派生。账号身份优先取 kiroApiKey，OAuth 号退化到 refreshToken。
        let clone_group = if req.copies.is_some() {
            req.kiro_api_key
                .as_deref()
                .or(req.refresh_token.as_deref())
                .map(crate::kiro::model::credentials::derive_clone_group)
        } else {
            None
        };
        // 起始序号 = 组内已有最大序号 + 1。**必须续号而非从 1 起**：
        // 「已有 4 个分身，再加 2 个」若从 1 重新编号，组内会出现两个 seq=1，
        // 面板上分身编号重复、无法定位是哪一份出的问题。
        let next_seq = clone_group
            .as_ref()
            .map(|g| {
                self.token_manager
                    .snapshot()
                    .entries
                    .iter()
                    .filter(|e| e.clone_group.as_deref() == Some(g.as_str()))
                    .filter_map(|e| e.clone_seq)
                    .max()
                    .unwrap_or(0)
                    + 1
            })
            .unwrap_or(1);
```

**4c 第 2..N 份递增序号**（`for seq in 2..=copies` 循环内）

old_string:
```rust
                let mut copy = new_cred.clone();
                copy.subscription_title = resolved_title.clone();
```
new_string:
```rust
                let mut copy = new_cred.clone();
                copy.subscription_title = resolved_title.clone();
                // 组内序号逐份递增（`new_cred` 拿的是 next_seq，本循环从第 2 份起）。
                copy.clone_seq = new_cred.clone_seq.map(|s| s + seq - 1);
```

---

### 5. `admin-ui/src/types/api.ts`

old_string:
```ts
  refreshFailureCount: number
  disabledReason?: string
```
new_string:
```ts
  refreshFailureCount: number
  /** 分身组标识（同一上游账号共享）。缺省 = 非分身/旧凭据，回退按 apiKeyHash 分组。 */
  cloneGroup?: string
  /** 分身组内序号（1 起）。缺省 = 本体，「一键删除分身」不会碰它。 */
  cloneSeq?: number
  disabledReason?: string
```

前端分组表达式：`const group = c.cloneGroup ?? c.apiKeyHash ?? c.refreshTokenHash ?? \`id:${c.id}\``；「是分身」判据 `c.cloneSeq != null`。

---

## 5. 向后兼容 / 回滚

- **全仓无 `deny_unknown_fields`**（`grep` 唯一命中是 `stream.rs:138` 的一句注释）。`CredentialsConfig` 是 `#[serde(untagged)]` 的 `Single|Multiple`，两个变体都是 `KiroCredentials`，未知键被忽略 → **旧版本读到 `cloneGroup`/`cloneSeq` 正常加载，不会 exit(1)**。
- 新字段 `#[serde(default, skip_serializing_if = "Option::is_none")]`：线上现有 `credentials.json` 无此键可加载；且非分身号回写时**不会新增键**，文件 diff 保持最小。

## 承重测试（各配一条，去掉实现即失败）

1. `derive_clone_group` 对同一 key 稳定、对不同 key 不同，且长度恒 16。
2. `KiroCredentials` 反序列化缺 `cloneGroup`/`cloneSeq` 的旧 JSON 成功且两字段为 `None`（锁 `#[serde(default)]`）。
3. `serde_json::to_value(非分身凭据)` 的 keys **不含** `cloneGroup`（锁 `skip_serializing_if`，即回滚安全）。
4. `serde_json::from_str::<CredentialsConfig>` 喂一个带 `cloneGroup` 的数组能成功（锁无 `deny_unknown_fields`，防将来有人加上）。
5. 源码级守卫（同 `explicit_copies_must_bypass_dedup_for_first_copy_too` 的写法）：`service.rs` 里 `clone_seq` 的赋值不得是字面 `Some(1)` —— 锁住「续号而非重编号」。

**未做**：`clone_tag` 字段（`name` 已覆盖，构造不出承重测试）、socks 节点字段（`proxy_url` 已覆盖）。