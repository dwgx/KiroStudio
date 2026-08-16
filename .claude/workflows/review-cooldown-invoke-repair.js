export const meta = {
  name: 'review-cooldown-invoke-repair',
  description: 'Adversarial multi-agent review of cooldown/scheduling and invoke JSON repair paths',
  phases: [
    { title: 'Find', detail: '9 dimension-specific bug finders over cooldown/scheduling/repair code' },
    { title: 'Verify', detail: '2 adversarial refuters per finding, majority vote' },
  ],
}

const REPO = '/Users/dwgx/Documents/Project/KiroStudio'

const FINDINGS_SCHEMA = {
  type: 'object',
  required: ['findings'],
  properties: {
    findings: {
      type: 'array',
      items: {
        type: 'object',
        required: ['title', 'file', 'line', 'severity', 'description', 'failure_scenario'],
        properties: {
          title: { type: 'string' },
          file: { type: 'string' },
          line: { type: 'integer' },
          severity: { type: 'string', enum: ['critical', 'high', 'medium', 'low'] },
          description: { type: 'string' },
          failure_scenario: { type: 'string', description: 'Concrete inputs/state leading to wrong behavior' },
          suggested_fix: { type: 'string' },
        },
      },
    },
  },
}

const VERDICT_SCHEMA = {
  type: 'object',
  required: ['refuted', 'reasoning'],
  properties: {
    refuted: { type: 'boolean', description: 'true if the finding is NOT a real bug' },
    reasoning: { type: 'string' },
    severity_adjustment: { type: 'string', description: 'optional: suggest different severity' },
  },
}

const COMMON = `你在审查 Rust 项目 KiroStudio（${REPO}），一个 Anthropic Messages API 兼容的反向代理网关（Anthropic 请求 → Kiro/AWS Q 上游 → 翻译回 Anthropic SSE）。
只报告真实缺陷：会导致错误行为、竞态、死锁、数据损坏、协议违规、饥饿、泄漏的问题。不报风格问题、不报"可以更好"的建议。
每条发现必须给出具体的触发场景（什么输入/什么并发时序 → 什么错误结果）。宁缺毋滥：读完整上下文再下结论，函数间的调用关系要实际追踪，不要臆测。
输出 JSON findings；没有真问题就返回空数组。`

const DIMENSIONS = [
  {
    key: 'cooldown-core',
    prompt: `${COMMON}
维度：冷却机制核心逻辑。精读 src/kiro/cooldown.rs 全文（630行）。
重点：8种冷却原因的时长映射是否自洽；set_cooldown_with_duration 的"保护已有更长冷却不被缩短"逻辑是否有边界错误（相同时长、过期条目、时钟回拨）；冷却过期判定与清除路径；并发下同一凭据同时被设置不同原因冷却的竞态；冷却状态与 token_manager 的交互（grep 调用点）。`,
  },
  {
    key: 'cooldown-consumers',
    prompt: `${COMMON}
维度：冷却的调用方一致性。grep 整个 src/ 中所有调用 cooldown（set_cooldown / is_cooling / clear 等）的位置，逐个检查：错误分类到冷却原因的映射是否正确（429/401/403/500/overloaded/MODEL_TEMPORARILY_UNAVAILABLE 各自触发什么冷却）；provider.rs 重试路径里是否存在"该冷却没冷却"或"不该冷却却冷却"的分支；v0.7.43 改动"MODEL_TEMPORARILY_UNAVAILABLE 不计健康惩罚"是否在所有路径生效；凭据禁用与冷却的重复/遗漏。`,
  },
  {
    key: 'scheduling-atomic',
    prompt: `${COMMON}
维度：原子选号与并发调度。精读 src/kiro/scheduling.rs 全文（InflightGuard RAII + RpmTracker 60s滑窗），再在 src/kiro/token_manager.rs 中找到选号临界区（balanced 8键选号，选号+inflight+1+rpm.record 应在同一把 parking_lot::Mutex 内）。
重点：InflightGuard 的 Drop 是否在所有失败路径（panic、early return、请求取消）都正确执行减一；RpmTracker 滑窗的边界（跨窗口瞬间、时钟精度）；选号临界区是否真的原子——找出任何在锁外读取后在锁内使用的过期数据（TOCTOU）；亲和（affinity.rs）与选号的交互是否绕过 inflight 计数。`,
  },
  {
    key: 'health-circuit',
    prompt: `${COMMON}
维度：AIMD 熔断器与健康分。精读 src/kiro/health.rs 全文（474行）。
重点：EWMA 健康分更新的数值边界（除零、NaN、初始值）；tick_circuit 状态机转换（closed→open→half-open→closed）的时序缺陷；族级连坐（family_key）的惩罚传播——一个号的 429 是否会错误地惩罚不该惩罚的号（对照 v0.7.42 "普通 429 用每凭据健康键而非族键"的修复是否完整）；snapshot() 修复后（先 tick_circuit 再读）是否引入新问题（get_mut 持锁时长、重入）。`,
  },
  {
    key: 'ratelimit-throttle',
    prompt: `${COMMON}
维度：限流与入站整形。精读 src/kiro/rate_limiter.rs（523行）与 src/kiro/throttle.rs 全文。
重点：每日限额的日期翻转（时区、UTC vs 本地）；最小间隔与退避抖动的计算溢出/下溢；AIMD RPM 自动挡的 step up/down 竞态；v0.7.40 改 notify_one 后是否存在唤醒丢失（waiter 在 notify 前还没 park）导致排队请求永久卡住；inbound_queue_timeout_passthrough 超时放行与令牌桶状态的一致性。`,
  },
  {
    key: 'invoke-sniff',
    prompt: `${COMMON}
维度：invoke 嗅探缓冲。精读 src/anthropic/stream.rs 中 invoke_sniff_buffer 相关全部代码：字段定义(~792行)、push 点(~1400,~1569)、drain_invoke_sniff_buffer(~1750-1843)、flush_invoke_sniff_buffer(~1844)、流结束调用点(~2272)。
重点：部分匹配保留逻辑（半个 <invoke 标签跨 chunk 到达时 remainder 的切分是否正确，UTF-8 多字节字符边界会不会 panic 或切坏）；flush=true 时残留半块的处理；thinking 模式路由修复(v0.7.40)后是否所有文本路径都过 sniff buffer（找绕过点）；嗅探到 invoke 后的状态转换是否会丢失已缓冲的前置文本；连续多个 invoke 块。`,
  },
  {
    key: 'json-repair',
    prompt: `${COMMON}
维度：工具参数 JSON 修复。精读 src/anthropic/stream.rs 中 repair_tool_json(~2778)、repair_json_glued(~2810)、repair_json_char_level(~2841) 及其全部调用点，以及 docs/INVALID-TOOL-PARAMETERS.md 了解设计意图。
重点：char_level 修复对合法 JSON 的误伤（合法的 \\\\uXXXX 转义、字符串内的大括号、嵌套引号）；glued 模式检测的假阳性（正常 JSON 中恰好出现 }{ 的字符串字面量）；截断恢复补齐括号的栈逻辑（字符串内的括号计数、转义引号 \\\\" 后的状态机）；修复后 JSON 语义是否可能与模型意图相反（静默改坏参数比报错更糟）；None 返回路径调用方如何处理。`,
  },
  {
    key: 'provider-retry',
    prompt: `${COMMON}
维度：provider 重试/故障转移与冷却调度的整体交互。精读 src/kiro/provider.rs 全文（1291行）。
重点：动态重试预算的计算与耗尽行为；故障转移换号时旧号的 InflightGuard/冷却/健康惩罚是否正确结算；重试循环中 overloaded 2s 退避 + overload_fallback_model 切换的边界（fallback 模型也 overloaded、fallback 与原模型能力不符）；Client 缓存的失效条件；所有凭据全冷却/全禁用时的行为（是否有 kiro.rs 式的自愈重置，还是直接对客户端报错、报什么错）；流式响应中途失败的重试是否会重复发送已发出的 SSE 事件。`,
  },
  {
    key: 'refresh-locks',
    prompt: `${COMMON}
维度：v0.7.43 每凭据刷新锁重构。在 src/kiro/token_manager.rs 中精读刷新相关代码：per-credential refresh lock 的获取/释放、1s/2s/4s 退避重试、双重检测（拿锁后二次检查是否已被他人刷新）、预刷新循环(src/kiro/refresh_loop.rs)与热路径按需刷新的交互。
重点：每凭据锁 map 的条目生命周期（凭据删除后锁条目泄漏或复用错乱）；退避重试期间持锁是否阻塞同凭据的其他请求过久；二次检查的条件是否与刷新前判定同口径（对照 CHANGELOG 0.7.38 A3/C2 的教训）；刷新失败计数与冷却/禁用的联动；API Key 凭据是否在所有刷新入口都被跳过。`,
  },
]

