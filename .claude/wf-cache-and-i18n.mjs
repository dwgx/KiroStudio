export const meta = {
  name: 'kirostudio-cache-and-scheduling-docs',
  description: '两路并发：prompt cache 开关复活 + 调度开关中文说明重写',
  phases: [{ title: 'Build', detail: '2 路并发产出精确补丁规格' }],
}

// 只要 2 路。上一轮 8 路 × high effort 把本地网关打爆（109 万 token，全部 502）。
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
          change: { type: 'string', description: '改成什么，含完整代码/文案' },
        },
      },
    },
    i18n_keys: {
      type: 'array',
      description: '需要新增/修改的 i18n key（三语齐全）',
      items: {
        type: 'object',
        required: ['key', 'zh', 'en', 'ja'],
        properties: {
          key: { type: 'string' },
          zh: { type: 'string' },
          en: { type: 'string' },
          ja: { type: 'string' },
          note: { type: 'string' },
        },
      },
    },
    mismatches: {
      type: 'array',
      description: '前端文案与后端实际行为不符之处（最高优先）',
      items: { type: 'string' },
    },
    tests: { type: 'array', items: { type: 'string' } },
    open_questions: { type: 'array', items: { type: 'string' } },
  },
}

const REPO = [
  '仓库 /Users/dwgx/Documents/Project/KiroStudio（Rust/Axum 网关 + React 面板）。',
  '',
  '硬约束：',
  '- **只读侦察，绝对不要修改任何文件**。产出补丁规格，由主会话落盘。',
  '- 工作树有 40 个未提交文件（多会话并行）。禁止 git checkout/stash/reset/commit/add，禁止全仓 cargo fmt。',
  '- 测试全在各源文件内联 #[cfg(test)]；src/test.rs 与 src/debug.rs 是孤儿不参与编译。',
  '- 构建一律 --no-default-features。',
  '- 注释与文案中文为主。',
  '- 每条结论必须引用真实 file:line，禁止凭印象断言。',
  '- **节制工具调用**：优先用 grep 精确定位再读片段，不要整文件通读大文件',
  '  （token_manager.rs 约 5900 行、stream.rs 约 2600 行）。',
].join('\n')

phase('Build')

