# 模型感知正向路由 —— 实现级设计

> 状态：实现级设计稿（2026-08-15）。只含设计，不含实现（**待 P1 波次实施**）。
> 上游设计：`docs/model-compat-plan.md` §5.1（P1：巡检 + 正向路由合并设计）+ ISSUES.md (d) 模型路由条目。
> 参考实现：zyphr `cached_model_support`（token_manager.rs:1529）+ `discovery_rank`（:1868-1910）。
> 本文件是 §5.1 的**实现级细化**：文件/函数/字段级落点、与既有机制逐点交互、测试与守卫落地。凡与 §5.1 有出入处均标注「实现级调整」并给理由。
> 代码锚点（已核实，2026-08-15）：`fetch_upstream_models`（passthrough.rs:579）、`select_custom_api_inner`（token_manager.rs:3125，排序键 :3238-3244）、`mark_model_unsupported`（:3317）/`is_model_blacklisted`（:3328，filter 于 :3160）、`MODEL_BLACKLIST_TTL_SECS=30*60`（:1062）、`model_blacklist` 字段（:2133）、`predict_passthrough_upstream_model`（provider.rs:1385，消费 :1611）、`map_target`（model_mapping.rs:57）、`effective_model`（deepseek_normalize.rs:328）、`set_custom_api_config`（token_manager.rs:7383）、后台任务先例（main.rs:432 region 探测 / :466 affinity 清理）。

## 1. 数据源：fetch_upstream_models 现状与缺口

**现状**（passthrough.rs:579，已核实）：

| 能力 | 状态 |
|---|---|
| 候选路径 | 智能剥离（`/anthropic`、`/v1`、`/anthropic/v1` 逐段剥 + `/models` 与 `/v1/models` 双形态，最贴近 base 优先，去重）|
| SSRF | `pinned_streaming_client`（M8：运行时复验 + DNS 固化 + 禁重定向）|
| 响应限流 | `read_json_capped`，`PASSTHROUGH_MODELS_CAP_BYTES = 4MB`（:33）|
| 解析 | `data[].id` / `models[]` / 纯数组三形态，排序去重 |
| 空列表 | 2xx + 空数组 → `Ok(vec![])`（如实返回，:665-693）|
| 失败 | 全部候选失败 → `Err`（错误信息附完整候选清单）|
| 鉴权 | 有 api_key 则带 `Authorization: Bearer`，无 key 裸打（兼容部分无需鉴权的上游）|
| 当前消费点 | 仅 admin 探测：`probe_upstream_models`（admin/service.rs:1368）+ `probe_models_standalone`（:1393）|

**结论：可直接作为巡检数据源，零新增网络代码。** 与 §5.1 第 1 条一致。

**缺口（实现级需补齐的，全部在调用侧而非函数内）**：

1. **调用的凭据来源**：巡检任务需要遍历「全部 custom_api 号」拿到 `KiroCredentials`。数据在 `TokenManager.entries`（token_manager.rs:2133 同 struct 内），需新增 `ids_needing_model_probe() -> Vec<u64>`（返回 custom_api、非禁用、非 deepseek 号；仿 `ids_needing_region_probe` 先例，main.rs:435）。
2. **proxy / tls_backend**：`TokenManager.config()` 已有（admin/service.rs:1378-1383 同款取法），无缺口。
3. **限频**：巡检是 30min 周期 × 4 号 = 分钟级以下低频，上游无感知；函数内 4MB cap 已兜住响应体积。**无需新增限频**。候选 URL 串行尝试（最多 6-7 个）已有。
4. **Kiro 池模型目录**：`fetch_upstream_models` 只服务 custom_api（`probe_upstream_models` 显式拒绝非 custom_api 号，admin/service.rs:1373）。ksk 号（`api_key` 凭据）走 Kiro messages 端点，**仓内不存在任何 ksk 号 `/models` 拉取代码**——Kiro 池没有模型目录概念（见 §6）。

## 2. 三态缓存结构

**实现级调整（相对 §5.1 第 2 条）：缓存形态从「per-(id, model) 状态表」改为「per-id 目录条目」（zyphr 形态）。**

