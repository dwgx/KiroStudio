// 帮助知识库数据（kirostudio）
// 来源：CLAUDE.md / STATUS.md / .claude/state/CURRENT.md / docs/* 及 docs/archive/*
// 目标读者：面板用户（不一定是开发者）
// 类型定义见 /tmp/help-contract.md

export interface HelpEntry {
  id: string
  title: string
  category: Category
  tags: string[]
  problem: string
  cause: string
  solution: string
  severity: 'high' | 'medium' | 'low'
  source: string
  codePath?: string
  updatedAt: string
}

export type Category =
  | 'pitfalls'
  | 'architecture'
  | 'protocol'
  | 'deploy'
  | 'faq'
  | 'research'
  | 'config'
  | 'security'

export interface HelpModule {
  path: string
  name: string
  role: string
  keyFiles: string[]
}

export interface HelpChainStep {
  id: string
  name: string
  desc: string
  codePath: string
}

export const HELP_ENTRIES: HelpEntry[] = [
  // ============ faq（用户视角可操作）============
  {
    id: 'all-pool-cooldown-429',
    title: '请求大量返回 429「全池冷却」，怎么办',
    category: 'faq',
    tags: ['429', '冷却', 'cooldown', '限流', '全池'],
    problem: '请求开始成批返回 429，错误信息提示所有凭据都在冷却中，或提示「所有凭据均已禁用（0/N）」。',
    cause: '每个上游 429 都会给对应号挂差异化冷却（普通限流 15s、可疑活动 20s、上游 5xx 30s、认证失败 3600s 且不自动恢复等）；号池小时全部号同时冷却会表现为全池 429。历史实测中面板上「限流 38%」里有一半其实是号池为空/全禁用，不是真被限流。',
    solution:
      '1. 打开面板「凭据」页，看每个号的冷却原因与剩余时长（AuthenticationFailed / AccountSuspended / QuotaExhausted 不自动恢复）。\n2. 冷却中的号等自动恢复即可；「冷却中」的僵尸号（认证失败类）用凭据操作里的「重置」或 relogin 恢复。\n3. 若是「0/N 全禁用」，去凭据页逐个检查禁用原因并重新启用或补号。\n4. 查看 trace 中 credential_id 是否为 NULL 且 retry_after_secs=10，那代表根本没发上游请求，问题在号池不在限流。',
    severity: 'high',
    source: 'docs/ARCHITECTURE.md §4.2 冷却系统 + docs/archive/capacity-truth.md §0',
    codePath: 'src/kiro/cooldown.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'credential-auto-disabled',
    title: '号被自动禁用了，怎么恢复',
    category: 'faq',
    tags: ['凭据', '禁用', 'disabled', 'TooManyFailures', '恢复'],
    problem: '面板里某个号变成「禁用」状态，请求不再走它，需要恢复使用。',
    cause: '自动禁用有多个入口：连续失败达阈值（TooManyFailures）、配额耗尽（QuotaExhausted）、账户暂停（AccountSuspended）、刷新失败过多等。禁用状态会持久化落盘，重启不复活（这是刻意修复的行为，防止死号重启后回池反复烧）。',
    solution:
      '1. 看禁用原因：TooManyFailures 通常是 region 配错或 key 真废；QuotaExhausted 是余额不足；AccountSuspended 是上游封号。\n2. 原因消除后（充值、换 region、确认 key 有效），在凭据页对该号执行「重置失败计数」（reset_failure_count）或「重新启用」。\n3. 若是 OAuth 类号（非 api_key 非 custom_api），可用「重新登录」（relogin）端点：清全部惩罚态并启用。\n4. 若确认是 region 配错导致反复失败，先修正 api_region 再启用，避免再次烧号。',
    severity: 'high',
    source: '.claude/state/CURRENT.md 2026-08-11 子任务（reprobe/quota/relogin 端点）+ docs/archive/HISTORY.md #13',
    codePath: 'src/admin/service.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'stale-balance',
    title: '面板显示的余额是旧的',
    category: 'faq',
    tags: ['余额', 'balance', '缓存', '陈旧'],
    problem: '余额数字长时间不变，或与实际扣费对不上。',
    cause: '余额走后台缓存（balance_cache）定期刷新，且余额缓存按账号键共享；面板展示的是缓存快照而不是实时查询，避免每次查看都打上游。刷新间隔内看到旧值是正常设计。',
    solution:
      '1. 点凭据上的「刷新余额」手动触发一次。\n2. 面板左上角有余额缓存时间戳，确认刷新任务（respawn_balance_task）在跑。\n3. 若长期不更新，检查后台日志里余额刷新任务是否报错（如 token 过期）。\n4. 超额自动禁用（autoDisableQuotaExceeded）依赖余额缓存判定，余额陈旧可能导致该开的开关不生效，重刷后再看。',
    severity: 'low',
    source: 'docs/MODULES.md §8 admin/service.rs + docs/ARCHITECTURE.md §五',
    codePath: 'src/admin/service.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'request-feels-slow',
    title: '请求明显变慢（首字迟迟不出）',
    category: 'faq',
    tags: ['慢', 'TTFB', 'websearch', '缓冲', '性能'],
    problem: '有时候请求要等很久才出第一个字，甚至像卡住了一样。',
    cause: '带 web_search 的混合工具请求会无条件走 WebSearch 回灌循环（与模型是否真搜索无关），整轮缓冲、循环结束才返回响应，TTFB 等于所有轮次时长之和（最多 5 轮），比普通流式慢得多。另入站整形开启时排队也会增加延迟（正常现象）。',
    solution:
      '1. 看请求的 trace：命中 websearch 回灌路径的请求 TTFB 会显著偏大，这是该路径的已知行为。\n2. 不需要搜索功能的客户端，把 tools 里的 web_search 移除即可走主路径真流式。\n3. 该路径的首字节握手改造已立项（等上号实测），暂无可配置开关。\n4. 若确认不是 websearch 路径，再检查入站整形排队与号池健康分。',
    severity: 'medium',
    source: 'docs/archive/websearch-buffering-rework.md §1.3 + .opencode/todo-2026-08-13.md P0-1',
    codePath: 'src/anthropic/websearch.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'connect-new-client',
    title: '怎么给新客户端（Claude Code / Cursor / SDK）配置接入',
    category: 'faq',
    tags: ['接入', '客户端', '配置', 'base_url', 'api_key'],
    problem: '新客户端不知道怎么连网关：地址、密钥、格式各是什么。',
    cause: '网关同时提供 Anthropic 兼容（/v1/messages、/cc/v1/messages）与 OpenAI 兼容（/openai）两套协议，端口单端口；密钥用网关自己的 adminKey 体系下发，与上游凭据无关，容易搞混。',
    solution:
      '1. 打开面板「接入信息」页（conn tab），里面有双协议卡片：Anthropic 兼容 + OpenAI 兼容，各自带 base_url 与 curl/env 示例，一键复制。\n2. base_url 填 `http://<主机>:<端口>`（/v1/messages 或 /cc/v1/messages），Claude Code 建议用 /cc/v1 变体以获得精确 input_tokens。\n3. 密钥填网关下发的 Key（x-api-key），不是上游 kiro 凭据。\n4. 浏览器/前端直连时注意 CORS 白名单（corsAllowedOrigins）与入站 IP 白名单。',
    severity: 'medium',
    source: '.claude/state/CURRENT.md 波次 4（接入信息页）+ docs/ARCHITECTURE.md §十一',
    codePath: 'src/anthropic/router.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'backup-restore',
    title: '怎么备份与恢复（升级前必读）',
    category: 'faq',
    tags: ['备份', '恢复', '回滚', 'crashloop', '数据'],
    problem: '担心升级或改配置把服务搞挂，需要知道数据在哪、怎么备份恢复。',
    cause: '数据分三处：配置与凭据在 config 卷（config.json / credentials.json / .at_rest.key），用量明细在 data 卷（traces.db / usage-*.jsonl），旧版二进制在 kirostudio.bak。config.json 每次写盘前自动轮换保留 3 份 .bak。',
    solution:
      '1. 升级前确认 `./data` 与 `./config` 两个卷都挂载了（容器部署），并留一份旧版本二进制/镜像 tag。\n2. 崩溃后先用 `docker inspect <容器> --format \'{{.RestartCount}}\'` 看重启次数是否持续增长，配合 `docker compose ps` 看健康状态。\n3. 配置被改坏：`cp ./config/config.json.bak ./config/config.json` 后重启（.bak.1/.bak.2 是更早的份）。\n4. systemd 部署会自动回滚（rollback-guard.sh），人工兜底 `cp /opt/kirostudio/bin/kirostudio.prev /opt/kirostudio/bin/kirostudio`。\n5. 备份整目录时务必排除 .at_rest.key（见「备份与密钥安全」条目）。',
    severity: 'high',
    source: 'docs/CRASHLOOP-ROLLBACK.md §2/§3 + docs/DEPLOYMENT.md §4',
    codePath: 'src/common/health_marker.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'admin-login-fails',
    title: '面板登录不了 / 管理接口 401',
    category: 'faq',
    tags: ['面板', '登录', 'adminKey', '401', '鉴权'],
    problem: '打开 /admin 登录失败，或调 /api/admin/* 接口一直 401。',
    cause: '两个常见原因：1) 配置字段拼写搞错——正确字段是 `adminApiKey`（不是 adminKey，历史上有人把正确断言改成错的，方向恰好相反）；2) 只有 admin_api_key 非空时才会挂载 Admin 路由，没配就整段没有。',
    solution:
      '1. 检查 config.json：字段名必须是 `adminApiKey`，且值非空。\n2. 面板用该 Key 登录（新版本 Key 存 sessionStorage，关标签即清，重新登录即可）。\n3. 改完配置需重启生效（adminKey 属 restart-only 字段，热重载不覆盖）。\n4. 确认访问路径正确：/admin 是 UI，/api/admin/* 是 API，两者共用同一 Key。',
    severity: 'high',
    source: 'CLAUDE.md「线上配置」节（adminApiKey 三处独立证据）+ docs/ARCHITECTURE.md 附录',
    codePath: 'src/admin/middleware.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'cors-error',
    title: '浏览器直连报 CORS 错误',
    category: 'faq',
    tags: ['CORS', '跨域', '浏览器'],
    problem: '前端页面/浏览器扩展直接调网关接口，被浏览器 CORS 拦截。',
    cause: '网关的 CorsLayer 只放行 corsAllowedOrigins 白名单里的来源；默认白名单不含你的站点来源时浏览器会拦截。',
    solution:
      '1. 在 config.json 的 `corsAllowedOrigins` 里加上你的来源（协议+域名+端口完整写）。\n2. 确认是 Origin 头匹配问题而不是 Key 问题（401 是鉴权，CORS 报错是网络层拦截）。\n3. 不建议开 `*`（配合 adminKey 泄漏即全站接管，且 CSP 相关修复假设了来源受控）。\n4. 命令行客户端（curl/Claude Code）不受 CORS 影响，先用它验证网关本身正常。',
    severity: 'low',
    source: 'docs/ARCHITECTURE.md §七（CorsLayer）+ .claude/state/CURRENT.md 波次 3（CSP 头）',
    codePath: 'src/anthropic/router.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'proxy-misconfigured',
    title: '代理配置错误导致全部请求失败',
    category: 'faq',
    tags: ['代理', 'proxy', 'socks', '失败'],
    problem: '配置代理后请求全部失败，或部分节点反复失败。',
    cause: '代理配置支持全局 proxy 与每凭据 effective_proxy；socks 代理池有自动健康调度（每 5 分钟探测，连续 3 次失败自动禁用节点）。若填了不可达的代理地址或账密错误，出站全部失败；节点被自动禁用后表现为「部分请求失败」。',
    solution:
      '1. 检查代理 URL 格式（支持 socks5://user:pass@host:port 内嵌账密）。\n2. 全局代理改了要重启才生效（proxy 属 restart-only 字段）；每凭据代理可热改。\n3. 面板设置页有「代理池健康调度」开关（socksAutoHealth，内存态），排查节点被自动禁用时先确认它开着、再看探测日志。\n4. 用诊断快照端点（/api/admin/diagnostics/snapshot）看代理池逐节点健康状态。',
    severity: 'medium',
    source: 'docs/DEPLOYMENT.md §6.3 + docs/MODULES.md §1 http_client.rs + CURRENT.md 波次 2',
    codePath: 'src/http_client.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'model-unsupported',
    title: '请求的模型不被支持 / 被挡下',
    category: 'faq',
    tags: ['模型', '白名单', 'blocklist', '映射', '400'],
    problem: '请求某个模型返回「模型不受支持」类错误（error_message 含 model_unsupported_by_pool 或 400）。',
    cause: '多道闸：1) 模型白名单（allowed_models）里没有该模型且非通吃号会被选号阶段挡下；2) 全池 model_blocklist（TTL 1800s）会临时挡住某个模型；3) deepseek 归一化白名单感知：原模型不在白名单会被改写，改写后名上游不认则选号阶段直接返回不可重试错误。',
    solution:
      '1. 看错误信息里的机器可读标记：model_unsupported_by_pool=1 是白名单挡下的（不可重试，重发无意义）。\n2. 在凭据的 allowed_models 里显式加入该模型，或改用通吃号。\n3. 若启用了模型映射（modelMapping），确认映射目标上游认得，或给该凭据开「豁免」（model-mapping-exempt）。\n4. 检查是否命中全局映射后仍走白名单预判（映射不进预判是已知残留，豁免开关是安全阀）。',
    severity: 'high',
    source: 'CLAUDE.md「deepseek 白名单感知模型映射」+ docs/ARCHITECTURE.md §4 ⑥whitelist_hit + CURRENT.md 模型映射节',
    codePath: 'src/kiro/model_mapping.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'image-upload-slow',
    title: '发图片时请求变慢 / 卡顿',
    category: 'faq',
    tags: ['图片', '上传', '慢', 'blocking'],
    problem: '带图片的请求明显变慢，或出现卡顿。',
    cause: '历史上图片处理（JPEG 重编码/校验 magic bytes）曾直接在 tokio worker 上做阻塞计算，会侵蚀异步线程池、延迟传导到所有并发请求；已修复为 spawn_blocking 移出 worker。若仍慢，一般是上游处理图片本身就慢或请求体超大。',
    solution:
      '1. 确认网关版本 >= v1.1.0（图片 block_in_place 修复在此版本）。\n2. 图片请求体受 DefaultBodyLimit（默认 256MiB）限制，超大会被拒。\n3. 图片数量受 image_dedup（MAX_TOTAL_IMAGES=20）约束，同图第二次出现会被占位符替换（正常行为，省 token）。\n4. 单张图过大建议先压缩再发，避免上游 5MiB 请求体上限触发压缩/400。',
    severity: 'low',
    source: '.claude/state/CURRENT.md 波次 1（图片 block_in_place）+ docs/archive/CACHE-RESEARCH.md §1.4 ⑧',
    codePath: 'src/anthropic/converter.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'ota-check-fails',
    title: '面板「检查更新」一直失败',
    category: 'faq',
    tags: ['OTA', '更新', '升级', '失败'],
    problem: '面板点检查更新报错，或提示没有可用更新但明知有新版本。',
    cause: 'OTA 检查更新走 GitHub 私有仓库，需要环境变量 KIROSTUDIO_UPDATE_REPO / KIROSTUDIO_UPDATE_TOKEN（检测到令牌时只走 GitHub 直连、剔除第三方镜像，避免 PAT 交给中间人）。线上 update.env 的 token 为空，面板按钮必然失败。',
    solution:
      '1. 确认线上 /etc/kirostudio/update.env 里已填 token（目前为空，是已知状态）。\n2. 若要启用：在 GitHub 网页建 fine-grained PAT（只勾 dwgx/KiroStudio-skiapi，权限 Contents:read + Actions:read），填入并重启服务。\n3. 在此之前用运维脚本 kirostudio-update 更新，功能等价。\n4. 自动检查默认关（otaAutoCheck 开关在设置页，内存态）。',
    severity: 'low',
    source: 'CLAUDE.md「OTA 检查更新按钮的前置条件」+ STATUS.md 未做清单',
    codePath: 'src/admin/update.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'test-kiro-path',
    title: '测 Kiro 路径时日志却零命中（请求被代挂分流）',
    category: 'faq',
    tags: ['排查', 'custom_api', '代挂', 'Kiro', '日志'],
    problem: '打 /v1/messages 测 Kiro/WebSearch 行为，Kiro 路径日志一条都没有，trace 里 credential_id 全是 authMethod=custom_api 的号。',
    cause: '代挂优先分流（should_try_custom_api_first）：只要池里有 custom_api 号，请求无条件优先走它们，Kiro 主路径根本不进。这是设计行为，不是 bug。',
    solution:
      '1. 查 trace 的 credential_id 对应的 authMethod，确认请求实际走了哪条路径。\n2. 要强制走 Kiro 路径：临时禁用全部 custom_api 号。\n3. 或者用能确定走 Kiro 的请求特征（如 custom_api 池全部冷却时自动回落）。',
    severity: 'low',
    source: 'CLAUDE.md「排查时的坑」第 4 条',
    codePath: 'src/kiro/provider.rs',
    updatedAt: '2026-08-14',
  },

  // ============ pitfalls（开发/运维踩坑）============
  {
    id: 'pit-ssrf-guards',
    title: 'SSRF 防护的坑：6to4 绕过、DNS rebinding、MIME 缺口',
    category: 'pitfalls',
    tags: ['SSRF', '安全', 'XSS', 'bg-img'],
    problem: '登录页背景图代理等出站 URL 功能被用来打内网；6to4 IPv6 曾是绕过路径；MIME 不限曾导致同源 XSS。',
    cause: '背景图代理 /admin/api/bg-img 匿名可达且回显响应体，历史上存在三类缺口：1) 6to4（2002::/16）嵌入 IPv4 绕过 IP 黑名单；2) DNS rebinding TOCTOU（校验后换解析）；3) 预取池（第三方 JSON 源可控）不校验 MIME，把 text/html 原样吐出并支持脚本执行，配合 adminKey 明文存储可完整接管面板。',
    solution:
      '1. 出站 URL 必须过 ssrf.rs 统一防线：只允许 http/https、解析所有候选 IP 逐个校验（拒绝私有/环回/链路本地/云元数据段，含 IPv4-mapped IPv6 与 6to4）、DNS 固定防 rebinding、禁用重定向。\n2. 响应类代理必须做 MIME 白名单（图片只回 image/*）+ 体积上限 + X-Content-Type-Options: nosniff。\n3. 任何新出的出站 URL 功能（如告警 webhook）都要评估 SSRF 面：webhook 地址绝不要填内网管理面地址。',
    severity: 'high',
    source: 'docs/archive/HISTORY.md #1/#16 + docs/ARCHITECTURE.md §七 + docs/DEPLOYMENT.md §6.4',
    codePath: 'src/common/ssrf.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'pit-compress-retry-deadlock',
    title: '压缩重试 target 序列反向：第 2、3 轮必然无效',
    category: 'pitfalls',
    tags: ['压缩', '重试', '死锁', '400', 'content_length'],
    problem: '大请求触发压缩重试后，只有第一轮有效，后两轮重试的 body 反而更大，上游 CONTENT_LENGTH 硬阈值必然再拒绝。',
    cause: '压缩重试目标公式写反：正确递减应为 trigger × 3/4 → 9/16 → 27/64（逐步压狠），实现却是 27/64 → 9/16 → 3/4（逐轮放大）——第 1 次重试即压最狠，后两轮产出更大 body 必然再败，等于 3 次重试只有 1 次有效。这是审查发现的真实缺陷（P0-2 压缩死锁自愈任务）。',
    solution:
      '1. 该 bug 已修复（compress_retry_target 独立成函数，65536 下限，守卫断言调用点）。\n2. 改压缩相关代码时必须同步守卫测试 `compress_retry_loop_uses_extracted_target_fn`（改公式/挪位置会故意红）。\n3. 新增压缩行为必须配行为回归测试，纯算法推导不够（本 bug 就是零测试漏网的）。',
    severity: 'high',
    source: '.claude/state/REVIEW-2026-08-11-ksk-path-fix.md F1/F4 + CURRENT.md 审查整改 F1',
    codePath: 'src/anthropic/handlers.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'pit-disable-not-persisted',
    title: '自动禁用不持久化：重启后死号复活',
    category: 'pitfalls',
    tags: ['禁用', '持久化', '重启', '死号'],
    problem: '额度耗尽/被封/refreshToken 失效的号重启后以 enabled 状态回池，重新走一遍禁用流程，浪费往返且反复触发上游错误。',
    cause: 'report_quota_exhausted / report_account_suspended / report_refresh_token_invalid / report_failure 禁用后只调 save_stats_debounced，而 StatsEntry 不含 disabled/disabled_reason，也不调 persist_credentials（唯一例外是 suspicious_activity）——禁用状态根本没落盘。',
    solution:
      '1. 已修复：新增 persist_disabled_state 统一收口，5 条禁用路径全部接入。\n2. 新增「禁用某号」的代码路径时必须走这个收口，否则同样会静默不持久化。\n3. 排查「重启后号复活」问题时先怀疑禁用路径没走 persist_disabled_state。',
    severity: 'high',
    source: 'docs/archive/HISTORY.md #13',
    codePath: 'src/kiro/token_manager.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'pit-region-burn',
    title: 'region 错配烧号：403 bearer-invalid 3 次即永久禁用',
    category: 'pitfalls',
    tags: ['region', '烧号', '403', 'ksk', '自动禁用'],
    problem: '改 config.region 后，ksk_ API Key 号在错误 region 无效，403 后连续计失败 3 次被自动禁用且落盘，重启也回不来；更糟的是「重探成功后自动恢复」在代码里根本不存在，那号被冻 24h 变僵尸。',
    cause: '403 AccessDeniedException: The bearer token ... invalid 是 region 错配的已知签名（三处注释写明），但热路径把「region 配错」当「key 报废」计 failure_count，3 次即 TooManyFailures 禁用落盘；且 ksk_ 号没写 apiRegion 时 100% 吃 config.region。',
    solution:
      '1. 已修复（apiKeyRegionAutoProbe，默认 true）：403 bearer-invalid 且 api_key 号时不再直接计失败，改后台触发 region 探测，命中写回 api_region、失败才计失败。\n2. 手动处置：给凭据显式写对 apiRegion（凭据字段优先于 config.region），再用 relogin/reset 恢复。\n3. 不要靠改全局 config.region 治跨 region 池——任何单一全局值都会烧掉另一半号，正确杠杆是每凭据 apiRegion。',
    severity: 'high',
    source: 'docs/archive/region-burn-fix.md §1/§6 + docs/archive/capacity-truth.md §4',
    codePath: 'src/kiro/token_manager.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'pit-config-lost-update',
    title: '并发写配置丢更新（lost update）',
    category: 'pitfalls',
    tags: ['配置', '并发', 'lost update', '写锁'],
    problem: '多个入口同时写 config 时，后写者把先写者的修改覆盖掉，配置静默丢失。',
    cause: 'update_config / import_config / set_load_balancing_mode 三条写路径原先没共用同一把锁，并发时产生 lost update（历史上是唯一的持久性错误类别）。',
    solution:
      '1. 已修复：三写路径统一走 config_write_lock。\n2. 新增加「写 config」的代码路径必须纳入同一把锁（守卫 test_config_write_lock_covers_both_write_paths 会红）。\n3. 排查配置改了不生效/被还原时，先怀疑并发写路径没加锁。',
    severity: 'medium',
    source: '.claude/state/CURRENT.md 波次 1 + .opencode/todo-2026-08-13.md P1-2',
    codePath: 'src/admin/service.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'pit-block-in-place',
    title: '阻塞计算直接跑在 tokio worker 上（图片重编码）',
    category: 'pitfalls',
    tags: ['性能', 'tokio', 'blocking', '图片'],
    problem: '带图片的请求把 tokio 异步线程池占死，并发请求集体变慢。',
    cause: '图片 JPEG 重编码/处理是 CPU 密集同步计算，原先直接在异步 handler 里跑，阻塞了 worker 线程，延迟传导到所有并发请求。代码里曾自认「待改」。',
    solution:
      '1. 已修复：改用 spawn_blocking / block_in_place 移出 worker（波次 1）。\n2. 新增任何 CPU 密集或同步 IO 操作时，先判断是否该移出 tokio worker（参考用量管道用独立 OS 线程的先例）。\n3. 排查「并发一高全体变慢」时，检查是否又有人把阻塞计算放进了异步热路径。',
    severity: 'medium',
    source: '.claude/state/CURRENT.md 波次 1 + .opencode/todo-2026-08-13.md P1-11',
    codePath: 'src/anthropic/handlers.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'pit-local-build-impossible',
    title: '本机 8GB 编不过：必须走服务器验证循环',
    category: 'pitfalls',
    tags: ['构建', 'CI', '验证', '8GB', 'docker'],
    problem: '本机 cargo build/test 编不过，且报错与代码质量无关（缺 admin-ui/dist 的 rust-embed E0599、缺 node_modules）。',
    cause: 'MacBook Air 8GB 内存跑不动完整 Rust 构建，且前端 rust-embed 是编译期嵌入、dist 在 .gitignore 里，fresh clone 后必报 E0599；本地测试的是发布版根本走不到的 TLS 分支（Cargo.toml default = native-tls 与出厂配置相反）。',
    solution:
      '1. 验证一律走 skiapi 服务器 Docker「验证循环」：快照（临时 index）→ scp → docker build --target builder → docker run 显式跑 cargo test --no-default-features。\n2. 所有 cargo 命令一律加 --no-default-features（纯 rustls，与出厂构建一致）。\n3. 判定标准必须看到 `test result: ok. NNNN passed; 0 failed`；新增测试再按名单独跑一次确认真执行了。',
    severity: 'high',
    source: 'CLAUDE.md「本机编译与线上验证」+ 构建与测试节',
    codePath: 'src/main.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'pit-guard-silent-green',
    title: '守卫测试静默变绿：注释字面量、宏捕获、切片锚点',
    category: 'pitfalls',
    tags: ['测试', '守卫', '假红', '假绿', '静默失效'],
    problem: '测试全绿但守卫实际没在守：删掉被保护的目标代码测试照样通过，守卫形同虚设。',
    cause: '三类真实踩坑：1) 注释里写了守卫要匹配的标记字面量，按「第一次出现位置」切分生产区时切分点提前、生产区被截断；2) 函数内 macro_rules! 靠捕获外层局部变量，而宏卫生性让标识符在定义处语境解析、解析不到那个绑定，检查形同不存在；3) 守卫的 needle 出现在测试段自身的字面量里，统计恰好凑数（巧合性截断）。',
    solution:
      '1. 写完守卫必须做破坏实验：实测「删掉目标它会不会红」，且破坏要类型等价（改 let x = ...; if let Some(_) = x 这类），直接删字段会编译错测不到守卫。\n2. 注释/文档里描述代码标记时刻意绕开字面量（运行时拼接 needle）。\n3. 函数内宏要用的外层变量显式当参数传进去，别靠捕获。\n4. 守卫切片用运行时拼接 + 显式截断（split_once），不要依赖「第一次出现」。',
    severity: 'high',
    source: 'CLAUDE.md「排查时的坑」第 8/9 条 + CURRENT.md 复审第三轮对抗审查',
    codePath: 'src/kiro/provider.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'pit-doc-line-drift',
    title: '文档行数与注释漂移：改代码不更新注释的代价',
    category: 'pitfalls',
    tags: ['文档', '注释', '漂移', '行数'],
    problem: '文档里写的行数、常量值、结构描述与代码实际不符，照着文档定位/决策会出错。',
    cause: '全仓行数比文档记录漂移 2.5 倍（35,800 → 90,032 行），文件级行数全部过期；注释与实现分叉的实例成群：ABSOLUTE_MAX_TOTAL_RETRIES 注释写 12 实际 4、压缩重试注释与实际公式相反、prompt_cache_enabled 是死配置而注释说「默认关砍 CPU 开销」的代码路径已不存在、config.rs 注释描述已删掉的 cache_tracker 行为。',
    solution:
      '1. 引用行数前用 codegraph（cg.py stat）现读，不要信文档数字。\n2. 改行为/常量/结构时同步更新相关注释，注释与实现分叉是事故的源头之一。\n3. 发现「文档断言」与代码矛盾时，以代码 + 测试为准，并修正文档（更正一条断言时要像验证原断言一样验证你的更正——曾有把正确事实改成错误方向的实例）。',
    severity: 'medium',
    source: 'CLAUDE.md「文档里的行数全部过期」+ CURRENT.md 注释漂移修复 8 处',
    codePath: 'src/model/config.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'pit-strings-check-unreliable',
    title: '用 strings 查编译产物验证改动是否上线——不可靠',
    category: 'pitfalls',
    tags: ['验证', 'strings', '二进制', '上线'],
    problem: '用 strings/grep 在编译产物里找不到刚加的字面量，误判改动没上线。',
    cause: '编译优化会内联/合并/删除字面量，字符串在二进制里 grep 不到不代表代码不在。',
    solution:
      '1. 验证改动是否在线：查已部署快照的源码（git show <tag>:<file> | grep），不要查二进制。\n2. 或直接看线上行为（trace 字段、日志标记），比静态检查可靠。\n3. 同类不可靠手段：本机 python 数括号平衡找语法错（char 字面量/生命周期标注误判），直接让 rustc 报行号。',
    severity: 'low',
    source: 'CLAUDE.md「排查时的坑」第 1/2 条',
    codePath: 'src/main.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'pit-frontend-dist-bind',
    title: '前端 dist 是 bind mount：hotswap deploy 只换后端',
    category: 'pitfalls',
    tags: ['前端', '部署', 'dist', 'bind mount'],
    problem: '改了前端重新部署，页面上没变化；或误以为构建失败。',
    cause: '线上 /opt/kirostudio-src/admin-ui/dist 以只读 bind mount 挂进容器（优先于 rust-embed），hotswap deploy 只换后端二进制，前端必须单独重建 dist 并同步。Vite 内容哈希相同说明源码没变，不是构建失败。',
    solution:
      '1. 改了 admin-ui 源码后：服务器上用 node 构建 dist，再 docker cp 覆盖 /opt/kirostudio-src/admin-ui/dist。\n2. 别把「dist 没变化」当成构建失败或部署失败。\n3. 前端构建必须用 pnpm（npm 会产生冲突锁文件）。',
    severity: 'medium',
    source: 'CLAUDE.md「改了前端还要单独同步 dist」+ CURRENT.md 试过不通的路 6',
    codePath: 'src/admin_ui/router.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'pit-ai-code-must-pass-ci',
    title: 'workflow agent 的代码必须过 CI 才能信',
    category: 'pitfalls',
    tags: ['AI', '代码', 'CI', '审查'],
    problem: 'agent 交付的代码报告里自称「逻辑自洽」，实际有 5 类真实缺陷，全靠 CI 抓出。',
    cause: '实测抓出的缺陷类型：raw string 内容以引号结尾导致提前闭合、/// 文档注释用在函数参数（Rust 不允许）、cap 触发后重入死循环、截断结果超出契约、测试 helper 喂裸字符串给需要 JSON 对象的函数。',
    solution:
      '1. 任何 agent 交付的代码，先过服务器 CI 再采信。\n2. 报告里「逻辑自洽」「测试通过」必须看到 `test result: ok` 才算数。\n3. 关键行为按名单独跑测试，防 filter 吞掉。',
    severity: 'medium',
    source: 'CLAUDE.md「排查时的坑」第 7 条',
    codePath: 'src/anthropic/handlers.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'pit-capacity-fake-number',
    title: '容量口径是假的：配置自乘的数不是实测',
    category: 'pitfalls',
    tags: ['容量', 'RPM', '限流', '配置', '假数字'],
    problem: '面板/脚本显示的「池容量」与实际吞吐差 3 倍，按它调参全在算空气，用户体感「配置根本没法调」。',
    cause: '池容量 = credentialRpmLimit × rpmHeadroomFactor 自乘出来的数（线上 500×85%=422），实测单号持续干净上限只有 137 RPM；且单号池时 rpm_saturation_gate_active 无条件返回 false，任何 rpm 阈值都不生效。',
    solution:
      '1. 判断任何限流配置的影响前，先确认它所在的分支真的被走到（单号池下 RPM 闸门是惰化的）。\n2. 调参用实测数据（traces.db 的 ok_before_429 判据：<100 号坏，>1000 且随 RPM 单调上升是真限流），别信配置算术。\n3. 改 credentialRpmLimit 前做控制实验，不要按文档旧值直接改（会把吞吐掐死一个数量级）。',
    severity: 'high',
    source: 'CLAUDE.md「容量口径是假的」+ docs/archive/capacity-truth.md',
    codePath: 'src/kiro/token_manager.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'pit-config-limit-vs-actual',
    title: '配置值上限不等于实际放大：先确认分支被走到',
    category: 'pitfalls',
    tags: ['放大', '重试', 'SWAP', '实测'],
    problem: '按配置上限推算「最坏 480 倍放大」并据此改配置，但改动没有任何效果。',
    cause: '按 SWAP_MAX_ATTEMPTS=60 × 客户端 × 网关推算 480×，实测 19579 次判定全部落在 [passthrough] 分支、零 swap，那个 60 从未被触及；真正生效的是 30s 墙钟预算（最大只到 7 次）。改 SWAP_MAX_ATTEMPTS 是死配置，该动的是 MAX_BUDGET_SECS。',
    solution:
      '1. 判断一个配置的影响，先确认它所在的分支实际有没有被走到，再谈它的值。\n2. 34 个限流旋钮实测只有 4 个真在限（两个代码常量 + 两个并发上限），其余是死配置/语义陷阱。\n3. 线上还有外挂 kiro_shield.py 在网关前做整请求重试（放大 5.6× 实测），改客户端可见的 503 文案前先 grep 仓外消费者（COOLING_MARKERS 词），否则 Retry-After 被丢弃走错退避阶梯。',
    severity: 'high',
    source: 'CLAUDE.md「放大链」「34 个限流旋钮」两节',
    codePath: 'src/kiro/provider.rs',
    updatedAt: '2026-08-14',
  },

  // ============ research（研究结论）============
  {
    id: 'res-cache-chain',
    title: '缓存链研究结论：真信号从未被解析、cache_read 是虚构值',
    category: 'research',
    tags: ['缓存', 'cache', 'tokenUsage', '研究'],
    problem: '面板的缓存命中率/cache_read 数字可信吗？',
    cause: '调研结论：1) 上游 Smithy 模型里有 CachePoint 与 MetadataEvent.tokenUsage.cacheReadInputTokens，而本仓 metadataEvent 从未被解析（当 Unknown 静默丢弃），真信号可能一直在线上；2) 下发给客户端的 cache_read_input_tokens 是本地估算的虚构值且无条件注入，还会反向扣减上游唯一准确的 input_tokens，纯中文多轮会话会触发 message_start.input_tokens=0；3) 口径不对称（工具定义只进分母不进分子）、估算器对中英文误差符号相反。',
    solution:
      '1. 看缓存相关数字时注意：cache_read 是估算值，不是上游真值（面板有 tooltip 标注）。\n2. 别再当「47% 折扣」是事实——它已被 46 万条 traces 证否（上游无隐式前缀 credit 折扣），不要拿它做决策。\n3. 正确顺序是：先度量（解析 metadataEvent）再止损（稳前缀）再调度，最后才考虑本地响应缓存。',
    severity: 'medium',
    source: 'docs/archive/CACHE-RESEARCH.md §0/§1.4 + docs/archive/prefix-stability-2026-08-06.md',
    codePath: 'src/kiro/model/events/base.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'res-capacity-truth',
    title: '选号容量真相：单号 137 RPM 是实测天花板',
    category: 'research',
    tags: ['容量', 'RPM', '实测', '429'],
    problem: '单个健康号到底能扛多少 RPM？429 率怎么归因？',
    cause: '用线上 27 万条 traces 只读实测：最佳 5 分钟持续零 429 = 137 RPM；「首个 429 之前的成功数」是决定性判据（速率限制不可能在第 9 个请求触发——号到手即分两种状态）；region 假说被三重证据证伪（RTT 指纹 + 跨区对照 + 直接读到 apiRegion 的号交叉验证）。',
    solution:
      '1. 号坏 vs 真限流判据（一条 SQL）：ok_before_429 < 100 → 号坏；> 1000 且 429% 随 RPM 单调上升 → 真限流。\n2. 429 与 403 是两条独立故障链，不要用一条解释另一条（error_message 字符串完全相同，不是判据）。\n3. 面板「限流」百分比混着「没号」和「真限流」，归因前先按 credential_id IS NULL 拆开。',
    severity: 'medium',
    source: 'docs/archive/capacity-truth.md §4/§5',
    codePath: 'src/kiro/scheduling.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'res-region-self-heal',
    title: 'region 纠错自愈设计：异步裁决 + 入池预探',
    category: 'research',
    tags: ['region', '自愈', '探测', '设计'],
    problem: '403 bearer-invalid 的号怎么处置才不会烧号也不会留僵尸？',
    cause: '研究结论：同步换 region 重试会压在用户请求里（3 次往返 + 耗墙钟），必须异步裁决；失败计数延后到探测有结论；探测起不来的分支必须回落 report_failure（防真废 key 变僵尸）；软冷却不能用 AuthenticationFailed（86400s 不可自动恢复 = 面板显示「冷却中」的僵尸）。',
    solution:
      '1. 该设计已实现（apiKeyRegionAutoProbe，默认 true）：403 触发后台探测，候选集 = 自己 region → 池内实测有效 region（按 success_count 降序）→ config region → 兜底表，去重截断 3 个。\n2. 三个防线缺一不可：探测起不来要回落计失败、每号 6h 最多一轮、单轮最多 3 region。\n3. 手动场景：新上号前先确认 apiRegion，或依赖入池预探自动写回。',
    severity: 'medium',
    source: 'docs/archive/region-burn-fix.md §2/§3/§4',
    codePath: 'src/kiro/regions.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'res-websearch-buffering',
    title: 'WebSearch 回灌路径 TTFB 改造设计（方案 C 已定）',
    category: 'research',
    tags: ['websearch', 'TTFB', '设计', '缓冲'],
    problem: '带 web_search 的混合请求 TTFB 等于整个循环时长，怎么改？',
    cause: '研究结论：内容只能「轮级」发布（回灌轮文本对客户端不可见是协议语义，不是实现偷懒），token 级真流式在此路径不存在；四方案对比后选定参考仓实证的「首字节握手」（ping + oneshot 竞争 200/错误码）：TTFB 恢复到上游首 chunk 水平，内容仍整循环批量，加每轮结束 ping 增强。',
    solution:
      '1. 该改造已设计完成（render_deferred_sse 骨架参考 ref-grey @795b9ca），等上号实测后实施。\n2. 不把目标定成 token 级流式——回灌轮内容不可发布，那是协议约束。\n3. 实施时保留 run_web_search_loop 函数名（两个源码级守卫锚定了它）。',
    severity: 'low',
    source: 'docs/archive/websearch-buffering-rework.md §5/§8 + .opencode/todo-2026-08-13.md P0-1',
    codePath: 'src/anthropic/websearch.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'res-unique-capabilities',
    title: '参考仓都没有的独有能力盘点',
    category: 'research',
    tags: ['对比', '能力', '吸收层', '独有'],
    problem: 'KiroStudio 相比同类网关（newapi / sub2api / kiro.rs 全版本）强在哪、缺什么？',
    cause: '五路并发研究结论：独有能力 7 项——吸收层（预算内吞 429 不让客户端看到）、共享预算（每请求 ≤4）、族级连坐（同租户整族退避）、AIMD 自动挡、SSRF 纵深、at-rest 加密、透传零转换；值得借鉴但未做的：显式模型渠道路由、统一错误规范化、模型级限流、成本核算（后两项已在波次 2 落地）、/v1/responses 入站。',
    solution:
      '1. 与上游对比排障时先确认「独有能力」是否在起作用：吸收层只覆盖主路径、不覆盖透传（线上 100% 流量走透传时吸收旋钮全程无效）。\n2. 模型级限流与成本表已在 v1.1.0 提供（by-model cost 列可识别烧钱模型）。\n3. 明确不做清单（有证据）：newapi 用户/支付/分销、Redis/MySQL 架构、ksk region 物化重写。',
    severity: 'low',
    source: '.opencode/todo-2026-08-13.md §一/§四',
    codePath: 'src/kiro/provider.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'res-fingerprint-simulator',
    title: '缓存 fingerprint 模拟器（cache 链 Layer 3）',
    category: 'research',
    tags: ['缓存', '指纹', 'fingerprint', 'Layer3'],
    problem: '网关侧如何模拟上游缓存命中以估算缓存收益？',
    cause: '研究+实现：参考仓 kiro2cc-proxy 的缓存指纹思路移植为纯内存版（最长公共前缀命中 + 会话隔离种子 + TTL 5m/1h 拆分 + LRU 淘汰），接进 cache 链 Layer 3；范围刻意裁剪：无持久化/无后台线程/成功才 commit；两个实现坑已修：非流式路径 Layer 3 被 Layer 2 槽吞掉（强制降槽）、工具块漂移 id 破坏命中链（签名剔除 id + JSON canonicalize）。',
    solution:
      '1. 指纹命中率面板数字来自模拟器，用于估算收益，不是上游真值。\n2. 指纹 Layer 3 在非流式路径的 creation 计数有已知低估（MINOR，登记待决策）。\n3. 签名稳定依赖 JSON key 有序（serde_json 默认 BTreeMap，不要开 preserve_order feature）。',
    severity: 'low',
    source: '.claude/state/CURRENT.md P1 移植（缓存 fingerprint 模拟器）+ docs/archive/CACHE-RESEARCH.md §1.3',
    codePath: 'src/anthropic/cache_fingerprint.rs',
    updatedAt: '2026-08-14',
  },

  // ============ architecture（架构地图）============
  {
    id: 'arch-request-chain',
    title: '请求完整链路与 12 键选号',
    category: 'architecture',
    tags: ['架构', '链路', '选号', '排序键'],
    problem: '一个请求从进来到最后返回，中间经历了什么？号是怎么被选中的？',
    cause: '链路：安全层（IP 白名单/限流）→ CORS/Body → 认证（constant_time_eq）→ 转换（Anthropic→Kiro）→ 压缩（防 5MiB）→ 调度（亲和 → 选号 → 重试/故障转移）→ 上游 event-stream → SSE 回转 → 用量入管道。选号是同一把锁临界区内的 12 键升序（unusable 沉底、starved 反饥饿、健康档、爬坡档、白名单命中、inflight、模型调用数、容量千分比、RPM 已用率、p_avail、success_count 兜底）。',
    solution:
      '1. 排障按链路逐层确认：trace 里 credential_id 有没有、error_message 标记是什么、延迟卡在哪层。\n2. 选号异常先看排序键语义：inflight 只进键不做硬门（防假性排队）；白名单号饱和前通吃号恒排后是刻意的显式路由优先。\n3. 优先级模式走同一套键（prio_first 恒 true），整层打爆才溢出。',
    severity: 'medium',
    source: 'docs/ARCHITECTURE.md §三/§四',
    codePath: 'src/kiro/token_manager.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'arch-v1-vs-cc',
    title: '/v1 与 /cc/v1 的差异：input_tokens 精确度',
    category: 'architecture',
    tags: ['架构', '/cc/v1', 'input_tokens', 'Claude Code'],
    problem: '两个消息端点有什么区别，客户端该用哪个？',
    cause: '/v1 立即发 message_start（input_tokens 为估算值）；/cc/v1 用 BufferedStreamContext 缓冲 message_start，等 contextUsageEvent 拿到精确 input_tokens 再发。Claude Code 依赖 message_start.usage.input_tokens 显示上下文进度条，需要精确值，所以 Claude Code 走 /cc/v1。',
    solution:
      '1. Claude Code 配 /cc/v1；通用 SDK 用 /v1。\n2. 两个端点共用同一套安全/转换/调度/压缩逻辑，只是流式上下文不同。\n3. ccAutoBuffer 开关影响的是 /v1 是否也走 buffered 变体。',
    severity: 'low',
    source: 'docs/ARCHITECTURE.md §3.1 + docs/PROTOCOL.md §5.3',
    codePath: 'src/anthropic/handlers.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'arch-hot-reload',
    title: '三层热重载：哪些配置改了即时生效，哪些要重启',
    category: 'architecture',
    tags: ['架构', '热重载', '配置', '重启'],
    problem: '改了配置，有的即时生效有的要重启，怎么区分？',
    cause: 'TIER1（ArcSwap 原子镜像）覆盖冷却/限流/亲和/RPM/负载均衡等，即时生效；TIER2（后台任务 abort+respawn）覆盖 token 预刷新、余额刷新间隔；TIER3（进程级镜像）覆盖 extract_thinking/compression/strip_env_noise。诚实边界：prompt_cache_ttl / proxy / tls / 端口 / adminKey 仍需重启。',
    solution:
      '1. 改 adminKey/proxy/tls/端口后必须重启。\n2. 面板改配置走 PUT /config（热重载派发），不要直改配置文件后重启。\n3. 注意：socks 自动健康开关是内存态（重启回默认 true），config.json 里没有对应字段。',
    severity: 'low',
    source: 'docs/ARCHITECTURE.md §九 + docs/DEPLOYMENT.md §6.3',
    codePath: 'src/model/config.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'arch-usage-pipeline',
    title: '用量统计管道：专用 OS 线程 + 满则丢弃',
    category: 'architecture',
    tags: ['架构', '用量', 'SQLite', 'pipeline'],
    problem: '用量统计为什么不会拖慢请求？数据会丢吗？',
    cause: '设计决策：sink 做同步阻塞 IO（SQLite execute / writeln!），若跑在 tokio worker 上会被慢盘/fsync 抖动侵蚀线程池、延迟传导回请求路径，所以用独立 OS 线程，请求路径只做一次非阻塞 try_send；通道满时丢弃 + AtomicU64 计数；sink panic 用 catch_unwind 隔离。',
    solution:
      '1. 面板用量数字在极端流量下可能丢尾部记录（满则丢弃是刻意的，计数可查）。\n2. SQLite 已加 busy_timeout 5000 与批量写（波次 1），双实例并发写不再静默丢账。\n3. 查不到某条 trace 时先确认 pipeline 是否满过。',
    severity: 'low',
    source: 'docs/ARCHITECTURE.md §八 + CURRENT.md 波次 1（trace_db 批量写）',
    codePath: 'src/usage/pipeline.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'arch-family-collateral',
    title: '族级连坐：同租户账号整族退避',
    category: 'architecture',
    tags: ['架构', '族级', '连坐', 'M365', 'AWS'],
    problem: '一个号被账户级风控，为什么整组号都变慢/退避？',
    cause: '健康熔断器的键 = family_key：M365 同租户 → m365:{tenant}、AWS 同账号 → aws:{account}，一号被账户级风控整族一起退避（这是设计，防逐个砸）；IdC/social/api_key → cred:{id} 各自独立不受连坐。',
    solution:
      '1. 面板健康分是按族聚合的，同族号会一起降分。\n2. 想隔离风险就把同租户号拆开管理（不同 family_key）。\n3. 排障时注意：裸 429 的 health 键历史上曾用错 cred:{id} 导致 M365 号永不熔断（已修，用 family_key_of）。',
    severity: 'low',
    source: 'docs/ARCHITECTURE.md §4.1 + docs/archive/HISTORY.md #12',
    codePath: 'src/kiro/health.rs',
    updatedAt: '2026-08-14',
  },

  // ============ protocol（协议转换）============
  {
    id: 'proto-event-stream',
    title: 'AWS event-stream 帧格式与双 CRC',
    category: 'protocol',
    tags: ['协议', 'event-stream', 'CRC', '二进制'],
    problem: '网关与 Kiro 上游之间的二进制协议长什么样？',
    cause: '帧结构：prelude(12B：total_length + header_length + prelude_crc) + headers(变长，10 种值类型) + payload + message_crc。全部大端序；双 CRC32 校验（ISO-HDLC）；帧上限 16MB。解码器是自实现的流式零拷贝状态机（Ready/Parsing/Recovering/Stopped），损坏时单字节跳跃恢复，连续 5 次错误进入 Stopped。',
    solution:
      '1. 协议层问题（乱码/流中断）先看解码器状态与 CRC 错误计数。\n2. 两个端点（IDE 走 URL 路径、CLI 走 X-Amz-Target 头 + 服务根）协议相同、凭据类型绑定不可互换（互换实测 403）。\n3. 自实现解码器是设计决策（无现成 Rust 库支持流式零拷贝解码）。',
    severity: 'low',
    source: 'docs/PROTOCOL.md §1/§6 + docs/ARCHITECTURE.md §6.3',
    codePath: 'src/kiro/parser/decoder.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'proto-model-mapping',
    title: 'Anthropic 与 Kiro 模型 ID 映射',
    category: 'protocol',
    tags: ['协议', '模型', '映射', 'modelId'],
    problem: '请求的模型名与上游实际用的模型 ID 怎么对应？',
    cause: 'contains 匹配 + 无版本号回退：claude-sonnet-4-* → sonnet-4.5、opus 4.x → opus-4.x、haiku → 4.5；映射进 Kiro 数字 ID（sonnet=14、opus=13、sonnet-3-7=12、haiku=10）。模型映射（modelMapping）是用户可配的全局扁平 map + 每凭据豁免，用量记原始名 + 映射后名双口径。',
    solution:
      '1. 面板 usage 页可按「请求模型」或「上游模型」两个维度看用量（双口径）。\n2. 设置页的 modelMapping JSON 卡可配映射（值类型校验，非法不提交）。\n3. 映射后不再判白名单（白名单管原始名），豁免开关是安全阀。',
    severity: 'low',
    source: 'docs/PROTOCOL.md §4.1 + CURRENT.md 模型映射节',
    codePath: 'src/anthropic/converter.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'proto-sse-sequence',
    title: 'SSE 事件序列与 thinking 处理',
    category: 'protocol',
    tags: ['协议', 'SSE', 'thinking', 'stream'],
    problem: '流式响应的事件顺序、thinking 块的呈现方式。',
    cause: '标准序列：message_start → content_block_start → content_block_delta×N → block_stop → message_delta → message_stop；Kiro 上游的 thinking 内容以 <thinking> 标签内嵌在文本中，配置 extract_thinking=true 时提取为独立 ContentBlock。',
    solution:
      '1. 客户端解析异常时按上面的事件序列核对。\n2. thinking 是否独立成块由 extract_thinking（TIER3 热开关）控制。\n3. 网关还会做 DSML 标记剥离与 tool_use XML 泄漏过滤（DeepSeek 上游场景），响应文本与上游原始字节不完全一致是正常的。',
    severity: 'low',
    source: 'docs/PROTOCOL.md §5 + CURRENT.md DSML 修复节',
    codePath: 'src/anthropic/stream.rs',
    updatedAt: '2026-08-14',
  },

  // ============ deploy（部署运维）============
  {
    id: 'deploy-data-volumes',
    title: '数据落卷：不挂载的后果是用量明细全丢',
    category: 'deploy',
    tags: ['部署', '卷', 'traces.db', '数据'],
    problem: '容器重建后面板用量/成功率归零重来。',
    cause: '用量数据（traces.db + usage-*.jsonl）默认落 usageDataDir（相对进程 cwd）；容器内 WORKDIR 是 /app，若 compose 没挂 `./data:/app/data`，重建后数据全丢。',
    solution:
      '1. 核对 compose volumes 有 `- ./data:/app/data`（或 usageDataDir 显式指向已挂载目录）。\n2. 升级/重建前确认 `./data` 与 `./config` 都在（升级核对清单第一条）。\n3. 健康检查：升级后 /healthz 的 sqlite_writable 必须为 true。',
    severity: 'high',
    source: 'docs/DEPLOYMENT.md §1/§5',
    codePath: 'src/usage/trace_db.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'deploy-log-limit',
    title: '日志无上限会写满磁盘',
    category: 'deploy',
    tags: ['部署', '日志', '磁盘', 'json-file'],
    problem: '容器日志无界膨胀占满磁盘，服务异常。',
    cause: 'json-file 日志驱动无上限时日志无限增长（用量管道还另写文件，日志是额外一路）。',
    solution:
      '1. compose 加 logging 段：max-size 10m + max-file 5。\n2. systemd 部署 journald 自带 SystemMaxUse 兜底，不要用 StandardOutput=file: 不轮转。\n3. 排查磁盘满时先看 docker logs 大小。',
    severity: 'medium',
    source: 'docs/DEPLOYMENT.md §2',
    codePath: 'src/main.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'deploy-healthcheck',
    title: '健康探针：探 /v1/models 不探 /admin',
    category: 'deploy',
    tags: ['部署', '健康检查', 'HEALTHCHECK', '探针'],
    problem: '容器一直显示 unhealthy，但服务其实正常；或反之。',
    cause: '正确探针是 /v1/models：200 与 401 都算健康（401 说明连接建立、路由命中、鉴权拦截正常）；/admin 是带 adminApiKey 鉴权的完整 UI 页，探它既重又不反映「网关可用」。另有未鉴权 GET /healthz（ok/version/config_loaded/pool_count/sqlite_writable）适合反代做主动探测。',
    solution:
      '1. healthcheck 探 /v1/models（grep -Eq ^(200|401)$）。\n2. Caddy/反代主动探测用 /healthz。\n3. 「hotswap status 报 FAIL」可能是误报：核实看 docker ps 实际状态 + Admin API 是否 200，判定标准是连接拒绝/超时（curl 000）而非 404。',
    severity: 'medium',
    source: 'docs/DEPLOYMENT.md §3/§6.1 + CLAUDE.md 排查坑 5',
    codePath: 'src/anthropic/handlers.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'deploy-crashloop-detect',
    title: 'crashloop 判定三要素与回滚',
    category: 'deploy',
    tags: ['部署', 'crashloop', '回滚', '判定'],
    problem: '新版启动即崩表现为「一直在重启」，怎么判定与处置？',
    cause: 'restart: unless-stopped 无限重启，新版启动即崩时表现为 Restarting。注意「进程活着但行为劣化」不算 crashloop（那是 watchdog 管辖）。',
    solution:
      '1. 判定三要素：RestartCount 持续增长（间隔几十秒看两次）+ docker compose ps 长期 unhealthy（90s 后）+ 日志尾部反复同一崩溃点。\n2. 处置：docker compose stop 后 image tag 指回上一版本再 up -d；配置被改坏用 config.json.bak 恢复。\n3. systemd 部署有自动回滚（rollback-guard.sh，阈值 3 次，约 10s 触发），不需要人工。',
    severity: 'high',
    source: 'docs/CRASHLOOP-ROLLBACK.md §1/§3/§4',
    codePath: 'src/common/health_marker.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'deploy-ota-selfupdate',
    title: 'OTA 自更新的安全链：sha256 + 回滚闭环',
    category: 'deploy',
    tags: ['部署', 'OTA', 'sha256', '回滚'],
    problem: 'OTA 更新怎么保证下载的二进制可信、失败能自愈？',
    cause: '下载的二进制必须过 sha256 校验（校验文件从 github.com 直连取，切同源投毒 RCE 链），原子 rename 覆盖 exe，替换前备份 .bak；systemd ExecStartPre 守卫实现「新版启动即崩 → 自动回滚旧版」，回滚决策放 systemd 层不放可能已崩的进程。',
    solution:
      '1. 手动验证 release 产物：重算 SHA256 与仓库发布值一致。\n2. 历史坑：OTA 曾按 OS 不按架构选资产（macOS 拿到 Linux ELF 当场死亡），现已按 OS×ARCH 穷举 6 组合；tag 与 Cargo.toml 版本不一致会无限升级循环，release.yml 已加门禁。\n3. 面板 OTA 需要 update.env 的 token（见「检查更新失败」条目）。',
    severity: 'medium',
    source: 'docs/ARCHITECTURE.md §九 + docs/archive/HISTORY.md #17/#18',
    codePath: 'src/admin/update.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'deploy-diagnostics',
    title: '运维观测三件套：诊断快照 / 恢复指标 / 端点健康',
    category: 'deploy',
    tags: ['部署', '观测', '诊断', 'snapshot'],
    problem: '排障时怎么一键拿到全量状态？',
    cause: '三个端点：GET /api/admin/diagnostics/snapshot（版本/逐号状态/代理池健康/自愈计数器一键聚合，纯观测零副作用）、/api/admin/recovery-metrics（自愈计数器）、/api/admin/endpoint-health（每凭据×端点实测成功率 EWMA）。均走 Admin API 鉴权。',
    solution:
      '1. 排障首查 diagnostics/snapshot。\n2. 号池/端点问题看 endpoint-health 与 recovery-metrics。\n3. 告警 webhook 消费的就是 recovery_metrics 的计数器（429 风暴/预算耗尽/号全灭/凭据禁用）。',
    severity: 'low',
    source: 'docs/DEPLOYMENT.md §6.2 + CURRENT.md 波次 2',
    codePath: 'src/admin/service.rs',
    updatedAt: '2026-08-14',
  },

  // ============ config（配置指南）============
  {
    id: 'cfg-field-name',
    title: '配置字段拼写核对：adminApiKey 不是 adminKey',
    category: 'config',
    tags: ['配置', 'adminApiKey', '拼写'],
    problem: '配了 adminKey 但面板鉴权不生效。',
    cause: '正确字段是 adminApiKey（serde camelCase，src/model/config.rs 的 admin_api_key）。历史上有人把正确断言改成错的（方向恰好相反），三次独立证据实读确认：线上 config.json 是 adminApiKey 且无 adminKey，两个运维脚本也读 adminApiKey 且工作正常。',
    solution:
      '1. config.json 写 adminApiKey，不写 adminKey。\n2. 改配置类文档/断言前，像验证原断言一样验证你的更正。\n3. 用前现读线上值，不要信文档里记的数字（credentialRpmLimit 记过 85/200，现读 100）。',
    severity: 'high',
    source: 'CLAUDE.md「线上配置」节（2026-08-06 三处独立证据）',
    codePath: 'src/model/config.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'cfg-semantic-traps',
    title: '三个语义陷阱开关：名字与真实后果不对应',
    category: 'config',
    tags: ['配置', '语义陷阱', 'cooldown', 'throttle'],
    problem: '几个开关名字看起来无害，实际后果相反或出人意料。',
    cause: '1) cooldownEnabled=false 不是「不用冷却」而是「429 过的号立刻可重选 = 换号原地打转」；2) inboundQueueTimeoutPassthrough=true 让整形层退化成延迟器（排队 5s 后放行），对重试外挂反而是放大器润滑剂；3) inboundRpmAuto 代码默认 true 但线上刻意 false（内置 AIMD 是单向棘轮：429 砍半、回升要 20s 静默，实测每 6.4s 一次 429 → 单调下滑锁死下限）。',
    solution:
      '1. cooldownEnabled 保持 true（关闭 = 429 号立刻可重选，恶化）。\n2. inboundRpmAuto 保持 false，由外部 throttle-autotune 脚本每 2 分钟调 inboundTargetRpm。\n3. 这些陷阱已被守卫 test_throttle_semantic_traps_defaults_are_documented 钉住，改默认值会红。',
    severity: 'medium',
    source: 'CLAUDE.md「三个语义陷阱」表',
    codePath: 'src/kiro/throttle.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'cfg-deliberate-deviations',
    title: '线上刻意偏离默认值的配置（别改回去）',
    category: 'config',
    tags: ['配置', '默认值', 'rateLimit', '生产'],
    problem: '哪些配置线上与代码默认不同且改回去会造成真实故障？',
    cause: 'rateLimitEnabled=false：每号最小间隔 1000ms 会在 241ms 处踢开亲和绑定 → 每次换号 → prompt cache 全丢，且实测速率与 429 率相关性仅 +0.09（防不住风控却让缓存失效）；rpmHardGateOverloadWait=true（与代码默认 false 相反）；七项 tool 容错全开（含默认关的 toolTruncationRecovery，宁可整轮重试也不下发半截参数）；trustForwardedHeader=false（sub2api 不转发 XFF，开了也拿不到真实 IP，反而 IP 黑名单会一封封全部流量）。',
    solution:
      '1. 调这些开关前先读 CL AUDE.md 依据与运维仓 docs/02-tuning.md。\n2. 不要按「代码默认值」推断线上行为。\n3. 改限流类配置前先做控制实验。',
    severity: 'medium',
    source: 'CLAUDE.md「线上配置参考」表',
    codePath: 'src/kiro/rate_limiter.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'cfg-alert-webhook',
    title: '告警 webhook 配置：冷却去重 + SSRF 风险',
    category: 'config',
    tags: ['配置', '告警', 'webhook', 'SSRF'],
    problem: '怎么配置告警推送？改完为什么不生效？',
    cause: '配置 alertWebhookUrl（+ alertCooldownSecs 默认 600s）后，关键自愈事件（吸收预算耗尽/failover 号全灭/重试配额耗尽/429 风暴）POST {key, value, window_secs, host} 到该地址；同 key 冷却窗口内只发一次，窗口内重复事件只累计计数。热更不生效（provider 构造时注入），改后需重启。',
    solution:
      '1. 改 alertWebhookUrl 后必须重启服务。\n2. 安全：网关会向该 URL 发请求，建议填内网不可达、只能外联的告警服务，绝不填内网管理面地址（SSRF 风险自负）。\n3. 告警文案/状态码改动前先 grep 仓外消费者（kiro_shield.py 按 body 文案分类，503 必须含 COOLING_MARKERS 词，否则 Retry-After 被丢弃）。',
    severity: 'medium',
    source: 'docs/DEPLOYMENT.md §6.4 + CLAUDE.md「改客户端可见的状态码前先 grep 仓外消费者」',
    codePath: 'src/common/alerting.rs',
    updatedAt: '2026-08-14',
  },

  // ============ security（安全）============
  {
    id: 'sec-at-rest-backup',
    title: 'at-rest 加密：备份时密钥必须与密文分离',
    category: 'security',
    tags: ['安全', '加密', '备份', '.at_rest.key'],
    problem: '开 at-rest 加密后，备份/迁移怎么做才安全？',
    cause: 'credentials.json 可开 at-rest 加密（XChaCha20-Poly1305），密钥 .at_rest.key（32 字节 0600）与凭据同目录。整目录打包备份会把密钥与密文一起带走 = 备份泄露即凭据全解（access_token/refresh_token/api_key/proxy_password 全部可解密）。',
    solution:
      '1. 备份包可以含 credentials.json（密文），但 .at_rest.key 必须单独保管（如密码管理器），绝不允许进凭据备份包。\n2. 整目录打包的备份脚本先加 .at_rest.key 排除。\n3. 密钥丢失 = 密文永久解不开：迁移/删除密钥前先在设置页导出明文凭据。\n4. Windows 上密钥文件权限收紧是 no-op（NTFS 限制），任何本地进程可读。',
    severity: 'high',
    source: 'docs/SECURITY-BACKUP.md + docs/CRASHLOOP-ROLLBACK.md §6',
    codePath: 'src/common/secret_store.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'sec-region-whitelist',
    title: 'region 白名单：防污染值拼进上游 host',
    category: 'security',
    tags: ['安全', 'region', '白名单', '注入'],
    problem: '凭据里带恶意 region 会不会把请求打到攻击者域名？',
    cause: '凭据 region/auth_region/api_region + idc 上号 region 必须过 SUPPORTED_KIRO_REGIONS 白名单，污染值不再拼进上游 host（否则 refresh_token 可能被 POST 到攻击者域）。',
    solution:
      '1. 手工编辑 credentials.json 时 region 只填白名单内的值。\n2. 上号流程（idc/region 探测）自带白名单校验。\n3. 排障看到请求打到奇怪 host 时，先检查凭据 region 是否过白名单。',
    severity: 'high',
    source: 'docs/ARCHITECTURE.md §七（region 白名单 H3/M1）+ docs/MODULES.md §4',
    codePath: 'src/kiro/regions.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'sec-csp-session',
    title: 'CSP 头与 adminKey 会话存储（XSS 接管链的最后一块）',
    category: 'security',
    tags: ['安全', 'CSP', 'XSS', 'sessionStorage'],
    problem: '面板 XSS 后攻击者能拿到什么？',
    cause: '历史上 adminKey 明文存 localStorage + 全仓无 CSP + 背景图代理 MIME 缺口 = 完整接管链。已修复：CSP 头 + adminKey 改存 sessionStorage（关标签即清）+ 背景图 MIME 白名单 + nosniff。at-rest 加密不抗本机攻击者（设计边界）。',
    solution:
      '1. 面板 Key 存 sessionStorage，关闭标签页即清除，每次重开需重新登录是正常行为。\n2. 新开的响应端点必须保持 CSP/nosniff 与 MIME 白名单纪律。\n3. 后台进程（同账号）可读内存态密钥，at-rest 加密只防「密文被拷走」，不防本机攻击者。',
    severity: 'medium',
    source: 'docs/archive/HISTORY.md #16 + CURRENT.md 波次 3（CSP + sessionStorage）+ docs/SECURITY-BACKUP.md 泄露场景表',
    codePath: 'src/admin_ui/router.rs',
    updatedAt: '2026-08-14',
  },
  {
    id: 'sec-auth-fail-closed',
    title: '认证 fail-closed：空 apiKey 拒绝启动',
    category: 'security',
    tags: ['安全', '认证', 'apiKey', 'fail-closed'],
    problem: '为什么配置里 apiKey 为空时服务拒绝启动？',
    cause: '设计决策：空白 apiKey 拒绝启动，防 fail-open 匿名消耗（别人连上来白嫖上游额度）。认证走 constant_time_eq 恒定时间比较防时序攻击。',
    solution:
      '1. 配置里 api_key 必须非空才能启动。\n2. 认证失败返回 401，客户端应据此换 Key 而非盲目重试。\n3. IP 白名单/每-IP 限流三者全未配时不挂中间件（零开销），配了才生效。',
    severity: 'low',
    source: 'docs/ARCHITECTURE.md 启动流程第 5 步 + §七',
    codePath: 'src/common/auth.rs',
    updatedAt: '2026-08-14',
  },
]

