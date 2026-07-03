# KiroStudio —— 全量实施计划 v1（2026-07-03）

> 目标（用户定）：把 BACKLOG 里所有能做的都做完，跟进最新、融合各开源项目最佳实现、彼此不冲突、优质完美。
> 方法：分批推进，每批独立编译+测试+验证；破坏性/动地基的先门控隔离。决策留痕本文件。
> 已验证接线母体：`http_client::build_client`(唯一客户端构造入口) · `endpoint/mod.rs` trait(已有错误识别钩子) · `provider.rs:281 call_api_with_retry`(请求主循环) · `token_manager` report_*(已映射 CooldownReason) · `main.rs`(装配母体)。

---

## 指导原则

1. **不冲突优先**：动地基的（G-1 换 TLS 栈）一律用 **Cargo feature 门控**，默认构建不变（rustls，Docker 照常），开特性才编 wreq/BoringSSL。两套客户端并存，运行时按配置选。
2. **每批可验证**：一批 = 一组内聚改动 → `cargo build` + `cargo test` + 关键路径实测 → 提交 → 部署验证。失败先修不进下一批。
3. **先低风险高价值，再动地基**：先把纯本地增量做扎实（大量 S 级已有地基），传输层质变项单独攻坚 + 抓包验证。
4. **复用已有骨架**：cooldown 7 档 / affinity remove_by_credential / endpoint 错误钩子 / DisabledReason 都已在，多数是「接线」不是「新建」。

---

## 批次 1 —— 账号健康处置闭环（G-7 + G-8/G-9）· 低风险纯本地
> 地基已在，主要接线。直击「号死了不再反复打」。

- **1.1 响应级错误分类器**：在 `endpoint/mod.rs` trait 扩 `classify_error(status,body) -> ErrorClass`（复用现有 `is_monthly_request_limit`/`is_bearer_token_invalid` 模式）。区分 欠费/限流/暂停(suspended/banned 关键词)/认证(401/403/invalid_grant)/profile不可用(**软失败·绝不停用**·借 Kiro-Go 经验)/瞬态。
- **1.2 分类→冷却路由**：`provider.rs` 401/403/402/429 各分支改为调分类器 → 映射到已有 `CooldownReason`（激活至今未触发的 AccountSuspended/QuotaExhausted/AuthenticationFailed 三变体）。
- **1.3 禁用→清亲和闭环**：分类到不可恢复原因触发禁用时，调用已有 `affinity.remove_by_credential`（现仅 Admin 手动禁用时调，补到自动禁用路径）。
- **1.4 指数退避（G-8）**：凭据加 `backoff_level`，429 无 server Retry-After 时 `base<<level` 封顶，成功清零。
- **1.5 body 抠 Retry-After（G-9）**：429 body 探 `resets_at`/`resets_in_seconds`，有则用其精确值，无则回退 1.4。
- 验证：单测覆盖分类器各分支 + 冷却映射；构造假 body 验证路由。

## 批次 2 —— 可观测性核心（阶段C，用户已确认「全套移植」）· 中风险
> 硬依赖链，顺序不能错。引入 rusqlite = SQLite 迁移地基。

- **2.1 metering.rs（G-2前置）**：解析上游 `meteringEvent{unit,usage:f64}` → 真实 credits。StreamContext 加 credits/cache 字段 + `resolved_usage()`。
- **2.2 埋点**：`provider.rs:281` 请求级计时（Instant）+ 凭据id/模型/成功失败；`handlers.rs:639` 回填真实 token。用请求内关联 id 串联。
- **2.3 trace_db.rs**：引入 `rusqlite`(bundled)，traces + trace_attempts 两表(WAL)，per-request + per-attempt 落账，9 类 outcome，retention。查询 `/api/admin/traces`。
- **2.4 usage_stats.rs**：JSONL 按天分文件 + 内存环形桶(24×31时+31日)预聚合，`rebuild_from_logs` 冷启动重放。查询 overview/timeseries/by-model/by-credential。
- **2.5 请求速率环形缓冲（G-14）**：per-credential 20桶/10分钟，O(1) 写，喂概览页即时健康。
- **2.6 async 用量管道（G-15）**：mpsc + worker，sink panic 隔离。
- **2.7 前端图表页**：加 recharts + 新 Tab（timeseries/by-model/by-credential），概览页加 G-14 sparkline。
- **2.8 cache_metering.rs（可选·G-2补）**：仅当要非零 cache 列时做（滑窗前缀 + split_against_total）。

## 批次 3 —— 反代安全加固（G 安全）· 低风险公网必备
- **3.1** CORS 收敛（allow_origin 白名单，替换 Any）
- **3.2** IP 白名单中间件（可配置）
- **3.3** 入口限流（tower 限流层）
- **3.4** admin 路由 body 限制（补齐，anthropic 已有 50MB）
- 验证：无 key/超限/非白名单 IP 各返回预期状态码。

## 批次 4 —— 工程化质感（G-16/G-17/G-18）· 中风险
- **4.1 优雅停机（G-17）**：`axum::serve().with_graceful_shutdown(SIGTERM)`，在途流 drain。（先做，为后续后台 loop 铺底）
- **4.2 配置热重载（G-16）**：`notify` 监听 config.json → sha256 去重 → debounce → `ArcSwap<Config>` 原子替换。补齐「可编辑设置页」最后一环：保存后无缝生效不重启。
- **4.3 SQLite 迁移骨架（G-18/P2-1）**：照 cc-switch 的 `PRAGMA user_version` + SAVEPOINT 阶梯迁移，把 credentials/config/stats 从 JSON 迁 SQLite（trace_db 已带进 rusqlite）。**at-rest 加密**：无可抄件，自研（主密钥 env 注入、进库前 AES-GCM 加密，argon2 派生）。
- **4.4 timer-heap 主动 token 预刷新（G-12）**：min-heap 按 NextRefreshAfter，过期前刷新去突发。

