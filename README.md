# KiroStudio

Rust/Axum 写的 LLM 网关，把 Anthropic Messages 协议翻译到 Kiro / AWS Q。前端 admin-ui（React）在编译期用 rust-embed 直接嵌进二进制，所以最终产物是**单个可执行文件**，不依赖外部静态资源目录。

## 目录

- [本地开发](#本地开发)
- [部署](#部署)
  - [1. 发布出包](#1-发布出包)
  - [2. 服务器一键部署](#2-服务器一键部署)
  - [3. 谨慎部署（可选）](#3-谨慎部署可选)
  - [4. 回滚](#4-回滚)
  - [5. 配置](#5-配置)

## 本地开发

后端（会读 `config/config.json` 与 `config/credentials.json`，也可用 `-c` / `--credentials` 指定）：

```bash
cargo run                                   # 用默认配置路径
cargo run -- -c config/config.json --credentials config/credentials.json
cargo test                                  # 跑测试
```

前端在 `admin-ui/`，开发时单独热更，改完 `pnpm build` 产物才会被 rust-embed 嵌入：

```bash
cd admin-ui
pnpm install
pnpm dev                                     # 本地热更，代理到后端
pnpm build                                   # 出 dist/，供 cargo build 嵌入
```

## 部署

架构上是单二进制，部署就是「换文件 + 重启 systemd」。生产跑在服务器上的 `kirostudio.service`（`Restart=always`，开机自启），主端口 **8990**。

> ⚠️ 8990 是 Claude Code 自己走的网关，是命脉。换二进制会有几秒中断，改核心逻辑或不放心时走[谨慎部署](#3-谨慎部署可选)。

### 1. 发布出包

打 tag 推上去，GitHub Actions 自动编译 musl 静态二进制并发到 Releases：

```bash
git tag v0.2.0
git push --tags
```

产物：`kirostudio-linux-x86_64`（纯 rustls、静态链接，`cargo build --release --no-default-features` + 前端 `pnpm build` 嵌入）。

### 2. 服务器一键部署

首次装 systemd unit（只跑一次）：

```bash
bash deploy/install-service.sh
```

之后每次升级，只需在服务器上：

```bash
bash deploy/deploy.sh          # 拉最新 release → 替换二进制 → 重启 → 健康检查 → 失败自动回滚
```

`deploy.sh` 会先备份当前二进制为 `.bak`，替换后做健康检查（`/v1/models`、`/admin`、admin API），任一步失败自动 `cp` 回备份并重启，保证服务不断。

### 3. 谨慎部署（可选）

改了核心链路、上线前想加一道保险时用两阶段蓝绿。新二进制先放到服务器 `/tmp/kirostudio-new`，然后：

```bash
bash deploy/bluegreen.sh verify     # 阶段1：临时端口 8995 起新实例做健康检查，完全不碰 8990
bash deploy/bluegreen.sh promote    # 阶段2：确认无误后才备份→停→替换→起主服务
```

`verify` 用主服务 config 的副本（改端口 + 只读一份 credentials 副本，避免与主服务争写），跑一轮健康探针并确认进程无 panic；只有你亲眼确认通过，才手动 `promote`。

### 4. 回滚

- **自动**：`deploy.sh` 部署失败会自动回滚到 `.bak` 备份并重启，无需干预。
- **手动**：

```bash
sudo cp /tmp/kirostudio.bak.<时间戳> /home/dwgx_user/KiroStudio/kirostudio
sudo systemctl restart kirostudio
sudo systemctl is-active kirostudio      # 确认已恢复
```

`promote` 失败时会直接把可用的回滚命令打印在终端，照抄即可。

### 5. 配置

两份文件放在服务器 `/home/dwgx_user/KiroStudio/config/`（本地开发放 `config/`），仓库里有 `config.example.json` 和 `credentials.example.*.json` 可参考。

`config/config.json`：

```json
{
  "host": "127.0.0.1",
  "port": 8990,
  "apiKey": "<客户端调用网关用的 key>",
  "adminApiKey": "<admin 后台/接口用的 key>",
  "tlsBackend": "rustls",
  "region": "us-east-1",
  "defaultEndpoint": "ide"
}
```

- `apiKey`：外部客户端（含 Claude Code）调 `/v1/*` 时带的 key。
- `adminApiKey`：admin-ui 与 `/api/admin/*` 用的 key，务必与 `apiKey` 不同。
- `tlsBackend` 固定 `rustls`（对应发布二进制的 `--no-default-features` 纯 rustls 构建）。
- 密钥请用你自己的真实值，别提交进仓库。

`config/credentials.json`：Kiro/AWS Q 的登录凭据（IdC / social / api_key 多种格式，见 `credentials.example.*.json`）。含刷新令牌，**权限必须 600**：

```bash
chmod 600 /home/dwgx_user/KiroStudio/config/credentials.json
```

