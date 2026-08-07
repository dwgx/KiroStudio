<!-- HISTORICAL-ARCHIVE-MARK -->
> ⚠️ **这是过程记录，不是当前状态。** 当前状态看仓根 `STATUS.md`（唯一真相源）。
>
> 本文件已确证含**过期断言**。历史上多次出现「后来的会话把过期断言当约束」而做出错误决策
> ——最严重的一次：一句无依据的「`q.*` 已停用」注释直接导致了一次错误的架构迁移，
> 改坏 region 探测让 US 号恒 403，上线后才发现并回滚。
>
> 读本文件时：**任何数字（测试数 / 配置值 / 池容量 / 行号）一律现读现验**，
> 结论性断言先按 `STATUS.md` 核一遍。本文件的价值在于**依据与推导过程**，不在于它的结论。

---

# KiroStudio 未解决问题清单（2026-08-06 逐条核实版）

> 来源：本目录 32 个历史会话 + 12 份 HANDOFF/STATUS/TASK + 22 份 docs + 30 个 workflow 脚本。
> **每条都用当前源码复核过**，不是照抄文档。文档说"待修"但代码里已修的，本清单归入「已闭合」。
> 标注：`[已核实]` = 本轮 grep/读码确认；`[未验]` = 依据文档、我没独立验证。

---

## 一、先看这个：文档说待修、实际已闭合的（别再重复做）

这批占了旧 HANDOFF 待办列表的一半以上，是"清单越读越长"的主因。

| 项 | 旧文档位置 | 当前状态 |
|---|---|---|
| 已知问题 #1–#19 | CLAUDE.md | 全部已修，各条保留依据备查 `[已核实]` |
| #20 admission 超时不可观测 | CLAUDE.md「当前待修」 | **已修**：`provider.rs:988-1002` 有 `bump_inbound_admission_timeout()` + `emit_record`，并有守卫测试 `:2302-2311` `[已核实]` |
| #21 `retries` 无法聚合 | CLAUDE.md「当前待修」 | **已修**：`usage_stats.rs:112-126` 有 `retries_sum` + 发生过重试的请求数（双分母） `[已核实]` |
| #22 停机注释与实现分叉 | CLAUDE.md「当前待修」 | **已修**：`main.rs:711` 已用 `select!` 竞速，注释改对 `[已核实]` |
| P0-2 `INSUFFICIENT_MODEL_CAPACITY` 落 bad_request | HANDOFF-08-05-NIGHT §2 | **已修**：`provider.rs:1426-1429` 的 `is_capacity_400` 门排在通用 400 之前 `[已核实]` |
| P0-1 全池冷却兜底放行 | STATUS-08-05 §2.6 | **已修**：`token_manager.rs:3376-3454` `select_ignoring_cooldown` + `:4054-4058` 优先于 bail `[已核实]` |
| P0-1 `select_next_credential` 排除集降级死循环 | HANDOFF-08-05-NIGHT §5.2 | **已修**：`token_manager.rs:2963-2973` 有降级，`:2973` 注释说明 fresh 为空时等于全集 `[已核实]` |
| 凭据管理行模式 UI | HANDOFF-08-05-NIGHT §5.1 | **已完成**：`credential-row.tsx`（44KB）+ `dashboard.tsx:114/1094` 列头与 `isRowView` 已接 `[已核实]` |
| 代理格式识别 | ui-and-p0 workflow | **已完成**：`proxy-line-parse.ts`（20KB）+ `.test.ts` + 已被 `clone-management-card.tsx` 引用 `[已核实]` |
| thinking 标签泄漏 | thinking-tag-leak workflow | **已落地**：`stream.rs` 有 30 处 `invoke_sniff_buffer`/`MAX_PARTIAL_TAG_BYTES` `[已核实]` |
| region 探测归因污染（batch2 item 3） | docs/batch2-region-endpoint-matrix.md | **本轮已修**（见下 §四） `[已核实]` |

