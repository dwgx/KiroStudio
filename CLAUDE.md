# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

> 面向 AI 助手的项目导航文件。每次开始新任务前先读此文件。
> **2026-10-13 复活线全量改写**：删掉 Mac/VPS 旧纪元已作废的约束（`ssh skiapi`、私有仓、多会话脏树、8GB Mac 编不过等），保留仍然成立的技术结论与教训。

---

## 第 0 步：先读 `STATUS.md`

`STATUS.md`（仓根）是**当下状态**入口（master 线、线上跑什么、待做什么）。
本文件是**长期约束**（构建、验证、部署、设计原则、历史教训）。两者都要读，顺序是 `STATUS.md` 先。

机器级板（hmc 服务与未决清单、核对命令）：`C:\Users\dwgx1\.dwgx\ops\boards\KiroStudio.md`。
历史过程稿（旧 `HANDOFF-*` / `PLAN-*` / `TRACKING-*`）价值在依据与推导过程、**不在结论**。

**更正一条断言时，要像验证原断言那样验证你的更正。**（历史事故：有人凭记忆把一条正确的事实改成错的，还写进了导航文件当依据。）

## 项目定位

KiroStudio 是 **Anthropic Messages API 兼容的反向代理网关**，Rust / Axum 编写。
接收标准 Anthropic 格式请求 → 转换为 Kiro/AWS Q 的 AWS event-stream 二进制协议 → 把响应翻译回 Anthropic SSE。

- 单端口单二进制（React 前端 `rust-embed` **编译期**内嵌）
- 上游端点：`ide`（`runtime.{region}.kiro.dev`）与 `cli`（`q.{region}.amazonaws.com`，`ksk_` API Key 号）——**两个都注册**
- 管理面板 `/admin`（React SPA），Admin API `/api/admin/*`
- 版本以 `Cargo.toml` 为准；发版闸门见 `docs/RELEASE-CHECKLIST.md`

## 仓库与推哪里（2026-10-13 起）

- **唯一 remote = `origin` = 公开仓 `dwgx/KiroStudio`**。旧私有仓 `KiroStudio-skiapi` 已删（404），不要再引用、不要再推。
- 本机工作树是**干净克隆**（不是旧多会话脏树）：常规 `git add/commit` 可用。
- 提交遵循 `CONTRIBUTING.md`：Conventional Commits（`<type>(<scope>): <中文描述>`，动词开头，无句号），一次一件事，**不写 AI 署名**。
- push 前跑三条检查并在回话里贴输出：① `git status --porcelain` 干净 ② `git fetch` 后 `git log --oneline @{u}..HEAD` 是本波产物 ③ `git diff --stat @{u}...HEAD` 无夹带。对上就推。
- 分支：`master` 是唯一主线（2026-10-13 合流：repo-kit 工具 + v1.2.2–4 产品线）。发布走 **tag**（`release.yml`），不重打旧 tag。
- CI（`.github/workflows/`）：`ci.yml` 在**任意分支 push** 上跑（`cargo test --no-default-features`）；`release.yml` 打 tag 出四端产物 + sha256；`deploy-build.yml` 手动触发；`banner.yml` 定时重生 banner。

## 仓库结构

