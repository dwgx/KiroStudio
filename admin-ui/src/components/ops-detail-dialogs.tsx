import { useState, useEffect, useMemo, type ReactNode } from 'react'
import { useQuery, useQueryClient } from '@tanstack/react-query'
import { toast } from 'sonner'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Select } from '@/components/ui/select'
import { Badge, type BadgeProps } from '@/components/ui/badge'
import { EmptyState } from '@/components/ui/empty-state'
import { Skeleton } from '@/components/ui/skeleton'
import { StatCard } from '@/components/ui/stat-card'
import { ConfirmDialog } from '@/components/ui/confirm-dialog'
import { cn } from '@/lib/utils'
import { searchTraces, type TraceRecord, type TraceSearchFilter } from '@/api/ops'
import {
  useUsageOverview,
  useUsageByModel,
  useUsageByCredential,
} from '@/hooks/use-usage'
import { listTrash, restoreCredential, purgeCredential } from '@/api/credentials'
import type { TrashItem, GroupStat, WindowSummary } from '@/types/api'
import {
  Search,
  X,
  FileClock,
  BarChart3,
  Trash2,
  RotateCcw,
  Trash,
  ImageIcon,
  Inbox,
  SearchX,
  ChevronLeft,
  ChevronRight,
} from 'lucide-react'

// ts_ms 是 epoch 毫秒。请求明细跨天,故展示「MM-DD HH:MM:SS」(本地时区,解析失败回退原值)。
function formatTraceTime(tsMs: number): string {
  const d = new Date(tsMs)
  if (Number.isNaN(d.getTime())) return String(tsMs)
  const p2 = (n: number) => String(n).padStart(2, '0')
  return `${p2(d.getMonth() + 1)}-${p2(d.getDate())} ${p2(d.getHours())}:${p2(d.getMinutes())}:${p2(d.getSeconds())}`
}

// RFC3339 → 相对时间(与 settings-page timeAgo 同实现,回收站行复用)。
function timeAgo(iso: string | null | undefined): string {
  if (!iso) return '—'
  const t = new Date(iso).getTime()
  if (!Number.isFinite(t)) return iso
  const diff = Date.now() - t
  if (diff < 0) return '刚刚'
  const sec = Math.floor(diff / 1000)
  if (sec < 60) return `${sec} 秒前`
  const min = Math.floor(sec / 60)
  if (min < 60) return `${min} 分钟前`
  const hour = Math.floor(min / 60)
  if (hour < 24) return `${hour} 小时前`
  const day = Math.floor(hour / 24)
  if (day < 30) return `${day} 天前`
  const month = Math.floor(day / 30)
  if (month < 12) return `${month} 个月前`
  return `${Math.floor(month / 12)} 年前`
}

// 大数字千分位(tokens/credits)。
function fmtNum(n: number): string {
  return Math.round(n).toLocaleString()
}

// outcome → Badge 变体 + 中文短标签。success 绿 / rate_limited 黄 / 其余红。
const OUTCOME_META: Record<string, { label: string; variant: BadgeProps['variant'] }> = {
  success: { label: '成功', variant: 'success' },
  rate_limited: { label: '限流', variant: 'warning' },
  auth_failed: { label: '鉴权失败', variant: 'destructive' },
  quota_exhausted: { label: '额度耗尽', variant: 'destructive' },
  account_suspended: { label: '账号封禁', variant: 'destructive' },
  server_error: { label: '服务错误', variant: 'destructive' },
  bad_request: { label: '请求错误', variant: 'destructive' },
  network_error: { label: '网络错误', variant: 'destructive' },
  other_error: { label: '其他错误', variant: 'destructive' },
}

function outcomeMeta(oc: string): { label: string; variant: BadgeProps['variant'] } {
  return OUTCOME_META[oc] ?? { label: oc, variant: 'secondary' }
}

// outcome 下拉选项(空=全部)。
const OUTCOME_OPTIONS = [
  { value: '', label: '全部结果' },
  ...Object.entries(OUTCOME_META).map(([value, m]) => ({ value, label: m.label })),
]

const PAGE_SIZE = 50

