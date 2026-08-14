# 对抗性审查：三份规格

> 三份规格在交给我时**都被截断**（后端停在 `clone_group: e.credentials.clone_group`、前端停在 `CredentialStatu`、shield 停在 §2 论证第 2 条）。P4（TIER3 热更）、后端 §5（扇出实现）、前端 §2.1 之后、以及**用户诉求 #4（推号 + 自动分身）的规格整节**我都没看到。下面只审我实际读到的内容 + 已核实的代码。

---

## 1. 回滚安全

`KiroCredentials`（credentials.rs:38-39）有 `rename_all="camelCase"`、**无** `deny_unknown_fields`；`Config` 同样无。旧二进制读到 `cloneGroup`/`cloneSeq` 会静默忽略 → `CredentialsConfig::load` 不报错、不 `exit(1)`。**「读得进去」这条防守成立。**

但**写回**会毁数据：旧版本任何一次 `persist_credentials()` 都会把 credentials.json 整体重写成不含这两个字段的形态。前滚后：
- api_key 分身的**分组**能自愈（`account_key` 从 key 哈希现算），
- 但 `clone_seq` 永久丢失 → 「一键删除分身」的唯一判据 `clone_seq.is_some()` 全 false → 按钮全禁用（若代码退化成"组内除最小 id 外全删"，则会删错号）。

**严重度：中。** 修法：启动 reconcile 时按（同 `api_key_hash` + 不同 `machine_id` + id 较大）回填 `clone_seq`，或在规格里显式承认"回滚一次即失去分身标识、需重新生成"。

### 1b 🔴 `cred:{parent_id}` 会被 id 复用撞上

`next_id` 是 `AtomicU64::fetch_add`（5786），进程内不复用；但**重启时** `next_id = max_existing_id + 1`（1807）再 `fetch_max(max_trash+1)`（2028）。若父号恰是最大 id、被删**且回收站清空**，重启后 id 被重新发放。新号（与旧组毫无关系）的 `account_key` = `cred:{同一个 id}` → **继承孤儿分身的组**：共享余额缓存、被「删除本组全部分身」扫走。线上 id 已从 #404 走到 #438，这不是理论场景。

**严重度：高。** 修法：`next_id` 起点改为持久化的单调水位（写进 stats 文件），或 OAuth 号的组键改用 `refresh_token` 首次入池时的哈希冻结值而非 id。

---

## 2. `#[serde(default)]` 完整性

6 个新配置项全部带 `#[serde(default)]` 或 `default = "fn"` —— **这条防守成立**，线上既有 config.json 不会加载失败。

真缺口在**镜像一致性**。config.rs:1008 那条测试写得很清楚：默认值散落**三处**（`default_x()` / handlers 的 TIER3 static 初值 / `ConfigSnapshotResponse::default`），历史上长期不一致。规格给 6 个字段 × 3 处 = **18 个可写错的点，零个守卫测试**。而 `ConfigSnapshotResponse::default` 是手写字面量，不走 `default_*()`。

失败场景：`ABSORB_ENABLED` static 初值写成 `true`，则**所有绕过 `create_router_with_provider` 的路径**（即全部 handler 单测）都开着吸收 —— 规格自称的"`enabled=false` 逐字节等价旧行为"在测试里不成立，而这正是历史事故的形态。

**严重度：中。** 修法：至少给 `absorb_enabled` 抄一份 `cc_auto_buffer_default_is_on_and_consistent_across_mirrors` + handlers 侧的 `..._static_matches_config_default`。

**socks_nodes.json 的 at-rest 加密**：`secret_store` 密钥是机器绑定的。换机 / 重建 VPS → 节点文件解不开。credentials.json 那条路径失败是 `exit(1)`（响亮）；socks 节点规格必须明写 fail-open（warn + 空列表），否则面板节点静默清零。低-中。

---

## 3. 🔴 吸收层会复现「一个请求压死整池」——且比 shield 更糟

已核实数字：`compute_max_retries(5,_) = min(5×3, 12) = 12`；`MAX_REQUEST_RETRY_BUDGET_SECS = 45`；`MAX_TRANSIENT_WAIT_SECS = 20`；`inbound_queue_max_wait_secs = 30`。

**致命点：`acquire_admission()` 在 `call_api_with_retry` 内、failover 循环外（provider.rs:810）—— 即「每个客户端请求只过一次」这个不变量，是靠"一个客户端请求 = 一次 `call_api_stream`"成立的。吸收层重打整个 `call_api_stream`，直接打破它。**

后果链（300 并发 / 账号真实 134 RPM / 5 分身）：

1. 单客户端请求最多 5 轮 → **消耗 5 个入站令牌**、最多 5×12 = **60 次上游调用**。
2. `throttle-autotune` 按号数算 `inboundTargetRpm = 5×72×80% = 288`。300 客户端 × 5 轮 = 最多 **1500 次准入/分钟 vs 288 个令牌** → 桶恒空。
3. 桶空 → 每轮排队满 `30s` 后 bail，错误串是
   `"入站限速排队超时(网关目标 N RPM 保护上游)retry_after_secs=N"`（provider.rs:812）。
