# Portal 积分制凭据解锁 — 设计

日期：2026-08-06
状态：待实现（设计已与需求方确认规则，尚未开工）

## 一句话

Portal 用户查看凭据明文需消耗积分。**每把 key 独立结算**：看的人越多，每人分摊越少，多付的部分实时退回余额。

## 为什么

现在明文是白给的——任何登录用户打开页面就看到全部 `ksk_`。加积分门槛有两个作用：限制无节制取用，以及让「有多少人在用同一把 key」对所有人可见（需求里的「已有多少人上车」）。

分摊退款的意义是**不惩罚先上车的人**。若只按人数定价而不退款，第 1 个人付 10 分、第 10 个人付 2 分，先承担风险的反而付最多，激励是反的。

## 计费规则

### 单价公式

```
unit_price(N) = max(min_price, min(default_price, ceil(total_price / N)))
```

- `N` = 该 key 的 distinct 解锁人数
- `default_price` = 10（单人价，同时是价格上限）
- `total_price` = 20（定价基数，见下方「两个必须知道的后果」）
- `min_price` = 1（下限）

三个参数各自可配，可组合成 10/50/1 之类的任意取值。

代入验证（默认参数，已用脚本跑过）：

| N | ceil(20/N) | 单价 | 说明 |
|---|---|---|---|
| 1 | 20 | **10** | 被 default_price 截断 |
| 2 | 10 | **10** | 恰好相等，无退款 |
| 3 | 7 | **7** | 20/3=6.67 上取整 |
| 4 | 5 | **5** | |
| 10 | 2 | **2** | |
| 20 | 1 | **1** | 触底 |
| 50 | 1 | **1** | 保持触底 |

### 两个必须知道的后果

这两条不是缺陷，是所选规则（ceil + 下限）的算术必然。写在这里以免日后被当成 bug。

**一、取整让总收入超过 total_price。** N=9 时每人 3 分，九人共 27 分，比 20 多 7 分：

| N | 单价 | 总收入 | 相对 20 的偏差 |
|---|---|---|---|
| 3 | 7 | 21 | +1 |
| 6 | 4 | 24 | +4 |
| 9 | 3 | 27 | +7 |

因此 `total_price` 的准确含义是**定价基数**，不是「系统对这把 key 只收 20 分」。若要求总收入严格等于 20，就必须让不同用户付不同价（有人 6 有人 7），那会破坏「人数相同则价格相同」的可解释性——不采用。

**二、N ≥ 20 后「均摊固定盘子」的说法失效。** 下限 1 分一生效，总收入随人数线性增长：50 人 = 50 分。这是下限带来的正常行为。

### 已付/应付差额模型

**不记录「退了多少」，只记录「已付多少」。** 退款 = `已付 − 应付`，每次人数变化时重算。

这是整套设计的正确性核心。原因：ceil 之下，「按当前单价退增量」的做法会随人数变化累积误差。差额模型保证**任何时刻**每人净支出恒等于 `unit_price(N)`，总账天然自洽，无需对账逻辑。

已用脚本模拟 1→50 人陆续上车，50 步全部满足 `每人 paid == 每人净支出 == unit_price(N)`，零异常。

N=3 → 4 的完整过程：

| 用户 | N=3 已付 | N=4 应付 | 动作 |
|---|---|---|---|
| A | 7 | 5 | 退 2 |
| B | 7 | 5 | 退 2 |
| C | 7 | 5 | 退 2 |
| D | — | 5 | 扣 5 |

### 幂等

同一用户对同一 key **只扣一次**。重复打开、刷新、再点解锁都不再扣分。故 `N` = distinct 用户数，页面上的「N 人已上车」就是 N 个不同账号。

### 余额不足

扣费前检查，不足则拒绝解锁并告知差额（「需 5 分，当前 3 分」）。**不允许负余额。**

### 退款落账

实时退回余额，并在流水里留可追溯记录：

```
-7  解锁 #9001（当时 3 人）
+2  #9001 均摊退款（现 4 人）
+3  #9001 均摊退款（现 10 人）
```

## 数据模型

三张新表，都在现有 `portal.db`（与 `portal_users` 同库，可靠外键级联）。

### `portal_balances`

```sql
CREATE TABLE portal_balances (
  user_id  INTEGER PRIMARY KEY REFERENCES portal_users(id) ON DELETE CASCADE,
  balance  INTEGER NOT NULL DEFAULT 0,   -- 当前余额（整数分，永不为负）
  topup    INTEGER NOT NULL DEFAULT 0,   -- 累计充值（对账）
  spent    INTEGER NOT NULL DEFAULT 0    -- 累计净支出（对账）
);
```

单独一张表而非给 `portal_users` 加列：余额是高频读写、需事务保护的字段，与账号信息生命周期不同。

### `portal_unlocks`

```sql
CREATE TABLE portal_unlocks (
  user_id        INTEGER NOT NULL REFERENCES portal_users(id) ON DELETE CASCADE,
  credential_id  INTEGER NOT NULL,        -- 凭据池 ID，不设外键（池不在本库）
  paid           INTEGER NOT NULL,        -- 累计已付：差额模型的核心
  unlocked_at_ms INTEGER NOT NULL,
  PRIMARY KEY (user_id, credential_id)
);
CREATE INDEX idx_portal_unlocks_cred ON portal_unlocks(credential_id);
```

