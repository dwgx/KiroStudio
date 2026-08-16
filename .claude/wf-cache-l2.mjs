export const meta = {
  name: 'kirostudio-cache-l2',
  description: '缓存 L2：估算值标注下发 + EXP-1/EXP-2 本地探针脚本',
  phases: [{ title: 'Build', detail: '2 路：标注方案 + 探针脚本设计' }],
}

// 只 2 路、effort medium。前两次 workflow（8 路 high、2 路 high）都被本地网关 502 打死。
const SPEC = {
  type: 'object',
  required: ['title', 'root_cause', 'patch_plan', 'tests'],
  properties: {
    title: { type: 'string' },
    root_cause: { type: 'string', description: '现状与设计结论（markdown）' },
    patch_plan: {
      type: 'array',
      items: {
        type: 'object',
        required: ['file', 'anchor', 'change'],
        properties: {
          file: { type: 'string' },
          anchor: { type: 'string', description: '唯一定位串，供 Edit 精确匹配' },
          change: { type: 'string', description: '改成什么，含完整代码' },
        },
      },
    },
    script: { type: 'string', description: '若任务要求产出脚本，完整脚本内容放这里' },
    tests: { type: 'array', items: { type: 'string' } },
    open_questions: { type: 'array', items: { type: 'string' } },
  },
}

const REPO = [
  '仓库 /Users/dwgx/Documents/Project/KiroStudio（Rust/Axum 网关）。',
  '',
  '硬约束：',
  '- **只读侦察，不要修改任何文件**。产出规格，主会话落盘。',
  '- 工作树有 40+ 未提交文件（多会话并行）。禁止 git checkout/stash/reset/commit/add，禁止全仓 cargo fmt。',
  '- 测试全在源文件内联 #[cfg(test)]。构建一律 --no-default-features。',
  '- 中文注释。引用真实 file:line，禁止凭印象断言。',
  '- **节制工具调用**：用 grep 精确定位再读片段，不要通读大文件',
  '  （handlers.rs ~2900 行、stream.rs ~2600 行、token_manager.rs ~6000 行）。',
  '',
  '已确证的关键事实（EXP-0 实测，见 docs/CACHE-EXP0-RESULT.md，不要重复验证）：',
  '- 上游 runtime.{region}.kiro.dev 的 metadataEvent 只有 {"stopReason":...}。',
  '  全窗口 grep tokenUsage|cacheReadInputTokens|cacheWriteInputTokens 零命中。',
  '- 所以网关下发的 cache_read_input_tokens 是 token::count_prefix_tokens 的**本地估算**。',
  '- 主会话刚把 promptCacheEnabled 从死配置接上真读取点：新增',
  '  handlers.rs 的 estimate_cache_breakdown(enabled, prefix_tokens, input_tokens)',
  '  与 PROMPT_CACHE_ENABLED 进程镜像（TIER3），关闭时返回 None（字段整体缺失而非置 0）。',
].join('\n')

phase('Build')

