# 客户端 Key 分发 —— 一期实现设计（P3-35）

> 状态：设计稿（2026-08-16）。立项依据 `.opencode/ISSUES.md` (d)「客户端 Key 分发」：
> 一期轻量分享 4-5 天；二期商业闭环 6-8 天。研究基础：`docs/ref-ZyphrZero-kiro.rs.md`
> （client_keys + 系统 Key 收编）+ `docs/ref-kiro2cc-proxy.md`（子 API Key 商业闭环）。
> 本设计已把两个参考仓的已知弱点（明文存储、普通 `==` 比较、裸 `fs::write`）全部规避。

## 0. 一句话结论

**值得做**。一期（分享场景）工作量 ~4.5 人日，鉴权路径零破坏（主 key 直比兜底保留），
哈希存储 + 常量时间比较 + 收编零迁移三条已研究结论全部落地，二期接口兼容点已预留。

## 1. 背景与目标

网关现状：`/v1`、`/cc/v1` 只有**一个**主 key（`config.apiKey`）和**一个** admin key
（`config.adminApiKey`）。要"把网关分享给朋友/团队用"，只能共享主 key——无法单独
禁用某个人、无法区分谁的用量。

一期目标：**轻量分享**——管理员创建多个 `sk-` 客户端 key 分发给用户，可独立启停、
删除，每把 key 的用量可归因展示。不涉及计费、不涉及绑定账号子集、不涉及用户自助面。

## 2. 一期范围

| 项 | 内容 |
|---|---|
| ClientKeyManager | `sk-` key 生成 / 启停 / 删除 / 哈希存储 / 原子持久化（新模块 `src/client_keys/`） |
| 鉴权优先匹配 | `/v1`、`/cc/v1` 中间件先查客户端 key 表，未命中回落主 key 直比（现有逻辑不动） |
| 主 key 收编 | 启动时把 `config.apiKey` 同步为 id=0 系统 key（不可删、可轮换），零迁移 |
| usage 归因 | `RequestRecord.client_key_id` + `UsageStats` 新维度 `by_client_key` + admin 查询 |
| admin API | CRUD + 启停 + 列表（含使用统计），挂在现有 `/api/admin` 鉴权树内 |
| 前端 | Key 管理页：创建（明文只回显一次）/ 复制 / 启停 / 删除 / 用量统计 |

不做（二期）：`bound_credential_ids` 账号绑定、`spendingLimit` 限额、`durationDays`
有效期懒激活、user-ui 自助面、key 重命名/轮换（手动轮换 = 删除重建）。

## 3. 详细设计

### 3.1 数据存储与结构

**存储位置**：新文件 `client_keys.json`，与 `credentials.json` **同目录**（走
`resolve_default_data_path` 统一解析，Windows 数据隔离语义一致）。

决策理由：

- 不进 `config.json`：客户端 key 是**运行态数据**（高频变更：创建/启停/删除/统计），
  `config.json` 走 `update_config` 全量 load→改→save→reload 语义，为它开写路径污染配置面。
- 不进 `credentials.json`：该文件有 at-rest 加密与多凭据格式兼容逻辑，动它破坏面大；
  且客户端 key 存**哈希**（见下），没有明文需要保护，加密无增益。
- 独立文件 + `fs_atomic`（`src/common/fs_atomic.rs`，temp → fsync → rename，创建即 0600）
  原子写——规避 zyphr 的裸 `fs::write`（崩溃丢文件/权限 0644 泄露）。

