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
# ASSET / SHA_CMD 在下面「平台/架构检查」里按 OS×ARCH 推导（不在此硬编码，
# 否则 macOS / arm64 会静默下到 Linux x86_64 的包）。

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
# 资产名必须与 .github/workflows/release.yml 的产出、以及 src/admin/update.rs 的 ASSET_BIN
# 三方保持一致（OS × ARCH 两个维度）。少一个组合 = 该平台用户装不上 / OTA 下错包。
step "环境检查"
OS="$(uname -s 2>/dev/null || echo unknown)"
ARCH="$(uname -m 2>/dev/null || echo unknown)"

# 归一化架构名：uname -m 在不同平台叫法不同（amd64/x86_64、arm64/aarch64）。
case "$ARCH" in
  x86_64|amd64)  ARCH_N="x86_64" ;;
  aarch64|arm64) ARCH_N="aarch64" ;;
  *) err "不支持的架构 $ARCH —— 请从源码构建或用 Docker(bash install.sh)。"; exit 1 ;;
esac

case "$OS" in
  Linux)
    ASSET="kirostudio-linux-$ARCH_N"
    # Linux 侧目前 CI 只产出 x86_64（musl 静态）。arm64 需自行构建，明确告知而非静默下错包。
    if [ "$ARCH_N" != "x86_64" ]; then
      err "Linux 预编译二进制目前仅提供 x86_64（当前 $ARCH）。请从源码构建或用 Docker(bash install.sh)。"
      exit 1
    fi
    ;;
  Darwin)
    ASSET="kirostudio-macos-$ARCH_N"
    ;;
  *)
    err "预编译二进制支持 Linux / macOS。当前 $OS/$ARCH —— 请用 Docker(bash install.sh)或从源码构建。"
    exit 1
    ;;
esac

# sha256 工具在 Linux 是 sha256sum，macOS 自带的是 shasum -a 256。
if command -v sha256sum >/dev/null 2>&1; then
  SHA_CMD="sha256sum"
elif command -v shasum >/dev/null 2>&1; then
  SHA_CMD="shasum -a 256"
else
  err "缺少 sha256sum / shasum,无法校验完整性,已中止。"; exit 1
fi
command -v curl >/dev/null 2>&1 || { err "缺少命令: curl,请先安装。"; exit 1; }

info "$OS $ARCH_N,依赖就绪（资产: $ASSET）"

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
ACTUAL="$($SHA_CMD "$TMP/$ASSET" | awk '{print $1}')"
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
  else rnd="$(date +%s%N | $SHA_CMD 2>/dev/null | head -c 48)"; [ -n "$rnd" ] || rnd="$(date +%s)$$"; fi
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

# ---- 6. 进程守护安装(用当前用户+当前目录,不写死任何路径)----
# Linux 用 systemd，macOS 用 launchd（LaunchAgent）。两者都提供"崩了自动拉起"，
# 这是 OTA 一键升级能成立的前提：面板升级完会让进程 exit(0)，需要守护者把新二进制拉起来。
if [ "${KIROSTUDIO_NO_SYSTEMD:-0}" = "1" ]; then
  step "跳过进程守护安装(KIROSTUDIO_NO_SYSTEMD=1)"
  info "前台运行: $BIN -c config/config.json --credentials config/credentials.json"

