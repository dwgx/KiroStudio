# 容器部署崩溃循环（crashloop）回滚流程

> 本仓同时维护两套部署形态：仓库根 `docker-compose.yml`（容器，默认 8990）
> 与 `deploy/` 的 systemd 脚本（线上 `/opt/skiapi` 现状）。本文件分别给出
> 两套的崩溃循环判定、回滚资产与操作步骤，全部以本仓实际脚本为准。

## 1. 判定标准：怎么算 crashloop

**容器（compose）**：`restart: unless-stopped` 会无限重启，新版启动即崩时
表现为「容器反复 Restarting」。判定三要素：

```bash
# 1) 重启计数持续增长（间隔几十秒看两次）
docker inspect kirostudio --format '{{.RestartCount}}'

# 2) 状态长期不健康（healthcheck 30s 间隔 x 3 次重试，失败约 90s 后标 unhealthy）
docker compose ps

# 3) 日志尾部反复出现同一崩溃点（每次重启日志尾部相同）
docker logs --tail 50 kirostudio
```

注意：**「进程活着但行为劣化」不算此处的 crashloop**（端口有人监听、探针
200/401 都过），那是 `deploy-watchdog.sh` 的管辖范围（见 §4）。

**systemd 部署**：`deploy/rollback-guard.sh`（ExecStartPre）+ 进程侧
`common::health_marker` 的计数语义：

- 每次启动前 `.boot_attempts` 计数器 +1；进程 bind 成功后清零。
- 只有「连 bind 都到不了就崩」的启动才让计数跨重启累积——健康后正常
  一键重启不会误判。
- 判定：存在 `kirostudio.bak`（有可回滚的旧版）且计数 >= 阈值（默认 3，
  RestartSec=3 下约 10s 攒够）→ 认定新版启动即崩，自动回滚。
- systemd 层还有二级止损：`StartLimitIntervalSec=60` / `StartLimitBurst=10`
  （install-service.sh 写入单元）。

## 2. 回滚资产盘点（升级前就该知道在哪）

| 资产 | 位置 | 语义 |
| --- | --- | --- |
| 旧配置/凭据 | compose 卷 `./config`（宿主目录，挂 `/app/config`） | 升级只换镜像/重建，卷不动 → 旧 `config.json`、`credentials.json` 天然还在 |
| 用量数据 | compose 卷 `./data`（挂 `/app/data`，`traces.db`、`usage-*.jsonl`） | 重建容器不丢，回滚无需处理 |
| 配置 .bak | `./config/config.json.bak`（.bak.1/.bak.2） | 进程内 `rotate_config_backup` 写盘前轮换，保留 3 份，**只含 config.json 本体** |
| 二进制 .bak（systemd） | `kirostudio.bak`（deploy.sh 轮换保留 5 份）、`kirostudio.bak.N` | 部署脚本维护的旧版二进制 |
| 二进制 .prev（hotswap） | `/opt/kirostudio/bin/kirostudio.prev` | hotswap.sh 换入新二进制前备份，交接失败自动用它回滚，回滚后仍保留 |

密钥注意：`.at_rest.key` 与凭据**同目录**（配置目录，非数据目录），
任何「整目录打包」的备份必须排除它，见 `docs/SECURITY-BACKUP.md`。

## 3. 容器（compose）回滚步骤

前置：确认 `./data` 与 `./config` 两个卷都在（升级前核对清单的第一条）。

**停新起旧（最常见，镜像本身没问题、只是配置/启动参数不对）**：

```bash
docker compose stop
# 把 image 指回上一版本 tag（升级前构建时按版本打 tag，如 kirostudio:2026-08-14）
# 或本地留了旧镜像：docker tag kirostudio:last-known-good kirostudio:latest
docker compose up -d
```

**恢复配置 .bak（若崩溃根因是配置被改坏）**：

