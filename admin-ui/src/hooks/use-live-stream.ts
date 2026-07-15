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
 * 与日志流同样的约束。断连（服务重启/网络抖动）2s 后自动重连；隐藏标签页暂停（省资源、避免后台空转）。
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
    let ctrl: AbortController | null = null
    let retryTimer: ReturnType<typeof setTimeout> | null = null

    const connect = async () => {
      // 隐藏标签页不连（可见性变化时由下方监听恢复）。
      if (cancelled || (typeof document !== 'undefined' && document.hidden)) return
      ctrl = new AbortController()
      try {
        const resp = await fetch('/api/admin/stream/live', {
          headers: { 'x-api-key': key },
          signal: ctrl.signal,
        })
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
            } catch {
              /* keep-alive 注释 / 非 JSON 行忽略 */
            }
          }
        }
      } catch {
        /* abort（卸载/隐藏）或断连：落到下方重连 */
      }
      if (!cancelled) {
        setConnected(false)
        retryTimer = setTimeout(connect, 2000)
      }
    }

    // 标签页可见性变化：隐藏时断开省资源，重新可见时立即重连。
    const onVisibility = () => {
      if (cancelled) return
      if (document.hidden) {
        ctrl?.abort()
      } else if (enabledRef.current) {
        if (retryTimer) clearTimeout(retryTimer)
        connect()
      }
    }
    document.addEventListener('visibilitychange', onVisibility)
    connect()

    return () => {
      cancelled = true
      if (retryTimer) clearTimeout(retryTimer)
      document.removeEventListener('visibilitychange', onVisibility)
      ctrl?.abort()
    }
  }, [enabled])

  return { frame, connected }
}