```
src/
├── main.rs                   入口：初始化 + 优雅停机 + config 防呆引导
├── model/
│   ├── config.rs             全局配置结构体（Config，serde camelCase）
│   └── arg.rs                CLI 参数（--config / --credentials）
├── common/
│   ├── security.rs           IP白名单/黑名单/每-IP限流/XFF最右段
│   ├── ssrf.rs               出站URL SSRF防护 + DNS固定
│   ├── auth.rs               API Key提取 + constant_time_eq
│   ├── auth_keys.rs          API Key 体系
│   ├── fs_atomic.rs          原子写文件（temp→fsync→rename）
│   ├── health_marker.rs      OTA健康标记/crashloop回滚
│   ├── log_buffer.rs         内存环形日志缓冲（面板实时日志）
│   ├── recovery_metrics.rs   自愈可观测性计数器
│   └── secret_store.rs       credentials at-rest XChaCha20-Poly1305加密
├── kiro/
│   ├── token_manager.rs      ★ 多凭据管理+选号+刷新+亲和（最大文件）
│   ├── provider.rs           核心代理：重试/故障转移/Client缓存/动态重试预算
│   ├── health.rs             AIMD熔断器 + EWMA健康分 + 族级连坐
│   ├── cooldown.rs           8种冷却原因 + 差异化时长
│   ├── rate_limiter.rs       每日/最小间隔/退避/抖动
│   ├── scheduling.rs         InflightGuard(RAII) + RpmTracker(60s滑窗)
│   ├── affinity.rs           会话亲和（session_id→credential_id，TTL）
│   ├── machine_id.rs         machineId生成/派生/撞车轮换
│   ├── overage.rs            超额开关（幂等+审计）
│   ├── web_portal.rs         app.kiro.dev Web Portal客户端（rpc-v2-cbor）
│   ├── refresh_loop.rs       受管后台预刷新任务
│   ├── throttle.rs           入站整形令牌桶（AIMD RPM自动挡）
│   ├── regions.rs            AWS region白名单
│   ├── diagnosis.rs          凭据诊断
│   ├── passthrough.rs        透传模式（custom_api 池，逐字节转发）
│   ├── auth/                 social.rs（OAuth PKCE）/ idc.rs（设备码）/ mod.rs
│   ├── endpoint/             ide.rs / cli.rs / mod.rs（KiroEndpoint trait；两个都注册）
│   ├── parser/               AWS event-stream二进制解码（双CRC32）
│   └── model/                凭据/请求/事件/用量限制数据结构
├── anthropic/
│   ├── converter.rs          ★ Anthropic→Kiro格式转换 + 环境噪音剥离
│   ├── stream.rs             ★ Kiro events→Anthropic SSE（流式+缓冲双ctx）
│   ├── handlers.rs           请求入口+流式/非流式+WebSearch分派
│   ├── websearch.rs          MCP WebSearch
│   ├── compressor.rs         输入压缩（空白折叠+tool_result头尾截断）
│   ├── model_catalog.rs      ★ 模型目录单一真相源（static CATALOG）
│   ├── tool_compat.rs        工具名缩短（63 字节上限）/ schema 兼容
│   ├── middleware.rs         认证中间件
│   ├── router.rs             路由装配
│   └── types.rs              Anthropic API数据类型
├── admin/
│   ├── service.rs            业务逻辑核心（凭据CRUD/余额/配置/OTA任务）
│   ├── handlers.rs           HTTP处理器
│   ├── router.rs             Admin路由装配
│   ├── update.rs             OTA自更新
│   ├── usage_handlers.rs     用量查询+insights+SSE stream/live
│   ├── social_login.rs / idc_login.rs / external_idp_login.rs   上号入口
│   └── middleware.rs         Admin鉴权中间件
├── usage/
│   ├── usage_stats.rs        JSONL+内存预聚合+设备/客户端识别
│   ├── record.rs             RequestRecord数据契约
│   ├── trace_db.rs           SQLite逐条持久化（rusqlite bundled）
│   └── pipeline.rs           异步管道（专用OS线程+SyncSender）
├── admin_ui/                 rust-embed React SPA 路由
├── openai/                   OpenAI兼容层（/v1/chat/completions、/v1/responses）
├── token.rs                  Token计算（本地估算+远程API）
├── http_client.rs            ProxyConfig + reqwest Client构建
├── tray.rs                   Windows系统托盘（仅Windows编译）
└── debug.rs                  调试工具（孤儿文件，不参与编译）

admin-ui/                     React + Vite + Tailwind前端（编译期embed进二进制）
docs/                         架构/协议/部署文档（索引 docs/README.md）
deploy/                       systemd/bluegreen/rollback脚本（旧纪元，部署以 HMC 为准）
```

## 关键设计原则

1. **单二进制** — 前端 rust-embed 内嵌，拷贝即跑
2. **原子选号** — 选号+inflight+1+rpm.record 在同一把 `parking_lot::Mutex` 临界区内完成，根治并发惊群
3. **双路径刷新** — 预刷新（后台定时）+ 按需刷新（请求热路径），每凭据独立刷新锁防重复刷
4. **三层热重载** — TIER1(ArcSwap原子镜像) / TIER2(后台任务abort+respawn) / TIER3(进程级Atomic镜像)
5. **用量管道用OS线程** — SQLite/fsync阻塞IO不跑在tokio worker上，try_send非阻塞入队
6. **族级连坐** — M365/AWS同租户账号共享 `family_key`，一号风控整族退避
7. **模型目录单一真相源** — `src/anthropic/model_catalog.rs` 的 `static CATALOG`：新增/改名/调倍率/调窗口**只改这一处**（`/v1/models`、探测清单、context window、映射全从它派生）；改它必须同步同文件守卫测试。未知模型**显式拒绝**，不静默降级。

