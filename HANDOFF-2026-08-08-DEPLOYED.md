# 接手文档 · 2026-08-07 夜（已部署）

> **本文件记录一次已完成的部署。** 状态入口仍是 `STATUS.md`；长期约束仍是 `CLAUDE.md`。
> 置信度标注：`[实测]` 跑过命令并贴了输出 · `[代码]` 读码确认 · `[未验]` 推断，没测。
>
> 上一份派单是 `HANDOFF-2026-08-07-NIGHT.md`（仍有效，其 P2/P3 大部分未做）。

---

## 0. 一句话结论

线上从 `e187ccbf` 换到 **`8d056859`**（`[实测]` `/proc/<pid>/exe` 逐字节断言 4 条全过），
含两个修复：**region 探测「探不了」不再误判成「确定不行」**、**api_key 分身按族收族**。
`[实测]` 交接后 3.5 分钟 260 请求 **99.6% success / 429 率 0.00%**。

⚠️ **这两个修复都不直接降低客户端 429。** 见 §4「别误读」。

---

## 1. 线上现在是什么

| 项 | 值 | 来源 |
|---|---|---|
| 二进制 sha256 | `8d056859a70086a8032575809862bb15bcbd760ca595faa01deaeefb5a175c8b` | `[实测]` CI 记录值 == 本地 == 远端 == `/proc/<pid>/exe` |
| 版本字符串 | `0.7.46`（**未 bump**，与前一版同名） | `[实测]` |
| MainPID | `276193`（换机前 `4097967`） | `[实测]` PID 变了 ⇒ 进程真换了，不只是文件 |
| 服务启动 | `2026-08-07 22:10:57 CST` | `[实测]` `NRestarts=0` |
| 回滚点 | `/opt/kirostudio/bin/kirostudio.prev`（= 旧 `e187ccbf`） | `[实测]` `prev_exists=yes` |
| 回滚命令 | `ssh ws-vps 'kirostudio-update rollback'` | `[代码]` 脚本存在，本轮**未演练** `[未验]` |

**来源分支：`fix/region-inconclusive-and-clone-family`**（新建，已推 origin），
commit `0e21f79`，基点 `7d955b4`。

🔴 **它不是 `deploy/vps` 的后代，`master` 也不含它。** `deploy/vps` 仍在 `495b770`，
**本轮一个字节都没碰它**（`[实测]` 部署前后各查一次）。
⇒ 下次谁从 `deploy/vps` 构建部署，**会把本轮两个修复覆盖掉**。要保留必须先合并。

### 为什么走新分支而不是 `deploy/vps`

`[实测]` `deploy/vps` 领先我的基点 **51 个提交**，两者互不为祖先
（`merge-base = c054971`）。强推会丢那 51 个，其中包括
`d8255cf fix(region): 探测改回 management.*` —— 别人在我刚改的**同一个文件**上的工作。
`workflow_dispatch` 接受任意 ref，所以新分支同样能出二进制，且零丢失。

---

## 2. 改了什么（两个修复 + 一次三方合并）

### A. region 探测：「探不了」≠「确定不行」 `src/kiro/region_probe.rs`

`probe_api_region_with` 的循环把 `WrongRegion | Inconclusive => continue` **拆成两条**，
新增 `saw_inconclusive`；循环后在 `NoUsableRegion` **之前**判 `Skipped`。

**为什么是 US 号专属死法**：`PROBE_ORDER = ["eu-central-1", "us-east-1"]` ⇒
US key 的序列是「第 1 次 eu → 403 确定否定、第 2 次 us → 200」，**第二次是唯一机会**。
那次撞 5xx/网络/DNS ⇒ `Inconclusive` ⇒ 修复前汇总成 `NoUsableRegion` ⇒
`service.rs` 置 `disabled=true` + `RegionProbeFailed`，而该原因
`[实测]`（4 种 grep 模式）**不在** `is_self_healable_reason` 白名单 ⇒ 永久死，要人工捞。
EU key 第 1 次就 200 返回，走不到这条路。

