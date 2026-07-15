import { useState, useEffect, useRef, useCallback, type ReactNode } from 'react'
import { useQuery } from '@tanstack/react-query'
import { toast } from 'sonner'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { getRecoveryMetrics, getLogs, type RecoveryMetrics, type LogEntry } from '@/api/ops'
import { useRatelimitInsights } from '@/hooks/use-usage'
import { useLiveStream } from '@/hooks/use-live-stream'
import { useSetDisabled, useResetFailure, useForceRefreshToken } from '@/hooks/use-credentials'
import type { RateLimitInsight } from '@/types/api'
import { storage } from '@/lib/storage'
import { Download, RefreshCw, Activity, Search, X, Copy, ShieldAlert, Zap, Power, RotateCcw } from 'lucide-react'
import { Select } from '@/components/ui/select'

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
  { key: 'textifiedInvokeHits', label: '文本化工具调用', warn: true },
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
      <PoolHealthCard />
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
        {data && data.atRestHealthy === false && (
          <div className="mb-3 rounded-md border border-red-500/40 bg-red-500/10 px-3 py-2 text-xs text-red-300">
            ⚠ at-rest 加密已开启,但上次凭据落盘回退成了明文(密钥文件读写失败)。磁盘上的凭据当前未加密——
            请检查密钥文件权限/磁盘可写后重试保存,或查看日志。
          </div>
        )}
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

// 单号健康行:状态点 + 健康分 + rpm/冷却 + 快捷操作。
function PoolHealthRow({
  it,
  busy,
  onRefresh,
  onReset,
  onToggleDisabled,
}: {
  it: RateLimitInsight
  busy: boolean
  onRefresh: () => void
  onReset: () => void
  onToggleDisabled: () => void
}) {
  const st = circuitStateOf(it)
  const meta = CIRCUIT_META[st]
  // 健康分百分比(无健康记录=满血 100%)。
  const healthPct = Math.round((it.health?.health ?? 1) * 100)
  return (
    <div className="flex items-center gap-3 rounded-md border border-[#2e2e2e] bg-[#111] px-3 py-2">
      <span className={`inline-block h-2 w-2 shrink-0 rounded-full ${meta.dot}`} />
      <span className="w-10 shrink-0 font-mono text-xs text-[#aaa]">#{it.id}</span>
      <span className={`w-20 shrink-0 text-xs font-medium ${meta.cls}`}>{meta.label}</span>
      {/* 健康分条 */}
      <div className="flex w-24 shrink-0 items-center gap-1.5" title={`健康分 ${healthPct}%`}>
        <div className="h-1.5 flex-1 overflow-hidden rounded-full bg-[#222]">
          <div
            className={`h-full rounded-full ${healthPct >= 60 ? 'bg-emerald-500' : healthPct >= 30 ? 'bg-amber-400' : 'bg-red-500'}`}
            style={{ width: `${healthPct}%` }}
          />
        </div>
        <span className="w-8 text-right text-[10px] tabular-nums text-[#888]">{healthPct}%</span>
      </div>
      {/* 关键指标:rpm + 熔断/冷却剩余 */}
      <span className="flex-1 truncate text-xs text-[#888]">
        <span className="tabular-nums">rpm {it.rpm}{it.rpmLimit > 0 ? `/${it.rpmLimit}` : ''}</span>
        {it.health?.circuitOpen && it.health.openRemainingSecs > 0 && (
          <span className="ml-2 text-red-400">熔断剩 {it.health.openRemainingSecs}s</span>
        )}
        {it.cooldown && (
          <span className="ml-2 text-sky-400">冷却 {Math.ceil(it.cooldown.remainingMs / 1000)}s</span>
        )}
        {it.recent429 > 0 && <span className="ml-2 text-amber-400">429×{it.recent429}</span>}
      </span>
      {/* 快捷操作 */}
      <div className="flex shrink-0 items-center gap-1">
        <button
          onClick={onRefresh}
          disabled={busy}
          title="强制刷新 Token"
          className="rounded p-1 text-[#888] hover:bg-[#222] hover:text-sky-400 disabled:opacity-40"
        >
          <Zap className="h-3.5 w-3.5" />
        </button>
        <button
          onClick={onReset}
          disabled={busy}
          title="重置失败计数并启用"
          className="rounded p-1 text-[#888] hover:bg-[#222] hover:text-emerald-400 disabled:opacity-40"
        >
          <RotateCcw className="h-3.5 w-3.5" />
        </button>
        <button
          onClick={onToggleDisabled}
          disabled={busy}
          title={it.disabled ? '启用' : '禁用'}
          className={`rounded p-1 hover:bg-[#222] disabled:opacity-40 ${it.disabled ? 'text-[#777] hover:text-emerald-400' : 'text-emerald-400 hover:text-red-400'}`}
        >
          <Power className="h-3.5 w-3.5" />
        </button>
      </div>
    </div>
  )
}

