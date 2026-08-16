# cooldownReason 中文耦合改造设计

> 状态：**已实现（2026-08-15 W11）**。原为设计稿（研究产出，未实施），W11 按本文档实现并补 2 处遗漏。
> 对应问题：`.opencode/ISSUES.md` (c) 前端 MAJOR「cooldownReason 用后端中文文案做判定」
> 日期：2026-08-15

## 1. 现状盘点

### 1.1 后端生产端（2 个响应出口，同源）

中文文案唯一来源是 `CooldownReason::description()`（`src/kiro/cooldown.rs:154-168`），
9 个变体全部输出中文。下发链路：

| 出口 | 响应结构 | 字段 | 代码位置 |
|---|---|---|---|
| admin API credentials 列表 | `CredentialsStatusResponse.credentials[]` | `cooldown_reason: Option<String>` | `src/admin/types.rs:146`；赋值 `src/admin/service.rs:849`（`cd.reason.description().to_string()`） |
| admin API 限流 insights | `RateLimitInsight.cooldown`（`CooldownDetail`） | `reason: String` | `src/admin/service.rs:301-308`；赋值 `:953-954`（同 `description()`） |

另外 `RateLimitInsight.insight_text`（`build_insight_text`，`src/admin/service.rs:358-...`、
中文拼接）与 ops 页 `CooldownDetail` 同为中文下发，但**前端只做展示、无字符串判定**。

### 1.2 前端消费端（3 处判定 + 2 处纯展示）

| 位置 | 判定/展示 | 消费字段 | 效果 |
|---|---|---|---|
| `admin-ui/src/components/credential-card.tsx:228` | **判定** `=== '速率限制'` | 列表 `cooldownReason` | 琥珀/红边框（`:766`）、冷却 pill 配色（`:836-838`） |
| `admin-ui/src/components/credential-row.tsx:255` | **判定** `=== '速率限制'` | 列表 `cooldownReason` | 状态点琥珀/红（`:129`）、DetailItem tone warn/bad（`:750`） |
| `admin-ui/src/components/overview-page.tsx:84` | **判定** `includes('可疑')` | **insights** `cooldown.reason`（`CooldownDetail`，**非**列表字段） | 风险等级 labelKey `suspicious` vs `cooldown`（`:87`） |
| `admin-ui/src/components/credential-card.tsx:844` | 展示 | `cooldownReason` | 冷却 pill 文案拼接 ` · ${reason}` |
| `admin-ui/src/components/credential-row.tsx:746-749` | 展示 | `cooldownReason` | DetailItem value `${reason} · Ns` |

前端类型定义：`admin-ui/src/types/api.ts:90`（`cooldownReason?: string`）、
`:1096-1104`（`CooldownDetail`）、`:1106-1125`（`RateLimitInsight`）。

> 注意：`overview-page.tsx:84` 消费的是 **insights 端点**的 `CooldownDetail.reason`，
> 与另两处消费的 credentials 列表字段是两条下发路径——改造必须覆盖两个出口，
> 漏掉任何一个都会留下语言耦合残留。

### 1.3 语言耦合危害

1. 后端改 `description()` 中文文案（如「速率限制」→「限速」）→ 前端 3 处判定全部静默失效：
   `'速率限制'` 判定变成恒 false（credential-card/row 全部走红色分支，overview 走 generic 分支），
   **无任何编译/运行期报错**。
2. en/ja 界面直接显示后端中文（卡片 pill、DetailItem、risk tooltip 均未走 i18n）。

## 2. 改造方案：后端下发枚举 code + 前端按枚举判定 + 文案走 i18n

### 2.1 枚举集合（必须覆盖 description() 全部分支）

`CooldownReason` 共 **9 个变体**（`src/kiro/cooldown.rs:51-98`），建议 code 采用 snake_case 稳定字符串：