// ============ 1. 请求明细 Dialog(traces,服务端搜索 + 分页) ============
export function TraceDetailDialog({
  open,
  onOpenChange,
}: {
  open: boolean
  onOpenChange: (v: boolean) => void
}) {
  // 文本输入(即时) + 防抖值(300ms,喂给查询)。
  const [textRaw, setTextRaw] = useState('')
  const [text, setText] = useState('')
  // 快捷过滤:model / clientIp / outcome / sessionId(联动过滤,点行内值可回填)。
  const [model, setModel] = useState('')
  const [clientIp, setClientIp] = useState('')
  const [outcome, setOutcome] = useState('')
  const [sessionId, setSessionId] = useState('')
  const [offset, setOffset] = useState(0)
  const [expandedId, setExpandedId] = useState<string | null>(null)

  // 文本防抖:输入停 300ms 后才更新查询词,避免每键一次请求。
  useEffect(() => {
    const t = setTimeout(() => setText(textRaw.trim()), 300)
    return () => clearTimeout(t)
  }, [textRaw])

  // 任一过滤条件变化即回到第一页(避免停在越界页显示空)。
  useEffect(() => {
    setOffset(0)
  }, [text, model, clientIp, outcome, sessionId])

  // 关闭时清空展开态(下次打开干净);过滤条件保留,便于二次排障。
  useEffect(() => {
    if (!open) setExpandedId(null)
  }, [open])

  // 构建过滤对象:仅带非空字段(空串不入参,后端也会归一,但保持 URL 干净)。
  const filter: TraceSearchFilter = useMemo(() => {
    const f: TraceSearchFilter = { limit: PAGE_SIZE, offset }
    if (text) f.text = text
    if (model.trim()) f.model = model.trim()
    if (clientIp.trim()) f.clientIp = clientIp.trim()
    if (outcome) f.outcome = outcome
    if (sessionId.trim()) f.sessionId = sessionId.trim()
    return f
  }, [text, model, clientIp, outcome, sessionId, offset])

  const { data, isLoading, isFetching } = useQuery({
    queryKey: ['traces-search', filter],
    queryFn: () => searchTraces(filter),
    enabled: open,
    placeholderData: (prev) => prev,
  })

  const items = data?.items ?? []
  const total = data?.total ?? 0
  const page = Math.floor(offset / PAGE_SIZE) + 1
  const totalPages = Math.max(1, Math.ceil(total / PAGE_SIZE))
  const hasFilters = !!(text || model || clientIp || outcome || sessionId)

  const clearAll = () => {
    setTextRaw('')
    setText('')
    setModel('')
    setClientIp('')
    setOutcome('')
    setSessionId('')
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="flex max-h-[88vh] w-[min(96vw,1100px)] max-w-none flex-col overflow-hidden">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <FileClock className="h-4 w-4" />
            请求明细
            <span className="text-xs font-normal text-muted-foreground tabular-nums">
              共 {total} 条 · 第 {page}/{totalPages} 页
            </span>
            {isFetching && !isLoading && (
              <span className="text-[11px] font-normal text-muted-foreground">刷新中…</span>
            )}
          </DialogTitle>
          <DialogDescription>
            服务端过滤 + 分页的逐请求 trace。点行看全文;点 IP / 设备 / 会话可回填为过滤条件。
          </DialogDescription>
        </DialogHeader>

        {/* 过滤栏 */}
        <div className="flex flex-wrap items-center gap-2">
          <div className="relative min-w-[200px] flex-1">
            <Search className="pointer-events-none absolute left-2 top-1/2 z-10 h-3.5 w-3.5 -translate-y-1/2 text-[#666]" />
            <Input
              value={textRaw}
              onChange={(e) => setTextRaw(e.target.value)}
              placeholder="搜索 error / request_id / model…"
              className="h-8 pl-7 pr-7 text-xs"
            />
            {textRaw && (
              <button
                onClick={() => setTextRaw('')}
                className="absolute right-1.5 top-1/2 z-10 -translate-y-1/2 text-[#666] hover:text-[#ededed]"
                title="清除"
              >
                <X className="h-3.5 w-3.5" />
              </button>
            )}
          </div>
          <Input
            value={model}
            onChange={(e) => setModel(e.target.value)}
            placeholder="模型"
            className="h-8 w-[150px] text-xs"
          />
          <Input
            value={clientIp}
            onChange={(e) => setClientIp(e.target.value)}
            placeholder="client IP"
            className="h-8 w-[130px] text-xs"
          />
          <Input
            value={sessionId}
            onChange={(e) => setSessionId(e.target.value)}
            placeholder="session"
            className="h-8 w-[130px] text-xs"
          />
          <Select
            value={outcome}
            onChange={setOutcome}
            options={OUTCOME_OPTIONS}
            aria-label="按结果过滤"
            className="h-8 w-[120px] shrink-0 [&>button]:h-8 [&>button]:py-1 [&>button]:text-xs"
          />
          {hasFilters && (
            <Button variant="ghost" size="sm" className="h-8 px-2 text-xs" onClick={clearAll}>
              <X className="mr-1 h-3.5 w-3.5" />
              清空
            </Button>
          )}
        </div>

        {/* 结果区 */}
        <div className="min-h-0 flex-1 overflow-y-auto rounded-md border border-[#2e2e2e] bg-[#0a0a0a]">
          {isLoading ? (
            <div className="space-y-1.5 p-2">
              {Array.from({ length: 8 }).map((_, i) => (
                <Skeleton key={i} className="h-9" />
              ))}
            </div>
          ) : items.length === 0 ? (
            <EmptyState
              icon={hasFilters ? SearchX : Inbox}
              title={hasFilters ? '无匹配请求' : '暂无请求明细'}
              description={hasFilters ? '调整或清空过滤条件' : undefined}
            />
          ) : (
            <table className="w-full border-collapse text-xs">
              <thead className="sticky top-0 z-10 bg-[#111] text-[#888]">
                <tr className="[&>th]:px-2 [&>th]:py-1.5 [&>th]:text-left [&>th]:font-medium">
                  <th>时间</th>
                  <th>模型</th>
                  <th>号</th>
                  <th>client IP</th>
                  <th>设备</th>
                  <th>会话</th>
                  <th className="text-right">tok(in/out)</th>
                  <th className="text-right">延迟</th>
                  <th>结果</th>
                </tr>
              </thead>
              <tbody>
                {items.map((it) => (
                  <TraceRow
                    key={it.request_id}
                    it={it}
                    expanded={expandedId === it.request_id}
                    onToggle={() =>
                      setExpandedId((prev) => (prev === it.request_id ? null : it.request_id))
                    }
                    onPickIp={(v) => setClientIp(v)}
                    onPickSession={(v) => setSessionId(v)}
                    onPickModel={(v) => setModel(v)}
                  />
                ))}
              </tbody>
            </table>
          )}
        </div>

        {/* 分页条 */}
        <div className="flex items-center justify-between gap-2">
          <span className="text-xs text-muted-foreground tabular-nums">
            共 {total} 条 · 第 {page}/{totalPages} 页
          </span>
          <div className="flex items-center gap-2">
            <Button
              variant="outline"
              size="sm"
              className="h-8 px-2 text-xs"
              disabled={offset === 0 || isFetching}
              onClick={() => setOffset((o) => Math.max(0, o - PAGE_SIZE))}
            >
              <ChevronLeft className="mr-1 h-3.5 w-3.5" />
              上一页
            </Button>
            <Button
              variant="outline"
              size="sm"
              className="h-8 px-2 text-xs"
              disabled={offset + PAGE_SIZE >= total || isFetching}
              onClick={() => setOffset((o) => o + PAGE_SIZE)}
            >
              下一页
              <ChevronRight className="ml-1 h-3.5 w-3.5" />
            </Button>
          </div>
        </div>
      </DialogContent>
    </Dialog>
  )
}