```bash
# W2 写盘前会把旧配置轮换成 .bak/.bak.1/.bak.2（config 目录内，容器内外同见）
cp ./config/config.json.bak ./config/config.json
docker compose restart
# 或直接删掉坏配置让服务以默认值引导后从设置页重配
```

**重建镜像（镜像构建产物坏了/代码问题，需回到旧代码）**：

```bash
docker compose stop
docker compose build --no-cache   # 重建的是当前代码；要回旧代码须在旧 commit 的
                                  # 独立 worktree 下构建并打旧 tag，再 up -d
docker compose up -d
```

> 本仓工作树禁止 git checkout 回退（多会话并发），回旧代码用
> `git worktree add` 另起目录构建，或直接用上一版本镜像 tag。

**验证恢复**：

```bash
docker compose ps                    # healthy
curl -s http://127.0.0.1:8990/healthz   # sqlite_writable=true
curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:8990/v1/models  # 200 或 401
```

## 4. systemd 部署（线上现状）回滚

已有成套机制，正常情况下不需要人工动作：

- **自动**：`rollback-guard.sh`（ExecStartPre）检测到 crashloop 后把坏版改名
  `kirostudio.failed.<时间戳>` 留证，用 `kirostudio.bak` 覆盖回旧版、删 .bak、
  清零计数，本次 ExecStart 随即拉起旧版。fail-safe：缺 .bak / 首次部署 /
  读计数失败一律放行启动，守卫绝不挡住服务。
- **hotswap.sh 语义**：零空窗交接失败时自动 `install` 回滚 `.prev` 并保留
  回滚点；`/opt/kirostudio/bin/kirostudio.prev` 是手动的回滚点（`kirostudio-update
  rollback` 也依赖它）。
- **deploy.sh**：健康检查失败（/admin 200 + /v1/models 200|401 + 运行进程
  md5 校验）自动用 `.bak` 恢复为 `${BIN}.rollback` 并重启，仍不健康才需要
  人工介入。
- **deploy-watchdog.sh**：交接后持续观测，panic 计数 >0 / 进程不在 /
  端口无人监听 / 网关侧成功率下降超容忍值（剔除上游归因）任一命中即调
  hotswap 回滚到指定回滚点。

人工回滚兜底：

```bash
sudo systemctl stop kirostudio
sudo cp -a /opt/kirostudio/bin/kirostudio.prev /opt/kirostudio/bin/kirostudio
sudo systemctl start kirostudio
```

## 5. 预防（从根上减少 crashloop）

- HEALTHCHECK 探 `/v1/models`：200 与 401 都算健康（401 说明连接建立、路由
  命中、鉴权拦截都正常）。不探 `/admin`（重、且需 adminApiKey）。
- `GET /healthz` 未鉴权探活：`sqlite_writable` 反映用量统计可用性。
- hotswap.sh 双预检：`--version` 能跑 + 二进制含 SO_REUSEPORT，提前拦住
  必败的交接。
- bluegreen.sh：新二进制先在临时端口 8995 用主配置副本验证，人工确认才
  promote。
- 升级前核对清单：确认 `./data` 与 `./config` 挂载 → 构建/拉取 → `docker
  compose ps` 看 healthy → `/healthz` 看 sqlite_writable。

## 6. 备份与密钥注意事项

- 本仓 deploy/ 下**没有独立的备份脚本**：systemd 侧的备份是二进制的
  `.bak` 轮换，容器侧的数据备份就是卷本身（`./data`、`./config`）。
- `config.json.bak` 轮换（W2 进程内）只含配置本体，不含凭据、不含
  at-rest 密钥文件。
- 密钥文件与凭据同目录（config 卷内）。**任何把 config 目录整目录打包
  带走/上传的备份操作，必须先排除密钥文件**——密钥与密文同进备份包 =
  备份泄露即凭据全解（详见 `docs/SECURITY-BACKUP.md`）。
- 密钥丢失 = 密文永久解不开：迁移/删除前先在设置页导出明文凭据。
