import { useEffect, useRef, useState } from 'react'
import { storage } from '@/lib/storage'

// SSE /api/admin/stream/live 的一帧（后端 usage_handlers.rs LiveFrame，camelCase）。
// 每 ~1.5s 一帧，只读内存零上游——比 10s 轮询跟手得多，用于号池实时指示。
export interface LiveCred {
  id: number
  rpm: number
  inflight: number
  coolingDown: boolean
  cooldownRemainingMs: number | null
  /** 熔断器是否 Open（真实熔断态）。无健康记录缺省 false。 */
  circuitOpen: boolean
  /** 健康分 [0,1]（EWMA 成功率 × 429 惩罚）。无健康记录缺省 1.0。 */
  healthScore: number
}

export interface LiveThroughput {
  currentRps: number
  tokensPerSec: number
}

export interface LiveFrame {
  globalInflight: number
  globalRpm: number
  creds: LiveCred[]
  throughput: LiveThroughput | null
}

interface LiveStreamState {
  /** 最近一帧；未连上时为 null。 */
  frame: LiveFrame | null
  /** SSE 是否已连上（断连时如实反映，不假装在推）。 */
  connected: boolean
}

/**
 * 消费 SSE /api/admin/stream/live，返回最新一帧 + 连接态。
 *
 * 为什么用 fetch + ReadableStream 而非 EventSource：EventSource 无法带自定义 header（x-api-key），
 * 与日志流同样的约束。断连（服务重启/网络抖动）按指数退避自动重连；隐藏标签页暂停（省资源、避免后台空转）。
 * `enabled=false` 时不连（调用方按当前 tab 决定是否需要实时流）。
 */
export function useLiveStream(enabled = true): LiveStreamState {
  const [frame, setFrame] = useState<LiveFrame | null>(null)
  const [connected, setConnected] = useState(false)
  // 用 ref 存最新 enabled，供可见性回调读，避免频繁重建连接。
  const enabledRef = useRef(enabled)
  enabledRef.current = enabled

  useEffect(() => {
    if (!enabled) {
      setConnected(false)
      return
    }
    const key = storage.getApiKey() ?? ''
    let cancelled = false
    // activeCtrl + generation 防连接泄漏：快速切标签页时 visibilitychange 会在上一轮
    // connect() 还卡在 await（fetch 或 reader.read()）时再起一轮。旧那轮 catch 后仍会
    // setConnected/排重连计时器，于是同时存在多条 SSE。后端每条残留连接按 1.5s/帧持续推送，
    // 泄漏会成倍放大后端负载。generation 让"过期的那轮"在恢复执行时直接自尽。
    let activeCtrl: AbortController | null = null
    let generation = 0
    let retryTimer: ReturnType<typeof setTimeout> | null = null
    // 连续失败次数，用于指数退避；收到第一帧真数据即归零（说明链路真的通了）。
    let attempt = 0

    const connect = async () => {
      // 隐藏标签页不连（可见性变化时由下方监听恢复）。
      if (cancelled || (typeof document !== 'undefined' && document.hidden)) return
      // 入口先掐掉上一条：同一时刻只允许一条存活连接。
      activeCtrl?.abort()
      const myGen = ++generation
      const ctrl = new AbortController()
      activeCtrl = ctrl
      try {
        const resp = await fetch('/api/admin/stream/live', {
          headers: { 'x-api-key': key },
          signal: ctrl.signal,
        })
        // fetch 对 502/503 是 resolve 而非 reject，而反代（Caddy）的 HTML 错误页让 resp.body
        // 非空 → 不查 ok 会把网关错误当成"已连上"，指示灯先亮绿再立刻闪回重连中。
        if (!resp.ok) throw new Error('http ' + resp.status)
        if (!resp.body) throw new Error('no body')
        setConnected(true)
        const reader = resp.body.getReader()
        const decoder = new TextDecoder()
        let buf = ''
        for (;;) {
          const { done, value } = await reader.read()
          if (done) break
          buf += decoder.decode(value, { stream: true })
          const parts = buf.split('\n\n')
          buf = parts.pop() ?? ''
          for (const part of parts) {
            const dataLine = part.split('\n').find((l) => l.startsWith('data:'))
            if (!dataLine) continue
            try {
              setFrame(JSON.parse(dataLine.slice(5).trim()) as LiveFrame)
              attempt = 0 // 真收到一帧 = 链路健康，退避从头计
            } catch {
              /* keep-alive 注释 / 非 JSON 行忽略 */
            }
          }
        }
      } catch {
        /* abort（卸载/隐藏/被新一轮顶掉）或断连：落到下方重连 */
      }
      // 已被更新的一轮取代：不碰 connected、不排计时器，否则两轮互相踩。
      if (myGen !== generation) return
      if (!cancelled) {
        setConnected(false)
        // 指数退避（2s/4s/8s…上限 30s）：后端长时间不可用时固定 2s 会持续砸重连请求。
        const delay = Math.min(2000 * 2 ** attempt, 30000)
        attempt++
        retryTimer = setTimeout(connect, delay)
      }
    }

    // 标签页可见性变化：隐藏时断开省资源，重新可见时立即重连。
    const onVisibility = () => {
      if (cancelled) return
      if (document.hidden) {
        activeCtrl?.abort()
      } else if (enabledRef.current) {
        // 已有存活连接时直接返回：可见性事件可能在连接还好着时触发（如切窗口回来），
        // 无条件 connect() 等于主动废掉一条正常连接再重建。
        if (activeCtrl && !activeCtrl.signal.aborted) return
        if (retryTimer) clearTimeout(retryTimer)
        attempt = 0 // 用户主动回到页面：立即重试一次，不背着历史退避
        connect()
      }
    }
    document.addEventListener('visibilitychange', onVisibility)
    connect()

    return () => {
      cancelled = true
      generation++ // 让所有在飞的 connect() 轮次作废
      if (retryTimer) clearTimeout(retryTimer)
      document.removeEventListener('visibilitychange', onVisibility)
      activeCtrl?.abort()
    }
  }, [enabled])

  return { frame, connected }
}