// 请求明细单行:紧凑一行 + 点击展开全文详情。IP/设备/会话可点击回填过滤(联动)。
function TraceRow({
  it,
  expanded,
  onToggle,
  onPickIp,
  onPickSession,
  onPickModel,
}: {
  it: TraceRecord
  expanded: boolean
  onToggle: () => void
  onPickIp: (v: string) => void
  onPickSession: (v: string) => void
  onPickModel: (v: string) => void
}) {
  const oc = outcomeMeta(it.outcome)
  const sessShort = it.session_id ? `${it.session_id.slice(0, 8)}…` : '—'
  // 可点击的联动值(阻止冒泡,避免同时触发行展开)。
  const pivot = (fn: () => void) => (e: React.MouseEvent) => {
    e.stopPropagation()
    fn()
  }
  return (
    <>
      <tr
        onClick={onToggle}
        className="cursor-pointer border-t border-[#161616] hover:bg-[#141414] [&>td]:px-2 [&>td]:py-1.5"
      >
        <td className="whitespace-nowrap text-[#888]" title={new Date(it.ts_ms).toLocaleString()}>
          {formatTraceTime(it.ts_ms)}
        </td>
        <td>
          <button
            onClick={pivot(() => it.model && onPickModel(it.model))}
            className="max-w-[130px] truncate font-mono text-[#ccc] hover:text-primary"
            title={`按模型过滤:${it.model}`}
          >
            {it.model || '—'}
          </button>
        </td>
        <td className="font-mono text-[#aaa]">
          {it.credential_id != null ? `#${it.credential_id}` : '—'}
        </td>
        <td>
          {it.client_ip ? (
            <button
              onClick={pivot(() => onPickIp(it.client_ip!))}
              className="font-mono text-sky-400/80 hover:text-sky-300"
              title={`按 IP 过滤:${it.client_ip}`}
            >
              {it.client_ip}
            </button>
          ) : (
            <span className="text-[#555]">—</span>
          )}
        </td>
        <td className="max-w-[110px] truncate text-[#888]" title={it.client_device ?? undefined}>
          {it.client_device || '—'}
        </td>
        <td>
          {it.session_id ? (
            <button
              onClick={pivot(() => onPickSession(it.session_id!))}
              className="font-mono text-[#888] hover:text-primary"
              title={`按会话过滤:${it.session_id}`}
            >
              {sessShort}
            </button>
          ) : (
            <span className="text-[#555]">—</span>
          )}
        </td>
        <td className="whitespace-nowrap text-right tabular-nums text-[#aaa]">
          {fmtNum(it.input_tokens)}/{fmtNum(it.output_tokens)}
        </td>
        <td className="whitespace-nowrap text-right tabular-nums text-[#aaa]">
          {it.latency_ms} ms
        </td>
        <td>
          <Badge variant={oc.variant} className="text-[10px]">{oc.label}</Badge>
        </td>
      </tr>
      {expanded && (
        <tr className="border-t border-[#161616] bg-[#0d0d0d]">
          <td colSpan={9} className="px-3 py-2">
            <div className="grid grid-cols-1 gap-x-6 gap-y-1 text-[11px] sm:grid-cols-2">
              <Detail label="request_id" value={it.request_id} mono />
              <Detail label="session_id" value={it.session_id ?? '—'} mono />
              <Detail label="流式" value={it.is_streaming ? '是' : '否'} />
              <Detail label="重试" value={String(it.retries)} />
              <Detail
                label="缓存 tok(读/写)"
                value={`${fmtNum(it.cache_read_tokens)} / ${fmtNum(it.cache_creation_tokens)}`}
              />
              <Detail
                label="credits"
                value={it.credits_used != null ? it.credits_used.toFixed(4) : '—'}
              />
              <Detail
                label="首 token"
                value={it.first_token_ms != null ? `${it.first_token_ms} ms` : '—'}
              />
              <Detail label="OS / 浏览器" value={`${it.client_os ?? '—'} / ${it.client_browser ?? '—'}`} />
              {it.error_message && (
                <div className="sm:col-span-2">
                  <span className="text-muted-foreground">error_message:</span>
                  <pre className="mt-1 max-h-40 overflow-auto whitespace-pre-wrap break-all rounded bg-[#161616] p-2 text-red-300">
                    {it.error_message}
                  </pre>
                </div>
              )}
            </div>
          </td>
        </tr>
      )}
    </>
  )
}

