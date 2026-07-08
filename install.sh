#!/usr/bin/env bash
# ============================================================================
# KiroStudio 一键部署引导脚本（跨平台 Bash：Linux / macOS / WSL / Git Bash）
#
#   bash install.sh
#
# 功能：检测 Docker 环境 -> 交互式设置端口与密钥（可自动生成）->
#       生成 config/config.json 与 config/credentials.json -> 构建并启动容器 ->
#       打印面板地址与后续上号引导。已有配置不覆盖（幂等）。
#
# 可用环境变量跳过交互（CI / 无人值守）：
#   KIROSTUDIO_PORT     对外端口（默认 8990）
#   KIROSTUDIO_API_KEY  客户端密钥（留空自动生成）
#   KIROSTUDIO_ADMIN_KEY 管理面板密钥（留空自动生成）
#   KIROSTUDIO_YES=1    非交互模式，全部用默认值 / 自动生成
# ============================================================================
set -euo pipefail

# ---- 输出着色（无 tty 时降级为纯文本）----
if [ -t 1 ]; then
  C_G='\033[0;32m'; C_Y='\033[1;33m'; C_R='\033[0;31m'; C_B='\033[0;36m'; C_N='\033[0m'
else
  C_G=''; C_Y=''; C_R=''; C_B=''; C_N=''
fi
info()  { printf "${C_G}[✓]${C_N} %s\n" "$1"; }
warn()  { printf "${C_Y}[!]${C_N} %s\n" "$1"; }
err()   { printf "${C_R}[✗]${C_N} %s\n" "$1" >&2; }
step()  { printf "\n${C_B}==>${C_N} %s\n" "$1"; }

# 脚本所在目录，允许从任意路径调用
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

CONFIG_DIR="$SCRIPT_DIR/config"
CONFIG_FILE="$CONFIG_DIR/config.json"
CRED_FILE="$CONFIG_DIR/credentials.json"

YES="${KIROSTUDIO_YES:-0}"

# ---- 1. 检测 Docker 与 Compose ----
step "检测 Docker 环境"

if ! command -v docker >/dev/null 2>&1; then
  err "未检测到 docker。请先安装 Docker："
  cat <<'EOF'
    - Linux (Ubuntu/Debian): curl -fsSL https://get.docker.com | sh
    - macOS / Windows:       安装 Docker Desktop  https://www.docker.com/products/docker-desktop
    - 其他发行版参考:         https://docs.docker.com/engine/install/
EOF
  exit 1
fi
info "docker 已安装：$(docker --version 2>/dev/null || echo '未知版本')"

# 优先用 docker compose（v2 插件），回退到独立 docker-compose（v1）
if docker compose version >/dev/null 2>&1; then
  COMPOSE="docker compose"
elif command -v docker-compose >/dev/null 2>&1; then
  COMPOSE="docker-compose"
else
  err "未检测到 Docker Compose。请安装 Compose 插件："
  cat <<'EOF'
    - Docker Desktop 已自带 compose
    - Linux 独立安装:  https://docs.docker.com/compose/install/linux/
EOF
  exit 1
fi
info "compose 命令：$COMPOSE"

# docker 守护进程是否可用
if ! docker info >/dev/null 2>&1; then
  err "无法连接 Docker 守护进程。请确认 Docker 已启动，且当前用户有权限（Linux 上可能需 sudo 或加入 docker 组）。"
  exit 1
fi

# ---- 2. 随机密钥生成器 ----
# 优先 openssl，其次 /dev/urandom，最后 date+PID 兜底
gen_key() {
  local prefix="$1"
  local rnd=""
  if command -v openssl >/dev/null 2>&1; then
    rnd="$(openssl rand -hex 24)"
  elif [ -r /dev/urandom ]; then
    rnd="$(LC_ALL=C tr -dc 'a-f0-9' < /dev/urandom | head -c 48)"
  else
    rnd="$(date +%s%N | sha256sum 2>/dev/null | head -c 48)"
    [ -n "$rnd" ] || rnd="$(date +%s)$$"
  fi
  printf "%s%s" "$prefix" "$rnd"
}

# ---- 3. 交互式收集参数 ----
step "配置部署参数"

# 端口
PORT="${KIROSTUDIO_PORT:-}"
if [ -z "$PORT" ] && [ "$YES" != "1" ]; then
  read -r -p "监听端口 [默认 8990]: " PORT || true
fi
PORT="${PORT:-8990}"
if ! printf '%s' "$PORT" | grep -qE '^[0-9]+$' || [ "$PORT" -lt 1 ] || [ "$PORT" -gt 65535 ]; then
  err "端口非法：$PORT（应为 1-65535 的整数）"
  exit 1
fi

# 客户端 apiKey
API_KEY="${KIROSTUDIO_API_KEY:-}"
if [ -z "$API_KEY" ] && [ "$YES" != "1" ]; then
  read -r -p "客户端 API Key（回车自动生成随机安全密钥）: " API_KEY || true
fi
if [ -z "$API_KEY" ]; then
  API_KEY="$(gen_key 'sk-kiro-')"
  info "已自动生成客户端 API Key"
