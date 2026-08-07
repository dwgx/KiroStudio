# region 自纠正设计（2026-08-06）

> 目标：**US 号导入进来跑得和 EU 一样**，且不依赖任何外部脚本喂 region。
> 结论来自对 ZyphrZero/kiro.rs v0.7.1 的挖掘 + 本机实测，**不是推理**。

---

## 一、问题：为什么 US 号现在 100% 废

### 实测证据（2026-08-06，凭据 #749 `ksk_u7Wd…`，已知只在 eu-central-1 授权）

用**与 `probe_dialog_endpoint` 完全相同的请求**（同 body、同全部 header）分别打两区：

```
【当前探测目标：q.*.amazonaws.com 服务根 + X-Amz-Target】
eu-central-1  -> HTTP 400  {"__type":"com.amazon.aws.codewhisperer#ValidationException",
                            "message":"Improperly formed request.","reason":"REQUEST_BODY_INVALID"}
us-east-1     -> HTTP 400  {"__type":"com.amazon.kiro.runtimeservice#ValidationException",
                            "message":"Improperly formed request.","reason":"REQUEST_BODY_INVALID"}
                            ↑ 它在 us-east-1 **没有授权**，却同样是 400

【旧探测目标：management.*.kiro.dev/getUsageLimits】
eu-central-1  -> HTTP 200                                             ← 已授权
us-east-1     -> HTTP 403  {"message":"Invalid token","reason":null}   ← 未授权
```

### 根因链

1. **AWS 先校验请求体格式、后校验 region 授权。** 探测体是**故意不完整**的
   （为了不消耗额度），所以在**任何**区都先撞 `REQUEST_BODY_INVALID` 400，
   授权那一关**根本没被求值**。
2. `classify_probe_result` 把 400 判 `Usable`（依据是「未授权会回 403 而非 400」——
   **该断言已被上面实测证否**）。
3. `PROBE_ORDER = ["eu-central-1", "us-east-1"]`，**EU 排第一**。
4. ⇒ **任何 `api_key` 号第一次探测都被判「eu-central-1 可用」**，不管真实授权在哪。
5. ⇒ US 号的 `api_region` 被写成 `eu-central-1` → 该号**恒 403**。

⚠️ 这是**第二轮 E-1 引入的回归**。E-1 的动机（"探 A 域名、决定 B 域名"）听起来合理，
但实测证明 `management.*` 虽是另一个域名却**确实能区分授权**、且与 `q.*` 可用性一致
（同一 key：`management.eu` 200 / `q.eu` 98.9% 成功；`management.us` 403）。
换到真实端点反而**丢掉**了区分能力。

---

## 二、ZyphrZero/kiro.rs 到底怎么做的（挖掘结论）

用户观察：「在 kiro-rs 那里直接添加 11 个就是可以用」。挖掘后的真相：

### 它是**无状态**的：每次调用都现场试两个区

`rest_api_region_candidates(sso_region) -> [&str; 2]`（`token_manager.rs:458`）
按 SSO region 前缀选主端点、另一个作 403 回退候选。**有 4 处调用**
（`:492`/`:587`/`:685`/`:776`），注释统一是「依据凭据 SSO 区域选择主端点，
**403 时回退到另一个端点**」。

**它从不回写 `api_region`** —— grep `api_region = ` 只命中测试与一处 validate 拷贝。

⇒ **没有「一次探测定终身」，就没有探错的可能。** 这才是「直接添加就能用」的机制。

### 但它的**对话路径不换区**

`provider.rs:374` 对 401/403 的处置是「凭据问题」→ 先试 force-refresh、
再 failover 到**另一个凭据**，**不换 region**。

⇒ kiro-rs 的对话路径**也没有**自纠正能力。它能跑是因为 `sync_us.py` 在交给它之前
就把 `apiRegion` 显式写死了（该脚本 docstring 原文：「同步时把有效区域显式写进
apiRegion，不依赖 kiro-rs 的全局默认值，避免两边默认值不一致导致打错区」）。

**即 kiro-rs 没解决这个问题，它把问题外包给了那个 Python 脚本。**

所以**不能照抄它**。正解是把两边优点合起来。

---

## 三、设计：三层，各自独立可回退

### L1 🔴 对话路径 403 → 换区重试（kiro-rs 的 REST 有、对话没有；我们两处都要）

`ksk_` 号打错区的 403 body 是 `bearer token included in the request is invalid`
/ `Invalid token`。这个信号在对话路径上**现在被当「凭据问题」→ 换号**，
而换号解决不了 —— **同一个号换个区就行**。

**判据必须窄**，且要与既有两条分支分清：
- `provider.rs` 已有 `bearer_invalid_but_proven`（`has_ever_succeeded` 为真 ⇒ 判瞬态抖动，
  只设冷却 + failover，**不** report_failure）。**region 错配与它是不同的东西**：
  前者该换区，后者该换号。
- 第二轮已给瞬态那条打了机器可读标记 `bearer_invalid_transient=1`。
  region 换区分支**必须排在它之后**，或用 `has_ever_succeeded` 同款二分：
  **从未成功过的号**才可能是 region 错配（已成功过说明区是对的）。

