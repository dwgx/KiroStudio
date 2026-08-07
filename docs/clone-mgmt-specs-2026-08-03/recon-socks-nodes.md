I have everything needed.

## 1. 现有 `/proxy/test` 完整契约

**路由**：`src/admin/router.rs:142` → `POST /api/admin/proxy/test`（在 `admin_auth_middleware` layer 内侧，需 adminKey）。

**请求体** `ProxyTestRequest`（`src/admin/handlers.rs:187-198`，**camelCase**）：

| JSON 字段 | Rust | 必填 | 说明 |
|---|---|---|---|
| `proxyUrl` | `proxy_url: String` | ✅ | 空串 / `"direct"`（大小写不敏感）= 测直连。可内嵌账密 |
| `proxyUsername` | `Option<String>` | `#[serde(default)]` | 显式字段**优先**覆盖 URL 内嵌 |
| `proxyPassword` | `Option<String>` | `#[serde(default)]` | 同上 |

**响应体** `ProxyTestResponse`（`handlers.rs:201-212`，camelCase）：`ok: bool` / `latencyMs: u64` / `exitIp: String|null` / `error: String|null`。**永远 HTTP 200**，失败靠 `ok=false` 表达（`handlers.rs:220-223` 注释）。

**实际行为**（`handlers.rs:225-306`）：
- `split_proxy_credentials` 拆内嵌账密 → `ProxyConfig::new(clean_url).with_auth(u,p)`（`handlers.rs:236-255`）
- `build_client(cfg, 10, tls_backend)` —— **超时 10 秒**（`handlers.rs:258`）
- GET **硬编码** `PROXY_TEST_PROBE_URL = "https://api.ipify.org?format=json"`（`handlers.rs:218`）；非 2xx 判失败；解析 `{"ip":...}` 填 `exitIp`，解析失败不影响 `ok`
- SSRF 设计：目标固定，请求方只能控制"走哪个代理"

前端已封装：`admin-ui/src/api/ops.ts:227-234`（`proxyTest({proxyUrl, proxyUsername?, proxyPassword?})`）、组件 `admin-ui/src/components/proxy-test-button.tsx:23-45`（props 用 `proxyUrl`）。**分身管理页直接复用这两个，无需新端点。**

## 2. 节点列表存哪 → 选 ② 独立文件 `socks_nodes.json`

存在 `token_manager.cache_dir()`（= credentials.json 同目录，`token_manager.rs:3787`），与 `trash.json` 同级同规格。

理由（三条都是"改成别的会真出事"）：
- **密码必须走 at-rest 加密**。`config.json` 的写入路径（`config.rs:987` 那条注释自认）是**明文**，`encrypt_credentials_at_rest` 只作用于 credentials/trash 两个文件。SOCKS 密码放 config.json = 明文落盘且被 OTA/备份带走。放独立文件可直接复用 `secret_store::{key_path_for, maybe_decrypt_to_string, encode_for_disk}`，抄 `persist_trash`（`token_manager.rs:3851-3886`）即可。
- **回滚兼容**。config.json 多一个数组字段，旧版本 `Config` 无该字段 → serde 默认 `deny_unknown_fields`? 本仓没开，但**旧版本会静默丢弃并在下次 `save` 时抹掉节点表**。独立文件旧版本压根不读，回滚后节点表完好。
- **不能选 ③**。凭据的 proxy 字段是"这个号用哪个代理"，节点表是"有哪些代理可选"。③ 下"添加一个还没绑号的节点"无处安放，且改一个节点要遍历改 N 个凭据。

热重载：节点表**不进热路径**（只在 UI 选择和分身生成时读），内存态 `parking_lot::Mutex<Vec<SocksNode>>` 即可，不需要 ArcSwap 三层镜像。

## 3. 节点数据结构 patch

新文件 `src/kiro/model/socks_node.rs`（或放进 `src/admin/types.rs`，前者更贴 `credentials.rs` 的位置）：

