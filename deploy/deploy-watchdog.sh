#!/usr/bin/env bash
# 上线后行为守卫 —— 劣化即自动回滚。
#
# 为什么需要它：`hotswap.sh` 的自动回滚只覆盖「新二进制起不来 / 健康检查不过」。
# 但真正危险的是**起来了却行为劣化**（错误率飙升、panic、选号退化）——那时端口有人监听、
# /v1/models 返 200，hotswap 认为成功，而客户端在大面积失败。
#
# 本脚本在交接后持续对比「基线」与「当前」，超阈值即调 hotswap 回滚到指定回滚点。
#
# 用法:
#   deploy-watchdog.sh <回滚点绝对路径> [观测分钟数] [成功率下降容忍百分点]
# 例:
#   deploy-watchdog.sh /opt/kirostudio/bin/kirostudio.rollback-pre0746b 15 10
#
# 判据（任一命中即回滚）：
#   1. panic 计数 > 0                —— 零容忍，panic 意味着有代码路径必然崩
#   2. 进程不在 / 端口无人监听        —— 已经死了
#   3. **网关侧**成功率相对基线下降 > 容忍值
#
# ⚠️ 判据 3 为什么不能用「总成功率」（第一版就是这么写的，实测会误回滚）：
#
# 总成功率被**上游号健康度**支配，与本次部署无关。实测同一天内它在 59%~99% 之间摆动：
# 403 `temporarily is suspended` 风控窗口一来（当天两次，各约 10 分钟、928/516 条），
# 成功率立刻腰斩。若以窗口外的 99% 作基线、容忍 10pp，下一个风控窗口必然触发回滚 ——
# 而那是上游在限流，回滚我们自己的二进制既无意义，还要多付一次重启的代价
# （当前关机路径有缺陷：8s sleep 吃掉 systemd 的 10s TimeoutStopSec → 每次重启都是 SIGKILL）。
#
# 故判据 3 只统计**网关自身**能负责的部分：把上游归因的 outcome
# （`auth_failed` = 403 风控/401、`rate_limited` = 上游 429）从分母里剔除。
# 剩下的分母是「拿到了号、上游也没限流」的请求 —— 它们失败才是网关的问题。
set -uo pipefail

ROLLBACK_BIN="${1:?用法: $0 <回滚点路径> [观测分钟] [容忍百分点] [显式基线%]}"
WATCH_MINUTES="${2:-15}"
DROP_TOLERANCE="${3:-10}"
# 第 4 个参数：**部署前**实测的网关侧成功率（百分数整数）。
#
# 为什么要能显式传：本脚本通常在交接**之后**启动，此时从 traces 采样"过去 20 分钟"
# 必然已混入新版本的数据 —— 用被污染的基线去判断新版本，等于拿新版本和自己比。
# 传入部署前实测值才是干净对照。留空则退化为"启动时采样"（可接受但偏乐观）。
EXPLICIT_BASELINE="${4:-}"

DB=/opt/kirostudio/data/usage/traces.db
UNIT=kirostudio
# 样本下限：低于此数不做成功率判定。20 条以下的比例噪声太大，
# 会在低流量时段把正常波动误判成劣化并触发不必要的回滚。
MIN_SAMPLES=20
# 采样间隔。取 60s：既能在 1~2 个窗口内发现劣化，又不至于让 sqlite 查询本身成为负载。
INTERVAL=60
# 成功率判据需**连续**命中多少轮才回滚。
#
# ⚠️ 这条是实测教训（第一次真实触发就是误判）：单个 60s 窗口的成功率噪声极大。
# 那次触发时第 19 轮 92%（正常），第 20 轮 58% → 回滚。事后查那 21 个失败是
# 14 个 bad_request（客户端请求错误）+ 7 个 server_error，而被回滚的那一版
# **只改了 copies 去重语义**，根本不碰请求路径 —— 即噪声被当成了回归。
#
# 取 3：连续 3 分钟持续劣化才动手。真回归（比如某代码路径必然失败）会持续存在，
# 而客户端突发错误 / 号池瞬时空窗都是分钟级的。代价是发现回归晚 2 分钟，
# 换来的是不再因噪声把无辜版本换掉（误回滚本身也要付一次重启）。
CONSECUTIVE_BREACHES=3

log() { printf '[watchdog] %s %s\n' "$(date '+%H:%M:%S')" "$*"; }

[[ -x $ROLLBACK_BIN ]] || { log "FAIL: 回滚点 $ROLLBACK_BIN 不存在或不可执行，拒绝启动守卫"; exit 1; }

# ---- 取窗口内**网关侧**成功率。输出 "分母 成功数"，查询失败输出 "0 0"。
#
# 分母剔除**不由网关版本负责**的三类 outcome：
#   auth_failed   —— 上游 403 账户级风控 / 401 凭据失效，上游在惩罚账号
#   rate_limited  —— 上游 429，上游在限速
#   bad_request   —— **客户端**请求错误（格式非法 / 超上游体积上限）。
#                    实测教训：第一次真实触发回滚时，21 个失败里 14 个是它，
#                    而被回滚的那版只改了 copies 去重语义、根本不碰请求路径。
#                    客户端发一批坏请求不该导致我们换二进制。
#
# 保留在分母里：success / server_error / network_error / other_error
#   —— 这些要么是网关自己的问题，要么是网关该正确分类却落到兜底的。
sample() {
  local secs="$1"
  sqlite3 "$DB" \
    "select count(*), sum(outcome='success') from traces
     where ts_ms > (strftime('%s','now') - $secs) * 1000
       and outcome not in ('auth_failed','rate_limited','bad_request');" 2>/dev/null \
    | awk -F'|' '{printf "%d %d", ($1==""?0:$1), ($2==""?0:$2)}'
}

