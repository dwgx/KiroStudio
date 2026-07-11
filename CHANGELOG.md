# Changelog

本项目版本变更记录。遵循语义化版本(SemVer)。

## [0.7.1] - 2026-07-11

### 修复
- **自定义 API 上号误报「请输入 Refresh Token」**：添加凭据选「自定义 API」时，提交校验的
  非-api_key 分支会先要求 Refresh Token，导致自定义 API（本不需要 refresh token）永远卡在
  这一步、走不到 base URL 校验。修为：custom_api 单独分支，只校验 base URL、不要 Refresh Token。

## [0.7.0] - 2026-07-11

### 新增（自定义 API 代挂透传）
- **自定义 API 凭据（Anthropic 兼容上游代挂）**：可在「添加凭据」里选「自定义 API」，填上游
  base URL + 密钥 + 请求上限。语义是**代挂透传**——Claude Code 打 `/v1/messages` 时，若选号
  命中自定义 API 凭据，就把原始请求体**原样透传**到该 base URL、换用它的密钥、响应流**原样回**
  （入口=出口=Anthropic，零协议转换，效果等同直接拿那个 key 用）。与 Kiro 号**混在同一池按
  优先级/负载均衡分流**。
  - **请求上限自动禁用**：累计请求数达到 `requestLimit` 自动禁用该凭据（防代挂 key 跑量超预算）。
  - 支持凭据级**代理 + 优先级**（复用现有 effective_proxy）。
  - **铁律：绝不污染 Kiro 主路径** —— 只在选号命中自定义 API 凭据时接管；选到 Kiro 号（或池中
    无自定义号）则原样走 Kiro 转发，行为字节级不变。透传响应独立流回，绝不进 Kiro 的 event-stream
    解码器/StreamContext。本地假上游实测透传通过（换 key + body 原样转发），505 测试双特性全绿。
  - 数据模型：`KiroCredentials` 加 `base_url`/`api_key`/`request_limit`（auth_method=custom_api），
    api_key 已加入 Debug 脱敏；自定义号在 `ensure_valid_token` 短路，不进 Kiro token 刷新/IdC 逻辑。

## [0.6.10] - 2026-07-11

### 修复（关键：Windows 裸双击 exe「点击没反应」）
- **exe 缺 config 时不再闪退，改为内置引导**：此前直接双击下载的单个 exe（当前目录无 `config.json`）
  会因缺 apiKey 立刻 `exit(1)`，控制台窗口一闪而过 = 用户看到「点击没反应」。现在 exe 启动时若
  配置缺失，**自动在 exe 同目录生成带强随机密钥的 config.json**（加密安全 RNG）、大字打印
  adminApiKey / apiKey / 面板地址，然后正常启动——裸双击开箱即用，无需先跑 start.bat。
  - 落盘路径：默认 `config.json` 时优先写 **exe 同目录**（双击时 cwd 常不是 exe 目录），
    但 cwd 已有 config 则沿用（兼容源码目录运行 / start.bat）；`--config` 显式路径原样尊重。
  - **幂等且绝不覆盖**：已有 config 完全不碰，二次运行不重新生成、密钥不变。
  - 排除了「缺 DLL」误因：核对线上 exe 导入表无 `VCRUNTIME140.dll`（crt-static 生效），
    「没反应」纯粹是缺 config 闪退，非运行库问题。

## [0.6.9] - 2026-07-11

### 改进（白名单 UI 补全）
- **凭据卡片直接管理「允许模型」白名单**：此前白名单只能在"测试可用模型"弹窗里设、且要先测出结果才出现，
  凭据卡片上既看不到也改不了。现在齿轮设置弹窗（优先级/RPM 同排）新增「允许模型（白名单）」勾选器——
  勾选即该号只接选中模型（成本安全硬门，全不选=不限制），一键保存；卡片主体在设了白名单时显示
  「白名单 N 项」徽标（悬停看具体模型）。
- **模板文案**：模型测试弹窗的快速勾选模板「仅国产便宜」改为「仅国产」。

## [0.6.8] - 2026-07-11

### 修复
- **侧边栏版本号硬编码**：侧边栏一直写死显示 `Admin Panel v0.6.4`，与后端真实版本脱节
  （设置页/OTA 显示正确，唯独侧边栏是死值）。改为读服务端真实版本：`/config` 响应新增
  `serverVersion`（编译期注入 `CARGO_PKG_VERSION`），侧边栏经共享的 `config-snapshot`
  查询取值（与设置页同一缓存键，零额外请求），取不到时不显示版本号而非显示过时值。

