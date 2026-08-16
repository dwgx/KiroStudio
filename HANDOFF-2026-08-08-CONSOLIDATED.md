# 交接文档 · 2026-08-08（合并后最终态 consolidated）

<!-- HISTORICAL-ARCHIVE-MARK -->
> ⚠️ **这是历史归档，不是当前状态。** 当前状态看仓根 `STATUS.md`（唯一真相源）。
> 本文件已确证含过期断言（线上 sha/测试数/守卫行号均已漂移），价值只在推导过程。

> 本文件是仓库**大规模合并 + 全量修复之后**的最终状态交接，取代
> `HANDOFF-2026-08-08-COMPRESSED.md`（记录到 23e1be2）与
> `HANDOFF-2026-08-08-DEPLOYED.md`（均已过时）。
> 状态入口仍是 `STATUS.md`；长期约束仍是 `CLAUDE.md`。

---

## 0. 一句话结论

master 已到 **`64b0c7b`**（已推 origin 私有仓 **`KiroStudio-skiapi`**），工作树 **0 未提交**
（`git status --porcelain` 为空）。**1533 测试全绿、clippy 0 error**。已用 **`kirostudio-hotswap deploy`**
部署到**新机 `143.20.230.62` 容器**（`skiapi-kirostudio`）。**旧机 ws-vps（143.20.230.248）已退役，别再用它部署。**

---

## 1. 当前代码状态（master 提交链）

```
64b0c7b fix: clippy——EmptyStreamGuard 去掉永不循环的 loop                     ← HEAD
6831b83 feat: 全量修复汇聚——vps 移植 + 会炸 bug + k2cc cache 链 + 重试上限4
42df1d2 snapshot(live): 端点换桶重构——已 hotswap 到生产但未提交
a617569 fix: 核验修复——deducted_chars 真扣后才前移 + 非流式 inline 接线 + 测试修正
3064b4b fix: review 修复——配置 merge Option 化 / usage 防重复扣减 / WebSearch 精确匹配 / schema 复用 converter / inline 接线
4c52ab2 feat: 修复协议自动化——deepseek 归一化配置化 + 请求/响应补坑 + 通用 schema 修复
d8142a6 fix: passthrough 过滤后空流兜底——补发 error 事件防客户端卡死
4e282e1 fix: 复核修复——非流式 redacted_thinking 漏滤 + Grep explanation 过度剥 + 注释命名/测试补全
beb0277 fix: review 修复批次——5xx压力信号/ksk尾引号/max_tokens显式thinking/工具映射对称/SSE多行data
ca0cc15 feat: 图片降采样/TOOL_SCHEMA_INVALID 容错 + DeepSeek 工具映射合并
```

- 工作树 **0 未提交**，当前分支 `master`。
- `42df1d2` 本属 `snapshot/live-endpoint-buckets`（生产快照），已并入 master 主线；
  端点换桶重构（cli-runtime 端点 + 429 自动换桶）现在 master 里就有。

### 分支 / tag 现状

| 分支 | 位置 | 用途 |
|---|---|---|
| `master` | `64b0c7b` | 主线（HEAD） |
| `consolidate/all` | 合并记录 | 大规模合并记录 |
| `snapshot/live-endpoint-buckets` | `42df1d2` | 生产快照（端点换桶） |
| `deploy/vps` | `495b770` | 部署构建用 |
| `fix/macos-support-and-critical-bugs` | — | 开发分支 |
| `fix/region-inconclusive-and-clone-family` | — | 开发分支 |
| `backup/worktree-snapshot` | — | 工作区快照备份 |

| 归档 tag | 指向 |
|---|---|
| `archive/prod-0e21f79` | 生产归档 |
| `archive/vps-2efcecb` | VPS 归档 |

---

## 2. 本轮完成内容（按主题）

### 2.1 会炸 3 条（6831b83）
- **passthrough_think_filter 非 object panic 守卫**：上游 thinking 块不是 object 时不再 panic。
- **buf 上限**：passthrough 过滤缓冲无界增长问题加硬上限。
- **separator min**：事件分隔符最小长度守卫（防空/畸形流）。
- `64b0c7b` 顺手清了 `EmptyStreamGuard` 里永不循环的 `loop`（clippy）。

### 2.2 vps 5 项移植（6831b83）
- **family_key clone_group 收族**：同租户克隆分身按 `family_key`/`clone_group` 归族，族级连坐统一。
- **token_manager 族级累加清零**：族级健康计数累加与复位。
- **subscription_unsupported 调用点**：订阅不支持错误在正确调用点上报/处理。
- **openai `plan_thinking` 门**：OpenAI 兼容层 `plan_thinking` 开关门。
- **usage_handlers PipelineHealth**：用量聚合出口合并既有 `dropped` 出口。

### 2.3 k2cc 四层 cache 链（6831b83）
- **metering 真值**：`kiro/model/events/metering.rs` 解析上游真值计量。
- **5m/1h 拆分**：cache 拆 `ephemeral_5m` / `ephemeral_1h` 两条窗口。
- **clamp**：cache 计数做边界钳制。
- **入库真值 / 对外 0.6657**：入库记**未缩放真值**，对外下发放大 `×0.6657`
  （`CLIENT_TOKEN_DISPLAY_SCALE`，0.65×(85/83) 保持原 compact 触发点）。
- **fingerprint TODO**：Layer 3 账号级前缀指纹（k2cc `cache/fingerprint.rs`）**未移植，恒 None**。

