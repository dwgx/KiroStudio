<div align="center">

# KiroStudio

**高性能 Anthropic 协议网关 —— 把 Anthropic Messages 请求转发到 Kiro / AWS Q，并附带一套现代化管理面板。**

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](./LICENSE)
[![Rust](https://img.shields.io/badge/Rust-2024-orange.svg)](https://www.rust-lang.org/)
[![Build](https://img.shields.io/badge/build-passing-brightgreen.svg)](#从源码构建)

</div>

---

KiroStudio 用 Rust / Axum 编写，接收标准 **Anthropic Messages API** 请求，转换后转发到 **Kiro / AWS Q** 上游，并把上游响应翻译回 Anthropic 格式。任何兼容 Anthropic 协议的客户端（Claude Code、各类 SDK、自研应用）都可以把 `base_url` 指向本网关直接使用。

前端管理面板（React + Vite）在编译期通过 `rust-embed` 嵌入二进制，**最终产物是单个可执行文件**，不依赖任何外部静态资源目录，拷贝即可运行。

## 致谢

本项目基于 [**hank9999/kiro.rs**](https://github.com/hank9999/kiro.rs)（MIT License）深度魔改与增强，在原项目「Anthropic ↔ Kiro 协议转换」核心之上做了大量工程化扩展。感谢原作者 [hank9999](https://github.com/hank9999) 打下的基础。

相较原项目，主要增强点：

- **多凭据智能调度** —— 负载均衡、故障转移、失败冷却、会话亲和、RPM 软限流
- **入口安全层** —— API Key 鉴权、CORS 白名单、IP 白名单（CIDR）、每-IP 限流、请求体大小限制
- **SSRF 防护** —— 出站地址校验，拦截指向内网/回环的请求
- **输入压缩管道** —— 请求体接近上游硬限制时自动压缩（空白折叠 + 超长 tool_result 智能截断）
- **多种上号方式** —— Social / IAM Identity Center (IdC) / External IdP，面板内网页上号
- **实时用量统计** —— 请求埋点、SQLite 落盘、按模型/凭据/客户端聚合、设备识别
- **现代化管理面板** —— 概览、凭据管理、用量分析、系统设置
- **一键部署** —— Docker Compose、预编译静态二进制、systemd 服务脚本

## 特性

| 能力 | 说明 |
| --- | --- |
| 协议转换 | Anthropic Messages `POST /v1/messages` ↔ Kiro / AWS Q，支持流式与非流式、工具调用、thinking 块、图片输入 |
| 多账号调度 | 多凭据负载均衡（priority / balanced 两种模式）、故障自动转移、失败冷却、会话亲和 |
| 管理面板 | React 面板内置于二进制：概览、凭据管理、用量分析、系统设置 |
| 网页上号 | 面板内完成 Social / IdC / External IdP 授权，凭据自动落库 |
| 用量统计 | 请求级埋点，按模型 / 凭据 / 客户端聚合，客户端设备识别，SQLite 落盘可保留 N 天 |
| 入口安全 | API Key 鉴权、CORS 白名单、IP 白名单、每-IP 限流、请求体大小限制、凭据日志脱敏 |
| 部署简单 | 单二进制、Docker 一键起、GitHub Release 预编译产物、systemd 脚本 |

## 快速开始

推荐用 Docker，无需本地 Rust / Node 环境。

### Docker（推荐）

```bash
git clone https://github.com/dwgx/KiroStudio.git
cd KiroStudio

# 准备配置：复制示例并改成你自己的 key
mkdir -p config
cp config.docker.example.json config/config.json
cp credentials.example.social.json config/credentials.json   # 按你的上号方式选择示例

# 起服务
docker compose up -d
```

默认映射到宿主机 **8991** 端口（容器内 8990，见 `docker-compose.yml`）。启动后访问 `http://localhost:8991/admin` 打开管理面板。

### 预编译二进制（GitHub Release）

从 [Releases](https://github.com/dwgx/KiroStudio/releases) 下载对应平台的静态二进制（Linux x86_64 为 `kirostudio-linux-x86_64`，纯 rustls、静态链接、无运行时依赖）：

```bash
# 下载并校验
curl -LO https://github.com/dwgx/KiroStudio/releases/latest/download/kirostudio-linux-x86_64
curl -LO https://github.com/dwgx/KiroStudio/releases/latest/download/kirostudio-linux-x86_64.sha256
sha256sum -c kirostudio-linux-x86_64.sha256
chmod +x kirostudio-linux-x86_64

# 准备配置后运行
./kirostudio-linux-x86_64 -c config/config.json --credentials config/credentials.json
```

### 从源码构建

需要 Rust（2024 edition）、Node 20+、pnpm 9+。前端必须先构建产出 `admin-ui/dist`，`rust-embed` 才能在编译期嵌入：

```bash
# 1. 构建前端
cd admin-ui
pnpm install --frozen-lockfile
pnpm build
cd ..

# 2. 构建后端（纯 rustls 发布构建）
cargo build --release --no-default-features

# 3. 运行
./target/release/kirostudio -c config/config.json --credentials config/credentials.json
```

## 配置

KiroStudio 读取两份文件：`config.json`（服务与安全配置）和 `credentials.json`（上游登录凭据）。默认在工作目录下查找，也可用命令行参数指定：

```bash
kirostudio -c <config.json 路径> --credentials <credentials.json 路径>
```

### config.json

最小可用配置：

```json
{
  "host": "127.0.0.1",
  "port": 8990,
  "apiKey": "sk-换成你自己的强随机-客户端密钥",
  "adminApiKey": "sk-换成你自己的强随机-管理密钥",
  "tlsBackend": "rustls",
  "region": "us-east-1",
  "defaultEndpoint": "ide"
}
```

常用字段：

| 字段 | 默认 | 说明 |
| --- | --- | --- |
| `host` | `127.0.0.1` | 监听地址。Docker/对外暴露时设 `0.0.0.0` |
| `port` | `8080` | 监听端口。**自定义端口改这里** |
| `apiKey` | 无（必填） | 客户端调用 `/v1/*` 时携带的密钥。为空会拒绝启动，避免无鉴权 |
| `adminApiKey` | 无 | 管理面板 / `/api/admin/*` 的密钥，**务必与 `apiKey` 不同** |
| `region` | `us-east-1` | 上游区域，可用 `authRegion` / `apiRegion` 分别覆盖 |
| `tlsBackend` | `rustls` | TLS 后端，发布二进制固定 `rustls` |
| `defaultEndpoint` | `ide` | 凭据未显式指定 endpoint 时使用的默认端点 |
| `loadBalancingMode` | `priority` | 多凭据调度模式：`priority`（按优先级）或 `balanced`（均衡） |
| `proxyUrl` | 无 | 出站代理，支持 `http://` / `https://` / `socks5://` |

安全相关（对外部署建议开启）：

| 字段 | 默认 | 说明 |
| --- | --- | --- |
| `corsAllowedOrigins` | `[]`（任意） | CORS 允许来源列表，非空时仅回显命中的 Origin |
| `ipAllowlist` | `[]`（不限） | 入口 IP 白名单，支持 IPv4/IPv6 CIDR，如 `["10.0.0.0/8"]` |
| `trustForwardedHeader` | `false` | 是否信任 `X-Forwarded-For`，**仅在可信反代之后才可开** |
| `ingressRateLimitPerMin` | `0`（不限） | 每-IP 每分钟最大请求数，超限返回 429 |
| `maxBodyBytes` | `52428800`（50 MiB） | 请求体最大字节数 |

调度与用量：

| 字段 | 默认 | 说明 |
| --- | --- | --- |
| `cooldownEnabled` | `true` | 凭据出错后短暂跳过（失败冷却） |
| `affinityEnabled` | `true` | 会话亲和，同一会话尽量复用同一凭据（balanced 下生效） |
| `credentialRpmLimit` | `0`（不限） | 每凭据 RPM 软上限，达到后降权而非硬跳过 |
| `usageEnabled` | `true` | 用量统计埋点与落盘 |
| `usageDataDir` | `data/usage` | 用量数据目录 |
| `usageRetentionDays` | `30` | 用量明细保留天数 |

> 完整字段与默认值以 `src/model/config.rs` 为准；未列出的字段均有安全的内置默认值。

### credentials.json

上游登录凭据，支持单对象或数组（多凭据）两种格式。凭据含刷新令牌，**权限务必收紧为 600**：

```bash
chmod 600 config/credentials.json
```

仓库提供多份示例，按你的上号方式选用：

- `credentials.example.social.json` —— Social 登录
- `credentials.example.idc.json` —— IAM Identity Center (IdC)
- `credentials.example.apikey.json` —— Kiro API Key
- `credentials.example.multiple.json` —— 多凭据数组（含 `priority` / `disabled` / `endpoint`）

也可以不手写凭据文件，直接在管理面板里[网页上号](#上号)。

## 使用引导

### 打开管理面板

浏览器访问 `http://<host>:<port>/admin`，用 `adminApiKey` 登录。面板包含四个主要区域：

- **概览** —— 服务状态、凭据健康、实时请求速率与用量总览
- **凭据** —— 查看/添加/禁用/删除凭据、优先级、余额、故障计数、导出
- **用量** —— 按时间、模型、凭据、客户端维度的用量分析与设备识别
- **设置** —— 在线调整服务配置

### 上号

进入「凭据」页，点击添加凭据，选择上号方式：

- **Social** —— 走浏览器 OAuth 授权，面板轮询完成后自动落库
- **IdC（IAM Identity Center）** —— 填入 IdC 参数完成设备授权流程
- **External IdP** —— 外部身份提供方登录

Docker / 服务器部署时，若浏览器无法直连后端本机回调端口，请在 `config.json` 设置 `callbackBaseUrl` 为可公网访问的地址（如 `https://kiro.example.com`），网页回调会打到 `{callbackBaseUrl}/api/admin/auth/callback`。

### 客户端接入

任何 Anthropic 协议客户端把 base URL 指向本网关、API Key 用 `config.json` 里的 `apiKey` 即可。

以 **Claude Code** 为例：

```bash
export ANTHROPIC_BASE_URL="http://localhost:8990"
export ANTHROPIC_API_KEY="sk-你的-apiKey"
claude
```

直接调用 API：

```bash
# 列出模型
curl http://localhost:8990/v1/models \
  -H "x-api-key: sk-你的-apiKey"

# 创建消息
curl http://localhost:8990/v1/messages \
  -H "x-api-key: sk-你的-apiKey" \
  -H "content-type: application/json" \
  -d '{
    "model": "claude-sonnet-4-20250514",
    "max_tokens": 1024,
    "messages": [{"role": "user", "content": "你好"}]
  }'
```

主要端点：

| 端点 | 说明 |
| --- | --- |
| `GET /v1/models` | 可用模型列表 |
| `POST /v1/messages` | 创建消息（流式 / 非流式） |
| `POST /v1/messages/count_tokens` | 计算 token 数 |
| `POST /cc/v1/messages` | Claude Code 兼容端点（流式时机略有差异） |
| `GET /admin` | 管理面板 |

认证支持 `x-api-key` 头或 `Authorization: Bearer <token>` 头。

## 目录结构

```
KiroStudio/
├── src/
│   ├── main.rs            # 入口：加载配置/凭据，装配路由与后台任务
│   ├── model/             # 配置与命令行参数模型
│   ├── anthropic/         # Anthropic 协议入站：路由、鉴权、handlers、流式
│   ├── kiro/              # Kiro/AWS Q 上游：协议转换、凭据、token 管理、调度
│   ├── admin/             # 管理 API：凭据管理、上号、用量查询、配置
│   ├── admin_ui/          # 面板静态资源服务（rust-embed 嵌入 dist）
│   ├── usage/             # 用量埋点、聚合、SQLite/JSONL 落盘
│   └── common/            # 安全（CORS/IP/限流）、SSRF 防护等公共组件
├── admin-ui/              # React + Vite 管理面板前端
├── deploy/                # systemd 安装 / 部署 / 蓝绿脚本
├── Dockerfile
├── docker-compose.yml
└── config.example.json    # 配置示例
```

## 开发

```bash
# 后端
cargo run -- -c config/config.json --credentials config/credentials.json
cargo test                                   # 运行测试

# 前端（独立热更，代理到后端；改完 pnpm build 才会被嵌入）
cd admin-ui
pnpm install
pnpm dev
```

参与贡献请阅读 [CONTRIBUTING.md](./CONTRIBUTING.md)。

## License

本项目基于 [MIT License](./LICENSE) 开源，Copyright (c) 2026 dwgx。

衍生自 [hank9999/kiro.rs](https://github.com/hank9999/kiro.rs)（MIT License, Copyright (c) 2026 hank9999），原始许可声明一并保留于 [LICENSE](./LICENSE) 文件中。