## [0.6.7] - 2026-07-11

### 新增（国产模型 + 成本安全）
- **国产模型可调用（GLM / DeepSeek / Qwen / MiniMax）**：Kiro 上游本身直收原生 modelId，
  `map_model` 加分支——`deepseek→deepseek-3.2`、`glm→glm-5`、`qwen→qwen3-coder-next`、
  `minimax→minimax-m2.5/m2.1`，并支持完整原生 id 直透；`/v1/models` 列出这些模型；上下文窗口
  默认 200k。计费按上游 meteringEvent 真实累加，不硬编码倍率。（能否用取决于该号订阅是否覆盖，
  不覆盖走 INVALID_MODEL_ID 模型级黑名单 + failover，不废号。）
- **每号「允许模型」白名单（成本安全硬门）**：凭据可设 `allowedModels`，选号在唯一收敛点
  `is_entry_selectable` + 平行 `transient_wait_duration` 两处硬过滤——设了白名单的号**只**接白名单内
  模型。用途：把便宜模型（国产）的流量锁死在指定便宜号上，**杜绝便宜请求溢出到贵号按贵号计费**。
  硬门语义：设太窄 + 号不够则该模型无号可用返错（防溢出优先于可用性，刻意如此）。新增
  `POST /credentials/{id}/allowed-models` 端点。
- **探测结果打标签持久化**：`probe_models` 完成后把「测试可用模型」结果（supported/unsupported/
  unknown + 时间）写入凭据、持久化，下次进测试页无需重测即可看到该号测过什么、结果如何。
- **白名单 UI**：模型测试弹窗加模板（仅国产便宜 / 仅 Claude / 全部）、测出 supported 一键设为白名单、
  展示历史测试结果。

### 修复 / 改进
- **`Invalid tool parameters` 根治**：根因是逐片透传 tool 参数 partial_json——上游帧非前缀单调时
  启发式重复拼接、或中间帧静默丢弃/截断，客户端拼接后的**总 JSON 非法**。改为 kiro2api 验证的
  范式：按 tool_use_id **缓冲到 content_block_stop 再一次性发单个 delta**（Anthropic 契约允许，
  客户端只在 stop 才 parse）。全程 String 级重组、删除字节切片（消除 char-boundary panic 面）；
  stop 时校验完整 JSON，非法则告警但原样发（绝不静默吞成空参数）；流截断时收尾 flush 残留缓冲 +
  关闭块。单点覆盖 /v1 流式、/cc/v1 缓冲、非流式三条路径。
- **tool 帧静默丢弃补盲（可观测性）**：`Event::from_frame` 失败此前无声吞帧。四处站点补 Err 分支——
  `toolUseEvent` 解析失败置 DecoderStopped 失败态（收尾补发 SSE error / 非流式返 502，客户端按
  api_error 重试，不再把截断当成功），非 tool 帧仅告警不置失败态（零误伤正常流）。
- **Claude Code 自动切协议**：识别到 CC 请求（`x-anthropic-billing-header` 或 UA 经
  `classify_device` 判为 claude-code）时，`/v1` 流式自动走 buffered 分发（等价 `/cc/v1`，
  input_tokens 用上游准确值），CC 无需手动改端点。可配置热更开关 `ccAutoBuffer`（默认开）。

## [0.6.6] - 2026-07-11

### 修复（v0.6.5 出厂构建随附的三处真实缺陷）
- **TLS 后端统一为 rustls，消除「切 native-tls 废网关」的雷**：v0.6.5 起出厂二进制一律
  `--no-default-features`（纯 rustls），不含 native-tls 后端；但设置页仍留着可点的「native-tls」
  按钮，用户点它保存并重启后，所有上游调用（刷 token / 转发）会命中 `bail!` 全部失败、网关直接废，
  只能手改 config.json 才能救回。三重根治：① 设置页移除 native-tls 按钮，TLS 后端改为只读展示
  `rustls`；② 后端 `http_client` 遇 `native-tls` 配置**静默回退 rustls**（不再 `bail`），兜底旧
  `config.json`；③ 保存配置时对任何非 rustls 值归一到 rustls，不再把死后端持久化。rustls 内置
  webpki + 系统根证书，功能等价，回退无副作用。
