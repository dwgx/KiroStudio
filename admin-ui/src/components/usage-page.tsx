import { useMemo, useState } from 'react'
import {
  Activity,
  CalendarDays,
  CalendarRange,
  Zap,
  Clock,
  TrendingUp,
  Coins,
  Bot,
  TerminalSquare,
  Terminal,
  Monitor,
  Laptop,
  Code,
  Code2,
  Braces,
  Globe,
  HelpCircle,
  type LucideIcon,
} from 'lucide-react'
import { Card } from '@/components/ui/card'
import { Badge } from '@/components/ui/badge'
import { Skeleton } from '@/components/ui/skeleton'
import {
  Tooltip,
  TooltipTrigger,
  TooltipContent,
  TooltipProvider,
} from '@/components/ui/tooltip'
import { StatCard, type StatAccent } from '@/components/ui/stat-card'
import { RadialGauge } from '@/components/overview/RadialGauge'
import { AreaTrendChart } from '@/components/overview/AreaTrendChart'
import {
  useUsageOverview,
  useUsageTimeseries,
  useUsageByModel,
  useUsageByCredential,
  useUsageRecent,
} from '@/hooks/use-usage'
import type {
  WindowSummary,
  SeriesPoint,
  GroupStat,
  RequestRecord,
  RequestOutcome,
} from '@/types/api'

// 大数字紧凑显示：1234 -> 1.2k
function compact(n: number): string {
  if (n < 1000) return n.toLocaleString()
  if (n < 1_000_000) return `${(n / 1000).toFixed(1)}k`
  return `${(n / 1_000_000).toFixed(1)}M`
}

// 成功率(0~100)映射到 StatCard 语义色
function accentForRate(rate: number | null): StatAccent {
  if (rate === null) return 'neutral'
  if (rate >= 90) return 'success'
  if (rate < 70) return 'destructive'
  return 'warning'
}

/* ============ 设备识别 ============ */

// 单个设备的展示元数据。className 全部写成静态完整字符串，确保 Tailwind 打包时不被裁剪。
interface DeviceMeta {
  icon: LucideIcon
  label: string
  // 徽标 tinted 底色（低饱和背景 + 同色文字 + 细边框）
  badge: string
  // 分布条填充色（实色，用于横条渐变起止）
  bar: string
}

// 规范取值 → 元数据。取值集合与后端 classify_device 契约一致。
const DEVICE_META: Record<string, DeviceMeta> = {
  'claude-code': {
    icon: Bot,
    label: 'Claude Code',
    // Anthropic 品牌橙：anthropic 官方 SDK 的 UA 后端归入 claude-code 类，故此项用品牌暖橙。
    // 用 8 位 hex（含 alpha）而非 /opacity 修饰符，避开非标准步进值被裁剪。
    badge: 'border-[#d9775740] bg-[#d9775720] text-[#e79c82]',
    bar: '#d97757',
  },
  curl: {
    icon: TerminalSquare,
    label: 'curl',
    badge: 'border-teal-500/25 bg-teal-500/10 text-teal-300',
    bar: '#14b8a6',
  },
  windows: {
    icon: Monitor,
    label: 'Windows',
    badge: 'border-sky-500/25 bg-sky-500/10 text-sky-300',
    bar: '#0ea5e9',
  },
  macos: {
    icon: Laptop,
    label: 'macOS',
    badge: 'border-slate-400/25 bg-slate-400/10 text-slate-300',
    bar: '#94a3b8',
  },
  linux: {
    icon: Terminal,
    label: 'Linux',
    badge: 'border-amber-500/25 bg-amber-500/10 text-amber-300',
    bar: '#f59e0b',
  },
  python: {
    icon: Code,
    label: 'Python',
    badge: 'border-cyan-500/25 bg-cyan-500/10 text-cyan-300',
    bar: '#06b6d4',
  },
  node: {
    icon: Code2,
    label: 'Node',
    badge: 'border-emerald-500/25 bg-emerald-500/10 text-emerald-300',
    bar: '#10b981',
  },
  vscode: {
    icon: Braces,
    label: 'VS Code',
    badge: 'border-blue-500/25 bg-blue-500/10 text-blue-300',
    bar: '#3b82f6',
  },
  browser: {
    icon: Globe,
    label: '浏览器',
    badge: 'border-indigo-500/25 bg-indigo-500/10 text-indigo-300',
    bar: '#6366f1',
  },
  unknown: {
    icon: HelpCircle,
    label: '未知',
    badge: 'border-border bg-secondary text-muted-foreground',
    bar: '#6b7280',
  },
}

