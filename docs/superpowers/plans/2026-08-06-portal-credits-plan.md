# Kiro 车队 · 积分制上车 — 实现计划

## 元信息

| | |
|---|---|
| 设计文档 | `docs/superpowers/specs/2026-08-06-portal-credits-design.md`（**唯一事实来源**，与本计划冲突时以设计文档为准） |
| 目标分支 | `feat/portal-credits`（已创建，基于 master） |
| 前序提交 | `ee4f3dd` 设计文档定稿 |
| 产品用词 | 界面文案统一用「车队」「上车」；代码标识符仍用 `unlock`/`unlockers`（英文标识符与中文文案分离，避免拼音变量名） |

**设计文档：** `docs/superpowers/specs/2026-08-06-portal-credits-design.md`
**目标分支：** `feat/portal-credits`（已创建，设计文档已提交）

## 要建的东西

Portal 用户看凭据明文要花积分。每把 key 独立结算：上车的人越多，每人分摊越少，多付的实时退回。上限 10 人，满员拒绝。

**术语（界面文案统一用这套）：** 凭据池 = **车队**，解锁查看 = **上车**，已解锁人数 = **车上几人**。代码标识符仍用 `unlock`/`unlockers`（英文标识符不掺中文业务词，避免日后翻译混乱）。

## 前置条件

- 分支 `feat/portal-credits` 已 checkout
- `src/portal/` 现有 9 个文件、934 个测试全绿
- 积分功能默认关闭，关闭时行为必须与现状完全一致

## 任务

### Task 1：配置项

**文件：** `src/model/config.rs`（修改）

在 `portal_require_https`（第 122 行附近）之后加 5 个字段，紧跟现有 portal 字段：

```rust
pub portal_credits_enabled: bool,        // #[serde(default)] 默认 false
pub portal_key_base_count: u32,          // default_portal_key_base_count() = 2
pub portal_key_base_price: i64,          // default_portal_key_base_price() = 10
pub portal_key_total_price: i64,         // default_portal_key_total_price() = 20
pub portal_key_max_unlockers: u32,       // default_portal_key_max_unlockers() = 10
pub portal_key_min_price: i64,           // default_portal_key_min_price() = 1
```

同步更新 `impl Default for Config`（第 899 行附近）。金额一律 `i64`——积分是整数分，用浮点会在退款差额上累积误差。

**验证：**
```bash
cargo build --bin kirostudio 2>&1 | grep -E "^error" | head
```
Expected: 无输出（编译通过）

---

### Task 2：`credits.rs` 纯计算模块

**文件：** `src/portal/credits.rs`（新建）、`src/portal/mod.rs`（修改，加 `pub mod credits;`）

```rust
pub struct Pricing {
    pub base_count: u32,
    pub base_price: i64,
    pub total_price: i64,
    pub max_unlockers: u32,
    pub min_price: i64,
}

impl Pricing {
    pub fn unit_price(&self, n: u32) -> i64 { ... }
    pub fn is_full(&self, n: u32) -> bool { n >= self.max_unlockers }
}
```

公式：`n <= base_count` → `base_price`；否则 `min(base_price, ceil_div(total_price, n))`；最后夹到 `>= min_price`。

**`ceil_div` 必须用整数运算**：`(total + n - 1) / n`。不用 `(total as f64 / n as f64).ceil()`——浮点在 20/3 这类除不尽的情况下有表示误差，而这个值直接决定扣多少钱。

**验证：**
```bash
cargo test --bin kirostudio portal::credits 2>&1 | tail -5
```
Expected: `test result: ok.`，至少 8 个测试

**Dependencies:** Task 1

---

### Task 3：`credits.rs` 穷举测试

**文件：** `src/portal/credits.rs`（修改，加 `mod tests`）

必须覆盖：

1. `base=2,price=10,total=20` → N=1..10 序列恰为 `[10,10,7,5,4,4,3,3,3,2]`
2. `base=4,price=5,total=20` → 恰为 `[5,5,5,5,4,4,3,3,3,2]`
3. **单调不增**：对 8 组参数组合（含病态 `base=4,price=5,total=100`）断言 `unit_price(n+1) <= unit_price(n)`。这条是防「人越多反而越贵」的唯一保障，后段 `min` 钳制就是为它存在的。
4. **差额模型**：模拟 1→10 人陆续上车，每步断言每人 `paid == unit_price(N)`（已用 Python 预演，10 步零异常）
5. 边界：`total < base_price`、`min > total`、`max=1`、参数为 0 不 panic
6. `is_full`：N=max 时 true，N=max-1 时 false

