# 生态调研索引（research）

> 2026-06-30 调研。给 KiroStudio 选地基用的参考项目实测情况。

## 谱系图
```
kiro2api / proxycast (灵感源)
      └─ hank9999/kiro.rs ★主干 1.7k (MIT, Anthropic兼容)
            ├─ ZyphrZero/kiro.rs 211★  (链路追踪/429冷却/健康检查/在线更新)
            └─ BenedictKing 88★ ─ M-JYuan 88★ ─ Foxfishc 33★
               (这条线专攻韧性: exclude_ids/雷暴防护/Overage实时)

Quorinex/Kiro-Go 956★ (Go, MIT, OpenAI+Anthropic 双协议)  —— 独立另一支
```

## 项目速查表
| 项目 | ★ | 语言/框架 | 类型 | 协议 | License | 取什么 |
|---|---|---|---|---|---|---|
| hank9999/kiro.rs | 1.7k | Rust+Axum | 网关 | Anthropic | MIT | **引擎主干** |
| ZyphrZero/kiro.rs | 211 | Rust+Axum | 网关 | Anthropic | MIT | 追踪/健康检查/在线更新 |
| M-JYuan/kiro.rs | 88 | Rust | 网关 | Anthropic | MIT | exclude_ids/雷暴防护/balance刷新 |
| Foxfishc/kiro.rs | 33 | Rust | 网关 | Anthropic | MIT | Overage实时/thinking增强 |
| BenedictKing/kiro.rs | 88 | Rust | 网关 | Anthropic | MIT | 凭据级代理/多级Region/可切TLS |
| Quorinex/Kiro-Go | 956 | Go | 网关 | OpenAI+Anthropic | MIT | 双协议出口思路 |
| farion1231/cc-switch | 111k | Tauri+Rust | 配置切换器 | - | MIT | **架构典范(SSOT/原子写/分层)** |
| chaogei/Kiro-account-manager | 1.3k | Electron | 账号GUI | OpenAI/Claude/Gemini | AGPL | 功能最全GUI思路(机器码/双向同步/批量) |
| hj01857655/kiro-account-manager | 1.9k | Tauri | 账号GUI | - | CC-BY-NC-SA | 我们fork过,有gateway雏形 |
| jlcodes99/cockpit-tools | 12.3k | Tauri+Go | 通用GUI | 多 | CC-BY-NC-SA | 多开实例隔离思路 |
| hamflx/cursor-reset | 955 | Shell/PS | 机器码重置 | - | - | telemetry全字段重置模式 |

## 关键技术情报
> ⚠️ 形态已转向"纯服务端"：机器码改为**网关注入凭据级 machineId**，不改本地。
> 下面 cursor-reset 一段仅作**历史调研参考**，KiroStudio **不做**本地 telemetry/注册表重置。

**cursor-reset 的完整设备指纹重置（仅历史参考，本项目不采用）**：
- Win 注册表 `HKLM\SOFTWARE\Microsoft\Cryptography\MachineGuid`
- `%APPDATA%\Cursor\User\globalStorage\storage.json` 四字段：
  `telemetry.machineId`(64hex) / `telemetry.macMachineId`(uuid) / `telemetry.devDeviceId`(guid) / `telemetry.sqmId`(大写带括号guid)
- 改前自动备份带时间戳、等进程退出、UTF-8无BOM+LF写回、保留只读属性
- ⚠️ Kiro IDE 的对应存储路径/字段名**尚未确认**，是阶段三前置研究任务

**cc-switch 架构（必抄）**：SSOT 单一数据源 `~/.cc-switch/cc-switch.db` SQLite；双层存储(SQLite可同步 + JSON设备级)；原子写(临时文件+rename)；Mutex并发；
分层 Commands→Services→DAO→Database；云同步走 Dropbox/OneDrive/iCloud/WebDAV。

**★ Kiro 上号机制 = AWS SSO/OIDC device authorization flow（已验证，阶段一关键）**：
- hank9999 **没有** OAuth 网页上号端点（搜 oauth/authorize/callback 命中 0），添加凭据全靠手动粘 token / 批量导入。
- 真实流程在 **Quorinex/Kiro-Go `auth/sso_token.go`**（Go，MIT，可学思路重写 Rust）：
  - 端点：`https://oidc.<region>.amazonaws.com`（device_authorization / token）+ `https://portal.sso.us-east-1.amazonaws.com`（session/device）+ start url `https://view.awsapps.com/start`
  - 步骤：registerDeviceClient(注册 device_code+refresh grant) → startDeviceAuth(拿 deviceCode/userCode) → getDeviceSessionToken → acceptUserCode → approveAuth → pollForToken(轮询取 access/refresh)
  - grantTypes: `urn:ietf:params:oauth:grant-type:device_code` + `refresh_token`
- ⚠️ `ImportFromSsoToken` 入参含 `bearerToken`——自动上号仍需先有 AWS bearer token 起步。**纯浏览器 OAuth 跳转（零手动起步）能否做到未验证**，阶段一专门验证。
- 相关参考文件：`Quorinex__Kiro-Go/auth/{sso_token,builderid,oidc,iam_sso}.go`

> 更详尽内容见 Claude 记忆库 kiro-ecosystem-projects-2026 / kiro-tools-landscape-2026。
