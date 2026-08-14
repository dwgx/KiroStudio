# 分身管理页 + SOCKS 节点 —— 可直接实施清单（2026-08-04）

只读侦察已完成，本文是**照做清单**。行号取自 2026-08-04 工作树（有其他会话未提交改动，改前重新 grep 定位）。
前置阅读 `docs/clone-mgmt-specs-2026-08-03/`，本文**覆盖**其中与代码不符的部分。
三语 i18n 的可粘贴 JSON 在同目录 `docs/clone-page-i18n.json`（见 §5）。

## 0. 先纠正三份旧规格里套不上的 anchor（已 grep 核实）

| 旧规格说 | 实际 | 影响 |
|---|---|---|
| 前端**两份** `CredentialStatusItem`（`api.ts:39` + `ops.ts:78`） | **只有一份**：`admin-ui/src/types/api.ts:15`。`ops.ts` 里没有凭据类型（只有 `TraceRecord`/`ProxyTestResult`）。"两份"实为 `retries` 那条（`api.ts:679` + `ops.ts:183`，与本任务无关） | 第 5 跳只改 1 个文件 |
| `CredentialSnapshot` | 不存在。实为 `CredentialEntrySnapshot`（`token_manager.rs:1396`，`rpm` 收尾于 `:1466`） | patch anchor 换名 |
| `SECTION_DEFS.icon` 用 `<Copy className=…/>` | 类型是 `React.ComponentType<{className?:string}>`（`settings-page.tsx:127`） | 必须写 `icon: Copy`，写 JSX 直接 tsc 报错 |
| JSX 挂载点 `:1791` | 回收站 `SectionGate` 实为 `settings-page.tsx:1835-1837` | — |
| `derive_clone_group()` 二次哈希 | 见 §1，**不要这个函数** | — |
| `KiroCredentials.copies` 字段 | 不存在。`copies` 只在 `AddCredentialRequest`（`admin/types.rs:379`），`MAX_CREDENTIAL_COPIES=16` 在 `service.rs:88` | — |

## 1. 三个身份键冲突 —— 统一方案（先做这个，否则后面全歪）

**结论：只加 1 个函数 + 1 个 pub 包装，不加 `derive_clone_group`，不新造 16-hex 哈希。**

| 名字 | 位置 | 语义 | 持久化 | 上 wire |
|---|---|---|---|---|
| `family_key`（已有） | `credentials.rs:762` | **限流连坐组**（M365 同租户 → `m365:{tenant}`） | 否，现算 | 否 |
| `api_key_hash`（已有） | `token_manager.rs:1426` / `admin/types.rs:62` / `api.ts:39` | key 的**完整 64 hex** sha256，前端去重用 | 否，现算 | ✅ 已下发 |
| `clone_group`（新，字段） | `credentials.rs` 尾部 | **分身组身份**，写入即冻结 | ✅ | ✅ 新增 |
| `account_key()`（新，方法） | `credentials.rs`，紧跟 `family_key` 后（`:788` 之后） | **同一上游账号**（余额/额度共享单位） | 否，现算 | ❌ **不下发** |

`account_key` 实现（三行优先级，**不再截断哈希**）：

```rust
    /// 账号键 —— 余额/额度共享单位，与 [`family_key`](Self::family_key)（限流连坐）正交。
    ///
    /// 三段优先级，顺序不可换：
    /// ① `clone_group` 有值 → 直接用（**冻结**）。OAuth 号 refresh_token 每次刷新都变，
    ///    现算会让同组分身在各自刷新后分裂；父号换 key 也不会把分身踢出组。
    /// ② 有 `kiro_api_key` → `acct:{完整 sha256 hex}`。**刻意复用 `api_key_hash` 的同一口径**
    ///    （`token_manager.rs:4801`），不截断到 16 位：同一身份两套派生必然漂移，
    ///    且截断值一旦落进 `kiro_balance_cache.json` 就成了第三份真相。
    /// ③ 否则 `cred:{id}` —— 与改动前逐位相同（零行为变化）。
    pub fn account_key(&self, id: u64) -> String {
        if let Some(g) = self.clone_group.as_deref().filter(|g| !g.is_empty()) {
            return g.to_string();
        }
        match self.kiro_api_key.as_deref() {
            Some(k) if !k.is_empty() => {
                use sha2::{Digest, Sha256};
                format!("acct:{:x}", Sha256::digest(k.as_bytes()))
            }
            _ => format!("cred:{id}"),
        }
    }
```