## 配置文件

- `config.json` — 主配置（serde camelCase；首次启动自动生成强随机密钥并落盘 600）
- `credentials.json` — 凭据（单对象或数组格式，可选 at-rest 加密）
- 监听：容器/服务器部署必须 `"host": "0.0.0.0"`（`127.0.0.1` 收不到 docker 端口映射）；端口默认 `8990`
- `usageDataDir` 默认 `data/usage`（相对进程 cwd）——容器里指向已挂载目录（如 `/app/config/usage`），否则重建容器数据全丢
- 健康：`/healthz`（JSON：`ok` / `version` / `build_sha` / `pool_count` / `sqlite_writable`）

## 开发约定

### Rust 风格
- 注释密度：中文注释为主，关键算法行内注释
- 错误处理：`anyhow::Result` for 业务逻辑，自定义 error types for HTTP响应
- 异步：`tokio::spawn` + `Arc<Mutex>`（parking_lot for sync，tokio::Mutex for async IO）
- 配置热重载字段必须同步更新三层热重载镜像

### 前端（admin-ui）
- React + TypeScript + Tailwind CSS + Vite；`pnpm` 包管理（别用 npm，会生成冲突锁文件）
- 编译后通过 `rust-embed` 嵌入二进制

### 构建与测试

```bash
# ⚠️ 必须先构建前端：rust-embed 是**编译期**嵌入 admin-ui/dist，缺 dist 时 cargo 直接报
#    E0599 "no associated function named `get` found for struct `Asset`"（不是友好报错）。
#    dist 在 .gitignore 里，fresh clone 后第一件事就是这句。
cd admin-ui && pnpm install --frozen-lockfile && pnpm build && cd ..

# ⚠️ 一律加 --no-default-features：Cargo.toml 的 default = ["native-tls"] 与出厂配置**相反**。
#    CI/Docker/出厂构建全部用 --no-default-features（纯 rustls）。
cargo build --release --no-default-features   # 生产构建（单二进制）
cargo test --no-default-features              # 全部测试（仅内联 #[cfg(test)]）
cargo test --no-default-features <name>       # 运行单个测试（按名称过滤）
cargo clippy --no-default-features && cargo fmt   # 提交前静态检查 + 格式化
cargo run -- -c config.json --credentials credentials.json   # 本地运行
cd admin-ui && pnpm dev                       # 前端热更开发（改前端后需 pnpm build 才进二进制）
```

> `src/test.rs` 与 `src/debug.rs` 不参与编译（`main.rs` 的 mod 列表里没有它们，已与现有 API 脱节）。
> 全部测试都是各源文件内联的 `#[cfg(test)]` 模块；改测试请改对应源文件，别动 `src/test.rs`。
> Rust 2024 edition。

### 验证在哪跑（2026-10-13 起）

- **GitHub CI（首选）**：push 任意分支 → `ci.yml` 跑全量测试，红绿可查。
- **HMC Docker**：`ssh hmc`（机器规矩见 `E:\dwgxhomecloud\AGENTS.md`）。用仓库 `Dockerfile` 的 `builder` target 构建 + `docker run` 显式跑 `cargo test --no-default-features`；判定标准是 `test result: ok. NNNN passed; 0 failed`（退出码来自 cargo 本身）。
- **本机 Windows**：rustc 1.97.1（=`rust-toolchain.toml` pin）/ node 24 / pnpm 11.7，可构建（先 `pnpm build`）。Owner 定的节奏：本机验证循环后置，编译优先 hmc/CI。
- 旧文档的「本机 8GB 编不过 / 必须走 skiapi 服务器 Docker 验证循环」是 Mac 纪元约束，**已作废**。

## 部署（2026-10-13 起）

- 生产 = **HMC**（`dwgxhomecloud`，`192.168.11.4`）工作区 `kirostudio`：
  - 镜像 `dwgx/kirostudio:<tag>`（alpine + GitHub Release 二进制，不从源码编译）；
    **部署/升级 = `E:\dwgxhomecloud\ops\kirostudio\deploy.sh`**（在 HMC 上跑；幂等；已存在的 config 不覆盖）。
    升级改两处：`ops/kirostudio/workspace.yml` 的 image tag + `deploy.sh` 的 VERSION。
  - 状态 `workspace/active/kirostudio/data/`（config.json / credentials.json / usage），经 `ws` 的 `mount_rw:` 读写挂进容器 `/app/config`；`ws up` 重建容器不丢。
  - 健康：`ssh hmc "curl -s http://127.0.0.1:18001/healthz"` → `{"ok":true,...}`；面板 `http://127.0.0.1:18001/admin`。
