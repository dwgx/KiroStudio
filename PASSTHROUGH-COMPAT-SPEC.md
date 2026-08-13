# 透传路径兼容性改造规格

给负责改代码的 Agent 的实施规格。目标是把 `custom_api` 代挂透传路径改成**语义等价于
「用户直接拿这个 key 打上游」**。

调查时间 2026-08-09。所有行号基于当时的 `dev` 分支，改前先 `git pull` 并重新核对。

## 为什么要改：一条真实故障

线上日志反复出现：

```
WARN kirostudio::kiro::passthrough
[透传] 上游请求失败(https://api.skiapi.dev/v1/messages): error sending request for url
```

时间点 10:35:22、10:46:05、11:00:25 —— **间歇性**，不是持续不可达。实测
`api.skiapi.dev` 是活的（POST 返回 `401` + `Via: 1.1 Caddy`），所以不是上游挂了。

根因在下面 P0。

## 参考实现：new-api 怎么做的

对照 `Calcium-Ion/new-api`（Go，业界用量较大的中继）的 `service/http_client.go`：

```go
var (
    httpClient   *http.Client                // 包级单例
    proxyClients = proxyHTTPClientCache{...}  // 按代理缓存 + sync.RWMutex
)
```

连接池默认值（`common/init.go:112-114`）：

| 参数 | 值 |
|---|---|
| `RELAY_MAX_IDLE_CONNS` | 500 |
| `RELAY_MAX_IDLE_CONNS_PER_HOST` | 100 |
| `RELAY_IDLE_CONN_TIMEOUT` | 90s |
| `TLSHandshakeTimeout` | 10s |
| `KeepAlive` | 30s |
| `ForceAttemptHTTP2` | true |

核心一条：**client 是单例/缓存复用的，永不每请求新建。**

---

## P0：透传每次请求都新建 reqwest Client

**这是 `error sending request` 的结构性根因，优先修这条。**

`src/kiro/passthrough.rs:67` 每次 `forward` 都调
`build_streaming_client_no_redirect`，`:250` 的 `fetch_upstream_models` 同样。
定义在 `src/http_client.rs:803-814`，只设了三项：

```rust
.read_timeout(Duration::from_secs(idle_secs))
.connect_timeout(Duration::from_secs(30))
.redirect(reqwest::redirect::Policy::none())
```

没有 `pool_idle_timeout`、`pool_max_idle_per_host`、`tcp_keepalive`。

后果：每请求重开 TCP + 重做 TLS 握手 + 重新解析系统代理（crate 开了 system-proxy
feature，`apply_tls_and_proxy` 未显式 `no_proxy`）。高并发下 ephemeral 端口与
TIME_WAIT 堆积，TLS 握手的 1-2 RTT 白加在 30s `connect_timeout` 里。

### 关键：主路径已经做对了，照搬即可

`src/kiro/provider.rs:846-856` 的 `client_for`：

```rust
fn client_for(&self, credentials: &KiroCredentials) -> anyhow::Result<Client> {
    let effective = credentials.effective_proxy(self.global_proxy.as_ref());
    let mut cache = self.client_cache.lock();
    if let Some(client) = cache.get(&effective) {
        return Ok(client.clone());
    }
    let client = build_streaming_client(effective.as_ref(), 720, self.tls_backend)?;
    cache.insert(effective, client.clone());
    Ok(client)
}
```

`provider.rs:775` 还做了预热。**只有透传路径漏了这层。**

### 要做的

1. 给透传加一个同构的按 `(effective_proxy, tls_backend)` 缓存的 client 获取函数。
   不要新造缓存机制，复用 `client_cache` 那套 `Mutex<HashMap>` 即可。
2. 在 `build_streaming_client_no_redirect` 上补连接池参数，取值对齐 new-api：
   `pool_idle_timeout(90s)`、`pool_max_idle_per_host(100)`、`tcp_keepalive(30s)`、
   `tcp_nodelay(true)`。
3. `fetch_upstream_models`（`:250`）同样改成复用。

**注意保留 `redirect::Policy::none()`** —— `http_client.rs:800-802` 的注释说明了原因：
公网中转站返回 `302 Location: http://169.254.169.254/...` 是典型 SSRF 绕过链，
禁重定向是纵深防护 C2，不要为了「更像原生」把它放开。

### 验收

- 连续打 100 个透传请求，`ss -tn | grep <上游IP> | wc -l` 应远小于 100（连接被复用）
- 观察 `error sending request` 是否消失或显著下降

---

## P1：透传 failover 循环没有墙钟预算

`src/kiro/provider.rs:1040-1150` 的 `try_custom_api_passthrough` 循环**无任何时间预算
检查**。连接层失败在 `passthrough.rs:124-133` 转成 502，而 `provider.rs:1089-1090` 的
`should_failover` 把 5xx（含这个 502）纳入换号。

每个 `forward` 带 30s `connect_timeout`。上游全挂时：每个号先烧 30s 再换下一个。

对比主路径的两道闸门（`src/kiro/provider.rs:42-50` 有详细注释）：

- `MAX_REQUEST_RETRY_BUDGET_SECS = 45`
- `ABSOLUTE_MAX_TOTAL_RETRIES = 12`

**透传循环没有同类闸门。** 叠上 sub2api 侧的 2 次重试 × 10 次账号切换，
`TASK-BUILTIN-RETRY.md:100-106` 已记录「单请求最坏放大到约 70~108 次上游调用」。

### 要做的

给透传循环加墙钟预算，与主路径同源（复用 `MAX_REQUEST_RETRY_BUDGET_SECS` 而不是
新定一个常量），并把 `connect_timeout` 从 30s 压到 8~10s —— 连接层 30s 对
「换号重试」场景太长，一个死号就吃掉大半预算。

