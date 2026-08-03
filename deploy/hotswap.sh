#!/usr/bin/env bash
# 零空窗热交接部署 —— 依赖二进制已开启 SO_REUSEPORT（见 main.rs 的 bind_listener）
#
# 为什么需要它：`systemctl restart kirostudio` 有实测 **20.16 秒**端口空窗。
# 原因是本服务优雅停机（等在途请求含 SSE 长流 drain 完才退出），而 systemd 要等旧进程
# 退出才起新进程，这段时间端口无人监听，入站请求全部连接拒绝（curl 返回 000）。
#
# 本脚本的交接顺序（全程端口始终有人监听，空窗 0ms）：
#   1. 起新实例（SO_REUSEPORT 让它与旧实例同时绑 8990，内核自动分流新连接）
#   2. 健康检查新实例；不通过则杀掉新实例、旧实例继续服务 → 本次升级零影响
#   3. 通过则 SIGTERM 旧实例：它停止接新连接、把在途请求 drain 完自行退出
#   4. 把新实例交给 systemd 接管（改 unit 的 MainPID 不可靠，故用 restart 前置换文件的
#      方式：新实例先以 systemd 托管身份启动，见下方 START_MODE 说明）
#
# 用法（在服务器上跑）：
#   hotswap.sh /tmp/kirostudio.new      # 完整交接
#   hotswap.sh /tmp/kirostudio.new check  # 只做健康检查，不交接
set -uo pipefail

NEW_BIN="${1:?用法: $0 <新二进制路径> [check]}"
MODE="${2:-swap}"

BIN=/opt/kirostudio/bin/kirostudio
CFG=/opt/kirostudio/data/config.json
CREDS=/opt/kirostudio/data/credentials.json
UNIT=kirostudio
# 健康检查用的探测端口：新实例先绑**主端口**（靠 SO_REUSEPORT），所以不能靠端口区分
# 新旧实例。改为按 PID 校验存活 + 打主端口确认整体可用。
HEALTH_TIMEOUT=25

log() { printf '[hotswap] %s\n' "$*"; }
die() { printf '[hotswap] FAIL: %s\n' "$*" >&2; exit 1; }

[[ -f $NEW_BIN ]] || die "$NEW_BIN 不存在"
[[ -f $CFG ]] || die "$CFG 不存在"

PORT=$(python3 -c "import json;print(json.load(open('$CFG')).get('port',8080))")
HOST=$(python3 -c "import json;print(json.load(open('$CFG')).get('host','127.0.0.1'))")
PROBE="http://${HOST}:${PORT}/v1/models"

# ---- 前置：确认新二进制能跑（不占端口，只看 --version 不炸）----
chmod +x "$NEW_BIN"
"$NEW_BIN" --version >/dev/null 2>&1 || die "$NEW_BIN 无法执行（架构不匹配？file 看一下）"

# ---- 前置：确认新二进制含 SO_REUSEPORT 支持 ----
# 没有它就不能同端口并存，交接会因 EADDRINUSE 失败 —— 提前拦住比中途失败安全。
# 用 grep -a 直接搜二进制，不经管道：`strings X | grep -q` 在 set -o pipefail 下会因
# grep 提早退出给 strings 发 SIGPIPE 而把整条判为失败 → 误报「不含该特性」。
if ! grep -aq "SO_REUSEPORT" "$NEW_BIN"; then
  die "新二进制未包含 SO_REUSEPORT（旧版本？）。它无法与运行中实例同端口并存，
       请改用 kirostudio-update 的常规替换流程（有 20s 空窗）或先升级到含该特性的版本。"
fi

OLD_PID=$(systemctl show "$UNIT" -p MainPID --value 2>/dev/null || echo 0)
log "当前实例 PID=$OLD_PID，主端口 ${HOST}:${PORT}"

# ---- 阶段 1：起新实例（与旧实例同端口并存）----
# ⚠️ 必须以**与运行中实例相同的用户**启动。
# Linux 的 SO_REUSEPORT 要求所有共享同一端口的 socket 属于同一 UID（防端口劫持的安全
# 设计）。以 root 起裸进程去和跑在 kirostudio 用户下的 systemd 实例共享端口会被内核
# 拒绝，报 EADDRINUSE —— 看起来像「REUSEPORT 没生效」，实则是 UID 不匹配。
SVC_USER=$(systemctl show "$UNIT" -p User --value 2>/dev/null)
SVC_USER=${SVC_USER:-root}