// 归一化取回设备元数据：空值/未收录一律落到 unknown，保证永远有可展示项。
function deviceMeta(key: string | null | undefined): DeviceMeta {
  if (!key) return DEVICE_META.unknown
  return DEVICE_META[key] ?? DEVICE_META.unknown
}

// 设备徽标：图标 + 中文/原文标签，tinted 底色小胶囊，一眼看清请求来源。
function DeviceBadge({ device }: { device: string | null | undefined }) {
  const meta = deviceMeta(device)
  const Icon = meta.icon
  return (
    <span
      className={`inline-flex items-center gap-1.5 rounded-md border px-2 py-0.5 text-[11px] font-medium leading-none ${meta.badge}`}
      title={device ?? 'unknown'}
    >
      <Icon className="h-3 w-3 shrink-0" />
      <span>{meta.label}</span>
    </span>
  )
}

// 设备明细单元：主行设备徽标，次行灰色小字补充 OS · 浏览器 · IP。
// 三个维度各自「有值才显示」，全空则次行整体省略，保持表格紧凑不杂乱。
function DeviceCell({ record }: { record: RequestRecord }) {
  // 收集存在的细分维度，用「·」分隔；IP 作为最末维度，仍是灰色小字。
  const parts: string[] = []
  if (record.client_os) parts.push(record.client_os)
  if (record.client_browser) parts.push(record.client_browser)
  if (record.client_ip) parts.push(record.client_ip)
  return (
    <div className="flex flex-col gap-0.5">
      <DeviceBadge device={record.client_device} />
      {parts.length > 0 && (
        <span
          className="truncate text-[10px] leading-tight text-muted-foreground/80 tabular-nums"
          title={parts.join(' · ')}
        >
          {parts.join(' · ')}
        </span>
      )}
    </div>
  )
}

/* ============ 通用小部件 ============ */

// 区块小标题（对齐概览页 SectionTitle）
function SectionTitle({ children, hint }: { children: React.ReactNode; hint?: string }) {
  return (
    <div className="mb-4 flex items-baseline justify-between gap-3">
      <h3 className="text-sm font-medium text-foreground">{children}</h3>
      {hint && <span className="shrink-0 text-xs text-muted-foreground">{hint}</span>}
    </div>
  )
}

// 趋势图上方的小指标（复刻概览页 MiniMetric）
function MiniMetric({
  icon: Icon,
  label,
  value,
  sub,
}: {
  icon: LucideIcon
  label: string
  value: React.ReactNode
  sub?: string
}) {
  return (
    <div className="flex items-center gap-2.5">
      <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-md bg-secondary text-muted-foreground">
        <Icon className="h-4 w-4" />
      </div>
      <div className="min-w-0">
        <div className="text-[11px] leading-tight text-muted-foreground">{label}</div>
        <div className="flex items-baseline gap-1.5">
          <span className="text-sm font-semibold tabular-nums text-foreground">{value}</span>
          {sub && <span className="text-[11px] text-muted-foreground">{sub}</span>}
        </div>
      </div>
    </div>
  )
}

