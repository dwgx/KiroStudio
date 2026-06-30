# 韧性合并清单 —— 三个 fork vs hank9999（基于真实 diff，2026-06-30）

> 已 diff：M-JYuan / Foxfishc / ZyphrZero 的 src 全树 vs hank9999。所有结论来自实际文件对比。
> 关键发现：**M-JYuan 与 Foxfishc 高度同源**（独有文件几乎一致），Foxfishc 相对 M-JYuan 真正独有仅 `kiro/overage.rs`。
> ZyphrZero 是**另一条独立线**，偏运营面（追踪/统计/分组/代理池）。

## 谱系（已验证）
```
hank9999 (主干，基线已含成熟的"瞬态错误不锁死凭据"雷暴防护)
   ├── M-JYuan ──┐ 同源，引入 CLIProxyAPIPlus 的防检测套件 + 压缩/截断
   │   Foxfishc ─┘ ≈M-JYuan + 独有 overage.rs(超额实时)
   └── ZyphrZero  独立线：运营面(trace_db/usage_stats/groups/proxy_pool/client_keys/binary_update)
```

## A. M-JYuan/Foxfishc 增量（源头：CLIProxyAPIPlus，命中"防关联最彻底"）★高价值

| 文件 | 行 | 作用 | 取否 |
|---|---|---|---|
| `kiro/fingerprint.rs` | 301 | **多维度设备指纹**，模拟真实 Kiro IDE 完整环境特征降低被检测 | ★★★必取（防关联核心） |
| `kiro/rate_limiter.rs` | 484 | 每日请求限制+请求间隔+指数退避，**模拟人类使用模式** | ★★★必取 |
| `kiro/cooldown.rs` | 388 | **分类冷却**：不同失败原因差异化冷却时长+自动清理 | ★★★必取（补 hank9999 单一退避） |
| `kiro/affinity.rs` | 86 | user_id↔credential 绑定，连续对话用同一凭据（防指纹跳变） | ★★必取 |
| `kiro/background_refresh.rs` | 346 | 后台定时刷新将过期 token，消除请求时刷新延迟 | ★★取 |
| `anthropic/compressor.rs` | 1565 | 输入压缩管道，规避 Kiro ~5MiB 请求体上限(否则400) | ★★取（稳定性） |
| `anthropic/truncation.rs` | 282 | 工具调用 JSON 截断检测+软恢复，引导分块重试 | ★★取 |
| `anthropic/cache_tracker.rs` | 940 | prompt cache 跟踪/计费 | ★可选 |
| `common/redact.rs` | 97 | 日志脱敏(Token/密钥/密码→`<redacted>`) | ★★取（安全，配合at-rest加密） |
| `kiro/overage.rs` (Foxfishc独有) | - | 超额(Overage)实时检测 | ★取 |

## B. ZyphrZero 增量（运营面，适合做"更好的后台"）

| 文件 | 行 | 作用 | 取否 |
|---|---|---|---|
| `admin/trace_db.rs` | 984 | **请求链路追踪持久化**(SQLite) | ★★★必取（你要追踪） |
| `admin/usage_stats.rs` | 908 | 用量记录+时序聚合 | ★★★必取（配额面板） |
| `admin/proxy_pool.rs` | 525 | **代理 IP 池管理** | ★★★必取（你要代理池） |
| `admin/client_keys.rs` | 656 | 客户端 API Key 管理(分发可控key) | ★★取 |
| `admin/groups.rs` | 376 | 账号分组(独立实体) | ★★取 |
| `admin/binary_update.rs` | 473 | 在线更新二进制 | ★可选（自用Docker重建即可，优先级低） |
| `anthropic/websearch_loop.rs` | 1427 | web_search agentic loop | ★可选 |

## C. 合并策略（务实，避免一次吞太多）

**原则**：hank9999 基线已成熟，按需吸收，不盲目全合。每项独立移植+验证。

- **阶段一必取**（韧性骨架）：cooldown + rate_limiter + fingerprint + affinity + background_refresh + redact
  → 这套是"防关联最彻底"的实体，且都是独立模块、低耦合，易移植
- **阶段一取**（稳定性）：compressor + truncation（防大请求/截断打断）
- **阶段二/三取**（运营面）：trace_db + usage_stats + proxy_pool + client_keys + groups
- **暂不取**：binary_update（自用 Docker 重建即可）、cache_tracker/websearch_loop（按需）

## D. ✅ License 已确认（红线解除）
- 防关联套件思想源头 **CLIProxyAPI (router-for-me) = MIT**（已读 LICENSE，Router-For.ME 版权）。
- M-JYuan/Foxfishc 本身是 hank9999（MIT 系）的 fork。
- **结论：A 类防关联套件可放心移植，保留版权致谢即可，无许可障碍。**
- 移植习惯：每移植一模块，先用 CodeGraph 看依赖(`codegraph node <符号>`)，再决定连带哪些。

## E. 下一步动作
1. ✅ CLIProxyAPI License 已确认 MIT（A 类可移植）
2. 起 KiroStudio 骨架：先搭 hank9999 基线 → 跑通 Anthropic → 再逐个吸收 C 阶段一清单
3. fingerprint.rs 与 machine_id 的关系要理清（machine_id 是凭据级单值，fingerprint 是多维环境特征，二者互补——一起用才是"最彻底防关联"）
