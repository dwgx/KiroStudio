#!/bin/bash
# KiroStudio OTA crashloop 回滚守卫（阶段A）
#
# 由 systemd `ExecStartPre=` 在每次 ExecStart **之前**运行（User=dwgx_user，非 root）。
# 与进程侧 `common::health_marker` 配合，构成「新版启动即崩 → 自动回滚 .bak 旧版」闭环。
# 回滚决策放这里（systemd 层），绝不放可能已崩的进程自己——见 RESEARCH-HOTRELOAD-ARCH-0708 §3.2 方案A。
#
# 机制：
#   - `.boot_attempts` 计数器：本脚本每次启动前 +1；进程 bind 成功后清零（health_marker::clear_boot_attempts）。
#     只有「连 bind 都到不了就崩」的启动才会让计数跨重启累积，从而与「健康后被正常一键重启」区分开。
#   - 判定 crashloop：`.bak` 存在（有可回滚的旧版）且计数 >= 阈值 → 认定新版启动即崩。
#   - 回滚：把坏版 kirostudio 改名 kirostudio.failed.$TS 留证，用 .bak 覆盖回旧版，删 .bak，清零计数。
#     本次 ExecStart 随即拉起回滚后的旧版。
#
# 安全：只在 WORKDIR 内做 cp/mv/rm，不调 systemctl（NoNewPrivileges=true 禁提权）。
# 幂等 + fail-safe：任何异常（缺 .bak / 首次部署 / 读计数失败）一律放行启动，绝不因守卫本身挡住服务。
set -uo pipefail

WORKDIR="${KIRO_WORKDIR:-/home/dwgx_user/KiroStudio}"
BIN="$WORKDIR/kirostudio"
BAK="$WORKDIR/kirostudio.bak"
COUNTER="$WORKDIR/kirostudio.boot_attempts"
# 连续启动阶段崩溃达到此次数即回滚（RestartSec=3 下约 10s 内攒够，足够快止损）。
THRESHOLD="${KIRO_ROLLBACK_THRESHOLD:-3}"

log() { echo "[rollback-guard] $*"; }

# 读当前计数（缺失/非法按 0）
attempts=0
if [ -f "$COUNTER" ]; then
    read -r attempts < "$COUNTER" 2>/dev/null || attempts=0
    case "$attempts" in
        ''|*[!0-9]*) attempts=0 ;;
    esac
fi

# 本次启动前 +1
attempts=$((attempts + 1))
echo "$attempts" > "$COUNTER" 2>/dev/null || log "警告：无法写计数器 $COUNTER（继续放行）"
log "启动尝试计数=$attempts（阈值=$THRESHOLD）"

# 判定 crashloop：有回滚点 且 计数达阈值
if [ -f "$BAK" ] && [ "$attempts" -ge "$THRESHOLD" ]; then
    TS=$(date +%Y%m%d-%H%M%S)
    FAILED="$WORKDIR/kirostudio.failed.$TS"
    log "检测到 crashloop（新版连续 $attempts 次启动阶段崩溃），开始回滚到 .bak 旧版"

    # 坏版留证（便于事后排查），失败不阻断回滚主流程
    if [ -f "$BIN" ]; then
        mv -f "$BIN" "$FAILED" 2>/dev/null && log "坏版已留证：$FAILED" || log "警告：坏版留证失败（继续回滚）"
    fi

    # 用 .bak 覆盖回旧版
    if cp -f "$BAK" "$BIN" 2>/dev/null && chmod 755 "$BIN" 2>/dev/null; then
        rm -f "$BAK" 2>/dev/null          # 删回滚点，防 ping-pong
        echo "0" > "$COUNTER" 2>/dev/null # 清零，给回滚后旧版干净起点
        log "回滚完成：已用 .bak 覆盖 kirostudio，本次 ExecStart 将拉起旧版"
    else
        # 回滚失败：把留证的坏版还原回去（至少让服务能按原样起，别两头空）
        log "错误：回滚 cp 失败，尝试还原坏版以免服务缺二进制"
        [ -f "$FAILED" ] && mv -f "$FAILED" "$BIN" 2>/dev/null
    fi
fi

# 守卫永远以 0 退出：绝不因自身逻辑挡住 ExecStart（fail-safe）。
exit 0
