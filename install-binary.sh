#!/usr/bin/env bash
# ============================================================================
# KiroStudio 预编译二进制一键部署（无需 Docker / Rust / Node）
#
#   bash install-binary.sh
#
# 功能：检测架构 -> 从 GitHub Release 下载预编译二进制 + sha256 并校验 ->
#       交互式设置端口与密钥(可自动生成) -> 生成 config -> 安装 systemd 服务(Linux,
#       用当前用户与当前目录,不写死任何人的路径) -> 启动。已有配置不覆盖(幂等)。
#
# 环境变量(CI / 无人值守)：
#   KIROSTUDIO_PORT      监听端口(默认 8990)
#   KIROSTUDIO_API_KEY   客户端密钥(留空自动生成)
#   KIROSTUDIO_ADMIN_KEY 管理面板密钥(留空自动生成)
#   KIROSTUDIO_YES=1     非交互,全用默认/自动生成
#   KIROSTUDIO_NO_SYSTEMD=1  跳过 systemd 安装,只下载+配置(前台/自行托管)
#   KIROSTUDIO_VERSION   指定版本 tag(默认 latest)
# ============================================================================
set -euo pipefail

REPO="dwgx/KiroStudio"
ASSET="kirostudio-linux-x86_64"

if [ -t 1 ]; then
  C_G='\033[0;32m'; C_Y='\033[1;33m'; C_R='\033[0;31m'; C_B='\033[0;36m'; C_N='\033[0m'
else C_G=''; C_Y=''; C_R=''; C_B=''; C_N=''; fi
info()  { printf "${C_G}[✓]${C_N} %s\n" "$1"; }
warn()  { printf "${C_Y}[!]${C_N} %s\n" "$1"; }
err()   { printf "${C_R}[✗]${C_N} %s\n" "$1" >&2; }
step()  { printf "\n${C_B}==>${C_N} %s\n" "$1"; }

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"
CONFIG_DIR="$SCRIPT_DIR/config"
CONFIG_FILE="$CONFIG_DIR/config.json"
CRED_FILE="$CONFIG_DIR/credentials.json"
BIN="$SCRIPT_DIR/kirostudio"
YES="${KIROSTUDIO_YES:-0}"

# ---- 1. 平台/架构检查 ----
step "环境检查"
OS="$(uname -s 2>/dev/null || echo unknown)"
ARCH="$(uname -m 2>/dev/null || echo unknown)"
if [ "$OS" != "Linux" ]; then
  err "预编译二进制仅提供 Linux x86_64。当前 $OS/$ARCH —— 请用 Docker(bash install.sh)或从源码构建。"
  exit 1
fi
if [ "$ARCH" != "x86_64" ] && [ "$ARCH" != "amd64" ]; then
  err "预编译二进制仅 x86_64,当前 $ARCH —— 请从源码构建或用 Docker。"
  exit 1
fi
for c in curl sha256sum; do
  command -v "$c" >/dev/null 2>&1 || { err "缺少命令: $c,请先安装。"; exit 1; }
done
info "Linux x86_64,依赖就绪"

# ---- 2. 下载二进制 + sha256(哈希强制 github 直连,与二进制解耦,防镜像投毒)----
step "下载预编译二进制"
VER="${KIROSTUDIO_VERSION:-latest}"
if [ "$VER" = "latest" ]; then
  BASE="https://github.com/$REPO/releases/latest/download"
else
  BASE="https://github.com/$REPO/releases/download/$VER"
fi
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
info "下载 $ASSET …"
curl -fSL --retry 3 -o "$TMP/$ASSET" "$BASE/$ASSET" || { err "下载二进制失败"; exit 1; }
info "下载 $ASSET.sha256(校验和)…"
curl -fSL --retry 3 -o "$TMP/$ASSET.sha256" "$BASE/$ASSET.sha256" || { err "下载校验和失败"; exit 1; }

step "校验完整性(sha256)"
EXPECTED="$(awk '{print $1}' "$TMP/$ASSET.sha256" | head -1)"
ACTUAL="$(sha256sum "$TMP/$ASSET" | awk '{print $1}')"
if [ -z "$EXPECTED" ] || [ "${#EXPECTED}" -ne 64 ]; then
  err "校验和文件格式异常,拒绝安装。"; exit 1
fi
if [ "$EXPECTED" != "$ACTUAL" ]; then
  err "sha256 不匹配!期望 $EXPECTED 实得 $ACTUAL —— 二进制可能被篡改,已中止。"; exit 1
fi
info "sha256 校验通过"
mv "$TMP/$ASSET" "$BIN"
chmod +x "$BIN"
info "二进制就位: $BIN"

# ---- 3. 密钥生成器 ----
gen_key() {
  local prefix="$1" rnd=""
  if command -v openssl >/dev/null 2>&1; then rnd="$(openssl rand -hex 24)"
  elif [ -r /dev/urandom ]; then rnd="$(LC_ALL=C tr -dc 'a-f0-9' < /dev/urandom | head -c 48)"
  else rnd="$(date +%s%N | sha256sum 2>/dev/null | head -c 48)"; [ -n "$rnd" ] || rnd="$(date +%s)$$"; fi
  printf "%s%s" "$prefix" "$rnd"
}