§5.1 建议 `HashMap<(credential_id, normalized_model), SupportState>`；实现级推荐：

```rust
// src/kiro/model_support_cache.rs（新模块，或直接放 token_manager.rs —— 见下）
struct ModelCatalogEntry {
    models: Vec<String>,          // 上游目录原样（已排序去重，fetch_upstream_models 返回）
    refreshed_at: std::time::Instant,
}
// TokenManager 新增字段（与 model_blacklist :2133 同款 parking_lot::Mutex 形态）：
model_catalog_cache:   parking_lot::Mutex<HashMap<u64, ModelCatalogEntry>>,
model_catalog_locks:   parking_lot::Mutex<HashMap<u64, Arc<tokio::sync::Mutex<()>>>>, // 单飞
model_catalog_backoff: parking_lot::Mutex<HashMap<u64, BackoffState>>,               // 退避
```

**为什么目录形态优于状态表**：

1. **条目数**：状态表 = 号数 × 请求过的模型数（每个新模型名一个条目）；目录 = 号数。线上 4 号 × 几十个模型名，状态表几十上百条目 vs 目录 4 条目。
2. **TTL 语义自然落在目录上**：一个号一个 `refreshed_at`，30min 到期整体重新巡检；状态表每个模型单独 TTL，到期时间参差，巡检调度要逐个对齐。
3. **判定是线性扫描不是查找**：`models.iter().any(|m| m.eq_ignore_ascii_case(target))`（zyphr :1541 同款），目录几十个名字，每选号一次扫描成本可忽略（在 entries 锁临界区内，但比 predict/白名单判定的成本低一个量级）。
4. **空列表语义天然正确**：空目录不写缓存 ⇒ 无条目 ⇒ 查询恒 Unknown，不需要单独的状态位。