// 详情键值对小项。
function Detail({ label, value, mono }: { label: string; value: ReactNode; mono?: boolean }) {
  return (
    <div className="flex gap-1.5">
      <span className="shrink-0 text-muted-foreground">{label}:</span>
      <span className={cn('min-w-0 break-all text-[#ccc]', mono && 'font-mono')}>{value}</span>
    </div>
  )
}

// ============ 2. 用量日志 Dialog(usage 聚合展示) ============
export function UsageDetailDialog({
  open,
  onOpenChange,
}: {
  open: boolean
  onOpenChange: (v: boolean) => void
}) {
  const { data: overview, isLoading: ovLoading } = useUsageOverview()
  const { data: byModel, isLoading: bmLoading } = useUsageByModel()
  const { data: byCred, isLoading: bcLoading } = useUsageByCredential()

  const w = overview?.last_24h
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="flex max-h-[88vh] w-[min(96vw,900px)] max-w-none flex-col overflow-hidden">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <BarChart3 className="h-4 w-4" />
            用量日志
          </DialogTitle>
          <DialogDescription>
            近 24 小时汇总 + 按模型 / 按号维度聚合(读本地统计,零上游)。
          </DialogDescription>
        </DialogHeader>

        <div className="min-h-0 flex-1 space-y-4 overflow-y-auto pr-1">
          {/* 近 24h KPI */}
          {ovLoading ? (
            <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
              {Array.from({ length: 4 }).map((_, i) => (
                <Skeleton key={i} className="h-[92px]" />
              ))}
            </div>
          ) : (
            <UsageKpiRow w={w} />
          )}

          {/* 按模型 */}
          <UsageGroupTable
            title="按模型"
            keyHeader="模型"
            rows={byModel}
            loading={bmLoading}
          />
          {/* 按号 */}
          <UsageGroupTable
            title="按号(credential)"
            keyHeader="号"
            rows={byCred}
            loading={bcLoading}
            keyPrefix="#"
          />
        </div>
      </DialogContent>
    </Dialog>
  )
}