// 单个时间窗口的 KPI 卡：金属按压 StatCard 外壳 + 内嵌 RadialGauge(成功率环) + 迷你分项。
function WindowCard({
  label,
  icon,
  w,
}: {
  label: string
  icon: LucideIcon
  w: WindowSummary
}) {
  const hasReq = w.requests > 0
  const rate = hasReq ? Math.round(w.success_rate * 100) : null
  return (
    <StatCard
      label={label}
      icon={icon}
      accent={accentForRate(rate)}
      value={
        <div className="flex items-center gap-3">
          <span>{compact(w.requests)}</span>
          <RadialGauge value={rate} size={44} stroke={6} />
        </div>
      }
      hint={
        <div className="grid w-full grid-cols-2 gap-x-4 gap-y-1 pt-0.5 text-[11px] tabular-nums">
          <span className="flex justify-between">
            <span className="text-muted-foreground">Tokens</span>
            <span className="font-medium text-foreground">{compact(w.total_tokens)}</span>
          </span>
          <span className="flex justify-between">
            <span className="text-muted-foreground">Credits</span>
            <span className="font-medium text-foreground">{w.credits_used.toFixed(2)}</span>
          </span>
          <span className="flex justify-between">
            <span className="text-muted-foreground">In/Out</span>
            <span className="font-medium text-foreground">
              {compact(w.input_tokens)}/{compact(w.output_tokens)}
            </span>
          </span>
          <span className="flex justify-between">
            <span className="text-muted-foreground">均延迟</span>
            <span className="font-medium text-foreground">{Math.round(w.avg_latency_ms)}ms</span>
          </span>
        </div>
      }
    />
  )
}

/* ============ 分组榜单（按模型 / 按凭据）============ */

// 沿用概览 RankBars 设计语言（序号徽标 + 渐变横条 + 宽度动画），横条右侧并列成功率着色数字。
// 请求量决定横条长度（相对榜首归一化）；成功率 ≥90 绿 / <70 红 / 中间黄。
function GroupRankList({
  rows,
  labelMap,
}: {
  rows: GroupStat[]
  /** 可选的 key → 展示名映射（如凭据 #id -> 备注） */
  labelMap?: (key: string) => string
}) {
  const sorted = useMemo(
    () => [...rows].sort((a, b) => b.requests - a.requests).slice(0, 8),
    [rows]
  )
  if (sorted.length === 0) {
    return <p className="text-sm text-muted-foreground">暂无数据</p>
  }
  const max = Math.max(1, ...sorted.map((r) => r.requests))
  return (
    <ol className="space-y-3">
      {sorted.map((r, i) => {
        const pct = Math.round((r.requests / max) * 100)
        const rate = r.requests > 0 ? Math.round(r.success_rate * 100) : null
        const rateColor =
          rate === null
            ? 'text-muted-foreground'
            : rate >= 90
              ? 'text-emerald-400'
              : rate < 70
                ? 'text-red-400'
                : 'text-amber-400'
        return (
          <li key={r.key} className="space-y-1.5">
            <div className="flex items-center justify-between gap-2 text-xs">
              <span className="flex min-w-0 items-center gap-2">
                <span
                  className={`flex h-4 w-4 shrink-0 items-center justify-center rounded text-[10px] font-semibold tabular-nums ${
                    i === 0 ? 'bg-primary/15 text-primary' : 'bg-secondary text-muted-foreground'
                  }`}
                >
                  {i + 1}
                </span>
                <span className="truncate text-muted-foreground" title={r.key}>
                  {labelMap ? labelMap(r.key) : r.key}
                </span>
              </span>
              <span className="flex shrink-0 items-baseline gap-3 tabular-nums">
                <span className={`font-medium ${rateColor}`}>{rate === null ? '—' : `${rate}%`}</span>
                <span className="w-14 text-right font-medium text-foreground">
                  {compact(r.requests)}
                </span>
              </span>
            </div>
            <div className="h-2 w-full overflow-hidden rounded-full bg-muted">
              <div
                className="h-full rounded-full transition-[width] duration-700 ease-out-expo motion-reduce:transition-none"
                style={{
                  width: `${pct}%`,
                  background: 'linear-gradient(90deg, hsl(var(--primary)/0.55), hsl(var(--primary)))',
                }}
              />
            </div>
            <div className="flex justify-between text-[10px] text-muted-foreground tabular-nums">
              <span>{compact(r.input_tokens + r.output_tokens)} tokens</span>
              <span>
                {r.credits_used.toFixed(2)} credits · {Math.round(r.avg_latency_ms)}ms
              </span>
            </div>
          </li>
        )
      })}
    </ol>
  )
}