| 变体 | 当前中文文案 | 建议 code |
|---|---|---|
| `RateLimitExceeded` | 速率限制 | `rate_limited` |
| `SuspiciousActivity` | 可疑活动风控 | `suspicious` |
| `AccountSuspended` | 账户暂停 | `account_suspended` |
| `QuotaExhausted` | 配额耗尽 | `quota_exhausted` |
| `TokenRefreshFailed` | Token 刷新失败 | `token_refresh_failed` |
| `AuthenticationFailed` | 认证失败 | `authentication_failed` |
| `AuthTransient` | 认证瞬态失败 | `auth_transient` |
| `ServerError` | 服务器错误 | `server_error` |
| `ModelUnavailable` | 模型暂时不可用 | `model_unavailable` |

前端目前只判定 `rate_limited` 和 `suspicious` 两个，但**下发必须全量**——否则新增冷却类型
又要等后端改完才敢用；且 code 是稳定 API 面，新增枚举不应破坏旧前端。

### 2.2 后端改动（2 处响应结构 + 1 个函数）

1. `src/kiro/cooldown.rs`：`impl CooldownReason` 新增
   `pub fn code(&self) -> &'static str`（与 `description()` 并列，返回上表 code）。
   注意：**保留 `description()` 原样**（`cooldownReason` 字段继续下发中文，兼容旧前端；
   description 也是现有测试的断言目标，`cooldown.rs:912-913`）。
2. `src/admin/types.rs:146` 旁新增 `cooldown_code: Option<String>`
   （`#[serde(skip_serializing_if = "Option::is_none")]`，camelCase 输出 `cooldownCode`）。
3. `src/admin/service.rs:849` 旁：`cooldown_code: cd.map(|c| c.reason.code().to_string())`。
4. `CooldownDetail`（`src/admin/service.rs:301-308`）新增 `pub code: String`；
   `:953-954` 赋值 `code: c.reason.code().to_string()`。

API 契约：响应结构**加字段不删字段**。`cooldownReason` 中文保留（展示兼容旧前端），
`cooldownCode` 为新增稳定判定面。旧前端（缓存或未升级）忽略未知字段，行为不变。

### 2.3 前端改动清单

| 文件:行 | 改法 |
|---|---|
| `admin-ui/src/types/api.ts:90` | `Credential` 加 `cooldownCode?: string`（注释：稳定枚举判定面，`cooldownReason` 仅供展示 fallback） |
| `admin-ui/src/types/api.ts:1096-1104` | `CooldownDetail` 加 `code: string` |
| `admin-ui/src/components/credential-card.tsx:228` | `const cooldownIsRateLimit = credential.cooldownCode === 'rate_limited'`（**保留** `cooldownReason` 展示） |
| `admin-ui/src/components/credential-row.tsx:255` | `const rateLimited = credential.cooldownCode === 'rate_limited'` |
| `admin-ui/src/components/overview-page.tsx:84` | `const isSuspicious = it.cooldown.code === 'suspicious'` |
| 展示文案（`:844` / `:746-749`） | 文案改走 i18n key（见下） |

**展示文案 i18n**（en/ja/zh 三语，`admin-ui/src/i18n/resources/*.json`，扁平 key + 单括号插值约定）：

```
credentialcard.cooldown.reason.rate_limited        = Rate limit / 速率限制 / レート制限
credentialcard.cooldown.reason.suspicious          = Suspicious activity / 可疑活动风控 / 不審なアクティビティ
credentialcard.cooldown.reason.account_suspended   = Account suspended / 账户暂停 / アカウント停止
credentialcard.cooldown.reason.quota_exhausted     = Quota exhausted / 配额耗尽 / クォータ枯渇
credentialcard.cooldown.reason.token_refresh_failed= Token refresh failed / Token 刷新失败 / トークン更新失敗
credentialcard.cooldown.reason.authentication_failed= Authentication failed / 认证失败 / 認証失敗
credentialcard.cooldown.reason.auth_transient      = Transient auth failure / 认证瞬态失败 / 一時的な認証失敗
credentialcard.cooldown.reason.server_error        = Server error / 服务器错误 / サーバーエラー
credentialcard.cooldown.reason.model_unavailable   = Model unavailable / 模型暂时不可用 / モデル利用不可
```

