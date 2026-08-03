# 下一会话的任务清单 — 2026-07-31

> **你的任务：完成本文第 2 节的待办，然后把全部改动上线。**
> 背景与教训见 `HANDOFF-2026-07-31.md`（同目录，352 行）——**第 0 节的六条硬约束必须先读**。
> 本文只讲「要做什么」和「怎么上线」。

---

## 1. 当前状态（起点）

| 项 | 值 |
|---|---|
| 线上 | **0.7.46**（`deploy/vps` = `89ee5299`），active，零空窗交接过 |
| 本地 `Cargo.toml` | 0.7.46（已 bump，与线上一致） |
| 本地未上线的源码改动 | **无** —— 源码已全部上线 |
| 质量门 | 969 单测全绿 / clippy 0 error / tsc 干净 |
| 号池 | 2/2，130 RPM |

### ✅ 已修**并已上线**（0.7.46）的两条（原本都在杀号）

> 这两条已由上一会话完成并验证，**不要重做**。留在这里是为了让你理解号池此前为何被烧。

**A. `ksk_` 号走错端点被误禁**（`src/kiro/provider.rs` 的 `endpoint_for`）

`endpoint_for` 原先只读 `credentials.endpoint` 原始字段，漏了 `effective_endpoint()` 里
「`ksk_` API Key 号自动路由到 CLI 端点」那一层。实测：

```
effective_endpoint=cli    hot_path=ide     ← 口径分叉
```

而 `endpoint/mod.rs` 的 `for_credentials` 文档明写"口径与 endpoint_for 完全一致" —— **那句话此前是假的**：
旁路走 `effective_endpoint`、请求热路径不走。

后果：一个**完全健康**的 `ksk_` 号（线上正是 ksk_ 开头），若未手工填 `endpoint: cli`，
请求打到 IDE 端点 → 403 → 连续 6 次 → 判死号自动禁用。**这直接解释了号池被持续烧。**

已修 + 2 个测试（源码级守卫 + 行为测试），回退即 FAIL。

**B. 403 风控打死全池后无自愈路径**（`token_manager.rs` 新增 `is_self_healable_reason`）

全池自愈的判定只匹配 `TooManyFailures`，而 403 风控走 `SuspiciousActivityAuto`。
线上实测（48h）：

```
「判定为死号并自动禁用」  46 次
「执行自愈」               0 次     ← 自愈对这个原因从未生效
```

而 403 `TEMPORARILY_SUSPENDED` 是**临时态**（历史事故：曾被当永久封禁 → 12h 内 88 次误禁 +
36 次全池活锁 → 拒绝率 100%）。已抽 `is_self_healable_reason()` 收口，
**刻意排除** `AccountSuspended` / `QuotaExceeded` / `Manual` 等（既有测试
`test_account_suspended_is_not_auto_recovered` 锁住这条边界，我验证过它仍通过）。

---

## 2. 你的任务

### ~~任务 1：验证并上线 A、B 两条~~ ✅ **已完成（0.7.46）**

上一会话已完成：4 个回归测试全部验过"回退即 FAIL"，走 CI → 三处 sha256 核对
（`03d6860a…`）→ 健康检查 → 零空窗交接 → 真实流量验证。

**上线后实测效果**（这是判断修复是否真生效的依据）：

| 指标 | 修复前 | 交接后 |
|---|---|---|
| 「判定为死号并自动禁用」 | 48h **46 次** | **0** |
| 403（`auth_failed`） | — | **0** |
| 近 5 分钟 outcome | — | **88 全 success** |
| panic | — | **0** |
| 号池 | 1/1，65 RPM | **2/2，130 RPM** |

关键佐证：当前池内两个号**都是 `ksk_` 且 `endpoint=None`** —— 正是缺陷 A 的受害场景，
修复后 403 与误禁均归零。