// ============ 模块地图（src/）============
export const HELP_MODULES: HelpModule[] = [
  {
    path: 'src/kiro/token_manager.rs',
    name: 'token_manager',
    role: '多凭据管理核心：选号（12 键排序）、Token 刷新、亲和、禁用/冷却状态、凭据 CRUD 与回收站。全仓最大文件，设计上刻意不拆。',
    keyFiles: ['token_manager.rs'],
  },
  {
    path: 'src/kiro/provider.rs',
    name: 'provider',
    role: '核心代理器：重试/故障转移/吸收层/入站闸门/Client 缓存/动态重试预算。调用 token_manager 选号后打上游。',
    keyFiles: ['provider.rs'],
  },
  {
    path: 'src/kiro/health.rs',
    name: 'health',
    role: 'AIMD 熔断器 + EWMA 健康分 + 族级连坐（family_key），p_avail 供选号排序权重。',
    keyFiles: ['health.rs'],
  },
  {
    path: 'src/kiro/cooldown.rs',
    name: 'cooldown',
    role: '8 种冷却原因 + 差异化时长 + 落盘持久化（kiro_cooldown.json），是选号硬门之一。',
    keyFiles: ['cooldown.rs'],
  },
  {
    path: 'src/kiro/passthrough.rs',
    name: 'passthrough',
    role: '透传模式（custom_api 代挂号）：零转换字节透传 + 透传级过滤器（DSML 剥离/thinking/空流守卫）。线上 100% 流量走此路径。',
    keyFiles: ['passthrough.rs', 'passthrough_think_filter.rs'],
  },
  {
    path: 'src/kiro/throttle.rs',
    name: 'throttle',
    role: '入站整形令牌桶（AIMD 自动 RPM 挡），三个语义陷阱开关所在。',
    keyFiles: ['throttle.rs'],
  },
  {
    path: 'src/kiro/scheduling.rs',
    name: 'scheduling',
    role: 'InflightGuard（RAII 在途计数）+ RpmTracker（60s 滚动窗），选号原子提交的组成部分。',
    keyFiles: ['scheduling.rs'],
  },
  {
    path: 'src/kiro/affinity.rs',
    name: 'affinity',
    role: '会话亲和（session_id → credential_id，TTL），稳态请求复用同一号保住前缀缓存。',
    keyFiles: ['affinity.rs'],
  },
  {
    path: 'src/kiro/endpoint/',
    name: 'endpoint',
    role: '上游端点注册表：IDE（runtime.{region}.kiro.dev，OAuth 号）与 CLI（q.{region}.amazonaws.com + X-Amz-Target，ksk_ 号），按凭据类型绑定不可互换。',
    keyFiles: ['mod.rs', 'ide.rs', 'cli.rs'],
  },
  {
    path: 'src/anthropic/handlers.rs',
    name: 'anthropic/handlers',
    role: '请求入口：/v1 与 /cc/v1、流式/非流式分派、WebSearch 回灌分派、压缩重试循环、入站闸门调用。',
    keyFiles: ['handlers.rs'],
  },
  {
    path: 'src/anthropic/converter.rs',
    name: 'anthropic/converter',
    role: 'Anthropic → Kiro 格式转换（三态状态机）+ 环境噪音剥离 + continuationId 派生 + 模型映射。',
    keyFiles: ['converter.rs'],
  },
  {
    path: 'src/anthropic/stream.rs',
    name: 'anthropic/stream',
    role: 'Kiro event-stream → Anthropic SSE 回转（Stream/Buffered 双上下文）+ thinking 提取 + DSML/XML 泄漏过滤。',
    keyFiles: ['stream.rs'],
  },
  {
    path: 'src/anthropic/websearch.rs',
    name: 'anthropic/websearch',
    role: 'MCP WebSearch：快路径 + 混合工具回灌循环（最多 5 轮）+ 结果块渲染。TTFB 改造对象。',
    keyFiles: ['websearch.rs'],
  },
  {
    path: 'src/anthropic/compressor.rs',
    name: 'anthropic/compressor',
    role: '输入压缩（空白折叠 + tool_result 头尾截断），规避上游 ~5MiB 请求体 400。',
    keyFiles: ['compressor.rs'],
  },
  {
    path: 'src/anthropic/cache_fingerprint.rs',
    name: 'anthropic/cache_fingerprint',
    role: '缓存指纹模拟器（cache 链 Layer 3）：纯内存最长公共前缀命中 + 会话隔离 + TTL 拆分。',
    keyFiles: ['cache_fingerprint.rs'],
  },
  {
    path: 'src/admin/service.rs',
    name: 'admin/service',
    role: 'Admin 业务逻辑核心：凭据 CRUD、余额缓存、配置热重载派发、三写路径同锁、诊断快照、OTA 任务。',
    keyFiles: ['service.rs'],
  },
  {
    path: 'src/admin/router.rs',
    name: 'admin/router',
    role: 'Admin 路由装配（鉴权路由 + 公开 OAuth 回调），端点清单见 ARCHITECTURE §十一。',
    keyFiles: ['router.rs', 'handlers.rs', 'types.rs'],
  },
  {
    path: 'src/usage/',
    name: 'usage',
    role: '用量管道：专用 OS 线程 + SyncSender（满则丢弃），SQLite 明细 + JSONL + 内存预聚合双 sink。',
    keyFiles: ['pipeline.rs', 'trace_db.rs', 'usage_stats.rs', 'record.rs'],
  },
  {
    path: 'src/common/alerting.rs',
    name: 'common/alerting',
    role: '告警 webhook：8 个自愈事件接入点，冷却去重 + 失败重试上限，纯函数三测。',
    keyFiles: ['alerting.rs'],
  },
  {
    path: 'src/common/fs_atomic.rs',
    name: 'common/fs_atomic',
    role: '原子写文件（temp → fsync → rename），配置/凭据落盘的安全写路径。',
    keyFiles: ['fs_atomic.rs'],
  },
  {
    path: 'src/common/ssrf.rs',
    name: 'common/ssrf',
    role: '出站 URL SSRF 防护：scheme 校验、IP 段黑名单（含 6to4/IPv4-mapped）、DNS 固定防 rebinding、禁重定向。',
    keyFiles: ['ssrf.rs'],
  },
  {
    path: 'src/common/secret_store.rs',
    name: 'common/secret_store',
    role: '凭据 at-rest 加密（XChaCha20-Poly1305）+ 密钥文件管理（.at_rest.key）。',
    keyFiles: ['secret_store.rs'],
  },
  {
    path: 'src/admin_ui/router.rs',
    name: 'admin_ui',
    role: 'rust-embed 内嵌 React SPA + 登录页背景图代理（含 SSRF 防护 + MIME 白名单）。',
    keyFiles: ['router.rs'],
  },
  {
    path: 'src/openai/',
    name: 'openai',
    role: 'OpenAI 兼容层（convert/handlers/types/mod），OpenAI 格式入站转换与用量口径。',
    keyFiles: ['convert.rs', 'handlers.rs', 'types.rs', 'mod.rs'],
  },
]