**数据结构**（`src/client_keys/manager.rs`）：

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientKey {
    pub id: u64,
    /// 展示掩码：`sk-` + SHA-256 前 8 位。**不是明文，也不是完整哈希**。
    pub key_prefix: String,
    /// 存储值：SHA-256(key 明文) 的 hex。**不存明文**（zyphr/k2cc 明文存储 = 已知弱点）。
    pub key_hash: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub disabled: bool,
    /// 创建时间（Unix 毫秒，与 RequestRecord.ts_ms 同口径）
    pub created_at: i64,
    /// 最后使用时间（Unix 毫秒；None = 从未使用）。内存更新 + 后台定期落盘。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<i64>,
    /// 系统 key（由 config.apiKey 收编，id=0，不可删除）。老数据无此字段默认 false。
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_system: bool,
    // ════ 二期预留（结构在场、serde default 向后兼容；一期语义不实现）════
    /// 绑定的账号子集（k2cc 路线：比 zyphr 分组更适合我们的分身机制）。一期恒 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound_credential_ids: Option<Vec<u64>>,
    /// 额度上限（单位见 limit_unit）。一期恒 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spending_limit: Option<f64>,
    /// 额度计量单位（"usd" | "credits"）。一期恒 None（无 spending_limit）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_unit: Option<String>,
    /// 有效期天数（懒激活：首次使用后开始计时）。一期恒 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_days: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activated_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
}

pub struct ClientKeyManager {
    inner: RwLock<Inner>,   // parking_lot
    path: Option<PathBuf>,  // None = 纯内存（测试）
}

