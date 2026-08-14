# KiroStudio 架构决策记录（2026-08-09）

> 本文件是**架构决策与踩坑记录**，供接手者理解"为什么代码长这样"。
> 它是 `CLAUDE.md`（长期约束）与 `STATUS.md`（当前状态）之外的第三层：
> 记录**经过三方代码对比、线上实测、对抗式评审后确立的结论**，防止后人推翻已验证的正确设计。

---

## 1. 核心结论：我们的 region/端点逻辑是三方中最强的，不要"照抄 K2CC 物化重写"

2026-08-09 对 OURS / kiro2cc-proxy / Foxfishc-kiro.rs / GreyGunG 四方做了逐行对比，结论：

| 维度 | OURS | K2CC | 谁更强 |
|---|---|---|---|
| region 解析 | **单一函数** `effective_upstream_region`（credentials.rs:544），5 端点全走它 | 三处拼、无统一 | **OURS** |
| 按区纠错自愈 | **有**（api_key 号 403 → 换区重试 → 成功回写 api_region） | 没有 | **OURS 独有** |
| 桶 key | `(credential_id, 解析后 host + target)`，**含 region** | `(credential_id, EndpointName)`，不含 region | **OURS**（K2CC 那个是潜在 bug，只是没换区机制才没暴露） |
| 端点派发 | EWMA 自适应（endpoint_health.rs） | attempt 轮询 | **OURS**（轮询对 ksk 的"死端点/不对称限流"零反应） |
| 端点个数 | 5（多 cli-runtime） | 4 | OURS |

**对抗式评审还发现：照抄 K2CC 的"物化 region 到端点对象"会亲手制造恒 403** ——
现在的正确性靠"select/封桶/请求 URL 三处共用同一个 call_creds"这个**结构保证**；
物化后变成靠调用纪律，漏一处就是 L1 自愈失效 + 桶键分叉。

**结论：不要物化重写。** 我们需要的"和 K2CC 一致"只限定为"ksk 可达端点集合 + 回退语义一致"，这条已满足。

---

## 2. 透传路径 P1–P5 修复（2026-08-09 上线，local-6859c62）

线上真实故障：`error sending request`（连接层）+ `outcome=bad_request` 但 `error_message` 空（根因不可见）。

| 项 | 改动 | 验证 |
|---|---|---|
| P0 | 透传 client 每请求新建 → 按 (proxy,tls) 缓存复用 + 连接池参数 | 上线后 `error sending request` 明显下降 |
| P1 | 透传 failover 加墙钟预算（复用 `MAX_REQUEST_RETRY_BUDGET_SECS=45`）+ connect_timeout 30→10s | 预算触发 0 次（未误伤） |
| P2 | 空流守卫 `guard_empty_stream` 铺满纯透传流式分支（此前只覆盖 deepseek 分支） | 二进制含 |
| P3 | 请求头白名单转发：`anthropic-beta` / `accept` / `accept-encoding` / `x-stainless-*`（修 1M 上下文在代挂路径失效） | 需真实 1M 请求验证 |
| P4 | 响应头白名单透传：`retry-after` / `x-ratelimit-*` / `request-id` / `anthropic-ratelimit-*` + **上游错误原文进日志与 trace** | 线上已见 23 次错误原文 |
| P5 | `fetch_upstream_models` 裸 `resp.json()` → `read_json_capped` 4MiB | — |

**P4 是最关键的一项**：它让"上游到底说了什么"第一次可见。此前的困境（`outcome=bad_request` 但查不到原因）就是因为它缺失。

---

## 3. deepseek 归一化的白名单感知（2026-08-09，修 1439 误选）

**根因**：选号侧 `effective_model` 无条件把非 `deepseek-` 前缀映射成 fallback，
于是白名单含 `deepseek-v4-flash` 的号（1365/1439）会对**任意** claude 模型放行，
选中后改写打过去上游不认 → 400，且错误体被丢弃 → 根因完全不可见。

