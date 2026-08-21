#!/usr/bin/env bash
# 带内容断言的部署 —— 在 hotswap.sh 之外补一道「线上二进制真的含/不含这次改动」的门。
#
# 为什么需要它(2026-08-07 事故):有人回退了本地代码,但**线上仍跑着回退前的二进制**,
# 于是"一次 429 就把号冷却 1800s"的补丁继续在线上把整池 40 个号在 2 秒内锁死 30 分钟。
# 当时的部署流程校验了 sha256 —— 但那只证明「上传的 == 本地编译的」,**不证明
# 「本地编译的含这次改动」**。同一批操作里还出现过一次 hash 未变(增量构建复用旧产物)
# 却被报告为部署成功。
#
# 所以本脚本要求显式声明断言,并在**三个时点**各验一次:
#   1. 上传前验本地二进制   —— 防"编译没带上改动"(增量构建陷阱)
#   2. hotswap 后验线上文件 —— 防"换的不是这个文件"
#   3. 交接后验运行中进程   —— 防"文件换了但进程还是旧的"
#
# 断言用 `grep -a` 直接搜二进制,**不用 `strings | grep`**:strings 会按不可打印字节切段,
# 把 UTF-8 中文字面量切碎 ⇒ 明明在也搜不到(该误判本身就发生过,导致误判"补丁没上线")。
#
# 用法:
#   deploy/verified-deploy.sh <本地二进制> [--must '字面量'].. [--must-not '字面量'].. [--dry-run]
#
# 例(部署本次回退,断言那个害人的补丁已消失):
#   deploy/verified-deploy.sh target/x86_64-unknown-linux-musl/release/kirostudio \
#       --must-not '命中账号级限流' --must 'SO_REUSEPORT'
set -euo pipefail

REMOTE=${REMOTE:-ws-vps}
REMOTE_BIN=/opt/kirostudio/bin/kirostudio
REMOTE_TMP=/tmp/kirostudio.new
HOTSWAP=/opt/kirostudio/bin/hotswap.sh

# ── SSH 连接复用（承重，不是优化）
#
# 2026-08-07 实测：本脚本一次运行要开 ~10 条独立 SSH（每条断言一条、记录状态、
# 完整性校验、hotswap、交接后验证各一条），跑到「4/5 hotswap 交接」时被
# `Connection closed by <ip>` 打断 —— fail2ban 把短时间内的多次连接判成暴力破解。
# 后果特别坑：那一步是 `test -x $HOTSWAP`，连接被拒会让它**误报成「hotswap.sh
# 不存在」**（文件其实在、sha 也对），把网络层问题伪装成部署前置条件缺失。
#
# ControlMaster 让全部 ssh/scp 复用同一条 TCP 连接 ⇒ fail2ban 只看到 1 次连接。
# CLAUDE.md 记的「多个 scp 用 && 串起来会被 fail2ban 拦」是同一个坑的另一面。
#
# ⚠️ 刻意**不**在这里设 ConnectTimeout：OpenSSH 对重复选项取**第一个**，
# 而脚本内联的 `-o ConnectTimeout=180`（hotswap 那步）必须生效。
SSH_CTL=/tmp/cc-kg-ssh-%C
SSH_OPTS=(-o ControlMaster=auto -o "ControlPath=$SSH_CTL" -o ControlPersist=120)
ssh()  { command ssh "${SSH_OPTS[@]}" "$@"; }
scp()  { command scp "${SSH_OPTS[@]}" "$@"; }

# 内容断言用 Python 逐字节比对，**不用 grep**。
#
# 2026-08-07 实测：macOS 的 BSD grep(2.6.0-FreeBSD) 在二进制里**找不到** UTF-8 中文
# 字面量 —— 默认 BRE / -F / -f 从文件读 pattern，直接 argv 与经 bash、zsh 共 9 种
# 组合全部 NOT FOUND，而该字面量确实存在（55 字节、offset 14344995、count=1，
# GNU grep 3.12 能找到）。即断言结果取决于 PATH 里先命中哪个 grep ⇒ 这道门会对
# **正确的**二进制报「改动没进去」，把唯一能拦住「回退了但线上还跑旧二进制」的
# 防线变成噪声。CLAUDE.md 记的是 `strings|grep` 切碎 UTF-8，这里是同类问题的另一面。
#
# 模式经 base64 传递：绕开 shell/ssh 的引用层，多字节序列不可能被改写。
LOCAL_HAS_LITERAL=${LOCAL_HAS_LITERAL:-$(cd "$(dirname "$0")" && pwd)/has_literal.py}
REMOTE_HAS_LITERAL=/tmp/has_literal_deploy.py