⚠️ 若后续观察到「判定为死号」重新上升，**不要先怀疑这两条**（已有回归测试锁住），
优先查是否上游真的在风控（看 `traces.db` 的逐号成功率，别信 `gateway-status` 的号数——它会假绿）。

### 任务 2（必做）：另一个 AI 报的其余 4 条，我已初步核实，**逐条判断后再动手**

我只做了初步 grep，**没有一条写过测试**。你要自己定性。

| # | 指控 | 我初步核实的结果 | 建议 |
|---|---|---|---|
| 2.1 | `promptCacheEnabled=false` 会清零计费缓存字段，而 `config.rs:297` 承诺"只管下发，不影响用量统计" | ⚠️ **未能证实**。`config.rs:297` 的承诺确实存在，但我 grep 不到"关闭时清零 record.cache_* "的代码。**可能是误报** | 先写测试：置 false → 跑一次记账 → 断言 `record.cache_read_tokens` 仍有值。测不出就标"已核实无问题"并记录，别改 |
| 2.2 | 5xx 与 429 共用同一去重集合 `rate_limited_this_call`，同链先 429 后 5xx 会静默吞掉第二个冷却 | ⚠️ **未能证实**。`rate_limited_this_call` 只出现在 429/风控分支（`provider.rs:475,612,729`），我在 5xx 分支没 grep 到它 | 同上，先测。若真共用，两者冷却策略不同（429 递增退避 vs 5xx 固定 30s），吞掉是真问题 |
| 2.3 | 关机固定睡 8 秒，是**地板不是上限**；且 SIGTERM 后监听器还在收新连接 | ✅ **成立**。`main.rs:717` 是裸 `tokio::time::sleep(8s)`，不是 `select!` —— 即使在途请求早已 drain 完也要白等 8 秒。每次交接白慢 8 秒 | 改成 `select!` 竞速：drain 完成 or 8s 超时，取先到者。**注意**：交接是零空窗的（新实例已在同端口顶着），所以这 8 秒不影响可用性，只影响交接总时长。优先级中 |
| 2.4 | 重试预算从 64 砍到 12 且移除 `.max(available)`，>12 个号的池子无法遍历；文档注释还写着"保证每个号至少试一次"已不成立 | ✅ **成立**。`compute_max_retries` 的 `_available` 参数已不参与计算（`provider.rs:65-77`），注释自己解释了为什么刻意移除（旧代码的内层 `.max(available)` 会让 12 的硬上限自我抵消 → 43 号时预算 43 → 单请求扫全池耗尽 45s 墙钟） | **这是刻意权衡，不是 bug**。但它依赖"坏号被自动禁用后不进候选集"这个前提。**该做的是把过期的文档注释改对**，而不是改行为。号池 >12 时需重新评估 |

### 任务 3（建议做）：`HANDOFF-2026-07-31.md` 第 3 节的未完成项

按性价比排序，**不必全做**：

1. **瘦 `live_snapshot`**（低风险、收益明确）：`live_creds` 每 1.5s 调 `snapshot()`，
   而 `LiveCred` 只用 id/rpm/inflight，却白算每号**两次 SHA256** + 十几个字段 clone。
2. **`hotswap.sh` 两个真 bug**：`:146` 回滚用 `mv "${BIN}.prev"`（**用一次就吃掉回滚点**，应为 `cp -a`）；
   无 `trap`（中途 Ctrl-C 会留孤儿裸实例常驻同端口、双写 `kiro_stats.json`）。
3. **`p_avail` 批量化**（⚠️ 碰**选号热路径**，错了会掀翻分流）：方案在 `HANDOFF-2026-07-31.md` 3.2，
   注意 `family_key` 会重复（M365 同租户）→ 批量版与逐个调用**不逐位相同**，是刻意的语义变更。
   **必须与 live_snapshot 拆成两个 commit**，否则出问题无法二分定位。

**明确不要做**（已有证据判定，别重做）：`busy_timeout`（rusqlite 已默认 5000ms）、
`spawn_blocking` 包 trace_db 查询、上游 `kiro.rs` 对照线（28 条 fix 零可修项）。理由见 3.5/3.6。