**验证：**
```bash
cargo test --bin kirostudio portal::credits -- --nocapture 2>&1 | grep -c "^test portal::credits"
```
Expected: ≥ 8

**Dependencies:** Task 2

---

### Task 4：四张新表

**文件：** `src/portal/store.rs`（修改 `init_schema`，第 119 行）

在 `portal_audit` 建表语句之后追加 4 张表，DDL 照抄设计文档「数据模型」节：`portal_balances`、`portal_unlocks`、`portal_key_pricing`、`portal_ledger`。

注意三处已在设计里定好、容易写反的地方：

- `portal_balances.user_id` / `portal_unlocks.user_id` **要** `REFERENCES portal_users(id) ON DELETE CASCADE`
- `portal_unlocks.credential_id` **不加**外键（凭据池在 `credentials.json`，不在本库）
- `portal_ledger` **完全不加**外键（用户删了流水要留，否则无法回答「这账号一共花了多少」）

`init_schema` 用的是 `CREATE TABLE IF NOT EXISTS`，所以对已有 `portal.db` 是幂等的、不需要迁移脚本。

**验证：**
```bash
cargo test --bin kirostudio portal::store 2>&1 | tail -3
```
Expected: 现有 store 测试仍全绿（新表不破坏旧行为）

**Dependencies:** Task 1

---

### Task 5：余额与流水的读写

**文件：** `src/portal/store.rs`（修改）

```rust
pub fn balance_of(&self, user_id: i64) -> Result<i64>;
pub fn wallet_of(&self, user_id: i64) -> Result<Wallet>;          // balance/topup/spent
pub fn ledger_of(&self, user_id: i64, limit: usize) -> Result<Vec<LedgerEntry>>;
pub fn topup(&self, user_id: i64, amount: i64, note: Option<&str>, now_ms: i64) -> Result<i64>;
```

`topup` 在一个事务里做三件事：改 `portal_balances`、累加 `topup` 或 `spent`、写一条 `portal_ledger`。返回新余额。

`amount` 允许负数（管理员扣减，`kind='admin_adjust'`），但**扣减后余额不得为负**——夹到 0 并在 note 里注明实际扣了多少。理由：负余额会让后续所有上车判断都要处理负数分支，而这个状态没有任何业务含义。

**验证：**
```bash
cargo test --bin kirostudio portal::store::tests::topup 2>&1 | tail -5
```
Expected: 充值/扣减/负数夹零/流水条数 4 个测试通过

**Dependencies:** Task 4

---

### Task 6：上车事务（本计划最关键的一步）

**文件：** `src/portal/store.rs`（修改）

```rust
pub enum BoardResult {
    Ok { price: i64, balance: i64, count: i64 },
    AlreadyOnboard { balance: i64, count: i64 },
    NotEnough { needed: i64, balance: i64 },
    Full { count: i64, max: i64 },
}

pub fn board(
    &self,
    user_id: i64,
    credential_id: i64,
    cfg: PricingParams,   // 当前配置，仅在该 key 尚无快照时使用
    now_ms: i64,
) -> Result<BoardResult>;
```

严格按设计文档「关键流程」的 9 步实现，全部包在 `BEGIN IMMEDIATE` 里：

1. 已上车 → 返回 `AlreadyOnboard`，**不扣分、不写流水**
2. 读 `portal_key_pricing`；无则用 `cfg` 并 `INSERT` 冻结（本 key 价格从此固定）
3. `count = distinct_unlockers(cred)`
4. `count >= snap.max` → `Full`
5. `price = unit_price(count + 1, snap)`
6. `balance < price` → `NotEnough`
7. 扣费、写 `portal_unlocks(paid=price)`、写 ledger(`kind='unlock'`)
8. 其余 `count` 人：`refund = paid - price`；`>0` 则加余额、`paid = price`、写 ledger(`kind='refund'`)
9. COMMIT

**用 `BEGIN IMMEDIATE` 而非默认的 deferred。** 默认事务先拿读锁、写时再升级，两个并发上车会一个拿到 `SQLITE_BUSY` 而失败；IMMEDIATE 开头就拿写锁，第二个请求排队等待，拿到的是第一个提交后的真实人数。这是「两人同抢导致都按旧人数计价」的唯一防线。