// ============ 请求链路图（横向步骤）============
export const HELP_CHAIN: HelpChainStep[] = [
  {
    id: 'chain-inbound',
    name: '客户端入站',
    desc: '可选 IP 白名单/每-IP 限流（XFF 最右段）→ CORS → Body 限制（默认 256MiB）→ 请求进入网关。',
    codePath: 'src/common/security.rs',
  },
  {
    id: 'chain-auth-gate',
    name: '鉴权与入站闸门',
    desc: 'constant_time_eq 验证 x-api-key / Bearer（空 key fail-closed），随后过入站整形闸门（令牌桶，超时 429 带 Retry-After）。',
    codePath: 'src/anthropic/middleware.rs',
  },
  {
    id: 'chain-convert',
    name: '转换层',
    desc: 'Anthropic → Kiro 格式转换：模型映射、三态状态机、环境噪音剥离、continuationId 派生。',
    codePath: 'src/anthropic/converter.rs',
  },
  {
    id: 'chain-compress',
    name: '压缩层',
    desc: '空白折叠 + tool_result 截断；大请求触发压缩重试循环（最多 3 次，target 逐步压狠，64KiB 下限）。',
    codePath: 'src/anthropic/compressor.rs',
  },
  {
    id: 'chain-select',
    name: '选号调度',
    desc: '会话亲和 → 12 键排序选号（同锁内 inflight+1 + rpm.record 原子提交）→ 健康/冷却/白名单硬门过滤。',
    codePath: 'src/kiro/token_manager.rs',
  },
  {
    id: 'chain-upstream',
    name: '上游调用与吸收重试',
    desc: '吸收层（预算内吞 429 不让客户端看到）→ 换号/故障转移（动态重试预算 + 45s 墙钟）→ 403 换区自愈 → AWS event-stream 上行。',
    codePath: 'src/kiro/provider.rs',
  },
  {
    id: 'chain-sse',
    name: '响应流 / SSE',
    desc: 'event-stream 解码（双 CRC）→ Kiro events → Anthropic SSE（message_start → deltas → message_stop）+ thinking 提取 + 过滤链。',
    codePath: 'src/anthropic/stream.rs',
  },
  {
    id: 'chain-usage',
    name: '用量记录',
    desc: '请求路径 try_send 非阻塞入队 → 专用 OS 线程 → SQLite 逐条 + JSONL + 内存预聚合（满则丢弃 + 计数）。',
    codePath: 'src/usage/pipeline.rs',
  },
  {
    id: 'chain-alert',
    name: '告警与冷却',
    desc: '失败闭环：429/5xx/风控分别计入冷却（差异化时长）与健康分（AIMD/族级连坐）；关键自愈事件经 webhook 告警（冷却去重）。',
    codePath: 'src/common/alerting.rs',
  },
]