⚠️ 换区重试**每个号最多一次**（用一个 per-call 的 `HashSet<u64>` 记录，
与既有 `force_refreshed` 同款范式），否则两个区来回打 = 放大。

### L2 🔴 成功的区立刻回写（kiro-rs 完全没有这一层）

L1 换区成功后，把该区写进 `api_region` 并持久化。
⇒ 第一次自我纠正之后写死，**后续请求零额外开销**。这比 kiro-rs「每次都试两个区」更省。

⚠️ 回写必须走既有的 `set_credential_api_region` 同款路径（过 region 白名单 +
`persist_credentials`），不要新写一套。
⚠️ 分身继承：回写父号后，`for seq in 2..=copies` 建的分身要能拿到（第一轮已有
`new_cred.api_region = Some(probed)` 的回写点与守卫测试
`probed_region_must_be_written_back_before_clone_loop`，照同样位置办）。

### L3 🟠 探测降级为「可选预热」，不再是唯一真相源

探测对了 ⇒ 省第一次的换区往返。探错了 ⇒ **也不致命**，因为 L1 会自纠正。

这直接消解了 §一那个回归的杀伤力：**探测判据错了也不会让号永久废掉**。

⚠️ 但探测判据**仍然要修**（`400` 不能判 `Usable`）—— L3 只是降低它的权重，
不是给它免责。两件事并行做，不互斥。

---

## 四、为什么这套比 kiro-rs 强

| | kiro-rs | 当前 KiroStudio | 本设计 |
|---|---|---|---|
| REST 换区 | ✅ 每次试两个区 | ✅ 有（`rest_api_region_candidates`） | ✅ 保留 |
| **对话换区** | ❌ 无（当凭据问题换号） | ❌ 无 | ✅ **L1** |
| 成功区回写 | ❌ 从不回写 | ⚠️ 只靠探测写一次（可能写错） | ✅ **L2** |
| 探测 | ❌ 完全没有 | ⚠️ 唯一真相源、判据已被证否 | ✅ **L3 降级为预热** |
| 依赖外部脚本 | ✅ 靠 `sync_us.py` 喂 region | ❌ 不依赖 | ❌ 不依赖 |
| 每请求开销 | 可能两次往返 | 一次（若区对） | 一次（自纠正后） |

关键优势：**单二进制自洽**，不需要外部脚本喂 region，也不需要每次试两个区。

---

## 五、实现顺序与文件分区

**必须串行**（都动 `provider.rs` / `token_manager.rs`，而那两个文件长期有并发写）：

1. **先**：探测判据修复（`region_probe.rs`）—— 已派 agent，进行中
2. **后**：L1 + L2（`provider.rs` + `token_manager.rs`）—— 等 shield 合并 agent 释放文件
3. **最后**：L3 只是把探测的失败处置从「禁用」改成「不禁用 + 交给 L1」，
   可能只需改 `service.rs` 几行（`AccountThrottled` 那条已经是这个形状，照它办）

---

## 六、验收（每条都要能构造「移除即失败」的测试）

- L1：构造「从未成功过的号 + region 错配 403」→ 断言换区重试而非换号；
  构造「已成功过的号 + 同样的 403」→ 断言**不**换区（走既有瞬态分支）
- L1 上限：断言同一个号在一次客户端请求内**最多换区一次**
- L2：断言成功区被写进 `api_region` 且持久化被调用
- L2 分身：断言回写发生在分身循环**之前**（照既有守卫测试的位置断言范式）
- **顺序断言**：region 换区分支在 `bearer_invalid_transient` 之后、在 401 之后
- 夹具用**真实链路会产生的串**（去 `provider.rs` 找 `format!` 处），不要自己编

⚠️ 本仓「纸面测试」第 8 种形态：**测了分支内部，没测分支顺序**。
真实事故：改三处、四条测试、三次「回退即 FAILED」全过而修复无效
（一条通用 400 分支排在特化分支之前先 `break` 了）。

---

## 七、已知的坑（都有实测依据，别重犯）

1. **`"All credentials"` 曾被挂在换号标记里** ⇒ 本该听网关 `Retry-After` 真值等 10 秒的、
   套了长阶梯等几十秒（外挂 `kiro_shield.py` 注释记的 2026-08-04 实测）。
   号池**冷却**与**换号空窗**是两回事，判据要分开。
2. **`ABSORB_MIN_USEFUL_ROUND_SECS = 20`** ⇒ `upstreamRetryAbsorbBudgetSecs`
   设成 ≤20 会让吸收层**结构上恒 0 轮**（闸门要求「剩余 > 退避 + 20」）。
   我在 2026-08-06 把它从 45 调到 20，实测线上立刻出现
   `absorb_stop="budget_too_small_for_round" rounds=0 class=PoolCooldown(1)`，已回滚到 45。
   **改这个配置前必须先读那个常量。**
3. **探测失败不要禁用号** —— `ids_needing_region_probe` 过滤 `!e.disabled`，
   一旦禁用**连重启回填都不再重探**。第二轮的 `AccountThrottled` 已是这个形状。
4. `/opt/kirostudio/bin/` 是**只读文件系统** ⇒ OTA 健康标记写不进去 ⇒
   **crashloop 自动回滚失效**。上线后必须人工确认服务健康。