**`cred:{id}` 的 id 复用风险怎么处理**（对抗审查 BLOCKER #7）：不改 `next_id`。
改判据 —— **任何破坏性操作（删分身/扇出）只认 `clone_seq.is_some()` 且 `clone_group` 字面相等**，
`account_key` 回落到 `cred:{id}` 的分支**永远进不了分身组**（因为那种号没有 `clone_group`）。
即 `cred:{id}` 只用于"我自己一个人"的退化场景，撞 id 也只会撞到一个孤立号。
配套测试：`account_clone_ids(新号 id)` 在新号无 `clone_group` 时必须返回空 vec。

`account_key_of` / `account_siblings` / `account_clone_ids` 三个 pub 方法照 `SPEC-backend.md` P1-3 抄
（`family_key_of` 在 `token_manager.rs:4119` 是**私有**的，新的三个必须 `pub`）。

## 2. clone 字段 5 跳管线（逐跳 file:line + patch）

### 跳 1 — `src/kiro/model/credentials.rs:232`（`pub endpoint` 之后、`}` 之前）

```rust
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,

    /// 分身组标识：同一上游账号的全部凭据共享。值 = 建组时父号的 `account_key()`。
    /// **写入即冻结**（见 `account_key` ① 段）。`None` = 非分身/旧文件。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clone_group: Option<String>,

    /// 组内序号（本体不写，故最小为 2）。`None` = 本体/普通号。
    /// 「一键删除分身」的唯一判据是 `clone_seq.is_some()` —— 本体永远没有它，
    /// 因此父号已删、组里只剩分身时也不会把仅存可用号清掉。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clone_seq: Option<u32>,
}
```
> 全仓无 `deny_unknown_fields` → 旧二进制读新文件不 `exit(1)`。但旧版一次 `persist_credentials()` 会抹掉这两键：
> 回滚一次即失去分身标识（分组可由 `api_key_hash` 自愈，`clone_seq` 不能）。**在 CHANGELOG 里明写这条，不做回填 reconcile。**

### 跳 2 — `src/kiro/token_manager.rs:1466`（`CredentialEntrySnapshot.rpm` 之后）+ `:4832`（构造处 `rpm: self.rpm.count(e.id),` 之后）

```rust
    pub rpm: u32,
    /// 分身组标识（`None` = 非分身；前端缺省时回退按 `apiKeyHash` 分组）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clone_group: Option<String>,
    /// 组内序号（2 起；`None` = 本体）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clone_seq: Option<u32>,
}
```
```rust
                    rpm: self.rpm.count(e.id),
                    clone_group: e.credentials.clone_group.clone(),
                    clone_seq: e.credentials.clone_seq,
```
⚠️ **不要**把 `account_key` 加进快照。它是现算的内部键，下发即产生"前端拿它 join `apiKeyHash`"的错误用法。

### 跳 3 — `src/admin/types.rs:102`（`CredentialStatusItem.rpm` 之后、`name` 之前）

```rust
    pub rpm: u32,
    /// 分身组标识（同一上游账号共享）。缺省 = 非分身/旧凭据
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clone_group: Option<String>,
    /// 组内序号（2 起）。缺省 = 本体，「一键删除分身」不会碰它
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clone_seq: Option<u32>,
    /// 用户自定义别名/备注（卡片展示优先于 email/#id）
```

### 跳 4 — `src/admin/service.rs:422`（`rpm: entry.rpm,` 之后、`name: entry.name,` 之前）

```rust
                rpm: entry.rpm,
                clone_group: entry.clone_group,
                clone_seq: entry.clone_seq,
                name: entry.name,
```
> 这一跳是**最容易漏**的：`CredentialStatusItem` 是逐字段搬运（`service.rs:384-425`），
> 漏了不报错（Rust 会报缺字段 —— 实际上会编译失败，这是好事）。真正静默的是跳 5。

### 跳 5 — `admin-ui/src/types/api.ts:56`（`rpm?: number` 之后）

```ts
  rpm?: number
  /** 分身组标识（后端 cloneGroup）。缺省 = 非分身/旧数据，回退按 apiKeyHash 分组。 */
  cloneGroup?: string
  /** 组内序号（2 起）。缺省 = 本体 —— 「删除分身」只认有此字段的。 */
  cloneSeq?: number
```
前端分组：`const key = c.cloneGroup ?? c.apiKeyHash ?? \`id:${c.id}\``；是分身 = `c.cloneSeq != null`。
**禁止**把 `cloneGroup` 与 `apiKeyHash` 字符串 join（前者带 `acct:` 前缀，后者裸 hex）。