- 部署机规矩：`E:\dwgxhomecloud` 是唯一写手（机器副本只快进）；凭据值只进 `E:\dwgxhomecloud-vault`，仓库/机器/会话都不存值。
- 旧 VPS/skiapi 部署线（`ssh skiapi`、hotswap、dist bind mount）**已作废**。
- newapi 接入（后续）：newapi 是容器，`bind:` 需加 `172.17.0.1` 或共用网络；密钥取 vault `kirostudio.md`。动 newapi 要 Owner 当轮点头。

## 排查时的坑（每条都踩过，别重蹈）

1. **`strings` 查编译产物验证改动是否上线 —— 不可靠**（编译优化掉字面量）。验证改动看已部署快照源码（`git show <tag>:<file>`）或行为。
2. **本机 python 数括号平衡找语法错 —— 不可靠**（char 字面量/生命周期误判）。让 rustc 报行号。
3. **`git diff --stat <snapshot>` 对未跟踪文件显示为「删除」**。文件在本地且被引用就正常，别当真删除去「恢复」。
4. **测 Kiro 路径会被 custom_api 代挂优先分流**（`should_try_custom_api_first`）：打 `/v1/messages` 测 WebSearch/Kiro 行为时，请求可能实际走代挂。要测 Kiro 路径须先禁用代挂。
5. **workflow agent 的代码必须过 CI 才能信**（历史：报告自称「逻辑自洽」的代码被 CI 抓出 5 类真实缺陷：raw string 提前闭合、文档注释用在参数上、cap 触发后死循环、截断超出契约、测试 helper 喂错类型）。
6. **测试绿 ≠ 守卫有效**：写完守卫必须实测「删掉目标它会不会红」；破坏实验要**类型等价**（直接删字段会引入编译错误，测不到守卫本身）。
7. **守卫用「找某标记第一次出现的位置」切分生产代码区时，注释里不能出现该标记的字面量**（命中注释会让切分点提前、生产区被截断，守卫**静默变绿**）。
8. **改客户端可见的状态码/文案前，先 grep 仓外消费者**（仓库内做对了 ≠ 链路上做对了；历史：外挂按 body 文案分类，文案里没 marker 时我们算的 Retry-After 被整个丢弃）。
9. **配置值上限 ≠ 实际放大**：读到一个上限就推算后果 = 算空气；先确认**它所在的那条分支实际有没有被走到**，再谈它的值。

## 关键架构结论（历史评审确立，别推翻）

- **ksk region/端点**：单一 region 真相源（`credentials.rs`）+ 按区纠错自愈 + 含 region 的 bucket_key。**不要做 K2CC 式物化重写**（会亲手制造恒 403 + 桶 key 双斜杠漂移）。
- **deepseek 归一化**：白名单感知 —— 选号与改写共用 `effective_model(raw, cfg, whitelist)`；改任何一处都要同步另一处，否则「选中了但改写后必失败」。
- **模型映射**：全局扁平 map + 每凭据豁免；用量记原始名 + 映射后名两维度；先映射再 deepseek 归一化。
- **P1 移植甄别（2026-08-11，对 Kiro-RS-Tool @795b9ca）**：6 项本仓已是超集（工具适配 / Write-Edit-Read / 半截 JSON / XML 泄漏过滤 / profileArn / 缓存方向），**不移植**；已移植 2 项：
  - **native effort 映射**（`native_thinking_effort_enabled`，**默认关**）：往上游注 `additionalModelRequestFields.output_config.effort`；白名单 4 模型；守卫钉住白名单 ⊆ `model_catalog`。默认关是刻意的。
  - **缓存 fingerprint 模拟器**（`anthropic/cache_fingerprint.rs`）：纯内存（无持久化/后台线程），最长公共前缀命中 + 会话隔离（种子=完整 user_id）；签名剔除工具块漂移 id + JSON canonicalize。

## 相关文档

`docs/README.md` 是索引。核心：`ARCHITECTURE.md`（系统架构）/ `PROTOCOL.md`（上游协议细节）/ `MODULES.md`（模块说明）/ `INVALID-TOOL-PARAMETERS.md`（工具参数修复原理）/ `UI-COMPONENTS.md`。
⚠️ 行号与「唯一端点」类断言会过期（历史实测：行数差 2.5 倍、ide/cli 两个端点都注册）——以代码为准。