fi

# 管理面板 adminApiKey
ADMIN_KEY="${KIROSTUDIO_ADMIN_KEY:-}"
if [ -z "$ADMIN_KEY" ] && [ "$YES" != "1" ]; then
  read -r -p "管理面板 Admin Key（回车自动生成随机安全密钥）: " ADMIN_KEY || true
fi
if [ -z "$ADMIN_KEY" ]; then
  ADMIN_KEY="$(gen_key 'sk-admin-')"
  info "已自动生成管理面板 Admin Key"
fi

# ---- 4. 写 .env（供 docker compose 读取端口）----
step "写入环境与配置文件"

ENV_FILE="$SCRIPT_DIR/.env"
if [ -f "$ENV_FILE" ] && grep -qE '^KIROSTUDIO_PORT=' "$ENV_FILE" 2>/dev/null; then
  # 已有 .env 则就地更新端口，保留其他自定义项
  if grep -qE "^KIROSTUDIO_PORT=$PORT$" "$ENV_FILE"; then
    info ".env 端口已是 $PORT，跳过"
  else
    tmp="$(mktemp)"
    sed "s/^KIROSTUDIO_PORT=.*/KIROSTUDIO_PORT=$PORT/" "$ENV_FILE" > "$tmp" && mv "$tmp" "$ENV_FILE"
    info "已更新 .env 端口为 $PORT"
  fi
else
  printf 'KIROSTUDIO_PORT=%s\n' "$PORT" > "$ENV_FILE"
  info "已写入 .env（端口 $PORT）"
fi

# ---- 5. 生成 config/config.json（幂等：已存在不覆盖）----
mkdir -p "$CONFIG_DIR"

if [ -f "$CONFIG_FILE" ]; then
  warn "已存在 $CONFIG_FILE，保留原文件不覆盖（如需重置请手动删除后重跑）"
else
  cat > "$CONFIG_FILE" <<EOF
{
  "host": "0.0.0.0",
  "port": 8990,
  "apiKey": "$API_KEY",
  "adminApiKey": "$ADMIN_KEY",
  "tlsBackend": "rustls",
  "region": "us-east-1",
  "defaultEndpoint": "ide",
  "loadBalancingMode": "priority"
}
EOF
  chmod 600 "$CONFIG_FILE" 2>/dev/null || true
  info "已生成 $CONFIG_FILE（权限 600）"
fi

# ---- 6. 生成空 credentials.json（幂等）----
if [ -f "$CRED_FILE" ]; then
  warn "已存在 $CRED_FILE，保留原文件不覆盖"
else
  printf '[]\n' > "$CRED_FILE"
  chmod 600 "$CRED_FILE" 2>/dev/null || true
  info "已生成空 $CRED_FILE（稍后在面板上号 / 加凭据）"
fi

# ---- 7. 构建并启动 ----
step "构建镜像并启动容器（首次构建较慢，请耐心等待）"
$COMPOSE up -d --build

# ---- 8. 探测本机 IP（尽力而为，仅用于打印引导链接）----
detect_ip() {
  local ip=""
  if command -v hostname >/dev/null 2>&1; then
    ip="$(hostname -I 2>/dev/null | awk '{print $1}')" || true
  fi
  if [ -z "$ip" ] && command -v ip >/dev/null 2>&1; then
    ip="$(ip route get 1.1.1.1 2>/dev/null | awk '{for(i=1;i<=NF;i++) if($i=="src"){print $(i+1); exit}}')" || true
  fi
  [ -n "$ip" ] && printf '%s' "$ip" || printf '<你的服务器IP>'
}
IP="$(detect_ip)"

# ---- 9. 完成引导 ----
step "部署完成"
cat <<EOF

  ${C_G}KiroStudio 已启动${C_N}

  管理面板:   ${C_B}http://${IP}:${PORT}/admin${C_N}
              本机访问也可用 http://127.0.0.1:${PORT}/admin
  面板密钥:   ${ADMIN_KEY}
  客户端密钥: ${API_KEY}
              （下游 Claude Code / SDK 用，请求头 x-api-key 携带）

  ${C_Y}下一步：上号 / 加凭据${C_N}
   1. 浏览器打开上面的管理面板地址，用「面板密钥」登录。
   2. 在「凭据」页添加账号：
        - 网页上号：点「添加凭据」走 OAuth 登录（服务器部署需在 config.json
          里设 callbackBaseUrl 为公网地址）。
        - 手动导入：粘贴 refreshToken / API Key，或用「批量导入」。
   3. 也可直接编辑 ${CRED_FILE} 后重启：${COMPOSE} restart

  ${C_Y}客户端接入示例（Claude Code）${C_N}
     export ANTHROPIC_BASE_URL=http://${IP}:${PORT}
     export ANTHROPIC_API_KEY=${API_KEY}

  常用命令：
     查看日志:  ${COMPOSE} logs -f
     停止:      ${COMPOSE} down
     重启:      ${COMPOSE} restart
     改端口:    编辑 .env 的 KIROSTUDIO_PORT 后 ${COMPOSE} up -d

EOF



