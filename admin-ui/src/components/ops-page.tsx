import { useState, useEffect, useRef, useCallback } from 'react'
import { useQuery } from '@tanstack/react-query'
import { toast } from 'sonner'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { getRecoveryMetrics, getLogs, type RecoveryMetrics, type LogEntry } from '@/api/ops'
import { storage } from '@/lib/storage'
import { Download, RefreshCw, Activity } from 'lucide-react'

// 自愈计数器展示项：字段 → 中文标签 + 是否"越多越该警惕"（用于配色）。
const METRIC_ITEMS: { key: keyof RecoveryMetrics; label: string; warn?: boolean }[] = [
  { key: 'refreshOk', label: '刷新成功' },
  { key: 'refreshFail', label: '刷新失败', warn: true },
  { key: 'failoverHops', label: 'failover 换号', warn: true },
  { key: 'failoverExhausted', label: 'failover 耗尽', warn: true },
  { key: 'deadTokensDisabled', label: '自动禁用死号', warn: true },
  { key: 'cooldownTriggered', label: '风控冷却触发', warn: true },
  { key: 'regionReprobeOk', label: 'region 重探成功' },
  { key: 'regionReprobeFail', label: 'region 重探失败', warn: true },
  { key: 'leakedCleanedRequests', label: '泄漏清洗请求', warn: true },
  { key: 'leakedSaturationRequests', label: '整段退化请求', warn: true },
]

function formatUptime(ms: number): string {
  const s = Math.floor(ms / 1000)
  const d = Math.floor(s / 86400)
  const h = Math.floor((s % 86400) / 3600)
  const m = Math.floor((s % 3600) / 60)
  if (d > 0) return `${d}天 ${h}小时`
  if (h > 0) return `${h}小时 ${m}分`
  return `${m}分`
}

const LEVEL_COLORS: Record<string, string> = {
  ERROR: 'text-red-400',
  WARN: 'text-amber-400',
  INFO: 'text-sky-400',
  DEBUG: 'text-[#888]',
  TRACE: 'text-[#666]',
}

export function OpsPage() {
  return (
    <div className="space-y-6">
      <RecoveryMetricsCard />
      <LogViewer />
    </div>
  )
}

function RecoveryMetricsCard() {
  // 每 5s 刷新计数器（纯内存端点，零上游）。
  const { data, isLoading, refetch } = useQuery({
    queryKey: ['recovery-metrics'],
    queryFn: getRecoveryMetrics,
    refetchInterval: 5000,
  })

  return (
    <Card>
      <CardHeader className="flex flex-row items-center justify-between pb-2">
        <CardTitle className="flex items-center gap-2 text-base">
          <Activity className="h-4 w-4" />
          自愈机器
          {data && (
            <span className="text-xs font-normal text-muted-foreground">
              · 运行 {formatUptime(data.uptimeMs)}（自启动累计，重启归零）
            </span>
          )}
        </CardTitle>
        <Button variant="ghost" size="sm" onClick={() => refetch()} className="h-7 px-2">
          <RefreshCw className="h-3.5 w-3.5" />
        </Button>
      </CardHeader>
      <CardContent>
        {isLoading ? (
          <p className="text-sm text-muted-foreground">加载中…</p>
        ) : data ? (
          <div className="grid grid-cols-2 gap-3 sm:grid-cols-3 lg:grid-cols-5">
            {METRIC_ITEMS.map((it) => {
              const v = data[it.key] as number
              const highlight = it.warn && v > 0
              return (
                <div
                  key={it.key}
                  className="rounded-md border border-[#2e2e2e] bg-[#111] px-3 py-2.5"
                >
                  <div className="text-xs text-muted-foreground">{it.label}</div>
                  <div
                    className={`mt-0.5 text-xl font-semibold tabular-nums ${
                      highlight ? 'text-amber-400' : 'text-[#ededed]'
                    }`}
                  >
                    {v}
                  </div>
                </div>
              )
            })}
          </div>
        ) : (
          <p className="text-sm text-muted-foreground">读取失败</p>
        )}
      </CardContent>
    </Card>
  )
}

const LEVEL_FILTERS = ['ALL', 'ERROR', 'WARN', 'INFO', 'DEBUG'] as const
type LevelFilter = (typeof LEVEL_FILTERS)[number]

// 级别过滤：entry 级别是否 ≥ 选定最低级别。
function rankOk(entryLevel: string, minLevel: string): boolean {
  const rank: Record<string, number> = { ERROR: 4, WARN: 3, INFO: 2, DEBUG: 1, TRACE: 0 }
  return (rank[entryLevel.toUpperCase()] ?? 0) >= (rank[minLevel.toUpperCase()] ?? 0)
}

