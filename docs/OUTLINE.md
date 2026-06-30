# KiroStudio —— 项目大纲（草案 v0.4）

> 状态：规划中 · 形态 = **自用单实例网关 + 网页后台 + 本地 SQLite**（Docker 部署在 Linux）
> 地基 = hank9999/kiro.rs（Axum 0.8 + Tokio，MIT）
> v0.4 重大简化：自用单人单设备 → **去掉 Supabase、去掉多租户**，云端能力暂不做。

---

## 0. 一句话定位

一个**自用**的 Kiro 综合网关：把 Kiro 后端转成 Anthropic（先）/ OpenAI（后）兼容接口，
带账号池、最强韧性、机器码注入、凭据级代理，全部在**单管理密码登录的网页后台**里管，
**本地 SQLite 存储**，**Docker 跑在自己的 Linux 服务器**上。无多租户、无第三方云。

## 1. 目标与非目标

### 目标
- 全协议网关：Anthropic 先扎实，再加 OpenAI（+Gemini 可选）
- 账号池：多源导入 / priority·balanced 负载均衡 / 自动切号 / 配额监控
- 最强韧性：合并 hank9999 + M-JYuan + Foxfishc 三家改进（exclude_ids / 雷暴防护 / 429冷却 / Overage实时）
- 机器码：凭据级 `machineId` 注入上游，每账号固定，网页后台管 + 变更追踪
- 代理：全局 + 凭据级单独代理 + 检测/健康检查 + 代理池
- 凭据安全：本地 SQLite + at-rest 加密（主密钥 env 注入、不落库）—— 补上全行业都缺的一环
- 网页后台：单管理密码登录，账号/机器码/代理/配额/追踪全可视化管理
- Docker 一键部署在 Linux（Debian/Ubuntu）

### 非目标（已砍）
- ❌ **Supabase / 任何第三方 BaaS**（自用不需要）
- ❌ **多租户 / 用户注册 / RLS 隔离**（单用户，一个管理密码即可）
- ❌ **本地桌面端 / 改本地 IDE telemetry / Win 注册表**（机器码改服务端注入）
- ⚠️ 云端同步、批量注册：暂不做，未来想做再议

完整功能清单见 **FEATURES.md**。

## 2. 许可证策略（已决）
- 地基 **hank9999/kiro.rs（MIT）**，移植须保留版权与致谢。
- 借鉴 AGPL(chaogei)/CC-BY-NC-SA(hj01857655/cockpit-tools) 只学思路、不抄代码。
- 参考库在 `reference/`，抄码前查 License（见 reference/README.md）。

## 3. 总体架构（自用单实例）

```
   AI 客户端 (Claude Code 等)            你的浏览器
   指向网关 URL，带网关 key              网页后台 (React, 扩展 hank9999 admin-ui)
            │                            单管理密码登录
            │                            账号池·机器码·代理·配额·追踪
            │                                     │
            │                            ┌────────┴─────────┐
            │                            │  本地 SQLite      │
            │                            │  凭据(at-rest加密) │
            │                            │  机器码·代理·日志   │
            │                            └────────┬─────────┘
   ┌────────▼─────────────────────────────────────▼────────┐
   │  KiroStudio 网关 (Rust + Axum 0.8, hank9999 核)         │
   │  /v1 /cc/v1 (Anthropic) → 协议转换                      │
   │  账号池调度 · 韧性合并 · token刷新回写                   │
   │  机器码注入(凭据级) · 凭据级代理出口 · 链路追踪          │
   └────────────────────────┬───────────────────────────────┘
              注入 machineId + 走指定代理出口
   ┌────────────────────────▼───────────────────────────────┐
   │  Kiro 上游 (AWS / Kiro 后端)                             │
   └───────────────────────────────────────────────────────────┘

   全部打包进单个 Docker 镜像，部署在你的 Linux 服务器。
```

## 4. 阶段规划（简化后三阶段）

### 阶段一：网关核心跑通（Anthropic）
- [ ] clone 已就位（reference/hank9999__kiro.rs），研读 Anthropic 转换链路并画出
- [ ] 起 KiroStudio 骨架（基于 hank9999，保留其 src/kiro + src/admin + admin-ui 分层）
- [ ] Anthropic 出口（`/v1/messages` + `/cc/v1`）跑通
- [ ] 账号池 priority/balanced + token 自动刷新回写 + 多级 Region
- [ ] 机器码注入 + 凭据级代理（hank9999 已有，验证并保留）
- [ ] ★ **网页 OAuth 上 Kiro 号**（hank9999 无，新建）：参考 Quorinex/Kiro-Go `auth/sso_token.go` 的 AWS SSO/OIDC device flow，重写成 Rust。**先验证纯浏览器 OAuth 跳转能否做到**（见 DISCUSSION Q9）
- [ ] 韧性合并：cherry-pick M-JYuan(exclude_ids/雷暴防护/balance刷新) + Foxfishc(Overage/429) + ZyphrZero(traces.db)
- [ ] 本地 SQLite 存储落地（凭据从 credentials.json → SQLite，预留加密接口）
- [ ] Docker 化：Dockerfile + docker-compose，能在 Linux 跑起来

### 阶段二：网页后台 + 安全加固
- [ ] 扩展 hank9999 admin-ui：单管理密码登录页
- [ ] 账号池 CRUD / 机器码管理（含变更追踪）/ 代理管理 / 配额面板 / 多源导入
- [ ] 凭据 at-rest 加密落地（主密钥 env 注入）
- [ ] 反代安全：timing-safe key 比较 / 可选 IP 白名单 / 限流 / body 限制

### 阶段三：增值
- [ ] OpenAI 出口（`/v1/chat/completions`，吸收 Quorinex/Kiro-Go 思路）
- [ ] 代理检测 / 健康检查 / 代理池
- [ ] 集中追踪面板：机器码变更日志、代理变更、用量统计
- [ ] （可选）Gemini 兼容、备份导出

## 5. 决策点（见 DISCUSSION.md）
Q1 ✅ hank9999 · Q2 ✅ 先Anthropic · Q6 ✅ **本地SQLite(去Supabase)** · Q7 ✅ 自用单实例+at-rest加密 · Q8 ✅ 机器码凭据级字段
- 形态 ✅ 自用单人单设备 · 部署 ✅ Docker on Linux · 多租户 ✅ 不做

## 6. 风险与边界
- **公网暴露**：跑在服务器上若开公网口，网关 key + 管理密码是唯一门 → 必须强随机、timing-safe 比较、建议加 IP 白名单/限流。
- **凭据安全**：本地 SQLite + at-rest 加密；主密钥不落库、不进 git。
- **Kiro/AWS ToS**：账号池/机器码注入/代理可能踩条款 —— 自用研究，文档声明。
- **上游漂移**：Kiro 后端协议变动会击穿转换层，需可配置 + 跟随参考库各 fork 更新。
- **单点**：单实例无冗余，自用可接受；数据备份靠定期导出 SQLite。