const results = await parallel([
  () =>
    agent(
      [
        REPO,
        '',
        '【任务 1】把 prompt cache 的死配置复活成真开关。',
        '',
        '已确证的事实（来自 docs/CACHE-EXP0-RESULT.md，已实测，不要重复实验）：',
        '- 上游 runtime.{region}.kiro.dev 的 metadataEvent 只有 {"stopReason":...}，',
        '  不发 tokenUsage / cacheReadInputTokens / cacheWriteInputTokens。',
        '- 所以 RFC 的 L2-2（接入上游真值）已取消。cache 数字全部是网关本地影子估算。',
        '',
        '要做的事：',
        '',
        '1. `promptCacheEnabled` 现在是**死配置**：grep 确认全仓零读取点',
        '   （我已初步确认只有 3 处注释提到它，src/anthropic/types.rs:205、',
        '   src/anthropic/converter.rs:380 和 :400）。请复核并设计把它变成真开关。',
        '   明确定义它控制什么，候选：',
        '   (a) 是否做影子缓存估算本身（省 CPU）',
        '   (b) 是否把 cache_read_input_tokens / cache_creation_input_tokens 下发给客户端',
        '   (c) 是否在用量统计里记 cache 字段',
        '   给出结论 + 理由。**关闭时必须是「不注入该字段」而不是「注入 0」**——',
        '   对 Anthropic 客户端来说 0 表示"确实没命中"，字段缺失表示"未记账"，语义不同，请论证。',
        '',
        '2. `promptCacheTtlSeconds` 现在 3600。找到它的**实际读取点**，说清它到底影响什么',
        '   （shadow cache 条目过期？continuationId 恢复窗口？）。RFC 建议改 300 对齐上游 5m，',
        '   请判断这个建议对不对、会不会让续传请求的 cache_read 归零。',
        '   顺带核实 v0.7.43 那条「经 continuationId 恢复 shadow cache 估算」的现状',
        '   （grep continuationId / shadow_cache / cache_usage）。',
        '',
        '3. cache_breakdown 注入的是**估算值**，却当真值下发给客户端（Claude Code 会显示',
        '   "缓存命中 12000 tokens"）。读 src/anthropic/handlers.rs 的 apply_cache_breakdown',
        '   （约 1241 行）与三个赋值点（约 1025 / 1249 / 2099 行）。',
        '   这是产品判断不只是技术判断：停止下发则面板缓存统计恒 0，继续下发则数字是编的。',
        '   请给出推荐方案并论证。候选：保留内部统计但不下发客户端 / 加字段标注 estimated /',
        '   由 promptCacheEnabled 统一控制。',
        '',
        '4. 注意 src/usage/record.rs 里已有 clamp_cache_to_input() 与 billed_input_tokens()，',
        '   以及 input_tokens 是 **gross 口径**（含 cache）而响应体的 usage.input_tokens 是',
        '   **billed 口径**这个已文档化的区别。你的方案不能破坏这个不变量。',
        '',
        '5. 配置项若有增改，注意 src/model/config.rs 的三层热重载约定',
        '   （TIER1 ArcSwap / TIER2 任务 respawn / TIER3 进程级 Atomic）——',
        '   说清这个字段属于哪层、要不要同步镜像。',
        '',
        '产出 patch_plan（file + anchor + change，anchor 要能被 Edit 精确匹配）+ tests。',
        'i18n_keys 填设置页需要的缓存相关文案（若你的方案要动面板）。',
      ].join('\n'),
      { schema: SPEC, label: 'cache-switch', effort: 'high' }
    ),
  () =>
    agent(
      [
        REPO,
        '',
        '【任务 2】设置页「调度」相关开关的中文说明重写。运维现在看不懂开关影响什么。',
        '',
        '线上这些项**刻意偏离代码默认值**，改回默认会造成真实故障（依据来自实测）：',
        '',
        '- `rateLimitEnabled` = false：它的每号最小间隔 1000ms 会在 241ms 处踢开亲和绑定',
        '  → 每次换号 → prompt cache 全丢。5339 样本实测「速率 vs 429 率」相关性仅 +0.09，',
        '  即它防不住风控却让缓存失效。',
        '- `inboundRpmAuto` = false：内置 AIMD 是单向棘轮，429 就砍半，回升要 20s 静默 ×N，',
        '  而实测每 6.4s 就有一次 429 → 单调下滑锁死在下限。实测卡在 30 RPM 而号池能跑 216。',
        '- `inboundTargetRpm`：由 throttle-autotune.timer 每 2 分钟按可用号数自动调',
        '  （号数 × 72 × 80%），补号后无需人工干预。',
        '- `rpmHardGateOverloadWait` = false（= 代码默认）：true 会在整池饱和时背压等待，',
        '  高并发下请求堆在网关里。',
        '- `credentialRpmLimit` = 85：替代内置兜底 30。每号有效阈值 = 85 × headroom 85% = 72。',
        '- `trustForwardedHeader` = false：上游 sub2api 的透传白名单里没有 X-Forwarded-For',
        '  也不转发它，开了拿不到真实用户 IP；而 KiroStudio 看到的 client_ip 恒为服务器自身',
        '  地址，若按它配 IP 黑名单会一封封掉全部流量。',
        '',
        '任务：',
        '',
        '1. 列出设置页所有调度/限流/亲和/熔断相关项。读',
        '   admin-ui/src/components/settings-page.tsx（用 grep 定位，文件很大）',
        '   与 admin-ui/src/i18n/resources/zh.json（1500 键，用 python/grep 过滤，别通读）。',
        '   给出每项的 i18n key + 现有文案。',
        '',
        '2. **与后端真实语义对账**（这部分优先级最高）：逐项去 src/model/config.rs 读实际',
        '   默认值与文档注释，再去真正的读取点确认它影响什么',
        '   （src/kiro/throttle.rs / rate_limiter.rs / scheduling.rs / affinity.rs /',
        '   health.rs / token_manager.rs）。把**前端文案与后端实际行为不符、或默认值写错**',
        '   的地方全部找出来，填进 mismatches 字段。这类错误会直接导致运维误操作。',
        '',
        '3. 为每项写更好的中文说明，标准：',
        '   - 讲「开了会发生什么 / 关了会发生什么」，不是复述字段名；',
        '   - 有实测依据的把关键数字写进去（如"实测 5339 样本相关性仅 +0.09"），让人不敢乱改；',
        '   - 标注默认值；线上刻意偏离默认的项要给警示；',
        '   - 相互影响的项要交叉引用（inboundRpmAuto ↔ inboundTargetRpm、',
        '     rateLimitEnabled ↔ 会话亲和 ↔ prompt cache）；',
        '   - 主文案一行，细节放 hint/tooltip 次级文案。',
        '',
        '4. en/ja 同步。三语 key 必须完全对齐（主会话有脚本跑双向 diff）。',
        '   日英不必像中文详尽，但不能缺 key。',
        '',
        '产出 i18n_keys（key + zh/en/ja 三语全填）+ patch_plan（若需要加 hint 行等 tsx 结构改动）',
        '+ mismatches（文案与实际不符清单）。',
      ].join('\n'),
      { schema: SPEC, label: 'scheduling-docs', effort: 'high' }
    ),
])

const ok = results.filter(Boolean)
log(`完成 ${ok.length}/2 路`)
return { cache: results[0], scheduling: results[1] }
