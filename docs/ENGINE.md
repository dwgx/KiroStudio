# 引擎理解 —— hank9999/kiro.rs 转换链路（基于真实源码，2026-06-30）

> 已读源码：main.rs / kiro/provider.rs(519) / kiro/token_manager.rs(2599) / machine_id.rs(282) /
> anthropic/{converter,stream} / kiro/parser/。所有结论标"已验证"，未读到的标"待确认"。
> 后端语言已定 = **Rust**（地基与3个韧性fork全是Rust+Axum，换语言=丢弃全部增量与reference价值）。

## 1. 整体请求链路（已验证）

```
AI客户端 POST /v1/messages (Anthropic协议)
   │
   ▼ [src/anthropic/router.rs → handlers.rs]  鉴权(common/auth.rs) + 限流
   │
   ▼ [src/anthropic/converter.rs 1793行]
   │   Anthropic 请求 → Kiro 的 ConversationState 结构
   │   (system/messages/tools → conversationState.currentMessage.userInputMessage)
   │
   ▼ [src/kiro/provider.rs::call_api_with_retry]   ★引擎心脏
   │   1. token_manager.acquire_context(model) → 选凭据(priority/balanced)
   │   2. machine_id::generate_from_credentials() → 注入机器码
   │   3. endpoint_for() → 选 Region 端点
   │   4. client_for() → 取该凭据的 HTTP client(含凭据级代理)
   │   5. endpoint.transform_api_body() + decorate_api() → 组装上游请求
   │   6. .send() → 打 Kiro 上游(AWS CodeWhisperer)
   │   7. 按 status 分类处理(见下方状态机)
   │
   ▼ [src/kiro/parser/ decoder/frame/header/crc]
   │   Kiro 返回的是 AWS event-stream 二进制帧 → 解码为事件
   │
   ▼ [src/anthropic/stream.rs 1989行]
   │   Kiro 事件 → Anthropic SSE (message_start/content_block_delta/...)
   │
   ▼ 流式回传给客户端
```

## 2. 凭据状态机 / 负载均衡（已验证，token_manager.rs）

- `CredentialEntry { disabled, disabled_reason, success_count, ... }`
- `DisabledReason` 枚举：`InvalidConfig` / `TooManyFailures` / `QuotaExhausted`(402)
- **select_next_credential**：
  - `priority` 模式(默认)：选 `min(priority)` 的可用凭据，固定 current_id
  - `balanced` 模式：选 `min(success_count, priority)`，每请求重新选不固定
- **自动恢复**：当所有凭据因 `TooManyFailures` 被禁用时，自动清除禁用重试（token_manager.rs:801）—— 防止瞬态故障锁死全池

## 3. HTTP 状态码处理策略（已验证，provider.rs:358-491）★韧性精华

| 状态 | hank9999 处理 | 是否切换凭据 |
|---|---|---|
| 2xx | report_success | - |
| 402 + 额度耗尽 | report_quota_exhausted → 禁用该凭据 + 故障转移 | ✅切换 |
| 400 | 直接 bail（请求问题，重试无意义） | ❌ |
| 401/403 + token失效 | **先 force_refresh 一次**(每凭据仅一次)，失败才 report_failure 切换 | 刷新后重试/切换 |
| 408/429/5xx | 重试但**不禁用不切换**（防瞬态把全池锁死） | ❌ 仅退避重试 |
| 其他4xx | 直接 bail | ❌ |
| 网络错误 | 退避重试，不禁用（防网络抖动误禁全池） | ❌ |

- 退避：指数 200ms→2s + 1/4抖动（retry_delay）
- max_retries = min(总凭据数 × MAX_PER_CRED, MAX_TOTAL)

> 评价：hank9999 这套"瞬态错误不锁死凭据"的策略本身就是**雷暴防护的核心思想**，已相当成熟。
> 我之前以为雷暴防护要靠 M-JYuan 补——纠正：hank9999 基线已有，M-JYuan 是在此之上加冷却分类。

## 4. 机器码注入（已验证，machine_id.rs）
- `generate_from_credentials(cred, config)` 优先级：凭据级 machineId > 全局配置 > 从凭据派生(sha256) > 兜底
- 注入点：provider.rs:304，每次请求组装时生成，带给上游
- **这就是"机器码=凭据级字段、网关注入"的技术依据**

## 5. Kiro 二进制帧协议（已验证，kiro/parser/）
- Kiro 上游返回 AWS event-stream 格式：`header.rs`(头) + `frame.rs`(帧) + `crc.rs`(校验) + `decoder.rs`(解码)
- 这是协议转换最硬核的部分，**直接复用 hank9999 实现，不重写**

## 6. 模块清单（hank9999 基线，全部 MIT 可复用）
```
src/anthropic/  converter(转换) stream(SSE) handlers router middleware websearch types
src/kiro/       provider(调度) token_manager(凭据池) machine_id parser/(帧解码)
                endpoint/(Region端点) model/(数据结构)
src/admin/      router handlers service types middleware  (管理API)
src/admin_ui/   router  (内嵌React后台静态资源)
src/common/     auth(鉴权)
```

## 7. 待确认（阶段一研读时验证）
- [ ] converter.rs 对 tool_use / thinking / image 的具体映射（GreyGunG/Kiro-RS-Tool 专门强化了 tool schema，可对比）
- [ ] stream.rs 的 SSE 事件完整序列是否覆盖 Claude Code 所有场景
- [ ] endpoint/ 的多 Region 切换触发条件
- [ ] admin-ui 现有页面清单（login-page 已确认存在）
