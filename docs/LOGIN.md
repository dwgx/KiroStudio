# 上号方案 —— 网页 OAuth 上 Kiro 号（基于真实源码，2026-06-30）

> 目标(用户)：网页后台点登录就上号，不手动粘 token。
> 依据：Quorinex/Kiro-Go `auth/sso_token.go`（已读源码，MIT，学思路重写 Rust）。
> hank9999 **无此功能**（搜 oauth/authorize/callback 命中 0），是 KiroStudio 的新建增量。

## 1. Kiro 上号真实机制 = AWS SSO/OIDC Device Authorization Flow（已验证）

端点：
- `https://oidc.<region>.amazonaws.com` —— OIDC（client注册 / device授权 / 取token）
- `https://portal.sso.us-east-1.amazonaws.com` —— SSO portal（device session）
- start url: `https://view.awsapps.com/start`

完整步骤（Kiro-Go sso_token.go 的 ImportFromSsoToken）：
```
1. registerDeviceClient   POST oidc/client/register
   → {clientName:"Kiro API Proxy", clientType:"public",
      grantTypes:[device_code, refresh_token], scopes, issuerUrl:startUrl}
   ← clientId, clientSecret
2. startDeviceAuth        POST oidc/device_authorization {clientId, clientSecret, startUrl}
   ← deviceCode, userCode, interval
3. getDeviceSessionToken  POST portal/session/device  (需 bearerToken) ← deviceSessionToken
4. acceptUserCode         POST oidc/device_authorization/accept_user_code
   (Referer: view.awsapps.com) ← deviceContext
5. approveAuth            POST oidc/.../approve {deviceContext, deviceSessionToken}
6. pollForToken           轮询 oidc/token (grant=device_code, 按 interval)
   ← accessToken, refreshToken, expiresIn  → 入库
```

## 2. ⚠️ 关键未验证点（阶段一必须先验证）
Kiro-Go 的 `ImportFromSsoToken(bearerToken, region)` **入参含 bearerToken** —— 步骤3 getDeviceSessionToken 需要它。
这说明 Kiro-Go 的"自动上号"**仍需先有一个 AWS bearer token 起步**，不是纯浏览器零起步。

**两种可能，阶段一抓包/试验确认走哪条：**
- **路线 A（标准 device flow，最可能可行）**：网页显示 userCode + 验证 URL（view.awsapps.com），
  用户在浏览器自己完成 AWS 登录授权，我们后台轮询 pollForToken 拿凭据。
  → 这是标准 OAuth device flow，**不需要我们持有 bearerToken**，步骤3/4/5可能是 Kiro-Go 走的"免交互捷径"，标准流可跳过。
- **路线 B（Kiro-Go 的免交互式）**：需要先拿到 bearerToken（从已登录的 Kiro IDE/CLI 提取），再自动跑完。
  → 仍比手动粘 access/refresh token 省事，但需要一个起步 token。

> 我的判断：**先实现路线 A（标准 device flow）**——用户点登录→显示 userCode/链接→浏览器授权→后台轮询入库。
> 这是最接近用户"网页点一下就上号"诉求、且不依赖预置 token 的方式。路线 B 作为"从现有 IDE 导入"的补充。

## 3. KiroStudio 落地设计（阶段一）
- 后端新增 `src/kiro/auth/device_flow.rs`（Rust 重写 Kiro-Go 逻辑，reqwest）
- admin API 新增端点：
  - `POST /api/admin/login/start` → 跑 register+startDeviceAuth，返回 {userCode, verificationUrl, deviceCode}
  - `POST /api/admin/login/poll` → pollForToken，成功则凭据入库(SQLite，加密)
- admin-ui 新增"登录 Kiro"对话框：显示 userCode + 可点链接 + 轮询状态
- 复用 hank9999 现有 add-credential-dialog 作为手动兜底（粘 token 仍保留）

## 4. 参考文件（reference/）
- `Quorinex__Kiro-Go/auth/sso_token.go` —— device flow 主逻辑（重点）
- `Quorinex__Kiro-Go/auth/{builderid,oidc,iam_sso}.go` —— 其他鉴权类型
- `hank9999__kiro.rs/src/kiro/token_manager.rs::refresh_token` —— 已有的 refresh 逻辑可复用
- `hank9999__kiro.rs/admin-ui/src/components/add-credential-dialog.tsx` —— 手动兜底入口