elif [ "$OS" = "Darwin" ]; then
  # macOS：用 per-user LaunchAgent（不需要 root，随登录会话启动）。
  # KeepAlive=true 等价于 systemd 的 Restart=always —— 这是 OTA 与面板"一键重启"
  # 在 macOS 上能自愈的关键（restart_service 的 macOS 分支虽会自行 spawn 助手拉起，
  # 但有守护者更稳妥：任何非预期崩溃也能恢复）。
  step "安装 launchd 服务 (macOS LaunchAgent)"
  LA_LABEL="com.dwgx.kirostudio"
  LA_DIR="$HOME/Library/LaunchAgents"
  LA_PLIST="$LA_DIR/$LA_LABEL.plist"
  mkdir -p "$LA_DIR" "$SCRIPT_DIR/logs"
  cat > "$LA_PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>$LA_LABEL</string>
    <key>ProgramArguments</key>
    <array>
        <string>$BIN</string>
        <string>-c</string>
        <string>config/config.json</string>
        <string>--credentials</string>
        <string>config/credentials.json</string>
    </array>
    <key>WorkingDirectory</key>
    <string>$SCRIPT_DIR</string>
    <!-- 等价于 systemd Restart=always：退出即拉起（含 OTA 升级后的 exit(0)） -->
    <key>KeepAlive</key>
    <true/>
    <key>RunAtLoad</key>
    <true/>
    <!-- 崩溃循环保护：最短重启间隔，避免起不来时疯狂重试打满 CPU -->
    <key>ThrottleInterval</key>
    <integer>3</integer>
    <key>StandardOutPath</key>
    <string>$SCRIPT_DIR/logs/kirostudio.out.log</string>
    <key>StandardErrorPath</key>
    <string>$SCRIPT_DIR/logs/kirostudio.err.log</string>
</dict>
</plist>
EOF
  # 先卸载旧的（忽略不存在的错误），再加载新的。
  launchctl unload "$LA_PLIST" >/dev/null 2>&1 || true
  launchctl load "$LA_PLIST" || { err "launchctl load 失败: $LA_PLIST"; exit 1; }
  sleep 2
  if launchctl list | grep -q "$LA_LABEL"; then
    info "服务已启动 (launchd: $LA_LABEL)"
    info "管理命令: launchctl unload/load $LA_PLIST"
    info "日志: $SCRIPT_DIR/logs/kirostudio.{out,err}.log"
  else
    err "服务启动失败,查看日志: $SCRIPT_DIR/logs/kirostudio.err.log"
    exit 1
  fi

elif ! command -v systemctl >/dev/null 2>&1; then
  step "跳过进程守护(无 systemctl)"
  info "前台运行: $BIN -c config/config.json --credentials config/credentials.json"

else
  step "安装 systemd 服务"
  RUN_USER="$(id -un)"
  UNIT="/etc/systemd/system/kirostudio.service"
  # 需要 root 写 /etc/systemd;非 root 自动加 sudo
  SUDO=""; [ "$(id -u)" -ne 0 ] && SUDO="sudo"
  # ⚠️ ExecStartPre 挂 rollback-guard.sh：OTA 推下一个"启动即崩"的二进制时，
  # 守卫脚本靠 <exe>.boot_attempts 计数 + <exe>.bak 回滚点，在连续 3 次启动失败后
  # 自动回滚到上一个已知良好版本。没有这一行，坏版本只会被 Restart=always 反复拉起，
  # 60s 内 10 次后被 StartLimit 停住 → 服务全程不可用且不会自愈。
  # 前缀 `-` 表示该步骤失败不阻断启动（守卫本身恒 exit 0，这里是双保险）。
  # 仅当脚本确实存在时才挂（install-binary.sh 只下载单个二进制，不含 deploy/ 目录）。
  GUARD_LINE=""
  if [ -f "$SCRIPT_DIR/deploy/rollback-guard.sh" ]; then
    chmod +x "$SCRIPT_DIR/deploy/rollback-guard.sh" 2>/dev/null || true
    GUARD_LINE="ExecStartPre=-$SCRIPT_DIR/deploy/rollback-guard.sh"
  fi
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
Environment=KIRO_WORKDIR=$SCRIPT_DIR
$GUARD_LINE
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
    [ -n "$GUARD_LINE" ] && info "已挂载 OTA 崩溃回滚守卫 (rollback-guard.sh)" \
      || warn "未找到 deploy/rollback-guard.sh —— OTA 崩溃时无自动回滚（建议整仓部署以获得该保护）"
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
