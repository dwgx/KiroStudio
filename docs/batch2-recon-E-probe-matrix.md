# 侦察 E：探测矩阵的技术可行性

> 只读侦察，未改任何 src/ 与 admin-ui/src/ 文件。所有行号会漂（树在动），每条附代码片段供重新定位。
> 标注约定：**[代码]** = 读码得出并附 file:line；**[未验]** = 推断，未在仓内找到支撑。

---

## 结论速览

| # | 结论 | 依据强度 |
|---|---|---|
| 1 | 规格里「`api_region` 的唯一消费者是 `CliEndpoint::host()`」**不准确**。两个端点的 `api_region()` 都不读 `credentials.api_region`，读的是 `effective_upstream_region()`；`api_region` 只是它**第四优先**的兜底 | [代码] |
| 2 | 归因污染比规格描述的更严重：探 `ap-southeast-1` 时 `rest_api_region_candidates` 把它**整个丢掉**、换成 `["us-east-1","eu-central-1"]` —— 不是「回退可能污染」，是「非 eu/us 的候选从未被测过」 | [代码] |
| 3 | 「假 eu」的确切机制：只在 us 授权的号，探 eu → eu 403 → 内部回退 us 200 → `Ok` → 写死 `api_region=eu-central-1` → CLI host 打 `q.eu-central-1` → 恒 403 | [代码] |
| 4 | 🔴 `probe_api_region` 的跳过门**漏了 `profile_arn`**。带 profileArn 的 ksk_ 号，每个候选拼出的是**同一个 host** → 首个候选返什么就写死什么。同一缺陷让手工 `set_credential_api_region` 也失效 | [代码] |
| 5 | `deep_verify_credential` 可照抄，且**不会误禁健康号** —— 源码守卫注释里「classify_balance_error 自动禁用凭据」这句**已过期**，现在的 `classify_balance_error` 只做错误分类 | [代码] |
| 6 | 推荐改法：探测侧**不用** `get_usage_limits`（选项 c）。它同时消掉根因（探 A 决定 B）与归因污染，且 `get_usage_limits` 一个字节都不用改 | [代码] |
| 7 | 探测**串行在用户的 HTTP 请求里**，且**无任何超时包裹**。今天最坏 2×2×60s = **240s**，之后还接一次 `get_usage_limits_for`（再 120s） | [代码] |
| 8 | `MAX_PROBE_ATTEMPTS` 是 const 且 `take()` 作用在**传入的 `order` 参数**上 → 扩矩阵时把长表传进去会被**静默截断成 2** | [代码] |
| 9 | 仓内**没有** `q.*.amazonaws.com` 到底存在几个区的数字。唯一间接证据：`external_idp_login.rs` 对 6 个区打 `q.*` 且注释称实测每端点只返本区 profile | [代码]+[未验] |

---

## 1. `region_probe.rs` 完整审读

文件：`/Users/dwgx/Documents/Project/KiroStudio/src/kiro/region_probe.rs`（443 行，9 个测试）

### 1.1 `probe_api_region` 控制流

签名（`region_probe.rs:154-159`）：

```rust
pub(crate) async fn probe_api_region(
    credentials: &KiroCredentials,
    config: &Arc<Config>,
    token: &str,
    order: &[&str],
) -> ProbeOutcome
```

| 步 | 位置 | 条件 | 动作 |
|---|---|---|---|
| ① early return | `:161-167` | `region.is_some() \|\| auth_region.is_some() \|\| api_region.is_some()` | `Skipped`（调用方明确意图） |
| ② early return | `:171-173` | `!credentials.is_api_key_credential()` | `Skipped`（OAuth 号 region 由 profileArn 决定） |
| ③ 循环前 | `:175` | — | `let effective_proxy = credentials.effective_proxy(None)`（⚠️ 传 `None`：**忽略全局代理**，只用凭据自己的 proxy_*） |
| ④ 循环 | `:176` | `for region in order.iter().take(MAX_PROBE_ATTEMPTS)` | 🔴 `take()` 作用在**参数** `order` 上而非常量表，见 §1.4 |
| ⑤ 每轮 | `:178-179` | — | `candidate = credentials.clone(); candidate.api_region = Some(region)` |
| ⑥ 每轮 | `:181-189` | — | `get_usage_limits(&candidate, config, token, proxy)` → `.map(\|_\| ()).map_err(\|e\| e.to_string())`（**丢掉整个成功响应，只留 unit**） |
| ⑦ early return | `:194` | `Usable` | `ProbeOutcome::Usable(region)` —— **首个即返，后续候选永不测** |
| ⑧ early return | `:195-198` | `TokenDead` | `ProbeOutcome::TokenDead`，停止 |
| ⑨ 继续 | `:199` | `WrongRegion \| Inconclusive` | `continue` |
| ⑩ 循环外 | `:202-206` | 全候选走完 | `ProbeOutcome::NoUsableRegion` |

**没有超时包裹**：整个函数内唯一的时间上限来自 `get_usage_limits` 里 `build_client(proxy, 60, ...)` 的 60s 总超时（`token_manager.rs:594`）。

### 1.2 `classify_probe_result` 判据表与优先级

`region_probe.rs:61-86`。输入是 `&Result<(), String>`（字符串匹配，**不是 status code**）。

| 序 | 位置 | 判据（对 `err.to_ascii_lowercase()`） | 结论 | 为什么排这个位置 |
|---|---|---|---|---|
| 0 | `:62-64` | `Ok(())` | `Usable` | — |
| 1 | `:70-72` | `"429"` / `"too many requests"` / `"throttling"` | `Usable` | ⭐ 承重。注释 `:68-69`：「上游 429 的响应体里可能同时含 "403" 之类的无关数字（例如 requestId），先判 403 会把可用 region 误杀」 |
| 2 | `:74-76` | `"401"` / `"认证失败"` | `TokenDead` | 注释 `:73`：「必须排在 403 之前 —— 有些上游响应两个码都提」 |
| 3 | `:78-84` | `"403"` / `"accessdenied"` / `"权限不足"` / `"bearer token included in the request is invalid"` | `WrongRegion` | 本模块要抓的信号 |
| 4 | `:85` | 其余（5xx / 网络 / 解析失败） | `Inconclusive` | 「可以试下一个，但不能据此判死」 |

⚠️ 判据 3 的 `"权限不足"` 正好命中 `get_usage_limits` 全候选耗尽时的 bail 文案
（`token_manager.rs:671-675`：`"权限不足，无法获取使用额度（已试全部 {} 个 REST 端点）"`）——
即「两个内部候选都 403」也归 `WrongRegion`，这一条是对的。

### 1.3 四个 `ProbeOutcome` 变体语义

`region_probe.rs:129-143`。枚举文档（`:111-128`）明写「为什么必须是枚举而不是 `Option<String>`」。

