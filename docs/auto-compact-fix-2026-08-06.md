# Claude Code 自动压缩在网关模式下为什么不触发（以及网关能做什么）

> **日期**：2026-08-06
> **实测对象**：本机 Claude Code 二进制 `~/.local/share/claude/versions/2.1.220`
> （`strings` 抽取后按日志字面量反向定位函数，只读）
>
> 🔴 **符号名会随 build 漂移，本文刻意不记符号名**。记的是**机制**与**可观测判据** ——
> 那两样跨版本稳定。任何后来的会话若要复核，用下面「怎么复核」一节的日志字面量重新定位。

---

## 结论先行

Claude Code 有**两条**压缩路径。网关（第三方 base_url）模式下：

| 路径 | 触发方式 | 网关模式下 |
|---|---|---|
| **反应式 auto-compact** | 按 token 水位主动压 | ❌ 结构性不可用（入口门提前 return） |
| **compact-and-retry** | 撞到「装不下」错误后压缩再重试 | ✅ **可用，且判据在服务端可控** |

⇒ 服务端唯一能做的补救是第二条：让「请求装不下」类错误的 message 命中它的判据。
本仓已落地，见 `src/anthropic/handlers.rs` 的 `OVERFLOW_COMPACT_HINT`。

---

## 一、反应式 auto-compact 的入口门

入口判定里有一道门（伪码，符号名已抹）：

```text
if (是否走反应式压缩() && !窗口来源不是兜底档(model, settings)) return false;
```

- 第一个谓词只看环境变量 `CLAUDE_CODE_REMOTE`：本地 CLI 恒为 true
  ⇒ 整个判定退化成 `if (窗口来源 == 兜底档) return false`。
- 第二个谓词 = 「解析出的窗口来源 ≠ 兜底档」。

**即：窗口来源落到兜底档 ⇒ 反应式压缩永不触发。**

那行 `autocompact: tokens=... level=... effectiveWindow=...` 的 debug 日志**在这道门之后**，
所以「一次都不打这行」就是门提前 return 的可观测证据。

## 二、窗口来源的优先级（2.1.220 抽出）

按顺序取第一个命中：

| # | 来源 | 条件 | 网关能否命中 |
|---|---|---|---|
| 1 | `env` | `CLAUDE_CODE_AUTO_COMPACT_WINDOW`，**纯整数**且 ∈ [1e5, 1e6] | ✅ 用户侧可设 |
| 2 | `settings` | 用户设置 `autoCompactWindow` | ✅ 用户侧可设 |
| 3 | `clientdata` | 读客户端**本地 bootstrap 缓存**里的 `auto_compact_windows`，且**读侧有 `provider !== "firstParty"` 门** | ❌ 缓存恒空，见下节 |
| 4 | `experiment` | 特定 model + 远端实验配置 | ❌ |
| 5 | `model-default`（白名单档） | **`有效窗口 < 1e6`** 且归一化 ID ∈ 白名单 Set | ⚠️ 只看 model ID，`[1m]` 必被跳过 |
| 5b | `model-default`（表查档） | 归一化 ID 命中内置窗口表（**无 `<1e6` 门**） | ⚠️ 表里只有一个模型 |
| 6 | `auto`（兜底） | 以上都不命中 | → **永不压缩** |

> ⚠️ **本档表比早先的口头结论多一档（5b）**。5b 没有 `<1e6` 那道门，是唯一可能把 1M 模型
> 救回来的路径 —— 但它查的表在这个 build 里只有 `claude-sonnet-5`（默认 967000），
> 且查表用的是**归一化后**的 ID。

两张白名单（**2.1.220 的值，会随版本变**）：

```text
白名单 Set:  ["claude-sonnet-4-6", "claude-opus-4-6", "claude-opus-4-8", "claude-opus-5"]
窗口表:      { "claude-sonnet-5": { default: 967000, surfaces: {...: 500000} } }
```

边界常量：下限 `1e5`、上限 `1e6`、白名单档取的窗口 `200000`。

## 三、`[1m]` 变体的代价（已确证）

判 1M 的方式是对**模型 ID 字符串**做 `/\[1m\]/i` 正则，命中即直接返回 `1e6`。于是：

