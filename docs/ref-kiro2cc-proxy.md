# 参考仓库总结：TsinHzl/kiro2cc-proxy（v2.9.6 @ 252b5ee）

> 2026-08-15 全仓吃透（4 路并行分析 + codegraph 索引 /tmp/ref-k2cc/.codegraph）。
> 与本仓同源（kiro.rs → 各自演进），k2cc 是更成熟的产品化版本（v2.9.6，双前端、openspec 规范驱动）。

## 定位与架构

Anthropic Claude API → Kiro API 反向代理（Claude Code 消费 Kiro 账号模型），MIT。axum 单二进制 = anthropic/ 协议转换 → kiro/ 多账号故障转移 → parser/ AWS event-stream → SSE 回写，+ admin/user 双 REST API + 双前端 + cache 指纹 + 用量/限流/失败日志。

## 核心机制（重点）

### 缓存 fingerprint 体系（四层降级链 cache/mod.rs:33-68）
- L1 metering 真值（上游 SSE 直取）→ L2 prefix 估算（当前主路径，token.rs:273-306）→ L3 fingerprint 命中 → L4 ratio 模拟（三角分布）
- clamp_to_total 保两条不变式：5m+1h==creation、read+creation<=total
- FingerprintTracker（fingerprint.rs 849 行）：账号级累积 SHA-256 断点链（S:/T:/M: 段），首段不匹配即 break（保守前缀语义），cap=85% total，5m/1h 双 TTL tier + ephemeral_1h_ratio 配置化拆分，30s 后台 evict
- canonicalize：tool_use input 递归排序、image/document 只 hash 前 8 字节、文本 trim
- **CLIENT_TOKEN_DISPLAY_SCALE = 0.6657**：对外 SSE 缩放展示（触发 Claude Code compact）、对内真实记账——双轨记账
- k_ref credits 换算（usage.rs:129）：sonnet=1.43/opus-4.6=1.90/opus-5+=2.36，配套 calibrate_kref.py 回归校准 + test-cache.sh 真实命中率测试

### 端点/region
- 4 端点 = 4 独立限流桶（Ide/Runtime/Codewhisperer/Amazonq），(credential, endpoint) 桶级 429 封禁 30s（endpoint.rs:120+），attempt 偏移轮询跳过封禁桶，全桶封禁硬错误不静默切账号

### 其他
- Sticky cache：agentContinuationId → 账号绑定 60min TTL（token_manager.rs:1301-1393）
- rotation_bias：429 后账号 bias+1 纯排序降温（:1805-1811）
- CallContext 三元组绑定防并发竞态（:856-866）
- RPM 门控：精确等待到 slot 释放上限 5s（provider.rs:1006-1040）
- 错误翻译：QUOTA_EXHAUSTED_ALL → **402 排 429 之前**；429 透传 Retry-After:5；400 上下文超限提示压缩
- 空响应判定：input>28% 窗口 或 output<30 无工具 → 上下文过大提示（防 agentic 循环卡死）
- machine_id UA 伪装、profileArn 每请求注入/移除、PDF 手写 Tj/TJ 解析

## 产品面

- **双前端**：admin-ui（13 面板：账号池/子 API Key/三级用量钻取/实时日志终端/故障日志/IP 归属地/14 天 credits 趋势）；user-ui（5 组件：子 key 登录 + 自助查额度/用量/按模型分组/请求日志）——「卖 key 给用户」商业闭环
- **子 API Key**（model/api_key.rs + admin/api_keys.rs）：spendingLimit（usd/credits 双单位）+ durationDays 懒激活 + boundCredentialIds 绑定账号子集（⚠️ 2026-08-15 验证：子 key 鉴权是普通 `==`，**非**常量时间；constant_time_eq 仅用于 admin 密码——移植时我们全程复用 constant_time_eq）
- 部署 5 形态：本地脚本（配置向导）/Linux systemd/三阶段 Docker/fly.io（256MB 按需启动零成本）/Zeabur+NewAPI 分发
- 工程化：openspec（12 specs + 19 归档变更完整决策史）、docs/ 中文体系（源码全景解析 1950 行 + 代码速查表 897 行）、test-cache.sh（credits 反推法）、calibrate_kref.py、CI（tag 跳过 beta/双平台 digest manifest merge/llvm-cov）

## 独有/先进点（按价值）

1. 四层 cache 降级链 + 0.6657 双轨记账（行业最精细）
2. 4 端点桶级 429 隔离
3. Sticky cache 会话级账号绑定
4. 子 API Key 商业闭环 + user-ui
5. 跨自然月配额自动恢复（recover_expired_quota_disables，代价仅一次 402）
6. **手动/自动禁用持久化分层**：credentials.json 只写 Manual，自动禁用写 kiro_stats.json（防重启后自动禁用被当手动钉死）——**我们 token_manager.rs:1575 注释已承认此问题**
7. h[0] 冻结三件套（PREV_H0 + cch 计费哈希归一化 + system-reminder 冻结）——同会话 system 永久冻结保前缀稳定
8. Tool Search defer_loading 指纹扩展
9. rotation_bias（~30 行）
10. 可观测性全家桶（throttle/failure 日志 + SSE 实时流 + ip2region 离线归属地）

## 可借鉴优先级

- **P0**：跨月配额恢复 + 禁用持久化分层（消除已知运维痛点）；rotation_bias
- **P1**：continuationId sticky；h[0] 冻结三件套；Tool Search 指纹扩展；子 API Key + user-ui；缓存测试方法论（真实账单反推）
- **P2**：ephemeral_1h_ratio 配置化；模型动态列表四层回退；PDF 提取；端点桶封禁细节核对；功能→行号速查表；废弃文档标注规范

## 发现的问题（k2cc 侧，移植时规避）

- **MAJOR**：无优雅关闭（Ctrl+C 丢最多 5s 记录）；fingerprint update 复活过期断点（请求间隔 <30s 时 TTL 形同虚设——我们「计算即记录」天然免疫）；流内 Event::Error/Exception 被吞（错误后少量输出会以 end_turn 蒙混）；PREV_H0 全局静态无租户隔离
- **安全**：test-cache.sh 硬编码真实凭据（公开仓库）；install_server.sh 以 root 运行
- MINOR：账号表永不移除空表泄漏；get_k_ref contains 匹配脆弱（未知/新版本 sonnet 系落 1.43 档低估 credits——⚠️ 2026-08-15 验证：`claude-opus-4.7.1` 例不成立，任何含 opus 的名字都命中 2.36 兜底档；脆弱方向是**未知 sonnet/haiku 落最贵档 1.43 高估**）；0.6657 全模型生效误导第三方客户端；decoder 双 API 行为不一致；try_recover 宣称逐字节扫描实际最多跳 4 字节；全局刷新锁；流式路径未接 fingerprint；近期知识注入硬编码 2025-03 时事