---

## 3. 上线流程（我实测走通过，照做即可）

### 3.1 bump 版本（必须，否则 tag 会被 CI 门禁拒）

```bash
# Cargo.toml: 0.7.45 → 0.7.46，然后 cargo build 同步 Cargo.lock
```

### 3.2 提交：plumbing 快照，**绝不碰真实 index**

工作树有 60+ 个其它会话的未提交改动，`git add`/`commit` 会把它们一起带走。

```bash
git fetch origin deploy/vps
export GIT_INDEX_FILE=/tmp/snap.index && rm -f "$GIT_INDEX_FILE"
git read-tree origin/deploy/vps
git add -A -- src admin-ui/src deploy Cargo.toml Cargo.lock
TREE=$(git write-tree)
# 先看清要上线的内容，确认没混入别人的改动
git diff-tree -r --stat origin/deploy/vps $TREE
C=$(git commit-tree $TREE -p origin/deploy/vps -m "fix(0.7.46): ...")
git branch -f deploy/vps $C && unset GIT_INDEX_FILE
git push origin deploy/vps
```

**做完必须核对**：`git rev-parse --abbrev-ref HEAD` 仍是 `fix/macos-support-and-critical-bugs`、
`git status --porcelain | wc -l` 与开始时一致。

> 💡 判断"哪些文件是我的改动"：逐个 `git diff-tree -p origin/deploy/vps $TREE -- <file>`
> 看新增行内容。别用"文件里有没有某个注释"这类启发式——我试过，会误判
> （标记性注释可能落在 diff 范围之外）。

### 3.3 CI 出二进制

```bash
gh workflow run deploy-build.yml --repo dwgx/KiroStudio-skiapi --ref deploy/vps -f run_tests=true
# 等 completed/success，然后：
gh run download <RUN_ID> --repo dwgx/KiroStudio-skiapi -D /tmp/dl
```

**三处 sha256 必须一致**：CI 的 `.sha256` 文件 / 本地 `shasum -a 256` / 服务器 `sha256sum`。

### 3.4 交接（零空窗）

```bash
scp /tmp/dl/kirostudio-linux-x86_64/kirostudio-linux-x86_64 ws-vps:/tmp/kirostudio.new
scp deploy/hotswap.sh ws-vps:/tmp/

# 备份（必做）——包括一个"持久"回滚点，因为 kirostudio-update rollback 用 mv 会吃掉 .prev
ssh ws-vps 'cp -a /opt/kirostudio/bin/kirostudio /opt/kirostudio/bin/kirostudio.rollback-0745
  cp -a /opt/kirostudio/data/credentials.json /opt/kirostudio/data/credentials.json.pre-0746'

ssh ws-vps 'chmod +x /tmp/kirostudio.new && /tmp/hotswap.sh /tmp/kirostudio.new check'  # 先只检查
ssh ws-vps '/tmp/hotswap.sh /tmp/kirostudio.new'                                        # 通过再交接
```

### 3.5 验证（缺一不可）