| 变体 | 位置 | 语义 | 调用方处置 |
|---|---|---|---|
| `Usable(String)` | `:132` | 探到可用 region | 写死 `api_region` + 启用 |
| `Skipped` | `:137` | **无需探测**（已带 region 字段 / 非 api_key 号） | **照常启用**。注释 `:135`：「与「探测失败」必须分开:这条是调用方的明确意图,**绝不能**据此禁用凭据」 |
| `NoUsableRegion` | `:139` | 候选全部 403/无结论 | 上号路径：保持禁用（`DisabledReason::RegionProbeFailed`）；启动回填路径：忽略 |
| `TokenDead` | `:142` | 401 | 同上，但 reason 是 `RegionProbeTokenDead`。注释 `:140-141`：「处置动作不同：这条要去查 token 来源，那条要去查 region 授权范围」 |

两条判决在 `token_manager.rs:5943-5945` 映射成 DisabledReason：

```rust
ProbeOutcome::NoUsableRegion => DisabledReason::RegionProbeFailed,
ProbeOutcome::TokenDead => DisabledReason::RegionProbeTokenDead,
```

`ProbeVerdict`（`:47-56`）是**单次候选**的结论，四个变体：`Usable` / `WrongRegion` / `TokenDead` / `Inconclusive`。别与 `ProbeOutcome` 混。

### 1.4 🔴 `PROBE_ORDER` 与 `MAX_PROBE_ATTEMPTS` —— 扩矩阵的陷阱

```rust
// region_probe.rs:99
pub(crate) const PROBE_ORDER: &[&str] = &["eu-central-1", "us-east-1"];
// region_probe.rs:107
pub(crate) const MAX_PROBE_ATTEMPTS: usize = PROBE_ORDER.len();
```

`MAX_PROBE_ATTEMPTS` 由常量表长度算出（= 2），但 `:176` 是
`for region in order.iter().take(MAX_PROBE_ATTEMPTS)` —— `take` 作用在**传入的 `order` 参数**上。
于是：**将来传一张 4 项的矩阵进来，第 3、4 项会被静默丢弃**，没有任何编译期或运行期提示。
扩矩阵时必须同时改掉这个 const，或改成 `order.len()`。

`PROBE_ORDER` 只有两项的理由（`:95-98`）：

> ⚠️ **只有两项**。实测 `management.{region}.kiro.dev` 与 `runtime.*`
> **只在 `us-east-1` 与 `eu-central-1` 解析**，其余 13 个区 DNS 都不通。

这条依据针对的是 `*.kiro.dev`，与 `q.*.amazonaws.com` 无关 —— 规格的判断成立。
另注：「其余 13 个区」与 `KIRO_DIALOG_REGIONS` 实际 **33** 项不符（33−2=31），
该数字是旧表遗留（`token_manager.rs:697` 的注释同样写「三张 region 表（34 / 24 / 6 项）」，
实测三表为 **33 / 24 / 6**）。纯注释漂移，不影响行为。

### 1.5 承重测试清单（9 个）

| 测试 | 位置 | 钉住什么 | 扩矩阵时会不会挡路 |
|---|---|---|---|
| `throttled_means_region_is_correct` | `:219` | ⭐ 429 判 `Usable`（3 个样本串） | 不挡（硬约束，别动） |
| `throttling_wins_over_incidental_403_text` | `:284` | ⭐ 顺序守卫：429 判据必须在 403 之前 | 不挡 |
| `forbidden_means_wrong_region` | `:235` | 403 / bearer-invalid → `WrongRegion` | 不挡 |
| `unauthorized_stops_probing` | `:251` | 401 → `TokenDead` | 不挡 |
| `transient_is_inconclusive` | `:260` | 5xx / 网络 / 超时 → `Inconclusive`（样本里含 `management.eu-central-1.kiro.dev` 字面量） | 不挡 |
| `success_is_usable` | `:275` | `Ok` → `Usable` | 不挡 |
| `probe_order_starts_with_measured_winners_and_is_capped` | `:305` | `PROBE_ORDER[0]=="eu-central-1"`、`[1]=="us-east-1"`、**`len()==2`**、`MAX_PROBE_ATTEMPTS==len()`、全项在 `KIRO_DIALOG_REGIONS` 内 | 🔴 **会挡**：`len()==2` 那条断言在扩表时必 FAIL，得连注释理由一起重写 |
| `skipped_must_be_distinguishable_from_probe_failure` | `:351`（`#[tokio::test]`） | 三个 region 字段各测一次 + OAuth 号 → 全 `Skipped`；且 `Skipped != NoUsableRegion/TokenDead` | 不挡（但**它没覆盖 `profile_arn`**，见 §4.2） |
| `add_credential_must_act_on_probe_verdict` | `:412` | 源码守卫：`service.rs` 必须含 `let probe_outcome`、`mark_region_probe_failed(credential_id`、`new_cred.disabled = true` | 不挡（改 service.rs 时保留这三个字面量即可） |

---

## 2. 🔴 `get_usage_limits` 的内部回退怎么隔离（本侦察核心）

### 2.1 候选列表怎么算出来的

`token_manager.rs:576-578`：

```rust
let region = credentials.effective_upstream_region(config);
// ⭐ 候选端点(主 + 403 回退)。见 rest_api_region_candidates 的完整说明。
let candidates = rest_api_region_candidates(region);
```

`rest_api_region_candidates`（`token_manager.rs:699-706`）：

```rust
fn rest_api_region_candidates(sso_region: &str) -> [&'static str; 2] {
    let primary_eu = sso_region == "eu-central-1" || sso_region.starts_with("eu-");
    if primary_eu { ["eu-central-1", "us-east-1"] } else { ["us-east-1", "eu-central-1"] }
}
```

**这是二值函数，返回值只有两种可能。** 传 `ap-southeast-1` 进去，返的是 `["us-east-1","eu-central-1"]` —— 
入参 region **被完全丢弃**。回归测试 `token_manager.rs:11607-11613` 明确钉死了这个语义：

```rust
for r in ["us-east-1", "us-west-2", "ap-northeast-1", "", "unknown"] {
    assert_eq!(rest_api_region_candidates(r), ["us-east-1", "eu-central-1"], ...);
}
```

⇒ 规格里「探 eu 时真正成功的可能是内部回退到的 us」是**其中一种形态**；
更根本的形态是：**除 eu-*/us-* 以外的任何候选，`get_usage_limits` 从来没测过它**。
探 `ap-southeast-1` 与探 `us-east-1` 打的是**逐字节相同的两个 host**。

### 2.2 403 回退在哪一步

`token_manager.rs:650-658`：

```rust
if status.as_u16() == 403 && idx + 1 < candidates.len() {
    tracing::debug!("getUsageLimits 在 {} 返回 403，尝试备用端点 {}", cand_region, candidates[idx + 1]);
    last_error = Some(format!("{} {}", status, body_text));
    continue;
}
```