struct Inner {
    /// id → 条目（鉴权扫描源；N 极小，全表扫描可接受）
    entries: HashMap<u64, ClientKey>,
    next_id: u64,           // 只增不减，id 永不复用（对齐 socks_next_id 教训）
}
```

要点：

- **为什么哈希可查**：key 明文 122 bit 熵（uuid v4），SHA-256 无盐即可（无彩虹表攻击面，
  不需要 HMAC/pepper——加了是过度设计）。存储面零明文，文件泄露/备份泄露无害。
- **为什么不用 `by_key` 精确索引**：zyphr 用 `by_key: HashMap<String, u64>` 判重。我们存哈希
  后判重 = 查 `key_hash`，可以加一个 `by_hash: HashMap<String, u64>` 加速**创建判重**；
  但**鉴权路径故意不用它**——见 3.3。
- **统计不落此文件**：用量统计唯一真相在 `UsageStats`（JSONL 可重放恢复），
  ClientKeyManager 只维护 `last_used_at` 元数据，避免双写漂移（见 3.4）。

**加载失败语义**：`client_keys.json` 不存在 → 空表正常启动；存在但解析失败 → `warn` +
空表启动（客户端 key 全部失效 = fail-closed 安全方向），**主 key 直比兜底**保证管理员
仍可进系统（见 3.3），修复文件后重启恢复。

### 3.2 生成格式

- 明文：`sk-` + `uuid::Uuid::new_v4().simple()`（32 hex，122 bit 熵；`uuid` 已是现有依赖，
  不引新 crate）。与上游 `ksk_` 凭据、主 `apiKey` 前缀区分，肉眼可辨来源。
- 落盘：`key_hash = sha256_hex(明文)`；`key_prefix = "sk-" + sha256_hex(明文)[..8]`
  （掩码从哈希截取而非明文，杜绝"明文前 8 位可猜"——虽然 32 hex 随机也猜不出，
  但统一走哈希面更干净，且列表接口永不接触明文）。
- 创建响应回显明文**一次**（`CreateClientKeyResponse { key, ... }`），此后任何接口
  只给 `key_prefix`。前端复制即走，之后明文在服务端不可恢复（只有哈希）。
- 明文只在创建函数内部存在一次（`create()` 返回前持有），不存字段、不进日志、
  不进 audit 记录。

### 3.3 鉴权流程（核心改动）

现状（`src/anthropic/middleware.rs:50-73`）：

```rust
match auth::extract_api_key(&request) {
    Some(key) if auth::constant_time_eq(&key, &state.api_key) => next.run(request).await,
    _ => 401 authentication_error,
}
```

改为（**加一个分支，主 key 分支原样保留**）：

```rust
// 1. 客户端 key 表优先匹配（全表常量时间扫描）
if let Some(client) = client_keys::authenticate(&key).await {   // Option<(u64 /*id*/, bool /*disabled*/)>
    if client.disabled {
        return 401;                       // 与未命中同响应体，防枚举
    }
    // 2. 命中：把 client_key_id 注入 request extensions，handler 埋点归因用
    request.extensions_mut().insert(AuthContext { client_key_id: Some(id) });
    return next.run(request).await;
}
// 3. 主 key 直比兜底（现有逻辑逐字保留 —— 主 key 行为零变化）
if auth::constant_time_eq(&key, &state.api_key) {
    return next.run(request).await;
}
401
```

关键决策：

- **表优先 + 主 key 兜底，双保险**：主 key 收编后也以 id=0 在表里（会先命中表），
  但**兜底分支必须保留**——`client_keys.json` 加载失败/损坏时（见 3.1），表为空而主 key
  照常可用，管理员永远有逃生通道。这符合本仓"绝不把自己锁死"的运维纪律
  （对照 credentials fail-safe 拒绝启动的哲学：客户端 key 可以全部失效，主入口不能挂）。
- **鉴权扫描不用 `by_hash` 精确索引，全表 `constant_time_eq`**：避免哈希存在性侧信道
  （虽然哈希不可逆，泄露"这个哈希在表里"无实义，但 N < 100 全表扫描是微秒级，
  与 zyphr 每请求一次 `fs::write` 相比开销可忽略，安全面做到最干净）。
- 比较对象是**存储哈希 vs 请求 key 的哈希**（都是 64 hex 定长），走现有
  `auth::constant_time_eq`（先各自 SHA-256 再 ct_eq，定长输入、无长度侧信道）。
- `disabled` 在命中后检查：与未命中返回**同一个** 401 响应体（`authentication_error`），
  攻击者无法区分"key 不存在"与"key 被禁用"。
- admin key 鉴权（`src/admin/middleware.rs`）**完全不动**：admin 树与 /v1 树是
  两个独立中间件，客户端 key 只加在 anthropic 侧。
- `AuthContext` 放 `src/anthropic/middleware.rs`（或 `src/common/auth.rs`），
  handler 侧从 `request.extensions()` 读取（与 client_ip 提取同一处埋点）。

### 3.4 usage 归因

**埋点**：`RequestRecord`（`src/usage/record.rs:63`）加字段：

```rust
/// 认证所用的客户端 key（表命中时；主 key 直比或无 key = None）。
/// serde default，兼容历史 JSONL（缺字段视为 None）。
#[serde(default)]
pub client_key_id: Option<u64>,
```

写入点：handlers 构造 record 处（与 `client_ip`/`client_device` 同段），从
`request.extensions()` 读 `AuthContext`。**不写** = 主 key 请求，完全向后兼容。
JSONL 序列化自动带出（历史重放缺字段 → None，聚合无影响）。

**聚合（新维度，不是复用 by_client）**：

- `by_client`（`usage_stats.rs` 的 `ClientAgg`）是**实时 RPM 环**（20×30s，10 分钟窗口），
  语义是"IP/机器维度实时速率"，与长期累计的 key 归因是两回事——不复用、不混淆。
- 新增 `Inner.by_client_key: HashMap<u64, Aggregate>`（`src/usage/usage_stats.rs`），
  对齐 `by_credential` 模式：u64 key 天然有界（≤ 创建过的 key 总数），
  不需要 `MODEL_KEY_CAP` 式的收敛；`Inner::apply` 里 `if let Some(kid) = r.client_key_id`
  累加。查询方法 `by_client_key() -> Vec<GroupStat>`（key = id 字符串，按请求数降序），
  与 `by_credential()` 同构，前端可复用 `GroupStat` 展示组件。
- **`last_used_at` 维护**：`UsageStats` 不知道谁在用 key（它只有聚合），
  所以 `last_used_at` 放 ClientKeyManager：鉴权命中时原子更新内存
  （`AtomicU64` / Mutex 内字段），**热路径零 IO**；后台每 5 分钟 tick 落盘一次
  （复用 `main.rs` 现有 5 分钟 cleanup tick 或 AdminService 受管任务槽），
  重启最多丢 5 分钟展示数据（非计费数据，可接受）。
- **统计归并**：admin 列表接口把 `by_client_key` 聚合 join 到 key 元数据上；
  `usage_enabled=false` 时统计全 0（页面标注），key 管理功能不受影响。

### 3.5 admin API

挂 `src/admin/router.rs` 现有 authed 树（admin key 鉴权自动覆盖，零新增暴露面）：

| 方法 | 路径 | 语义 |
|---|---|---|
| GET | `/client-keys` | 列表：id / keyPrefix（掩码）/ name / description / disabled / isSystem / createdAt / lastUsedAt / 用量统计（requests/tokens/credits，来自 by_client_key） |
| POST | `/client-keys` | 创建。body `{ name, description? }`。响应**唯一一次**回显明文 key。校验：name 非空（trim）；description 长度上限 200。isSystem 恒 false |
| POST | `/client-keys/{id}/disabled` | 启停。body `{ disabled: bool }`。id=0 系统 key 可启停（禁用了主 key 即失效——这是收编的自然语义），但不可删 |
| DELETE | `/client-keys/{id}` | 删除。id=0（系统 key）拒绝 400。删除后该 key 立即失效（内存表移除 + 落盘） |

服务层放 `AdminService`（`src/admin/service.rs`）持有 `Arc<ClientKeyManager>`（构造时注入，
与 `token_manager` 并列）。`ClientKeyManager` 自身是一个独立结构，AdminService 只做
HTTP 层编排（校验/join 统计），CRUD 逻辑全在 manager——`src/admin/service.rs` 已 12000+
行，不往里堆。

前端 API 类型（`admin-ui/src/types/api.ts`）：

```ts
export interface ClientKeySummary {
  id: number
  keyPrefix: string        // "sk-xxxxxxxx…" 掩码
  name: string
  description?: string
  disabled: boolean
  isSystem: boolean
  createdAt: number
  lastUsedAt?: number
  stats?: { requests: number; inputTokens: number; outputTokens: number; creditsUsed: number }
}
export interface CreateClientKeyResponse extends ClientKeySummary {
  key: string              // 明文，仅创建响应存在
}
```

### 3.6 前端（Key 管理页）

跟随现有模式（`admin-ui/src/components/`，懒加载 + app-shell 导航 + React Query +
三语 i18n en/zh/ja）：

- 新文件 `admin-ui/src/components/client-keys-page.tsx`：表格（掩码 / 名称 / 备注 /
  创建时间 / 最后使用 / 请求数 / tokens / 状态），行操作：启用/禁用、删除（确认弹窗）；
  "创建 Key"按钮 → 对话框（name + description）→ 提交后**弹窗只回显一次明文**
  （复制按钮 + 提示"关闭后不可再查看"）；id=0 行标记"系统 Key"且无删除按钮；
  `usage_enabled=false` 时统计列显示 "—"。
- `admin-ui/src/api/client-keys.ts`：axios 实例复用 `/api/admin` baseURL + x-api-key
  拦截器（对齐 `credentials.ts` 模式），四个函数：list / create / setDisabled / remove。
- `admin-ui/src/hooks/use-client-keys.ts`：React Query mutations（invalidate 模式对齐
  `use-credentials.ts`）。
- `app-shell.tsx`：导航注册新菜单项（`appshell.nav.clientKeys`）+ 懒加载。
- i18n：`en.json` / `zh.json` / `ja.json` 各加 ~14 键（页面标题、列头、按钮、创建弹窗、
  复制提示、删除确认、系统 key 徽标、无统计提示）。

## 4. 二期预留（接口兼容点）

一期落盘结构已含二期字段（`bound_credential_ids` / `spending_limit` / `limit_unit` /
`duration_days` / `activated_at` / `expires_at`），全部 serde default + 一期恒 None，
**旧文件升级无迁移、新文件二期字段补位无破坏**。

| 二期能力 | 一期预留的接缝 |
|---|---|
| `bound_credential_ids` 账号子集绑定 | 字段在场；二期在 `select_endpoint`/`select_custom_api` 入口按 `AuthContext.client_key_id` 过滤候选池（我们分身机制适配 k2cc 路线，比 zyphr 分组更适合） |
| `spendingLimit` 限额 | 字段在场；二期在 handlers 埋点处加 `check_quota(client_key_id, record)` 挂钩（限额拒绝返回 402，对齐现有 quota-402 设计） |
| `durationDays` 懒激活 | 字段在场；二期在鉴权命中处实现 `activate()` 语义（首次使用置 `activated_at` + `expires_at`，过期 key 401） |
| user-ui 自助面 | 一期 admin API 的列表/统计查询即数据源；二期独立前端（对齐 k2cc user-ui 5 组件）消费，无需改后端 |
| 轮换（rotate） | 删除重建已覆盖；系统 key 轮换 = 改 `config.apiKey` 后 re-sync（见 S2 接线点） |

约束：二期**只加字段/加端点**，不改一期字段类型与端点签名（`key_hash` 永存、
`POST /client-keys` 响应结构不回退）。

## 5. 安全设计

| 威胁 | 对策 |
|---|---|
| 文件泄露（备份/磁盘/权限） | 只存 SHA-256 哈希 + 掩码；`fs_atomic` 创建即 0600；不存明文 |
| 彩虹表/离线破解 | key 122 bit 熵（uuid v4），无盐 SHA-256 安全；密钥库的明文泄露面为零 |
| 时序侧信道 | 鉴权全表 `constant_time_eq`（定长 64 hex 哈希比对，现有实现再 SHA-256 + ct_eq，长度不泄漏） |
| 枚举探测（哪些 key 有效） | 不存在 / 禁用 / 主 key 错误 → 同一个 401 `authentication_error`；无任何 key 校验端点 |
| key 落日志 | 明文/哈希/掩码一律不进日志（对齐凭据脱敏纪律 `main.rs:376` 收窄打印模式）；审计中间件只记 id 不记 key |
| 响应泄露 | 明文只在创建响应出现一次；列表/审计只回显掩码；GET 接口不返回 `key_hash`（hash 在文件里，API 层不需要） |
| 禁用不生效 | 命中表后即时检查 `disabled`（内存态），落盘即生效 |
| 创建判重 | 同明文二次创建返回既有条目（zyphr 语义）或 409——**设计取 409 拒绝**：一次性的明文回显语义下"返回既有条目"会让创建者拿不到明文，反而更困惑；判重按 `key_hash` 查 |
| 系统 key 误删 | `DELETE /{id}` 对 `is_system` 返回 400；`sync_system_key` 只由主 key 变更触发 |

## 6. 实施步骤（独立 CI 顺序）

每步结束走 skiapi Docker 验证循环（`cargo test --no-default-features`）；
前端步走 `pnpm test` + `pnpm build`。

| 步 | 内容 | 验证策略 |
|---|---|---|
| **S1** ClientKeyManager（0.5d） | `src/client_keys/` 新模块：结构 + 生成 + 哈希 + CRUD + `fs_atomic` 持久化 + load/save + `by_hash` 判重 + 纯内存模式（path=None，测试用） | 单测（tempdir）：生成格式 `sk-`+32hex、落盘哈希非明文、重载 round-trip、判重 409、fs_atomic 0600、损坏文件 fail-closed 空表 |
| **S2** 主 key 收编（0.5d） | `main.rs` 启动时 `sync_system_key(config.apiKey)`（id=0，is_system，不可删）；admin 改 `apiKey`（`update_config`）后 re-sync 接线点（标注 TODO 注释 + 守卫：改 key 时同步调 `sync_system_key`） | 单测：id=0 存在、元数据保留、与旧明文冲突的非系统条目被清除、重复启动幂等 |
| **S3** 鉴权改造（1d） | `auth_middleware` 加表优先分支 + `AuthContext` extensions + handler 埋点读取 | 回归矩阵钉死：①主 key x-api-key ②主 key Bearer ③空 key ④错 key ⑤长 key ⑥客户端 key 放行 + `AuthContext` 注入 ⑦禁用 key 401 与未命中同体 ⑧表空时主 key 照常（兜底分支）⑨主 key 请求 `client_key_id=None` |
| **S4** 归因（1d） | `RequestRecord.client_key_id` + `Inner.by_client_key` 聚合 + `by_client_key()` 查询 + handler 埋点接线 | 单测：聚合正确（同 key 多请求累计/多 key 分桶/None 不入桶）、`GroupStat` 排序、历史 JSONL（无字段）重放不破坏、`last_used_at` 更新 + 5 分钟落盘（可注入时间） |
| **S5** admin API（0.5d） | router 4 端点 + AdminService 编排（校验/join 统计/掩码只读） | 单测：创建回显一次明文、列表无明文、启停即时生效、删 id=0 拒绝、usage_enabled=false 统计为 0 |
| **S6** 前端（1-1.5d） | `client-keys-page.tsx` + api/hooks + app-shell 导航 + 三语 i18n | `pnpm test`（组件/复制一次交互/i18n 键齐全）+ `pnpm build`；手动走查创建→复制→禁用→删除闭环 |

合计 ≈ **4.5-5 人日**（含 CI 往返与 review）。

## 7. 风险

| 风险 | 评估与缓解 |
|---|---|
| 鉴权路径回归影响主 key / admin key | 低-中。主 key 直比分支**逐字保留**，S3 回归矩阵覆盖 9 种组合；admin 树完全不动；上线先灰度观察 /v1 401 率 |
| 热路径开销 | 低。鉴权增加一次 SHA-256 + N<100 次定长比较（微秒级）；`last_used_at` 内存原子更新，落盘移出热路径（5 分钟 tick）——比 zyphr 每请求 `fs::write` 强两个量级 |
| `client_keys.json` 损坏/权限错 | 低。fail-closed 空表 + 主 key 兜底 + 启动 warn；不阻塞启动（对照 credentials 的 fail-safe 拒绝启动：客户端 key 可全失效，主入口不可挂） |
| 前端改动量 | 中（预估 1-1.5d）：新页 + 复制一次交互 + 三语 14 键 + app-shell 注册。交互简单，跟随 settings-page 既有表单/表格模式 |
| 统计双真相漂移 | 已规避：用量唯一真相在 `UsageStats`（JSONL 可重放），manager 只维护 `last_used_at` 展示元数据 |
| 多会话并发工作树 | 新文件纯新增无冲突；`middleware.rs`/`router.rs` 改动前看 `.claude/state/CURRENT.md` 守卫清单，改动后走临时 index 快照提交纪律 |
| 二期范围膨胀回灌一期 | 接口兼容点已钉死（§4 约束）：一期不实现二期字段语义、不改端点签名；二期字段只在结构体定义，实现留到二期 |

## 8. 自 review 结论（必做项核对）

1. **鉴权路径不影响现有主 key / admin key 行为**：✅ 主 key 直比分支逐字保留为兜底，
   新增分支只在前置位置尝试；admin 中间件零改动；S3 回归矩阵将 9 种组合钉死。
2. **哈希存储不引入明文泄漏**：✅ 落盘仅 SHA-256 hex + 掩码；明文只在 `create()`
   内部短暂存在并唯一一次回显；列表/审计/日志均无明文；`fs_atomic` 0600。
   对比 zyphr/k2cc 的明文 `key` 字段——本设计在存储面严格强于两个参考仓。
3. **二期兼容点成立**：✅ 二期 5 个字段以 serde default 常驻结构，旧文件无迁移；
   鉴权/埋点/选号三处接缝（§4）均已标注；约束"只加字段/端点不改签名"守住。
4. **额外自查**：`by_client_key` 与既有 `by_client`（RPM 环）语义不混淆；统计唯一真相
   在 usage 侧；加载失败 fail-closed 但主 key 逃生通道保留；id=0 系统 key 不可删。

## 附：与参考仓差异对照

| 维度 | zyphr client_keys | k2cc api_keys | 本设计（一期） |
|---|---|---|---|
| 存储 | 明文 `key` | 明文 `key` | **SHA-256 哈希 + 掩码** |
| 持久化 | 裸 `fs::write` | 裸 `fs` | **`fs_atomic`（temp→fsync→rename，0600）** |
| 比较 | 常量时间 | 普通 `==`（弱点） | **全表常量时间（定长哈希比对）** |
| 主 key 兼容 | 收编 id=0 + 表内匹配 | 无收编概念 | 收编 id=0 + **主 key 直比兜底双保险** |
| 归因 | key 内嵌计数器 | key 内嵌计数器 | **usage 管道单真相 + `by_client_key` 聚合** |
| 二期 | group 分组 | bound_credential_ids + limit + duration | **字段预留 + 接口兼容点**（k2cc 路线） |