```bash
# 1. 版本 + 只有一个进程（排除孤儿裸实例）
ssh ws-vps '/opt/kirostudio/bin/kirostudio --version; pgrep -c -f /opt/kirostudio/bin/kirostudio'
#    → 0.7.46 且计数为 1

# 2. 真实流量（⚠️ 最关键：hotswap 的健康检查只打 /v1/models，
#    它不碰号池、不碰上游 —— 选号/流式的回归它抓不到）
ssh ws-vps 'K=$(python3 -c "import json;print(json.load(open(\"/opt/kirostudio/data/config.json\"))[\"apiKey\"])")
  curl -s -o /dev/null -w "非流式=%{http_code}\n" -X POST -H "x-api-key: $K" -H "content-type: application/json" \
    -d "{\"model\":\"claude-sonnet-5\",\"max_tokens\":16,\"messages\":[{\"role\":\"user\",\"content\":\"say ok\"}]}" \
    http://172.30.0.1:8990/v1/messages'

# 3. 错误率 + panic
ssh ws-vps 'D=/opt/kirostudio/data/usage/traces.db
  sqlite3 -header -column $D "select outcome,count(*) n from traces where ts_ms>(strftime(\"%s\",\"now\")-300)*1000 group by 1 order by n desc;"
  journalctl -u kirostudio --since "10 minutes ago" --no-pager | grep -c panicked'

# 4. ⭐ 本次特有：确认 ksk_ 号不再被误禁（这是任务 1 的验证点）
ssh ws-vps 'journalctl -u kirostudio --since "30 minutes ago" --no-pager | grep -c "判定为死号并自动禁用"'
#    → 观察一段时间后应显著低于修复前（48h 46 次的基线）
```

### 3.6 打 tag（让 OTA 能升到这一版）

**建议放在流量验证通过之后**，不要给未验证的二进制固化版本号。

```bash
git tag -a v0.7.46 <commit> -m "v0.7.46: ..." && git push origin v0.7.46
```

`release.yml` 会校验 tag 与 `Cargo.toml` 版本一致（不一致直接 fail），然后建 Release
（linux + macOS×2 + windows 四个 asset + sha256）。

### 3.7 回滚

```bash
ssh ws-vps '/tmp/hotswap.sh /opt/kirostudio/bin/kirostudio.rollback-0745'   # 零空窗
```

**回滚起不来时**查 `credentials.json` 是否含新枚举变体。0.7.45 起 `DisabledReason` 已有
`#[serde(other)] Unknown` 兜底，所以 **0.7.46 ↔ 0.7.45 互相回滚是安全的**。

---

## 4. OTA 现状（已打通，但有个隐患）

```json
{"has_update":false,"local_version":"0.7.45","latest_version":"v0.7.45","error":null}
```

`error: null` = 能读 private 仓库的 Release。三个前置全齐（Release / PAT / EnvironmentFile）。

⚠️ **PAT 是 `gho_` 型**（取自 `gh auth token`，写在 `/etc/kirostudio/update.env`）。
**用户下次 `gh auth login` 重新登录后它会轮换 → OTA 失效。**
彻底稳的做法：GitHub 网页建 fine-grained PAT（只勾 `dwgx/KiroStudio-skiapi`，Contents: read），
换掉 env 里那一行再重启。链路已验证通，换 token 只是改一行 + 重启。

**打完 tag 后可以顺手验一次**：面板「检查更新」应显示 `has_update: true` 并能升级。

---

## 5. 三条最容易踩的坑（我都踩过）

1. **测试"通过"不等于它测到了东西。** 我验回滚 BLOCKER 时复现代码漏了 struct 级
   `#[serde(rename_all)]`，字段名从未匹配上，测试"通过"其实什么都没测。
   **写完测试要问：它是否真的执行到了被测路径？** 最可靠的验证是"回退修复 → 必须 FAILED"。
2. **同一前提失败两次就停下来量，不要继续改测试。** G1 元测试我失败三次才做对，
   前两次都是"机制推理正确但复现方式不对"。第三次写诊断脚本把两种实现**并排实测**才找到真因。
3. **改代码只用 Edit 工具。** 我用 `sed` 改 Rust 破坏过大括号（模式里含 `|`）；
   用 `git checkout-index` 还原文件毁掉别人 515 行未提交代码。
   备份只用 `cp` 到 `/tmp`，且**别急着 `rm`**。

另外：fan-out subagent 时注意 **agent 调用走的正是 `k1ro.skiapi.dev`（我们自己的网关）**，
号池紧张时 8 路并行必然打穿它 —— 本轮 fleet 被 502 打死 **5 次**。
建议 agent 只做只读侦察、并发 ≤3，写代码由主线串行做。
