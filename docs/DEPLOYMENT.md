# 部署与运维健壮性（Docker / systemd）

本文档覆盖容器与服务器的运维健壮性配置：数据持久化、日志上限、健康探针、
crashloop 回滚。仓库根的 `docker-compose.yml` 已内置全部容器侧配置；
服务器 `/opt/skiapi` 的 compose 与 systemd 单元应逐条对照本文核对。

## 1. 数据落卷（traces.db / usage-*.jsonl 持久化）

用量数据（SQLite 明细 `traces.db` + 按天 JSONL `usage-*.jsonl`）默认落在
`usageDataDir`（缺省 `data/usage`，相对进程 cwd；容器内 WORKDIR 为 `/app`）。

- **仓库 compose**：已挂 `./data:/app/data`，容器重启/重建不丢数据。
- **服务器 `/opt/skiapi`**：核对 compose 的 `volumes`，确认有
  `- ./data:/app/data`（或把 `usageDataDir` 显式指向已挂载目录）。
  不挂载的后果：容器重建后用量明细与统计全丢，面板成功率/RPM 归零重来。

数据目录里可能还有 `kiro_balance_cache.json`、`socks_nodes.json` 等缓存
（取决于 `cacheDir` 配置），一并落在持久化卷内更好。

## 2. 日志上限（防磁盘写满）

容器日志驱动无上限时，`json-file` 会让日志无界膨胀占满磁盘（用量管道还另写
文件，日志是额外一路）。仓库 compose 已设：

```yaml
logging:
  driver: json-file
  options:
    max-size: "10m"
    max-file: "5"
```

- **服务器 `/opt/skiapi`**：同样加这段（或 `docker run --log-opt max-size=10m --log-opt max-file=5`）。
- **systemd 部署**：`journald` 本身有 `SystemMaxUse` 兜底，但建议在单元里加
  `StandardOutput=journal`（默认）即可；若用 `StandardOutput=file:` 需自行轮转。

## 3. HEALTHCHECK（探 /v1/models，401 也活）

仓库 compose 探 `http://127.0.0.1:8990/v1/models`，**200 与 401 都算健康**：

```yaml
healthcheck:
  test: ["CMD-SHELL", "curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:8990/v1/models | grep -Eq '^(200|401)$'"]
  interval: 30s
  timeout: 5s
  start_period: 20s
  retries: 3
```

为什么不是 `/admin`：`/admin` 是带 adminApiKey 鉴权的完整 UI 页，探它既重又不
反映「网关可用」。`/v1/models` 不带有效 apiKey 时返 401——但 401 说明连接建立、
路由命中、鉴权拦截都正常，进程活着。HTTP 层挂了（连接拒绝/超时）才会让探针失败。

另外新增了未鉴权的 `GET /healthz`（返回 `{ok, version, config_loaded, pool_count,
sqlite_writable}`），适合反代（Caddy 等）做主动探测；它不需要任何密钥。

- **服务器 `/opt/skiapi`**：若容器内镜像有 curl，直接照抄；否则用 `wget` 或
  `CMD-SHELL` 里用 sh 自写（镜像必须带探活工具，见 Dockerfile 的 `apk add curl`）。

## 4. crashloop 回滚

### Docker 部署

`restart: unless-stopped` 会无限重启——新版启动即崩时表现为「一直在重启」。
两个处置：

- **有限重启**：用 `restart: on-failure:5`（最多重启 5 次后容器进入 exited 状态，
  `docker compose ps` 可看见失败状态，便于人工介入）。代价：`on-failure:N` 不覆盖
  `docker stop`/重启守护进程的场景，`unless-stopped` 更省心。
- **推荐组合**：保持 `unless-stopped` + 部署脚本检测。升级前把当前版本留一份
  （如 `kirostudio.bak`），配合 `docker inspect` 看容器重启次数：

  ```bash
  # 连续重启检测（示例：3 次以上且状态 unhealthy 即回滚）
  docker inspect kirostudio --format '{{.RestartCount}}'
  ```

  回滚动作：`docker compose stop && docker compose build --no-cache`（旧镜像 tag
  覆盖）或直接改 image tag 指回上一版本再 `up -d`。

### systemd 部署（线上 /opt/skiapi 现状）

已有成套机制，**不需要**另改：

- `deploy/rollback-guard.sh`（ExecStartPre）+ 进程侧 `common::health_marker`：
  新版启动即崩（连 bind 都到不了）时，`.boot_attempts` 计数跨重启累积到阈值
  （默认 3），自动用 `kirostudio.bak` 回滚旧版再启动。
- 配合 `Restart=on-failure`（RestartSec=3），crashloop 约 10s 内触发回滚。
- 进程 bind 成功后清零计数，健康运行不受影响。

## 5. 升级核对清单

1. `docker compose pull` / 构建新镜像前，确认 `./data` 与 `./config` 都已挂载；
2. 升级后 `docker compose ps` 确认 `healthy`（healthcheck 探 /v1/models）；
3. 打开 `GET /healthz` 看 `sqlite_writable` 是否为 true（用量统计可用）；
4. 万一 crashloop：Docker 看 `RestartCount`，systemd 等 `rollback-guard.sh`
   自动回滚，或手动 `cp kirostudio.bak kirostudio` 回退。

## 6. 运维观测与告警

### 6.1 健康探针 `/healthz`（未鉴权）

见 §3 末尾：返回 `{ok, version, config_loaded, pool_count, sqlite_writable}`，
反代（Caddy 等）主动探测用，不需要任何密钥。`pool_count` 反映当前凭据池
规模，`sqlite_writable` 反映用量明细落盘是否可用。

### 6.2 诊断快照端点（Admin API）

`GET /api/admin/diagnostics/snapshot` —— 运维诊断一键聚合（纯观测，零副作用）：
版本 / 逐号状态（禁用、冷却、健康分、余额）/ 代理池健康 / 自愈计数器等
全部收进一个 JSON，排障首查这一处。走 Admin API 鉴权（`adminApiKey`）。
同类的按需观测端点还有 `/api/admin/recovery-metrics`（自愈计数器）、
`/api/admin/endpoint-health`（每凭据×端点的实测成功率）、
`/api/admin/logs/export`（内存环形日志导出）。

### 6.3 代理池自动健康调度开关（内存态，不进 config.json）

`socks_auto_health`：后台每 5 分钟对池内启用代理节点做一轮健康探测，
连续失败达阈值自动禁用该节点。**AdminService 内存态开关**（默认开；
重启回默认 true），经 Admin API `PUT /config` 修改，config.json 里没有
对应字段——排查「面板开关改不到」类问题时先确认走的是 PUT /config
而非直改配置文件。

### 6.4 Webhook 告警（config.json 静态配置）

配置 `alertWebhookUrl`（+ `alertCooldownSecs`，默认 600s）后，关键自愈事件
（吸收预算耗尽 / failover 号全灭 / 重试配额耗尽 / 429 风暴吸收轮等）会
POST 一条 `{key, value, window_secs, host}` JSON 到该地址；同 key 在冷却
窗口内只发一次，窗口内重复事件只累计计数（value 携带增量）。**热更不生效**
（provider 构造时注入），改后需重启。⚠️ 安全：网关会向该 URL 发起请求，
SSRF 风险自负——建议填内网不可达、只能外联的告警服务，绝不填内网管理面
地址。