```rust
/// 一个可复用的 SOCKS/HTTP 代理节点（「分身管理」页维护的节点表）。
///
/// 与 `KiroCredentials.proxy_*` 的关系：这里是**候选池**（有哪些节点可用），
/// 凭据字段是**绑定结果**（这个号走哪个节点）。生成分身时从池里取节点写进凭据。
/// 密码随文件走 at-rest 加密（与 credentials/trash 同开关同密钥），故绝不放 config.json。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SocksNode {
    /// 节点 id（进程内自增，持久化后保持稳定；删除不复用）
    pub id: u64,
    /// 展示名（如 "US-West-1"）；为空时前端回落显示 host:port
    #[serde(default)]
    pub name: String,
    /// 代理 URL：socks5://host:port / http://host:port。可内嵌账密（读入时应拆到下面两字段）
    pub url: String,
    /// 代理用户名（可选）
    #[serde(default)]
    pub username: Option<String>,
    /// 代理密码（可选，落盘随文件加密）
    #[serde(default)]
    pub password: Option<String>,
    /// 是否可用于分配（关掉的节点不参与「一键生成分身」，但保留记录）
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 最近一次测速结果（沿用 /proxy/test 的语义；None = 从未测过）
    #[serde(default)]
    pub last_test: Option<SocksNodeTest>,
    /// 创建时间（Unix 秒）
    #[serde(default)]
    pub created_at: u64,
}

/// 最近一次 `/proxy/test` 的结果快照（前端在节点卡片上直接渲染）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SocksNodeTest {
    pub ok: bool,
    #[serde(default)]
    pub latency_ms: u64,
    #[serde(default)]
    pub exit_ip: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    /// 测试时刻（Unix 秒），用于前端显示「N 分钟前」
    #[serde(default)]
    pub tested_at: u64,
}

fn default_true() -> bool {
    true
}
```

注意：`enabled` 用 `default = "default_true"` 而非 `#[serde(default)]`——后者对 bool 默认 `false`，会让**回滚再升级后所有节点变禁用**，池空 → 一键生成分身全部落直连。

**没有枚举变体**，故不涉及 `#[serde(other)]` 兜底。

## 4. CRUD 端点设计

全部挂在 `src/admin/router.rs` 现有 `/proxy/test` 附近（同一 auth layer 内）：

```rust
// SOCKS 节点表（「分身管理」页）：候选代理池的 CRUD;测速沿用上面的 /proxy/test
.route("/socks-nodes", get(list_socks_nodes).post(create_socks_node))
.route("/socks-nodes/{id}", put(update_socks_node).delete(delete_socks_node))
```

| 方法 路径 | 请求体 | 响应 |
|---|---|---|
| `GET /api/admin/socks-nodes` | — | `{ "nodes": [SocksNode] }`，**password 一律替换为 `null`**（响应体绝不回吐密码，与凭据端点同口径） |
| `POST /api/admin/socks-nodes` | `CreateSocksNodeRequest` | `{ "node": SocksNode }`（password 已抹） |
| `PUT /api/admin/socks-nodes/{id}` | `UpdateSocksNodeRequest`，全字段 `Option`，**`password: None` = 不改，`Some("")` = 清除** | 同上 |
| `DELETE /api/admin/socks-nodes/{id}` | — | `SuccessResponse::new("已删除节点 …")`（与 `purge_trash_batch` 同风格，`handlers.rs:171-177`） |

```rust
/// POST /api/admin/socks-nodes 请求体。
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateSocksNodeRequest {
    #[serde(default)]
    pub name: String,
    /// 代理 URL（socks5://host:port，允许内嵌账密）
    pub url: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}
```

前端发送形如 `{"name":"US-1","url":"socks5://1.2.3.4:1080","username":"u","password":"p","enabled":true}`（camelCase）。`PUT` 体同名字段全 `Option<...>` + `#[serde(default)]`。

**测速结果回写**：不新增端点。前端调既有 `proxyTest`，拿到结果后带进 `PUT /socks-nodes/{id}` 的 `lastTest` 字段。这样零新增探测逻辑，也符合"沿用现有容器"。

## 5. SSRF 判断：要校验，但**只校验 IP 层、不能复用 `validate_outbound_url` 原样**

结论：新增节点/改 URL 时，**必须**对代理的 host:port 做禁止段检查，用 `SsrfPolicy::AdminConfigured`。