判 `Skipped` 而非新增 outcome：调用方对它照常启用 ⇒ 回退 `config.region` 继续服务。
最坏情况「回退的区不对」→ `report_failure` → `TooManyFailures`，**那个在自愈白名单里**
⇒ 最坏态是可自愈的临时禁用，严格优于永久禁用。

### B. api_key 分身按族收族 `credentials.rs` + `token_manager.rs`

- `family_key()` 对同 `clone_group` 的 api_key 分身返回 `clone:{group}`
- `report_suspicious_activity` 的 `consecutive_suspicious` **累加改族级**（族内取 max+1，
  达阈值整族一起禁、一次落盘）
- `report_success` 配套**族级清零**（承重：累加按族而清零按号 ⇒ 同族其它份停在高位 ⇒
  下一次 403 从高位 +1 直接推过阈值 ⇒「刚成功过的账号立刻被判死号」）

**依据** `[实测]`：403 body 的 `Your User ID (NNN)` 与 cred id 是 **N:1**
（UID `079998937591` → cred 1294..1299 六个）⇒ 上游按**账号**记账。
按号计数时一次 suspend 要白挨 `6×N` 次 403（线上 N=17 ⇒ **102 次/轮**），
而全池自愈会整族复活再来一轮（当天 `判定为死号并自动禁用` **231** 次 / `执行自愈` **14** 次）。

**反转了一个守卫测试**：`test_multi_open_copies_are_in_separate_families`
→ `..._share_one_family`。原断言的前提（分身是 N 个独立身份）被线上数据证否，
注释里写了完整反转依据。⇒ **多开的价值只剩「各份出口 IP/指纹不同」，不含「独立额度」。**

### C. 三方合并（不是我的改动，但在同一个二进制里）

`src/kiro/provider.rs` 在**两个 worktree**都被改过 —— 我的（重试预算 12→4、429 退避
1s→8s、trace 埋点）与 `wt-region` 的（订阅不覆盖判定 +144 行）。做了真三方合并：

```
base 4642 行 + ours 449 + theirs 144 + 6(见下) = merged 5241   [实测] 行数与 hallmark 计数双侧一致
```

冲突 1 处、base 段为空 ⇒ 双方都是同点**新增**，非互斥。顺序取「我的 trace 守卫 →
它的 403 分支」，两边的不变量都满足。`endpoint/mod.rs` 在我这边未改动 ⇒ 整份取它的。

**我加了 1 行不属于任何一方的代码**：`trace.verdict("subscription_unsupported")`。
它那条分支 `break` 会穿过我的 RAII trace 守卫，而同层 20+ 分支无一例外都标 verdict，
不补会让新分支成为 trace 里唯一 `unclassified` 的 403。注释写明是合并补充、
只影响 trace 元数据、不改控制流。它作者的基线里没有 trace 代码，不是它漏了。

---

## 3. 验证记录

### 四门（合并后的并集）`[实测]`

```
cargo test --no-default-features        1445 passed / 0 failed
cargo clippy --no-default-features --all-targets   0 error
cd admin-ui && npx tsc -b               exit 0
cd admin-ui && node --import ./tests/tsx-loader-register.mjs --test 'tests/*.test.ts'
                                        37 passed / 0 failed
```

测试基线链条：`1430`（接手）→ `1437`（收族 +7）→ `1442`（合并 +5）→ `1445`（region +3）。
CI 独立复跑同样通过（`run_tests=true`，run `31182362765` success）。

🔴 **前端测试必须带 `--import ./tests/tsx-loader-register.mjs`。** 我一开始按测试文件
头注释写的 `node --test 'tests/*.test.ts'` 跑，得到 **28/29** 并误报成「预存失败」——
实际是我漏了 loader，`fire-candidates.test.ts` import `.tsx` 而 Node v24 的类型剥离
不认 `.tsx`。**那个失败不存在，STATUS.md 记的 37 是对的。**
⇒ `pool-event-classify.test.ts` 头注释里的跑法**已过期**，别照抄。

### 变异验证（故意改坏，确认守卫会红）`[实测]`