---

## 二、真正未解决 · 高优先（有实测数据支撑）

### H1 🔴 CLI 端点与 kiro-rs 有 7 处差异，`origin` 那条是 429 的头号嫌疑

**这是当前最值钱的一条。** 用户原话：同一把 key 在 kiro-rs 完全没问题、在 KiroStudio 429。

`ksk_` 号**本身就是 CLI 凭据**，而两边对 body 的处理完全不同 `[已核实]`：

| 项 | kiro-rs | KiroStudio | 位置 |
|---|---|---|---|
| **`origin`** | `set_origin_kiro_cli` 把所有 `AI_EDITOR` → **`KIRO_CLI`** | 仍是 **`AI_EDITOR`** | `converter.rs:900` 硬编码 |
| `agentContinuationId` | **删掉** | 从 conversationId 派生后发送 | `converter.rs:830-837` |
| history 里 `modelId` | **删掉** | 保留 | — |
| body 额外字段 | 无 | 加 `agentTaskType`+`agentMode=vibe` | `cli.rs` `inject_cli_agent_fields` |
| `x-amzn-codewhisperer-optout` | `false` | `true` | `cli.rs` |
| `x-amzn-kiro-agent-mode` | **不发**（只 IDE 发） | 发 `vibe` | `cli.rs` |
| `amz-sdk-request` | `attempt=1; max=3` | `attempt=1; max=1` | `cli.rs` |
| UA | 不含 machineId | 嵌 machineId | `cli.rs` |

即 KiroStudio 拿 CLI 密钥、对上游报称自己是 IDE。`origin` 极可能参与上游配额/限流分档。

**做法**：`origin: KIRO_CLI` 做成**配置开关默认关**，单号开、比 429 率，不要全池直切。
**不要**一次改 7 项 —— 那样失败了不知道是哪项。

### H2 🔴 397 次 `AccessDeniedException` 落兜底 502（P1-1）

region 错配型 403（`bearer token included in the request is invalid`）**没有分支接** `[已核实]`：
`handlers.rs` 只有 `is_upstream_temporarily_suspended` 的窄判据。

⛔ **不要放宽那个窄判据**（`handlers.rs:548-553` 写明泛匹配 `AccessDeniedException` 会把永久封号吞成可重试）。**新加独立分支。**

### H3 🟠 图片 media_type 不按 magic bytes 校正（49 次 400）

`converter.rs:1036` 的 `get_image_format` 只读客户端声明值 `[已核实]`（全仓 grep `FFD8|magic|0x89` 零命中）。
客户端声明 `image/png` 实际是 jpeg ⇒ 上游 `ValidationException` 400。
判据：`FFD8`=jpeg / `89504E47`=png / `47494638`=gif / `RIFF....WEBP`=webp。纯本地修复，无上游依赖。

### H4 🟠 兜底放行聚集单号

`select_ignoring_cooldown` 只按冷却剩余排序、完全确定性、无 inflight 无速率 `[已核实]`。
实测 #578 近 3h 拿 128 次、单分钟峰值 63 `[未验，来自 HANDOFF-08-05-NIGHT]`。
设计约束：**按 id 轮转而非随机**（`tie_break_jitter` 曾存在被删，理由是不可复现，见 `token_manager.rs:3078` 一带）；**只放行一个号**，否则惊群。

---

## 三、真正未解决 · 中低优先

### M1 🟠 `!thinking_enabled` 时空响应
`process_reasoning_content` 在 thinking 关闭时整帧丢弃。若某轮模型只吐 reasoning 无正文 ⇒ 空响应。
正解：**只在 reasoning 非空且正文为空时才降级下发**。别照抄 kiro-rs 的无条件转正文（会把内部推理混进用户可见回答）。

### M2 🟠 `inboundTargetRpm` / `inboundRpmMax` 打架
STATUS-08-05 §3.3 记「本轮恶化了」`[未验]`。改前先读 `ws-vps/docs/02-tuning.md`。

