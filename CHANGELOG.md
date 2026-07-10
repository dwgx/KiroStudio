# Changelog

本项目版本变更记录。遵循语义化版本(SemVer)。

## [0.5.0] - 2026-07-11

本轮聚焦**通知系统重写**与**架构文档校准**。

### UI
- **通知系统重写**(弃用 sonner,改自研 `admin-ui/src/lib/toaster.tsx`):此前多条通知并发时,
  sonner 的折叠态需靠一堆 `!important` CSS 硬掰其内部堆叠状态机,导致闪烁 / 空白灰卡 / hover 才
  显现等问题。改为极简 pub/sub store + 自绘 Toaster,完全掌控堆叠:竖直平铺、硬上限 5 条(超出丢
  最旧防刷屏堆爆)、底部倒计时进度条、hover 暂停、常驻关闭叉叉,保留右下角纯实色去光污染视觉。
  经 Vite alias + tsconfig paths 把 `sonner` 重定向到自研模块,现有全部 `toast.*` 调用点零改动。
- **号池健康通知批量合并**:同类事件(ARN 缺失/号禁用/额度耗尽/可疑活动风控)≥3 条时合并为一条
  汇总通知(标题给数量、描述列出前几个),避免号池批量出事时刷屏;1-2 条仍逐条带详细描述。

### 文档
- **`docs/ARCHITECTURE.md` / `docs/MODULES.md` 按当前代码全面校准**(用 codegraph 索引 + 源码逐一
  取证):修正代码规模(约 35,800 行)、上游端点(`runtime.{region}.kiro.dev`)、单端口 nest
  (admin 不再独立 :8992)、balanced 8 键选号 + AIMD 熔断器 + 族级连坐、动态重试预算 + 45s 墙钟、
  冷却时长现值;补全 health/compressor/overage/web_portal/health_marker/ssrf/scheduling/
  external_idp_login/update 等新模块;删除已移除的 cache_tracker 记述。

## [0.4.0] - 2026-07-10

本轮聚焦**性能、安全、上号可用性与 UI 打磨**,并规整了发布与一键部署流程。

### 性能
- **删除影子 prompt 缓存记账**:该记账在 30-40 万 token 大请求热路径同步跑 SHA256 前缀
  指纹计算,是可观固定开销且并不省钱(真正省上游 credit 的是 continuationId 确定性派生,
  未受影响)。移除后大请求慢尾从 16-31s 降到 ~6s。`promptCacheEnabled` 默认关。

### 安全(审计修复)
- **H1 OTA 完整性**:`.sha256` 校验文件改从 github.com 直连获取(独立可信信道),不再与
  二进制共用第三方镜像 —— 恶意/被劫持镜像无法再"同源投毒"绕过校验(此前构成 RCE 面)。
- **H2 XFF 伪造**:`trust_forwarded` 开启时改取 `X-Forwarded-For` **最右**可信段(而非可被
  客户端伪造的最左段),堵住绕过 IP 白名单/每-IP 限流。默认 `trustForwardedHeader=false`。
- **H3 region 注入**:凭据的 `region/auth_region/api_region` 字段过 AWS region 白名单,污染值
  不再拼进上游 host(此前可致 refresh_token 被 POST 到攻击者域名)。
- **M1 idc SSRF**:idc 上号 `region` 参数白名单校验,非法拒绝。
- 附带:修客户端可触发的 UTF-8 切片 panic、social OAuth CSRF 改 fail-closed、web_search
  补 `tool_use_id`、前端最近请求表 key 修复。

### 上号 / 凭据
- **external_idp(M365/Azure)根治**:kiro.dev 迁移后 external_idp 号必须带自己租户的真实
  profileArn,动态 ListAvailableProfiles 解析补全;余额查询改用统一 profileArn 口径,修
  external_idp 号余额显示为空的问题。

### UI
- 全站蓝色转圈圈换成贴合内容形状的**骨架屏**。
- 新增**号池健康通知**(右下角 toast):ARN 缺失/号禁用/额度耗尽/账户风控,状态跃迁提醒。
- **toast 重写**为干净扁平风(去光污染、关闭按钮清晰可见)。
- 版本字段改为**可选预设 + 自定义**(combobox);KPI 大数字**线性滚动动画**;修 KIRO PRO MAX
  订阅标签截断。

### 发布 / 部署
- 提交历史按主题拆分;`install.sh` 一键部署(Docker + 预编译二进制两条路径)防呆加固。

## 早期版本

- **0.3.x** — 上游 endpoint 迁移 kiro.dev、动态 profileArn、配置热重载三部曲、429 自适应熔断、
  M365 族级限速、per-credential RPM、OTA 回滚兜底。
- **0.2.x** — 仓库公开、历史脱敏、部署脚本 + Docker + systemd。
- **0.1.x** — 初版:多凭据聚合、Anthropic 兼容网关、管理面板。
