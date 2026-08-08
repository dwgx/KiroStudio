# Portal 积分制凭据解锁 — 设计

日期：2026-08-06
状态：待实现（设计已与需求方确认规则，尚未开工）

## 一句话

Portal 用户查看凭据明文需消耗积分。**每把 key 独立结算**：看的人越多，每人分摊越少，多付的部分实时退回余额。

## 为什么

现在明文是白给的——任何登录用户打开页面就看到全部 `ksk_`。加积分门槛有两个作用：限制无节制取用，以及让「有多少人在用同一把 key」对所有人可见（需求里的「已有多少人上车」）。

分摊退款的意义是**不惩罚先上车的人**。若只按人数定价而不退款，第 1 个人付 10 分、第 10 个人付 2 分，先承担风险的反而付最多，激励是反的。

## 计费规则

### 单价公式（两段式）

```
N ≤ base_count  →  base_price                                  ← 前段：固定价
N >  base_count  →  max(min_price, min(base_price, ceil(total_price / N)))   ← 后段：均摊
```

- `N` = 该 key 的 distinct 解锁人数，**上限 `max_unlockers` = 10**
- `base_count` = 2（前几人享固定价）
- `base_price` = 10（那个固定价，同时是全局价格上限）
- `total_price` = 20（后段的均摊基数，见下方「一个必须知道的后果」）
- `min_price` = 1（下限，默认参数下永不触发，见下）

五个参数各自可配。**前段的「几人」和「多少分」是两个独立参数**，因此「2 人 10 分」「4 人 5 分」「6 人 3 分」都能直接配出来，不必反推基数。

代入验证（两组配置，均已用脚本逐行跑过）：

| N | `base=2, price=10, total=20` | `base=4, price=5, total=20` |
|---|---|---|
| 1 | **10** | **5** |
| 2 | **10** | **5** |
| 3 | **7** | **5** |
| 4 | **5** | **5** |
| 5 | **4** | **4** |
| 6 | **4** | **4** |
| 7 | **3** | **3** |
| 8 | **3** | **3** |
| 9 | **3** | **3** |
| 10 | **2** | **2** |

左列与旧公式的结果**完全一致**——旧的单段公式是新公式在 `base_count=2` 时的特例，所以这次改动向后兼容，已有配置的行为不变。

### 后段为什么要取 `min(base_price, …)`

不取 min 会让某些配置组合出现**越多人越贵**。以 `base_count=4, base_price=5, total_price=100` 为例：

| | N=4 | N=5 | N=6 |
|---|---|---|---|
| 不取 min | 5 | **20** | 17 |
| 取 min | 5 | **5** | 5 |

N=4→5 时价格从 5 跳到 20，因为 `ceil(100/5)=20` 远高于前段的固定价。这会让用户看到「多来一个人，我要多付 15 分」——而整套设计的承诺正好相反。

取 min 把后段钳在前段价格之下，保证**单调不增**（人数增加时单价只会降或平，绝不上涨）。已用脚本验证 8 组配置组合（含上述病态组合）全部满足单调不增。这条性质是用户能理解这套规则的前提，必须在 `credits.rs` 里有对应断言。

### 人数上限 = 10

第 11 个人解锁被拒，返回「该凭据查看人数已满（10/10）」，不扣分。

这个上限有三个作用：

1. **把总收入偏差限在有界范围内**（见下方后果）。没有上限时，1 分下限一生效，总收入会随人数无限线性增长。
2. **`min_price` 因此成为死参数**。默认参数下 N 最大为 10，`ceil(20/10)=2` 永远大于 1，下限触发不了。保留这个配置项是为了「改大 total_price 或改小 max_unlockers 时仍有兜底」，但默认部署下它不生效——**不要依赖它来控制最低价**。
3. **让「谁在用这把 key」是个有限集合**。10 个人共享一把凭据已经是滥用风险的上限，再多则这把 key 的行为特征会杂乱到无法归因。

### 一个必须知道的后果

**取整让总收入超过 total_price，但有界。** N=9 时每人 3 分，九人共 27 分，比 20 多 7 分：

| N | 单价 | 总收入 | 相对 20 的偏差 |
|---|---|---|---|
| 2 | 10 | 20 | 0 |
| 3 | 7 | 21 | +1 |
| 6 | 4 | 24 | +4 |
| 9 | 3 | 27 | **+7（最坏）** |
| 10 | 2 | 20 | 0 |

因此 `total_price` 的准确含义是**定价基数**，不是「系统对这把 key 只收 20 分」。

若要求总收入严格等于 20，就必须让不同用户付不同价（有人 6 有人 7），那会破坏「人数相同则价格相同」的可解释性，也让退款算不清。不采用。偏差有界（最坏 +7）且可解释，接受。

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

