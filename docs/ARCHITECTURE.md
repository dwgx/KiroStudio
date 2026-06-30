# KiroStudio 架构与功能设计（v1）

> 目标：研究现行可用的 fork 源码，设计并搭建完整的网页前端 + 后端 + 功能 + 设置。
> 基线 hank9999（成熟、MIT），按价值从三个 fork 吸收增量。本文是实现蓝图。

## 0. 设计原则
- **单实例自用优先**：默认配置面向"一个人 + 若干 Kiro 账号 + Docker 部署"，而非多租户 SaaS。
- **每项独立移植 + 编译/测试验证**，不盲目全合。
- **需验证外部行为的（上游格式）先验证再接线**，不照搬"假装在工作"的半成品。
- **后端 Rust/Axum 单二进制 + 内嵌前端（rust-embed）**，一个容器跑起来。

## 1. 源码情报地图（已实测）

| 能力 | hank9999 基线 | M-JYuan/Foxfishc | ZyphrZero | 采用来源 |
|---|---|---|---|---|
| 凭据 CRUD / 余额 / 负载均衡 | ✅ | ✅ | ✅ | 基线 |
| 冷却 + 拟人限流 + 指纹 | ✗ | ✅(指纹半成品) | 部分 | M-JYuan（已移植冷却/限流）|
| **网页 OAuth 登录(PKCE)** | ✗ | ✗ | ✅(本地+远程回调) | **ZyphrZero** ★ |
| 统计分析(overview/timeseries/by-model/by-credential) | ✗ | ✗ | ✅ | ZyphrZero |
| client-keys(下游多密钥) | ✗ | ✗ | ✅ | ZyphrZero(阶段二) |
| groups(凭据分组) | ✗ | ✗ | ✅ | ZyphrZero(阶段三) |
| proxy-pool(代理池) | 单代理 | per-cred 代理 | 代理池+健康检查 | 按需 |
| trace-log(请求留痕) | ✗ | ✗ | ✅ | ZyphrZero(阶段二) |
| per-cred endpoint/region/proxy | ✗ | ✅ | ✅ | M-JYuan |

**关键解锁**：ZyphrZero `src/kiro/auth/social.rs` + `idc.rs` 是真能用的 PKCE OAuth，
对接真实 Kiro 端点（`app.kiro.dev` / `prod.us-east-1.auth.desktop.kiro.dev`），
且 service 层支持 `callbackBaseUrl` 远程回调模式 → **Docker 远程部署也能网页上号**。

## 2. 后端架构（Rust/Axum 单二进制）

```
main → 启动三套监听（同进程）
├── Anthropic 端点 /v1/*           （网关本体，下游 Claude Code 接这里）
├── Admin API   /api/admin/*       （管理后台 API，admin key 鉴权）
└── Admin UI    /admin             （内嵌前端 rust-embed）

src/
├── anthropic/     转换层（/v1/messages ↔ Kiro CodeWhisperer）
├── kiro/
│   ├── provider          调度 + 重试 + 故障转移
│   ├── token_manager     多凭据状态机（+ 已接 cooldown/rate_limiter）
│   ├── cooldown/rate_limiter/affinity   防关联（批次1已接2个）
│   ├── auth/             ★待移植：social.rs(PKCE) + idc.rs（网页上号）
│   ├── endpoint/         ide/cli 端点定义
│   └── parser/           Kiro 二进制帧解析
├── admin/
│   ├── router/handlers/service/types   Admin API
│   ├── usage_stats       ★待移植：统计聚合
│   └── trace_db          ★待移植：请求留痕（阶段二）
└── admin_ui/      rust-embed 内嵌前端
```

### 2.1 网页上号设计（核心新功能，移植自 ZyphrZero）
两种回调模式，前端无感切换：
- **本地模式**（不配 callbackBaseUrl）：后端起临时 TCP 端口（3128/4649…），浏览器在本机完成回调。适合本机跑。
- **远程模式**（配 `callbackBaseUrl=https://你的域名`）：公网 GET 回调路由 `/auth/callback/*` 接收 code。适合 Docker/服务器部署。★ 自用远程首选。

流程（前端视角）：
1. `POST /api/admin/auth/social/start` → 返回 `{ session_id, portal_url }`
2. 前端弹窗展示 `portal_url`（二维码/新窗口），用户浏览器登录 Kiro
3. 前端轮询 `POST /api/admin/auth/social/poll/{session_id}` → pending / done(返回凭据) / error
4. done 后凭据自动加入池并持久化

### 2.2 设置项（config.json，全部 camelCase，已有 + 规划）
```jsonc
{
  // 基础
  "host", "port", "apiKey", "adminApiKey", "region",
  "tlsBackend", "defaultEndpoint", "loadBalancingMode",
  // 防关联（批次1已加）
  "cooldownEnabled": true,        // 反应式冷却，默认开
  "rateLimitEnabled": false,      // 拟人限流，默认关（单用户会变慢）
  "rateLimitDailyMax": 500, "rateLimitMinIntervalMs": 1000,
  // 网页上号（待加）
  "callbackBaseUrl": null,        // 配了=远程回调模式
  // 统计（待加）
  "statsEnabled": true, "statsDbPath": "data/stats.db"
}
```
设置页（前端）把这些做成可视化开关/输入，热更新走 `/api/admin/config/*`。

## 3. 前端架构（React 18 + Vite + Tailwind + Radix + react-query）

沿用基线技术栈，扩展为多页（顶部 Tab 导航）：

```
admin-ui/src/
├── App.tsx                登录态 + 路由切换
├── components/
│   ├── login-page         后台单密码登录（adminApiKey）
│   ├── layout/            顶栏 + Tab 导航（仪表盘/凭据/统计/设置）
│   ├── dashboard          总览：凭据健康度、用量摘要、冷却状态
│   ├── credentials/
│   │   ├── credential-card     单凭据卡（状态/优先级/冷却/禁用/删除）
│   │   ├── add-credential      手动加（token json）
│   │   ├── social-login-dialog ★网页上号弹窗（轮询 + 二维码/链接）
│   │   ├── balance-dialog      余额/订阅
│   │   └── batch-import        批量导入
│   ├── stats/             ★图表：用量时序、按模型饼图、按凭据柱状
│   └── settings/          ★设置页：防关联开关、回调模式、负载均衡
└── api/                   axios 客户端（按域拆分）
```

图表库：ZyphrZero 用了 recharts，移植时一并引入。

## 4. 实施阶段（每阶段独立可交付 + 验证）

- **阶段 A（进行中）防关联骨架**：✅ cooldown + rate_limiter 已接线。待：affinity 接线、batch-1 收尾。
- **阶段 B 网页上号**：移植 `kiro/auth/{social,idc}.rs` + admin auth handlers + 前端 social-login-dialog。★最高价值。
- **阶段 C 统计分析**：移植 usage_stats + stats API + 前端图表页。
- **阶段 D 设置页 + per-cred 配置**：设置可视化 + per-cred endpoint/region/proxy。
- **阶段 E 运营增强（按需）**：client-keys、groups、proxy-pool、trace-log。

## 5. 风险与验证点
- **网页上号 PKCE**：ZyphrZero 已实跑，但我们要验证 `kiro_version` 等头与当前上游兼容；先在本地模式抓一次成功登录再上远程模式。
- **指纹注入**：M-JYuan 是半成品（UA 注入方法从未被调用），推迟到抓包确认上游 UA 格式后再做。
- **每移植一模块**：先 `cargo build` 零 warning + 跑该模块单测，再接线。
</parameter>
</invoke>