# ---- 基线 ----
if [[ -n $EXPLICIT_BASELINE ]]; then
  BASE_PCT="$EXPLICIT_BASELINE"
  log "基线（显式传入，部署前实测）：${BASE_PCT}%（容忍下降 ${DROP_TOLERANCE} 个百分点）"
else
  read -r BASE_N BASE_OK <<<"$(sample 1200)"
  if (( BASE_N < MIN_SAMPLES )); then
    log "基线样本不足（$BASE_N < $MIN_SAMPLES），成功率判据禁用，只保留 panic 与存活判据"
    BASE_PCT=-1
  else
    BASE_PCT=$(( BASE_OK * 100 / BASE_N ))
    log "基线（启动时采样，⚠️ 可能已含新版本数据）：$BASE_OK/$BASE_N = ${BASE_PCT}%"
  fi
fi

do_rollback() {
  local reason="$1"
  log "🔴 判定劣化：$reason"
  log "执行回滚 → $ROLLBACK_BIN"
  if /tmp/hotswap.sh "$ROLLBACK_BIN"; then
    log "回滚完成。当前版本：$(/opt/kirostudio/bin/kirostudio --version 2>&1)"
  else
    # hotswap 自身失败时兜底：直接换文件 + restart。
    # 用 install 而非 cp：install 会先 unlink 目标，避免对正在执行的二进制写入报 ETXTBSY。
    log "hotswap 回滚失败，改用直接换入 + restart"
    install -o kirostudio -g kirostudio -m 755 "$ROLLBACK_BIN" /opt/kirostudio/bin/kirostudio \
      && systemctl restart "$UNIT" \
      && log "兜底回滚完成" \
      || log "🔴 兜底回滚也失败，需人工介入"
  fi
  exit 2
}

log "守卫启动：观测 ${WATCH_MINUTES} 分钟，每 ${INTERVAL}s 采样一次，成功率需连续 ${CONSECUTIVE_BREACHES} 轮劣化才回滚"
ROUNDS=$(( WATCH_MINUTES * 60 / INTERVAL ))
# 连续劣化计数。任何一轮达标即清零 —— 只有**持续**劣化才判回归。
breaches=0

for (( r = 1; r <= ROUNDS; r++ )); do
  sleep "$INTERVAL"

  # 判据 3：还活着吗（最便宜也最致命，先查）
  MAIN=$(systemctl show "$UNIT" -p MainPID --value 2>/dev/null || echo 0)
  if [[ $MAIN == 0 ]] || ! kill -0 "$MAIN" 2>/dev/null; then
    do_rollback "服务进程不存在（MainPID=$MAIN）"
  fi
  if ! ss -ltn 2>/dev/null | grep -q ':8990'; then
    do_rollback "8990 端口无人监听"
  fi

  # 判据 1：panic 零容忍
  PANICS=$(journalctl -u "$UNIT" --since "${INTERVAL} seconds ago" --no-pager 2>/dev/null \
    | grep -c 'panicked' || true)
  if (( PANICS > 0 )); then
    do_rollback "本轮出现 $PANICS 次 panic"
  fi

  # 判据 2：成功率
  read -r N OK <<<"$(sample "$INTERVAL")"
  if (( BASE_PCT >= 0 && N >= MIN_SAMPLES )); then
    PCT=$(( OK * 100 / N ))
    DROP=$(( BASE_PCT - PCT ))
    if (( DROP > DROP_TOLERANCE )); then
      breaches=$(( breaches + 1 ))
      log "[$r/$ROUNDS] ⚠️ 成功率 ${PCT}%（$OK/$N）基线 ${BASE_PCT}% 差 ${DROP}pp —— 连续劣化 ${breaches}/${CONSECUTIVE_BREACHES}"
      if (( breaches >= CONSECUTIVE_BREACHES )); then
        do_rollback "成功率连续 ${breaches} 轮低于基线超过 ${DROP_TOLERANCE}pp（最近一轮 ${PCT}% vs 基线 ${BASE_PCT}%）"
      fi
    else
      # 达标即清零：只有**持续**劣化才算回归，单轮噪声不累计。
      if (( breaches > 0 )); then
        log "[$r/$ROUNDS] 成功率 ${PCT}%（$OK/$N）已回到容忍内，连续劣化计数清零"
      else
        log "[$r/$ROUNDS] 成功率 ${PCT}%（$OK/$N）基线 ${BASE_PCT}% 差 ${DROP}pp"
      fi
      breaches=0
    fi
  else
    log "[$r/$ROUNDS] 样本 $N（不足 $MIN_SAMPLES 或基线缺失），仅存活/panic 判据"
  fi
done

log "✅ 观测期结束，未触发回滚。当前版本：$(/opt/kirostudio/bin/kirostudio --version 2>&1)"
