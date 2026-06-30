# KiroStudio —— 完整功能清单（FEATURES）

> 把市面上所有 Kiro 工具的功能 + 你要的云端能力，汇总成一张大表。
> 标记：✅已有现成参考 · ★你明确要求 · ☁️云端能力(本项目新增维度) · ⚠️灰色/谨慎
> 来源缩写：H=hank9999/kiro.rs · Z=ZyphrZero · F=Foxfishc · MJ=M-JYuan · KG=Kiro-Go · CG=chaogei · CC=cc-switch · CR=cursor-reset · CKP=cockpit-tools

---

## A. 网关 / 协议转换引擎
| 功能 | 来源 | 说明 |
|---|---|---|
| Anthropic 兼容 `/v1/messages` + `/cc/v1` | H/Z/F/MJ | kiro.rs 系原生强项 |
| OpenAI 兼容 `/v1/chat/completions` `/v1/responses` | KG/CG | 双协议出口 |
| Gemini v1beta 兼容 | CG | 三协议全覆盖（可选） |
| SSE 流式 + 非流式 | 全部 | Anthropic 事件格式 |
| 工具调用 / function calling | 全部 | |
| Extended Thinking / Reasoning 转换 | H/F | opus-4.x-thinking 走 adaptive (F) |
| 内置 WebSearch 工具转换 | H/Z | |
| 图片缩放处理 | Z | |
| count_tokens / tiktoken 计量 | H/CG | |
| Prompt Cache 模拟 + TTL 记账 | Z/MJ | TTL 对齐上游真实写入(MJ) |

## B. 账号池 / 调度 / 韧性
| 功能 | 来源 | 说明 |
|---|---|---|
| 多凭据账号池 | 全部 | |
| 负载均衡 priority / balanced | H/Z | 固定优先级 / 均衡分配 |
| 智能重试(单凭据3次/单请求9次) | H | |
| ★ exclude_ids 不撞已失败凭据 | MJ/F | retry 链路强制跳过坏号 |
| ★ 新凭据「雷暴防护」 | MJ/F | recent_usage 中位数做 baseline 防 429 |
| 429 风控冷却 | Z | |
| 熔断 + 指数退避 + 配额过滤 | CG | |
| 故障转移(失败计数跳过/配额耗尽禁用) | 全部 | |
| 周期性 balance 刷新(10min)+ 余额不足主动禁用 | MJ/F | |
| Overages 在线启停(SSE实时) + Overage-aware 余额 | F | |
| credentialRpm 凭据级限流(0=真禁用) | MJ | |

## C. Token / 鉴权
| 功能 | 来源 | 说明 |
|---|---|---|
| 多鉴权:OAuth/BuilderId/Social/Enterprise-IdC/API Key | H/CG | |
| token 自动刷新 + 回写凭据(SQLite) | 全部 | |
| 双向 token 同步(监听 IDE token 反向回写) | CG | 避免被强制登出（思路，自用按需） |
| 多源导入:OAuth / Token / JSON / kiro-cli / IDE本地 | CG + 我们PR#135 | CLI 导入逻辑可复用 PR#135 |
| 客户端 API Key 分发 | Z | 给下游 AI 客户端分发可控 key |

## D. 机器码 / 防关联（★你的重点 —— 「服务端注入」模型）
> 机器码**不改用户本地**，而是作为**凭据级字段**由网关注入给 Kiro 上游。
> 已验证 hank9999 `src/kiro/machine_id.rs` 原生支持（凭据级 machineId > 全局 > 派生 > 兜底）。
| 功能 | 来源 | 说明 |
|---|---|---|
| ★ 凭据级 `machineId` 注入上游 | H 原生 | 64hex / UUID 均可，网关转发时带给 Kiro |
| ★ 每账号固定机器码 | H + 本项目 | 给该凭据存一个固定值 → 同账号永远同指纹（无本地设备概念，天然一致） |
| ★ 网页后台配机器码 | 本项目 | 存本地 SQLite，网页可改 |
| ★ 机器码变更追踪 | 本项目 | 记录每账号历史机器码、当前值、变更日志（SQLite） |
| ★ 机器码备份 + 恢复 | 本项目 | 导出/导入（自用本地备份） |
| ❌ ~~改本地 IDE telemetry / Win 注册表 MachineGuid~~ | ~~CR/fork~~ | **作废**：与无本地端形态冲突，不再需要 |
| ❌ ~~多开实例本地隔离~~ | ~~CKP~~ | **作废**：无本地端 |