### 写入端（生成分身）

新端点 `POST /api/admin/credentials/{id}/clone`，走 `service.rs` 新方法，**不复用 `add_credential(copies)`**
（那条路只对 API Key 号能继承，见 `service.rs:1094-1100`；OAuth 号分身要靠克隆 `export_credential(id)`
的完整对象，`token_manager.rs:2186`）。骨架照 `SPEC-backend.md` §3，四条铁律：
1. region 三件套 + `subscription_title` 必须从父号带（不带 → 403 bearer invalid，实测 0% 成功，`service.rs:1080-1090` 注释有依据）
2. `machine_id = None`（入池派生→撞车→自动轮换出独立指纹）
3. `clone_seq` = 组内现有最大 seq + 1，**最小 2**；同时把 `clone_group` **回写父号**
4. `disabled = false` / `disabled_reason = None`（父号被禁不传染）
5. 恢复/去重判据用 `(clone_group, clone_seq)`，**绝不用 machineId**（入池撞车会轮换 → 同组 machineId 永不相同）

若走既有 `POST /credentials` + `copies` 手工路径，注意 `service.rs:1176` 的 `req.copies.is_some()`
让**第 1 份也绕过去重** —— 这是刻意的（给已有号加分身），但意味着 `copies=1` 显式给值会造出重复号。

## 3. SOCKS 节点

| 项 | 决定 |
|---|---|
| 存储 | 独立文件 `socks_nodes.json`，放 `token_manager.cache_dir()`（`token_manager.rs:3787`），与 `trash.json` 同级 |
| at-rest 加密 | 复用 `secret_store::{key_path_for, encode_for_disk, maybe_decrypt_to_string}`，读写照抄 `persist_trash`（`token_manager.rs:3852-3887`） |
| 解密失败 | **fail-soft**：`warn!` + 空表。绝不 bail —— 密钥是机器绑定的，换机/重建 VPS 时 credentials 那条路径是 `exit(1)`，节点表不该跟着让服务起不来 |
| `enabled` 默认 | `#[serde(default = "default_true")]`。裸 `#[serde(default)]` 对 bool 是 `false` → 回滚再升级后全节点变禁用、池空 → 生成分身全落直连 |
| 上限 | `MAX_SOCKS_NODES = 64` |
| 内存态 | `parking_lot::Mutex<Vec<SocksNode>>` on `AdminService`，不进热路径，不需要三层热重载镜像 |

结构体照 `recon-socks-nodes.md` §3（`src/kiro/model/socks_node.rs` 新文件 + `model/mod.rs` 加 `pub mod socks_node;`）。

**端点**（挂 `src/admin/router.rs:142` 的 `/proxy/test` 旁，同一 `admin_auth_middleware` 层内）：

| 方法 路径 | 体（camelCase） | 响应 |
|---|---|---|
| `GET /socks/nodes` | — | `{total, nodes:[…]}`，`password` **恒 null** + 另给 `hasPassword: bool` |
| `POST /socks/nodes` | `SocksNodeUpsertRequest`（`id: None` = 新建，`Some` 不存在 → 404 不静默新建） | `{id, message}` |
| `DELETE /socks/nodes/{id}` | — | `{deleted}` |
| `POST /socks/nodes/{id}/test` | — | `ProxyTestResponse` + 写回 `last_test` |

**「GET 抹密码 → 前端编辑清掉密码」那个坑**，两侧同时钉死：
- 后端：`password: Option<String>` + `#[serde(default)]`。**省略该键 = 不改；`Some("") = 清空`**。
  绝不能写成必填 `String`（那样改个节点名就把密码抹了，已绑该节点的分身全部掉线）。
- 前端：表单持 `passwordTouched: boolean`，未触碰密码框时**请求体里不带 `password` 键**（不是带 `undefined` —— axios 会丢掉 undefined，恰好正确，但要写注释说明是刻意依赖这个行为）。
- 承重测试：`omitted_password_keeps_existing` / `empty_password_clears` 两条。