// 熔断态 → 展示元信息(标签/配色)。真实熔断态来自后端 HealthTracker,非前端启发式。
type CircuitState = 'open' | 'halfOpen' | 'cooldown' | 'disabled' | 'healthy' | 'warn'

function circuitStateOf(it: RateLimitInsight): CircuitState {
  if (it.disabled) return 'disabled'
  if (it.health?.circuitOpen) return 'open'
  if (it.health?.halfOpen) return 'halfOpen'
  if (it.cooldown) return 'cooldown'
  // 健康分 < 0.6 视为亚健康(EWMA 已被 429 拉低但未跳闸)。
  if (it.health && it.health.health < 0.6) return 'warn'
  return 'healthy'
}

const CIRCUIT_META: Record<CircuitState, { label: string; cls: string; dot: string }> = {
  open: { label: '熔断', cls: 'text-red-400', dot: 'bg-red-500' },
  halfOpen: { label: '半开试探', cls: 'text-amber-400', dot: 'bg-amber-400' },
  cooldown: { label: '冷却中', cls: 'text-sky-400', dot: 'bg-sky-400' },
  disabled: { label: '已禁用', cls: 'text-[#777]', dot: 'bg-[#555]' },
  warn: { label: '亚健康', cls: 'text-amber-300', dot: 'bg-amber-300' },
  healthy: { label: '健康', cls: 'text-emerald-400', dot: 'bg-emerald-500' },
}

// 号池健康总览:每号真实熔断态 + 健康分 + 冷却剩余 + 快捷运维操作(强刷/重置/启用禁用)。
// 数据双源:insights(10s 轮询,给全量字段)+ SSE live 帧(~1.5s,实时覆盖 rpm/inflight/熔断/健康分),
// 让实时指标跟手、又不丢 insights 的推断文案/软上限。
function PoolHealthCard() {
  const { data, isLoading } = useRatelimitInsights()
  const { frame, connected } = useLiveStream(true)
  const setDisabled = useSetDisabled()
  const resetFailure = useResetFailure()
  const forceRefresh = useForceRefreshToken()

  // 用 SSE live 帧的实时值覆盖 insights 的对应字段(id 对齐;live 帧缺该号则保留 insights 值)。
  const liveById = new Map((frame?.creds ?? []).map((c) => [c.id, c]))
  const insights: RateLimitInsight[] = (data ?? []).map((it) => {
    const lv = liveById.get(it.id)
    if (!lv) return it
    return {
      ...it,
      rpm: lv.rpm,
      inflight: lv.inflight,
      // 实时熔断态/健康分覆盖(insights.health 可能为 10s 前的);其余 health 字段保留。
      health: it.health
        ? { ...it.health, circuitOpen: lv.circuitOpen, health: lv.healthScore }
        : it.health,
    }
  })
  // 排序:最需要关注的排前(熔断/半开/冷却/亚健康 > 健康 > 禁用)。
  const order: Record<CircuitState, number> = { open: 0, halfOpen: 1, cooldown: 2, warn: 3, healthy: 4, disabled: 5 }
  const sorted = [...insights].sort((a, b) => order[circuitStateOf(a)] - order[circuitStateOf(b)] || a.id - b.id)

  const busy = setDisabled.isPending || resetFailure.isPending || forceRefresh.isPending

  return (
    <Card>
      <CardHeader className="flex flex-row items-center justify-between pb-2">
        <CardTitle className="flex items-center gap-2 text-base">
          <ShieldAlert className="h-4 w-4" />
          号池健康
          <span className="flex items-center gap-1.5 text-xs font-normal text-muted-foreground">
            <span
              className={`inline-block h-1.5 w-1.5 rounded-full ${connected ? 'animate-pulse bg-emerald-400' : 'bg-[#666]'}`}
              title={connected ? '实时流已连接（~1.5s）' : '实时流未连接，回退 10s 轮询'}
            />
            · 真实熔断态 + 健康分(零上游{connected ? '，实时' : '，10s 轮询'})
          </span>
        </CardTitle>
      </CardHeader>
      <CardContent>
        {isLoading ? (
          <p className="text-sm text-muted-foreground">加载中…</p>
        ) : sorted.length === 0 ? (
          <p className="text-sm text-muted-foreground">暂无凭据</p>
        ) : (
          <div className="space-y-1.5">
            {sorted.map((it) => (
              <PoolHealthRow
                key={it.id}
                it={it}
                busy={busy}
                onRefresh={() => forceRefresh.mutate(it.id, {
                  onSuccess: () => toast.success(`#${it.id} 已触发刷新`),
                  onError: () => toast.error(`#${it.id} 刷新失败`),
                })}
                onReset={() => resetFailure.mutate(it.id, {
                  onSuccess: () => toast.success(`#${it.id} 已重置并启用`),
                  onError: () => toast.error(`#${it.id} 重置失败`),
                })}
                onToggleDisabled={() => setDisabled.mutate(
                  { id: it.id, disabled: !it.disabled },
                  {
                    onSuccess: () => toast.success(`#${it.id} 已${it.disabled ? '启用' : '禁用'}`),
                    onError: () => toast.error('操作失败'),
                  },
                )}
              />
            ))}
          </div>
        )}
      </CardContent>
    </Card>
  )
}