**SupportState 枚举**（zyphr :1113 同款，放新模块）：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelSupport { Confirmed, Unknown, Unsupported }
```

**查询函数**（签名同 zyphr :1529）：`fn model_support(&self, id: u64, target: &str) -> ModelSupport`：
- 无缓存条目 / 条目过期（`refreshed_at.elapsed() > MODEL_CATALOG_TTL`）→ `Unknown`；
- 目录含 target（`eq_ignore_ascii_case`）→ `Confirmed`；
- 目录明确不含 → `Unsupported`。

**TTL**：`MODEL_CATALOG_TTL_SECS = 30 * 60`（对齐 `MODEL_BLACKLIST_TTL_SECS`，同一常量同量级，注释互相引用）。过期条目在查询时惰性判 Unknown，由后台任务下次巡检覆盖写。

**写入时机 —— 唯一写入源是巡检成功**。**失败分类反馈（provider.rs:1971 `mark_model_unsupported`）不写正向缓存**，理由：

1. 负向证据已有专属通道（黑名单 30min 跳过），语义等价「临时 Unsupported」且优先级更高（见 §4）；写正向缓存是第二条重复路径。
2. 双写路径制造 TTL 语义分叉：黑名单 30min 过期自愈 vs 缓存 Unsupported 何时清除，两套时钟要协调。
3. 单一写入源让缓存内容完全可预测、可测试（三态转换测试只面对巡检一条路径）。
4. 黑名单键是**原始名**（现状），正向键是**改写后名**（见 §4），失败反馈写正向缓存还要额外做键换算，纯复杂度。

**失效（清缓存挂点，全部在 token_manager.rs）**：
- `set_custom_api_config`（:7383）：base_url/api_key 变更 → 清该号目录 + 重置退避 + 移除单飞锁（换上游后旧锁无意义）。
- `set_credential_deepseek_normalize`（:7023）：置 true → 清缓存（该号停止巡检）；置 false → 清缓存（下次巡检恢复）。
- `add_credential` / `delete_credential`（:7985 / :8394、:8418）：新增号无需清（无条目），删除号清掉防内存残留。
- 统一收口为 `fn invalidate_model_catalog(&self, id: u64)`（zyphr `remove_model_cache` :1511 同款），上述四处调用。凭据禁用（`disabled`）不清缓存（缓存无害，巡检循环跳过禁用号即可）。

**模块归属**：结构与查询函数放 `src/kiro/model_support_cache.rs`（§5.1 建议名），但 TokenManager 字段 + 查询/清缓存方法放 token_manager.rs（`select_custom_api_inner` 与失效钩子都在该文件，字段不跨文件则零 pub 接口面）。新模块只放 `ModelSupport` 枚举 + `ModelCatalogEntry` + 纯判定函数（`support_for(target, &entry) -> ModelSupport`，无状态可单测）。这个分工避免 TokenManager 对外暴露缓存 Mutex。

## 3. Unsupported 的处理语义：support_rank 与排序键融合

**实现级澄清（修正 §5.1/守卫 #8 的措辞歧义）：support_rank 前插进的是「透传池排序键」，不是 Kiro 主路径 12 位键。**

透传池排序键现状（token_manager.rs:3238-3244，5 位）：

```rust
(
    e.credentials.priority,     // ① 优先级（池内首选）
    ramp_tier,                  // ② 爬坡压力档
    rpm_of(e.id),               // ③ 近 60s RPM
    model_calls_of(e.id),       // ④ 该模型近期调用数
    e.inflight.load(...),       // ⑤ 在途
)
```

前插后（6 位）：

```rust
(
    support_rank,               // ① NEW support_rank（Confirmed=0 / Unknown=1 / Unsupported=2）
    e.credentials.priority,     // ② 原① 优先级
    ramp_tier,                  // ③ 原② 爬坡
    rpm_of(e.id),               // ④ 原③
    model_calls_of(e.id),       // ⑤ 原④
    e.inflight.load(...),       // ⑥ 原⑤
)
```

**support_rank 求值（在 min_by_key 闭包内，每候选一次，与既有键同款快照）**：

```rust
// 对非 deepseek 号：目标名 = 改写后名（映射链）；deepseek 号短路恒 Unknown。
// 与 predict_passthrough_upstream_model 同源但只取映射层（deepseek 号不巡检，
// normalize 层对缓存查询无意义）——见 §4「预判名参与判定」。
let support_rank = if e.credentials.deepseek_normalize == Some(true) {
    1u8 // 不巡检 → 缓存恒空 → 恒 Unknown，短路省一次 map_target
} else {
    let target = if e.credentials.model_mapping_exempt == Some(true) {
        m
    } else {
        crate::kiro::model_mapping::map_target(m, &mapping_rules).unwrap_or(m)
    };
    match self.model_support(e.id, target) {
        ModelSupport::Confirmed => 0,
        ModelSupport::Unknown => 1,
        ModelSupport::Unsupported => 2,
    }
};
```

（`mapping_rules` 在 `select_custom_api_inner` 内 `self.config().model_mapping.clone()` 取一次，与 :3142 取 `global_ds` 同款。）

**为什么 key 排序（而非加权）**：与 Kiro 主路径 `health_tier` 同风格（小整数升序档位，:3884），排序键可测、可断言、无魔法系数。

**「全候选 Unsupported 退化放行」——判定点即实现点：不加任何判定**。Unsupported 号**留在候选集里**（filter 不排除），min_by_key 总会选出最前的号（Unsupported 只是排最后）。只要 filter 里不加 `!= Unsupported` 条件（刻意不学 zyphr :1725/:1775 的过滤语义），退化放行自动成立。测试钉死这一点（§8 验收 3）。

**「第 5 位 ramp_tier 前后」问题的答复**：support_rank 在**首位**（priority 之前）。这是 §5.1 既定且评审确认的有意行为变化：目录确认的号优先于配置优先级——注意**优先级（priority）语义被降为第 2 位**，用户「这个中转站优先」的显式配置在 Confirmed 号面前让位。权衡：
- 倾向支持（设计稿已定）：正向证据（目录）比静态配置（priority）更接近「该号能服务该模型」的事实，这正是本功能的出发点。
- 兜底：若线上观察到「用户显式 priority 的号长期选不到」，可把 support_rank 移到 priority 之后（一行改动 + 排序测试更新），文档预留此开关。

## 4. 与黑名单 / 严格语义 / 白名单的交互

**优先级总序（filter 先于排序，天然分层，零冲突）**：

```
filter 层（候选集）:  禁用 → 冷却 → exclude → 黑名单(原始名) → 白名单(硬门)
sort 层（排序键）:    support_rank(改写后名) → priority → ramp → rpm → ...
```

**黑名单（负向，运行时事实）vs 正向缓存**：黑名单在 filter（:3160）先执行——黑名单命中的号**直接出局**，support_rank 根本看不到它。**黑名单 > 正向缓存**，与 §5.1 第 7 条一致。P1 阶段两者键空间不同（黑名单=原始名现状、正向=改写后名），但**不冲突**：黑名单是「该号出局」的硬判定（子集），正向是「剩余候选排序」的软偏好，判定先后天然隔离。P2 黑名单键切改写后名（model-compat-plan §6.5）后键空间统一，本设计键空间已提前定好，切换零迁移（守卫 #2 精神）。

**严格语义（effective_model）**：不改写链、不碰 `effective_model` 本体。正向判定复用的是改写链的**预测函数**（predict / map_target），只读、无副作用。选号层的正向判定与改写层的实际改写是「预测 vs 事实」关系：P0 测试矩阵已钉死「predict == 改写」逐分支对应（provider.rs:7852+），P1 复用同一函数族，无新分叉面。

**「deepseek normalize 号的预判名是否参与正向判定」——实现级答复**：
- **deepseek 号（`deepseek_normalize == Some(true)`）不参与**：巡检跳过（§5），缓存恒空，`model_support` 恒 Unknown，support_rank 恒 1。`predict_passthrough_upstream_model` 的 normalize 分支在选号处**不被调用**（短路，见 §3 代码）。理由：这类号的 OpenAI 形态 `/models` 目录列的是原生名（deepseek-chat 等），而请求被 normalize 改写后的目标名（fallback_model，可配任意值）与目录对应关系不可预测，目录数据会误导判定（ISSUES (d) ⑤ 评审补强）。
- **非 deepseek 号**：预判名（改写后名 = `map_target` 结果，exempt 号回落原始名）**参与正向判定**——这是本设计键空间的核心决策：请求实际打到上游的名字才与目录可比。若用原始名判定，映射过的请求（claude-opus-5 → gpt-5.6-sol）会在 pigcode 类目录里永远查不中（目录列 gpt-*，没有 claude-*）。

**白名单**：硬门在 filter（:3161-3190，`is_entry_selectable` 层语义），先于排序。正向缓存不改变白名单语义——白名单外的号照旧被过滤，白名单内的号再按 support_rank 排序。白名单判定对 deepseek 号用 `effective_model`、普通号用原名（:3174-3189 现状），与正向判定键（改写后名）不同**但正交**：白名单是「授权」硬门，正向是「能力」软偏好，各司其职（model-compat-plan §6.5 已论证黑名单与白名单键空间不同是有意为之，同理适用于正向缓存）。

**吸收层**：不参与选号，零交互。

## 5. 巡检调度

**任务形态**：main.rs `tokio::spawn`（region 探测 :432 同款：启动延迟 10s 后跑首轮，避免与 token 预刷新抢上游往返；然后 30min ticker，`MissedTickBehavior::Skip`，affinity 清理 :466 同款）。

```rust
// 伪代码（实现级）
let mut ticker = tokio::time::interval(Duration::from_secs(30 * 60));
ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
tokio::time::sleep(Duration::from_secs(10)).await;   // 首轮延迟（region 探测同款）
probe_round().await;
loop { ticker.tick().await; probe_round().await; }

