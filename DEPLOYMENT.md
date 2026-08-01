# KiroStudio 生产部署记录

## 2026-08-01 v9 部署（当前生产）

### 部署时间
- 2026-08-02 00:04 CST

### 镜像信息
- **镜像**: `kirostudio:v9`
- **版本**: v0.7.44
- **大小**: 42.3MB
- **对应 commit**: `2bdb4d7`

### 容器配置
```bash
docker run -d \
  --name kirostudio \
  --restart unless-stopped \
  -p 8990:8990 \
  -v /Users/a1234/Desktop/项目/KiroStudio/config:/app/config \
  -e RUST_LOG='info,kirostudio::kiro::provider=debug,kirostudio::anthropic::converter=debug,kirostudio::anthropic::truncate=info,kirostudio::anthropic::sanitize_history=info' \
  kirostudio:v9
```

**注**：v9 移除了 `KIRO_TOOL_TRACE` 探针（v8 抓根因用，已完成使命）。

### 本次修复：429 + INSUFFICIENT_MODEL_CAPACITY 走容量路径

Commit `2bdb4d7`。修复遗漏的容量信号，防止模型容量不足被误处置成凭据限流。

**问题**：生产实证 2026-08-01 14:52
```
429 {"reason":"INSUFFICIENT_MODEL_CAPACITY"}  ← 模型容量不足
→ RPM自动降档 270→135
→ 凭据进入瞬时冷却 duration_secs=15        ← 误处置
→ 所有可用凭据均在冷却，返回 429+Retry-After=14
```

`INSUFFICIENT_MODEL_CAPACITY` 是模型容量不足信号（类似 503 + `MODEL_TEMPORARILY_UNAVAILABLE`），
但此前只认 503 形式。429 带此信号时掉进通用限流分支，被当成**凭据被限流**误处置 —— 冷了一个健康的号。

**三重误处置**：
1. 冷却凭据是错的 —— 模型容量不足是全局问题，所有凭据对同一过载模型完全等价，切换无意义
2. 端点回退也救不了 —— 三个端点后面是同一份模型容量，整链全 429
3. 被凭据数放大 —— 生产只启用 1 个凭据，任何冷却都等于全池冷却，触发 `allCoolingFastFail` → 客户端硬等 14s（若不冷却，慢速退避重试约 2s 即可自愈）

**修复**：
- 扩展容量信号识别：`default_is_model_temporarily_unavailable` 加 `INSUFFICIENT_MODEL_CAPACITY`
- 放宽状态码门控：容量路径从「503 专属」改为「503 或 429」均可进入（只要 body 带容量信号）
- 进入容量路径后：慢速退避 1s base、不冷却、不扣健康分（与既有 503 路径一致）

普通 429（无容量信号）仍走冷却路径，行为不变。

### 历史累积修改

#### v8: 工具参数丢帧根因修复（commit `b98f7ce`）
Codex `Invalid tool parameters` 真根因：`merge_tool_input` 规则 5 把真增量碎片误判成「迟到的旧短快照」丢弃。

`gpt-5.6-sol` 逐 token 流式发 tool input，帧序列 `{"plan":[` → `{"` → `step` …
第 2 帧 `{"` 是缓冲 `{"plan":[` 的前缀，旧规则丢弃它，拼出 `{"plan":[step":"…`（中间少一整段 `{"`）→ 客户端 parse 失败。

**为什么查不出来**：
- 归因层标成 `truncated`，标签指向上游，实则是网关自己丢帧
- 修复层补不回 —— 缺的是中间结构字符，修复层只补尾部
- 生产 23/23 修复全失败（穷举证明：值串内截断本应 100% 可修）

**修复**：规则 5 加前提 `is_complete_json(buf)`。已完整的 JSON 对象无法被增量续写 —— buf 完整时前缀帧是旧快照，buf 未完整时是真增量。

#### v7: 工具日志可观测性（commit `b6b01c7`）
- tool_use 日志补 `model` 字段（多客户端场景区分 GPT/Claude）
- 新增「坏参数未下发」统一出口日志
- 开启 `converter=debug` 让 GPT effort 注入可见