/* ============ 按设备分布（前端从 recent 聚合）============ */

// 单个设备的聚合结果
interface DeviceAgg {
  key: string
  requests: number
  success: number
}

// 对最近请求按 client_device 聚合出各设备的请求数与成功数，降序返回。
function aggregateDevices(rows: RequestRecord[]): DeviceAgg[] {
  const map = new Map<string, DeviceAgg>()
  for (const r of rows) {
    const key = r.client_device || 'unknown'
    const agg = map.get(key) ?? { key, requests: 0, success: 0 }
    agg.requests += 1
    if (r.outcome === 'success') agg.success += 1
    map.set(key, agg)
  }
  return [...map.values()].sort((a, b) => b.requests - a.requests)
}

// 按设备分布：每个设备一行——设备徽标 + 请求占比横条（用设备主题色）+ 请求数/占比。
// 数据来自前端对 recent 的聚合，无需后端新端点。
function DeviceDistribution({ rows }: { rows: RequestRecord[] }) {
  const aggs = useMemo(() => aggregateDevices(rows), [rows])
  if (aggs.length === 0) {
    return <p className="text-sm text-muted-foreground">暂无请求记录</p>
  }
  const total = aggs.reduce((a, d) => a + d.requests, 0)
  const max = Math.max(1, ...aggs.map((d) => d.requests))
  return (
    <ol className="space-y-3">
      {aggs.map((d) => {
        const meta = deviceMeta(d.key)
        const pct = Math.round((d.requests / max) * 100)
        const share = total > 0 ? Math.round((d.requests / total) * 100) : 0
        return (
          <li key={d.key} className="space-y-1.5">
            <div className="flex items-center justify-between gap-2 text-xs">
              <DeviceBadge device={d.key} />
              <span className="flex shrink-0 items-baseline gap-3 tabular-nums">
                <span className="text-muted-foreground">{share}%</span>
                <span className="w-12 text-right font-medium text-foreground">
                  {compact(d.requests)}
                </span>
              </span>
            </div>
            <div className="h-2 w-full overflow-hidden rounded-full bg-muted">
              <div
                className="h-full rounded-full transition-[width] duration-700 ease-out-expo motion-reduce:transition-none"
                style={{
                  width: `${pct}%`,
                  background: `linear-gradient(90deg, ${meta.bar}88, ${meta.bar})`,
                }}
              />
            </div>
          </li>
        )
      })}
    </ol>
  )
}

/* ============ 结果徽标 ============ */

const OUTCOME_LABEL: Record<RequestOutcome, string> = {
  success: '成功',
  rate_limited: '限流',
  auth_failed: '认证失败',
  quota_exhausted: '额度用尽',
  account_suspended: '账户暂停',
  server_error: '服务端错误',
  bad_request: '请求错误',
  network_error: '网络错误',
  other_error: '其它错误',
}

// 结果分类 → tinted badge 语义色：成功绿 / 限流·额度黄 / 其余红
function outcomeVariant(o: RequestOutcome): 'success' | 'warning' | 'destructive' {
  if (o === 'success') return 'success'
  if (o === 'rate_limited' || o === 'quota_exhausted') return 'warning'
  return 'destructive'
}

function OutcomeBadge({ outcome }: { outcome: RequestOutcome }) {
  return (
    <Badge variant={outcomeVariant(outcome)} className="text-[10px]">
      {OUTCOME_LABEL[outcome] ?? outcome}
    </Badge>
  )
}

/* ============ 最近请求明细表 ============ */

// 详情弹层里的单行：左灰标签、右等宽值，值可换行不撑破。空值统一显示「—」。
function DetailRow({
  label,
  value,
  mono = true,
}: {
  label: string
  value: React.ReactNode
  mono?: boolean
}) {
  const empty = value === null || value === undefined || value === ''
  return (
    <div className="flex items-start justify-between gap-4">
      <span className="shrink-0 text-muted-foreground">{label}</span>
      <span
        className={`min-w-0 break-all text-right text-foreground ${mono ? 'tabular-nums' : ''}`}
      >
        {empty ? <span className="text-muted-foreground/60">—</span> : value}
      </span>
    </div>
  )
}