**验证：**
```bash
cargo test --bin kirostudio portal::store::tests::board 2>&1 | tail -8
```
Expected: 幂等/余额不足回滚/满员回滚/退款正确/并发不超员 全部通过

**Dependencies:** Task 3, Task 5

---

### Task 7：并发正确性测试

**文件：** `src/portal/store.rs`（在 `mod tests` 内追加）

三个用例，都用 `Arc<PortalDb>` + `std::thread::spawn` 真并发，不是顺序调用假装并发：

- `board_concurrent_two_users_same_key`：2 线程同抢空 key → 最终 `count=2`、两人各付 `basePrice`、总扣 `2*basePrice`
- `board_concurrent_at_capacity`：已满 `max` 人，2 线程同抢 → 都得 `Full`，**`count` 仍为 `max`**（这条失败说明 TOCTOU 竞态存在，会超卖名额）
- `board_concurrent_first_unlock_snapshot`：2 线程同时首次上车 → `portal_key_pricing` 只有 1 行，两人价格相同

内存库（`open_in_memory`）在多线程下共享同一连接与锁，能真实暴露串行化问题；若某用例需要独立库文件，用 `tempfile` 建临时路径。

**验证：**
```bash
for i in 1 2 3 4 5; do cargo test --bin kirostudio portal::store::tests::board_concurrent 2>&1 | tail -1; done
```
Expected: 5 次全绿（并发 bug 常常偶发，跑一次通过不算通过）

**Dependencies:** Task 6

---

### Task 8：明文按上车状态下发（安全红线）

**文件：** `src/portal/http.rs`（修改 `list_keys`，约 505-700 行）

`CredentialRow` 新增字段：

```rust
onboard: bool,          // 我是否已上车
board_price: i64,       // 当前上车价（未上车时展示用）
board_count: i64,       // 已上车人数
max_boarders: i64,      // 名额上限
full: bool,             // 是否已满（且我不在车上）
```

改动 `key` 字段的填充逻辑：

```rust
let key = if !credits_enabled() {
    plain_lookup(...)              // 积分未启用：与现状完全一致
} else if onboard {
    plain_lookup(...)              // 已上车：给明文
} else {
    None                           // 未上车：明文不进响应体
};
```

**这是本计划的安全红线。** 未上车时 `key` 必须为 `None`，而不是"前端不显示"——后者等于明文已经发到浏览器，F12 或 `curl` 直接拿到，整套积分门槛形同虚设。

同时给响应加 `wallet` 字段（`balance`/`onboardCount`），省掉前端一次额外请求。

**验证：**
```bash
cargo test --bin kirostudio portal::http 2>&1 | tail -5
```
Expected: 编译通过、现有 http 测试不回归

**Dependencies:** Task 6

---

### Task 9：上车与钱包接口

**文件：** `src/portal/http.rs`（修改）

新增两个 handler，注册到私有路由层（`require_session` 之后，约 727 行）：

- `POST /portal/api/board/{credential_id}` → 调 `store.board(...)`
  - `Ok` → 200 `{ok:true, price, balance, count, key}`（**明文在这里首次下发**）
  - `AlreadyOnboard` → 200 `{ok:true, alreadyOnboard:true, key}`
  - `NotEnough` → **402** `{error:"积分不足", needed, balance}`
  - `Full` → **409** `{error:"该车已满", count, max}`
- `GET /portal/api/wallet` → `{balance, topup, spent, ledger:[...]}`（最近 100 条）

两个 handler 都要写审计：`board_ok` / `board_fail_insufficient` / `board_fail_full`，detail 记 `cred=<id> price=<n>`。明文外显是必须留痕的动作——事后要能回答「谁在什么时候上了哪辆车」。

`portalCreditsEnabled=false` 时 `POST /board` 返回 **404**（该功能不存在），与 `portalEnabled` 关闭时的处理一致：不确认功能存在。

**验证：**
```bash
cargo build --bin kirostudio 2>&1 | grep -E "^(error|warning: unused)" | head -5
```
Expected: 无 error

**Dependencies:** Task 8

---

### Task 10：管理侧充值与钱包查询

**文件：** `src/portal/admin_api.rs`（修改，路由在 287-292 行）

新增三个端点（都在现有 admin 鉴权中间件之后）：

- `POST /users/{id}/topup` → body `{amount: i64, note: Option<String>}`
  - `amount > 0` 充值、`< 0` 扣减、`== 0` 拒绝（400，无意义操作）
  - 写审计 `admin_topup`，detail 记 `amount=<n> note=<...>`