> 注：v7 的 `model` 字段是定位 v8 根因的关键 —— 它坐实 23 次事件全来自 `gpt-5.6-sol`，把排查范围从「网关通用逻辑」收窄到「GPT 的逐 token 流式模式」。

#### v6: 历史扁平化 + 截断增强（commit `b5e1e67`）
- 历史工具扁平化（`sanitize_history`）：修复长会话多组工具导致 400
- `promptCacheEnabled` 向上游正确传递
- 截断增强：孤立清理、交替性补偿（补 ACK）、跨轮恢复

### 验证状态
- ✅ 单元测试：**835 passed / 0 failed**（+3 容量信号回归测试）
- ✅ 端到端：claude-opus-5-thinking 工具调用正常
- ✅ 容器健康 / 端口 8990 / 配置挂载正常
- ✅ 生产累计（v8+v9）：非法 JSON 0 次

### 回滚方案
```bash
docker stop kirostudio && docker rm kirostudio
docker run -d \
  --name kirostudio \
  --restart unless-stopped \
  -p 8990:8990 \
  -v /Users/a1234/Desktop/项目/KiroStudio/config:/app/config \
  -e RUST_LOG='info,kirostudio::kiro::provider=debug,kirostudio::anthropic::converter=debug' \
  kirostudio:v8   # 或 v7-prod / v6
```
可回滚镜像：`v9` / `v8` / `v7-prod` / `v7` / `v6`

### 待观察 / 未处理

1. **`toolTruncationRecovery` 仍为 `false`**。根因是丢帧而非截断，该开关不再必需；建议先观察数日，若无新增截断则保持关闭。
2. **启用凭据数为 1**（2026-08-02 核实）。单凭据下任何冷却都等于全池冷却，触发 `allCoolingFastFail` 快速失败 → 客户端硬等。建议：
   - 短期：若另一个凭据可用，启用它做冗余（2 凭据时冷却只是降速，不会完全中断）
   - 长期：v9 已修容量误冷却，但普通限流冷却仍会触发，冗余能显著改善可用性
3. **Codex 404（已给出方案，未确认是否已改）**：`~/.codex/config.toml` 的 `base_url` 需带 `/v1`（`http://127.0.0.1:8990/v1`），否则 Codex 请求 `/responses` 而网关只注册 `/v1/responses`。

---

## v10 代码已提交但镜像构建受阻（2026-08-02）

### 代码状态
- **Commit**: `c2f1126` fix(single-credential): 唯一凭据保护 + 死端点负缓存
- **已推送**: fork/master
- **镜像**: ❌ **构建失败**（环境问题，非代码问题）

### 构建障碍

Handler 编译错误（exit code 101）：
```
error[E0277]: the trait bound `fn(...) -> ... {post_messages}: Handler<_, _>` is not satisfied
```

**根因**：依赖漂移。v9 commit（2bdb4d7）在之前的干净环境下能构建（v9 镜像即证据），现在重构 v9 commit 也编译失败。Handler 相关代码自 v9 以来零改动，故障与本次修复无关。

可能原因：
- Cargo.lock 漂移或 registry 更新
- axum/tower 版本兼容性破坏
- musl target 交叉编译工具链问题

### v10 修复内容（代码已完成，等待构建环境修复）

#### 问题复现（2026-08-01 16:22 生产日志）

```
16:22:00  上游 429（真限流，reason: null）
16:22:00  凭据 #47 冷却 15s                   ← 仅1个启用凭据
16:22:01  全池冷却，返回 429 Retry-After=14    ← 网关自己拒，未打上游
16:22:02  全池冷却，返回 429 Retry-After=12
16:22:06  全池冷却，返回 429 Retry-After=8
16:22:10  全池冷却，返回 429 Retry-After=4
```

Codex 重试预算被网关造的 429 吃光 → `exceeded retry limit`。kiro-go 无此问题（无 cooldown + fast-fail 层）。

#### 根因 A：唯一凭据下冷却 = 全域中断