4. **`absorb_class_of` 把它判成 `PoolCooldown` → 可重试** —— 因为它与全池冷却**共用 `retry_after_secs=` 标记，两者字符串上不可区分**。
5. 于是吸收层**重试网关自己的背压信号**。2 轮 × 30s = 60s 预算耗尽 → 客户端等 60s 才拿到 429，而今天是 <2s 拿到 429 后自己退避。

这就是 shield 的 p50 73.2s 的成因被搬进网关。规格把 `MIN_DELAY` 从 1.0s 放到 150ms，只是让这个循环转得更快。

**AIMD 二次放大（确证正反馈）**：每次上游 429 都 `report_upstream_429`；吸收层把观测到的 429 放大约 5×，而 `maybe_step_up` 要求 20s 静默。已知事实是每 6.4s 就有一次 429 → 降档更快 → 目标 RPM 更低 → 准入超时更多 → 吸收轮次更多。闭环。

**严重度：致命。必须修三处才能实施：**
- (a) 入站准入超时**换独立标记**（如 `inbound_admission_timeout=1`），`absorb_class_of` 显式返回 `None`；
- (b) 吸收重试**不得重新走 `acquire_admission`** —— 要么把重试下沉到 provider 内（准入闸门之下），要么传 `already_admitted` 标志；
- (c) 预算判据改为 `剩余 >= 一轮最坏耗时`（45s + 20s）而非 `剩余 >= min_delay`，否则第 2 轮必然在半路被 deadline 砍断、白打一轮上游。

---

## 4. 403 吸收

**默认 `false` —— 这条防守成立。** 规格的三条论证（窗口 10min ≫ 60s 预算、成功率≈0、自愈退避的实测教训）我认。

但**开启后与自愈退避确实打架**，且比规格说的更严重：
- `SELF_HEAL_BASE_BACKOFF=60s` / `MAX=900s`（token_manager.rs:762-764）存在的意义就是**停止**向刚 403 的账号试探；吸收层在 15s 内重试同一账号，直接抵消它。
- **5 个分身是同一个账号** → 403 是账号级 → 12 次换号只是把同一个被惩罚账号走 12 遍。开着 `absorb_suspended` 时单请求最坏 **60 次探测打进正在惩罚的窗口**。
- 还有一条规格没提的：`report_upstream_pressure`（3197）把 suspend 也喂给 AIMD。吸收放大后的 suspend 被重复计数 N 次 → 降档也放大 N 倍。

**严重度：高（仅在开关被打开时）。** 修法：`absorb_suspended=true` 时把额外轮次**硬钉为 1**，并在 UI 文案里写明"与自愈退避冲突"，而不只是"不建议"。

---

## 5. 自动分身默认关闭 —— **无法确认**

我拿到的三份规格里**完全没有**用户诉求 #4（推号内置 + 设置开关 + 可选自动分身）的任何内容。没有 `autoClone` 字段、没有 `parse_import_keys_request` 的改动、没有配置项。**这一整节缺失。**

而且这里 serde 默认值**帮不上忙**：`import_keys` 走的是**手写解析器**（handlers.rs:427-439 的 `parse_import_keys_request`，注释说明刻意不用 derive）。所以"字段缺失 → false"必须手写 `unwrap_or(false)` 并**用测试钉死**。同时 `/api/import/keys`（router.rs:177）是外部 kiro-accounting 推号入口，那条路径**永远不会带** `autoClone` 字段 —— 若默认取 `true`，外部推号会静默给每个新号造 16 份分身。

**严重度：致命（缺规格）。** 实施前必须补：字段名、默认 false、两条路径各一个"缺字段 → 不分身"的测试。

---

## 6. 余额缓存兼容性 —— 兼容性成立，但**收益不成立**

规格**否决**改 key、改走写侧扇出 → 磁盘格式不变，无 panic 无 bail。即便真改了 key，`load_balance_cache_from`（service.rs:3038）的 `k.parse::<u64>().ok()?` 在 `filter_map` 内，非数字 key 只是静默丢弃。**这条防守成立。**

问题是扇出**两个宣称收益都拿不到**：

**(a)「上游探测从 N 次降到 1 次」—— 假的。** `refresh_all_balances_gently`（service.rs:901-940）对每个未禁用 id **直接调 `fetch_balance(*id)`，完全绕过 `balance_cache`**。5 个分身每 30 分钟仍打 5 次 `web_portal`。而规格把"减少重复探测=降风控"列为**主要收益**。修法：那个循环的 `ids` 必须按 `account_key` 去重。

**(b)「消除面板 5 个不同百分比」—— 只能维持到下一个请求。** `get_cached_balances`（service.rs:865-880）在返回前叠加**按 id** 的乐观修正 `used_now[id] - baselines[id]`。扇出把同一份真值写进 5 个 id 后，每个 id 再各自加上自己的 delta → **又变成 5 个不同的 `usage_percentage`**，正是用户截图里的现象。修法：乐观修正必须按 `account_key` 聚合（同组 delta 求和后整组只加一次）。