- `GET /users/{id}/wallet` → `{balance, topup, spent, ledger[]}`
- `GET /pricing` → 当前四个配置参数 + 说明（前端展示"当前车费规则"）

`AdminUserRow` 增加 `balance: i64` 字段，`list_users` 联表查出（`LEFT JOIN portal_balances`，无记录时为 0）。

**绝不新增任何返回 `password_hash` 的路径。** 这条红线在 Task 10 尤其容易破——联表查询时用 `SELECT u.id, u.username, ..., COALESCE(b.balance, 0)` 显式列字段，不要 `SELECT u.*`。

**验证：**
```bash
cargo test --bin kirostudio portal::admin_api 2>&1 | tail -5
```
Expected: 编译通过

**Dependencies:** Task 6

---

### Task 11：配置接线

**文件：** `src/main.rs`（修改，portal 装配块约 600-660 行）

- 启动时把四个参数读进 `PortalState`（或进程级镜像，与 `portal::http::set_enabled` 同一套写法）
- `portalCreditsEnabled=true` 且 `portalEnabled=true` 时打印一行启动日志，写明当前车费规则：`车队积分已启用：前 2 人各 10 分，之后 20/N 均摊，最多 10 人`
- 只开 `portalCreditsEnabled` 但没开 `portalEnabled` → `warn` 提示后者才是总开关

**验证：**
```bash
cargo build --bin kirostudio 2>&1 | tail -3
```
Expected: 无 error

**Dependencies:** Task 9, Task 10

---

### Task 12：用户页改成"上车"交互

**文件：** `src/portal/page.rs`（修改）

术语全面改成车队话术（用户的原话）：

| 原文案 | 新文案 |
|---|---|
| 凭据查看 | Kiro 车队 |
| 密钥列 | 车票 |
| （无按钮） | `[上车 · 7分]` |
| （直接显示明文） | 未上车显示 `••••••••`，上车后显示明文 + 复制按钮 |

新增元素：

1. **余额条**（页面顶部）：`余额 42 分 · 已上 3 辆车`
2. **每行「上车」按钮**：显示当前车费，点击 → `POST /board/{id}` → 成功后就地把该行的车票替换成明文 + 复制按钮，并更新余额条
3. **上车人数列**：`3/10 人`，满员显示 `已满` 徽章并禁用按钮
4. **失败提示**：402 → `积分不足：需 7 分，当前 3 分`；409 → `该车已满（10/10）`

**必须遵守现有页面的两条铁律**（见 `page.rs` 头部注释）：

- 全程 `textContent` / `createElement`，**绝不 `innerHTML`**
- 任何新增 CSS 类必须真的在 `<style>` 块里定义

第二条是踩过的坑：`.sm` / `.spark-b` 两次类名对不上导致布局静默失效，靠截图才发现。本任务完成后**必须**跑一致性检查（Task 13）。

**验证：**
```bash
cargo build --bin kirostudio 2>&1 | tail -3
```
Expected: 无 error

**Dependencies:** Task 9

---

### Task 13：页面一致性自动检查

**文件：** `src/portal/page.rs`（在 `#[cfg(test)] mod tests` 中新增）

把之前手工做的检查固化成 Rust 测试，不再依赖我记得去跑脚本：

```rust
#[test]
fn every_js_id_exists_in_html() { ... }      // $('x') 都能在 id="x" 找到
#[test]
fn every_css_class_used_is_defined() { ... } // className 用到的类都在 <style> 里
#[test]
fn no_inner_html_usage() { ... }             // 全文不得出现 innerHTML
```

实现方式：`PAGE_HTML` 是 `&'static str`，测试里直接正则扫它。第三个测试是安全测试——`innerHTML` 一旦出现，凭据里的 `<` 就会被当代码执行。

**这三个测试能挡住本次会话已经踩过两次的 bug**（缺 `id="summary"`、类名 `.sm` vs `.summary`）。

**验证：**
```bash
cargo test --bin kirostudio portal::page 2>&1 | tail -8
```
Expected: 3 个测试全绿

**Dependencies:** Task 12

---

### Task 14：admin 前端加充值与余额

**文件：**
- `admin-ui/src/api/portal.ts`（修改）
- `admin-ui/src/hooks/use-portal.ts`（修改）
- `admin-ui/src/components/portal-page.tsx`（修改）
- `admin-ui/src/i18n/resources/{zh,en,ja}.json`（修改）