const LEVEL_FILTERS = ['ALL', 'ERROR', 'WARN', 'INFO', 'DEBUG'] as const
type LevelFilter = (typeof LEVEL_FILTERS)[number]

// 前端日志缓冲上限（条）。与后端 ring 对齐（5000），覆盖更长排障/搜索窗口。
const CLIENT_LOG_CAP = 5000

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
  // 关键字搜索（匹配 message + target，大小写不敏感）。
  const [search, setSearch] = useState('')
  // 模块（target）过滤：'' = 全部。
  const [moduleFilter, setModuleFilter] = useState('')
  // 展开查看详情的条目 seq（点开单条看全文 + 复制）。
  const [expandedSeq, setExpandedSeq] = useState<number | null>(null)
  const scrollRef = useRef<HTMLDivElement>(null)
  // 滚动锁定防呆：用户手动上滚离开底部时暂停自动滚到底，避免"想看历史却被拽回底部"；
  // 贴底时恢复自动跟随。用 ref 存"是否贴底"避免每帧 setState。
  const atBottomRef = useRef(true)
  // seq 单调递增，去重只需记「已见的最大 seq」——用单个数字而非 Set，避免长会话内 Set 无界增长。
  const lastSeq = useRef<number>(-1)

  const levelParam = level === 'ALL' ? undefined : level

  // 追加日志（按 seq 单调去重，环形上限 CLIENT_LOG_CAP 条防无界增长）。
  const append = useCallback((entries: LogEntry[]) => {
    if (entries.length === 0) return
    setLogs((prev) => {
      const merged = [...prev]
      for (const e of entries) {
        if (e.seq <= lastSeq.current) continue
        lastSeq.current = e.seq
        merged.push(e)
      }
      return merged.length > CLIENT_LOG_CAP ? merged.slice(merged.length - CLIENT_LOG_CAP) : merged
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
    // 清空日志后复位"贴底"标志:否则切级别/切 live 时若之前上滚过,atBottomRef 卡在 false,
    // 容器变空不再触发 scroll 事件无从纠正,新日志永不自动跟随(排障页可观测性回归)。
    atBottomRef.current = true

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
        // 同上:快照重载后复位贴底标志,避免过期 false 卡住后续自动跟随。
        atBottomRef.current = true
      })
      .catch(() => toast.error('拉取日志失败'))
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [live, level])

  // 新日志自动滚到底（仅实时模式 + 用户当前贴底时；上滚看历史则不打断）。
  useEffect(() => {
    if (live && atBottomRef.current && scrollRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight
    }
  }, [logs, live])

  // 监听滚动，维护"是否贴底"（阈值 24px，容忍亚像素/惯性）。
  const handleScroll = useCallback(() => {
    const el = scrollRef.current
    if (!el) return
    atBottomRef.current = el.scrollHeight - el.scrollTop - el.clientHeight < 24
  }, [])

  // 派生：按模块 + 关键字过滤后的可见日志（级别过滤已在拉取/流入口做）。
  const q = search.trim().toLowerCase()
  const visibleLogs = logs.filter((e) => {
    if (moduleFilter && e.target !== moduleFilter) return false
    if (q && !e.message.toLowerCase().includes(q) && !e.target.toLowerCase().includes(q)) return false
    return true
  })

  // 已见模块（target）集合，供下拉过滤；按字母序稳定排列。
  const modules = Array.from(new Set(logs.map((e) => e.target))).sort()

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
      <CardHeader className="flex flex-col gap-2 pb-2">
       <div className="flex flex-row items-center justify-between gap-2">
        <CardTitle className="text-base">
          实时日志
          <span className="ml-2 text-xs font-normal text-muted-foreground tabular-nums">
            {visibleLogs.length}
            {visibleLogs.length !== logs.length ? ` / ${logs.length}` : ''} 条
          </span>
        </CardTitle>
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
       </div>
       {/* 搜索 + 模块过滤行 */}
       <div className="flex flex-row items-center gap-2">
         <div className="relative flex-1">
           <Search className="pointer-events-none absolute left-2 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-[#666]" />
           <input
             value={search}
             onChange={(e) => setSearch(e.target.value)}
             placeholder="搜索消息或模块…"
             className="h-7 w-full rounded-md border border-[#2e2e2e] bg-[#0a0a0a] pl-7 pr-7 text-xs text-[#ededed] placeholder:text-[#555] focus:border-[#0070f3] focus:outline-none"
           />
           {search && (
             <button
               onClick={() => setSearch('')}
               className="absolute right-1.5 top-1/2 -translate-y-1/2 text-[#666] hover:text-[#ededed]"
               title="清除搜索"
             >
               <X className="h-3.5 w-3.5" />
             </button>
           )}
         </div>
         <Select
           value={moduleFilter}
           onChange={setModuleFilter}
           aria-label="按模块过滤"
           className="w-[180px] shrink-0"
           options={[
             { value: '', label: '全部模块' },
             ...modules.map((m) => ({ value: m, label: m })),
           ]}
         />
       </div>
      </CardHeader>
      <CardContent>
        <div
          ref={scrollRef}
          onScroll={handleScroll}
          className="h-[420px] overflow-y-auto rounded-md border border-[#2e2e2e] bg-[#0a0a0a] p-2 font-mono text-xs leading-relaxed"
        >
          {visibleLogs.length === 0 ? (
            <p className="p-4 text-center text-muted-foreground">
              {logs.length === 0 ? '暂无日志' : '无匹配日志（调整搜索/过滤）'}
            </p>
          ) : (
            visibleLogs.map((e) => (
              <LogRow
                key={e.seq}
                entry={e}
                expanded={expandedSeq === e.seq}
                onToggle={() => setExpandedSeq((prev) => (prev === e.seq ? null : e.seq))}
                highlight={q}
              />
            ))
          )}
        </div>
      </CardContent>
    </Card>
  )
}