**SSRF**：不能用 `validate_outbound_url`（`ssrf.rs:292`，scheme 白名单只有 https/http）。
新增 `pub async fn validate_proxy_address(url) -> Result<(),String>` 于 `src/common/ssrf.rs`（同模块内可直接用私有的
`parse_host_port`（`:228`）/ `is_forbidden_ip_with`（`:190`）/ `describe_rejection`（`:207`））：
scheme 白名单 `socks5|socks5h|http|https`，IP 层用 **`SsrfPolicy::AdminConfigured`**（`:36`）——
不能用 `Strict`，它会拒掉 `198.18.0.0/15`，而国内 Clash/Surge fake-IP 池正在该段（已知问题 #19）。
DNS 失败 fail-open（与 `validate_outbound_url` 同口径；IP 字面量这个主攻击面已在上面拦住）。
校验点：`POST /socks/nodes` 写入时 + `POST /proxy/test`（后者现在**完全没校验代理侧**，
`handlers.rs:234-255` 直接把 `clean_url` 塞进 `ProxyConfig` → 拿到 adminKey 即可用 `latencyMs`/`error` 做内网端口扫描）。
测试四条：`169.254.169.254` → Err；`198.18.0.1` → Ok；`https://1.1.1.1:443` → Ok；`ftp://x:21` → Err。

**测速**：零新增探测逻辑。前端调既有 `POST /proxy/test`（`handlers.rs:225`，恒 200 靠 `ok` 判成败），
组件 `admin-ui/src/components/proxy-test-button.tsx` **零改动直接用**。
后端 N4 要把 `handlers.rs:225-306` 的探测体抽成 `pub(crate) async fn run_proxy_probe(...)` 供两处共用 ——
复制那 80 行会让 `PROXY_TEST_PROBE_URL`（`:218`）分叉。

**分身换节点**：调既有 `setCredentialProxy`（`admin-ui/src/api/credentials.ts:175`）。
它发 **snake_case**（`proxy_url`/`proxy_username`/`proxy_password`，`:182`），因为后端 `SetProxyRequest`
（`handlers.rs:324`）无 `rename_all` —— 全仓少数例外。**不要"顺手统一"成 camelCase**，也不要自己写 `api.post`：
发 camelCase 会静默变成"清除代理"（空 `proxy_url` = 回退全局）。

## 4. settings-page 挂载（4 处，anchor 已核实）

1. `settings-page.tsx:125` 联合类型尾部加 `| 'clones'`
2. `:136` 之前插 `{ id: 'clones', labelKey: 'settingspage.section.clones', icon: Copy },`（**不是 JSX**）；`:5-28` lucide 导入块补 `Copy`
3. `:159` 一带追加 `{ section: 'clones', titleKey: 'settingspage.card.clones', kwKey: 'settingspage.card.clones.kw' },`
4. `:1837`（回收站 `</SectionGate>`）之后插

```tsx
      <SectionGate section="clones" titleKey="settingspage.card.clones" kwKey="settingspage.card.clones.kw">
        <CloneManagementCard />
      </SectionGate>
```

`CloneManagementCard` 写成顶层独立组件（同 `TrashCard`），自带 `<Card>` 与自己的 react-query，
**不进 `FormState`/`diff`**（底部保存栏 `:2447` 只管 form diff；分身操作是即时生效的动作型 UI）。

## 5. i18n（51 键 × 3 语，扁平点分键）

三语**可直接粘贴的 JSON 已生成**在 `docs/clone-page-i18n.json`（`{zh|en|ja}` 三段，键序一致、
各 51 键，已用脚本断言三份键集完全相同）。落地做法：把对应段的 `"key": "value",` 行原样插进
`admin-ui/src/i18n/resources/{zh,en,ja}.json` 各自 `"settingspage.card.trash"`（三份都在 `:1103`）
一带，保持字母序。三份现各 **1523** 键，改完必须仍等量（这是本仓唯一的 i18n 完整性闸门）。

键分组：`card.clones{,.kw}` / `section.clones` / `clones.{desc,summary,primary,cloneBadge,cloneCount,orphan,ungrouped,
viewBalance,balanceShared,balanceAsOf,copies,bindNode,direct,generate,generating,generateHint,selectAll,
deleteSelected,deleteAll,empty,loadFailed}` / `clones.confirm.{generate,delete}{Title,Desc}` /
`clones.nodes.{title,desc,namePlaceholder,urlPlaceholder,userPlaceholder,passPlaceholder,add,save,bound,
unbound,disabled,delete,confirmDeleteTitle,confirmDeleteDesc,empty,capped}` /
`clones.toast.{generated,generatedPartial,deleted,deletedPartial,nodeSaved,nodeSaveFailed,nodeDeleted}`