# ---- 4. 收集参数 ----
step "配置部署参数"
PORT="${KIROSTUDIO_PORT:-}"
if [ -z "$PORT" ] && [ "$YES" != "1" ]; then read -r -p "监听端口 [默认 8990]: " PORT || true; fi
PORT="${PORT:-8990}"
if ! printf '%s' "$PORT" | grep -qE '^[0-9]+$' || [ "$PORT" -lt 1 ] || [ "$PORT" -gt 65535 ]; then
  err "端口非法: $PORT"; exit 1
fi
API_KEY="${KIROSTUDIO_API_KEY:-}"
if [ -z "$API_KEY" ] && [ "$YES" != "1" ]; then read -r -p "客户端 API Key(回车自动生成): " API_KEY || true; fi
[ -n "$API_KEY" ] || { API_KEY="$(gen_key 'sk-kiro-')"; info "已自动生成客户端 API Key"; }
ADMIN_KEY="${KIROSTUDIO_ADMIN_KEY:-}"
if [ -z "$ADMIN_KEY" ] && [ "$YES" != "1" ]; then read -r -p "管理 Admin Key(回车自动生成): " ADMIN_KEY || true; fi
[ -n "$ADMIN_KEY" ] || { ADMIN_KEY="$(gen_key 'sk-admin-')"; info "已自动生成管理 Admin Key"; }

# ---- 5. 生成 config(幂等,已存在不覆盖)----
step "写入配置"
mkdir -p "$CONFIG_DIR"
if [ -f "$CONFIG_FILE" ]; then
  warn "已存在 $CONFIG_FILE,保留不覆盖(重置请手动删后重跑)"
else
  # 二进制直跑默认监听 0.0.0.0:$PORT。host 0.0.0.0 便于对外/局域网访问;
  # 若仅本机用,可改回 127.0.0.1。
  cat > "$CONFIG_FILE" <<EOF
{
  "host": "0.0.0.0",
  "port": $PORT,
  "apiKey": "$API_KEY",
  "adminApiKey": "$ADMIN_KEY",
  "tlsBackend": "rustls",
  "region": "us-east-1",
  "defaultEndpoint": "ide",
  "loadBalancingMode": "priority"
}
EOF
  chmod 600 "$CONFIG_FILE"
  info "已生成 $CONFIG_FILE(0600)"
fi
if [ ! -f "$CRED_FILE" ]; then
  echo '[]' > "$CRED_FILE"; chmod 600 "$CRED_FILE"
  info "已生成空 $CRED_FILE(启动后在管理面板上号)"
fi

# ---- 6. systemd 安装(用当前用户+当前目录,不写死任何路径)----
if [ "${KIROSTUDIO_NO_SYSTEMD:-0}" = "1" ] || ! command -v systemctl >/dev/null 2>&1; then
  step "跳过 systemd(未启用或无 systemctl)"
  info "前台运行: $BIN -c config/config.json --credentials config/credentials.json"
else
  step "安装 systemd 服务"
  RUN_USER="$(id -un)"
  UNIT="/etc/systemd/system/kirostudio.service"
  # 需要 root 写 /etc/systemd;非 root 自动加 sudo
  SUDO=""; [ "$(id -u)" -ne 0 ] && SUDO="sudo"
  $SUDO tee "$UNIT" >/dev/null <<EOF
[Unit]
Description=KiroStudio (Anthropic <-> Kiro API 网关)
After=network-online.target
Wants=network-online.target
StartLimitIntervalSec=60
StartLimitBurst=10

[Service]
Type=simple
User=$RUN_USER
WorkingDirectory=$SCRIPT_DIR
ExecStart=$BIN -c config/config.json --credentials config/credentials.json
Restart=always
RestartSec=3

[Install]
WantedBy=multi-user.target
EOF
  $SUDO systemctl daemon-reload
  $SUDO systemctl enable kirostudio >/dev/null 2>&1 || true
  $SUDO systemctl restart kirostudio
  sleep 2
  if $SUDO systemctl is-active --quiet kirostudio; then
    info "服务已启动(kirostudio.service,用户 $RUN_USER)"
  else
    err "服务启动失败,查看日志: sudo journalctl -u kirostudio -n 50"
    exit 1
  fi
fi

# ---- 7. 收尾 ----
step "完成"
IP="$(hostname -I 2>/dev/null | awk '{print $1}')"; [ -n "$IP" ] || IP="<本机IP>"
cat <<EOF

  ${C_G}KiroStudio 已部署${C_N}
  管理面板:  http://$IP:$PORT/admin
  API 端点:  http://$IP:$PORT/v1/messages
  客户端 Key: $API_KEY
  管理 Key:   $ADMIN_KEY   ${C_Y}(请妥善保存)${C_N}

  下一步: 打开管理面板 -> 上号(social/idc/微软SSO)-> 即可用。
EOF
