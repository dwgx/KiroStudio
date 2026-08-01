# KiroStudio 生产部署记录

## 2026-08-01 v8 部署（当前生产）

### 部署时间
- 2026-08-01 23:28 CST

### 镜像信息
- **镜像**: `kirostudio:v8`
- **版本**: v0.7.44
- **大小**: 42.3MB
- **对应 commit**: `b98f7ce`

### 容器配置
```bash
docker run -d \
  --name kirostudio \
  --restart unless-stopped \
  -p 8990:8990 \
  -v /Users/a1234/Desktop/项目/KiroStudio/config:/app/config \
  -e RUST_LOG='info,kirostudio::kiro::provider=debug,kirostudio::anthropic::converter=debug,kirostudio::anthropic::truncate=info,kirostudio::anthropic::sanitize_history=info,kiro::tool_trace=warn' \
  -e KIRO_TOOL_TRACE=1 \
  kirostudio:v8
```

### 本次核心修复：`merge_tool_input` 规则 5 丢真增量碎片

Commit `b98f7ce`。这是 Codex `Invalid tool parameters` 的**真根因**，此前被误归因为「上游截断」。

**现场**：`gpt-5.6-sol` 逐 token 流式发 tool input，帧序列形如
`{"plan":[` → `{"` → `step` → `":"` → …

第 2 帧 `{"` 恰是缓冲 `{"plan":[` 的前缀，旧规则 5 判为「迟到的旧短快照」直接丢弃，
拼装出 `{"plan":[step":"…`（中间少一整段 `{"`）→ 客户端 parse 失败。

**为什么此前查不出来**：
- 归因层按「结构未闭合」把它标成 `truncated`，标签一直指向上游，实则是网关自己丢帧
- 修复层补不回——缺的是**中间**的结构字符，而修复层只补尾部（闭合字符串、补括号）
- 这正是生产 23 次「截断」0 次修复成功的原因（穷举测试证明：值串内截断本应 100% 可修）

**修复**：规则 5 增加前提 `is_complete_json(buf)`。
判据是**已完整的 JSON 对象无法再被增量续写**——buf 完整时更短的前缀帧只可能是迟到的旧快照
（规则 5 原本要防的场景）；buf 未完整时必然是真增量碎片，须走第 7 步追加。

### 历史累积修改

#### v6: 历史扁平化 + 截断增强（commit `b5e1e67`）
- 历史工具扁平化（`sanitize_history`）：修复长会话多组工具导致 400
- `promptCacheEnabled` 向上游正确传递
- 截断增强：孤立清理、交替性补偿（补 ACK）、跨轮恢复

#### v7: 工具日志可观测性（commit `b6b01c7`）
- tool_use 日志补 `model` 字段（多客户端场景区分 GPT/Claude）
- 新增「坏参数未下发」统一出口日志
- 开启 `converter=debug` 让 GPT effort 注入可见

> 注：v7 的 `model` 字段是定位本次根因的关键——它坐实了 23 次事件全部来自
> `gpt-5.6-sol`，把排查范围从「网关通用逻辑」收窄到「GPT 的逐 token 流式模式」。

### 验证状态
- ✅ 单元测试：**832 passed / 0 failed**
- ✅ 根因回归测试已锁：生产坏串同形帧序列重放 → 拼出合法 JSON
- ✅ 规则 5 原有保护场景未破：`test_merge_full_then_shorter_prefix_kept` 仍通过
- ✅ 穷举截断覆盖率测试：确认修复层对值串内截断 100% 有效（用于排除误归因）
- ✅ 生产实测：251 请求（其中 `gpt-5.6-sol` 202 次），**非法 JSON 0 次**（v7 同等窗口为 23 次）
- ✅ 端到端：`update_plan` 5 元素嵌套 object 数组全部完整
- ✅ 容器健康 / 端口 8990 / 配置挂载正常

### 诊断探针说明

生产当前保留 `KIRO_TOOL_TRACE=1` + `kiro::tool_trace=warn`：
- **只在参数已非法时**打印坏串全文（warn 级），正常流零输出
- 逐帧合并轨迹是 `trace!` 级，被 `=warn` 过滤掉，**不会产生帧洪水**
- 作用：若同类问题再现，可立刻拿到坏串原文定性，无需重启改配置

确认稳定后可移除（去掉 `-e KIRO_TOOL_TRACE=1` 与 `kiro::tool_trace=warn` 重建容器）。

### 回滚方案
```bash
docker stop kirostudio && docker rm kirostudio
docker run -d \
  --name kirostudio \
  --restart unless-stopped \
  -p 8990:8990 \
  -v /Users/a1234/Desktop/项目/KiroStudio/config:/app/config \
  -e RUST_LOG='info,kirostudio::kiro::provider=debug,kirostudio::anthropic::converter=debug' \
  kirostudio:v7-prod   # 或 v6
```
可回滚镜像：`v8` / `v7-prod` / `v7` / `v6`

### 待观察 / 未处理

1. **`toolTruncationRecovery` 仍为 `false`**。根因是丢帧而非截断，该开关不再是必需项；
   建议先观察数日，若无新增截断则保持关闭。
2. **上游可用性抖动（与本问题无关）**：v7 窗口内出现 6 次
   `codewhisperer eu-central-1` 连接层失败 + 1 次 `INSUFFICIENT_MODEL_CAPACITY` 429
   （触发 RPM 自动降档 270→135）。凭据自带 `apiRegion: eu-central-1`，打 eu 属配置预期。
   这是独立的上游侧问题，需要时另查。
3. **Codex 404（已给出方案，未确认是否已改）**：`~/.codex/config.toml` 的
   `base_url` 需带 `/v1`（`http://127.0.0.1:8990/v1`），否则 Codex 请求
   `/responses` 而网关只注册 `/v1/responses`。

---

## 历史部署

### 2026-08-01 v7-prod
- 工具日志可观测性增强（`model` 字段 + 处置结果日志 + effort 可见）
- 运行约 50 分钟后由 v8 替换

### 2026-07-31 v6
- 历史扁平化 + promptCacheEnabled 修复 + 截断增强
- 运行约 26 小时

### 2026-07-29 初始版本
- 基于上游 KiroStudio，增加请求体超限主动丢弃历史机制