只对 403 回退（401/429/5xx 直接 `bail`，`:660-667`）。这条有源码守卫测试
`test_usage_limits_falls_back_only_on_403`（`token_manager.rs:11627`），
断言 body 里必须含字面量 `status.as_u16() == 403 && idx + 1 < candidates.len()`。

⚠️ 另一个隐蔽点：`:629` 是 `let response = request.send().await?;` ——
**网络错误直接 `?` 抛出，不进回退**。所以第一个候选 DNS 失败/超时时，第二个候选**根本不会被试**。
探测侧看到的是 `Inconclusive`，而实际上「另一个区可能通」这个信息丢了。

### 2.3 返回值里有没有「实际生效的是哪个 region」

**没有。** 成功路径 `token_manager.rs:631-640`：

```rust
if status.is_success() {
    let data: UsageLimitsResponse = response.json().await?;
    if idx > 0 {
        tracing::info!("getUsageLimits 在主端点失败后由备用端点 {} 成功（该号 SSO region 与 REST 端点不同区）", cand_region);
    }
    return Ok(data);
}
```

`idx > 0` 这个信息**只进了 tracing 日志**，返回值是纯 `UsageLimitsResponse`（额度/订阅字段，无 region）。
而 `region_probe.rs:188` 还进一步 `.map(|_| ())` 把整个响应丢掉。
⇒ 探测侧**在结构上不可能**知道实际成功的是哪个区。

### 2.4 「假 eu」的确切代码路径

以「只在 us-east-1 授权的 ksk_ 号、无任何 region 字段」为例：

| 步 | 代码 | 发生什么 |
|---|---|---|
| 1 | `region_probe.rs:178-179` | `candidate.api_region = Some("eu-central-1")`（`PROBE_ORDER[0]`） |
| 2 | `credentials.rs:498-513` | `effective_upstream_region` → profile_arn 无 → region 无 → auth_region 无 → `effective_api_region` → `api_region="eu-central-1"` |
| 3 | `token_manager.rs:578` | `rest_api_region_candidates("eu-central-1")` → `["eu-central-1","us-east-1"]` |
| 4 | `token_manager.rs:599` | 打 `management.eu-central-1.kiro.dev` → **403** |
| 5 | `token_manager.rs:650-657` | 403 且还有候选 → `continue` |
| 6 | `token_manager.rs:599` | 打 `management.us-east-1.kiro.dev` → **200** |
| 7 | `token_manager.rs:639` | `return Ok(data)`（日志说「备用端点 us-east-1 成功」，返回值不带） |
| 8 | `region_probe.rs:191-194` | `classify_probe_result(&Ok(())) == Usable` → `return Usable("eu-central-1")` |
| 9 | `token_manager.rs:5904` | `entry.credentials.api_region = Some("eu-central-1")` 并落盘 |
| 10 | `service.rs:1785` | 回写进 `new_cred`，**全部分身继承这个假 eu** |
| 11 | `cli.rs:79` | 此后每个请求打 `https://q.eu-central-1.amazonaws.com/` → 恒 403 |

⇒ 用户看到的「默认都是 eu」里，**只在 us 授权的那部分号是假 eu，而且是必然的**（eu 排在表首）。
反向的假 us 不会发生：只在 eu 授权的号，第一个候选就是 eu 且会 200。

### 2.5 三种改法与侵入面

`get_usage_limits` 的**全部**调用方（`grep -rn "get_usage_limits(" --include='*.rs' src/`）：

| # | 位置 | 代码 |
|---|---|---|
| 1 | `token_manager.rs:6154` | `get_usage_limits(&credentials, &cfg, &token, effective_proxy.as_ref())` —— 在 `get_usage_limits_for` 里 |
| 2 | `region_probe.rs:181` | 探测路径（本次要改的那个） |
| 3 | `token_manager.rs:11632` | 测试里的 `include_str!` 源码守卫（找 `"pub(crate) async fn get_usage_limits("` 这个字面量前缀） |

只有 2 个真实调用方。⚠️ 但 #3 的守卫是按**签名字符串前缀**找的 —— 加参数安全（前缀不变），**改函数名会 panic**。

---

#### (a) 加一个不做内部回退的入口

形态：`get_usage_limits_pinned(creds, cfg, token, proxy, pinned_region)`，
或给现函数加 `allow_region_fallback: bool` 参数。

侵入面：
- 加布尔参数 → 改 1 处调用方（`:6154` 补 `true`）+ 探测侧传 `false`。守卫测试不受影响。
- 新开函数 → 两函数共 90% 逻辑，必然分叉（本仓已有先例：`update.rs:246-250` 的注释明写
  「上一轮漏改就是因为同一逻辑各写了一份」）。
- **仍然探 `management.*`** —— 根因（探 A 决定 B）**没解**。
- 而且 `management.*` 只在 2 个区解析，pinned 到 `ap-southeast-1` 只会 DNS 失败判 `Inconclusive`。

#### (b) 让被调函数回传实际生效的 region

形态：返回 `(UsageLimitsResponse, &'static str)` 或新结构体 `UsageLimitsOutcome { data, effective_region }`。

侵入面：
- 改 2 处调用方（`:6154` 需解构；`region_probe.rs:188` 的 `.map(|_| ())` 要改成保留 region）。
- 守卫测试 `:11632` 不受影响（按签名前缀找）。
- 好处：`get_usage_limits_for` 那侧也白拿一个可观测量（现在只有 debug 日志）。
- **同样没解根因**，且对非 eu/us 候选依旧无意义（`rest_api_region_candidates` 只返那两个）。

#### (c) 探测侧不用 `get_usage_limits`，改用别的探针 ✅ 推荐

形态：探测自己按「凭据实际会用的端点」构造请求，照 `deep_verify_credential`
（`token_manager.rs:6265-6356`）的模式。仓内**已有两个同构先例**可抄：

| 先例 | 位置 | 特点 |
|---|---|---|
| `deep_verify_credential` | `token_manager.rs:6294-6331` | 走 `endpoint::for_credentials` + `api_url` + `decorate_api` + `transform_api_body`，**零手搓 host** |
| `probe_profile_usable` | `token_manager.rs:919-986` | 自己拼 `management.{region}.kiro.dev`，**无内部回退**、30s client、只读、`classify_profile_probe` 分类 |

侵入面：
- `get_usage_limits` **一个字节都不动** → 守卫测试、`get_usage_limits_for` 全不受影响。
- 只改 `region_probe.rs`：`get_usage_limits(...)` 那一段（`:181-189`）换成
  「构造 endpoint + 发请求 + 按 status 分类」。`classify_probe_result` 的**判据表可保留**
  （只要仍喂它「status + body」拼出的字符串），⇒ 那 6 个承重测试全部继续有效。
- 附带解决：能测 `q.*` 的任意区；能构造「端点 × region」二维矩阵；能拿到真实 status 而不是字符串匹配。