// 一条请求的完整详情：hover 行时平滑弹出。汇总设备三维（device·os·browser）、
// 客户端 IP、凭据、结果（含错误信息）、token、延迟/首字、重试、流式、会话窗口等。
function RequestDetail({ record: r }: { record: RequestRecord }) {
  const dev = deviceMeta(r.client_device)
  // 设备三维拼一行：主类 + OS + 浏览器，各自有值才拼。
  const deviceLine = [dev.label, r.client_os, r.client_browser].filter(Boolean).join(' · ')
  return (
    <div className="w-[280px] max-w-[80vw] space-y-1.5 text-[11px] leading-relaxed">
      <div className="mb-1 flex items-center justify-between gap-3 border-b border-border/60 pb-1.5">
        <span className="font-medium text-foreground">请求详情</span>
        <OutcomeBadge outcome={r.outcome} />
      </div>
      <DetailRow label="时间" value={new Date(r.ts_ms).toLocaleString()} />
      <DetailRow label="模型" value={<span className="font-medium">{r.model}</span>} mono={false} />
      <DetailRow label="设备" value={deviceLine} mono={false} />
      <DetailRow label="客户端 IP" value={r.client_ip} />
      <DetailRow label="凭据" value={r.credential_id != null ? `#${r.credential_id}` : null} />
      <DetailRow
        label="Token（入/出）"
        value={`${r.input_tokens.toLocaleString()} / ${r.output_tokens.toLocaleString()}`}
      />
      {r.credits_used != null && (
        <DetailRow label="Credits" value={r.credits_used.toFixed(2)} />
      )}
      <DetailRow
        label="延迟"
        value={
          <>
            {r.latency_ms.toLocaleString()}ms
            {r.first_token_ms != null && (
              <span className="text-muted-foreground"> · 首字 {r.first_token_ms.toLocaleString()}ms</span>
            )}
          </>
        }
      />
      <DetailRow
        label="重试 / 流式"
        value={`${r.retries} 次 · ${r.is_streaming ? '是' : '否'}`}
      />
      {r.session_id && <DetailRow label="会话窗口" value={r.session_id} />}
      <DetailRow label="请求 ID" value={r.request_id} />
      {r.error_message && (
        <div className="mt-1.5 border-t border-border/60 pt-1.5">
          <div className="mb-0.5 text-muted-foreground">错误信息</div>
          <div className="break-all text-red-400">{r.error_message}</div>
        </div>
      )}
    </div>
  )
}

// 列：时间 / 模型 / 设备(带图标徽标) / 凭据 / 结果 / In-Out / 延迟。
// 行悬停高亮，并平滑弹出该请求的完整详情浮层——不用点就能 hover 看全每条请求。
function RecentTable({ rows }: { rows: RequestRecord[] }) {
  if (rows.length === 0) {
    return <p className="text-sm text-muted-foreground">暂无请求记录</p>
  }
  return (
    <TooltipProvider delayDuration={120} skipDelayDuration={300}>
      <div className="overflow-x-auto">
        <table className="w-full text-sm">
          <thead>
            <tr className="border-b border-border text-xs text-muted-foreground">
              <th className="py-2 pr-3 text-left font-medium">时间</th>
              <th className="py-2 pr-3 text-left font-medium">模型</th>
              <th className="py-2 pr-3 text-left font-medium">设备</th>
              <th className="py-2 pr-3 text-right font-medium">凭据</th>
              <th className="py-2 pr-3 text-center font-medium">结果</th>
              <th className="py-2 pr-3 text-right font-medium">In/Out</th>
              <th className="py-2 text-right font-medium">延迟</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((r) => (
              <Tooltip key={r.request_id}>
                <TooltipTrigger asChild>
                  <tr className="cursor-default border-b border-border/40 transition-colors last:border-0 hover:bg-secondary/40 data-[state=delayed-open]:bg-secondary/60">
                    <td className="whitespace-nowrap py-2 pr-3 tabular-nums text-muted-foreground">
                      {new Date(r.ts_ms).toLocaleString()}
                    </td>
                    <td className="max-w-[160px] truncate py-2 pr-3">{r.model}</td>
                    <td className="py-2 pr-3 align-top">
                      <DeviceCell record={r} />
                    </td>
                    <td className="py-2 pr-3 text-right tabular-nums text-muted-foreground">
                      {r.credential_id != null ? `#${r.credential_id}` : '—'}
                    </td>
                    <td className="py-2 pr-3 text-center">
                      <OutcomeBadge outcome={r.outcome} />
                    </td>
                    <td className="py-2 pr-3 text-right tabular-nums">
                      {r.input_tokens}/{r.output_tokens}
                    </td>
                    <td className="py-2 text-right tabular-nums text-muted-foreground">
                      {r.latency_ms}ms
                    </td>
                  </tr>
                </TooltipTrigger>
                <TooltipContent side="left" align="start" className="px-3 py-2.5">
                  <RequestDetail record={r} />
                </TooltipContent>
              </Tooltip>
            ))}
          </tbody>
        </table>
      </div>
    </TooltipProvider>
  )
}