function UsageKpiRow({ w }: { w?: WindowSummary }) {
  const reqs = w?.requests ?? 0
  const tok = w?.total_tokens ?? 0
  const credits = w?.credits_used ?? 0
  const rate = w?.success_rate ?? 0
  return (
    <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
      <StatCard label="请求数" value={fmtNum(reqs)} accent="primary" />
      <StatCard label="Tokens" value={fmtNum(tok)} accent="neutral" />
      <StatCard label="Credits" value={credits.toFixed(2)} accent="neutral" />
      <StatCard
        label="成功率"
        value={`${(rate * 100).toFixed(1)}%`}
        accent={rate >= 0.95 ? 'success' : rate >= 0.8 ? 'warning' : 'destructive'}
      />
    </div>
  )
}

// 分组用量表(按模型 / 按号复用):key + 请求 + 成功率 + tok(in/out) + credits + 平均延迟。
function UsageGroupTable({
  title,
  keyHeader,
  rows,
  loading,
  keyPrefix = '',
}: {
  title: string
  keyHeader: string
  rows?: GroupStat[]
  loading: boolean
  keyPrefix?: string
}) {
  const sorted = useMemo(
    () => [...(rows ?? [])].sort((a, b) => b.requests - a.requests),
    [rows],
  )
  return (
    <div className="rounded-md border border-[#2e2e2e] bg-[#0a0a0a]">
      <div className="border-b border-[#2e2e2e] px-3 py-2 text-sm font-medium">{title}</div>
      {loading ? (
        <div className="space-y-1.5 p-2">
          {Array.from({ length: 4 }).map((_, i) => (
            <Skeleton key={i} className="h-7" />
          ))}
        </div>
      ) : sorted.length === 0 ? (
        <EmptyState icon={Inbox} title="暂无数据" className="py-6" />
      ) : (
        <table className="w-full border-collapse text-xs">
          <thead className="text-[#888]">
            <tr className="[&>th]:px-3 [&>th]:py-1.5 [&>th]:text-left [&>th]:font-medium">
              <th>{keyHeader}</th>
              <th className="text-right">请求</th>
              <th className="text-right">成功率</th>
              <th className="text-right">tok(in/out)</th>
              <th className="text-right">credits</th>
              <th className="text-right">均延迟</th>
            </tr>
          </thead>
          <tbody>
            {sorted.map((r) => (
              <tr key={r.key} className="border-t border-[#161616] [&>td]:px-3 [&>td]:py-1.5">
                <td className="max-w-[220px] truncate font-mono text-[#ccc]" title={r.key}>
                  {keyPrefix}
                  {r.key}
                </td>
                <td className="text-right tabular-nums text-[#aaa]">{fmtNum(r.requests)}</td>
                <td className="text-right tabular-nums">
                  <span
                    className={cn(
                      r.success_rate >= 0.95
                        ? 'text-emerald-400'
                        : r.success_rate >= 0.8
                          ? 'text-amber-400'
                          : 'text-red-400',
                    )}
                  >
                    {(r.success_rate * 100).toFixed(1)}%
                  </span>
                </td>
                <td className="text-right tabular-nums text-[#aaa]">
                  {fmtNum(r.input_tokens)}/{fmtNum(r.output_tokens)}
                </td>
                <td className="text-right tabular-nums text-[#aaa]">{r.credits_used.toFixed(3)}</td>
                <td className="text-right tabular-nums text-[#888]">{Math.round(r.avg_latency_ms)} ms</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  )
}

// ============ 3. 凭据回收站 Dialog(trash) ============
export function TrashDetailDialog({
  open,
  onOpenChange,
}: {
  open: boolean
  onOpenChange: (v: boolean) => void
}) {
  const queryClient = useQueryClient()
  const { data, isLoading, isFetching } = useQuery({
    queryKey: ['trash'],
    queryFn: listTrash,
    enabled: open,
  })
  const list = data?.trash ?? []

  const [busyId, setBusyId] = useState<number | null>(null)
  // 永久清除二次确认目标(不可逆,走 ConfirmDialog)。
  const [purgeTarget, setPurgeTarget] = useState<TrashItem | null>(null)

  const invalidate = () => {
    queryClient.invalidateQueries({ queryKey: ['trash'] })
    queryClient.invalidateQueries({ queryKey: ['credentials'] })
  }

  const handleRestore = async (item: TrashItem) => {
    setBusyId(item.id)
    try {
      await restoreCredential(item.id)
      toast.success(`已恢复凭据 #${item.id}`)
      invalidate()
    } catch {
      toast.error(`恢复 #${item.id} 失败`)
    } finally {
      setBusyId(null)
    }
  }

  const runPurge = async () => {
    if (!purgeTarget) return
    const id = purgeTarget.id
    setBusyId(id)
    try {
      await purgeCredential(id)
      toast.success(`已永久清除凭据 #${id}`)
      setPurgeTarget(null)
      invalidate()
    } catch {
      toast.error(`清除 #${id} 失败`)
    } finally {
      setBusyId(null)
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="flex max-h-[85vh] w-[min(96vw,720px)] max-w-none flex-col overflow-hidden">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <Trash2 className="h-4 w-4" />
            凭据回收站
            <span className="text-xs font-normal text-muted-foreground tabular-nums">
              {list.length} 项
            </span>
            {isFetching && !isLoading && (
              <span className="text-[11px] font-normal text-muted-foreground">刷新中…</span>
            )}
          </DialogTitle>
          <DialogDescription>
            已删除的凭据暂存于此,可恢复回号池或永久清除。永久清除
            <strong className="text-red-400">不可恢复</strong>。
          </DialogDescription>
        </DialogHeader>

        <div className="min-h-0 flex-1 overflow-y-auto">
          {isLoading ? (
            <div className="space-y-2">
              {Array.from({ length: 3 }).map((_, i) => (
                <Skeleton key={i} className="h-14" />
              ))}
            </div>
          ) : list.length === 0 ? (
            <EmptyState icon={Trash2} title="回收站为空" description="删除凭据后会暂存于此" />
          ) : (
            <div className="space-y-1.5">
              {list.map((item) => (
                <div
                  key={item.id}
                  className="flex items-center justify-between gap-3 rounded-md border border-[#2e2e2e] bg-[#111] px-3 py-2"
                >
                  <div className="min-w-0">
                    <div className="flex items-center gap-2 text-sm">
                      <span className="font-mono text-[#aaa]">#{item.id}</span>
                      <span className="truncate">{item.email || '(无 email)'}</span>
                      {item.authMethod && (
                        <Badge variant="outline" className="text-[10px]">{item.authMethod}</Badge>
                      )}
                    </div>
                    <div className="mt-0.5 flex flex-wrap items-center gap-x-2 gap-y-0.5 text-[11px] text-muted-foreground">
                      {item.maskedApiKey && (
                        <span className="font-mono">{item.maskedApiKey}</span>
                      )}
                      <span title={item.deletedAt}>删除于 {timeAgo(item.deletedAt)}</span>
                      <span>·</span>
                      <span>成功 {item.successCount} 次</span>
                      <span>·</span>
                      <span title={item.lastUsedAt ?? undefined}>
                        最后调用 {timeAgo(item.lastUsedAt)}
                      </span>
                    </div>
                  </div>
                  <div className="flex shrink-0 items-center gap-2">
                    <Button
                      variant="outline"
                      size="sm"
                      className="h-8 px-2 text-xs"
                      disabled={busyId === item.id}
                      onClick={() => handleRestore(item)}
                    >
                      <RotateCcw className="mr-1 h-3.5 w-3.5" />
                      恢复
                    </Button>
                    <Button
                      variant="destructive"
                      size="sm"
                      className="h-8 px-2 text-xs"
                      disabled={busyId === item.id}
                      onClick={() => setPurgeTarget(item)}
                    >
                      <Trash className="mr-1 h-3.5 w-3.5" />
                      永久清除
                    </Button>
                  </div>
                </div>
              ))}
            </div>
          )}
        </div>
      </DialogContent>

      {/* 永久清除二次确认(不可逆) */}
      <ConfirmDialog
        open={purgeTarget !== null}
        onOpenChange={(v) => !v && setPurgeTarget(null)}
        title={`永久清除凭据 #${purgeTarget?.id ?? ''}？`}
        description={
          <span>
            此操作将<strong className="text-red-400">永久删除,无法恢复</strong>
            ,该凭据将从回收站彻底清除。确定继续？
          </span>
        }
        confirmLabel="确认永久清除"
        destructive
        loading={busyId !== null && busyId === purgeTarget?.id}
        onConfirm={runPurge}
      />
    </Dialog>
  )
}

// ============ 4. 登录背景图缓存 Dialog(bg_cache) ============
export function BgCacheDetailDialog({
  open,
  onOpenChange,
  count,
}: {
  open: boolean
  onOpenChange: (v: boolean) => void
  // 缓存张数(来自 storage stats 的 bg_cache 分区 items 字段)。
  count: number
}) {
  const idxs = useMemo(() => Array.from({ length: Math.max(0, count) }, (_, i) => i), [count])
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="flex max-h-[85vh] w-[min(96vw,860px)] max-w-none flex-col overflow-hidden">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <ImageIcon className="h-4 w-4" />
            登录背景图缓存
            <span className="text-xs font-normal text-muted-foreground tabular-nums">
              {count} 张
            </span>
          </DialogTitle>
          <DialogDescription>
            常驻内存的登录页随机背景图池,清理即释放内存(下次登录会重新拉取填充)。
          </DialogDescription>
        </DialogHeader>

        <div className="min-h-0 flex-1 overflow-y-auto">
          {idxs.length === 0 ? (
            <EmptyState icon={ImageIcon} title="缓存为空" description="尚未拉取任何背景图" />
          ) : (
            <div className="grid grid-cols-2 gap-3 sm:grid-cols-3">
              {idxs.map((i) => (
                <div
                  key={i}
                  className="group relative aspect-video overflow-hidden rounded-md border border-[#2e2e2e] bg-[#111]"
                >
                  <img
                    src={`/admin/api/bg-cached?idx=${i}`}
                    loading="lazy"
                    alt={`背景图 #${i}`}
                    className="h-full w-full object-cover transition-transform duration-300 ease-out group-hover:scale-110"
                  />
                  <span className="absolute left-1.5 top-1.5 rounded bg-black/60 px-1.5 py-0.5 font-mono text-[10px] text-white/90">
                    #{i}
                  </span>
                </div>
              ))}
            </div>
          )}
        </div>
      </DialogContent>
    </Dialog>
  )
}