// 单条日志行：点击展开看全文 + 复制；搜索命中高亮。
function LogRow({
  entry,
  expanded,
  onToggle,
  highlight,
}: {
  entry: LogEntry
  expanded: boolean
  onToggle: () => void
  highlight: string
}) {
  const handleCopy = (ev: React.MouseEvent) => {
    ev.stopPropagation()
    // 复制整条（时间 + 级别 + 模块 + 消息），方便贴 issue。
    const line = `${entry.ts} ${entry.level} ${entry.target} ${entry.message}`
    navigator.clipboard.writeText(line).then(
      () => toast.success('已复制该条日志'),
      () => toast.error('复制失败'),
    )
  }
  return (
    <div
      onClick={onToggle}
      className="flex cursor-pointer gap-2 border-b border-[#161616] py-0.5 hover:bg-[#141414]"
    >
      <span className="shrink-0 text-[#555]">{entry.ts.slice(11, 23)}</span>
      <span className={`shrink-0 font-semibold ${LEVEL_COLORS[entry.level] ?? 'text-[#888]'}`}>
        {entry.level.padEnd(5)}
      </span>
      <span className="shrink-0 text-[#666]">{entry.target}</span>
      <span
        className={`flex-1 whitespace-pre-wrap break-all text-[#ccc] ${expanded ? '' : 'line-clamp-2'}`}
      >
        {highlightMatch(entry.message, highlight)}
      </span>
      {expanded && (
        <button
          onClick={handleCopy}
          className="shrink-0 self-start text-[#666] hover:text-[#ededed]"
          title="复制整条"
        >
          <Copy className="h-3.5 w-3.5" />
        </button>
      )}
    </div>
  )
}

// 关键字高亮：命中片段套黄底。大小写不敏感；空词原样返回。
function highlightMatch(text: string, q: string): ReactNode {
  if (!q) return text
  const lower = text.toLowerCase()
  const needle = q.toLowerCase()
  const parts: ReactNode[] = []
  let i = 0
  let key = 0
  for (;;) {
    const idx = lower.indexOf(needle, i)
    if (idx === -1) {
      parts.push(text.slice(i))
      break
    }
    if (idx > i) parts.push(text.slice(i, idx))
    parts.push(
      <mark key={key++} className="bg-amber-500/30 text-amber-200">
        {text.slice(idx, idx + needle.length)}
      </mark>,
    )
    i = idx + needle.length
  }
  return parts
}