```text
`claude-opus-4-8[1m]` → 正则命中 → 有效窗口 = 1e6
  → 第 5 档的「窗口 < 1e6」不成立 → 跳过白名单
  → 第 5b 档：归一化**不剥** `[1m]`（只剥 `-\d{8}$` 日期后缀）→ 查表不中
  → 落第 6 档 `auto` → 入口门 return → **该模型永不自动压缩**
```

本仓 CATALOG 确实通告 `[1m]` 变体（`supports_1m: true` 的 6 个模型，含
`claude-opus-4-8[1m]` / `claude-opus-4-6[1m]`）。**这些变体不该删**：1M 窗口是真功能
（给只能传纯模型名的客户端一个显式变体名），失效的只是客户端侧一个便利特性，两者不对等。
代价已写进 `model_catalog.rs` 的 `supports_1m` 文档注释。

> 唯一的例外路径：若客户端侧「1M 额度被封」的进程内标志为真，有效窗口会退回 200000，
> 反而重新命中第 5 档。网关不发相关额度头 ⇒ 该标志恒 false ⇒ 对本仓用户不成立。

## 四、为什么网关侧没有协议通道

第 3 档（`clientdata`）是六档里唯一「能让服务端替客户端声明窗口」的一档，但它**读的是
客户端本地磁盘缓存**，而那份缓存只有一个写入者：Claude Code 自己向
`<base_url>/api/claude_cli/bootstrap` 发的那次 GET。本网关不实现该端点 ⇒ 缓存恒空 ⇒ 该档恒
返回 null。

> 🔴 **一处需要纠正的旧口头结论**：曾有说法是「网关模式下 bootstrap 整个被跳过
> （`[Bootstrap] Skipped: 3P provider`）」。**实测不是这样**：那条 skip 的条件是
> provider ≠ `firstParty`，而 provider 由 `CLAUDE_CODE_USE_BEDROCK` / `_VERTEX` /
> `_FOUNDRY` / `_MANTLE` 等**环境变量**决定 —— 单纯改 `ANTHROPIC_BASE_URL` 指向本网关时
> provider **仍是 `firstParty`**，bootstrap **会真的去 fetch**（打的是
> `<ANTHROPIC_BASE_URL>/api/claude_cli/bootstrap`）。同理第 3 档读侧那道
> `provider !== "firstParty"` 的门对本网关**是放行的**。
>
> 结论没变（缓存仍恒空 ⇒ 该档不可用），但**原因不同**：不是「被跳过」，而是
> **端点 404 / 响应不含 `auto_compact_windows`**。
>
> ⚠️ 这个纠正带出一个**理论上存在的协议通道**：bootstrap 响应体的 schema 里确实有
> `auto_compact_windows`（形如 `{ "<model-id>": <int> }`，`nullish`），命中后会被写进本地
> 缓存并被第 3 档读到。即**网关若实现 `GET /api/claude_cli/bootstrap` 并返回该字段，
> 理论上可以直接把窗口喂给客户端**。
> **本次刻意不做**：① 未做端到端验证（只读了 schema 与写缓存的代码路径，没跑通真实
> 客户端）；② 它要求网关模仿一个未公开的第一方内部端点，跨版本稳定性无从保证；
> ③ 哨兵串那条路径成本低得多且已验证。留档备查，别当成已验证结论用。

## 五、服务端补救：compact-and-retry 的哨兵串

compact-and-retry 那条路径的判据是对错误 message 做**小写化子串匹配**：

```text
msg.toLowerCase().includes("prompt is too long")
  || msg.toLowerCase().includes("input is too long for requested model")
```

命中后客户端会压缩上下文并**自动重试**。要点（均已实测确认）：

- 该路径的前置条件只有「auto-compact 总开关开」+「非远端会话」两项，
  **不再过第 2 节那张来源表** ⇒ 它在 `[1m]` 变体上照样有效。
- message 里的 token 数字是**可选**的。有一个正则会去抓
  `prompt is too long ... N tokens > M` 里的两个数，抓不到只是让提示语措辞变笼统，
  **不影响是否触发压缩**。所以不必伪造数字。