**修复**：`effective_model` 加 `whitelist` 参数 —— 原模型名在白名单里就保持原名，否则才 fallback。
**选号侧与改写侧共用同一个判定**（token_manager.rs:2927-2938 / passthrough.rs:159），
不可能再"选中了但改写后必失败"。

**效果**：修复后 `claude-opus-5` 不再误选 1439；`claude-opus-4.6/4.7` 这类无号可服务的模型在选号阶段被正确挡下（返回明确 404）。

---

## 4. 前端已交付（2026-08-09）

- **i18n 66 处双花括号修复**：`{{n}}` → `{n}`。根因是插值配了单花括号 `{var}`，
  双花括号被解析成带花括号的变量名 `` `{n}` `` 与实参不匹配 → 字面显示。加了 `skipOnVariables: true` + 3 条回归测试。
- **行视图多选**：Ctrl/⌘+左键加减选 + Shift 区间选（复用 `use-credential-selection` 里已写好但从未接线的 `selectRange`）。
- **行视图拖拽框选**：marquee，复用 `lib/marquee-geometry.ts`（与 canvas 共用几何函数），只作用于当前页。
- **子菜单方向**：SubContent 补 `side="right"` + sideOffset + collisionPadding。
- **端点健康卡**：`GET /api/admin/endpoint-health`，运维页展示每凭据每端点的 EWMA 成功率 + 样本数。

---

## 5. 部署与回滚的坑（每个都踩过）

1. **`kirostudio-hotswap deploy` 只换后端二进制，前端 dist 必须单独同步**（bind mount）。
   改前端不同步 dist = 静默不生效，且无任何报错。我已因此被误判两次。
   → 判断线上 dist 新旧：`ls -la /opt/kirostudio-src/admin-ui/dist/assets/dashboard*.js` 看 mtime。
2. **selfheal 会用 `.env` 的 tag 把 kirostudio 静默降级**（CLAUDE.md 也记过）。
   部署后 `.env` 必须与容器一致，否则 selfheal 每 2 分钟一次可能把它换回旧镜像。
3. **`hotswap status` 可能误报**（`FAIL docker compose up 失败` 但容器实际 healthy）。
   它探测 `/health` 而二进制没有该路由 → 404 判失败。核实要看 `docker ps` 实际状态 + Admin API 是否 200。
4. **`strings` 查编译产物不可靠**：方法名/字符串常量可能被优化掉（P2 字面量查不到但快照里明明有）。
   要验证"改动是否在二进制里"，用**我新加的特有字符串字面量**查（如 `BUCKET_KEY_PLACEHOLDER`）。

---

## 6. 三个待实现的真实用户缺口（workflow 中）

| 缺口 | 参考仓 | 我们的现状 | 影响 |
|---|---|---|---|
| 自适应二次压缩循环 | ref-mjy handlers.rs:245-450 | 压一次就放手，注释"交上游 400" | 长 agent 会话必然整轮失败 |
| WebSearch agentic 回灌 | ref-grey websearch_loop.rs:1269 | 把 web_search 剔掉再转发 | CC 带 WebSearch + 其他工具时静默失效 |
| tool_use XML 泄漏过滤 | ref-grey stream.rs:33 | 只处理 `<invoke`/`antml:` 形态 | 上游把工具调用当正文吐时客户端渲染裸 XML |

---

## 7. 遗留（诚实记录）

- **`#1436`（唯一 ksk 号）零真实流量** —— ksk 路径所有改动的正确性只有测试保证，无线上验证。
- **`Failed to parse JSON`（2026-08-09 用户报告）** —— 已确证不是 KiroStudio 的 P1–P5 造成：
  trace 无相关失败、P2 只走 SSE 分支、1305 的 502 来自它指向的 k2cc 容器（再转
  `q.lt4net.amazonaws.com`，那是 k2cc 自己的问题）。
- `bucket_id` / `bucket_key` 的尾斜杠：改前旧实现尾斜杠在 format 串里，任何重构都要
  **先抓 golden value 测试**再动，否则比特级漂移会让 429 桶 key 变化。
