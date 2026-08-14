## 1 前后端契约比对

| 前端要调 | 后端现状 | 命名风格 | 判定 |
|---|---|---|---|
| `POST /credentials/{id}/proxy` | 存在 `handlers.rs:311` | **snake_case**（`proxy_url`/`proxy_username`/`proxy_password`，无 `rename_all`） | ⚠️ 必须走既有 wrapper `setCredentialProxy`（`admin-ui/src/api/credentials.ts:177-185`，已正确发 snake_case）。新页面若自己 `api.post` 写 camelCase → 静默丢字段、代理绑不上 |
| `POST /proxy/test` | 存在 `handlers.rs:189` | camelCase | ✅ `ProxyTestButton` 零改动可用 |
| `POST /credentials/batch-delete` | 存在 `router.rs:65`，`BatchDeleteRequest` camelCase | camelCase `{ids, force}` | ✅ `credentials.ts:368` 已封装 |
| `GET /credentials/{id}/balance`、`GET /credentials/balances/cached` | `router.rs:91,97`；hooks 已有（`use-credentials.ts:55,67`） | camelCase | ✅ |
| `POST /credentials/{id}/clone` | **可见规格里没有**（后端文本截断于 §1/P1-3） | — | 🔴 未确认。需明确路由行、请求体类型名 + `rename_all="camelCase"` + 前端发什么 |
| `GET/POST/DELETE /socks-nodes` | 裁决表采纳了 `socks_nodes.json`，但**未见路由与类型定义** | — | 🔴 未确认，同上 |
| `CredentialStatusItem.cloneGroup/cloneSeq` | P1-1 只改了 `KiroCredentials` | — | 🔴 **最大缺口**，见 §1.1 |

### 1.1 clone 字段的 4 跳管线只做了第 1 跳

`KiroCredentials`（P1-1 已做）→ `token_manager::CredentialStatus`（结构体 `token_manager.rs:1420` 一带 + 构造处 `:4795-4810`）→ `admin::types::CredentialStatusItem`（`types.rs:24`）→ `service.rs:395-405` 逐字段搬运 → `admin-ui/src/types/api.ts:39` **与** `admin-ui/src/api/ops.ts:78`（两份独立类型都得加）。

可见的后端规格一跳都没覆盖第 2-4 跳。缺任一跳前端拿不到 `cloneGroup`，整页退化为只读 —— 前端规格自己的降级表已预言了这点。**这是必须补进后端规格的 5 处 patch。**

### 1.2 `account_key` 与既有 `api_key_hash` 不是同一个串

`api_key_hash`（`token_manager.rs:4802`）= `sha256_hex(key)` **完整 64 hex**；`account_key` = `acct:` + **前 16 hex**。前端按 `apiKeyHash` 分组的降级路径可行，但**不得**把 `cloneGroup` 与 `apiKeyHash` 做字符串 join。请在规格里写死这句。

另：`credentials.rs` 当前零 `sha2` 引用（已 grep 确认），P1-2 的函数内 `use sha2::{Digest, Sha256}` 是必需的，写法正确。

## 2 用户 6 条诉求核对

| # | 诉求 | 落点 | 判定 |
|---|---|---|---|
| 1 | shield 内置 + 设置开关 | shield 规格 §1 六项配置 + `settings-page.tsx:2102` 同卡片 Switch | ✅ 功能齐；⚠️ **缺统计暴露**。用户原话痛点是「面板完全看不到」，规格里没有吸收次数/命中率计数器与面板展示 → 内置后仍看不到，诉求只完成一半 |
| 2 | 分身要有标签标识 | P1-1 `clone_group`/`clone_seq`；裁决表以「已有 `name`」为由拒绝自由文本 tag | ⚠️ 裁决本身成立，但 clone 创建路径**必须写 `name`**（如 `{父name} #2`）。可见规格未写 → 卡片上分身与本体长得一样，诉求实际未达成 |
| 3 | 分身管理页 | 加 socks ✅(B 区)／看主凭据 ✅／**查看余额** ✅(就地 `useCredentialBalance`)／一键生成分身 ✅(NumberStepper 1..16 对齐 `MAX_CREDENTIAL_COPIES=16`)／**一键删除分身** ✅(`account_clone_ids` + `clone_seq.is_some()` 判据，本体恒不入列)／**socks 沿用现有容器含测速** ✅(`ProxyTestButton` 零改动) | ✅ 六个子项全覆盖 |
| 4 | 推号内置 + 开关 + 可选自动分身 | **三份规格均无** | 🔴 **整条漏掉**，见 §2.1 |
| 5 | 同账号余额同步 | 裁决表 + §5 写侧扇出 | ⚠️ 见 §2.2，温和刷新循环的去重未确认 |
| 6 | 三语齐 | shield 8 键三语齐；**分身页无 i18n 表** | 🔴 见 §3 |