# ⚠️ 日志必须有大小上限。这个重定向此前是裸 `> /tmp/hotswap-new.log`，即**无界文件**：
# 任何高频日志都会一直写到磁盘满。已经真实发生过一次「日志把磁盘打满、连 bash 都无法创建
# 临时文件」的事故，代价远超它本身的用途（这个文件只在健康检查失败时被 tail -20 看一眼）。
#
# 用 head -c 做硬上限而非 logrotate：这是个**短命**的前台进程（交接完成即由 systemd 接管、
# journald 负责后续日志），引入 logrotate 配置反而是多一处要维护的状态。
# head 到达上限后关闭管道，写端拿到 EPIPE/SIGPIPE 而**不会**阻塞或杀死服务进程
# （Rust 的 tracing 写失败只是丢日志，不 panic）。
LOG=/tmp/hotswap-new.log
LOG_CAP=$((32 * 1024 * 1024))   # 32MiB：足够容纳启动期全部日志，且远小于任何磁盘余量
log "启动新实例（SO_REUSEPORT 同端口并存，以 $SVC_USER 身份，旧实例继续服务）…"
#
# ⚠️ 用**进程替换** `> >(...)` 而不是管道 `| head`：管道会让下面的 `$!` 取到 `head` 的 PID
# 而非服务进程的，`kill -0 $NEW_PID` 的存活检查与后续 SIGTERM 全部打在错误的进程上
# （交接会静默失效 —— 最危险的那种失效）。进程替换保持 `$!` 仍是服务进程。
: > "$LOG"   # 每次交接从空文件开始，避免上一次的内容混淆排障
if [[ $SVC_USER != "$(id -un)" ]]; then
  setpriv --reuid="$SVC_USER" --regid="$SVC_USER" --init-groups \
    "$NEW_BIN" -c "$CFG" --credentials "$CREDS" > >(head -c "$LOG_CAP" > "$LOG") 2>&1 &
else
  "$NEW_BIN" -c "$CFG" --credentials "$CREDS" > >(head -c "$LOG_CAP" > "$LOG") 2>&1 &
fi
NEW_PID=$!

# ---- 孤儿裸实例兜底：任何异常退出都必须收掉阶段 1 起的裸进程 ----
# 它不受 systemd 托管，但**一绑上端口就在真实分流流量**（SO_REUSEPORT 由内核派发）。
# 中途 Ctrl-C / die / 意外退出若不收，它会常驻同端口：持续吃真实请求却不在 systemctl
# 视野内（`systemctl stop` 停不掉它），还会与受管实例**双写** kiro_stats.json
# （last-writer-wins → 用量统计静默错乱）。
#
# GRACEFUL_KILL_ISSUED 用来区分两种退出：
#   - 已刻意发过 SIGTERM（成功交接的阶段 4 / check 模式 / 回滚分支）：什么都不做。
#     那些路径刻意不 wait —— 裸实例可能还在 drain SSE 长流，这是预期行为。
#   - 未发过（异常中断）：先 SIGTERM 给它同样的 drain 机会，等到 main.rs 的
#     drain 上限（8s）再多给 2s 余量，仍在则 SIGKILL 保证不留孤儿。
GRACEFUL_KILL_ISSUED=0
cleanup_bare_instance() {
  [[ ${GRACEFUL_KILL_ISSUED:-0} == 1 ]] && return 0
  local pid=${NEW_PID:-}
  [[ -n $pid ]] || return 0
  kill -0 "$pid" 2>/dev/null || return 0
  log "异常退出：回收临时裸实例 PID=$pid（避免孤儿常驻 ${HOST}:${PORT} 分流真实流量）"
  kill "$pid" 2>/dev/null
  for _ in $(seq 1 10); do
    kill -0 "$pid" 2>/dev/null || return 0
    sleep 1
  done
  log "裸实例 10s 未退出，SIGKILL"
  kill -9 "$pid" 2>/dev/null
}
trap cleanup_bare_instance EXIT
trap 'die "收到中断信号"' INT TERM

sleep 1
kill -0 "$NEW_PID" 2>/dev/null || {
  log "新实例启动即退出，日志尾部："; tail -20 /tmp/hotswap-new.log
  # SO_REUSEPORT 要求**所有**绑同端口的 socket 都设置该选项。若当前运行的旧实例是
  # 不带该特性的版本，新实例即便自己设了也会 EADDRINUSE —— 这是从旧版本迁移时的
  # 一次性过渡问题，不是本次二进制有毛病。此时只能走常规替换（有空窗），
  # 之后的每次升级才能零空窗。
  if grep -q "Address in use" /tmp/hotswap-new.log 2>/dev/null; then
    die "端口被占用且无法共享：当前运行的实例很可能是**不含 SO_REUSEPORT** 的旧版本。
     零空窗交接要求新旧双方都开启该选项，故本次需先做一次常规替换：
       cp -a $BIN ${BIN}.prev && install -o kirostudio -g kirostudio -m 755 $NEW_BIN $BIN && systemctl restart $UNIT
     （该次约有 20s 空窗）。此后运行的实例已含该特性，再用本脚本即可零空窗。"
  fi
  die "新实例无法启动，旧实例未受影响"
}