- **Windows 面板「OTA 在线更新」修好**：OTA 资产名此前硬编码 Linux（`kirostudio-linux-x86_64`），
  Windows 用户点面板升级会下载 Linux ELF（下错平台，即便 sha256 自洽也无法运行）、再试图覆盖
  运行中的 `.exe`（Windows 锁定，失败）。两处根治：① 资产名按运行平台编译期选择（Windows 取
  `kirostudio-windows-x86_64.exe`）；② 替换步骤按平台分流——Windows 用「rename 旧 exe→.bak（备份+
  腾路径）→ rename 新 exe→原路径」绕开文件锁，重启由 start.bat/run.bat 监督循环按原路径拉起新
  二进制；替换失败自动回滚，不留缺失的 exe。至此 Windows 面板一键升级真正可用。
- **CI 增加出厂构建测试门禁**：此前 `cargo test` 只跑默认特性（native-tls），从未覆盖真正发布的
  `--no-default-features`（纯 rustls）构建 = 出厂配置存在测试盲区。`release.yml` 新增 `test` 任务，
  在构建任何产物前先以出厂特性跑全量测试（492 通过），Linux/Windows 两个 build 任务均 `needs` 它，
  测试不过不发布。

## [0.6.5] - 2026-07-11

### 新增（Windows 本机部署，纯增量层，不改任何 `src/` 运行逻辑）
- **引导式启动器 `deploy/windows/start.bat`（双击即跑）**：检测配置 → 缺失/损坏则自动生成带强
  随机密钥的 `config.json`（无 BOM，避免后端 `serde_json` 报 `expected value at line 1 column 1`）
  → 大字打印 adminApiKey/apiKey/面板地址 → 拉起网关。首次零手工配置。
- **监督循环（等价 systemd `Restart=always`）**：`start.bat` / `run.bat` 内置守护循环，网关干净
  自退（exit 0）后自动重拉——**让 admin 面板「一键重启」/ OTA 后重启在 Windows 真正生效**（Windows
  前台无守护进程，此前点重启只会停服不自起）。按退出码区分：0=面板重启→重拉；非零=崩溃→退避重试，
  连续 5 次放弃并报错（不无限刷屏）；Ctrl-C / 关窗口=停服。已在 Windows 实机测试通过。
- **更新脚本 `deploy/windows/update.bat`**：`git pull` + 重建前端/exe，等价面板 OTA（面板 OTA 在
  Windows 不适用：它下载 Linux musl 二进制 + 依赖 rename 运行中 exe）。带防呆：已跟踪文件脏改动
  拒绝更新（不吞用户改动，untracked 文件不误伤）、检测到 exe 运行中拒绝重编（Windows 锁定运行中 .exe）。
- **零运行库依赖 `.cargo/config.toml`（`+crt-static`）**：仅对 `windows-msvc` 目标生效，静态链接 C
  运行时，消除对 `VCRUNTIME140.dll`（VC++ Redistributable）的依赖——任意 Win10+ x64 机器双击即跑，
  无需预装任何运行库。**不影响 Linux/macOS 构建**（cfg 条件不匹配，GitHub Actions Linux 产物不变）。
- **发布产物新增 Windows exe**：`release.yml` 增加 `kirostudio-windows-x86_64.exe`（纯 rustls，
  `--no-default-features`，前端已内嵌）+ sha256，Release 页可直接下载运行。
- **部署文档 `docs/DEPLOY-WINDOWS.md`**：兼容性矩阵、从零跑起、日常运维（停止/重启/更新）、
  与 Linux 版差异表、常见问题。

## [0.6.4] - 2026-07-11

### 修复（模型探测超时）
- **前端 axios 超时**：模型探测现在对每个模型发真实生成请求（可耗时数十秒~数分钟），却被全局
  15s 超时掐断，报 `timeout of 15000ms exceeded`。给探测请求单独放宽到 5 分钟（其它 admin 操作
  仍保留 15s 兜底不变）。
- **后端探测客户端超时**：探测要消费完整生成流，此前用 `.timeout(30s)` 总超时，慢模型生成中途被
  掐断→误判 unknown/失败（与 `Connection closed mid-response` 同类）。改用 `build_streaming_client`
  的 `read_timeout`（空闲间隔 60s）——只要上游在吐数据就不超时，真卡死 60s 无数据才放弃。

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