**先读 `provider.rs:42-50` 的注释再动手。** 那里解释了为什么 429 刻意不在网关内重试
（小号池下一个卡住的请求能把整池压死）。加预算是收紧，不要顺手把 429 重试打开。

---

## P2：空流守卫只覆盖一半路径

`guard_empty_stream` 只在「deepseek 归一化 + `text/event-stream`」分支生效
（`passthrough.rs:177-191`）。纯透传流式分支 `passthrough.rs:210-219` 直接
`Body::from_stream(byte_stream)`，没有守卫。

后果：非 deepseek 代挂号的上游返回 200 但空流时，原样回给客户端 →
Claude Code 报 `Stream ended without receiving any events`，卡死 agentic 循环。

`passthrough.rs:145-165` 的注释已经把这个失效模式讲清楚了（空 chunk 在 HTTP 层
不可见，chunked body 会以「正常终止」收尾），守卫却没铺满。

### 要做的

把 `guard_empty_stream` 提到两个流式分支的公共路径上。

---

## P3：`anthropic-beta: context-1m` 丢失 → 1M 上下文在代挂路径失效

`passthrough.rs:113-122` 只设四个头：

```rust
.header(header::CONTENT_TYPE, "application/json")
.header("anthropic-version", "2023-06-01")
.header("x-api-key", key)
.header(header::AUTHORIZATION, format!("Bearer {key}"))
```

客户端发来的 `anthropic-beta`、`accept-encoding`、`x-stainless-*` 全丢。

对比主路径：`src/kiro/endpoint/ide.rs:142-146` 对 1M 变体**显式注入**
`anthropic-beta: context-1m-2025-08-07`，`model_catalog.rs:73` 说明 `[1m]` 变体依赖该头。

**所以 1M 变体走代挂时，上游拿不到这个头，1M 窗口不被放开。** 这是与主路径的实际
行为偏差，不只是规范洁癖。

### 要做的

按 new-api 的思路建**请求头白名单**转发，至少覆盖：

- `anthropic-beta`（关键，1M 上下文依赖）
- `accept`、`accept-encoding`
- `x-stainless-*`（Anthropic SDK 的客户端标识，部分上游按它判断行为）

必须**排除**的（由本层重写或不该转发）：`host`、`content-length`、
`transfer-encoding`、`connection`、`authorization`/`x-api-key`（已换成本凭据的）、
`x-forwarded-*`（`CLAUDE.md:149` 说明 `trustForwardedHeader` 保持 false 是刻意的）。

同时给 reqwest 开 `gzip` feature（`Cargo.toml:21` 目前没开）—— 若代挂上游是
Anthropic 直连，1M 响应默认 gzip 传输，不声明 `accept-encoding` 可能被降级。
这条是推测，不确定上游行为，改了不会变坏所以一并做。

---

## P4：响应头只透传 `content-type`

`passthrough.rs:138-143` 只读 `content-type`，构建响应时也只设它
（`:184` / `:201` / `:217`）。上游 429 时的 `Retry-After`、`x-ratelimit-*`、
`request-id` 全部丢弃。

后果：客户端收到 429 但拿不到 `Retry-After`，只能用自身固定退避，加剧与上游的
429 碰撞 —— 这与 P1 描述的重试叠乘互相放大。

### 要做的

响应头也按白名单透传：`retry-after`、`x-ratelimit-*`、`request-id`、
`anthropic-ratelimit-*`。保持排除 `content-length`（body 被改写/流式时会错）
和 `transfer-encoding`。

---

## P5：`fetch_upstream_models` 用无上限 `resp.json()`

`passthrough.rs:262-265` 直接 `resp.json().await`。

而本仓 `src/common/http_read.rs:4-16` 明确点名「`resp.json()` 会把整个响应体无上限
读进内存」是 OOM 反模式，并收口了 `read_json_capped` 供全仓调用。

同一个文件里 `forward` 的非流式响应就用了 `read_body_capped` 加 32MiB cap
（`passthrough.rs:195`），这里却裸奔。外部可控的上游模型列表（被劫持/DNS 投毒）
可无上限放大内存。

### 要做的

换成 `read_json_capped`，cap 取一个模型列表够用的值（1~4 MiB）。

---

## 不要改的（刻意设计，别当 bug 拆）

- **`redirect::Policy::none()`** —— 防 302 跳内网的 SSRF，见 `http_client.rs:800-802`
- **`trustForwardedHeader = false`** —— `CLAUDE.md:149` 解释了：sub2api 的
  `allowedHeaders` 里没有 `X-Forwarded-For`，开了也拿不到真实 IP，反而会让
  IP 黑名单封掉全部流量
- **429 不在网关内重试** —— `provider.rs:42-50`，小号池下会把整池压死
- **请求体非流式（`Bytes` 缓冲）** —— 透传需原样转发 + 需先解析判断是否 deepseek
  归一化，与主路径一致，不是偏差

## 已核实为「不是问题」的

调查中怀疑过、验证后确认实现正确，不用改：

- **空 body 不会进入透传** —— `src/admin/handlers.rs:1393` 先 `serde_json::from_slice`
  解析，失败直接 400。所以「透传发出 0 字节 body」的假设不成立
- **非 failover 的 4xx 上游响应体原样流式回传** —— 正确
- **`Bytes::clone` 是浅拷贝（Arc）** —— 不构成额外内存拷贝；真拷贝来自 JSON 结构化
  副本，属正常开销

## 建议顺序

P0 → P1 → P2 是一组（都关系到「请求能不能成功」），P3 → P4 是一组
（关系到「行为是否与原生等价」），P5 独立。

P0 单独修完就应该能看到 `error sending request` 明显下降，建议先只改 P0 观察一轮
再继续，便于归因。
