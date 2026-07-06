#!/bin/bash
# 蓝绿验证部署 —— 主服务(8990)要稳，新二进制先在临时端口充分验证，人工确认才换主服务
# 用法：在服务器上跑。阶段1=验证(默认)，阶段2=切换(需显式 promote 参数)
set -u
STAGE="${1:-verify}"
TMP_PORT=8995
LIVE_BIN="/home/dwgx_user/KiroStudio/kirostudio"
LIVE_CFG="/home/dwgx_user/KiroStudio/config/config.json"
LIVE_CREDS="/home/dwgx_user/KiroStudio/config/credentials.json"
NEW_BIN="/tmp/kirostudio-new"
ADMIN_KEY="sk-dwgx-admin"

if [ "$STAGE" = "verify" ]; then
  echo "=== 蓝绿阶段1：临时端口 $TMP_PORT 验证新二进制(不碰主服务 8990) ==="
  [ -f "$NEW_BIN" ] || { echo "FAIL: $NEW_BIN 不存在，先提取新二进制"; exit 1; }
  # 用主服务的 config 副本改端口 + 只读一份 credentials 副本(避免与主服务争写)
  sudo cp "$LIVE_CFG" /tmp/bg-config.json
  sudo cp "$LIVE_CREDS" /tmp/bg-creds.json 2>/dev/null || echo "[]" > /tmp/bg-creds.json
  sudo chown "$(whoami)" /tmp/bg-config.json /tmp/bg-creds.json
  node -e 'const fs=require("fs");const c=JSON.parse(fs.readFileSync("/tmp/bg-config.json"));c.port='$TMP_PORT';fs.writeFileSync("/tmp/bg-config.json",JSON.stringify(c,null,2))'
  chmod +x "$NEW_BIN"
  # 后台起临时实例
  pkill -f "kirostudio-new -c /tmp/bg-config.json" 2>/dev/null; sleep 1
  nohup "$NEW_BIN" -c /tmp/bg-config.json --credentials /tmp/bg-creds.json > /tmp/bg-verify.log 2>&1 &
  BGPID=$!
  echo "临时实例 pid=$BGPID，等待启动..."
  sleep 4
  # 健康检查
  fail=0
  echo "--- 健康检查 ---"
  for probe in \
    "models:GET:/v1/models:x-api-key:sk-test" \
    "admin:GET:/admin::" \
    "creds:GET:/api/admin/credentials:x-api-key:$ADMIN_KEY" \
    "trash:GET:/api/admin/credentials/trash:x-api-key:$ADMIN_KEY"; do
    IFS=: read name method path hk hv <<< "$probe"
    if [ -n "$hk" ]; then
      code=$(curl -s -o /dev/null -w "%{http_code}" --max-time 10 -H "$hk: $hv" "http://localhost:$TMP_PORT$path")
    else
      code=$(curl -s -o /dev/null -w "%{http_code}" --max-time 10 "http://localhost:$TMP_PORT$path")
    fi
    if [ "$code" = "200" ] || [ "$code" = "401" ]; then echo "  ✓ $name = $code"; else echo "  ✗ $name = $code"; fail=1; fi
  done
  # 进程还活着吗(没 panic)
  if kill -0 $BGPID 2>/dev/null; then echo "  ✓ 进程存活(无 panic)"; else echo "  ✗ 进程已死，见 /tmp/bg-verify.log"; fail=1; fi
  echo "--- 启动日志尾 ---"; tail -5 /tmp/bg-verify.log
  # 收尾：停临时实例
  kill $BGPID 2>/dev/null
  if [ "$fail" = "0" ]; then
    echo "=== 验证通过 ✓ 新二进制健康。确认无误后用 'bash $0 promote' 切换主服务 ==="
    exit 0
  else
    echo "=== 验证失败 ✗ 不要切换主服务，先修 ==="
    exit 1
  fi

elif [ "$STAGE" = "promote" ]; then
  echo "=== 蓝绿阶段2：切换主服务(先备份→stop→cp→start) ==="
  [ -f "$NEW_BIN" ] || { echo "FAIL: $NEW_BIN 不存在"; exit 1; }
  ts=$(date +%Y%m%d-%H%M%S)
  sudo cp "$LIVE_BIN" "/tmp/kirostudio.bak.$ts" && echo "已备份旧二进制到 /tmp/kirostudio.bak.$ts"
  sudo systemctl stop kirostudio && echo "已停主服务"
  sudo cp "$NEW_BIN" "$LIVE_BIN" && sudo chmod +x "$LIVE_BIN" && echo "已替换"
  sudo systemctl start kirostudio && sleep 3
  if sudo systemctl is-active --quiet kirostudio; then
    echo "=== 主服务已切换并存活 ✓ ==="
    curl -s -o /dev/null -w "8990 models=%{http_code}\n" -H "x-api-key: sk-test" http://localhost:8990/v1/models
  else
    echo "=== 主服务启动失败！回滚：sudo cp /tmp/kirostudio.bak.$ts $LIVE_BIN && sudo systemctl start kirostudio ==="
    exit 1
  fi
else
  echo "用法: bash $0 [verify|promote]"; exit 1
fi