**推荐 (c)**，理由三条：
1. 唯一能同时消掉根因（探 A 决定 B）与归因污染的选项。(a)(b) 只修归因，仍在探错域名。
2. 侵入面**最小**：不碰 `get_usage_limits` 及其守卫测试与另一个调用方。
3. 仓内有两个可直接照抄的先例，且其中一个（`deep_verify`）解决过完全相同的
   「别手搓 host」问题并留了完整理由注释。

⚠️ 若还想保留 `management.*` 作为兜底探针（例如 `q.*` 全 403 时），
那时才需要 (a) 或 (b) 中的一个来隔离回退。**建议先只做 (c)**，别一次上两套。

---

## 3. `deep_verify_credential` 能否照抄

**能，而且是本轮最该照抄的那份。** 位置：`token_manager.rs:6265-6356`。

### 3.1 完整调用序列

| 步 | 位置 | 代码 |
|---|---|---|
| 0 | `:6269-6279` | custom_api 分流：`is_custom_api_credential()` → 走 `deep_verify_custom_api` 后 return（**探测路径可省**：`is_api_key_credential()` 已把 custom_api 排除在外，见 §3.4） |
| 1 | `:6283` | `let (credentials, token) = self.ensure_valid_token(id).await?;` |
| 2 | `:6285` | `let cfg = self.config.load();`（`ArcSwap` Guard，deref 成 `&Config`） |
| 3 | `:6294` | `let endpoint = crate::kiro::endpoint::for_credentials(&credentials, &cfg.default_endpoint);` |
| 4 | `:6295` | `let machine_id = machine_id::generate_from_credentials(&credentials, &cfg);` |
| 5 | `:6296-6303` | 构造 `RequestContext { credentials, token, machine_id, config, is_1m: false }` |
| 6 | `:6304` | `let url = endpoint.api_url(&rctx);` |
| 7 | `:6308-6318` | 最小 body：`{"conversationState":{"conversationId":<uuid>,"currentMessage":{"userInputMessage":{"content":"hi"}}}}`（**故意缺 modelId**） |
| 8 | `:6319` | `let body = endpoint.transform_api_body(&body, &rctx);` |
| 9 | `:6321-6322` | `effective_proxy(self.proxy.as_ref())` + `build_client(proxy, 30, cfg.tls_backend)?`（**30s**，非 60s） |
| 10 | `:6324-6329` | `endpoint.decorate_api(client.post(&url).header("content-type", endpoint.content_type()), &rctx)` |
| 11 | `:6331` | `let response = request.body(body).send().await?;` |
| 12 | `:6335-6355` | 403（含 `suspended` 子判） → bail；401 → bail；其余（含 400）→ `Ok(())` |

关键注释（`:6286-6293`，本轮要复用的正是这段理由）：

> 为何不再手搓 `runtime.{region}.kiro.dev`：CLI(ksk_)号必须走
> `q.{region}.amazonaws.com` 服务根 + X-Amz-Target + 不带 profileArn，打 IDE 端点
> 稳定 403。而本函数把 403 当「权限被拒/疑似封号」上报，classify_balance_error 据此
> **自动禁用凭据** → 一个完全健康的 ksk_ 号会被验活自己弄死。

这条模式被源码守卫钉住（`token_manager.rs:8028-8049`
`should_use_endpoint_abstraction_in_deep_verify`）：body 必须含 `endpoint::for_credentials`
与 `endpoint.api_url(&rctx)`，且**不得**含 `runtime.{}.kiro.dev` 与 `effective_profile_arn`。
另一份同构守卫覆盖 `probe_single_model`（`:8052`）。**新增的探测函数应该一并加进这个守卫家族。**

### 3.2 `RequestContext` 全部字段与来源

定义在 `endpoint/mod.rs:202-215`，5 个字段全是引用（无 clone）：

| 字段 | 类型 | 来源（deep_verify 里） | 探测侧怎么给 |
|---|---|---|---|
| `credentials` | `&'a KiroCredentials` | `ensure_valid_token` 返回的最新快照 | **要给 clone 后改过 `api_region`/`endpoint` 的 candidate** |
| `token` | `&'a str` | 同上第二个返回值；api_key 号即 `kiro_api_key` | 探测已有 `token` 参数 |
| `machine_id` | `&'a str` | `machine_id::generate_from_credentials(&credentials, &cfg)`（`machine_id.rs:83`） | 同样算一次即可（**用原凭据算，别用 candidate**，否则改字段会不会影响派生要另行确认 —— 实际不影响：派生只看 `machine_id` / `kiro_api_key` / `refresh_token`，见 `machine_id.rs:83-95`） |
| `config` | `&'a Config` | `self.config.load()` Guard deref | 探测已有 `config: &Arc<Config>` 参数，deref 即可 |
| `is_1m` | `bool` | 固定 `false`（注释 `:6301-6302`：「验活不涉及 1M 变体」） | 固定 `false` |

### 3.3 🔴 关键风险：复用 deep_verify 会不会在 403 时误禁一个健康号？

**不会。而且上面那段注释里的「classify_balance_error 据此自动禁用凭据」已经过期了。** 三条证据：

**证据 1 —— `classify_balance_error` 现在不禁用任何东西。**
`service.rs:4627-4682` 全函数只做 `anyhow::Error` → `AdminServiceError` 的映射
（`Diagnosed` / `NotFound` / `InvalidCredential` / `UpstreamError` / `InternalError`），
**没有任何 `entry.disabled = true` 或 `report_*` 调用**。

**证据 2 —— deep_verify 的 403 文案甚至不命中它的上游错误分支。**
`token_manager.rs:6340` bail 的是 `"权限被拒绝 (403): {}"`，而 `service.rs:4664` 匹配的是
`msg.contains("权限不足")` —— **两个不同的词**。所以 deep_verify 的 403 落
`service.rs:4680` 的 `InternalError`（HTTP 500），既不禁用也不算「上游错误」。
（`:6338` 的 `"账号已被封禁 (suspended)"` 同样不命中任何分支。）

**证据 3 —— 真正会禁用的 `report_account_suspended` 只在对话热路径被调。**
`grep -rn "report_account_suspended" --include='*.rs' src/` 的非测试命中只有两处，
都在 `provider.rs`（`:828`、`:1338`）—— 对话失败路径。deep_verify / probe_models 都不碰它。

**因此**：探测复用 deep_verify 的构造模式是安全的。但有两条**必须保留的边界**：

| 边界 | 依据 |
|---|---|
| 探测函数**不要**调 `report_failure` / `report_account_suspended` / `persist_disabled_state` | 那些才是真正的禁用入口（`token_manager.rs:4921` / `:5289`） |
| 探测**不要**对非 api_key 号调 `ensure_valid_token` 之外的刷新 | `ensure_valid_token` 对 api_key 号在 `:4284-4290` 直接返回 `kiro_api_key`，**零上游往返、零副作用**；对 OAuth 号才会走 `refresh_token_locked`（`:4306`），那条路**能**触发禁用（`report_refresh_token_invalid` 等）。探测只对 api_key 号跑，天然避开 |