/* ============ 骨架屏（局部加载占位）============ */

// 单张 KPI 卡骨架：贴合 WindowCard 的金属卡排版——标题条 + 大数值区(数字+环) + 双列分项。
function WindowCardSkeleton() {
  return (
    <div className="card-metal p-5">
      <Skeleton className="h-3 w-24" />
      <div className="mt-4 flex items-center gap-3">
        <Skeleton className="h-7 w-20" />
        <Skeleton className="h-11 w-11 rounded-full" />
      </div>
      <div className="mt-4 grid grid-cols-2 gap-x-4 gap-y-2">
        {Array.from({ length: 4 }).map((_, i) => (
          <Skeleton key={i} className="h-3 w-full" />
        ))}
      </div>
    </div>
  )
}

// 分组榜单骨架：若干行「序号 + 标签 + 横条」，贴合 GroupRankList / DeviceDistribution。
function RankListSkeleton({ rows = 5 }: { rows?: number }) {
  return (
    <ol className="space-y-3">
      {Array.from({ length: rows }).map((_, i) => (
        <li key={i} className="space-y-1.5">
          <div className="flex items-center justify-between gap-2">
            <Skeleton className="h-4 w-28" />
            <Skeleton className="h-4 w-12" />
          </div>
          <Skeleton className="h-2 w-full rounded-full" />
        </li>
      ))}
    </ol>
  )
}

// 最近请求表骨架：表头占位 + 若干行等宽条带。
function RecentTableSkeleton({ rows = 8 }: { rows?: number }) {
  return (
    <div className="space-y-2.5">
      <Skeleton className="h-4 w-full opacity-60" />
      {Array.from({ length: rows }).map((_, i) => (
        <Skeleton key={i} className="h-6 w-full" />
      ))}
    </div>
  )
}

/* ============ 降级态 ============ */

// 统计功能未启用（后端返回 503）时的提示
function StatsDisabled() {
  return (
    <Card className="p-12 text-center text-sm text-muted-foreground">
      用量统计未启用。请在配置中设置 <code className="font-mono">usageEnabled: true</code> 并重启服务。
    </Card>
  )
}

// 时间序列裁剪：后端 hourly 返回 48 桶跨 2 天、前导多为空桶，
// 空桶占去一半宽度会把有数据的段横向挤扁 → 看着"空"。
// 与概览页（slice(-24)）同一治法：先截最近区间，再裁掉前导全 0 桶，让有数据段铺满可视宽度。
function trimSeries(points: SeriesPoint[], granularity: 'hourly' | 'daily'): SeriesPoint[] {
  const windowed = granularity === 'hourly' ? points.slice(-24) : points.slice(-30)
  const firstIdx = windowed.findIndex((p) => p.requests > 0)
  return firstIdx > 0 ? windowed.slice(firstIdx) : windowed
}

/* ============ 页面 ============ */

