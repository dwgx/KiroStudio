# 参考仓库总结：ZyphrZero/kiro.rs（v0.7.6 @ d3cec44）

> 2026-08-15 全仓吃透（4 路并行分析 + codegraph 索引 /tmp/ref-zyphr/.codegraph）。
> 与本仓同源（同是 hank9999/kiro.rs 分支）。zyphr 强在「协议面与运营面」，我们强在「调度面与安全面」。

## 概览

三协议兼容（Anthropic Messages / OpenAI Chat Completions / OpenAI Responses）的 Kiro 网关，47193 行，~572 测试。模块：anthropic/（协议面，含 cache_metering.rs 磁盘持久化缓存计量）、kiro/（token_manager.rs 6884 行单文件核心）、admin/（client_keys/groups/proxy_pool/binary_update 等 13 模块）、model/（custom_models.rs 配置化模型注册表）。

## 独有/先进机制（我们缺失或更弱）

| # | 机制 | 位置 | 说明 |
|---|---|---|---|
| 1 | **模型感知正向路由** | token_manager.rs:1529 cached_model_support 三态（Confirmed/Unsupported/Unknown）+ :1868 discovery_rank 排序 | /v1/models 实时查询号池模型目录；路由时已确认含目标模型的号优先、明确不含的号跳过、未加载的仍允许。**正向最优分配 vs 我们的负向黑名单**——直接命中 mixed pool 首次打错号痛点 |
| 2 | **客户端 Key + 分组租户隔离** | admin/client_keys.rs + admin/groups.rs | sk- Key 绑定分组过滤可用凭据；系统 Key id=0 可轮换；groups 引用校验防 typo。对应我们 P3-35（csk_ 待决） |
| 3 | **GPT 双通道 reasoning effort** | kiro/model/requests/kiro.rs:58-78 + converter.rs:514-557 | 同一结构体 output_config.effort（Claude）+ reasoning.effort（GPT-5.6 sol/terra/luna 六档）。我们只有 Claude 单通道 |
| 4 | **OpenAI 显式会话标识亲和** | anthropic/openai.rs:62-86 | 四来源提取 UUID（prompt_cache_key → x-session-affinity → x-client-request-id → session_id）作 conversationId。我们的 openai 路径不认客户端会话头 |
| 5 | **customModels 配置化模型映射** | model/config.rs:39-68 + custom_models.rs | config.json 加模型映射零发版。我们的 model_mapping 是代码内置 |
| 6 | **RPM 全超限类型化 429 + 最早释放 Retry-After** | token_manager.rs:1728-1847 | 按滑动窗口时间戳精确推导剩余秒数，类型化 429 |
| 7 | **Token 轮换从源文件重载** | token_manager.rs:2280-2371 | refreshToken invalid_grant 时先读源文件（IDE 侧 rotation），有新版直接加载重试，~90 行 |
| 8 | **最早类型化 429 优先** | provider.rs:988-1016 take_rate_limit_error | 重试循环保留最早 429，不让 generic 错误覆盖 Retry-After |
| 9 | **客户端格式错误防 503 风暴** | provider.rs:796-807 | 上游 5xx 返回「messages 违反协议」类错误不重试不换号不计失败 |
| 10 | **批量导入 SSE + 失败回滚 + 客户端中断感知** | admin/handlers.rs:233-328 | buffer_unordered(8) 逐条推送，验活失败自动回滚删除，前端关对话框=abort |
| 11 | **ImageBudget 入口诊断** | handlers.rs:263-295 | 入站 O(N) 数 inline base64 图，超 IMAGE_BUDGET_WARN_BYTES 预警，~30 行 |
| 12 | **websearch loop 空结果重试闸门** | websearch_loop.rs:209-225 | 空 tool_result 续轮先 Retry（MAX_EMPTY_TOOL_RESULT_RETRIES）再 Fail，防上游退化空转 |
| 13 | **Responses namespace 递归展开 + web_search 原生注入** | responses.rs:385-528 / :362-384 | Codex collaboration 工具组展开；声明 web_search 时网关内代答 |
| 14 | **CacheMeter 磁盘持久化** | cache_metering.rs:123,204-233 | 跨重启持久化，我们 cache_fingerprint 纯内存 |

## 我们更优（zyphr 短板）

- 调度器：我们 12 位排序键/9 种冷却/EWMA+熔断/族级连坐/吸收层/共享预算 —— zyphr 只有连续失败计数+7 种 DisabledReason
- 存储：我们 XChaCha20-Poly1305 加密，zyphr 明文 JSON
- 刷新锁：我们 per-credential，zyphr 全局一把
- 自愈：我们 trigger_count 递增退避 + 持久化，zyphr 固定间隔 + throttled_until 不持久化（重启丢风控窗口）
- 失败转移：我们有 MAX_SUSPICIOUS_FAILOVERS_PER_CALL 防线性扫全池，zyphr 没有

## 可借鉴优先级

- **P0**：模型感知正向路由（需与黑名单/严格语义双轨设计）
- **P1**：customModels 配置化映射；客户端 Key+分组；token 轮换源文件重载
- **P2**：GPT reasoning 双通道；OpenAI 会话标识亲和；admin 真实模型测试；RPM 精确 Retry-After；最早 429 优先；上游明确 Retry-After 时禁内部换号
- **低**：ImageBudget 诊断、空结果重试闸门、Responses 注入/namespace、CacheMeter 持久化

## 发现的问题（zyphr 侧）

- OpenAI 流式是**合成 SSE 非实时**（README:285 自认），TTFB 差
- 凭据/Key 明文存储；全局刷新锁；自愈固定间隔；无健康分概念（混池半死号频繁被选）
- provider.rs:559-563 DEBUG 打印完整请求头（含 Authorization）——我们自查是否有同类

## 可借鉴（anthropic 层专项）

- **web_search 判定收紧 OR→AND**：zyphr websearch.rs:105-109 `name=="web_search" && tool_type.starts_with("web_search_")`，有测试钉死「普通自定义工具叫 web_search 不触发原生快路径」；**我们 websearch.rs:278-283 是 OR**——客户端自定义 MCP 工具恰好叫 web_search 会被误吞进快路径/回灌循环（MAJOR 级潜在缺陷，待确认）
- **input_schema 用 BTreeMap**（types.rs:247，key 字典序稳定序列化保 prompt cache 前缀稳定）；我们 HashMap 仅指纹层 canonical_json 兜底，**转发上游字节序仍可能抖动**
- convert_tools 小差异：按小写名去重 + fs_append 隐藏