- 只看 message，**不看 HTTP 状态码** ⇒ 本仓这两条继续返 400 是对的（原请求重试确实无意义，
  客户端要做的是先压缩再重试）。

### 落地位置与验收判据

| 项 | 值 |
|---|---|
| 常量 | `src/anthropic/handlers.rs::OVERFLOW_COMPACT_HINT = "prompt is too long"` |
| 命中分支 | `translate_context_input` 的 `CONTENT_LENGTH_EXCEEDS_THRESHOLD` 与 `Input is too long` |
| 形态 | `prompt is too long: <既有中文排障文案>`（**前缀**，不是替换） |
| 状态码 | 不变，仍 `400 invalid_request_error` |
| 承重测试 | `overflow_errors_must_match_claude_code_compact_retry_predicate` |

承重测试断言的是**外部消费者的判据能命中**（`msg.to_lowercase().contains(...)`），并刻意
写**字面量**而非引用 `OVERFLOW_COMPACT_HINT` —— 引用了就是同义反复（把常量改成别的值，
断言依然成立）。已做双向回退验证：把常量换成别的串 → FAILED；把前缀从 format! 里删掉
→ FAILED。

🔴 **改这两条文案时必须保留英文前缀。** 删掉它不会有任何编译或运行期报错，只会让用户的
自动压缩静默失效。

## 六、用户侧绕过（最直接的解）

服务端补救只覆盖「已经撞满」的情形；想恢复**按水位主动压缩**，只能在客户端侧把窗口来源
抬到第 1 或第 2 档：

```bash
export CLAUDE_CODE_AUTO_COMPACT_WINDOW=200000
```

- 必须是**纯整数**，且落在 `[100000, 1000000]`。
- 写 `200k` / `500k` 之类会被解析成 `200` / `500`，再被 clamp 到下限 `1e5`
  —— 不会报错，只是值不是你以为的那个。
- 等价手段：用户设置文件里的 `autoCompactWindow`（第 2 档）。

> ⚠️ 本仓任何自动化**不要**去改用户的 `~/.claude/settings.json`：那是用户自己的配置，
> 需他本人确认。

## 七、怎么复核（下次会话照这个做，别信本文的二手结论）

```bash
mkdir -p /tmp/cc-ac
strings -n 6 ~/.local/share/claude/versions/<版本> > /tmp/cc-ac/s.txt
```

然后按**日志字面量**反向定位（它们比符号名稳定）：

- `autocompact: tokens=` —— 入口门之后那行，函数体里就能看到那道门
- `[Bootstrap] Skipped` —— bootstrap 的各条 skip 分支与其真实条件
- `prompt is too long` —— compact-and-retry 判据 + 抓 token 数的正则

⚠️ **不要用 `grep -o "...\{0,200\}"`**：这个二进制里有 30 万字符量级的压缩单行，
`grep -o` 会灾难性回溯（实测两条命令双双跑满 120s 超时）。用 `awk` 的 `index()`
按字节窗口截取。

⚠️ 用完 `rm -rf /tmp/cc-ac`（`strings` 输出约 20MB）。

### 端到端验证自动压缩是否真的在跑

```bash
claude --debug
```

看有没有 `autocompact: tokens=... level=... effectiveWindow=...` 那行：

- **打了** → 门过了，窗口来源不是兜底档，反应式压缩在工作（`level` 是 `compact`/`blocked`
  时会真压）。
- **一次都不打** → 落了兜底档、门提前 return ⇒ 该会话只能靠第 5 节的 compact-and-retry。

---

## 本文档的记账约定

这个仓有个反复出现的病症：**结论产生了但没回写到断言处，后来的会话把过期断言当约束**。
所以：

- 每条结论都标了**日期**与**build 版本**。
- 凡「符号名」一律不记（会漂），只记机制与可观测判据。
- 第 4 节那条纠正就是这个病症的实例：一条听起来合理的旧结论（「bootstrap 被跳过」）
  在本次复核中被证伪，而它的**结论恰好是对的** —— 这种「对的结论 + 错的原因」最难发现，
  因为没人会去复核一个不妨碍任何事的解释。