建议抽一个纯函数 helper（如 `admin-ui/src/lib/cooldown.ts`）：

```ts
// 未知 code / 老后端（cooldownCode 缺失）→ fallback 用后端原文字符串
export function cooldownReasonLabel(code: string | undefined, reason: string | undefined): string
export function isRateLimitCooldown(code: string | undefined): boolean  // === 'rate_limited'
export function isSuspiciousCooldown(code: string | undefined): boolean // === 'suspicious'
```

**兼容策略（前端先行部署场景）**：前端判定改用 `cooldownCode`，字段缺失（旧后端）时
`isRateLimitCooldown` 返回 false → 走红色 generic 分支，**无害降级**（只影响颜色，
不影响功能）；展示 label 缺 key 时 fallback 到后端中文原串。发布顺序仍建议后端先发、
前端后发（同仓库同发也无风险，字段兼容双向）。

## 3. 风险

| 风险 | 评估 |
|---|---|
| **其他消费端** | rg 全仓确认：除上述 3 处判定 + 2 处展示外，trace/usage 路径**无** cooldown_reason 消费。`ops-page.tsx:566-567` 只用 `cooldown.remainingMs`（无判定）；`insightText`（`:742` 展示）是中文拼接但无判定，本期不动（同属 en/ja 显示中文现象，登记后续）。**最大遗漏风险是 `overview-page.tsx:84`**（消费 insights 端点而非列表字段）——本方案已覆盖 |
| 旧前端 / 缓存旧响应 | 响应加字段不删字段；旧前端忽略 `cooldownCode` 继续用中文判定（保持现状行为）。前端缓存窗口为 dashboard 轮询周期（秒级），短暂不一致无影响。前端判定缺失 code 时降级 generic 分支，无害 |
| 后端文案仍可能被改 | `description()` 保留中文，未来仍可调整（仅影响展示），但**判定已与文案脱钩**，改文案不再静默破坏判定 |
| 枚举集合遗漏 | 9 变体与 `description()` match 分支逐一对齐；`QuotaExhausted` 当前无触发路径（注释自认），也枚举化保留。review 时以 `description()` 的 match 分支为核对基准 |
| 测试断言耦合 | `cooldown.rs:912-913` 断言中文文案——保留 `description()` 即不受影响；新增 `code()` 单测需独立断言 |

## 4. 工作量

| 部分 | 工作量 | 内容 |
|---|---|---|
| 后端 | ~1 小时 | `code()` 函数 + 2 处响应结构加字段 + 单测（9 分支 code 映射、code 与 description 一一对应、CredentialsStatusResponse/insights 响应含 cooldownCode） |
| 前端 | ~半天 | 3 处判定改枚举 + helper 抽函数 + 展示 i18n（9 key × 3 语言）+ 类型定义 |

**测试策略**：

- 后端：`cargo test`（服务器 Docker 验证循环，`--no-default-features`）——`code()` 全分支断言；
  已有 `description()` 中文断言保留不动。
- 前端：`admin-ui` **目前无单测框架**（package.json 无 test 脚本，仅有 tsc/lint 检查）。
  判定逻辑已抽纯函数，可在 `lib/cooldown.ts` 上补轻量 vitest 单测（4 个 case：
  rate_limited→true、suspicious→false、其他 code→false、undefined→false；label 三语 + fallback）。
  若不想引入框架，则靠 tsc + lint 保证类型收口（code 为字符串字面量联合类型可进一步
  由编译器兜底），判定回归靠手工验证 dashboard 三种冷却态配色。
