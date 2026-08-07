# 第二批：key 上号自动识别区域与端点（待第一批落地后执行）

> 状态：待派。与第一批（P0 死循环 / 代理格式 / 行模式 UI / 上号选节点）文件全冲突，
> 必须等第一批实测确认树干净后再动。

## 用户要求（原话归纳）

「不管推进来什么号，只要是个 key，都是自动识别自动选择最优」——
上 key 后自动测 us / eu 两个区，自动选最优端点，探不出就通知；
即使自动识别有问题，内置右键设置也要能手工改。

## 根因：探测的是 A 域名，决定的是 B 域名

- `region_probe::probe_api_region` 用 `token_manager::get_usage_limits()` 当判据
  → 打 `management.{region}.kiro.dev`
- 结论写进 `api_region`
- 而 `api_region` 的**唯一消费者**是 `CliEndpoint::host()` = `q.{region}.amazonaws.com`
  （`src/kiro/endpoint/cli.rs:48`）

三个连锁后果：

1. `PROBE_ORDER` 只有 2 项（`region_probe.rs:99`），注释理由是
   「management/runtime 只在 us-east-1 与 eu-central-1 解析 DNS」——
   那是 **kiro.dev 的约束**，对 `q.*.amazonaws.com` 不成立。
   只在第三个区授权的 `ksk_` 号会被判 `NoUsableRegion` 而保持禁用。

2. `get_usage_limits` 自带 403 换区回退（`token_manager.rs:645`，
   `rest_api_region_candidates` 返 2 个候选）后返回 `Ok`。
   → 探 `eu-central-1` 时真正成功的可能是内部回退到的 `us-east-1`，
   而 probe 见 `Ok` 就 `return Usable("eu-central-1")` 并写死 `api_region`，
   分身还继承（`service.rs:1466` 回写块）。
   **用户看到的「默认都是 eu」有一部分是假的 eu。**

3. `region_probe.rs:194` 首个 `Usable` 即 `return` → eu 通就永不测 us。

## 要做成的样子

### 探测矩阵

探测走**该凭据实际会用的那个端点**，复用
`endpoint::for_credentials()` + `api_url()` + `decorate_api()` + `transform_api_body()`，
照 `token_manager.rs:6193` `deep_verify_credential` 的模式 ——
那里已经解决过同一个「别手搓 host」的问题，注释写明了理由
（CLI 号打 IDE 端点稳定 403，而 403 被当「疑似封号」自动禁用 → 健康号被验活弄死）。

|              | q.\*（cli） | runtime.\*（ide） |
|--------------|------------|------------------|
| eu-central-1 | 测         | 仅当 q.\* 两区皆 403 |
| us-east-1    | 测         | 仅当 q.\* 两区皆 403 |

判据沿用现有（`classify_probe_result`）：
- 2xx / **429** = Usable（429 说明打到了正确的区，只是拥堵 —— 这条是承重的）
- 403 = 该组合不通
- 401 = token 本身废了，立即停止别再探
- 5xx / 网络 = Inconclusive，试下一个但不据此判死

**全部测完再选**，不是首个即返。选择依据按实测：q.\* 优先
（300 并发 0 个 429 vs runtime.\* 31%）。

结果**同时**写死 `api_region` 与 `endpoint`，从此该号不依赖任何全局默认值。

### 成本控制

4 次上游往返，而上号路径是**串行在用户的 HTTP 请求里**的（面板会多转一圈）。
故常见路径压到 2 次：先测 q.\* × 2 个区，只在两个都 403 时才试 runtime.\*。

### OAuth 号不进 cli 列

social / idc / M365 号**只探 region、端点固定 ide**。
CLI 端点无条件发 `tokentype: API_KEY` 且绝不注入 profileArn
（`cli.rs:90` / `cli.rs:118-121`，注释明写带上反而 403）。
矩阵只对 `ksk_` 号展开。

## 六项改动

1. **探测改打真实端点**（根因）。不要保留两条并行的探测实现。
2. **端点 × region 矩阵**，全测完再选。扩 `PROBE_ORDER` 时每个新增候选要写清依据。
3. **`get_usage_limits` 的内部回退不得污染归因** ——
   探测路径必须能拿到「实际成功的是哪个区」。
   要么加一个不做内部回退的入口，要么让被调函数回传实际生效的 region。
   选侵入面小的那个，并在注释里写清为什么不能直接信 `Ok`。
4. **notification 补两个 case**：`use-pool-notifications.ts` 的 `disabledReason`
   switch 里没有 `RegionProbeFailed` / `RegionProbeTokenDead`，
   会落 `default` 显示原始英文枚举名。补文案 + 三语 i18n。
   两条的处置动作不同（一条查 region 授权范围、一条查 token 来源），文案要能区分。
5. **右键菜单加「区域 ▸」**，手工值**压过**自动探测结果。
   `effective_endpoint` 的既有语义就是显式字段优先（`credentials.rs:741`），
   region 侧要对齐成同样的优先级。
6. **批量清理禁用凭据、排除代挂**。判据现成：
   `DisabledReason::PassthroughFailed` / `PassthroughOverloaded` 是代挂专属，
   加 `is_custom_api_credential()`。走 `delete_credential`（进回收站可恢复）
   而非 `purge_credential`。注意 `delete_credential` 有「必须先禁用」前置门，
   而清理目标本来就是禁用号，正好。

## 硬约束

- 不要放宽 `is_upstream_temporarily_suspended`（`handlers.rs:554`）的窄判据 ——
  `:548-553` 写明泛匹配 `AccessDeniedException` 会把永久封号吞成可重试。
- 不要动 `classify_probe_result` 里「429 判 Usable 且必须排在 403 之前」这条
  （`region_probe.rs:79` 的 ⭐ 注释 + 承重测试 `throttled_means_region_is_correct`）。
- 不要动 `effective_saturation_limit` 的返回语义。
- 保持「探不出可用 region 就维持禁用」（`service.rs:1496` 的 P0 处置）——
  05:41 那次 15 个号以启用态入池、窗口里打到错区恒 403、3 次即自动禁用、
  每个只跑 1~6 个请求 0 成功；4 分钟后同批 key 探到 eu-central-1 后 881/881 全成功。

## 验收

- `cargo test --no-default-features` 全绿 + `clippy` 0 error + `pnpm tsc --noEmit` 干净
- 回归测试覆盖：探测走的 host 是 q.\* 而非 management.\*（可断言 URL 构造）；
  「内部回退成功不得归因给被探 region」；OAuth 号不被扫进 cli 列
- 改完回答：一个只在 ap-southeast-1 授权的 `ksk_` 号，
  从上号到第一个成功请求，会走哪几步、每步打哪个 host？