// 信息架构（自上而下）：
//  1. 主标题「用量统计」
//  2. 三窗口 KPI 墙（24h / 7d / 30d，金属按压卡 + 成功率环）——用了多少、成功率
//  3. 请求趋势（AreaTrendChart，按小时/按天切换，上方 4 个区间小指标）——最近走势
//  4. 三分组（按模型 / 按凭据 / 按设备）——谁在用、用什么
//  5. 最近请求明细表（含设备列）——最近发生了什么、每条来自哪台设备
export function UsagePage() {
  const [granularity, setGranularity] = useState<'hourly' | 'daily'>('hourly')
  const overview = useUsageOverview()
  const timeseries = useUsageTimeseries(granularity)
  const byModel = useUsageByModel()
  const byCredential = useUsageByCredential()
  // 拉够多条（200）以便按设备聚合出有代表性的分布
  const recent = useUsageRecent(200)

  // 裁剪后的趋势数据（去空桶后铺满宽度），据此算峰值/活跃桶/合计小指标
  const chartSeries = useMemo(
    () => trimSeries(timeseries.data ?? [], granularity),
    [timeseries.data, granularity]
  )
  const trendMetrics = useMemo(() => {
    if (chartSeries.length === 0) return null
    let peak = chartSeries[0]
    for (const p of chartSeries) if (p.requests > peak.requests) peak = p
    const totalReq = chartSeries.reduce((a, p) => a + p.requests, 0)
    const totalOk = chartSeries.reduce((a, p) => a + p.success, 0)
    const activeBuckets = chartSeries.filter((p) => p.requests > 0).length
    const unit = granularity === 'hourly' ? '小时' : '天'
    const d = new Date(peak.ts_ms)
    const peakLabel =
      granularity === 'hourly'
        ? `${String(d.getHours()).padStart(2, '0')}:00`
        : `${d.getMonth() + 1}/${d.getDate()}`
    const rate = totalReq > 0 ? Math.round((totalOk / totalReq) * 100) : null
    return { peak, peakLabel, totalReq, totalOk, activeBuckets, unit, rate }
  }, [chartSeries, granularity])

  // 从最近请求聚合出参与聚合的设备种类数（供「按设备」区块副标题展示）
  const recentRows = recent.data ?? []
  const deviceKinds = useMemo(
    () => new Set(recentRows.map((r) => r.client_device || 'unknown')).size,
    [recentRows]
  )

  // 后端未启用统计时返回 503
  const disabled =
    (overview.error as { response?: { status?: number } } | undefined)?.response?.status === 503

  if (disabled) {
    return (
      <div className="space-y-6">
        <h2 className="text-xl font-semibold text-gradient-brand">用量统计</h2>
        <StatsDisabled />
      </div>
    )
  }

  return (
    <div className="space-y-6">
      <h2 className="text-xl font-semibold text-gradient-brand">用量统计</h2>

      {/* 1) 三窗口 KPI 墙：大数字 + 成功率环 + 分项。数据未到位时逐卡骨架占位。 */}
      <div className="grid gap-4 md:grid-cols-3">
        {overview.data ? (
          <>
            <WindowCard label="最近 24 小时" icon={Activity} w={overview.data.last_24h} />
            <WindowCard label="最近 7 天" icon={CalendarDays} w={overview.data.last_7d} />
            <WindowCard label="最近 30 天" icon={CalendarRange} w={overview.data.last_30d} />
          </>
        ) : (
          <>
            <WindowCardSkeleton />
            <WindowCardSkeleton />
            <WindowCardSkeleton />
          </>
        )}
      </div>

      {/* 2) 全宽请求趋势：AreaTrendChart（面积渐变 + 平滑曲线 + 跟随鼠标 tooltip） */}
      <Card className="p-5">
        <div className="mb-4 flex flex-wrap items-center justify-between gap-3">
          <h3 className="text-sm font-medium text-foreground">请求趋势</h3>
          <div className="inline-flex rounded-md border border-border bg-secondary/40 p-0.5 text-xs">
            {(['hourly', 'daily'] as const).map((g) => (
              <button
                key={g}
                onClick={() => setGranularity(g)}
                className={`rounded px-2.5 py-1 font-medium transition-colors ${
                  granularity === g
                    ? 'bg-card text-foreground shadow-sm'
                    : 'text-muted-foreground hover:text-foreground'
                }`}
              >
                {g === 'hourly' ? '按小时' : '按天'}
              </button>
            ))}
          </div>
        </div>

        {timeseries.isLoading ? (
          <div className="space-y-5">
            <div className="grid grid-cols-2 gap-4 border-b border-border pb-4 sm:grid-cols-4">
              {Array.from({ length: 4 }).map((_, i) => (
                <div key={i} className="flex items-center gap-2.5">
                  <Skeleton className="h-8 w-8 rounded-md" />
                  <div className="flex-1 space-y-1.5">
                    <Skeleton className="h-2.5 w-16" />
                    <Skeleton className="h-3 w-10" />
                  </div>
                </div>
              ))}
            </div>
            <Skeleton className="h-[280px] w-full rounded-lg" />
          </div>
        ) : (
          <>
            {trendMetrics && (
              <div className="mb-5 grid grid-cols-2 gap-4 border-b border-border pb-4 sm:grid-cols-4">
                <MiniMetric
                  icon={Zap}
                  label={`峰值请求 / ${trendMetrics.unit}`}
                  value={compact(trendMetrics.peak.requests)}
                  sub={trendMetrics.peakLabel}
                />
                <MiniMetric
                  icon={Clock}
                  label="活跃时段"
                  value={`${trendMetrics.activeBuckets}`}
                  sub={`/ ${chartSeries.length} ${trendMetrics.unit}`}
                />
                <MiniMetric
                  icon={TrendingUp}
                  label="区间成功率"
                  value={trendMetrics.rate === null ? '—' : `${trendMetrics.rate}%`}
                  sub={trendMetrics.rate === null ? undefined : `${compact(trendMetrics.totalOk)} 成功`}
                />
                <MiniMetric
                  icon={Coins}
                  label="区间请求"
                  value={compact(trendMetrics.totalReq)}
                />
              </div>
            )}

            <AreaTrendChart
              points={chartSeries}
              height={280}
              showRate
              granularity={granularity}
            />

            <div className="mt-3 flex flex-wrap gap-x-4 gap-y-1.5 text-xs text-muted-foreground">
              <span className="flex items-center gap-1.5">
                <span className="h-0.5 w-4 rounded bg-primary" /> 请求量
              </span>
              <span className="flex items-center gap-1.5">
                <span className="h-0 w-4 border-t-2 border-dashed border-emerald-500" /> 成功率
              </span>
              <span className="flex items-center gap-1.5">
                <span className="h-1.5 w-1.5 rounded-full bg-red-500/60" /> 有失败
              </span>
            </div>
          </>
        )}
      </Card>

      {/* 3) 三分组：按模型 / 按凭据 / 按设备 */}
      <div className="grid gap-4 lg:grid-cols-3">
        <Card className="p-5">
          <SectionTitle hint="按请求量排序">按模型</SectionTitle>
          {byModel.isLoading ? <RankListSkeleton /> : <GroupRankList rows={byModel.data ?? []} />}
        </Card>
        <Card className="p-5">
          <SectionTitle hint="请求量 / 成功率对比">按凭据</SectionTitle>
          {byCredential.isLoading ? (
            <RankListSkeleton />
          ) : (
            <GroupRankList
              rows={byCredential.data ?? []}
              labelMap={(k) => (/^\d+$/.test(k) ? `#${k}` : k)}
            />
          )}
        </Card>
        <Card className="p-5">
          <SectionTitle
            hint={recent.isLoading ? '加载中…' : `${deviceKinds} 种设备 · 近 ${recentRows.length} 条`}
          >
            按设备
          </SectionTitle>
          {recent.isLoading ? <RankListSkeleton /> : <DeviceDistribution rows={recentRows} />}
        </Card>
      </div>

      {/* 4) 最近请求明细（含设备列） */}
      <Card className="p-5">
        <SectionTitle hint={recent.isLoading ? '加载中…' : `最近 ${recentRows.length} 条`}>
          最近请求
        </SectionTitle>
        {recent.isLoading ? <RecentTableSkeleton /> : <RecentTable rows={recentRows} />}
      </Card>
    </div>
  )
}