### `portal_key_pricing`

**价格参数快照。** 一把 key 首次被解锁时，把当时的三个参数冻结在这里，此后这把 key 一直用它们计价。

```sql
CREATE TABLE portal_key_pricing (
  credential_id  INTEGER PRIMARY KEY,     -- 一把 key 一行
  base_count     INTEGER NOT NULL,        -- 冻结时的前段人数
  base_price     INTEGER NOT NULL,        -- 冻结时的前段固定价 / 上限
  total_price    INTEGER NOT NULL,        -- 冻结时的均摊基数
  min_price      INTEGER NOT NULL,        -- 冻结时的下限
  max_unlockers  INTEGER NOT NULL,        -- 冻结时的人数上限
  frozen_at_ms   INTEGER NOT NULL
);
```

五个参数**全部**冻结，不只是价格。`base_count` 也必须冻——否则管理员把前段从 2 人改成 4 人时，一把已有 3 人解锁的 key 会从「第 3 人按均摊 7 分」变成「第 3 人享固定价 10 分」，价格上涨，正是快照要防的事。

**为什么需要它。** 若不冻结，管理员把基数 20 调到 40 时，已有 4 人解锁的 key 单价会从 5 分跳到 10 分，产生两个都不可接受的选择：追扣老用户 5 分（用户莫名少钱，余额可能被扣成负数），或让老用户维持 5 分而新用户付 8 分（同一把 key 两种价格，「均摊」的说法当场失效，且 `paid` 字段语义分裂成新旧两套、无法用单一公式校验总账）。

冻结之后，语义变得干净：**改价只影响之后才首次被解锁的 key**。已经在用的 key 参数不动，`paid` 字段永远可以用该 key 的快照参数验证。

**推论：改价不产生退款。** 老 key 的单价不变，没有差额可退，因此不存在「改价退款」这类流水。改价只写审计（`admin_change_price`，记录改前改后的值），不触碰任何用户余额。这一点是上面那个决定的直接结果，不是遗漏。

首次解锁时若该行不存在则插入（与解锁在同一事务内，避免并发下两人各插一次）；读取时若缺失则回退到当前配置（兼容积分功能开启前就存在的 key）。

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
  2. 读 key_pricing(cred)：
       有快照 → 用快照里的 (default, total, min, max)
       无快照 → 用当前配置，并写入快照（本 key 的价格从此固定）
  3. N_cur = distinct_unlockers(cred)
  4. N_cur >= max（来自快照）？ → ROLLBACK，409 + 「已满 (N/max)」
  5. N_new = N_cur + 1
  6. price = unit_price(N_new, 快照参数)
  7. 余额 < price？ → ROLLBACK，402 + 差额提示
  8. 扣 price；写 unlocks(paid=price)；写 ledger(unlock)
  9. 该 key 的其余 (N_new − 1) 人：
       refund = paid − price
       refund > 0 → 加余额；paid = price；写 ledger(refund)
COMMIT
→ 返回明文
```

**第 2 步的快照写入必须在同一事务内。** 两人同时首次解锁同一把 key 时，若快照写在事务外，二者可能各自读到不同的配置值（管理员正好在此刻改价），于是同一把 key 上出现两套参数、`paid` 语义分裂。放进事务里由 SQLite 写锁串行化：先到者写快照，后到者读到的必然是同一份。

**满员检查必须在事务内、在扣费之前。** 放到事务外就是一个 TOCTOU 竞态：10 人已满时两人同时点解锁，都读到 `N_cur=10` 之前的旧值而通过检查，最终变成 12 人。放在扣费之后则更糟——钱扣了才发现满员，得靠回滚兜住，多一条容易出错的路径。

同理，整个流程必须在一个事务内。两人同时点解锁时若无 `BEGIN IMMEDIATE`，二者可能都读到 `N=2` 而各按 10 分扣，正确结果应是都按 7 分（N=3）。用 IMMEDIATE 提前拿写锁，避免读锁升级失败。

## 接口

### 用户侧

- `POST /portal/api/unlock/{credential_id}` → `{ok, price, balance, key}`
  - 余额不足 → **402** + `{needed, balance}`
  - 名额已满 → **409** + `{unlockCount, maxUnlockers}`（用 409 冲突而非 403：这是「状态冲突，换个 key 或等人退出」，不是「你没权限」）
- `GET /portal/api/keys` → 每行新增 `unlocked`(bool)、`unlockPrice`(当前单价)、`unlockCount`(N，已上车人数)、`maxUnlockers`(上限)、`full`(bool，N 是否已达上限)；**未解锁时 `key` 字段不下发**
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
  "portalKeyBaseCount": 2,         // 前段人数：前几个人享固定价
  "portalKeyBasePrice": 10,        // 前段固定价，同时是全局价格上限
  "portalKeyTotalPrice": 20,       // 均摊基数：N > baseCount 时按 total/N 分摊
  "portalKeyMaxUnlockers": 10,     // 每把 key 最多几人解锁（满员后拒绝）
  "portalKeyMinPrice": 1           // 下限；默认参数下永不触发，见下
}
```

