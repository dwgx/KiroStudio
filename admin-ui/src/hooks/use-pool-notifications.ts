import { useEffect, useRef } from 'react'
import { toast } from 'sonner'
import { useCredentials } from '@/hooks/use-credentials'
import { useRatelimitInsights } from '@/hooks/use-usage'
import type { CredentialStatusItem, RateLimitInsight } from '@/types/api'

/**
 * 号池健康事件通知（右下角 toast，跟随全站通知设计系统）。
 *
 * 复用 useCredentials(30s) + useRatelimitInsights(10s) 已有的轮询数据，**只在状态跃迁时**
 * 弹一次通知（用 ref 记住上一轮的"已通知指纹"，避免每次轮询重复刷屏）。零额外上游调用。
 *
 * 覆盖四类事件（dwgx 指定）：
 * - ARN 缺失/解析失败：号缺 hasProfileArn（对话会 400 profileArn is required）
 * - 号死/被禁用：disabled 从 false→true（按 disabledReason 给中文原因）
 * - 余额耗尽/低：disabledReason=QuotaExceeded，或订阅额度耗尽
 * - 可疑活动风控：insights 冷却 reason 含"可疑活动"（账户级软风控，最痛点）
 *
 * 通知去重键设计：每类事件用 `{类型}:{id}:{关键状态}` 做指纹，存进 seenRef。
 * 状态恢复（如号重新启用、冷却结束）时从 seenRef 移除，使下次再发生能重新通知。
 */

/** 号的展示名：别名 > 邮箱 > #id。 */
function credLabel(c: { id: number; name?: string; email?: string }): string {
  if (c.name && c.name.trim()) return c.name.trim()
  if (c.email && c.email.trim()) return c.email.trim()
  return `#${c.id}`
}

/** disabledReason → 中文短语（与后端 DisabledReason 对齐）。 */
function disabledReasonText(reason?: string): string {
  switch (reason) {
    case 'QuotaExceeded':
      return '额度已用尽'
    case 'AccountSuspended':
      return '账号被上游暂停/封禁'
    case 'SuspiciousActivityAuto':
      return '连续可疑活动风控，已自动禁用'
    case 'TooManyFailures':
      return '连续失败过多，已自动禁用'
    case 'RefreshTokenInvalid':
      return 'refreshToken 永久失效'
    case 'SubscriptionInvalid':
      return '订阅失效/降级（INVALID_MODEL_ID），已移出调度'
    case 'InvalidConfig':
      return '凭据配置不完整'
    case 'Manual':
      return '手动禁用'
    default:
      return reason ? `已禁用（${reason}）` : '已禁用'
  }
}

/**
 * 批量发射：同类事件 1-2 条逐条发（保留详细描述），≥3 条合并成一条汇总通知
 * （标题给数量，描述列出前几个 + "等 N 个"），避免号池批量出事时刷屏。
 */
const MERGE_THRESHOLD = 3
function flushBatch(
  _cat: string,
  labels: string[],
  cfg: {
    one: (label: string) => string
    manyTitle: (count: number) => string
    type: 'warning' | 'error'
    desc: string
  },
) {
  if (labels.length === 0) return
  const fire = cfg.type === 'error' ? toast.error : toast.warning
  if (labels.length < MERGE_THRESHOLD) {
    for (const label of labels) {
      fire(cfg.one(label), { description: cfg.desc, duration: cfg.type === 'error' ? 10000 : 8000 })
    }
    return
  }
  const head = labels.slice(0, 3).join('、')
  const rest = labels.length > 3 ? ` 等 ${labels.length} 个` : ''
  fire(cfg.manyTitle(labels.length), {
    description: `${head}${rest}。${cfg.desc}`,
    duration: 11000,
  })
}