phase('Find')
log('扇出 9 个维度审查 agent')

const results = await pipeline(
  DIMENSIONS,
  d => agent(d.prompt, { label: `find:${d.key}`, phase: 'Find', schema: FINDINGS_SCHEMA, effort: 'high' }),
  (r, d) => {
    if (!r || !r.findings || !r.findings.length) return []
    log(`${d.key}: ${r.findings.length} 条候选`)
    return parallel(r.findings.map(f => () =>
      parallel([0, 1].map(i => () =>
        agent(`${COMMON}
你是对抗性核实者 #${i + 1}。下面是另一个审查者声称的缺陷，你的任务是**证伪它**。打开 ${REPO} 中相关文件，读完整上下文（包括调用方和被调方），验证触发场景是否真的成立。
若场景不成立、被其他代码防住、或实际影响可忽略 → refuted=true。若确认是真缺陷 → refuted=false 并说明为何反驳不成立。拿不准时倾向 refuted=true（宁可漏报不可误报）。

声称的缺陷：
标题：${f.title}
位置：${f.file}:${f.line}
严重度：${f.severity}
描述：${f.description}
触发场景：${f.failure_scenario}`, { label: `verify:${f.title.slice(0, 30)}`, phase: 'Verify', schema: VERDICT_SCHEMA, effort: 'high' })
      )).then(votes => {
        const valid = votes.filter(Boolean)
        const confirmed = valid.length > 0 && valid.every(v => !v.refuted)
        return { ...f, confirmed, votes: valid.map(v => ({ refuted: v.refuted, reasoning: v.reasoning.slice(0, 300) })) }
      })
    ))
  }
)

const all = results.filter(Boolean).flat().filter(Boolean)
const confirmed = all.filter(f => f.confirmed)
const rejected = all.filter(f => !f.confirmed)
log(`候选 ${all.length} 条，确认 ${confirmed.length} 条，被证伪 ${rejected.length} 条`)

const order = { critical: 0, high: 1, medium: 2, low: 3 }
confirmed.sort((a, b) => (order[a.severity] ?? 9) - (order[b.severity] ?? 9))

return {
  confirmed: confirmed.map(f => ({ title: f.title, file: f.file, line: f.line, severity: f.severity, description: f.description, failure_scenario: f.failure_scenario, suggested_fix: f.suggested_fix })),
  rejected: rejected.map(f => ({ title: f.title, file: f.file, line: f.line, reason: f.votes.find(v => v.refuted)?.reasoning })),
}