**(c) 调度侧被污染。** `BalanceResponse.id` 是结构体字段（types.rs:433），扇出复制时若不重写，兄弟条目里带着源 id。更要紧的是 `push_balance_snapshots_to_scheduler`（service.rs:950+）按 id 逐条建 `BalanceSnapshot` → 5 份**相同的 `remaining`** 喂给 `balance_factor` → 余额加权分流认为池子有 5 倍余额。这与「5×167=835 虚高」是同一类错误，只是换了个维度。必须在回推处按 account 去重或均分。

---

## 7. 可测试性 / 哪些测试其实测不到东西

**(a) 🔴 P1-4 的 anchor 不存在。** 规格说改 `CredentialSnapshot`；仓里没有这个类型，只有 `CredentialEntrySnapshot`（token_manager.rs:1396）、`TrashSnapshot`、`ManagerSnapshot`、`BalanceSnapshot`。patch 套不上。

**(b) 🔴 后端规格完全没碰 `CredentialStatusItem`（types.rs:24 / 构造在 service.rs:384）**，而那才是面板凭据列表实际收到的类型。前端规格依赖的 `cloneGroup`/`cloneSeq` 在后端规格里**无来源**。"一次性完美交付"直接不成立。

**(c) `account_key` 与既有 `api_key_hash` 重复。** 后者是同一个 key 的**完整** sha256，已经在 `CredentialEntrySnapshot:1426`、`CredentialStatusItem:145`、以及前端两份类型（api.ts:39/78）里下发了。再造一份 16-hex 截断哈希 = 同一身份的两套派生 = 必然漂移。更省的做法：`acct:{api_key_hash[..16]}` 直接由既有字段算，或干脆用 `api_key_hash` 当组键、不上 `account_key` 到 wire。

**(d) `account_key` 对 key 轮换非幂等。** `add_credential` 路径 5818 行 `validated_cred.kiro_api_key = new_cred.kiro_api_key` 会覆写 key。父号换 key → 父号 `account_key` 变、分身仍持冻结旧值 → **组分裂、余额又发散、删分身不再匹配**。规格没有覆盖这个用例；照规格写的"轮换父号 key 后组仍自洽"测试会 FAIL。

**(e) `should_agree_with_renderer` 是弱测试。** `absorb_class_of` 与 `map_provider_error_inner` 是同 4 个谓词的**两份独立拷贝**，还各自复制了一遍 `retry_after_secs=` 的解析代码。喂 N 个样本串比对两者是真测试，但只能覆盖你列出的串 —— 而本仓历史事故的形态恰恰是"某个 bail 串谁都匹配不上"，这种漂移它看不见。**根治**：让 `map_provider_error_inner` 接收 `Option<AbsorbClass>` 参数，全仓只留一个分类器 —— 消掉漂移面而不是测试它。

**(f) §3 那条不可区分性没有任何测试能救。** 两个语义完全不同的错误（全池冷却 vs 网关自己的入站背压）共用同一个字符串标记，任何分类器都分不开。必须改标记。

---

## BLOCKER 清单（不修不能实施）

1. **吸收层重入 `acquire_admission`** → 单请求吃 5 个入站令牌、把 288 RPM 的桶按 1500/min 抽干。必须下沉到准入闸门之下或传 already-admitted 标志。（§3，致命）
2. **入站准入超时被误判为可吸收** —— 与全池冷却共用 `retry_after_secs=`。必须给准入超时独立标记并显式 `None`。（§3，致命）
3. **P1-4 anchor `CredentialSnapshot` 不存在**，且 `CredentialStatusItem` / service.rs:384 完全未覆盖 → 前端依赖无来源。（§7a/7b，致命）
4. **用户诉求 #4（推号 + 自动分身默认关）整节缺失**；且 `import_keys` 是手写解析器，serde 默认值无效，`/api/import/keys` 外部路径永不带该字段。（§5，致命）
5. **`refresh_all_balances_gently` 未按 account 去重** → "探测 N→1"的主要收益为零，风控风险未降。（§6a，高）
6. **乐观修正仍按 id 叠加** → 扇出后百分比立刻重新发散，用户看到的 bug 没修掉。（§6b，高）
7. **`cred:{parent_id}` 遇 id 复用会把无关新号并进旧组**（重启后 `next_id` 从 max+1 重算）。（§1b，高）
8. **key 轮换使 `account_key` 分裂**，冻结语义与 5818 行的覆写冲突。（§7d，高）
9. **吸收层预算判据**必须按"一轮最坏耗时（45s+20s）"而非 min_delay 检查，否则第 2 轮必被 deadline 半路砍断、白打上游。（§3c，高）

### 防守成立的
- 新增字段的**读**兼容（无 `deny_unknown_fields`，不会 `exit(1)`）— §1
- 6 项 `#[serde(default)]` 完整 — §2
- 403 吸收**默认关** — §4
- 余额缓存磁盘格式不变、不 panic 不 bail — §6
- 吸收点在 `map_provider_error` 返回处，此时客户端确实 **0 字节已提交**（handlers.rs:1103/1461/2118 三处都在响应体构造之前）— 且 `call_api_stream(request_body: &str)` 重放零克隆成本