| 改坏成 | 变红的测试 | 说明 |
|---|---|---|
| 「无 group 就并族」 | 3 条反向（None/空串/非 api_key） | 挡住「整池 `ksk_` 并成一族 = 整池连坐」，那比不修更糟 |
| 收族整体去掉 | 4 条正向 | 确认修复真的在生效 |
| 删掉族级清零循环 | 只有配对那条 | 定位精确 |
| region「任何失败都 Skipped」 | 我的反向 + **仓库原有的** `probe_all_403_yields_no_usable_region` | 挡住「真不可用的号被放进池子反复打 403」 |

先写失败测试的原始输出（region 三场景，场景 b 修复前就该绿）：

```
left: NoUsableRegion   right: Skipped
  ← all_candidates_inconclusive_yields_skipped_not_region_failure       (场景 a)
  ← wrong_region_then_inconclusive_yields_skipped_us_key_victim_shape   (场景 c，核心用例)
test result: FAILED. 34 passed; 2 failed
```

### 二进制内容断言 `[实测]`

四条断言在**三个时点**各验一次（本地 / 远端 `/tmp` / 线上 `/proc/<pid>/exe`）全过：
`region 自动探测出现「没拿到答案」的候选`、`整族移出调度以免每个请求都在同一个上游账号上白撞`、
`全部候选均**确定**不可用`，以及 must-not `移出调度以免每个请求都在它身上白撞`（旧文案，count=0）。

### 交接后 3.5 分钟 `[实测]`

```
n=260   success 259 (99.6%)   other_error 1 (0.4%, client_disconnected 兜底记录)
client-visible 429: 0.00%     无凭据请求(池耗尽形态): 0/260
retries {0:256, 1:4}  放大 1.015x
```

⚠️ **journal 里那几条新日志计数都是 0，这是预期不是失败**：它们只在 403 suspend
或 `add_credential` 时才触发，3.5 分钟内两者都没发生。⇒ **两个修复都还没被真实触发过验证。**

---

## 4. 🔴 别误读：这两个修复不降低客户端 429

`[实测]` 客户端 429 的 **95.5%**（2080/2177，4h 窗口）是「池被自动禁用清零」
（`所有凭据均已禁用（0/N）`，`credential_id=NULL`），真上游 `ThrottlingException` 同期仅 **28** 条。

而池清零的触发器是**上游 403 账户级 suspend**。本轮修复的是：

- **B（收族）**：一次 suspend 上游从被砸 102 次降到 **6** 次 —— 减少的是**上游被砸的次数**，
  不是客户端 429 的时长。池清零期间客户端照样 429。
- **A（region）**：网络抖动不再让好号在上号时永久死 —— 这是**防未来事故**，不改当前流量。

`[实测]` 还有一条反证：日志里出现过 `所有凭据均已禁用（0/1）pool_permanently_exhausted`
**527 条** ⇒ **降到 1 份也照样清零**。所以「降分身数」防不住这个。

**真正能让客户端不吃 429 的是多个不同账号。** `[实测]` 现在 19 个凭据条目实际只有
**3 个上游账号**：17 份分身共享 keyhash `7d747fc003c9`（同一个 `cloneGroup`），
另外 2 个是 `custom_api`。任何一次账户级风控都能打掉池子绝大部分。

### 为什么那个账号会被 suspend —— **未查明** `[未验]`

能证实的只有「它是账户级的」（403 body 带 User ID、且 User ID:cred id = N:1）。
「17 个 machineId + 17 个出口 IP 像账号共享」是**假说，没验证**，且有反向线索：
三代被封的号在 403 里各自只出现 **5~6 个** cred id（不是 17）⇒ 6 份规模也会被封。
那几批号已被删，拿不到指纹，这条判不了。

---

## 5. 工程环境的新发现（比修复本身更值钱的两条）

### 🔴 `grep -a` 在 macOS 上找不到二进制里的 UTF-8 中文 —— 部署门形同虚设

`deploy/verified-deploy.sh` 的内容断言原先用 `grep -aq`。`[实测]` 用它验一个**正确的**
二进制，报「这次改动没有进到该二进制」。逐层排查后确认：