理由：
- 危害真实存在。填 `socks5://127.0.0.1:1080` 或 `socks5://169.254.169.254:1080` 后，网关会主动向该地址发起 TCP 连接并按 SOCKS 协议交互——这就是标准 SSRF 原语，攻击者（拿到 adminKey 的低权用户）可用 `latencyMs` / `error` 文本做内网端口扫描侧信道。`/proxy/test` 现有的"目标 URL 硬编码"防线**只管目标不管代理**，代理侧完全没校验（`handlers.rs:236-255` 直接把 `clean_url` 塞进 `ProxyConfig`）。
- 但 `validate_outbound_url` 不能直接用：它的 scheme 白名单是 `https`（+可选 `http`），会把 `socks5://` / `socks5h://` 一律拒掉。
- 用 `AdminConfigured` 而不是 `Strict`：代理地址来自管理员亲手输入，且 RFC 2544 段（`198.18.0.0/15`）豁免是本仓刻意为之——已知问题 #19 记录过，国内开发者的 Clash/Surge fake-IP 池正落在该段，**用 Strict 会把合法本机代理拒掉**。

推荐做法（新增小函数，不动 `ssrf.rs` 现有语义）：

```rust
/// 校验一个**代理地址**（而非请求目标）不指向内网/环回/元数据段。
///
/// 与 [`validate_outbound_url`] 的区别：scheme 白名单是代理协议
/// （socks5/socks5h/http/https），不是被请求 URL 的协议。IP 层复用同一套禁止段判定。
///
/// 用 [`SsrfPolicy::AdminConfigured`] 而非 Strict：地址由管理员在面板亲手填写，
/// 且 RFC 2544 段豁免是必要的——国内 fake-IP 模式代理（Clash/Surge）正落在
/// 198.18.0.0/15，用 Strict 会把合法本机代理判成攻击（见已知问题 #19）。
pub async fn validate_proxy_address(url: &str) -> Result<(), String> {
    let scheme = url
        .split_once("://")
        .map(|(s, _)| s.to_ascii_lowercase())
        .ok_or_else(|| "代理地址缺少 scheme".to_string())?;
    if !matches!(scheme.as_str(), "socks5" | "socks5h" | "http" | "https") {
        return Err(format!("代理 scheme 不被允许(socks5/socks5h/http/https): {scheme}"));
    }
    let (host, port) = parse_host_port(url)?;
    match tokio::net::lookup_host((host.as_str(), port)).await {
        Ok(iter) => {
            for sa in iter {
                if is_forbidden_ip_with(sa.ip(), SsrfPolicy::AdminConfigured) {
                    return Err(describe_rejection(sa.ip()));
                }
            }
            Ok(())
        }
        // 与 validate_outbound_url 同口径：DNS 失败是网络问题不是攻击，放行。
        // IP 字面量（主要攻击向量）不走真实 DNS，上面已拦。
        Err(_) => Ok(()),
    }
}
```

`parse_host_port`（`ssrf.rs:~260`）目前是私有的，同模块内调用无需改可见性。

**校验点放哪**：`POST/PUT /socks-nodes`（写入时主防线，与 custom_api base_url 同模式）。是否同时在 `/proxy/test` 加，取决于是否接受"该端点仍可做内网探测"——建议加，成本是一次 `lookup_host`，且失败路径已有 `ok=false` + `error` 结构可承载拒绝原因，不破坏契约。

**「移除它即失败」的测试**：`validate_proxy_address("socks5://169.254.169.254:1080")` 必须 `Err`；`"socks5://198.18.0.1:1080"` 必须 `Ok`（守住 #19 的豁免）；`"https://1.1.1.1:443"` 必须 `Ok`；`"ftp://x:21"` 必须 `Err`。四条都构造得出，改动该做。

## 一个次要缺口（不在本任务范围，仅备案）

`GET /socks-nodes` 抹密码后，前端编辑节点时拿不到原密码，若前端把空密码原样 `PUT` 回来会**清掉密码**。故 `UpdateSocksNodeRequest.password` 的语义必须是 `None`=不改、`Some("")`=清除（已在上面第 4 节写明），前端表单**未触碰密码框时不能带 `password` 键**。这条不落实就是"编辑节点名导致所有分身掉线"。