import { useEffect, useRef } from 'react'
import { toast } from 'sonner'
import i18n from '@/i18n'
import { useCredentials } from '@/hooks/use-credentials'
import { useRatelimitInsights } from '@/hooks/use-usage'
import { disabledReasonLabel } from '@/lib/i18n-labels'
import { classifyDisabledReason } from '@/lib/pool-event-classify'
import type { PoolEventCategory } from '@/lib/pool-event-classify'
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

/**
 * disabledReason → 展示短语。**转发给 `lib/i18n-labels` 的 `disabledReasonLabel`**，
 * 不在这里再写一份中文表。
 *
 * 为什么改成转发：这里原先是一份硬编码 switch，只覆盖 8 个原因，而后端
 * `DisabledReason` 有 14 个 ⇒ 缺的全落 `default` 显示裸英文枚举名
 * （`RegionProbeFailed` / `PassthroughFailed` / `RequestLimitReached` …）。
 * 更糟的是两份真相源已经分叉：这里写的 `RefreshTokenInvalid` 与
 * `SubscriptionInvalid` **后端根本不下发**（实际枚举名是 `InvalidRefreshToken`，
 * 而 `SubscriptionInvalid` 在 `token_manager.rs` 的 `as_str()` 里不存在）
 * ⇒ 那两个 case 是永不命中的死分支，看起来"覆盖了"其实没有。
 *
 * 转发后，后端加新变体只需在 i18n-labels 的表里加一行，通知与卡片/回收站同步生效。
 */
function disabledReasonText(reason?: string): string {
  if (!reason) return i18n.t('poolNotify.disabledReasonEmpty')
  const label = disabledReasonLabel(reason)
  // `disabledReasonLabel` 对未收录的原因**原样返回**（即裸枚举名）。此处包一层
  // 「已禁用（X）」，让本版本不认识的新变体至少读起来是句话而不是一个英文标识符。
  return label === reason ? i18n.t('poolNotify.disabledWithReason', { reason }) : label
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
  const head = labels.slice(0, 3).join(i18n.t('poolNotify.listSep'))
  const rest = labels.length > 3 ? i18n.t('poolNotify.etcCount', { count: labels.length }) : ''
  fire(cfg.manyTitle(labels.length), {
    description: i18n.t('poolNotify.mergedDesc', { head, rest, desc: cfg.desc }),
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
    // 类别定义与归类规则在 `lib/pool-event-classify`（纯函数，测试锁住分支顺序）。
    type Cat = PoolEventCategory
    const batch: Record<Cat, string[]> = {
      arn: [],
      quota: [],
      disabled: [],
      suspicious: [],
      regionProbe: [],
      regionTokenDead: [],
    }

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
          toast.loading(i18n.t('poolNotify.initLoading', { label }), { id: key })
          initPending.set(c.id, { key, startedAt: Date.now() })
        } else if (needsArn && !c.disabled && c.hasProfileArn) {
          // 入池即带 arn(如网页 social 号,无中间态)→ 直接"已就绪"。
          toast.success(i18n.t('poolNotify.initReady', { label }))
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
      // 2. 号死/被禁用。归类交给 `classifyDisabledReason`：额度耗尽走 quota、
      //    region 探测两条各走自己的类（处置动作不同，见该函数注释），其余落兜底。
      //    指纹里带 disabledReason：原因变了（如 TooManyFailures → AccountSuspended）
      //    应当重新通知一次，因为处置动作跟着变了。
      if (c.disabled) {
        const cat = classifyDisabledReason(c.disabledReason)
        const key = `${cat}:${c.id}:${c.disabledReason ?? ''}`
        // quota / region 两类的 desc 已把原因说清楚，标题里不再重复；
        // 兜底类必须带原因，否则"某个号被禁用了"给不出任何排查方向。
        const text = cat === 'disabled' ? i18n.t('poolNotify.disabledWithLabel', { label, reason: disabledReasonText(c.disabledReason) }) : label
        track(key, cat, text)
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
        toast.success(i18n.t('poolNotify.initDone', { label: credLabel(c) }), { id: info.key })
        initPending.delete(id)
      } else if (Date.now() - info.startedAt > INIT_TIMEOUT_MS) {
        toast.warning(i18n.t('poolNotify.initTimeout', { label: credLabel(c) }), { id: info.key })
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
      one: (n) => i18n.t('poolNotify.arn.one', { label: n }),
      manyTitle: (k) => i18n.t('poolNotify.arn.manyTitle', { count: k }),
      type: 'warning',
      desc: i18n.t('poolNotify.arn.desc'),
    })
    flushBatch('quota', batch.quota, {
      one: (n) => i18n.t('poolNotify.quota.one', { label: n }),
      manyTitle: (k) => i18n.t('poolNotify.quota.manyTitle', { count: k }),
      type: 'error',
      desc: i18n.t('poolNotify.quota.desc'),
    })
    flushBatch('disabled', batch.disabled, {
      one: (n) => i18n.t('poolNotify.disabled.one', { label: n }),
      manyTitle: (k) => i18n.t('poolNotify.disabled.manyTitle', { count: k }),
      type: 'error',
      desc: i18n.t('poolNotify.disabled.desc'),
    })
    flushBatch('suspicious', batch.suspicious, {
      one: (n) => i18n.t('poolNotify.suspicious.one', { label: n }),
      manyTitle: (k) => i18n.t('poolNotify.suspicious.manyTitle', { count: k }),
      type: 'warning',
      desc: i18n.t('poolNotify.suspicious.desc'),
    })
    // region 探测两类：都不在自愈白名单里（人工确认后需手动启用），所以 desc 必须
    // 直接给出该查哪儿 —— 两条的排查方向完全相反，混成一句就等于没给方向。
    flushBatch('regionProbe', batch.regionProbe, {
      one: (n) => i18n.t('poolNotify.regionProbe.one', { label: n }),
      manyTitle: (k) => i18n.t('poolNotify.regionProbe.manyTitle', { count: k }),
      type: 'error',
      desc: i18n.t('poolNotify.regionProbe.desc'),
    })
    flushBatch('regionTokenDead', batch.regionTokenDead, {
      one: (n) => i18n.t('poolNotify.regionTokenDead.one', { label: n }),
      manyTitle: (k) => i18n.t('poolNotify.regionTokenDead.manyTitle', { count: k }),
      type: 'error',
      desc: i18n.t('poolNotify.regionTokenDead.desc'),
    })

    // 回收：本轮不再处于问题态的键从 seen 移除，使问题再次发生时能重新通知。
    for (const key of Array.from(seen)) {
      if (!activeKeys.has(key)) seen.delete(key)
    }
    if (!primedRef.current) primedRef.current = true
  }, [creds, insights])
}