**你要的两种配置分别这么写：**

「2 人 10 分」（默认）：
```jsonc
{ "portalKeyBaseCount": 2, "portalKeyBasePrice": 10, "portalKeyTotalPrice": 20 }
// → 10 10 7 5 4 4 3 3 3 2
```

「4 人 5 分」：
```jsonc
{ "portalKeyBaseCount": 4, "portalKeyBasePrice": 5, "portalKeyTotalPrice": 20 }
// → 5 5 5 5 4 4 3 3 3 2
```

默认关闭的理由与 `portalEnabled` 一致：升级版本不该让已有部署突然开始收费、把现有用户挡在门外。

### 改价的影响范围

**改配置只影响「还没有任何人解锁过」的 key。** 已有解锁者的 key 在首次解锁时就把四个参数写进了 `portal_key_pricing` 快照，此后永久按快照计价，改配置对它没有任何作用。

这条规则带来两个直接结论，都是有意的：

**一、改价不会产生退款。** 老 key 的参数不变 → 应付不变 → 没有差额可退。所以流水里不存在「改价退款」这种记录，也不需要为改价写任何批量重算逻辑。（曾考虑过「涨价不追扣、降价照退」的折中方案，但它会让同一把 key 上出现两种价格，`paid` 字段的含义按用户分裂，总账无法用一个公式校验——不采用。）

**二、改价仍然要写审计。** 记录「谁在何时把 total 从 20 改成 40」。虽然没有用户余额受影响，但价格是计费的基础参数，改动必须可追溯——否则日后对账时无法解释「为什么 key#7 是 20/10 而 key#9 是 40/10」。审计动作名 `admin_change_pricing`，detail 记下改动前后的值。

**`portalKeyMinPrice` 在默认参数下是死参数。** 上限 10 人时最低单价是 `ceil(20/10)=2`，永远碰不到下限 1。保留它只为「把上限调大到 20+ 或把基数调小」这类改配置的场景兜底——那时它才开始起作用。不删除，但也不要指望它在默认配置下有任何效果。

## 测试

`credits.rs` 纯函数：
- N=1..max 全覆盖：**单调不增**、恒 ≥ min_price、恒 ≤ base_price
- 前段恒定：N ≤ baseCount 时单价恒等于 basePrice（用 baseCount=1/2/4/10 各验一遍）
- 你给的两组配置逐项对齐：
  - `base=2,price=10,total=20` → `10 10 7 5 4 4 3 3 3 2`
  - `base=4,price=5,total=20` → `5 5 5 5 4 4 3 3 3 2`
- **病态配置的 min 钳制**（这条最重要）：`base=4, price=5, total=100` 时，
  若后段不取 `min(basePrice, …)`，N=5 会从 5 分跳到 20 分——价格随人数上涨。
  断言该组合下序列仍单调不增（全 5 分）。已用脚本验证 8 组配置，全部通过。
- 差额模型：模拟 1→10 人陆续上车，断言**任何时刻**每人净支出 == `unit_price(N)`（已用脚本预演，10 步零异常）
- 满员判定：`N == max` 时新用户被拒，已解锁者不受影响
- 参数边界：`total < basePrice`、`min > total`、`max = 1`、`baseCount > max`、三者为 0
- 整数运算：ceil 用整数实现而非浮点（避免 20/3 的浮点表示误差）

store 层事务：
- 幂等：同一 (user, cred) 解锁两次 → 只扣一次、只一条 ledger
- 余额不足：拒绝后 balances 与 unlocks **均无变化**（回滚彻底）
- 满员：第 11 人被拒后，前 10 人的 paid 与余额均无变化
- 并发：两线程同抢一把 key → 最终 N=2、各付 10、总扣 20
- 并发满员：10 人已满时两线程同抢 → 都被拒，**不会出现 N=11**
- 级联：删用户后 balances/unlocks 清空，ledger 保留

参数快照（`portal_key_pricing`）：
- 首次解锁写入快照；第 2..N 人读到的是**同一份**快照
- 改配置后老 key 价格不变：4 人已解锁 → 改 total 20→40 → 第 5 人仍按旧参数 `ceil(20/5)=4` 计价
- 改配置后新 key 用新参数：另一把从未解锁的 key → 第 1 人按新参数
- 快照缺失（老数据升级场景）→ 回退当前配置，不 panic
- 快照与配置无关性：删掉配置项后老 key 仍按快照结算

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