### 2.1 推号（诉求 4）缺口细节

- 后端 `/import/keys` 双路径已存在（`router.rs:67` / `:177`）；**前端今天没有任何调用方** —— `kam-import-dialog.tsx` 走的是 `useAddCredential`（`:155`）即 `POST /credentials`，不是 `/import/keys`。所以「内置」= 新建前端入口。
- 开关默认值有陷阱：外部 kiro-accounting 已在推号。若新增 `importKeysEnabled` 默认 `false`，**线上推号当场断**。默认必须 `true`（`#[serde(default = "default_true")]`）。
- 自动分身开关默认 **false**（用户明确要求），字段建议 `importKeysAutoCloneCopies: u32 = 1`（1 = 不分身），比 bool 更省一个字段。
- ⚠️ `parse_import_keys_request`（`types.rs:1061`）是**手写 `serde_json::Value` 解析器**，兼容 4 种历史格式，不是 derive。新字段要手工加进该函数，并扩 `types.rs:1221+` 的解析测试；同时 `ImportKeysRequest`（`:963`）无 serde 属性（纯内部结构）——别给它加 `rename_all` 误导实现者。

### 2.2 余额扇出的写侧清点（供核对 §5 是否全覆盖）

写侧 4 处：`service.rs:765`（单号 fetch 后写）、`:928`（`refresh_all_balances_gently` 循环内写）、`:1393`（删除清理）、`:1478`（彻底删除清理）。读侧 2 处：`:718`、`:832`。

- `refresh_all_balances_gently`（`:901-945`）按**未禁用 id 全量**迭代 + 每个 sleep `spacing_secs`。若只在写侧扇出而循环不按 `account_key` 去重，**5 个分身仍打 5 次上游探测** —— 而「省掉 N-1 次探测」是裁决表自己认定的主要收益。请确认 §5 含循环去重；未含则补。
- `:1393`/`:1478` 的 `cache.remove(&id)` 在扇出模型下必须只删自己那条，**不能**顺带清同账号兄弟（否则删一个分身把整组余额显示清空）。

## 3 i18n 完整性

- 现状：zh/en/ja **各 1522 键，扁平**（已实测）。三份必须等量。
- 🔴 shield 规格的键前缀 `settings.absorb.*` **不存在于本仓命名空间**：zh.json 里 `settings.` 开头 **0 键**，设置页全部走 `settingspage.*`（362 键，如 `settingspage.network.proxy.label`）。应改为 `settingspage.absorb.*`。8 个键的三语值本身完整。
- 🔴 前端规格只在 ASCII 图里出现 `settingspage.card.clones` 等键名（前缀风格正确），**没有 zh/en/ja 值表**。分身页涉及组标题/主号/分身徽章/份数/生成/删除确认/节点增删/测速结果/空态/错误 toast，粗估 35-50 键 × 3 语言全缺。这是本轮最大的 i18n 缺口，必须在实施前补成表。

## 4 新增配置项汇总

| 字段（Rust） | 前端键 | 类型 | 默认 | `#[serde(default)]` | 前端暴露 |
|---|---|---|---|---|---|
| `upstream_retry_absorb_enabled` | `upstreamRetryAbsorbEnabled` | bool | false | ✅ 裸 default | ✅ Switch |
| `upstream_retry_absorb_budget_secs` | 同名 camel | u64 | 60 | ✅ 命名 fn | ✅ 数字 |
| `upstream_retry_absorb_max_attempts` | 同名 camel | u32 | 4 | ✅ | ✅ |
| `upstream_retry_absorb_min_delay_ms` | 同名 camel | u64 | 150 | ✅ | ✅ |
| `upstream_retry_absorb_max_delay_secs` | 同名 camel | u64 | 15 | ✅ | ✅ |
| `upstream_retry_absorb_suspended` | 同名 camel | bool | false | ✅ | ✅ |
| **待补** `import_keys_enabled` | `importKeysEnabled` | bool | **true** | 需命名 fn | ✅ |
| **待补** `import_keys_auto_clone_copies` | `importKeysAutoCloneCopies` | u32 | **1** | ✅ | ✅ |
| socks 节点 | — | 独立文件 `socks_nodes.json` | — | 不进 config.json | ✅ |