### M3 🟠 额度/积分刷新太慢（用户明确提过）`[未验]`

### M4 🟠 吸收层三条未修（默认关，手动开之前必须修）`[未验]`
依据在 `docs/absorb-layer-design.md`。

### M5 🟡 SOCKS 节点前端不可编辑
后端有 upsert/delete/test/bulk-import 四个端点、前端 API 函数也全有 `[已核实]`，
但 grep `update|edit|patch|put` 零命中 ⇒ **只能新增删除，不能改**。

### M6 🟡 batch2 剩余四项（本轮只做了 item 3、item 5 一半）
依据 `docs/batch2-region-endpoint-matrix.md:74-98`：
1. **探测改打真实端点** —— 现在探 `management.*.kiro.dev`、决定 `q.*.amazonaws.com`（探 A 决定 B）。本轮修的是"归因不被污染"，**域名不一致这条根因仍在** `[已核实]`
2. **端点 × region 矩阵**，全测完再选
4. **notification 补两 case** —— `use-pool-notifications.ts:32-52` 的 switch 缺 `RegionProbeFailed`/`RegionProbeTokenDead`，会落 `default` 显示原始枚举名。⚠️ 注意 i18n 三语文案**已有**、`i18n-labels.ts:58-59` 也已接，**只差这个硬编码中文 switch** `[已核实]`
6. **批量清理禁用号、排除代挂** —— 判据现成（`PassthroughFailed`/`PassthroughOverloaded` + `is_custom_api_credential()`），走 `delete_credential`（进回收站）而非 `purge` `[已核实]`

### M7 🟡 手改 region 的 UI 控件（本轮做了一半）
`setCredentialApiRegion` API + `useSetCredentialApiRegion` hook 已上线可用 `[已核实]`，
**只缺界面入口**。挂 `credential-card.tsx` 右键菜单还是详情对话框，需用户定。
在此之前改 region 只能手调 API 或改 `credentials.json`。

---

## 四、本轮（2026-08-06）已修并上线的六条

已 hotswap 到线上 `97afaf0`，1189 测试全绿 / clippy 0 error / tsc 干净。

| # | 缺陷 | 位置 |
|---|---|---|
| ① | 探测用带 403 换区回退的 `get_usage_limits` → 探 eu 内部偷偷回退 us 成功 → 把 **eu** 写死 | 新增 `fetch_usage_limits_once` + `get_usage_limits_in_region` |
| ② | `api_region` 优先级最低被 `auth_region` 压住 → 手改 us 不生效 | `credentials.rs:498` 加 api_key 专属分支 |
| ③ | `ImportKeyItem` 只有 3 字段，推号方给的 `apiRegion` 被静默丢弃 | `types.rs` + `service.rs` |
| ④ | `CredentialStatusItem` 不下发 region → 面板恒显 `—` | `types.rs` + `token_manager.rs` |
| ⑤ | 前端把 `ksk_` 号 region 只塞进 `authRegion` | `batch-import-dialog.tsx`、`add-credential-dialog.tsx` |
| ⑥ | `api-region` 端点前端零调用 | `credentials.ts` + `use-credentials.ts`（UI 控件见 M7） |

**⚠️ 这六条对当前线上 429 无效果**：18 个凭据里 17 个可用、全部 `apiRegion=eu-central-1` 且
`region`/`authRegion` 为空 ⇒ 修复前后同样解析到 `q.eu-central-1`，一个号的 host 都不变 `[已核实]`。
它修的是「下次导入 US 号还会踩同一个坑」+「错了能在面板上看见、能手改」。

---

## 五、429 的真实归因（防止后续再误判）

近 2h 实测 `[已核实]`：上游真 429 **7689** 次、全池冷却 699 次、吸收层预算不足 504 次。
monitor `by_cred` 里 `cred=None` 一桶：**12110 请求 / 95.2% rate_limited / 0 成功** —— 那是还没分到号就被挡掉的。