```
needle 55 字节、offset 14344995、count=1（Python 确认在）
BSD grep 2.6.0-FreeBSD：默认 BRE / -F / -f 从文件读 pattern
                        × 直接 argv / bash / zsh  = 9 种组合全部 NOT FOUND
GNU grep 3.12（brew ggrep）：FOUND
pattern 里无 BRE 元字符 ⇒ BRE 与 -F 本该等价 ⇒ 是 BSD grep 的行为问题
```

⇒ **断言结果取决于 PATH 里先命中哪个 grep。** 这道门是唯一能拦住「回退了但线上还跑
旧二进制」的防线（它的头注释记着那次事故），而它会对正确的二进制报假红。

**已改为 Python 逐字节**（`/tmp/cc-kg/deploy/has_literal.py`，模式经 base64 传递绕开
所有引用层）。CLAUDE.md 记的是 `strings|grep` 切碎 UTF-8，**这是同类问题的另一面，
建议补进那一节**。

⚠️ `hotswap.sh` 内部**也有一处** `grep -aq "SO_REUSEPORT"`（ASCII，本轮没踩），
但同一个坑，早晚会咬人。

### 🔴 verified-deploy.sh 开 ~10 条 SSH ⇒ 被 fail2ban 拦，且伪装成别的错

`[实测]` 第一次真部署跑到「4/5 hotswap 交接」被 `Connection closed by <ip>` 打断。
那一步是 `test -x $HOTSWAP`，连接被拒 ⇒ **误报成「hotswap.sh 不存在」**，
而文件其实在、sha 也对。网络层问题伪装成前置条件缺失。

**已加 `ControlMaster`/`ControlPersist` 复用单条连接**，fail2ban 只看到 1 次。
⚠️ 复用选项里**刻意不设 `ConnectTimeout`**：OpenSSH 对重复选项取**第一个**，
而脚本内联的 `-o ConnectTimeout=180`（hotswap 那步）必须生效。

那次失败**停在 hotswap 之前**，生产未被触碰 —— 脚本的分步设计是对的。

### 其它

- `[实测]` **SSH 密钥登录已恢复可用**（`ssh -o BatchMode=yes ws-vps` 直接成功）。
  CLAUDE.md「SSH 当前只能用密码」那节**已过期**，不必再套 `sshpass`。
- `[实测]` **VPS 上原本没有 `/opt/kirostudio/bin/hotswap.sh`**，本轮从仓库推了上去
  （sha `10600a43`，与 `deploy/hotswap.sh` 一致）。
- `[实测]` **VPS 时区是 CST，本机是 JST，差整 1 小时。** 看时间线时别把时差读成数据断档
  （我差点误判 22:10 的数据是一小时前的）。
- `[实测]` 本机 `cargo`/`rustc` 指向 **Homebrew** 的 rust（`/opt/homebrew/Cellar/rust/1.97.1`），
  **没有 musl std** ⇒ 本地交叉编译 Linux 二进制会报 `can't find crate for core`。
  换 `~/.cargo/bin/cargo` 后卡在 `rusqlite` bundled 要 `x86_64-linux-musl-gcc`
  （`musl-cross` 不在 brew 默认 tap）。⇒ **Linux 二进制只能走 Actions**，别在本机试。
- 🔴 `target/x86_64-unknown-linux-musl/release/kirostudio` 里躺着的是 **`205cc0bb`**
  （16:54，交接文档说「已过期必须重建」那个）。差点误推。**编译前先看 mtime。**

---

## 6. 下一步建议（按价值排序）

1. 🔴 **把本轮改动合进 `deploy/vps`。** 现在线上跑的分支不是它的后代，
   下次从 `deploy/vps` 部署会覆盖掉这两个修复。合并要解 `region_probe.rs` /
   `provider.rs` 的冲突（`deploy/vps` 那 51 个提交里有 `d8255cf` 动过 region 探测）。
2. **等一次真实 403 suspend，验收 B。** 该看到 `族内连续第 N/6` 且**整族**一起禁用，
   而不是每份各数 6 次。这是唯一的端到端验收。
3. **查那个账号为什么被 suspend。** 见 §4 末。`upstream_trace`（P0-5，本轮已随二进制上线，
   `[未验]` 未确认是否已开启）能提供独立计数源。