无重复定义、无命名不一致（六项 absorb 前缀统一）。注意 shield 规格漏了三处配套：`src/anthropic/mod.rs` 需 `pub use handlers::set_absorb_policy;`（`set_cc_auto_buffer` 就是这么导出的，`mod.rs:39`），`service.rs:2239` 的 `hot_or_display_changed` 或聚合式需加 `absorb_changed`，以及 `ConfigSnapshotResponse::default()`（`types.rs:1433`）六项 —— 后两者规格提了，`mod.rs` 那行没提。

## 5 合并后实施顺序（每步可独立回滚）

1. **C1 后端数据模型**：P1-1/P1-2/P1-3（credentials 两字段 + `account_key` + token_manager 三方法）。纯新增，无行为变化。
2. **C2 clone 字段管线**：`CredentialStatus` → `CredentialStatusItem` → `service.rs` 搬运 → `types.rs`/`ops.ts` 两份 TS 类型。**必须早于任何前端页面**。
3. **C3 余额按账号扇出**：写侧 4 处 + 循环去重。独立可回滚，无前端依赖（响应结构不变，仍 `HashMap<u64,_>`）。
4. **C4 socks 节点存储 + 路由**（`socks_nodes.json` + at-rest 加密 + 3 个端点）。
5. **C5 clone 端点** `POST /credentials/{id}/clone`（含继承 `api_region`/`region`/`auth_region`/`subscription_title` —— 主线未上线的修复①是它的前置，不继承则分身 403，实测 0% 成功；若①未在本轮先上，C5 必须自带该继承逻辑）。
6. **C6 shield 吸收层**（配置 6 项 + `AbsorbPolicy` + `mod.rs` 导出 + service 热更 + 前端开关 + `settingspage.absorb.*` 三语）。与 C1-C5 完全解耦，可任意位置插入。
7. **C7 分身管理页**（`clone-management-card.tsx` + 三语键表）。依赖 C2/C4/C5。
8. **C8 推号内置**（config 两项 + 手改 `parse_import_keys_request` + 前端入口 + 三语）。依赖 C5（自动分身要调 clone 逻辑）。

隐含依赖只有一条真的：**C7 依赖 C2**，而可见规格里 C2 不存在 → 若按现规格实施，前端 commit 会先落地、页面上线即哑。

## 6 缺「回退即 FAIL」测试的改动

| 改动 | 缺什么测试 |
|---|---|
| `account_key` 冻结语义 | 「OAuth 分身刷新后 `refresh_token` 变化 → 仍同组」。删掉 `clone_group` 短路分支就应 FAIL。这是 P1-2 注释里唯一无法靠 review 保证的行为 |
| `account_key` 对 OAuth 号回退 `cred:{id}` | 「无 `kiro_api_key` 时输出与改动前逐位相同」——保证零行为变化的断言 |
| `account_clone_ids` 排除本体 | 「组内只剩分身、父号已删 → 返回列表不含唯一可用号」。这条直接对应「一键删除分身不会删光池子」，删掉 `clone_seq.is_some()` 过滤即应 FAIL |
| 余额扇出 | 「同 `kiroApiKey` 的 5 条凭据，写入一次后 5 个 id 读到**同一个 `cached_at`**」。当前 5 个不同百分比的线上现象正是它的反例 |
| 温和刷新去重 | 「5 分身 + 1 独立号 → `fetch_balance` 只被调 2 次」。需要可注入的 fetch 计数器；若做不到，明说该项无测试保障 |
| `parse_import_keys_request` 新字段 | 4 种格式 × 自动分身字段缺省 → `copies=1`。默认关是用户硬要求，必须有断言钉住 |
| absorb 默认全关 = 零行为变化 | 「空 config.json 反序列化后 `upstream_retry_absorb_enabled == false` 且 `AbsorbPolicy::from_config` 产出 no-op」 |
| 旧 config.json 兼容 | 一条「不含任何新键的 config.json 能成功反序列化」的测试，同时覆盖 8 个新配置项。这是「服务起不来」那条铁律的唯一自动闸门 |
| clone 继承 region | 「clone 出的凭据 `api_region`/`region`/`auth_region`/`subscription_title` 与父号相等」。对应实测 0% → 83/45/100/88% 的那个缺陷 |

构造不出因而**不该做**的：`clone_seq` 的连续性/唯一性约束（删中间一个分身后序号出现空洞是无害的，加约束只会引入重排逻辑与新失败面）。

**未能确认**（后端与前端规格文本在我这里被截断）：clone 端点与 socks 端点的具体路由/类型/serde 属性、§5 扇出是否含温和刷新循环去重、shield 规格 P4 之后的 `mod.rs` 导出与统计计数。上述三处按「缺口」计入，若规格全文已有请以全文为准。