// probe_round：
let ids = tm.ids_needing_model_probe();   // custom_api && !disabled && !deepseek_normalize
for id in ids {
    if tm.model_catalog_in_backoff(id) { continue; }      // 退避中：本轮跳过（状态留到到期）
    let _lock = tm.model_catalog_lock(id).await;          // 单飞：per-id TokioMutex
    let cfg = tm.config();
    let proxy = cfg.proxy_url.as_deref().map(ProxyConfig::new);
    match fetch_upstream_models(&cred, proxy.as_ref(), cfg.tls_backend).await {
        Ok(models) if !models.is_empty() => {
            tm.store_model_catalog(id, models);           // 写缓存 + diff 日志 + 重置退避
        }
        Ok(_empty) => { /* 空列表：不写缓存（保持 Unknown），退避不变。下周期再探 */ }
        Err(e) => { tm.bump_model_catalog_backoff(id); tracing::warn!(...); }
    }
}
```

**决策与理由（对齐 §5.1 第 3-4 条 + 评审补强）**：

| 项 | 决策 | 理由 |
|---|---|---|
| 周期 | 30min（对齐 NewAPI 巡检 + 黑名单 TTL 同量级）| 目录变更滞后最多 30min，与负向证据 TTL 同尺度；低频不扰上游 |
| 单号单飞 | per-id `Arc<tokio::sync::Mutex<()>>`（zyphr `model_refresh_lock` :1521 同款）| 后台循环本身串行天然无并发；锁防未来多路触发（admin 手动刷新按钮、双任务）时重复打。锁粒度 per-id 而非全局：一个号慢不阻塞其它号 |
| 失败退避 | 指数：1min → 2min → 4min → 上限 30min；成功（非空 2xx）重置 | 网络/解析错误连续累计；退避期间该号维持 Unknown（=现状排序，不惩罚）——**退避状态不进排序判定**（§5.1 第 4 条） |
| 空列表 | 不写缓存、不算失败、不重置退避 | 空列表可能是上游暂时故障；固化成「无模型」会让目录失真永久化（守卫 #6）。保持 Unknown 最安全；退避不重置 = 下周期照常再探，不引入额外惩罚 |
| 失败日志 | `tracing::warn!` 带 credential_id + 错误 | 与黑名单日志同风格 |
| deepseek 跳过 | `deepseek_normalize == Some(true)` 的号不在巡检列表 | 目录语义不同（§4）；判据用 per-credential 字段，不依赖全局配置 |

**diff 日志（对齐 §5.1 第 8 条合并点）**：`store_model_catalog` 内与旧目录对比，新增/移除模型记 `tracing::info!`。**P1 不做通知**（alerting 通道默认关的接线留 P2，避免 P1 增加未用配置项）；移除模型不清黑名单（黑名单 30min TTL 自愈，§5.1 第 8 条已定）。租约不引入（单实例，singleflight 即等价物，§5.1 第 8 条已定）。

**不在选号热路径触发巡检**（评审补强）：`model_support` 查询是纯内存读，绝不因 Unknown 触发 fetch。zyphr 是路由时刷新（`cached_or_refresh_models_for` :1596），我们刻意不学——8GB 机器 + 高并发下选号路径任何网络等待都不可接受，且单实例后台任务周期已覆盖。

## 6. Kiro 池 vs 透传池

**铁律：正向路由只做透传池。**

- **巡检范围**：`ids_needing_model_probe()` 只返回 `is_custom_api_credential()` 的号。ksk 号（`api_key` 凭据）走 Kiro messages 端点，**仓内无任何 ksk 号模型目录拉取代码**（`fetch_upstream_models` 的调用方 `probe_upstream_models` 显式拒绝非 custom_api，admin/service.rs:1373）——Kiro 池没有 `/models` 概念，模型集由 Kiro 账号订阅档决定（不可枚举）。
- **排序键**：`support_rank` 只存在于 `select_custom_api_inner`（透传池函数）。Kiro 主路径 12 位排序键（:3880-3893）**零改动**——这也修正了 §5.1 守卫 #8「后续 12 位键语义不动」的措辞：12 位键在 Kiro 主路径，support_rank 根本不在那个函数里，不存在「前插」问题；透传池后续 5 位键语义不动。
- **缓存数据**：`model_catalog_cache` 键为 credential id，custom_api 号 id 与 ksk 号 id 同空间但巡检只写 custom_api 号，ksk 号查询不存在（select_custom_api_inner 只处理 custom_api）。
- **跨池仲裁**（`should_try_custom_api_first`，handlers.rs:2043）不感知模型目录，维持现状——两池优先仲裁是配置语义，正向路由是池内排序，正交。

## 7. 工作量拆分（每步独立 CI）

| 步 | 内容 | 测试策略 | 风险 | 工时 |
|---|---|---|---|---|
| **S1** | 新模块：`ModelSupport` 枚举 + `ModelCatalogEntry` + 纯判定函数 `support_for`；TokenManager 三个字段 + `model_support` 查询 + `invalidate_model_catalog` | 纯单测：三态转换（目录含/不含/无条目/条目过期）、大小写不敏感、空目录恒 Unknown。**零网络、零行为变化**（无写入者）| 低（纯新增）| 0.5 人日 |
| **S2** | support_rank 接入 `select_custom_api_inner` 排序键（6 位）| 行为单测：Confirmed 号先选（构造目录后 select 断言）；Unknown 维持原排序；Unsupported 压后但**仍可选**（全 Unsupported 退化放行）；黑名单命中仍出局（黑名单>正向）；deepseek 号恒 Unknown 短路；map_target 改写后名参与判定 | 中：排序键是热路径，闭包内加 map_target + 锁查询（parking_lot Mutex 短临界，与 model_blacklist 同款）| 0.5 人日 |
| **S3** | 后台巡检任务：`ids_needing_model_probe` + 单飞锁 + 退避 + 空列表不写 + 写缓存 + diff 日志；main.rs spawn 接线 | 函数级提取 `probe_round` 接受注入的 fetch 闭包（`async Fn(&KiroCredentials) -> anyhow::Result<Vec<String>>`），单测注入 mock：退避增长/成功重置、空列表不写缓存、失败维持 Unknown、单飞（并发调 probe_round 两次只打一次网络——mock 计数）、deepseek 跳过 | 中：后台任务与热路径并发（缓存写 vs 排序读）——parking_lot Mutex 原子性已保证；fetch 失败拖慢周期（退避兜住）| 1 人日 |
| **S4** | 失效钩子四处（set_custom_api_config / set_credential_deepseek_normalize / add / delete）+ 守卫注释 | 单测：改 base_url 后旧目录立即失效（Unknown）；deepseek 开关翻转清缓存 | 低 | 0.25 人日 |
| **S5** | 文档 + 守卫收口 + 线上对照 | 守卫 needle（CURRENT.md 纪律）；观察期指标见 §8 | 低 | 0.25 人日 |

总计 **2.5 人日**（落在 ISSUES (d) 的 2-3 人日区间）。S1/S2 可同批提交（无巡检也是安全行为变化）；S3 独立提交；S4/S5 收尾。**S1+S2 先落地**（纯离线、零网络、可独立验证排序语义），S3 巡检随时可加。

## 8. 验收标准

**单测（S1-S4 内嵌，全部 `cargo test --no-default-features` 走服务器验证循环）**：

1. 三态转换：目录含 → Confirmed；不含 → Unsupported；无条目/条目过期 → Unknown。（S1）
2. TTL：条目过期后查询恒 Unknown（不依赖真实时间，用可控 elapsed 或直接构造过期条目）。（S1）
3. 全 Unsupported 退化放行：目录全不含目标模型时 select 仍返回号（不返 None、不落 Kiro）。（S2）
4. 黑名单优先：黑名单命中的号即使 Confirmed 也出局（filter 先于排序）。（S2）
5. support_rank 排序：Confirmed < Unknown < Unsupported；同档内原有 5 键语义不变（priority 仍生效于同档）。（S2）
6. deepseek 号不巡检（S3）、select 时短路恒 Unknown（S2）。
7. 空列表不写缓存：mock fetch 返空 → 缓存无条目 → Unknown。（S3）
8. 退避：连续失败 1→2→4→30min 上限；成功（非空）重置。（S3）
9. 单飞：并发两次 probe_round 只打一次网络（mock 计数）。（S3）
10. 失效钩子：改 base_url → 旧目录失效。（S4）

**守卫（源码级，对齐 CURRENT.md:30 守卫清单纪律：needle 运行时拼接、测试段不写被守卫代码完整字面量）**：

- 守卫 #8（support_rank 首位）：`test_support_rank_outranks_priority_in_passthrough_sort_key`（命名对齐 `test_ramp_tier_outranks_inflight_in_sort_key` 先例，CURRENT.md:47）——行为断言「目录 Confirmed 的号先于 priority 更优的 Unknown 号」。注释在排序键上方写明 6 位键结构。
- 守卫 #6（空列表不写缓存）+ #7（退化放行）：对应单测 7、3 即守卫本体。
- 两池隔离守卫：Kiro 主路径 12 位排序键测试（:13904 既有）不因本改动变色；透传池排序键测试断言 support_rank 存在。注释注明「support_rank 只进透传池，Kiro 主路径 12 位键不碰」。

**线上对照（nbus 部署后观察期，24-48h）**：

| 指标 | 期望 | 失真检测 |
|---|---|---|
| Confirmed 号占总选号比例 | 显著 > 0（巡检首轮后）| 若恒 0 → 巡检未跑通，查日志 |
| Confirmed 号失败率 vs Unknown 号失败率 | ≤ Unknown 号 | 若明显更高 → 目录失真（广告位≠承诺），靠黑名单兜底 + 检查 TTL |
| 黑名单命中（`upstream_says_model_unsupported` 日志）频率 | 不升（预期持平或降）| 若升 → support_rank 把目录确认的号集中打爆，检查 Confirmed 号集合 |
| upstream_trace 按 credential_id 的 400/404 分布 | Confirmed 号不应异常 | 同上 |

## 9. 自 review：与既有机制零冲突论证

- **黑名单**：分层（filter 出局 vs 排序偏好），黑名单优先，语义不变；P1 键空间不同但判定先后天然隔离；P2 键切换零迁移（键空间已定）。
- **严格语义（effective_model）**：本体与改写链零改动；正向判定复用 predict/map_target 只读函数；P0 测试矩阵已钉死「predict == 改写」。
- **白名单硬门**：filter 层先于排序，巡检不触碰 `allowed_models` 判定；白名单（授权）与正向（能力）键空间不同是有意为之（同 §6.5 黑名单论证）。
- **两池隔离**：巡检范围、排序键、缓存数据三层都只在透传池；Kiro 主路径 12 位键零改动；跨池仲裁不感知目录。
- **吸收层**：不参与选号与模型名链路，零交互。
- **新口径分叉**：正向键 = predict 改写后名（单一来源，守卫 #2 精神）；失败反馈不进正向缓存（单一写入源）；normalize_model_key 函数 P1 不引入（目录形态下无「键归一化」消费点，eq_ignore_ascii_case 判定已大小写不敏感——这是对 §5.3 的实现级简化，P2 与黑名单键切换同批引入，一次切换一次验证）。
- **热路径成本**：排序闭包新增一次 map_target + 一次锁内线性扫描（几十条）；与既有 rpm/ramp 批量预取相比可忽略；锁为 parking_lot 短临界（与 model_blacklist 同款）。

**遗留风险（诚实披露）**：① 目录是广告位不是承诺，Unsupported 压后语义依赖上游列表完整性——已三重兜底（压后不跳过 + 30min TTL 重查 + 黑名单运行时证据优先），观察期盯 Confirmed 失败率；② support_rank 压过 priority 是有意行为变化，线上观察 priority 语义是否被过度稀释，必要时一行降位；③ P1 黑名单键（原始名）与正向键（改写后名）并存，期间「黑名单命中但正向 Confirmed」的组合出现时黑名单赢（安全方向）。

## 10. 一句话结论

**做**：S1+S2（缓存 + 排序接入，纯离线零网络）先行落地，S3（巡检任务）紧随——三态缓存与 support_rank 是安全行为变化且每步独立 CI，整个 P1 约 2.5 人日。