function LogViewer() {
  const [logs, setLogs] = useState<LogEntry[]>([])
  const [level, setLevel] = useState<LevelFilter>('INFO')
  const [live, setLive] = useState(true)
  const [connected, setConnected] = useState(false)
  const [downloading, setDownloading] = useState(false)
  const scrollRef = useRef<HTMLDivElement>(null)
  // seq 单调递增，去重只需记「已见的最大 seq」——用单个数字而非 Set，避免长会话内 Set 无界增长。
  const lastSeq = useRef<number>(-1)

  const levelParam = level === 'ALL' ? undefined : level

  // 追加日志（按 seq 单调去重，环形上限 2000 条防无界增长）。
  const append = useCallback((entries: LogEntry[]) => {
    if (entries.length === 0) return
    setLogs((prev) => {
      const merged = [...prev]
      for (const e of entries) {
        if (e.seq <= lastSeq.current) continue
        lastSeq.current = e.seq
        merged.push(e)
      }
      return merged.length > 2000 ? merged.slice(merged.length - 2000) : merged
    })
  }, [])

  // 实时 SSE 流：EventSource 无法带自定义 header，故用 fetch + ReadableStream 手动读 SSE。
  // 断连(服务重启/网络抖动)会自动重连，并用 connected 状态如实反映连接态——绝不让
  // "实时"指示灯在流已死时仍假装在推(这正是本页要排障的场景)。
  useEffect(() => {
    if (!live) {
      setConnected(false)
      return
    }
    const key = storage.getApiKey() ?? ''
    const ctrl = new AbortController()
    let cancelled = false
    let retryTimer: ReturnType<typeof setTimeout> | null = null
    setLogs([])
    lastSeq.current = -1

    const connect = async () => {
      try {
        const resp = await fetch('/api/admin/logs/stream', {
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
              const entry = JSON.parse(dataLine.slice(5).trim()) as LogEntry
              if (!levelParam || rankOk(entry.level, levelParam)) append([entry])
            } catch {
              /* 心跳/非 JSON 行忽略 */
            }
          }
        }
      } catch {
        /* abort(切走/卸载)或断连:落到下方重连 */
      }
      // 流结束(done 或异常):若非主动取消,标记断连并 2s 后自动重连。
      if (!cancelled) {
        setConnected(false)
        retryTimer = setTimeout(connect, 2000)
      }
    }
    connect()

    return () => {
      cancelled = true
      if (retryTimer) clearTimeout(retryTimer)
      ctrl.abort()
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [live, level])

  // 非实时模式：手动拉一次快照。
  useEffect(() => {
    if (live) return
    getLogs({ level: levelParam })
      .then((entries) => {
        lastSeq.current = entries.length > 0 ? Math.max(...entries.map((e) => e.seq)) : -1
        setLogs(entries)
      })
      .catch(() => toast.error('拉取日志失败'))
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [live, level])

  // 新日志自动滚到底（仅实时模式）。
  useEffect(() => {
    if (live && scrollRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight
    }
  }, [logs, live])

  // 一键导出 JSONL：fetch 带鉴权 header → blob → 触发下载。
  const handleExport = async () => {
    setDownloading(true)
    try {
      const key = storage.getApiKey() ?? ''
      const q = levelParam ? `?level=${levelParam}` : ''
      const resp = await fetch(`/api/admin/logs/export${q}`, { headers: { 'x-api-key': key } })
      if (!resp.ok) throw new Error('导出失败')
      const blob = await resp.blob()
      const url = URL.createObjectURL(blob)
      const a = document.createElement('a')
      a.href = url
      a.download = `kirostudio-logs-${new Date().toISOString().slice(0, 19).replace(/[:T]/g, '')}.jsonl`
      a.click()
      URL.revokeObjectURL(url)
      toast.success('日志已导出')
    } catch {
      toast.error('导出失败')
    } finally {
      setDownloading(false)
    }
  }

  return (
    <Card>
      <CardHeader className="flex flex-row items-center justify-between gap-2 pb-2">
        <CardTitle className="text-base">实时日志</CardTitle>
        <div className="flex items-center gap-2">
          <div className="flex rounded-md border border-[#2e2e2e] p-0.5">
            {LEVEL_FILTERS.map((lv) => (
              <button
                key={lv}
                onClick={() => setLevel(lv)}
                className={`rounded px-2 py-0.5 text-xs transition-colors ${
                  level === lv ? 'bg-[#0070f3] text-white' : 'text-[#888] hover:text-[#ededed]'
                }`}
              >
                {lv}
              </button>
            ))}
          </div>
          <Button
            variant={live ? 'default' : 'outline'}
            size="sm"
            onClick={() => setLive((v) => !v)}
            className="h-7 gap-1 px-2 text-xs"
            title={live && !connected ? '连接已断开，正在重连…' : undefined}
          >
            <span
              className={`inline-block h-1.5 w-1.5 rounded-full ${
                !live ? 'bg-[#666]' : connected ? 'animate-pulse bg-white' : 'animate-pulse bg-amber-400'
              }`}
            />
            {!live ? '暂停' : connected ? '实时' : '重连中'}
          </Button>
          <Button variant="outline" size="sm" onClick={handleExport} disabled={downloading} className="h-7 gap-1 px-2 text-xs">
            <Download className="h-3.5 w-3.5" />
            导出
          </Button>
        </div>
      </CardHeader>
      <CardContent>
        <div
          ref={scrollRef}
          className="h-[420px] overflow-y-auto rounded-md border border-[#2e2e2e] bg-[#0a0a0a] p-2 font-mono text-xs leading-relaxed"
        >
          {logs.length === 0 ? (
            <p className="p-4 text-center text-muted-foreground">暂无日志</p>
          ) : (
            logs.map((e) => (
              <div key={e.seq} className="flex gap-2 border-b border-[#161616] py-0.5">
                <span className="shrink-0 text-[#555]">{e.ts.slice(11, 23)}</span>
                <span className={`shrink-0 font-semibold ${LEVEL_COLORS[e.level] ?? 'text-[#888]'}`}>
                  {e.level.padEnd(5)}
                </span>
                <span className="shrink-0 text-[#666]">{e.target}</span>
                <span className="whitespace-pre-wrap break-all text-[#ccc]">{e.message}</span>
              </div>
            ))
          )}
        </div>
      </CardContent>
    </Card>
  )
}
