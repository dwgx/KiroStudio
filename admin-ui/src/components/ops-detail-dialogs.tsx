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
  Filter,
  Download,
  CalendarClock,
  Check,
  Square,
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

// datetime-local 输入值 → epoch 毫秒(空串/非法回 undefined)。
function localInputToMs(v: string): number | undefined {
  if (!v) return undefined
  const t = new Date(v).getTime()
  return Number.isFinite(t) ? t : undefined
}

// 日期时间选择组件:包 native datetime-local(自带日历弹层,零依赖),补日历图标 + 快捷「此刻」/清除,
// 深色主题对齐。native 控件本身点击弹日历,不会有 label 转发错位问题(不用 label 包裹)。
function DateTimeField({
  value,
  onChange,
  ariaLabel,
}: {
  value: string
  onChange: (v: string) => void
  ariaLabel?: string
}) {
  // 生成本地时区的 datetime-local 字符串(YYYY-MM-DDTHH:mm)。
  const nowLocal = () => {
    const d = new Date()
    const pad = (n: number) => String(n).padStart(2, '0')
    return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`
  }
  return (
    <div className="flex items-center gap-1">
      <div className="relative flex-1">
        <CalendarClock className="pointer-events-none absolute left-2 top-1/2 z-10 h-3.5 w-3.5 -translate-y-1/2 text-[#666]" />
        <Input
          type="datetime-local"
          value={value}
          onChange={(e) => onChange(e.target.value)}
          aria-label={ariaLabel}
          className="h-8 pl-7 text-xs [color-scheme:dark]"
        />
      </div>
      <Button
        type="button"
        variant="ghost"
        size="sm"
        className="h-8 shrink-0 px-1.5 text-[10px] text-muted-foreground hover:text-[#ededed]"
        onClick={() => onChange(nowLocal())}
        title="设为此刻"
      >
        此刻
      </Button>
      {value && (
        <Button
          type="button"
          variant="ghost"
          size="sm"
          className="h-8 w-8 shrink-0 p-0 text-muted-foreground hover:text-[#ededed]"
          onClick={() => onChange('')}
          title="清除"
        >
          <X className="h-3.5 w-3.5" />
        </Button>
      )}
    </div>
  )
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

// 饼图主题色板(取自主题色调,循环取用;禁图表库,纯 SVG 自绘)。
const PIE_COLORS = [
  'hsl(var(--primary))',
  'hsl(160 84% 45%)',
  'hsl(38 92% 55%)',
  'hsl(280 65% 60%)',
  'hsl(199 89% 55%)',
  'hsl(0 84% 62%)',
  'hsl(48 96% 55%)',
  'hsl(220 9% 60%)',
]

// 饼图单段(外部传入)。
interface PieSegment {
  label: string
  value: number
  color: string
}

// 极角 → SVG 坐标(半径 r,圆心 cx/cy,angle 弧度从 12 点方向顺时针)。
function polar(cx: number, cy: number, r: number, angle: number): [number, number] {
  return [cx + r * Math.sin(angle), cy - r * Math.cos(angle)]
}

// 纯 SVG 自绘饼图 + 图例(禁图表库)。value 为占比权重,自动归一为角度;
// 图例含 label + 百分比。空数据(总和为 0)显示占位文案。
function PieChart({ segments, size = 132 }: { segments: PieSegment[]; size?: number }) {
  const total = segments.reduce((s, seg) => s + Math.max(0, seg.value), 0)
  const cx = size / 2
  const cy = size / 2
  const r = size / 2 - 2

  // 生成每段的扇形 path(单段占满时画整圆,避免 arc 起止点重合消失)。
  let acc = 0
  const arcs = segments
    .filter((s) => s.value > 0)
    .map((seg) => {
      const start = (acc / total) * Math.PI * 2
      acc += seg.value
      const end = (acc / total) * Math.PI * 2
      const frac = seg.value / total
      const [x0, y0] = polar(cx, cy, r, start)
      const [x1, y1] = polar(cx, cy, r, end)
      const largeArc = end - start > Math.PI ? 1 : 0
      // 整段占满 → 画整圆(两段半圆拼接)。
      const d =
        frac >= 0.999
          ? `M ${cx} ${cy - r} A ${r} ${r} 0 1 1 ${cx - 0.01} ${cy - r} Z`
          : `M ${cx} ${cy} L ${x0} ${y0} A ${r} ${r} 0 ${largeArc} 1 ${x1} ${y1} Z`
      return { d, color: seg.color, label: seg.label, value: seg.value, frac }
    })

  return (
    <div className="flex items-center gap-4">
      {total <= 0 ? (
        <div
          className="flex shrink-0 items-center justify-center rounded-full border border-dashed border-[#2e2e2e] text-[10px] text-muted-foreground"
          style={{ width: size, height: size }}
        >
          无数据
        </div>
      ) : (
        <svg
          width={size}
          height={size}
          viewBox={`0 0 ${size} ${size}`}
          className="shrink-0"
          role="img"
        >
          {arcs.map((a, i) => (
            <path key={i} d={a.d} fill={a.color} stroke="hsl(var(--card))" strokeWidth={1} />
          ))}
        </svg>
      )}
      <ul className="min-w-0 flex-1 space-y-1 text-[11px]">
        {arcs.map((a, i) => (
          <li key={i} className="flex items-center gap-1.5">
            <span
              className="h-2.5 w-2.5 shrink-0 rounded-[2px]"
              style={{ background: a.color }}
            />
            <span className="min-w-0 flex-1 truncate text-[#ccc]" title={a.label}>
              {a.label}
            </span>
            <span className="shrink-0 tabular-nums text-muted-foreground">
              {(a.frac * 100).toFixed(1)}%
            </span>
          </li>
        ))}
      </ul>
    </div>
  )
}

// ============ 1. 请求明细 Dialog(traces,服务端搜索 + 分页) ============
export function TraceDetailDialog({
  open,
  onOpenChange,
}: {
  open: boolean
  onOpenChange: (v: boolean) => void
}) {
  // 文本输入(即时) + 防抖值(300ms,喂给查询)。text 为主搜索框(全文)。
  const [textRaw, setTextRaw] = useState('')
  const [text, setText] = useState('')
  // 高级过滤:model / clientIp / outcome / sessionId / 时间范围(收在筛选面板内)。
  const [model, setModel] = useState('')
  const [clientIp, setClientIp] = useState('')
  const [outcome, setOutcome] = useState('')
  const [sessionId, setSessionId] = useState('')
  // 时间范围以 datetime-local 字符串暂存(本地时区),查询前转 epoch 毫秒。
  const [tsFromRaw, setTsFromRaw] = useState('')
  const [tsToRaw, setTsToRaw] = useState('')
  const [offset, setOffset] = useState(0)
  const [expandedId, setExpandedId] = useState<string | null>(null)
  // 筛选面板开合(内嵌可折叠,项目无 Popover 组件)。
  const [panelOpen, setPanelOpen] = useState(false)

  // 文本防抖:输入停 300ms 后才更新查询词,避免每键一次请求。
  useEffect(() => {
    const t = setTimeout(() => setText(textRaw.trim()), 300)
    return () => clearTimeout(t)
  }, [textRaw])

  // 任一过滤条件变化即回到第一页(避免停在越界页显示空)。联动回填也走此处归零。
  useEffect(() => {
    setOffset(0)
  }, [text, model, clientIp, outcome, sessionId, tsFromRaw, tsToRaw])

  // 关闭时清空展开态与筛选面板(下次打开干净);过滤条件保留,便于二次排障。
  useEffect(() => {
    if (!open) {
      setExpandedId(null)
      setPanelOpen(false)
    }
  }, [open])

  const tsFrom = localInputToMs(tsFromRaw)
  const tsTo = localInputToMs(tsToRaw)

  // 构建过滤对象:仅带非空字段(空串不入参,后端也会归一,但保持 URL 干净)。
  const filter: TraceSearchFilter = useMemo(() => {
    const f: TraceSearchFilter = { limit: PAGE_SIZE, offset }
    if (text) f.text = text
    if (model.trim()) f.model = model.trim()
    if (clientIp.trim()) f.clientIp = clientIp.trim()
    if (outcome) f.outcome = outcome
    if (sessionId.trim()) f.sessionId = sessionId.trim()
    if (tsFrom != null) f.tsFrom = tsFrom
    if (tsTo != null) f.tsTo = tsTo
    return f
  }, [text, model, clientIp, outcome, sessionId, tsFrom, tsTo, offset])

  const { data, isLoading, isFetching } = useQuery({
    queryKey: ['traces-search', filter],
    queryFn: () => searchTraces(filter),
    enabled: open,
    placeholderData: (prev) => prev,
  })

  const items = data?.items ?? []
  const total = data?.total ?? 0
  // 分页派生:total=0 时 totalPages 兜底为 1(显示「第 1/1 页」而非 0);page 夹在合法区间。
  const totalPages = Math.max(1, Math.ceil(total / PAGE_SIZE))
  const page = total === 0 ? 1 : Math.min(totalPages, Math.floor(offset / PAGE_SIZE) + 1)
  // 面板内高级过滤条件个数(不含主搜索框 text)——用于筛选按钮上的 badge。
  const activeFilterCount =
    (model.trim() ? 1 : 0) +
    (clientIp.trim() ? 1 : 0) +
    (outcome ? 1 : 0) +
    (sessionId.trim() ? 1 : 0) +
    (tsFrom != null ? 1 : 0) +
    (tsTo != null ? 1 : 0)
  const hasFilters = !!text || activeFilterCount > 0

  // 清空面板内的高级过滤(不动主搜索框)。
  const clearPanel = () => {
    setModel('')
    setClientIp('')
    setOutcome('')
    setSessionId('')
    setTsFromRaw('')
    setTsToRaw('')
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

        {/* 过滤栏:主搜索框(全文) + 筛选按钮(打开高级筛选面板) */}
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
          <Button
            variant={panelOpen || activeFilterCount > 0 ? 'secondary' : 'outline'}
            size="sm"
            className="h-8 shrink-0 px-2 text-xs"
            onClick={() => setPanelOpen((v) => !v)}
          >
            <Filter className="mr-1 h-3.5 w-3.5" />
            筛选
            {activeFilterCount > 0 && (
              <Badge variant="default" className="ml-1.5 h-4 min-w-4 justify-center px-1 text-[10px] tabular-nums">
                {activeFilterCount}
              </Badge>
            )}
          </Button>
        </div>

        {/* 高级筛选面板(内嵌可折叠) */}
        {panelOpen && (
          <div className="space-y-3 rounded-md border border-[#2e2e2e] bg-[#0d0d0d] p-3">
            <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-3">
              {/* 用 div 而非 label 包裹:label 会把点击(含上方 span 文字区域)转发给内部 input,
                  导致"点标题文字也聚焦到输入框"的错位(dwgx 反馈)。改 div + 独立 span 标题即消除。 */}
              <div className="space-y-1">
                <span className="block text-[11px] text-muted-foreground">client IP</span>
                <Input
                  value={clientIp}
                  onChange={(e) => setClientIp(e.target.value)}
                  placeholder="如 203.0.113.5"
                  className="h-8 text-xs"
                />
              </div>
              <div className="space-y-1">
                <span className="block text-[11px] text-muted-foreground">session</span>
                <Input
                  value={sessionId}
                  onChange={(e) => setSessionId(e.target.value)}
                  placeholder="会话 ID(前缀即可)"
                  className="h-8 text-xs"
                />
              </div>
              <div className="space-y-1">
                <span className="block text-[11px] text-muted-foreground">模型</span>
                <Input
                  value={model}
                  onChange={(e) => setModel(e.target.value)}
                  placeholder="如 claude-opus-4"
                  className="h-8 text-xs"
                />
              </div>
              <div className="space-y-1">
                <span className="block text-[11px] text-muted-foreground">结果</span>
                <Select
                  value={outcome}
                  onChange={setOutcome}
                  options={OUTCOME_OPTIONS}
                  aria-label="按结果过滤"
                  className="[&>button]:h-8 [&>button]:py-1 [&>button]:text-xs"
                />
              </div>
              <div className="space-y-1">
                <span className="block text-[11px] text-muted-foreground">起始时间</span>
                <DateTimeField value={tsFromRaw} onChange={setTsFromRaw} ariaLabel="起始时间" />
              </div>
              <div className="space-y-1">
                <span className="block text-[11px] text-muted-foreground">截止时间</span>
                <DateTimeField value={tsToRaw} onChange={setTsToRaw} ariaLabel="截止时间" />
              </div>
            </div>
            <div className="flex items-center justify-end gap-2">
              <Button
                variant="ghost"
                size="sm"
                className="h-8 px-2 text-xs"
                disabled={activeFilterCount === 0}
                onClick={clearPanel}
              >
                <X className="mr-1 h-3.5 w-3.5" />
                清空
              </Button>
              <Button
                variant="secondary"
                size="sm"
                className="h-8 px-3 text-xs"
                onClick={() => setPanelOpen(false)}
              >
                应用筛选
              </Button>
            </div>
          </div>
        )}

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

  // 时间窗切换:24h / 7天 / 30天 / 全部(数据均已在 overview 里,零额外请求)。
  const [win, setWin] = useState<'last_24h' | 'last_7d' | 'last_30d' | 'all_time'>('last_24h')
  const WINDOW_OPTIONS: { key: typeof win; label: string }[] = [
    { key: 'last_24h', label: '24 小时' },
    { key: 'last_7d', label: '7 天' },
    { key: 'last_30d', label: '30 天' },
    { key: 'all_time', label: '全部' },
  ]
  const w = overview?.[win]

  // 饼图数据:①按模型的 token 占比(取 top,总和 > 0 才画) ②按号的请求数占比。
  // 只取前 8 个,其余合并为「其它」段,避免图例过长。
  const modelTokenSegments = useMemo(
    () => buildPieSegments(byModel, (r) => r.input_tokens + r.output_tokens),
    [byModel],
  )
  const credReqSegments = useMemo(
    () => buildPieSegments(byCred, (r) => r.requests, '#'),
    [byCred],
  )

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="flex max-h-[90vh] w-[min(96vw,1100px)] max-w-none flex-col overflow-hidden">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <BarChart3 className="h-4 w-4" />
            用量日志
          </DialogTitle>
          <DialogDescription>
            汇总 + 占比饼图 + 按模型 / 按号维度聚合(读本地统计,零上游)。KPI 随时间窗切换;饼图为累计维度。
          </DialogDescription>
        </DialogHeader>

        <div className="min-h-0 flex-1 space-y-4 overflow-y-auto pr-1">
          {/* 时间窗切换段控件:24h / 7天 / 30天 / 全部 */}
          <div className="inline-flex rounded-md border border-[#2e2e2e] bg-[#0d0d0d] p-0.5">
            {WINDOW_OPTIONS.map((opt) => (
              <button
                key={opt.key}
                type="button"
                onClick={() => setWin(opt.key)}
                className={
                  'h-7 rounded px-3 text-xs transition-colors ' +
                  (win === opt.key
                    ? 'bg-[#2a2a2a] text-[#ededed]'
                    : 'text-muted-foreground hover:text-[#ededed]')
                }
              >
                {opt.label}
              </button>
            ))}
          </div>

          {/* 顶部:所选时间窗 KPI */}
          {ovLoading ? (
            <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
              {Array.from({ length: 4 }).map((_, i) => (
                <Skeleton key={i} className="h-[92px]" />
              ))}
            </div>
          ) : (
            <UsageKpiRow w={w} />
          )}

          {/* 中部:两饼图并排(token 占比 / 请求数占比) */}
          <div className="grid grid-cols-1 gap-3 md:grid-cols-2">
            <UsagePieCard
              title="Token 占比(按模型)"
              loading={bmLoading}
              segments={modelTokenSegments}
            />
            <UsagePieCard
              title="请求数占比(按号)"
              loading={bcLoading}
              segments={credReqSegments}
            />
          </div>

          {/* 底部:按模型 / 按号明细表 */}
          <UsageGroupTable
            title="按模型"
            keyHeader="模型"
            rows={byModel}
            loading={bmLoading}
          />
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

// 从分组统计构建饼图段:按取值降序,取前 8,其余聚合成「其它」;循环取主题色。
function buildPieSegments(
  rows: GroupStat[] | undefined,
  pick: (r: GroupStat) => number,
  keyPrefix = '',
): PieSegment[] {
  const list = [...(rows ?? [])]
    .map((r) => ({ label: `${keyPrefix}${r.key}`, value: Math.max(0, pick(r)) }))
    .filter((r) => r.value > 0)
    .sort((a, b) => b.value - a.value)
  const TOP = 7
  const head = list.slice(0, TOP)
  const rest = list.slice(TOP)
  const segments: PieSegment[] = head.map((r, i) => ({
    ...r,
    color: PIE_COLORS[i % PIE_COLORS.length],
  }))
  if (rest.length > 0) {
    segments.push({
      label: `其它(${rest.length})`,
      value: rest.reduce((s, r) => s + r.value, 0),
      color: PIE_COLORS[PIE_COLORS.length - 1],
    })
  }
  return segments
}

// 饼图卡片(带标题 + 加载骨架 + 空态)。
function UsagePieCard({
  title,
  loading,
  segments,
}: {
  title: string
  loading: boolean
  segments: PieSegment[]
}) {
  return (
    <div className="rounded-md border border-[#2e2e2e] bg-[#0a0a0a]">
      <div className="border-b border-[#2e2e2e] px-3 py-2 text-sm font-medium">{title}</div>
      <div className="p-4">
        {loading ? (
          <Skeleton className="h-[132px]" />
        ) : segments.length === 0 ? (
          <EmptyState icon={Inbox} title="暂无数据" className="py-6" />
        ) : (
          <PieChart segments={segments} />
        )}
      </div>
    </div>
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
        // 行多时表格自身独立滚动(表头 sticky 吸顶),避免长表格把整个弹窗撑长、只能滚外层。
        <div className="max-h-[320px] overflow-y-auto">
        <table className="w-full border-collapse text-xs">
          <thead className="sticky top-0 z-10 bg-[#0a0a0a] text-[#888]">
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
        </div>
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
  // 放大预览:记住当前放大的图索引(null=未放大)。
  const [lightboxIdx, setLightboxIdx] = useState<number | null>(null)
  // 多选态:Ctrl/Cmd+点击勾选的图索引集合,用于批量下载。
  const [selectedImgs, setSelectedImgs] = useState<Set<number>>(new Set())
  const bgUrl = (i: number) => `/admin/api/bg-cached?idx=${i}`

  // 切换单张勾选(additive:保留其它选中项)。
  const toggleSelect = (i: number) => {
    setSelectedImgs((prev) => {
      const next = new Set(prev)
      if (next.has(i)) next.delete(i)
      else next.add(i)
      return next
    })
  }

  // 缩略图点击:Ctrl/Cmd 键按下=勾选/取消选(多选下载);否则=放大预览。
  const onThumbClick = (i: number, e: React.MouseEvent) => {
    if (e.ctrlKey || e.metaKey) {
      e.preventDefault()
      toggleSelect(i)
    } else {
      setLightboxIdx(i)
    }
  }

  // 批量下载选中的图:逐个触发浏览器下载(<a download> 编程点击),间隔小延时避免被浏览器拦截。
  const downloadSelected = async () => {
    const list = Array.from(selectedImgs).sort((a, b) => a - b)
    for (const i of list) {
      const a = document.createElement('a')
      a.href = bgUrl(i)
      a.download = `bg-${i}.jpg`
      document.body.appendChild(a)
      a.click()
      a.remove()
      // 多文件连续下载时给浏览器一点间隔,否则部分浏览器只保存最后一个。
      await new Promise((r) => setTimeout(r, 250))
    }
  }

  // Esc 关闭 lightbox(仅在放大态挂监听)。放大态时优先只收起 lightbox,不关整个 Dialog
  // (Dialog 的 onEscapeKeyDown 另有拦截,见下方 DialogContent)。
  useEffect(() => {
    if (lightboxIdx === null) return
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.stopPropagation()
        setLightboxIdx(null)
      }
    }
    // 捕获阶段监听,先于 Radix Dialog 的文档级监听处理,阻止其冒泡关闭 Dialog。
    window.addEventListener('keydown', onKey, true)
    return () => window.removeEventListener('keydown', onKey, true)
  }, [lightboxIdx])

  // Dialog 关闭时顺带收起 lightbox + 清空多选。
  useEffect(() => {
    if (!open) {
      setLightboxIdx(null)
      setSelectedImgs(new Set())
    }
  }, [open])

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        className="flex max-h-[85vh] w-[min(96vw,860px)] max-w-none flex-col overflow-hidden"
        onEscapeKeyDown={(e) => {
          // 放大态按 ESC:只收起 lightbox,不关整个 Dialog(拦截 Radix 默认关闭行为)。
          if (lightboxIdx !== null) {
            e.preventDefault()
            setLightboxIdx(null)
          }
        }}
      >
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <ImageIcon className="h-4 w-4" />
            登录背景图缓存
            <span className="text-xs font-normal text-muted-foreground tabular-nums">
              {count} 张
            </span>
          </DialogTitle>
          <DialogDescription>
            常驻内存的登录页随机背景图池。单击放大预览,Ctrl/⌘+单击勾选多张后可批量下载,悬停右上角可下载单张。清理即释放内存(下次登录会重新拉取填充)。
          </DialogDescription>
        </DialogHeader>

        {/* 多选工具栏:有勾选时出现,批量下载 / 清空选择。 */}
        {selectedImgs.size > 0 && (
          <div className="flex items-center gap-2 rounded-md border border-primary/40 bg-primary/[0.06] px-3 py-2 text-sm">
            <span className="text-[#ededed]">已选 {selectedImgs.size} 张</span>
            <div className="ml-auto flex items-center gap-2">
              <button
                type="button"
                onClick={downloadSelected}
                className="flex items-center gap-1 rounded bg-primary/80 px-2.5 py-1 text-xs text-white hover:bg-primary"
              >
                <Download className="h-3.5 w-3.5" />
                批量下载
              </button>
              <button
                type="button"
                onClick={() => setSelectedImgs(new Set())}
                className="rounded border border-[#2e2e2e] px-2.5 py-1 text-xs text-muted-foreground hover:text-[#ededed]"
              >
                清空
              </button>
            </div>
          </div>
        )}

        <div className="min-h-0 flex-1 overflow-y-auto">
          {idxs.length === 0 ? (
            <EmptyState icon={ImageIcon} title="缓存为空" description="尚未拉取任何背景图" />
          ) : (
            <div className="grid grid-cols-2 gap-3 sm:grid-cols-3">
              {idxs.map((i) => {
                const isSel = selectedImgs.has(i)
                return (
                <div
                  key={i}
                  onClick={(e) => onThumbClick(i, e)}
                  className={cn(
                    'group relative aspect-video cursor-zoom-in overflow-hidden rounded-md border bg-[#111]',
                    isSel ? 'border-primary ring-2 ring-primary' : 'border-[#2e2e2e]',
                  )}
                >
                  <img
                    src={bgUrl(i)}
                    loading="lazy"
                    alt={`背景图 #${i}`}
                    className="h-full w-full object-cover transition-transform duration-300 ease-out group-hover:scale-110"
                  />
                  <span className="absolute left-1.5 top-1.5 rounded bg-black/60 px-1.5 py-0.5 font-mono text-[10px] text-white/90">
                    #{i}
                  </span>
                  {/* 勾选钮:Ctrl/⌘+单击图片也可勾选;此钮直接点亦可(stopPropagation 避免放大)。 */}
                  <button
                    type="button"
                    onClick={(e) => {
                      e.stopPropagation()
                      toggleSelect(i)
                    }}
                    title={isSel ? '取消选择' : '勾选(可批量下载)'}
                    className={cn(
                      'absolute left-1.5 bottom-1.5 flex h-6 w-6 items-center justify-center rounded transition-opacity',
                      isSel
                        ? 'bg-primary text-white opacity-100'
                        : 'bg-black/60 text-white/90 opacity-0 hover:bg-black/80 group-hover:opacity-100',
                    )}
                  >
                    {isSel ? <Check className="h-3.5 w-3.5" /> : <Square className="h-3.5 w-3.5" />}
                  </button>
                  {/* hover 显示下载钮:stopPropagation 避免触发放大 */}
                  <a
                    href={bgUrl(i)}
                    download={`bg-${i}.jpg`}
                    onClick={(e) => e.stopPropagation()}
                    title="下载此图"
                    className="absolute right-1.5 top-1.5 flex h-6 w-6 items-center justify-center rounded bg-black/60 text-white/90 opacity-0 transition-opacity hover:bg-black/80 group-hover:opacity-100"
                  >
                    <Download className="h-3.5 w-3.5" />
                  </a>
                </div>
                )
              })}
            </div>
          )}
        </div>
      </DialogContent>

      {/* 放大 lightbox:全屏半透明 overlay 居中大图,点背景或 Esc 关闭 */}
      {lightboxIdx !== null && (
        <div
          onClick={() => setLightboxIdx(null)}
          className="fixed inset-0 z-[100] flex items-center justify-center bg-black/85 p-6 backdrop-blur-sm"
          role="dialog"
          aria-label={`背景图 #${lightboxIdx} 放大预览`}
        >
          <button
            onClick={() => setLightboxIdx(null)}
            title="关闭(Esc)"
            className="absolute right-4 top-4 flex h-9 w-9 items-center justify-center rounded-full bg-white/10 text-white/90 hover:bg-white/20"
          >
            <X className="h-5 w-5" />
          </button>
          <div className="relative max-h-full max-w-full" onClick={(e) => e.stopPropagation()}>
            <img
              src={bgUrl(lightboxIdx)}
              alt={`背景图 #${lightboxIdx}`}
              className="max-h-[80vh] max-w-full rounded-md object-contain shadow-2xl"
            />
            <div className="absolute bottom-2 left-2 flex items-center gap-2">
              <span className="rounded bg-black/60 px-2 py-0.5 font-mono text-xs text-white/90">
                #{lightboxIdx}
              </span>
              <a
                href={bgUrl(lightboxIdx)}
                download={`bg-${lightboxIdx}.jpg`}
                onClick={(e) => e.stopPropagation()}
                className="flex items-center gap-1 rounded bg-black/60 px-2 py-0.5 text-xs text-white/90 hover:bg-black/80"
              >
                <Download className="h-3.5 w-3.5" />
                下载
              </a>
            </div>
          </div>
        </div>
      )}
    </Dialog>
  )
}