⚠️ 顺带记一笔（不属本轮改动，但注释在骗人）：
`token_manager.rs:6291-6292` 与 `:8009-8010` 两处注释都断言「验活侧经 classify_balance_error
**自动禁用凭据**」。按上述证据 1/2 这已不成立。守卫测试本身仍有价值（防手搓 host），
但**它的理由说明已过期**，读代码的人会据此高估风险。

### 3.4 探测侧不必抄的部分

- **custom_api 分流**（`:6269-6279`）：`probe_api_region` 的门是 `is_api_key_credential()`
  （`credentials.rs:655-662`：看 `kiro_api_key.is_some()` 或 `auth_method ∈ {api_key, apikey}`），
  而 custom_api 号是 `auth_method == "custom_api"`（`credentials.rs:620-622`）⇒ 天然被排除。
  ⚠️ **边界**：一个同时带 `kiro_api_key` 和 `base_url` 的号会被 `is_api_key_credential()` 判 true
  而被探测。`effective_endpoint` 那侧显式排除了这种组合（`credentials.rs:751`
  `if !self.is_custom_api_credential() && self.is_api_key_credential()`），
  探测侧**没有**这道排除。属既有小缺口，扩矩阵时顺手对齐即可。
- **`suspended` 子判**（`:6337-6339`）：探测只需 region 结论，是否封号交给对话路径。

---

## 4. 端点侧怎么构造「cli × us-east-1」这种组合

### 4.1 两个端点的 `api_region()` 读什么

**两个都不读 `credentials.api_region`。** 它们读同一个函数：

```rust
// cli.rs:41-45
fn api_region<'a>(&self, ctx: &'a RequestContext<'_>) -> &'a str {
    // 与 IDE 端点同口径：profileArn 第 4 段 > 凭据 region > config；ksk_ 号通常无
    // profileArn/region → 回退 config region（默认 us-east-1，实测 q.us-east-1 可用）。
    ctx.credentials.effective_upstream_region(ctx.config)
}
// cli.rs:47-49
fn host(&self, ctx) -> String { format!("q.{}.amazonaws.com", self.api_region(ctx)) }
// cli.rs:79
fn api_url(&self, ctx) -> String { format!("https://q.{}.amazonaws.com/", self.api_region(ctx)) }
```

```rust
// ide.rs:44-52 —— 逐字同构，只有 host 模板不同
ctx.credentials.effective_upstream_region(ctx.config)
format!("runtime.{}.kiro.dev", self.api_region(ctx))
```

`effective_upstream_region`（`credentials.rs:498-513`）的**四级优先级**：

| 级 | 位置 | 读什么 | 约束 |
|---|---|---|---|
| 1 | `:499-505` | `self.profile_arn` 第 4 段 | 必须 `arn:*:codewhisperer:` 前缀 + region 命中 `KIRO_DIALOG_REGIONS`（`region_from_profile_arn`，`:520-535`） |
| 2 | `:508-510` | `self.region` | 过 `sanitized_region` 白名单 |
| 3 | `:511` | `self.auth_region` | 同上 |
| 4 | `:512` | `self.effective_api_region(config)` → `self.api_region` → `config.api_region` → `config.region` | `credentials.rs:483-488` + `config.rs:1081-1082` |

⇒ **`api_region` 是第四优先。** 规格里「`api_region` 的唯一消费者是 `CliEndpoint::host()`」应改成：
「`api_region` 是 `effective_upstream_region` 的末级兜底，而后者的三个消费者是
`cli.rs:48/79/84`（`q.*`）、`ide.rs:51/86/91`（`runtime.*`）、`token_manager.rs:576`（`management.*`）」。
对 ksk_ 号，因为 `effective_endpoint` 自动路由到 cli（`credentials.rs:751-753`），
**决定性的那个**确实是 `cli.rs` —— 但探测同时也在影响 `management.*` 的候选选择（§2.4 第 3 步）。

### 4.2 🔴 clone 一份凭据改 `api_region` 够不够？——不够

参与 host 构造的**其它字段**（按上表优先级，任一有值就压过 `api_region`）：

| 字段 | 探测时是否为 None | 风险 |
|---|---|---|
| `region` | `probe_api_region:161` 已挡（有值即 `Skipped`） | 安全 |
| `auth_region` | `:162` 已挡 | 安全 |
| `api_region` | `:163` 已挡 | 安全 |
| **`profile_arn`** | 🔴 **没挡** | 见下 |

`probe_api_region:161-167` 的跳过门：

```rust
if credentials.region.is_some() || credentials.auth_region.is_some() || credentials.api_region.is_some() {
    return ProbeOutcome::Skipped;
}
```

**没有 `profile_arn.is_none()` 这一条。** 而 `AddCredentialRequest` 是允许带 profileArn 的
（`service.rs:1499`：`profile_arn: req.profile_arn`）。于是一个带合法 profileArn 的 ksk_ 号：

- `effective_upstream_region` 在**第 1 级**就返回 ARN region → `candidate.api_region` 被完全忽略
- 两个候选拼出**同一个 host** → 首个候选返什么就写死什么（若通就写 `eu-central-1`，纯属捏造）
- 而写下去的 `api_region` 在真实请求里**同样被 ARN 压过** ⇒ 写了也没用

⚠️ 同一缺陷让**手工右键改 region 也失效**：`set_credential_api_region`
（`token_manager.rs:5703-5725`）只写 `entry.credentials.api_region`。
号带 `profile_arn` 或 `region` 时，手工值被第 1/2 级压过 → **面板显示改了、实际 host 没变**。
这直接打到用户要求「即使自动识别出错，内置右键设置也要能手工改」。

对照：`probe_profile_usable`（`token_manager.rs:927-929`）就正确处理了这一层 ——
它设 `profile_arn` 后立刻 `cred.sync_region_from_arn()` 让 region 与 ARN 物理绑定。
探测侧需要的是**反向**动作：清掉会压过 api_region 的上级字段，或直接改写 `region` 字段。

**建议的 candidate 构造**（比只改 `api_region` 稳）：

```
let mut candidate = credentials.clone();
candidate.profile_arn = None;   // ksk_ 号本就不该带（effective_profile_arn 对它返 None）
candidate.api_region  = Some(region);
candidate.endpoint    = Some(endpoint_name);   // "cli" / "ide"
```

清 `profile_arn` 对 ksk_ 号是**语义正确**的，不是 hack：`effective_profile_arn()`
对 api_key 号硬性返回 `None`（`credentials.rs:716-718`，注释 `:686-692` 有实测依据：
「套上**别人账户**的占位 ARN 会被上游判为凭据与 ARN 不匹配：`getUsageLimits` 直接 403 Invalid token」）。
即 ksk_ 号的 profileArn 本来就不会被发出去，只会污染 host 计算。

### 4.3 怎么钉住端点

`effective_endpoint`（`credentials.rs:741-756`）三级：