# ---- 阶段 2：健康检查 ----
log "健康检查（最长 ${HEALTH_TIMEOUT}s）…"
ok=0
for _ in $(seq 1 "$HEALTH_TIMEOUT"); do
  code=$(curl -s -o /dev/null -w "%{http_code}" --max-time 3 "$PROBE" -H "x-api-key: probe" 2>/dev/null || echo 000)
  # 401 = 服务活着且鉴权生效（探测 key 是假的），200 = 放行；两者都算健康
  if [[ $code == 401 || $code == 200 ]]; then ok=1; break; fi
  kill -0 "$NEW_PID" 2>/dev/null || { log "新实例中途退出"; tail -20 /tmp/hotswap-new.log; die "新实例不健康"; }
  sleep 1
done
# ⚠️ 这里刻意**不**设 GRACEFUL_KILL_ISSUED：本实例从未通过健康检查，没有值得保全的
# 在途请求，而它已绑上端口（SO_REUSEPORT 会给它派发真实流量）→ 保证不留孤儿优先于
# 优雅 drain，故让 EXIT trap 接手并在 10s 后升级为 SIGKILL。别"顺手补上"这个 flag。
[[ $ok == 1 ]] || { kill "$NEW_PID" 2>/dev/null; die "健康检查未通过（最后状态 $code），已杀新实例，旧实例继续服务"; }
log "新实例健康（PID=$NEW_PID）"

if [[ $MODE == check ]]; then
  log "check 模式：不交接，杀掉新实例"
  kill "$NEW_PID" 2>/dev/null
  GRACEFUL_KILL_ISSUED=1   # 已优雅收掉，EXIT trap 不再升级为 SIGKILL
  log "完成（旧实例未受任何影响）"
  exit 0
fi

# ---- 阶段 3：换文件 + 让 systemd 接管新版本 ----
# 说明：阶段 1 起的实例是**裸进程**（不受 systemd 托管），只用于验证新二进制在真实
# 配置下健康。真正的交接靠 SO_REUSEPORT：先让 systemd 起一个受管的新实例（此时端口
# 上有 裸新实例 + 旧受管实例 + 新受管实例，内核分流均可用），再杀掉裸实例。
log "备份当前二进制到 ${BIN}.prev 并换入新版本"
cp -a "$BIN" "${BIN}.prev" || die "备份失败，中止（未改动任何东西）"
install -o kirostudio -g kirostudio -m 755 "$NEW_BIN" "$BIN" || die "换入失败"

log "restart systemd 单元（旧受管实例优雅退出期间，裸新实例仍在同端口顶着 → 零空窗）"
systemctl restart "$UNIT"

# 等 systemd 的新实例就绪
ok=0
for _ in $(seq 1 "$HEALTH_TIMEOUT"); do
  code=$(curl -s -o /dev/null -w "%{http_code}" --max-time 3 "$PROBE" -H "x-api-key: probe" 2>/dev/null || echo 000)
  newmain=$(systemctl show "$UNIT" -p MainPID --value 2>/dev/null || echo 0)
  if [[ ( $code == 401 || $code == 200 ) && $newmain != 0 && $newmain != "$OLD_PID" ]]; then ok=1; break; fi
  sleep 1
done

if [[ $ok != 1 ]]; then
  log "systemd 新实例未就绪，回滚二进制并重启"
  # ⚠️ 这里**不能**用 `mv "${BIN}.prev" "$BIN"`：rename 会把回滚点本身消耗掉，
  # 回滚完成后 ${BIN}.prev 不再存在 → 紧接着的 `kirostudio-update rollback`
  # （它也依赖 .prev）无处可回，而这正是"交接刚失败"最需要它的时刻。
  #
  # 也**不能**用 `cp -a`：此刻 systemctl restart 已发出，$BIN 路径上的二进制
  # 正在被执行，往正在执行的可执行文件写会拿到 ETXTBSY（原作者用 mv 换目录项
  # 而非改 inode，正是为了绕开这一点）。
  #
  # 用 install：GNU install 会先 unlink 目标再创建（copy.c 的
  # unlink_dest_before_opening），故既避开 ETXTBSY 又保留 .prev。
  # 它同时设好 owner/mode，原先那行 chown/chmod 因此不再需要。
  install -o kirostudio -g kirostudio -m 755 "${BIN}.prev" "$BIN" \
    || log "⚠️ 回滚换入失败，${BIN}.prev 仍完好，需人工处理"
  systemctl restart "$UNIT"
  kill "$NEW_PID" 2>/dev/null
  GRACEFUL_KILL_ISSUED=1
  die "交接失败，已回滚到旧二进制（回滚点 ${BIN}.prev 仍保留）"
fi

log "systemd 新实例已就绪（MainPID=$(systemctl show "$UNIT" -p MainPID --value)）"

# ---- 阶段 4：撤掉临时裸实例 ----
log "SIGTERM 临时裸实例（它会把自己的在途请求 drain 完再退出）"
kill "$NEW_PID" 2>/dev/null
GRACEFUL_KILL_ISSUED=1   # 已优雅收掉，EXIT trap 不再升级为 SIGKILL
# 不 wait：它可能还在 drain SSE 长流，这是预期行为，不该阻塞脚本。

log "完成。回滚点在 ${BIN}.prev（kirostudio-update rollback 可用）"
