#!/bin/bash
# KiroStudio 首次安装 systemd 服务 —— 幂等，可重复执行
# 用法：bash install-service.sh
set -euo pipefail

SVC="kirostudio"
USER_NAME="dwgx_user"
WORKDIR="/home/dwgx_user/KiroStudio"
BIN="$WORKDIR/kirostudio"
UNIT="/etc/systemd/system/${SVC}.service"

echo "=== [1/4] 前置检查 ==="
[ -x "$BIN" ] || echo "  警告：$BIN 尚不存在或不可执行(可先跑 deploy.sh 拉二进制)"
[ -f "$WORKDIR/config/config.json" ] || echo "  警告：$WORKDIR/config/config.json 不存在"
[ -f "$WORKDIR/config/credentials.json" ] || echo "  警告：$WORKDIR/config/credentials.json 不存在"

echo "=== [2/4] 写 systemd 单元 $UNIT ==="
# 相对路径基于 WorkingDirectory；ExecStart 用绝对二进制 + 相对 config
sudo tee "$UNIT" >/dev/null <<EOF
[Unit]
Description=KiroStudio (Anthropic <-> Kiro API 网关)
After=network-online.target
Wants=network-online.target
# 二级止损：60s 内启动超 10 次则 systemd 暂停自动重启（回滚决策仍以守卫的文件计数为准）。
StartLimitIntervalSec=60
StartLimitBurst=10

[Service]
Type=simple
User=$USER_NAME
Group=$USER_NAME
WorkingDirectory=$WORKDIR
# OTA crashloop 回滚守卫：每次启动前跑，检测新版启动即崩则自动回滚 .bak 旧版（阶段A）。
# 以 - 前缀声明「失败不阻断启动」（守卫内部也 fail-safe exit 0，双保险）。
ExecStartPre=-$WORKDIR/deploy/rollback-guard.sh
ExecStart=$BIN -c config/config.json --credentials config/credentials.json
Restart=always
RestartSec=3

[Install]
WantedBy=multi-user.target
EOF
echo "  已写入单元文件"

echo "=== [3/4] 重载 systemd 并开机自启 ==="
sudo systemctl daemon-reload
sudo systemctl enable "$SVC"
echo "  已 daemon-reload + enable"

echo "=== [4/4] 完成 ==="
echo "  单元已安装。首次启动请执行： sudo systemctl start $SVC"
echo "  查看状态： systemctl status $SVC"
echo "  后续升级用： bash deploy/deploy.sh"