- 冷却的唯一价值是「让调度引到另一个号」
- 可用凭据=1 时无备用号，冷却等于全域中断
- fast-fail 把一次瞬态 429（上游几秒内可自愈）放大成 15 秒客户端硬等
- 次级放大：`上游 429 → RPM自动降档 30→20`，入站整形被拖到地板

#### 根因 B：死端点每请求白跑一跳延迟

端点回退链在 eu-central-1 的实际情况：
- `ide` (q.eu-central-1.amazonaws.com) → ✅ 可达
- `codewhisperer.eu-central-1.amazonaws.com` → ❌ **DNS 不存在**（容器内实测 exitcode=6）
- `amazonq` (q.eu-central-1.amazonaws.com) → 同 host，等于第二次打 ide

每个请求在 codewhisperer 白跑一次 DNS 失败（connect timeout）才继续，等于回退链凭空多一跳延迟。0eaa141 那次专门修的横向退路，在 eu-central-1 退化成单端点。

#### 修复实现

**1. 唯一凭据保护** (`token_manager.rs`，+20 行)

冷却 fast-fail 决策点加前置判断：
```rust
let available = count_enabled();
let sole_credential = available == 1;
if all_cooling_fast_fail && wait > 2s && !sole_credential { bail!(...) }
if sole_credential && wait > 2s {
    tracing::warn!("唯一凭据冷却中，网关内短等而非 fast-fail");
}
// 继续走 20s 短等重试（上游限流通常几秒内自愈）
```

**2. 死端点负缓存** (`provider.rs`，+70 行)

新增 `dead_endpoints: Mutex<HashMap<endpoint@region, Instant>>`：
- 连接层失败（DNS/TCP/TLS）→ 记 (endpoint, region)，TTL 30 分钟
- 下次该端点在负缓存内 → 直接跳过（链尾除外，保证至少发一次）
- 拿到 HTTP 响应（含 429/5xx）→ 清负缓存（连接层通了）

判据：
- `e.is_connect() || e.is_timeout() || e.is_request()` 进负缓存
- 状态码错误（429/5xx）是容量问题，host 本身好，绝不进负缓存

#### 验证状态

- ✅ 语法检查：`rustc --crate-type lib` 通过
- ✅ DNS 实测：codewhisperer.eu-central-1 不存在，us-east-1 / q.eu-central-1 可达
- ✅ 两处改动独立：token_manager 与 provider 互不依赖，可独立回滚
- ❌ 集成构建：受阻于环境问题（handler 编译错误）

### 临时缓解方案（不需要v10镜像）

当前生产已是单凭据场景（启用数=1），v10 修复高度相关。在镜像可构建之前：

1. **配置修改** — 改 `config.json`（热更或重启生效）：
   ```json
   "allCoolingFastFail": false
   ```
   效果：全池冷却时网关内短等（上限 20s）而非立刻甩 429。Codex 不消耗重试预算。

2. **凭据 region 改 us-east-1**（如果可行）— 让 codewhisperer 变成真正可达的第二个 host。
   影响：改变上游落点和延迟，属于环境决策。

3. **启用第二个凭据**（如果有） — 一次解决根因 A。有备用号时冷却回归本意（降速而非中断）。

### 下一步

1. **修复构建环境**：清理 Cargo.lock / 检查 axum 版本 / 验证 musl target
2. **构建 v10 镜像**并部署
3. 观察日志确认：
   - 唯一凭据冷却时出现「网关内短等而非 fast-fail」日志
   - codewhisperer.eu-central-1 出现「近期连接失败，跳过本跳」日志
   - 全池冷却事件大幅减少

---

## 历史部署

### 2026-08-01 v8
- 工具参数丢帧根因修复（`merge_tool_input` 规则 5 收窄）
- 运行约 30 分钟后由 v9 替换

### 2026-08-01 v7-prod
- 工具日志可观测性增强（`model` 字段 + 处置结果日志 + effort 可见）
- 运行约 50 分钟后由 v8 替换

### 2026-07-31 v6
- 历史扁平化 + promptCacheEnabled 修复 + 截断增强
- 运行约 26 小时

### 2026-07-29 初始版本
- 基于上游 KiroStudio，增加请求体超限主动丢弃历史机制