| 级 | 位置 | 规则 |
|---|---|---|
| 1 | `:743-748` | **显式 `endpoint` 字段优先**（trim 后非空即用） |
| 2 | `:751-753` | `!is_custom_api_credential() && is_api_key_credential()` → `"cli"` |
| 3 | `:755` | `config.default_endpoint`（默认 `"ide"`，`config.rs:198-200`） |

⇒ 探「cli × R」设 `candidate.endpoint = Some("cli")`；探「ide × R」设 `Some("ide")`。
再用 `endpoint::for_credentials(&candidate, &cfg.default_endpoint)`（`endpoint/mod.rs:57-63`）取实现。
该函数对未知名字**回退 IdeEndpoint 而非报错**（`:62`），所以名字必须用常量
`cli::CLI_ENDPOINT_NAME` / `ide::IDE_ENDPOINT_NAME`（`cli.rs:28` / `ide.rs:18`），别写字面量。

### 4.4 OAuth 号为什么不能进 cli 列 —— `cli.rs` 原文

模块头注释（`cli.rs:3-13`）：

> 对应「Kiro API Key」(`ksk_` 前缀) 号，它们本质是 AWS IAM Identity Center(IdC) 账号
> 的 CLI 访问密钥。这类号**绝不能**走 IDE 端点（`runtime.{region}.kiro.dev/generateAssistantResponse`）——
> 实测会被上游 403（`User is not authorized` / 缺自己租户真实 profileArn 时 400/403）。
>
> - **`tokentype: API_KEY`** 必带。
> - **绝不注入 profileArn**：API_KEY 认证既不使用也不支持 profileArn；带上反而 403。
>   （这是它与 IDE 端点 `transform_api_body` 注入 ARN 的**根本区别**。）

`decorate_api` 无条件发 `tokentype: API_KEY`（`cli.rs:90`）：

```rust
req.header("X-Amz-Target", CLI_AMZ_TARGET)
    .header("tokentype", "API_KEY")
```

`:99-100` 的刻意不注入：

> 刻意不注入 profileArn / anthropic-beta：API_KEY 认证不使用 profileArn；
> CLI 端点的 1M 窗口由上游按 modelId 决定，不依赖 anthropic-beta 头。

`transform_api_body`（`cli.rs:114-117`）：

> CLI 协议：注入 agentTaskType/agentMode="vibe"，**绝不**注入 profileArn。

两条回归测试钉死：`test_inject_cli_agent_fields_never_adds_profile_arn`（`cli.rs:162`）与
`should_never_inject_profile_arn_even_when_credential_has_one`（`cli.rs:215`，凭据自带 ARN 也不注入）。

⇒ 把一个 social/idc/M365 号塞进 cli 列，会给它发 `tokentype: API_KEY` 且剥掉它**必需**的
profileArn（IDE 侧 `ide.rs:143` 是必注入的，且 `credentials.rs:708-710` 记录了
「kiro.dev 迁移后 external_idp 号**必须**带自己租户的真实 profileArn，缺了直接 `400 profileArn is required`」）
⇒ 稳定失败。**矩阵只对 `ksk_` 号展开**，规格这条成立。
（现状也已经天然满足：`probe_api_region:171` 的 `is_api_key_credential()` 门就在那儿。）

---

## 5. 成本：探测串行在用户请求里吗

### 5.1 两个调用点

`grep -rn "probe_and_persist_api_region" --include='*.rs' src/`：

| # | 位置 | 上下文 | 是否阻塞用户 |
|---|---|---|---|
| 1 | `service.rs:1757-1760` | `add_credential` 内 | 🔴 **是** |
| 2 | `main.rs:428` | 启动回填后台任务 | 否（`tokio::spawn` + 先 sleep 10s + 每个间隔 3s，`main.rs:405-430`） |

调用点 1 的完整代码（`service.rs:1757-1760`）：

```rust
let probe_outcome = self
    .token_manager
    .probe_and_persist_api_region(credential_id)
    .await;
```

**在 `add_credential` 的第几步**：入池（`service.rs:1737` `add_credential(new_cred.clone()).await`）
**之后**、`get_usage_limits_for` （`:1825`）**之前**。顺序被源码守卫钉死
（`token_manager.rs:11732-11742`：`probe_and_persist_api_region(credential_id)` 的下标必须 <
`get_usage_limits_for(credential_id)` 的下标）。

**用户点「添加凭据」要等它**：HTTP handler 是完全 await 的 ——
`handlers.rs:581`：`match state.service.add_credential(payload).await`，
中间没有 `spawn`、没有提前返回。`add_credential` 里的注释也自认这一点
（`region_probe.rs:105-106`：「上号路径是**串行**在用户的「添加凭据」HTTP 请求里的（面板会多转一次圈）」）。

### 5.2 超时：没有任何包裹，只有 client 级预算

- **无 `tokio::time::timeout`**：`probe_and_persist_api_region`（`token_manager.rs:5848-5924`）
  与 `probe_api_region`（`region_probe.rs:154-207`）里都没有。
- **无 HTTP 服务端超时层**：`grep TimeoutLayer` 在 `main.rs` / `admin/router.rs` 零命中；
  唯一的 `.timeout(...)` 在 `admin_ui/router.rs:148`（那是背景图代理的 reqwest client，不是 server layer）。
- **单次探测的 client 预算 = 60s 总超时**：`token_manager.rs:594`
  `let client = build_client(proxy, 60, config.tls_backend)?;`
  而 `build_client`（`http_client.rs:775-783`）用的是 `.timeout(Duration::from_secs(timeout_secs))`
  —— reqwest 的**整请求生命周期**总时长（不是 idle）。

对照：`deep_verify` 用 **30s**（`token_manager.rs:6322`）、`probe_profile_usable` 用 **30s**（`:950`）。
探测改走端点探针时应取 30s 而非 60s。

### 5.3 最坏耗时账

**今天（2 个候选，走 `get_usage_limits`）**：每个候选内部还有 2 个 REST 候选（§2.1），
且**每一跳独立计 60s**（同一个 client，但 `.timeout` 是 per-request）：

```
2 (PROBE_ORDER) × 2 (rest 内部候选) × 60s = 240s
```

⚠️ 修正：只有「403 → continue」才会走满内部 2 跳；网络错误在 `:629` 直接 `?` 抛出，
所以纯挂死场景是 `2 × 60s = 120s`，403 挂死场景才是 240s。**取上界 240s**。

之后还接一次 `get_usage_limits_for`（`service.rs:1825`）= 再 `2 × 60s = 120s`。
⇒ **单次「添加凭据」理论最坏 ≈ 360s，且无服务端超时兜住。**
（探测失败时 `:1820-1824` 会跳过 `get_usage_limits_for`，此时上界 240s。）

**规格提议的 4 次往返（2 端点 × 2 区）**，若走端点探针（无内部回退）+ 30s client：

```
4 × 30s = 120s 最坏
```