复合主键天然保证幂等。`credential_id` 不设外键——凭据池在 `credentials.json` 而非本库，且凭据被删后解锁记录应保留（历史仍需可查，与 `portal_audit` 同理）。

### `portal_ledger`

```sql
CREATE TABLE portal_ledger (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  user_id       INTEGER NOT NULL,
  at_ms         INTEGER NOT NULL,
  delta         INTEGER NOT NULL,         -- 正=进账，负=出账
  balance_after INTEGER NOT NULL,         -- 快照，对账不必重放全部流水
  kind          TEXT NOT NULL,            -- topup / unlock / refund / admin_adjust
  credential_id INTEGER,
  note          TEXT
);
CREATE INDEX idx_portal_ledger_user ON portal_ledger(user_id, at_ms);
```

不设外键，理由同审计表：用户被删后流水应保留，否则无法回答「这个账号一共花了多少」。

## 组件划分

```
src/portal/
  credits.rs   ← 新增：纯计算（单价、差额），零 I/O，可穷举单测
  store.rs     ← 扩展：三表 CRUD + 事务化 unlock/refund
  http.rs      ← 扩展：POST /portal/api/unlock/{id}；keys 响应加锁定态
  admin_api.rs ← 扩展：充值 / 调账 / 查余额流水
  page.rs      ← 扩展：解锁按钮、余额条、上车人数
```

`credits.rs` 独立的理由：这套公式是全系统唯一的正确性核心，必须能在不碰数据库的情况下穷举验证（N=1..100、参数边界、取整行为）。混在 store 里就只能靠集成测试覆盖，而集成测试跑不了 100 种人数组合。

## 关键流程：解锁（单事务）

```
BEGIN IMMEDIATE
  1. 已解锁？ → 直接返回明文（幂等，不扣分，不写流水）
  2. N_new = distinct_unlockers(cred) + 1
  3. price  = unit_price(N_new)
  4. 余额 < price？ → ROLLBACK，402 + 差额提示
  5. 扣 price；写 unlocks(paid=price)；写 ledger(unlock)
  6. 该 key 的其余 (N_new − 1) 人：
       refund = paid − price
       refund > 0 → 加余额；paid = price；写 ledger(refund)
COMMIT
→ 返回明文
```

必须在一个事务内。两人同时点解锁时，若无 `BEGIN IMMEDIATE`，二者可能都读到 `N=2` 而各按 10 分扣，正确结果应是都按 7 分（N=3）。用 IMMEDIATE 提前拿写锁，避免读锁升级失败。

## 接口

### 用户侧

- `POST /portal/api/unlock/{credential_id}` → `{ok, price, balance, key}`；余额不足返回 402 + `{needed, balance}`
- `GET /portal/api/keys` → 每行新增 `unlocked`(bool)、`unlockPrice`(当前单价)、`unlockCount`(N，即上车人数)；**未解锁时 `key` 字段不下发**
- `GET /portal/api/wallet` → `{balance, topup, spent, ledger[]}`

关键：未解锁的明文**不进响应体**。不是前端隐藏——那等于已经把明文发给了浏览器，F12 就能看到，门槛形同虚设。

### 管理侧

- `POST /api/admin/portal/users/{id}/topup` → `{amount, note}`；支持负数做扣减（记为 `admin_adjust`）
- `GET /api/admin/portal/users/{id}/wallet` → 余额 + 流水
- 用户列表增加 `balance` 列

## 配置

```jsonc
{
  "portalCreditsEnabled": false,   // 默认关：不开就是现在的白给行为，升级不改变现状
  "portalKeyDefaultPrice": 10,     // 单人价 / 价格上限
  "portalKeyTotalPrice": 20,       // 定价基数
  "portalKeyMinPrice": 1           // 下限
}
```

默认关闭的理由与 `portalEnabled` 一致：升级版本不该让已有部署突然开始收费、把现有用户挡在门外。

## 测试

`credits.rs` 纯函数：
- N=1..100 全覆盖：单调不增、恒 ≥ min_price、恒 ≤ default_price
- 差额模型：模拟 1→50 人陆续上车，断言**任何时刻**每人净支出 == `unit_price(N)`
- 参数边界：`total < default`、`min > total`、三者为 0
- 整数运算：ceil 用整数实现而非浮点（避免 20/3 的浮点表示误差）

store 层事务：
- 幂等：同一 (user, cred) 解锁两次 → 只扣一次、只一条 ledger
- 余额不足：拒绝后 balances 与 unlocks **均无变化**（回滚彻底）
- 并发：两线程同抢一把 key → 最终 N=2、各付 10、总扣 20
- 级联：删用户后 balances/unlocks 清空，ledger 保留

http 层：
- 未解锁时响应体内**搜不到明文**（带对照组：已解锁的 key 必须搜得到，否则搜索方法本身无效）
- 402 带 needed/balance
- `portalCreditsEnabled=false` 时行为与现状完全一致（回归保护）

## 明确不做

- 积分过期 / 有效期
- 自助充值、支付对接（手动充值已满足需求）
- 不同 key 不同价（现在全池同价）
- 跨 key 打折（每把 key 独立结算）
- 「首次 10 分」的例外规则（全员一致，无特殊情况）