export function usePoolNotifications() {
  const { data: creds } = useCredentials()
  const { data: insights } = useRatelimitInsights()

  // 已通知指纹集合：跨轮询保留，状态恢复时移除对应键。
  const seenRef = useRef<Set<string>>(new Set())
  // 首轮不弹历史事件（避免打开面板瞬间把既有问题全刷一遍）——先把当前问题态记进 seen。
  const primedRef = useRef(false)
  // 新号初始化跟踪:knownIds=已见过的号 id(首轮全灌入,不弹);initPending=正在"初始化中"的号
  // (hasProfileArn 尚未翻 true),记 toast key + 起始时刻,用于翻牌成功/超时兜底。
  const knownIdsRef = useRef<Set<number>>(new Set())
  const initPendingRef = useRef<Map<number, { key: string; startedAt: number }>>(new Map())

  useEffect(() => {
    if (!creds?.credentials) return
    const list: CredentialStatusItem[] = creds.credentials
    const seen = seenRef.current

    // 本轮所有"问题态"指纹，用于回收恢复态的键
    const activeKeys = new Set<string>()

    // 批量合并：本轮**新触发**的事件先按类别攒起来，最后统一发；
    // 同类 ≥3 条合并成一条汇总（如"3 个号已禁用"），避免号池批量出事时刷屏。
    type Cat = 'arn' | 'quota' | 'disabled' | 'suspicious'
    const batch: Record<Cat, string[]> = { arn: [], quota: [], disabled: [], suspicious: [] }

    // 标记指纹为"已见"，若是本轮新出现且已过首轮 prime，则归入对应类别的批次。
    const track = (key: string, cat: Cat, label: string) => {
      activeKeys.add(key)
      if (seen.has(key)) return
      seen.add(key)
      if (primedRef.current) batch[cat].push(label)
    }

    // 新号初始化通知:首轮(未 primed)把当前所有 id 灌入 knownIds,不弹(避免刷新页面误判为新号)。
    const known = knownIdsRef.current
    const initPending = initPendingRef.current
    const firstRun = !primedRef.current
    if (firstRun) {
      for (const c of list) known.add(c.id)
    }

    for (const c of list) {
      const label = credLabel(c)
      const isCustomApi = c.authMethod === 'custom_api' || !!c.baseUrl
      // 仅 Kiro 类号(非 custom_api / 非 api_key)有 profileArn 概念,才涉及"初始化中→完成"。
      const needsArn = !isCustomApi && c.authMethod !== 'api_key'

      // ── 新号初始化事件(primed 之后才处理,首轮已全灌 knownIds)──
      if (!firstRun && !known.has(c.id)) {
        known.add(c.id) // 真·新入池号
        if (needsArn && !c.disabled && !c.hasProfileArn) {
          // 需要解析 profileArn 且尚未就绪 → 弹"初始化中"loading,记 pending 等翻牌。
          // (禁用号排除:如 RefreshTokenInvalid→disabled 且无 arn,不该弹"初始化中"永转 + 与"已禁用"矛盾)
          const key = `init:${c.id}`
          toast.loading(`凭据 ${label} 初始化中… 正在刷新 Token、解析 Profile ARN`, { id: key })
          initPending.set(c.id, { key, startedAt: Date.now() })
        } else if (needsArn && !c.disabled && c.hasProfileArn) {
          // 入池即带 arn(如网页 social 号,无中间态)→ 直接"已就绪"。
          toast.success(`凭据 ${label} 已就绪，进入调度`)
        }
        // api_key / custom_api:无 profile 概念,不弹初始化(它们本就即插即用)。
      }

      // 1. ARN 缺失（仅 Kiro 号需要 profileArn；api_key 与 custom_api 代挂号都无此概念）
      //    custom_api 是 Anthropic 兼容中转站,直接打 base_url,根本不走 Kiro profileArn 逻辑,
      //    绝不能对它误报"缺少 Profile ARN / 请刷新 Token"。
      //    ⭐正在初始化中(initPending)的新号也跳过 ARN 缺失告警——它本来就在解析 arn,不是异常。
      if (!c.hasProfileArn && needsArn && !isCustomApi && !c.disabled && !initPending.has(c.id)) {
        track(`arn:${c.id}`, 'arn', label)
      }
      // 2. 号死/被禁用（额度耗尽单独归 quota 语义）
      if (c.disabled) {
        if (c.disabledReason === 'QuotaExceeded') {
          track(`quota:${c.id}`, 'quota', label)
        } else {
          track(`disabled:${c.id}:${c.disabledReason ?? ''}`, 'disabled', `${label}（${disabledReasonText(c.disabledReason)}）`)
        }
      }
    }

    // 新号初始化 pending 翻牌:遍历正在初始化的号,hasProfileArn 变 true → 原地翻成"完成";
    // 号消失(被删)→ 清掉;超 90s 仍未就绪 → 超时告警(ARN 解析失败场景,避免 loading 永转)。
    const INIT_TIMEOUT_MS = 90_000
    for (const [id, info] of Array.from(initPending)) {
      const c = list.find((x) => x.id === id)
      if (!c) {
        toast.dismiss(info.key)
        initPending.delete(id)
        continue
      }
      // 初始化途中被禁用(如刷新失败→RefreshTokenInvalid):立即拆 spinner,别转到超时误报。
      if (c.disabled) {
        toast.dismiss(info.key)
        initPending.delete(id)
        continue
      }
      if (c.hasProfileArn) {
        toast.success(`凭据 ${credLabel(c)} 初始化完成，已进入调度`, { id: info.key })
        initPending.delete(id)
      } else if (Date.now() - info.startedAt > INIT_TIMEOUT_MS) {
        toast.warning(`凭据 ${credLabel(c)} 初始化超时，请手动刷新 Token`, { id: info.key })
        initPending.delete(id)
      }
    }

    // 3. 可疑活动风控：从 insights 的冷却原因判定（账户级软风控，最痛点）
    if (insights) {
      for (const it of insights as RateLimitInsight[]) {
        if ((it.cooldown?.reason ?? '').includes('可疑活动')) {
          const c = list.find((x) => x.id === it.id)
          track(`suspicious:${it.id}`, 'suspicious', c ? credLabel(c) : `#${it.id}`)
        }
      }
    }

    // 统一发射：每类 1-2 条逐条发（含详细描述），≥3 条合并成一条汇总。
    flushBatch('arn', batch.arn, {
      one: (n) => `凭据 ${n} 缺少 Profile ARN`,
      manyTitle: (k) => `${k} 个凭据缺少 Profile ARN`,
      type: 'warning',
      desc: '对话会返回 400 profileArn is required。请刷新 Token 触发动态解析，或检查是否已开通 Kiro。',
    })
    flushBatch('quota', batch.quota, {
      one: (n) => `凭据 ${n} 额度已用尽`,
      manyTitle: (k) => `${k} 个凭据额度已用尽`,
      type: 'error',
      desc: '已达上游月度请求上限，已移出调度。可加号或等下月重置。',
    })
    flushBatch('disabled', batch.disabled, {
      one: (n) => `凭据 ${n}`,
      manyTitle: (k) => `${k} 个凭据被自动禁用`,
      type: 'error',
      desc: '已移出调度池。可在凭据管理里查看并处理。',
    })
    flushBatch('suspicious', batch.suspicious, {
      one: (n) => `凭据 ${n} 触发账户级可疑活动风控`,
      manyTitle: (k) => `${k} 个凭据触发账户级可疑活动风控`,
      type: 'warning',
      desc: '上游临时限速中，已分钟级退避避免加重风控。频繁触发建议加号分流。',
    })

    // 回收：本轮不再处于问题态的键从 seen 移除，使问题再次发生时能重新通知。
    for (const key of Array.from(seen)) {
      if (!activeKeys.has(key)) seen.delete(key)
    }
    if (!primedRef.current) primedRef.current = true
  }, [creds, insights])
}