## 批次 5 —— 防关联身份层（G-3/G-4/G-5/G-6 + G-2 多维指纹）· 中风险·需抓包
> ⚠️ 全部属「需验证外部行为」：移植后必须抓包确认上游真接受，别假设有效。

- **5.1 多维确定性指纹（G-2主体）**：`fingerprint.rs` 以 refresh_token 为种子派生 ~11 维（os/node/kiro版本/分辨率/时区/语言），同号不漂移。
- **5.2 UA/header 与指纹自洽（G-6）**：`endpoint/ide.rs` UA 从指纹版本字段拼出 + 每请求换 amz-sdk-invocation-id。**不是随机UA池**。
- **5.3 稳定化 device profile（G-3）**：per-credential profile 7天TTL，版本只升不降。
- **5.4 出站 header 擦洗（G-4）**：删 X-Forwarded-For/Via/Sec-Ch-Ua*/Accept-Encoding:zstd 等代理/浏览器 tell。
- **5.5 identity confuse（G-5）**：per-credential uuid_v5 重映射 conversationId/session 等，响应反向映射。
- 验证：抓包对比真实 Claude Code 流量的 header/字段。

## 批次 6 —— 传输层指纹伪装（G-1 uTLS JA3/JA4）· 最高风险·质变级·门控隔离
> 这是「绝对比老版好用」的核心，也是唯一动 TLS 栈的项。用 feature 门控确保不冲突。

- **6.1 spike 验证**：先加 `wreq`(Apache-2.0，留致谢) 依赖到 feature `impersonate`，写一个最小 spike：用 wreq + `Emulation::` 打一次 Kiro 上游，抓包确认 JA3/JA4 握手被接受、能拿到正常响应。**打不通就止损，不强推。**
- **6.2 feature 门控双客户端**：`http_client` 按 feature 分叉——默认 rustls reqwest（Docker 不变），`impersonate` 特性下走 wreq/BoringSSL。`client_for` 是唯一调用点，改动收敛。验证 socks5 代理在 wreq 下可用。
- **6.3 Docker 可选构建**：Dockerfile 加可选构建 arg，开 `impersonate` 才装 cmake/clang/perl/libclang。默认镜像不变重不变慢。
- 验证：抓包确认伪装生效 + 默认构建回归无影响。

## 批次 7 —— 协议扩展（P1-1/P1-2/P2-3 + G-23）· 中/大风险
- **7.1 翻译注册表骨架（G-23）**：`(From,To)->Transforms` 可插拔，先立骨架。
- **7.2 OpenAI 出口 /v1/chat/completions（P1-1）**：上游全复用，加 OpenAI↔Kiro 转换层 + SSE 编码器（arguments 字符串↔object、role:tool 归属、reasoning_content）。
- **7.3 主动 payload 截断（P1-2）**：发送前测字节超限丢最旧 history 轮 + 占位符。
- **7.4 /v1/responses（P2-3，按需）**：previous_response_id 链式历史。

## 批次 8 —— 运营面 + 后台体验（G-10~G-13/G-19~G-22 + P1-3/5/6/7）· 中风险
- **8.1 proxy_pool 代理池 + 健康检查（P1-3）** + **出口IP自检（G-6来源CLIProxyAPI getPublicIP）**
- **8.2 per-(凭据,模型)冷却（G-10）** + **健康评分加权选路（G-11，hj思路自研）**
- **8.3 亲和滑窗续期+反向失效（G-13）**
- **8.4 client_keys 客户端密钥分发（P1-5）** + **groups 分组（P1-6）**
- **8.5 凭据导入导出（P1-8）** + **图片缩放（P1-7，加 image/base64 依赖）**
- **8.6 前端**：配额进度条+倒计时（G-19）、批量多选（G-20）、暗色模式（G-21）、i18n（G-22，最后）

## 批次 9 —— 自研空白项 · 独立攻坚
- **9.1 新号预热 warmup**：11 项目零实现，纯自研。新号标 `warming`，首日限配额/低频探活成功 N 次再入主池。（与批次1雷暴防护协同）
- **9.2 Prometheus /metrics**：接 metrics crate。
- **9.3 SSE 推送到后台面板**：实时账号状态/日志流。

---

## 依赖与风险总表

| 批次 | 新依赖 | 风险 | 门控 | 抓包验证 |
|---|---|---|---|---|
| 1 | 无 | 低 | — | 否 |
| 2 | rusqlite,recharts | 中 | — | 否 |
| 3 | tower 限流 | 低 | — | 否 |
| 4 | notify,arc-swap,aes-gcm,argon2 | 中 | — | 否 |
| 5 | 无 | 中 | — | **是** |
| 6 | wreq,wreq-util(BoringSSL) | **高** | **feature `impersonate`** | **是** |
| 7 | 无 | 中 | — | 否 |
| 8 | image,base64 | 中 | — | 否 |
| 9 | metrics | 中 | — | 否 |

## 执行约定
- 每批完成：`cargo build && cargo test` 通过 → git 提交（规范 message）→ 部署 home-cloud:8991 验证 → 更新本文件标 ✅。
- 破坏性操作（force-recreate 容器/改 Dockerfile）先说明。不动 8990 旧 kiro-rs。
- 上游行为验证项（批次5/6）实测未通过则止损回报，不假设有效。
- 建议起点：**批次1**（低风险、地基已在、立即见效），随后批次2（用户已确认的阶段C）。