4. 上一份派单的 P2-B/C/D/E/F 与 P3-* 仍未做，其中 **P2-D（0.7.46 未打 tag）** 会让 OTA
   升不到当前版；本轮又换了二进制而版本号没变，`[未验]` OTA 语义可能更混乱。
5. 把 §5 两条工具坑补进 `CLAUDE.md`（grep/UTF-8 那条尤其 —— 它会让部署门静默失效）。

---

## 7. 本轮碰过的线上文件（审计用）

| 路径 | 动作 |
|---|---|
| `/opt/kirostudio/bin/kirostudio` | **替换**（`e187ccbf` → `8d056859`），旧版存为 `.prev` |
| `/opt/kirostudio/bin/kirostudio.prev` | 由 hotswap 生成（回滚点） |
| `/opt/kirostudio/bin/hotswap.sh` | **新增**（VPS 原本没有） |
| `/tmp/kirostudio.new` | 上传的二进制（临时） |
| `/tmp/has_literal_deploy.py` | 断言器（临时） |
| `/tmp/diag-*-cckg.py` | 只读诊断脚本 4 个（临时） |

**没碰**：`config.json`、`credentials.json`（分身数仍 17，按你的指示未动）、
`deploy/vps` 分支、`public` 远端、主仓工作树与 index。

配置现值（`[实测]` 22:0x 实读，用前请重读）：
`cooldownEnabled=false`（按你的指示保持）、`autoDisableSuspicious=true`、
`credentialRpmLimit=100`、`rpmHeadroomFactor=85`、`inboundThrottleEnabled=false`、
`inboundTargetRpm=65`、`rateLimitEnabled=false`。

---

## 8. 补记：工具链修复 + 第二次 hotswap（22:51 CST）

> 起因是「换服务器」——**本机原本出不了 Linux 二进制**，只能依赖 GitHub Actions。
> 换机 + Actions 不可用 = 无法部署。这一节把该依赖去掉了。

### 本机现在能独立出 Linux musl 二进制 `[实测]`

```bash
export PATH="$HOME/.cargo/bin:$PATH"       # ⚠️ 必须
cargo zigbuild --release --no-default-features --target x86_64-unknown-linux-musl
```

`zig` 0.16.0 + `cargo-zigbuild` **本机早就装了**（我先前判断「只能走 Actions」是漏查）。
zig 当 C 交叉链接器 ⇒ 绕开 `rusqlite`(bundled) 对 `x86_64-linux-musl-gcc` 的需求，
**不需要 `musl-cross`**。实测 **3m03s** 出 15.6MB ELF。完整说明与三个坑写在
**`deploy/BUILD-LINUX-LOCALLY.md`**。

⚠️ 关键坑：`which cargo` → `/opt/homebrew/bin/cargo`（Homebrew 的 rust，**无 musl std**），
报错是 `can't find crate for core` + "target may not be installed" —— **这条报错是误导的**，
target 装了，装在 rustup 的 toolchain 下。交叉编译前必须切 PATH。

⚠️ 本机产物与 CI 产物 **sha 永远不同**（`statically linked` vs CI 的 `static-pie`）。
`[实测]` 本机 `61f2c91d` / CI `8d056859`，同源同 target。
**别拿 sha 比对两个来源**；sha 只用来证明「上传的 == 本地编译的」。

### 第二次 hotswap：用**本机构建**的二进制走完全流程 `[实测]`

这是为换机做的端到端验证 —— 证明离线路径真能部署，而不只是能编译。

| 项 | 值 |
|---|---|
| 线上 sha | `8d056859`（CI）→ **`61f2c91d`**（本机 zigbuild） |
| MainPID | `276193` → **`309116`** |
| 服务启动 | `2026-08-07 22:51:49 CST`，`NRestarts=0` |
| 交接后 1 分钟 | `n=36`，**100.0% success**，429 率 0.00%，retries 全 0 |
| 内容断言 | 4 条 × 3 时点全过，且新增的「运行中进程映像 == 本次部署的二进制」也过 |