比现状**更快**。规格里「常见路径压到 2 次：先测 q.* × 2 区，只在两个都 403 时才试 runtime.*」
则常见路径是 `2 × 30s = 60s` 上界、实测 RTT 量级约 0.4~1.6s（[未验]，无仓内实测数据）。

### 5.4 批量导入会把这个成本 ×4

`import_keys`（`service.rs:2058-2112`）用 `Semaphore::new(IMPORT_MAX_IN_FLIGHT)`，
`IMPORT_MAX_IN_FLIGHT = 4`（`service.rs:77`）。每条都走 `import_one_key` → `add_credential`
⇒ 同一时刻**最多 4 路探测并发打上游**。扩矩阵后是 4 × 4 = 16 个在飞探测请求。
`IMPORT_MAX_IN_FLIGHT` 的注释（`service.rs:70-71`）明说上界由上游风控决定 ——
**扩矩阵时这个常量的依据（「单条耗时以那次 get_usage_limits 往返为主」）会失真，需要一并复核。**

`clone_credential`（`service.rs:1321-1381`）复用同一条 `add_credential_with_intent` 路径，
但它从池中既有号继承 `api_region`（`service.rs:1520-1522` 的 `inherit`）
⇒ 探测在 `:5862-5868` 的廉价预判处就 `Skipped`，**加分身不付探测成本**。

---

## 6. 矩阵该多大

### 6.1 `regions.rs` 三张表（实测项数）

文件：`/Users/dwgx/Documents/Project/KiroStudio/src/kiro/regions.rs`（93 行）

| 表 | 位置 | 项数 | 用途（原文） |
|---|---|---|---|
| `KIRO_DIALOG_REGIONS` | `:20-35` | **33** | 「Kiro 对话/余额端点（`runtime.{r}.kiro.dev` / `management.{r}.kiro.dev`）与 profileArn 第 4 段的**合法 region 白名单**。用于严格校验、防止污染值拼进上游 host」 |
| `OIDC_PROBE_REGIONS` | `:43-52` | **24** | `oidc.{r}.amazonaws.com` 的 IdC device flow 探测候选 |
| `PROFILE_PROBE_REGIONS` | `:59-62` | **6** | external_idp 动态解析 profileArn 的多 region 探测候选：`us-east-1, eu-central-1, us-west-2, eu-west-1, ap-southeast-1, ap-northeast-1` |

⚠️ **`KIRO_DIALOG_REGIONS` 是白名单，不是「端点存在于这些区」的清单。** 模块头
`regions.rs:9` 明写用途是「严格校验、防止污染值拼进上游 host」。它含 `us-gov-*` 与 `cn-*`
（`:33-34`）—— 隔离分区，`*.kiro.dev` 几乎确定不在那儿解析（`OIDC_PROBE_REGIONS` 就明确
排除了它们，理由见 `:41-42`）。所以**不能**拿这 33 项当矩阵候选池。

### 6.2 `q.*.amazonaws.com` 到底在几个区存在

**仓内没有这个数字。** 我查了：`grep -rn "q\.[a-z-]*\.amazonaws" docs/ *.md` → 零命中；
源码里所有 `q.*` 出现处（`cli.rs:48/79/84`、`external_idp_login.rs:760`、
`token_manager.rs:12533/12548` 测试）都不带区数结论。

**唯一间接证据** —— `external_idp_login.rs:747-806` 的 `list_region_profile_arns`：

```rust
// :747-748
/// 打**单个 region** 的 `q.{region}.amazonaws.com` ListAvailableProfiles,返回该 region 的
/// 全部 profile arn(实测每端点只返回本 region 的 profile)。
let host = format!("q.{}.amazonaws.com", region);   // :760
```

它被 `merge_probe_regions`（`:812`）喂 `PROFILE_PROBE_REGIONS` 的 **6 个区**，
且注释说「**实测**每端点只返回本 region 的 profile」——「每端点」这个措辞暗示 6 个都有响应。
但这只是措辞，**没有任何一行代码或注释说这 6 个 host 都解析成功**（`:858-861` 的循环把
每个 region 的错误吞成 debug 日志继续 → 5 个区全 DNS 失败也长得一模一样）。

⇒ **`q.*` 存在于几个区：未确认。** 已知只有「`PROFILE_PROBE_REGIONS` 那 6 个被代码真实打过」，
以及规格里用户实测的 `q.eu-central-1`（300 并发 200/300 全过、0 个 429）。
`q.us-east-1` 有间接支撑：`cli.rs:42-43` 注释「ksk_ 号通常无 profileArn/region → 回退
config region（默认 us-east-1，**实测 q.us-east-1 可用**）」。

**反向证据（`*.kiro.dev` 侧）确实存在**：`region_probe.rs:95-96` 与
`token_manager.rs:683-686` 两处独立记载「`management.*` / `runtime.*` 只有这两个区解析得到，
其余 DNS 直接不通」，且 `token_manager.rs:686` 记了那个坑的症状：
「上游回 `403 {"message":"Invalid token"}` —— 那个文案会让人误判成 token 坏了」。

### 6.3 建议的矩阵

⚠️ 前提：每个候选 = 一次真实上游往返，且串行在用户请求里（§5）。以下按**依据强度**排：

| 阶段 | 组合 | 依据 | 成本 |
|---|---|---|---|
| **第 1 轮（必做）** | `cli × eu-central-1` | 用户实测 300 并发 200/300、0 个 429（规格）；线上池里两个 eu 号（`STATUS-2026-08-05.md:120`）；`region_probe.rs:91` 「eu-central-1 能用的号 99 个」 | 1 |
| | `cli × us-east-1` | `cli.rs:42-43` 「实测 q.us-east-1 可用」；`region_probe.rs:91` 「us-east-1 有 11 个」；线上池里 1 个 us 号 | 1 |
| **第 2 轮（仅当第 1 轮两个都 403）** | `ide × eu-central-1` | `runtime.eu-central-1.kiro.dev` 确认解析（`region_probe.rs:95-96`）；用户实测 206/300、94 个 429 | 1 |
| | `ide × us-east-1` | 同上，两个区都解析 | 1 |
| **第 3 轮（需 config 开关，默认关）** | `cli × {us-west-2, eu-west-1, ap-southeast-1, ap-northeast-1}` | 只有「`external_idp_login` 对这 4 个区打过 `q.*`」这一条弱依据，**没有确认它们解析** | 4 |

**不建议**：
- 把 `KIRO_DIALOG_REGIONS` 全 33 项当候选（§6.1：那是校验白名单，含 gov/cn 隔离分区）。
- 第 3 轮无条件开启。它对 `ap-southeast-1`-only 的号是唯一出路，但对**绝大多数**号是 4 次白打
  的上游往返；而「只在第三区授权的 ksk_ 号真实存在吗」目前**无任何实测支撑**
  （规格里那句「只在第三个区授权的 `ksk_` 号会被判 NoUsableRegion」是推理，不是观测）。
  建议做成 `regionProbeExtendedCandidates: false` 之类的配置项，出现真实受害号再打开。