const results = await parallel([
  () =>
    agent(
      [
        REPO,
        '',
        '【任务 1】把下发的 cache 记账**标注为估算**，让客户端能分辨它不是上游真值。',
        '',
        '用户已决策：**继续下发**（保持客户端缓存显示与面板统计不回退），',
        '但要加标注说明是网关估算。不要改成停止下发。',
        '',
        '要做的事：',
        '1. 找到 cache 字段注入客户端响应的**全部**位置。已知线索：',
        '   grep cache_read_input_tokens src/anthropic/stream.rs 有约 5 处注入点',
        '   （usage_json / message_start / message_delta 等），都从 StreamContext.cache_usage 读。',
        '   非流式路径在 handlers.rs。请把完整清单列出来（file:line + 哪条响应事件）。',
        '2. 设计标注方式。用户选的方案是**响应头** X-KiroStudio-Cache-Estimated: true。',
        '   请查证：',
        '   (a) 流式（SSE）与非流式两条路径分别在哪里能加响应头？流式的 header 必须在',
        '       第一个 chunk 之前写，看现有代码是否还有机会（grep Response::builder / IntoResponse）。',
        '   (b) 加了这个头会不会破坏 Claude Code / Anthropic SDK 的解析？（自定义 X- 头一般安全，',
        '       但请确认没有 CORS 白名单会拦——grep cors_allowed_origins / expose_headers）',
        '   (c) 是否还应在 SSE 的 usage 对象里加一个字段（如 kirostudio_cache_estimated: true）？',
        '       注意 Anthropic 客户端可能对未知字段严格校验，请给出判断与理由。',
        '3. 该标注只在**实际下发了** cache 字段时出现（promptCacheEnabled=true 且有前缀命中）；',
        '   字段缺失时不应出现，否则头与体自相矛盾。',
        '4. 顺带核实：面板/SQLite 侧的用量统计是否需要同样标注（usage_stats.rs 已有',
        '   cache_read_tokens 字段，前端 i18n 里已有 cacheEstimateNote 之类的键——grep 确认）。',
        '',
        '产出 patch_plan（file + anchor + change，anchor 要能被 Edit 精确匹配）+ tests。',
      ].join('\n'),
      { schema: SPEC, label: 'cache-estimated-marker', effort: 'medium' }
    ),
  () =>
    agent(
      [
        REPO,
        '',
        '【任务 2】设计 EXP-1 / EXP-2 的**本地**探针脚本。',
        '',
        '目标：判断 converter.rs:747 记录的那次 0.141 → 0.075 credit 观测（约 47% 折扣）',
        '是**真的上游前缀缓存折扣**，还是 credit 阶梯取整造成的假象。',
        '这决定 RFC 的 L0（前缀稳定）层有没有价值。',
        '',
        '先读 docs/CACHE-RFC.md 的「实验矩阵」章节（grep EXP-1 / EXP-2 定位，别通读 31KB），',
        '把两个实验的设计、因变量、预算控制照抄出来并落成可执行脚本。',
        '',
        '运行环境（重要）：',
        '- 打**本地** KiroStudio 实例：http://127.0.0.1:8990，它是活的、池里有 9 个号。',
        '- adminKey 与 apiKey 在 /Users/dwgx/Documents/Project/KiroStudio/target/release/config.json',
        '  （脚本应从该文件读，不要硬编码密钥，也不要把密钥打印到 stdout）。',
        '- **不要**打生产 VPS（143.20.230.248）。',
        '',
        '预算控制必须落进脚本（RFC 的约束）：',
        '- QPS ≤ 0.2（即每次请求间隔 ≥ 5 秒）',
        '- 遇 429 立即暂停 5 分钟',
        '- 总请求量 ≤ 350（约 70 credits）',
        '- 脚本要能中断续跑（把已完成的观测追加写入 JSONL，重跑时跳过）',
        '- 每次请求后打印累计请求数与预估 credit 消耗，超预算自动停止',
        '',
        '实验设计要点（请从 RFC 核实并细化）：',
        '- EXP-1（credit 函数标定）：用**递增长度**的输入测 credit 取值，判断 credit 是否为',
        '  粗粒度阶梯。RFC 说若 8 个档位上 credit 取值数 ≤ 3 则不可当命中率仪器，',
        '  EXP-2 须改用 TTFT 作因变量。脚本要自动做这个判定并输出结论。',
        '- EXP-2（是否真有前缀缓存）：同一超长前缀连续打 N 次，看第 2 次起 credit 是否下降、',
        '  TTFT 是否显著下降。要有对照组（每次换不同前缀）。',
        '- 怎么读到 credit：查 KiroStudio 哪个接口/字段能拿到本次请求的 credits_used',
        '  （grep credits_used，可能在 /api/admin/usage/* 或 trace_db）。TTFT 同理',
        '  （grep first_token_ms）。这一步必须查证，不能假设。',
        '',
        '把完整可执行脚本放进 script 字段（Python 3，只用标准库 + 已装的东西；',
        '若必须用第三方库请说明）。脚本要有 --dry-run 模式先打印计划不真打请求。',
        '',
        'patch_plan 填「若实验需要临时代码补丁」（RFC 提到 EXP-1 需要一个临时补丁，请查证是什么）。',
      ].join('\n'),
      { schema: SPEC, label: 'exp-probe-script', effort: 'medium' }
    ),
])

const ok = results.filter(Boolean)
log(`完成 ${ok.length}/2 路`)
return { marker: results[0], probe: results[1] }
