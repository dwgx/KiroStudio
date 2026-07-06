#!/bin/bash
# KiroStudio 一键部署 —— 命脉安全第一：拉取→校验→换inode→重启→验证真上线→失败自动回滚
# 8990 是 Claude Code 自己的命脉，绝不留停摆。
# 用法：bash deploy.sh [tag]    tag 默认 latest
set -euo pipefail

REPO="dwgx/KiroStudio"
BIN="/home/dwgx_user/KiroStudio/kirostudio"
SVC="kirostudio"
HEALTH="http://127.0.0.1:8990/admin"
MODELS="http://127.0.0.1:8990/v1/models"
ASSET="kirostudio-linux-x86_64"
TAG="${1:-latest}"
TMP="/tmp"

# 验证真上线：运行进程 md5 必须等于期望 md5(教训:光看磁盘 md5 会被骗,要验 /proc/PID/exe)
# 用法：health_ok <期望md5>
health_ok() {
  local want="$1" pid m code i
  pid=$(systemctl show -p MainPID --value "$SVC")
  [ -n "$pid" ] && [ "$pid" != "0" ] || { echo "  ✗ 无 MainPID(服务没起来)"; return 1; }
  m=$(sudo md5sum "/proc/$pid/exe" 2>/dev/null | awk '{print $1}')
  echo "  运行进程 pid=$pid exe-md5=$m"
  [ "$m" = "$want" ] || { echo "  ✗ 运行进程二进制 md5 与期望不符(可能没真上线)"; return 1; }
  code=""
  for i in 1 2 3 4 5; do
    code=$(curl -s -o /dev/null -w "%{http_code}" --max-time 10 "$HEALTH" || true)
    [ "$code" = "200" ] && break
    sleep 1
  done
  [ "$code" = "200" ] || { echo "  ✗ /admin 健康检查 = $code"; return 1; }
  echo "  ✓ /admin = 200"
  # /v1/models 无有效 key 返回 401 也算服务在线(路由已挂)
  code=$(curl -s -o /dev/null -w "%{http_code}" --max-time 10 -H "x-api-key: sk-test" "$MODELS" || true)
  [ "$code" = "200" ] || [ "$code" = "401" ] || { echo "  ✗ /v1/models = $code"; return 1; }
  echo "  ✓ /v1/models = $code"
  return 0
}

echo "=== [1/6] 拉取二进制 (tag=$TAG) ==="
rm -f "$TMP/$ASSET" "$TMP/$ASSET.sha256"
if command -v gh >/dev/null 2>&1; then
  echo "使用 gh 下载 release 资产..."
  if [ "$TAG" = "latest" ]; then
    gh release download -R "$REPO" -p "$ASSET" -p "$ASSET.sha256" -D "$TMP" --clobber
  else
    gh release download "$TAG" -R "$REPO" -p "$ASSET" -p "$ASSET.sha256" -D "$TMP" --clobber
  fi
else
  echo "无 gh，改用 curl 从 github releases 下载..."
  if [ "$TAG" = "latest" ]; then
    base="https://github.com/$REPO/releases/latest/download"
  else
    base="https://github.com/$REPO/releases/download/$TAG"
  fi
  curl -fsSL "$base/$ASSET" -o "$TMP/$ASSET"
  curl -fsSL "$base/$ASSET.sha256" -o "$TMP/$ASSET.sha256"
fi
[ -s "$TMP/$ASSET" ] || { echo "FAIL: 未下载到 $ASSET"; exit 1; }
echo "已下载到 $TMP/$ASSET"

echo "=== [2/6] 校验 (sha256 + ELF) ==="
# sha256 文件内记录的文件名是 kirostudio-linux-x86_64，需在 /tmp 下校验
( cd "$TMP" && sha256sum -c "$ASSET.sha256" ) || { echo "FAIL: sha256 校验不通过，中止(不碰现网)"; exit 1; }
file "$TMP/$ASSET" | grep -q "ELF" || { echo "FAIL: 不是 ELF 可执行文件，中止"; exit 1; }
chmod +x "$TMP/$ASSET"
NEW_MD5=$(md5sum "$TMP/$ASSET" | awk '{print $1}')
echo "  ✓ sha256 通过；ELF 确认；新二进制 md5=$NEW_MD5"

echo "=== [3/6] 备份现网二进制 ==="
TS=$(date +%Y%m%d-%H%M%S)
BAK="${BIN}.bak.$TS"
if [ -f "$BIN" ]; then
  cp -f "$BIN" "$BAK"
  echo "  已备份到 $BAK"
else
  echo "  现网无旧二进制(首次部署)，跳过备份"
  BAK=""
fi

echo "=== [4/6] 替换二进制 (换 inode 避免 Text file busy) ==="
# 教训：正在运行的二进制不能直接 cp 覆盖(Text file busy)，
# 必须 cp 到同目录临时文件再 mv -f 覆盖 —— mv 换的是 inode，运行中的老进程继续用老 inode。
cp -f "$TMP/$ASSET" "${BIN}.new"
chmod +x "${BIN}.new"
mv -f "${BIN}.new" "$BIN"
echo "  已换 inode 到 $BIN"

echo "=== [5/6] 重启服务并验证真上线 ==="
sudo systemctl restart "$SVC"
sleep 5
if health_ok "$NEW_MD5"; then
  echo "  ✓ 新二进制已真正上线并健康"
else
  echo "!!! 健康检查失败，触发自动回滚 !!!"
  if [ -n "$BAK" ] && [ -f "$BAK" ]; then
    cp -f "$BAK" "${BIN}.rollback"
    mv -f "${BIN}.rollback" "$BIN"
    OLD_MD5=$(md5sum "$BIN" | awk '{print $1}')
    sudo systemctl restart "$SVC"
    sleep 5
    if health_ok "$OLD_MD5"; then
      echo "=== 已回滚到旧二进制并恢复健康 ✓，8990 未停摆。请排查新版本后重试 ==="
    else
      echo "!!! 回滚后仍不健康，需人工介入：sudo systemctl status $SVC；备份在 $BAK !!!"
    fi
  else
    echo "!!! 无可用备份(首次部署失败)，需人工介入：sudo systemctl status $SVC !!!"
  fi
  exit 1
fi

echo "=== [6/6] 收尾 ==="
"$BIN" --version 2>/dev/null || echo "(--version 不可用，跳过)"
# 只保留最近 5 个备份
ls -1t "${BIN}.bak."* 2>/dev/null | tail -n +6 | while read -r old; do
  rm -f "$old" && echo "  清理旧备份 $old"
done
echo "=== 部署成功 ✓ tag=$TAG md5=$NEW_MD5 ==="