（全部以 `settingspage.` 为前缀；`card.clones.kw` 是搜索同义词，三语各写一套。）

⚠️ `ConfirmDialog` 的「取消/处理中…」是硬编码中文（`confirm-dialog.tsx:52,59`），本页不修但英日用户会看到，
单独一个 patch 处理。`confirm.*Desc` 里的强调用 `<strong>` 包（`description` 是 `ReactNode`），不要指望 markdown。

## 6. 实施顺序（每步独立可回滚）

| 步 | 内容 | 依赖 | 回滚代价 |
|---|---|---|---|
| S1 | `account_key()` + `clone_group`/`clone_seq` 两字段（跳 1） | — | 纯新增，零行为变化 |
| S2 | `account_key_of`/`account_siblings`/`account_clone_ids` + 跳 2 | S1 | 快照多两个字段，无消费方 |
| S3 | 跳 3 + 跳 4（`CredentialStatusItem` + service 搬运） | S2 | 响应多两个可选字段 |
| S4 | 跳 5（`types/api.ts`） | S3 | 前端类型，无运行影响 |
| S5 | socks 节点：模型 + 持久化 + `validate_proxy_address` + 4 端点 + `run_proxy_probe` 抽取 | — | 与 S1-S4 完全解耦，可并行 |
| S6 | `POST /credentials/{id}/clone` + 组视图（可选，也可让前端纯前端分组） | S2, S5 | 退回手工 `copies` 多开 |
| S7 | `clone-management-card.tsx` + settings-page 4 处挂载 + i18n 三语 | S4, S5, S6 | 删一个分区 |
| S8 | 余额按账号扇出（`store_balance_fanout` + `refresh_all_balances_gently`（`service.rs:901`）循环按 `account_key` 去重 + `get_cached_balances`（`:828`）乐观修正按组聚合） | S2 | 退回按 id 各查各的 |

**真依赖只有 S7→S4/S5**。S8 单独列出是因为它是"同账号余额一致"的唯一修法，
且**三处都得改**（只改写侧，`refresh_all_balances_gently` 仍打 N 次上游、乐观修正仍按 id 发散 → 用户看到的 bug 不消失）。

## 7. ⚠️ 单号无分身时，哪些功能不可见 / 无收益

用户当前只有 1 个号且不打算补号。按上面实施后，实际可见性：

| 功能 | 单号时 | 何时才有收益 |
|---|---|---|
| SOCKS 节点区（增删/测速/绑定） | ✅ **完全可用**。可给唯一那个号绑代理、测速 | 立即 |
| 主凭据卡 + 查看余额 | ✅ 可见（组内只有它自己，显示为"未分身账号"） | 立即 |
| 「一键生成分身」按钮 | ✅ 可见可点 —— 这是唯一从 1 → N 的入口 | 立即 |
| 分身列表 / 全选 / 删除选中 | ❌ 空态（`cloneSeq` 全 undefined） | 生成分身后 |
| 「一键删除本组全部分身」 | ❌ 禁用（`clone_ids` 为空） | 同上 |
| 分组折叠 / `summary` 计数 | 显示「1 组 · 1 个凭据」，视觉上是退化的单卡 | ≥2 个账号或有分身后 |
| 余额扇出（S8） | ❌ **零收益**。1 个号无兄弟，`account_siblings` 返回 `vec![self]`，行为与改动前逐位相同 | 有分身后（消除面板 5 个不同百分比 + 上游探测 N→1） |
| `account_key` 归一 | ❌ 零收益（`cred:{id}` 分支） | 同上 |

**结论：S1-S4 与 S8 在单号下是纯基础设施投入，用户看不到任何变化**；
真正立刻可用的是 **S5（socks 节点表）** 和 **S7 里的生成入口**。
若要控制本轮范围，最小可交付 = **S5 + S7（页面只做节点区 + 主凭据卡 + 生成按钮）**，
把 S1-S4/S6/S8 留到确实生成了分身之后 —— 此时页面用 `apiKeyHash` 分组即可（前端降级路径本来就写了这条）。
反过来说：**若本轮就要"一键生成分身"落地，S1/S2/S6 是硬前置**（没有 `clone_seq`，
「一键删除分身」的判据不存在，只能靠"组内除最小 id 外全删"—— 那会在父号被删后删光池子）。