逐号健康度：`rate_limited_pct` 多在 0–3%（#563 0.5%、#503 0%、#632 0.3%），**没有一个号呈 403 特征**
⇒ 当前池子 region 全对。**429 是号池容量不够，不是端点错、不是 region 错。**

压 429 只有三条路：加号 / 降 `inboundTargetRpm` / 试 H1 的 `origin: KIRO_CLI`。

---

## 六、需要用户拍板才能动的（我不能替你决定）

1. **H1 的开关名与放开节奏** —— 单号试 or 全池切
2. **M7 UI 控件挂哪儿** —— 右键菜单 / 详情对话框
3. **cache 三选一** —— 停掉假 `cache_read` / 保留+标注（已有 `CACHE_ESTIMATED_HEADER`）/ 移植 kiro-rs `CacheMeter`（400+ 行进热路径）。已实测**上游无隐式前缀缓存**（首次 0.95833 vs 后续 0.95514，差 0.3%，n=2795/5152）`[未验，来自 HANDOFF]`
4. **分身主份要不要分节点** —— 用户说"主凭据没有代理"，而 workflow 被派的是"主份也该分节点"，**两个方向相反**，HANDOFF-08-05-NIGHT §4 记为未答
5. **403 永久禁用阈值** —— 取证结论是**不要上按次数的判据**（traces retention 只 ~24h，最可能含真封号的样本压根不在库里）

---

## 七、三个未验收的 workflow 产出

HANDOFF-08-05-NIGHT §4 列出、当时没人验证 `[未验]`：
1. `clone-group-delete` — 分身页「删除整个账号组」，走 `batch-delete {ids, force:true}`
2. `clone-node-picker` — 主份不分节点 / 无法手选 / 选择器不过滤分身
3. `thinking-tag-leak` — 已确认落地 `stream.rs`（见 §一）

验收重点：审查报告有没有指出"必须修"的问题。

---

## 八、硬约束（违反造成真实损失，逐条来自实测事故）

1. **工作树有其它会话的未提交改动**（当前 78 文件）。禁止对真实 index 做 `checkout`/`switch`/`stash`/`reset`/`commit`/`add`；禁止全仓 `cargo fmt`
2. 提交用 git plumbing 在临时 index 快照，**逐个文件 `add`，不要 `-A`**；做完验证分支名与 `git status --porcelain | wc -l` 与开始时一致
3. commit message 用 `-F 文件` 而非 `-m`（反引号会被 shell 展开）
4. **禁止 heredoc 写文件** —— 本机实测会让整个会话的 Bash **永久静默且转录零记录**。写文件一律 Write/Edit
5. 推 `origin` 不推 `public`（`public` = PUBLIC 仓已冻结）
6. 不在 VPS 上编译（4 核会抢死正在服务的 sub2api），走 Actions `deploy-build.yml`
7. 不碰线上配置值（`credentialRpmLimit`/`inboundTargetRpm` 等），依据在 `ws-vps/docs/02-tuning.md`，且"容量口径是假的"那节说明按实测改小会把吞吐掐死一个数量级
8. Rust 必须加 `--no-default-features`（`default = ["native-tls"]` 与出厂配置相反）
9. macOS：没有 `timeout`（是 `gtimeout`）；`--include='*.rs'` 必须加引号；`find -newermt` 不可用（bfs）

### 本仓「纸面测试」已知形态（第 8 种是本轮新增的）
> **测了分支内部，没测分支顺序。**
HANDOFF-08-05-NIGHT §2 的教训：改三处、四条测试、三次"回退即 FAILED"全过，而修复无效 ——
因为测的是纯函数与分支内部形状，与**分支之间的顺序**无关。

---

## 九、回滚

```bash
ssh ws-vps 'kirostudio-update rollback'   # 回到 kirostudio.prev
ssh ws-vps 'gateway-status'               # 全链路巡检
```