LOCAL_BIN=""
MUST=()
MUST_NOT=()
DRY_RUN=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --must)     MUST+=("$2"); shift 2 ;;
    --must-not) MUST_NOT+=("$2"); shift 2 ;;
    --dry-run)  DRY_RUN=1; shift ;;
    -*)         printf 'unknown flag: %s\n' "$1" >&2; exit 2 ;;
    *)          LOCAL_BIN="$1"; shift ;;
  esac
done

log()  { printf '[deploy] %s\n' "$*"; }
die()  { printf '[deploy] FAIL: %s\n' "$*" >&2; exit 1; }
ok()   { printf '[deploy]   ✓ %s\n' "$*"; }

[[ -n $LOCAL_BIN ]] || die "用法: $0 <本地二进制> [--must STR].. [--must-not STR].. [--dry-run]"
[[ -f $LOCAL_BIN ]] || die "$LOCAL_BIN 不存在"
(( ${#MUST[@]} + ${#MUST_NOT[@]} > 0 )) || die "必须至少给一条 --must / --must-not 断言。
     没有断言的部署无法回答『这次改动真的上线了吗』—— 那正是本脚本存在的原因。"

# ── 断言器:对一个文件跑全部断言。$1=位置描述 $2=文件路径 $3=是否远端(1/0)
assert_file() {
  local where="$1" path="$2" remote="${3:-0}" pat rc b64
  for pat in ${MUST[@]+"${MUST[@]}"}; do
    b64=$(printf '%s' "$pat" | base64 | tr -d '\n')
    if (( remote )); then
      ssh -o ConnectTimeout=10 "$REMOTE" "python3 $REMOTE_HAS_LITERAL $(printf '%q' "$path") $b64" && rc=0 || rc=$?
    else
      python3 "$LOCAL_HAS_LITERAL" "$path" "$b64" && rc=0 || rc=$?
    fi
    (( rc == 0 )) || die "$where 缺少必须存在的字面量: $pat
     ⇒ 这次改动**没有**进到该二进制。若是本地文件,极可能是增量构建复用了旧产物:
        cargo clean -p kirostudio 后重新构建。"
    ok "$where 含 '$pat'"
  done
  for pat in ${MUST_NOT[@]+"${MUST_NOT[@]}"}; do
    b64=$(printf '%s' "$pat" | base64 | tr -d '\n')
    if (( remote )); then
      ssh -o ConnectTimeout=10 "$REMOTE" "python3 $REMOTE_HAS_LITERAL $(printf '%q' "$path") $b64" && rc=0 || rc=1
    else
      python3 "$LOCAL_HAS_LITERAL" "$path" "$b64" && rc=0 || rc=1
    fi
    (( rc == 1 )) || die "$where 仍含应当消失的字面量: $pat
     ⇒ 回退/修复**没有**进到该二进制。别继续部署,先确认改的是哪份源码。"
    ok "$where 不含 '$pat'"
  done
}

# ── 0. 架构与可执行性
log "0/5 本地二进制体检"
[[ -f $LOCAL_HAS_LITERAL ]] || die "缺少断言器 $LOCAL_HAS_LITERAL（与本脚本同目录的 has_literal.py）"
ok "断言器 $(basename "$LOCAL_HAS_LITERAL")（Python 逐字节，不用 grep）"
file "$LOCAL_BIN" | grep -q 'ELF 64-bit.*x86-64' \
  || die "$LOCAL_BIN 不是 x86-64 ELF(VPS 是 x86_64;交叉编译目标写对了吗)
     实际: $(file -b "$LOCAL_BIN")"
ok "$(file -b "$LOCAL_BIN" | cut -c1-60)"
LOCAL_SHA=$(shasum -a 256 "$LOCAL_BIN" | awk '{print $1}')
ok "sha256 ${LOCAL_SHA:0:16}…"

# ── 1. 上传前:验本地二进制含这次改动(增量构建陷阱在这里被拦住)
log "1/5 断言本地二进制"
assert_file "本地 $LOCAL_BIN" "$LOCAL_BIN" 0

# ── 2. 记录部署前的线上状态(用于事后对比与回滚判断)
log "2/5 记录线上现状"
PRE=$(ssh -o ConnectTimeout=10 "$REMOTE" "
  printf 'sha=%s\n' \"\$(sha256sum $REMOTE_BIN | cut -d' ' -f1)\"
  printf 'pid=%s\n' \"\$(systemctl show kirostudio -p MainPID --value)\"
  printf 'nrestarts=%s\n' \"\$(systemctl show kirostudio -p NRestarts --value)\"
  printf 'active=%s\n' \"\$(systemctl is-active kirostudio)\"
") || die "无法读取线上状态(SSH 不通?)"
printf '%s\n' "$PRE" | sed 's/^/[deploy]   /'
PRE_SHA=$(printf '%s\n' "$PRE" | sed -n 's/^sha=//p')
[[ $PRE_SHA == "$LOCAL_SHA" ]] && die "线上 sha256 与本地**完全相同**($LOCAL_SHA)。
     要么已经部署过这一版,要么本地构建没带上改动。不做无意义的交接。"

if (( DRY_RUN )); then
  log "--dry-run:到此为止,未上传、未交接"
  exit 0
fi

# ── 3. 上传 + 远端 sha 复核 + 远端断言
log "3/5 上传并在远端复核"
# 断言器先上去：远端三处 assert_file 都要用它（scp 而非 `ssh 'cat > f'` ——
# CLAUDE.md 实测后者静默不写入）。
scp -q "$LOCAL_HAS_LITERAL" "$REMOTE:$REMOTE_HAS_LITERAL" || die "断言器上传失败"
ssh -o ConnectTimeout=10 "$REMOTE" "test -s $REMOTE_HAS_LITERAL" \
  || die "断言器上传后在远端为空/不存在"
ok "断言器已就位 $REMOTE_HAS_LITERAL"
scp -q "$LOCAL_BIN" "$REMOTE:$REMOTE_TMP" || die "scp 失败"
REMOTE_SHA=$(ssh -o ConnectTimeout=10 "$REMOTE" "sha256sum $REMOTE_TMP | cut -d' ' -f1")
[[ $REMOTE_SHA == "$LOCAL_SHA" ]] || die "上传后 sha256 不一致:本地 $LOCAL_SHA / 远端 $REMOTE_SHA"
ok "传输完整性 OK"
assert_file "远端 $REMOTE_TMP" "$REMOTE_TMP" 1

# ── 4. 交接(用仓库版 hotswap.sh:零空窗 SO_REUSEPORT,健康检查失败自动保旧实例)
log "4/5 hotswap 交接"
# hotswap.sh 从**仓库版**同步过去（幂等）。
#
# 为什么自动同步而不是报错让人手动推：2026-08-07 实测，VPS 上原本就**没有**这个文件，
# 于是一次已经通过全部断言的部署死在这一步。而它是仓库里现成的、且必须与本脚本同版本
# （hotswap 的健康检查/回滚语义与这里的交接后验证是配套的）。
# 换服务器时这一步是刚需：新机器上必然没有它。
#
# 仍坚持装到 $HOTSWAP（/opt/.../bin/）而不是 /tmp：原注释的理由成立 ——
# 临时路径无版本管理，曾出现与仓库版分叉。
LOCAL_HOTSWAP=${LOCAL_HOTSWAP:-$(cd "$(dirname "$0")" && pwd)/hotswap.sh}
if [[ -f $LOCAL_HOTSWAP ]]; then
  scp -q "$LOCAL_HOTSWAP" "$REMOTE:$HOTSWAP" || die "hotswap.sh 上传失败"
  ssh "$REMOTE" "chmod +x $HOTSWAP" || die "hotswap.sh chmod 失败"
  ok "hotswap.sh 已与仓库版同步（$(shasum -a 256 "$LOCAL_HOTSWAP" | cut -c1-16)…）"
else
  log "  ⚠️ 本地没有 $LOCAL_HOTSWAP，跳过同步，改为校验远端已有的"
fi
ssh -o ConnectTimeout=10 "$REMOTE" "test -x $HOTSWAP" \
  || die "$HOTSWAP 不存在或不可执行，且本地无 $LOCAL_HOTSWAP 可推。
     ⚠️ 不要用 /tmp/hotswap.sh —— 临时路径无版本管理,曾出现与仓库版分叉。
     ⚠️ 也要排除 SSH 被 fail2ban 拒连：本步骤的 test -x 在连接被拒时会**伪装成
        「文件不存在」**（2026-08-07 实测踩过）。先手动 ssh 一次确认能连。"
ssh -o ConnectTimeout=180 "$REMOTE" "$HOTSWAP $REMOTE_TMP" || die "hotswap 失败。
     该脚本在健康检查不通过时会杀掉新实例并保留旧实例 ⇒ 线上应仍在服务旧版本。
     用下面这条确认,再决定是否重试:
       ssh $REMOTE 'systemctl is-active kirostudio; tail -30 /tmp/hotswap-new.log'"

# ── 5. 交接后:验线上文件 + 验运行中进程(文件换了≠进程换了)
log "5/5 交接后验证"
assert_file "线上 $REMOTE_BIN" "$REMOTE_BIN" 1

POST=$(ssh -o ConnectTimeout=15 "$REMOTE" "
  printf 'sha=%s\n' \"\$(sha256sum $REMOTE_BIN | cut -d' ' -f1)\"
  printf 'pid=%s\n' \"\$(systemctl show kirostudio -p MainPID --value)\"
  printf 'nrestarts=%s\n' \"\$(systemctl show kirostudio -p NRestarts --value)\"
  printf 'active=%s\n' \"\$(systemctl is-active kirostudio)\"
  printf 'prev_exists=%s\n' \"\$(test -s ${REMOTE_BIN}.prev && echo yes || echo NO)\"
  # ⚠️ host/port 必须从 config.json 现读，**不能写死 127.0.0.1:8990**。
  # 2026-08-07 实测：线上 host 是 172.30.0.1（内网网关地址），服务只 bind 那个地址
  # ⇒ 打 127.0.0.1 得 000，于是这道门在一次**完全成功**的交接后报「健康探针 000」。
  # hotswap.sh 自己是从 config 读 host 的（所以它的健康检查过了），两处口径不一致。
  # 换服务器后 host 大概率又不一样，写死必然再错一次。
  H=\$(python3 -c \"import json;print(json.load(open('/opt/kirostudio/data/config.json')).get('host','127.0.0.1'))\")
  P=\$(python3 -c \"import json;print(json.load(open('/opt/kirostudio/data/config.json')).get('port',8080))\")
  printf 'probe_url=http://%s:%s/v1/models\n' \"\$H\" \"\$P\"
  printf 'probe=%s\n' \"\$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 -H 'x-api-key: probe' \"http://\$H:\$P/v1/models\")\"
")
printf '%s\n' "$POST" | sed 's/^/[deploy]   /'

POST_SHA=$(printf '%s\n' "$POST" | sed -n 's/^sha=//p')
POST_PID=$(printf '%s\n' "$POST" | sed -n 's/^pid=//p')
PRE_PID=$(printf '%s\n' "$PRE" | sed -n 's/^pid=//p')
PROBE=$(printf '%s\n' "$POST" | sed -n 's/^probe=//p')

[[ $POST_SHA == "$LOCAL_SHA" ]] || die "线上文件 sha 仍是旧值 ⇒ 替换没生效"
[[ $POST_PID != "$PRE_PID" ]]   || die "MainPID 未变($PRE_PID)⇒ 文件换了但**进程还是旧的**。
     这正是『改了没上线』最隐蔽的形态:sha 对、断言过、行为却没变。"
[[ $PROBE == 401 || $PROBE == 200 ]] || die "健康探针返回 $PROBE(期望 401/200)"

# 运行中进程的可执行映像 —— 最硬的一道:直接验内核眼里那个进程加载的是什么
log "验证运行中进程的映像"
RUN_SHA=$(ssh -o ConnectTimeout=10 "$REMOTE" "sha256sum /proc/$POST_PID/exe 2>/dev/null | cut -d' ' -f1")
if [[ -n $RUN_SHA ]]; then
  [[ $RUN_SHA == "$LOCAL_SHA" ]] || die "运行中进程(PID $POST_PID)的映像 sha=$RUN_SHA
     ≠ 本次部署的 $LOCAL_SHA ⇒ **进程跑的不是这个二进制**。"
  ok "运行中进程映像 == 本次部署的二进制"
else
  log "  ! 无法读 /proc/$POST_PID/exe,跳过该项(不视为失败)"
fi

printf '\n[deploy] ✅ 部署完成并已验证\n'
printf '[deploy]   PID  %s → %s\n' "$PRE_PID" "$POST_PID"
printf '[deploy]   sha  %s… → %s…\n' "${PRE_SHA:0:12}" "${POST_SHA:0:12}"
printf '[deploy]   回滚 ssh %s '"'"'kirostudio-update rollback'"'"'\n' "$REMOTE"
printf '[deploy]   ⚠️ 回滚前先确认 %s.prev 不含你刚修掉的东西:\n' "$REMOTE_BIN"
# ⚠️ 这里给的命令**不能用 grep** —— 两个独立原因，实测都踩过：
#   ① BSD grep 在二进制里找不到 UTF-8 中文（本脚本断言器换成 Python 的同一个理由）；
#   ② `printf '%q'` 对中文会转义成 $'\346\212...' 字节序列，打印出来是乱码
#      （实测输出 `移�207��203度以...`），复制粘贴执行还会因转义层再错一次。
# 改为给 base64 + 断言器的命令：与本脚本三处断言完全同一套口径，可直接粘贴执行。
for pat in ${MUST_NOT[@]+"${MUST_NOT[@]}"}; do
  printf '[deploy]      ssh %s "python3 %s %s.prev %s"   # 期望 rc=1(不含)\n' \
    "$REMOTE" "$REMOTE_HAS_LITERAL" "$REMOTE_BIN" \
    "$(printf '%s' "$pat" | base64 | tr -d '\n')"
done