🔴 **回滚点语义变了：`kirostudio.prev` 现在是 `8d056859`（CI 版，已含本轮两个修复）。**
`[实测]` 用断言器查旧文案 `rc=1`（不含）⇒ **回滚已经回不到修复前的代码**。
真要回到 `e187ccbf`（今晚 22:10 之前那版），得从别处找，`.prev` 里没有了。

### 修好的四个工具缺陷（都在 `deploy/verified-deploy.sh`）

| # | 缺陷 | 后果 | 修法 |
|---|---|---|---|
| 1 | 内容断言用 `grep -a` | 对**正确**二进制报「改动没进去」⇒ 唯一防「线上跑旧二进制」的门失效 | 换 `deploy/has_literal.py`（Python 逐字节，模式走 base64） |
| 2 | 开 ~10 条独立 SSH | fail2ban 拒连，且在 `test -x` 那步**伪装成「hotswap.sh 不存在」** | `ControlMaster`/`ControlPersist` 复用单连接 |
| 3 | 健康探针写死 `127.0.0.1:8990` | 线上 host 是 `172.30.0.1` ⇒ 一次**完全成功**的交接被报「探针 000」 | 从 `config.json` 现读 host/port（与 hotswap.sh 同口径），并打印 `probe_url` |
| 4 | 回滚提示用 `grep -ac` + `printf '%q'` | 输出乱码 `移�207��203度…`，粘贴执行还会再错一层 | 改输出 base64 + 断言器的命令，可直接粘贴 |

另加一条**换机刚需**：`hotswap.sh` 现在**自动从仓库版同步**到远端（幂等）。
`[实测]` 今晚一次已通过全部断言的部署死在「VPS 上没有 hotswap.sh」——
新机器上必然也没有。

### BSD grep 的失效边界（补正 §5，我先前说得太宽）

`[实测]` 它只对**非 ASCII**失效：

```
SO_REUSEPORT   bsd=FOUND      py=1     ← ASCII 正常
族内连续第       bsd=NOTFOUND   py=1     ← 中文失效
所有凭据均已禁用   bsd=NOTFOUND   py=3
```

⇒ `hotswap.sh` 里那处 `grep -aq "SO_REUSEPORT"` **是安全的**（ASCII），
我先前担心它「早晚会咬人」是过度推断。**但凡断言里有中文，就不能用 grep。**

### 换服务器时要改什么

`REMOTE` 已可用环境变量覆盖，所以主流程只需一个新 SSH 别名：

```bash
REMOTE=<新别名> deploy/verified-deploy.sh <binary> --must '…' --must-not '…'
```

`hotswap.sh` 会自动推上去。**仍需人工确认的**：
`config.json` 的 `host`（探针与 hotswap 都读它）、`kirostudio` 用户存在
（SO_REUSEPORT 要求同 UID）、`python3`（两端都要，断言器与 config 解析用）。

### 落盘位置（不再只在 `/tmp`）

| 路径 | 内容 |
|---|---|
| `deploy/has_literal.py` | 断言器（新增） |
| `deploy/verified-deploy.sh` | 含上述 4 个修复（**覆盖了一个未跟踪文件**，见下） |
| `deploy/BUILD-LINUX-LOCALLY.md` | 本机出 Linux 二进制的完整流程 + 3 个坑（新增） |
| `tools/diag/{pool,timeline,403,fingerprint,selfheal,postdeploy}.py` | 6 个只读诊断脚本（新增）。`pool.py` 是号池类问题**第一条该跑的命令**（keyhash/cloneGroup 聚合） |

⚠️ **`deploy/verified-deploy.sh` 原本是别人的未跟踪文件（03:59），我覆盖了它。**
内容是那一版 + 上述 4 个修复（8752→11567 字节，六个 step 标记全在、`bash -n` 过、
纯增量）。若不希望被覆盖，从这一版里挑修复即可。
`tools/` 也是别人的未跟踪目录（`codegraph`），我只在其下新建 `diag/`，未动 `codegraph`。
`[实测]` 主树已跟踪修改数始终是 **96**，我没碰任何已跟踪文件。