### 2.4 deepseek 缺陷（6831b83 + 4c52ab2 + a617569）
- **图片降采样**：`hard_max_pixels` 上限 + kill switch + GIF 帧数限制。
- **白名单门序**：`output_config` 白名单先按 `effective_model` 判定（修 gate 顺序）。
- **deducted_chars 累计口径**：真扣之后才前移累计游标（防重复扣减）。
- **端点守卫**：端点桶 region 维度 + 守卫测试覆盖。

### 2.5 重试上限 4 + 长退避（6831b83）
- **重试上限 12 → 4**：防 60×12 重试放大（配合 `kiro_shield.py` 外挂预算）。
- **retry_delay_throttle**：429 退避 1s → 8s，注释同步重写。

### 2.6 此前几轮（已并入 master 的前置提交）
- 图片降采样 / `TOOL_SCHEMA_INVALID` 容错 / DeepSeek 工具映射合并（ca0cc15）。
- 5xx 压力信号 / ksk 尾引号 / max_tokens 显式 thinking / 工具映射对称 / SSE 多行 data（beb0277）。
- 非流式 `redacted_thinking` 漏滤 + Grep explanation 过度剥（4e282e1）。
- passthrough 过滤后空流兜底——补发 error 事件防客户端卡死（d8142a6）。
- deepseek 归一化配置化 + 请求/响应补坑 + 通用 schema 修复（4c52ab2）。
- 配置 merge Option 化 / usage 防重复扣减 / WebSearch 精确匹配 / schema 复用 converter / inline 接线（3064b4b）。

---

## 3. 🔴 部署信息（关键，别再搞错机器）

- **新机**：`143.20.230.62`，SSH `-p 673 -i ~/.ssh/id_ed25519_pcs_root root@143.20.230.62`。
  容器化（`/opt/skiapi/docker-compose.yml`），KiroStudio 容器 `skiapi-kirostudio`。
- **旧机 ws-vps（143.20.230.248）已退役**——不要再用它部署。
- **部署命令**（在新机跑）：
  ```bash
  kirostudio-hotswap deploy master   # 拉代码→编译→新镜像→热更新（shield 吸收空窗）
  kirostudio-hotswap rollback        # 回滚到上一镜像
  kirostudio-hotswap status          # 查看版本
  ```
- **前端 dist**：新机无 node，`admin-ui/dist` 需本地 `pnpm build` 后 scp 到
  `/opt/kirostudio-src/admin-ui/dist`（注意 `scp -r` 会嵌套成 `dist/dist`，需修正）。
- **代码到新机**：本地 `git push` 到新机裸仓
  `ssh://root@143.20.230.62/srv/git/KiroStudio-skiapi.git`（master），然后新机 `kirostudio-hotswap deploy`。
- **完整流程文档**：`ws-vps/docs/13-kirostudio-hotswap.md`（新机容器化 hotswap）。

---

## 4. 已知遗留 / 边界

1. **bucket_id 接线需 provider ctx**：`endpoint/mod.rs::bucket_id(ctx)` 已定义（同 host 同 target
   才算同一上游限流桶），但 `provider.rs` 的 `endpoint_buckets` key 仍是
   `(credential_id, name.to_string())`，**未换成 `endpoint.bucket_id(ctx)`**（见
   `endpoint/mod.rs:141-145` 注释）。换桶时拿不到完整 `RequestContext`，属接线待办。
2. **cache fingerprint TODO**：Layer 3 账号级前缀指纹（k2cc `cache/fingerprint.rs`）**未移植，
   恒 `None`**（`anthropic/cache.rs` 的 `fingerprint_usage` 相关注释）。
3. **codewhisperer / amazonq 面板下拉未移除**：`ENDPOINT_NAMES` 仍含这两个端点
   （`endpoint/mod.rs:45-51`），前端凭据卡片/行端点下拉会显示它们。实现已注册但用途待定
   （是补全功能还是从下拉移除，需拍板）。
4. **usage 0.6657 对外缩放保留**：入库用未缩放真值，对外下发放大 `×0.6657`
   （`CLIENT_TOKEN_DISPLAY_SCALE`，`stream.rs:180`）。这是刻意保留的展示缩放，别改成真值直出
   （会破坏客户端 compact 触发点口径）。
5. **0.7.46 未打 tag**：OTA「检查更新」升不到这一版（需打 tag 才能走 OTA 通道）。

---

## 5. 后续建议

1. **bucket_id 接线**：`provider.rs::select_endpoint` 的 `endpoint_buckets` key 换成
   `(credential_id, endpoint.bucket_id(ctx))`，使换桶维度从端点名升级为「host+target」。
2. **cache fingerprint 移植**：把 k2cc `cache/fingerprint.rs` 的 Layer 3 账号级前缀指纹移植进来，
   当前三层只实现到两层（metering 真值 + 5m/1h 窗口）。
3. **codewhisperer / amazonq 定夺**：要么补全这两个端点的真实协议，要么从 `ENDPOINT_NAMES`
   与前端下拉移除，避免误选。
4. **打 tag**：给当前 master 打 `v0.7.46` tag，让 OTA 通道可用。
5. **容器化部署流程定型**：镜像 build+push+rollback 规范尚未定型（`kirostudio-update` 仍是旧
   systemd 二进制流程，对容器无效），改镜像前先核对 `/opt/skiapi/docker-compose.yml` 与
   `ws-vps/docs/11-rebuild-new62.md`。
6. **健康标记路径**：容器化后 `/usr/local/bin/kirostudio.health` 只读，应改写到 `/app/data`。