## E. 代理（★你的重点）
| 功能 | 来源 | 说明 |
|---|---|---|
| ★ 全局代理(HTTP/HTTPS/SOCKS5) | H/CG/KG | |
| ★ 凭据级单独代理 | H/BK | 每账号独立代理，优先级 凭据>全局>direct |
| ★ 代理检测/健康检查 | Z思路 | 测连通性/延迟/出口IP |
| 代理池 + 轮询 | Z | |
| `direct` 强制直连选项 | H/BK | |
| 多级 Region(Auth Region / API Region 分离) | H/BK | 配合代理做地域适配 |

## F. 监控 / 可观测
| 功能 | 来源 | 说明 |
|---|---|---|
| 配额监控面板(进度条+重置时间) | CG/CKP | 每模型剩余额度 |
| SQLite 请求链路追踪 traces.db | Z | |
| 用量统计 / 成本追踪 | 全部/CC | |
| Prometheus /metrics + 审计日志 | CG | |
| 低余额自动切号 | CG | |
| 唤醒任务(定时ping提前触发配额重置周期) | CKP | ⚠️ |

## G. 账号管理后台（★ 网页，非桌面）
> 形态转向：原"桌面 GUI"全部改为**网页后台**（扩展 hank9999 的 admin-ui React 工程）。
| 功能 | 来源 | 说明 |
|---|---|---|
| ★ 网页单管理密码登录 | 本项目+H | 单用户，一个密码进后台（非多租户） |
| 多账号 CRUD / 一键切换 | H admin-ui + 全部GUI思路 | 网页操作 |
| 分组 / 标签 / 批量管理 | CG/CKP | |
| 批量导入 / 导出 | H admin-ui(已有批量/KAM导入) + CG | |
| 多语言 i18n | KG/CG | |
| ❌ ~~系统托盘快速切换~~ | ~~CC/CKP~~ | **作废**：无桌面端 |
| ⚠️ 批量账号注册(邮箱集成) | CG | 灰色，后置 |

## H. 架构 / 安全 / 存储（地基 —— 自用单实例）
> SSOT/分层思想保留，存储 = 本地 SQLite（v0.4 砍掉 Supabase/多租户）。
| 功能 | 来源 | 说明 |
|---|---|---|
| SSOT 单一数据源 | CC思想 | 落到本地 SQLite |
| 分层架构 Commands→Services→DAO | CC + H现有 | H 已有 admin: router/handlers/service/types 分层 |
| 本地 SQLite 存储 | H思路 | 凭据/机器码/代理/日志，替代 credentials.json 单文件 |
| 凭据 at-rest 加密 | 本项目 | 主密钥 env 注入、不落库，进库前加密——补 hank9999/Kiro-Go 都缺的一环 |
| ★ 单管理密码登录后台 | 本项目+H | 替代 hank9999 单 admin_api_key 直配，网页加登录页 |
| 反代安全(timing-safe key/IP名单/限流/body限) | CG + H现有 | H 已有 constant_time_eq；公网暴露必备 |
| Docker 部署(Linux Debian/Ubuntu) | H现有 | H 已有 Dockerfile + docker-compose，扩展即可 |
| 客户端分发 key | H/Z | 给下游 AI 客户端分发可控 key |
| ❌ ~~Supabase / 多租户 / RLS / JWT~~ | - | **作废**：自用单人不需要 |

---

## KiroStudio 相对现存工具的增量（自用单实例版）
形态 = **网关 + 网页后台 + 本地 SQLite + Docker(Linux)**，单用户单管理密码。相对各参考项目的增量：
1. **韧性合并** —— 把 hank9999 + M-JYuan + Foxfishc + ZyphrZero 散落各 fork 的韧性改进合到一处
   （exclude_ids / 雷暴防护 / 429冷却 / Overage实时 / traces.db）
2. **机器码服务端注入** —— 每账号固定 `machineId`（凭据级字段），网关注入上游；网页后台管 + 变更追踪。已验证 hank9999 原生支持
3. **凭据 at-rest 加密** —— 补上 hank9999/Kiro-Go 都缺的一环（主密钥 env 注入、不落库）
4. **更好的管理后台** —— 在 hank9999 admin-ui 上扩展机器码/代理/配额/追踪可视化 + 单密码登录
5. **双协议（后续）** —— Anthropic 先扎实，再加 OpenAI（吸收 Quorinex/Kiro-Go 思路）
> 注：v0.4 已砍掉云端（Supabase/多租户）。跨设备/多人若未来需要，再议是否重新引入。
> 安全边界：自用单实例，服务端=你自己的服务器，密钥在你手里；公网暴露靠网关 key + 管理密码 + IP白名单/限流。
