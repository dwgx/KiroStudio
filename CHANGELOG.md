# Changelog

本项目版本变更记录。遵循语义化版本(SemVer)。

## [0.6.3] - 2026-07-11

### 修复（关键）
- **`Connection closed mid-response` 根治**：对话路径的 HTTP client 此前用 reqwest 的 `.timeout()`
  （**整个请求生命周期总超时**，720s），覆盖读响应体全过程——对流式是致命的：一个健康但耗时长
  的大请求（opus 大 prompt / 64k max_tokens，生成可超 12 分钟）会在**流中途被硬掐**，上游流没读完
  就断、我方 SSE 随之断裂，下游报 `Connection closed mid-response` 并疯狂重试。新增
  `build_streaming_client` 改用 **`read_timeout`（两次数据之间的空闲间隔上限）+ connect_timeout**，
  只要上游持续吐 token 流就永不被掐，只有真卡死才中断。仅换对话路径两个 client，其它一次性请求
  （auth/token/探测/count）保留总超时不变。
- **模型探测请求体修正**：探测此前用手搓的最小请求体（缺 chatTriggerType/origin 等必填字段），
  上游一律回通用 400（与模型权限无关）导致非全绿即全红、且拿不到 credits。改为复用 converter
  生成**与真实对话同构的合法请求体**、再覆盖 modelId，才能真正触发上游的模型权限判定 +
  消费流解析真实 meteringEvent 计费。

### UI / 默认值
- **模型测试改为独立弹窗**：可自选要测的模型（10 个候选带计费倍率）、结果保留在页可反复测、
  底部"返回"不清结果。每模型真实计费、逐号显示花费 + 总花费。
- **userKey badge 换行修复**：设置页 userKey 输入行的"已设置/未设置"标签不再被挤压换行。
- **R18 图源默认改为关闭**（全年龄 r18=0）：截图/演示/给别人看面板更安全，需要再手动开。

## [0.6.2] - 2026-07-11

### 功能 / 修复
- **模型测试重做**：从卡片按钮改为**勾选凭据后顶部批量栏的「测试可用模型」+ 独立弹窗**
  （仿批量验活）。修正此前只看 HTTP status 导致的**假阳性**（#82/#77 明明受限却全绿）——
  现**真正消费上游 event-stream**，流内出现 error/exception(含 INVALID_MODEL_ID)才判不支持，
  其它 400 也保守判不可用。
- **真实计费 + 花费提示**：每个候选模型发一个无提示词真实请求、解析 meteringEvent 累加**真实
  credit 消耗**；每号显示"花费 X credits"，整轮完成 toast 报"本轮共花费 X credits"。
- **候选模型清单**用真实 Kiro modelId（qwen3-coder-next / haiku-4.5 / sonnet-4.5/4.6 /
  opus-4.6/4.8，从便宜到贵），探测直发 modelId 不过映射，国产模型亦可测。
- 诚实边界：判定依赖上游"无权限模型才返回 INVALID_MODEL_ID"的行为，弹窗内已明确标注可能偏乐观。

## [0.6.1] - 2026-07-11

修正 0.6.0 INVALID_MODEL_ID 处置的**致命设计缺陷**（0.6.0 未部署上线即被发布前对抗性复核拦下）。

### 修复（关键）
- **INVALID_MODEL_ID 改为模型级处置**（原 0.6.0 是凭据级、模型盲）：此前把某号对某模型返回
  `INVALID_MODEL_ID` 当成"整个号坏了"——冷却该号 300s，反复命中还自动禁用整个号。后果：一个
  客户端请求一个订阅不含的模型（如 opus-4.8），几秒内就能把**能正常服务其它模型**（sonnet/haiku）
  的号乃至整池全部打下线，且被禁号不参与自愈、需手动重启。现改为只记"该号+该模型"短期黑名单
  （TTL 30min），选号**仅对该模型**跳过它，该号对其它模型照常调度；**绝不**冷却/禁用整个号。
- **failover 透传修正**：仅当所有未禁用号都已对**当前模型**返回 INVALID_MODEL_ID 时，才向客户端
  透传真实 400（模型无效）；此前因可用性判定忽略冷却态，永远走不到透传分支，客户端收到的是
  429/502 死循环而非干净的"模型不存在"。移除了会误伤的 `SubscriptionInvalid` 自动禁用整号逻辑。
- **模型探测健壮性**：`probe_available_models` 单模型遇上游 5xx/网络错误降级为 `unknown`（不再
  误判 supported，也不再因一个模型失败中止整轮）；结果区分 supported/unsupported/unknown 三态。
- **deep_verify 诚实化**：移除其永不触发的 INVALID_MODEL_ID 死分支（探测体不含 modelId），明确
  分工——deep_verify 只做认证/封禁验活，模型可用性由 probe_available_models 负责。

## [0.6.0] - 2026-07-11

本轮聚焦**订阅失效处置、账号可用性诊断与每账号花费统计**。

### 调度 / 韧性
- **INVALID_MODEL_ID 识别 + 故障转移**：此前上游返回 `400 INVALID_MODEL_ID`（多因某号订阅
  被取消/降级、原本能用的模型不再开放）时，请求当场失败透传给客户端、坏号还留在轮转里反复命中。
  现改为：命中时给该号冷却并 **failover 到订阅仍有效的号**（换个号往往能成功）；短时间内反复命中
  达阈值即判定订阅失效、**自动禁用**（新增 `DisabledReason::SubscriptionInvalid`，可人工/自愈恢复）；
  仅当**所有**号都返回该错误时才判定模型本身无效、透传给客户端。
- **深度验活修正**：`deep_verify` 此前把一切 400 当"凭据有效"，会把订阅已失效的号误判为"活着"。
  现识别 `INVALID_MODEL_ID` 并如实报出"订阅失效/降级"。

### 功能
- **每账号生命周期累计花费**：凭据卡片新增"累计花费"，按上游 meteringEvent 真实计费累加，
  持久化进 `kiro_stats.json`，**独立于用量保留期**（明细按 30 天滚动清理，此累计只增不清），
  软删/恢复无损保留。
- **选中令牌后探测可用模型**：新增 `GET /api/admin/credentials/{id}/models`，对候选模型逐个发极小
  探测请求，按 `INVALID_MODEL_ID` 与否判定该号支持哪些模型（Kiro 无原生列模型接口，仅手动触发、
  约 7 次轻量上游调用，绝不进请求热路径）。凭据卡片加"测可用模型"按钮 + 结果展示。
- **禁用的号也能刷新 Token**：刷新按钮去掉"已禁用则禁用"的前端门（后端本就支持），便于排查/恢复。

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