- API 客户端加 `topupUser(id, amount, note)` / `getUserWallet(id)` / `getPricing()`
- 用户列表加「余额」列 + 「充值」按钮（弹窗输入分数与备注，支持负数扣减）
- 充值成功后 invalidate `portal-users` / `portal-wallet` 两个 query key
- 顶部状态卡增加一格「当前车费规则」：`前 2 人 10 分 · 之后 20/N · 上限 10 人`
- 三个语言的键都要加（`zh` / `en` / `ja` 必须同步，缺一个会在该语言下显示成裸键名）

**验证：**
```bash
cd admin-ui && npx --yes pnpm@9 exec tsc -b && npx --yes pnpm@9 run build 2>&1 | tail -3
```
Expected: `tsc` 退出 0，build 成功

**Dependencies:** Task 10

---

### Task 15：全量回归 + 镜像验证

**文件：** 无（验证任务）

1. 全量测试：`cargo test --bin kirostudio`
2. **关闭开关的回归**：`portalCreditsEnabled=false` 时，portal 行为与本次改动前**完全一致**（明文直接可见、无上车按钮）
3. 构建 demo 镜像，起在 8991（**不碰生产 8990**）
4. 端到端脚本验证：
   - 建 3 个用户，管理侧各充 50 分
   - 用户 A 上车 key#9001 → 扣 10 分（N=1）
   - 用户 B 上车同一 key → 各 10 分（N=2，无退款）
   - 用户 C 上车 → 三人各 7 分，**A 和 B 各退 3 分**（差额模型的核心验证）
   - 用户 A 余额不足场景 → 402
   - 满员场景 → 409
   - **`curl` 未上车用户的 `/keys`，grep 明文必须 0 命中**（带对照组：已上车用户必须搜到）
5. 生产未受影响：`docker ps` 确认 `kirostudio` 仍是 v16、`restarts=0`

**验证：** 上述每一步都要有明确输出，不能只看"没报错"。特别是第 4 步的退款验证——这是整套设计最容易写错的地方，必须看到 A/B 余额真的从 40 变成 43。

**Dependencies:** Task 11, Task 13, Task 14

---

## 成功标准

1. `cargo test --bin kirostudio` 全绿，新增测试不少于 25 个
2. `credits.rs` 的穷举测试覆盖 N=1..max × 8 组参数配置，全部单调不增
3. 差额模型验证：1→10 人陆续上车，**任何时刻**每人净支出 == `unit_price(N)`
4. 并发测试连跑 5 次无抖动
5. 未上车时明文不出现在任何响应体中（带对照组证明检测有效）
6. `portalCreditsEnabled=false` 时行为与改动前完全一致
7. 前端 `tsc -b` 干净，三语言 i18n 键齐全
8. demo 容器端到端跑通全部 5 个场景
9. 生产 8990 全程未受影响

## 回滚方案

**代码回滚：** 全部改动在 `feat/portal-credits` 分支，`git checkout master` 即可。生产跑的是 v16 镜像，与本分支无关。

**数据回滚：** 四张新表都是 `CREATE TABLE IF NOT EXISTS`，不修改任何现有表结构，因此旧版二进制读新库不会出错（多几张它不认识的表而已）。若要彻底清除：

```sql
DROP TABLE portal_ledger;
DROP TABLE portal_key_pricing;
DROP TABLE portal_unlocks;
DROP TABLE portal_balances;
```

**配置回滚：** 把 `portalCreditsEnabled` 改回 `false`，热更即时生效，明文恢复直接可见——不需要重启，也不需要动数据。这是默认关闭设计的直接好处。

## 风险点

| 风险 | 应对 |
|---|---|
| 差额退款算错，用户余额对不上 | `credits.rs` 纯函数穷举 + 1→10 人模拟断言（Task 3） |
| 并发下 N 算错或超员 | `BEGIN IMMEDIATE` + 并发测试连跑 5 次（Task 7） |
| 未上车明文泄漏 | Task 8 服务端置 `None` + Task 15 curl 验证带对照组 |
| 页面类名/id 对不上导致静默失效 | Task 13 固化成三个 Rust 测试 |
| 改价影响老 key | 参数快照表（Task 5），改价只影响新 key |
| 升级后老数据无快照 | 快照缺失回退当前配置，测试覆盖（Task 5） |