**排序依据**：`ide` 放在 `cli` 之后，理由是用户实测 `runtime.*` 有 31% 的 429
（94/300）而 `q.*` 是 0 个。⚠️ 但 §注意规格自己的告警：耗时差（21.4s vs 5.3s）
可能是服务端排队而非净余量，**结论只用于「runtime 更容易 429」**。
另有反向证据：`STATUS-2026-08-05.md:「端点（IDE vs CLI）决定 429」→ 两个都干净
（454 IDE 0.1% / 473 CLI 0%）` —— 即端点与 429 的关系**在另一批数据里不成立**。
所以「cli 优先」的真正依据应该是**协议正确性**（ksk_ 号必须走 cli，`cli.rs:3-13`），
不是 429 率。

### 6.4 扩矩阵的三个硬前提

| # | 事项 | 位置 |
|---|---|---|
| 1 | `MAX_PROBE_ATTEMPTS` 必须跟着改（或改成 `order.len()`），否则新候选被**静默截断** | `region_probe.rs:107` + `:176` |
| 2 | 测试 `probe_order_starts_with_measured_winners_and_is_capped` 的 `PROBE_ORDER.len()==2` 断言必 FAIL，要连注释理由（`:295-303`）一起重写 | `region_probe.rs:312-320` |
| 3 | 所有候选必须在 `KIRO_DIALOG_REGIONS` 内（测试 `:322-327` 断言此事），否则 `sanitized_region` 会把它过滤掉、`api_region` 写了也不生效 | `region_probe.rs:322-327` + `credentials.rs:459-465` |

---

## 7. 顺带发现（不在提问清单里，但影响本轮设计）

| # | 发现 | 位置 | 影响 |
|---|---|---|---|
| A | 🔴 探测跳过门漏 `profile_arn`，导致带 ARN 的 ksk_ 号「所有候选打同一 host」；**同一缺陷让手工 `set_credential_api_region` 也不生效** | `region_probe.rs:161-167`；`token_manager.rs:5703-5725`；优先级见 `credentials.rs:499-505` | 直接打到用户要求「右键手工改要能生效」。手工入口需要同时清 `profile_arn`（对 ksk_ 号语义正确，见 §4.2）或改写 `region` |
| B | `probe_api_region:175` 用 `effective_proxy(None)` —— **忽略全局代理**，只用凭据自己的 proxy_* | `region_probe.rs:175`；对照 `token_manager.rs:6321` 的 `effective_proxy(self.proxy.as_ref())` | 全局代理环境下探测走裸连、真实请求走代理 ⇒ 探测的出口 IP 与实际不同，403 结论可能不可迁移 |
| C | `probe_api_region` 门是 `is_api_key_credential()`，**不排除** 同时带 `base_url` 的号；`effective_endpoint`（`credentials.rs:751`）那侧排除了 | `region_probe.rs:171` vs `credentials.rs:751` | 小缺口，扩矩阵时顺手对齐 |
| D | `get_usage_limits` 网络错误在 `:629` 直接 `?` 抛出，**不进 403 回退** ⇒ 主端点 DNS 失败时备用端点根本不被试 | `token_manager.rs:629` | 影响成本上界估算（§5.3）与「Inconclusive 是否真的无结论」 |
| E | 两处注释断言「验活侧经 `classify_balance_error` **自动禁用凭据**」已过期：现函数只做错误分类（`service.rs:4627-4682` 无任何禁用），且 deep_verify 的 403 文案「权限被拒绝」不匹配它的「权限不足」判据 | `token_manager.rs:6291-6292`、`:8009-8010` vs `service.rs:4627-4682`、`:6340`、`:4664` | 会让人高估「复用 deep_verify」的风险。守卫测试本身仍有价值（防手搓 host），只是理由说明该更正 |
| F | 注释数字漂移：`region_probe.rs:96/299` 写「其余 13 个区」、`token_manager.rs:697` 写「三张 region 表（34 / 24 / 6 项）」，实测三表为 **33 / 24 / 6**（33−2=31 而非 13） | `regions.rs:20-62` | 纯注释，不影响行为 |
| G | 全仓**只有 2 个** `get_usage_limits` 真实调用方（`token_manager.rs:6154` / `region_probe.rs:181`）+ 1 个按签名前缀找字面量的守卫测试（`:11632`）⇒ 改返回类型可行，**改函数名会让守卫 panic** | 见 §2.5 | 决定了 (b) 方案的侵入面确实很小 |

---

## 摘要（≤300 字，供下游直接消费）

`region_probe.rs` 探 `management.*` 而 `api_region` 决定 `q.*`，根因确认。但归因污染比规格更重：
`rest_api_region_candidates`（`token_manager.rs:699`）是**二值函数**，非 eu-*/us-* 的候选被整个丢弃 ——
探 `ap-southeast-1` 与探 `us-east-1` 打的是同一对 host。「假 eu」的确切链路：只在 us 授权的号，
eu 403 → 内部回退 us 200 → `Ok` → 写死 `api_region=eu-central-1` → CLI 恒 403（§2.4 十一步）。

**推荐改法 (c)：探测侧不用 `get_usage_limits`**，照 `deep_verify_credential`
（`token_manager.rs:6294-6331`：`for_credentials` → `RequestContext` → `api_url` → `decorate_api`
→ `transform_api_body`）自建端点探针。`get_usage_limits` 一字节不动 ⇒ 不碰它的守卫测试与另一调用方。
**复用 deep_verify 不会误禁健康号** —— 那句「classify_balance_error 自动禁用」注释已过期
（`service.rs:4627-4682` 只做分类，且 403 文案「权限被拒绝」不匹配它的「权限不足」判据）。

**两个必须处理的坑**：① `MAX_PROBE_ATTEMPTS`（`region_probe.rs:107`）是常量而 `take()` 作用在传入
`order` 上 → 扩矩阵会被静默截断成 2；② 跳过门（`:161-167`）漏 `profile_arn`，带 ARN 的 ksk_ 号
所有候选拼同一 host，**手工 `set_credential_api_region` 同样被 ARN 压过而失效**（`credentials.rs:499-505`）。

**成本**：探测完全 await 在用户 HTTP 请求里（`handlers.rs:581` → `service.rs:1757`），
**无任何超时包裹**，client 60s；今天最坏 240s。改 4 次往返 + 30s client = 120s，比现状更快。
批量导入并发 4（`service.rs:77`）会把它 ×4。

**矩阵**：`q.*` 存在几个区**未确认**（仓内无数字；唯一间接证据是 `external_idp_login.rs:760`
对 `PROFILE_PROBE_REGIONS` 6 个区打过 `q.*`）。建议 cli×{eu-central-1,us-east-1} 必做，
ide×两区仅在 cli 全 403 时试，其余 4 区放 config 开关默认关。
`KIRO_DIALOG_REGIONS`（**33** 项，非注释说的 34）是校验白名单含 gov/cn，不能当候选池。
