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
    case 'InvalidConfig':
      return '凭据配置不完整'
    case 'Manual':
      return '手动禁用'
    default:
      return reason ? `已禁用（${reason}）` : '已禁用'
  }
}

export function usePoolNotifications() {
  const { data: creds } = useCredentials()
  const { data: insights } = useRatelimitInsights()

  // 已通知指纹集合：跨轮询保留，状态恢复时移除对应键。
  const seenRef = useRef<Set<string>>(new Set())
  // 首轮不弹历史事件（避免打开面板瞬间把既有问题全刷一遍）——先把当前问题态记进 seen。
  const primedRef = useRef(false)

  useEffect(() => {
    if (!creds?.credentials) return
    const list: CredentialStatusItem[] = creds.credentials
    const seen = seenRef.current

    // 本轮所有"问题态"指纹，用于回收恢复态的键
    const activeKeys = new Set<string>()

    const emit = (key: string, fire: () => void) => {
      activeKeys.add(key)
      if (seen.has(key)) return
      seen.add(key)
      if (primedRef.current) fire() // 首轮只 prime 不弹
    }

    for (const c of list) {
      const label = credLabel(c)

      // 1. ARN 缺失（非 api_key 号才需要 profileArn；api_key 无此概念）
      if (!c.hasProfileArn && c.authMethod !== 'api_key' && !c.disabled) {
        emit(`arn:${c.id}`, () =>
          toast.warning(`凭据 ${label} 缺少 Profile ARN`, {
            description: '对话会返回 400 profileArn is required。请刷新 Token 触发动态解析，或检查该号是否已开通 Kiro。',
            duration: 8000,
          })
        )
      }

      // 2. 号死/被禁用
      if (c.disabled) {
        const reason = disabledReasonText(c.disabledReason)
        // 额度耗尽单独归到"余额"语义（红），其余禁用归"号死"（红）
        if (c.disabledReason === 'QuotaExceeded') {
          emit(`quota:${c.id}`, () =>
            toast.error(`凭据 ${label} 额度已用尽`, {
              description: '该号已达上游月度请求上限，已移出调度。可加号或等下月重置。',
              duration: 10000,
            })
          )
        } else {
          emit(`disabled:${c.id}:${c.disabledReason ?? ''}`, () =>
            toast.error(`凭据 ${label} ${reason}`, {
              description: '已移出调度池。可在凭据管理里查看并处理。',
              duration: 10000,
            })
          )
        }
      }
    }

    // 3. 可疑活动风控：从 insights 的冷却原因判定（账户级软风控，最痛点）
    if (insights) {
      for (const it of insights as RateLimitInsight[]) {
        const reason = it.cooldown?.reason ?? ''
        if (reason.includes('可疑活动')) {
          const c = list.find((x) => x.id === it.id)
          const label = c ? credLabel(c) : `#${it.id}`
          const secs = Math.ceil((it.cooldown?.remainingMs ?? 0) / 1000)
          emit(`suspicious:${it.id}`, () =>
            toast.warning(`凭据 ${label} 触发账户级可疑活动风控`, {
              description: `上游临时限速中（约 ${secs}s），已分钟级退避避免加重风控。频繁触发建议加号分流。`,
              duration: 9000,
            })
          )
        }
      }
    }

    // 回收：本轮不再处于问题态的键从 seen 移除，使问题再次发生时能重新通知。
    for (const key of Array.from(seen)) {
      if (!activeKeys.has(key)) seen.delete(key)
    }

    // 首轮结束后开启弹窗（此前已把当前问题态 prime 进 seen）。
    if (!primedRef.current) primedRef.current = true
  }, [creds, insights])
}
