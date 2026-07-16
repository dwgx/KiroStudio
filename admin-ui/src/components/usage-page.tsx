import { Fragment, useMemo, useState, useEffect, useRef } from 'react'
import { createPortal } from 'react-dom'
import { useTranslation } from 'react-i18next'
import {
  Activity,
  CalendarDays,
  CalendarRange,
  Zap,
  Clock,
  TrendingUp,
  Coins,
  TerminalSquare,
  Terminal,
  Monitor,
  Laptop,
  Code,
  Code2,
  Braces,
  Globe,
  HelpCircle,
  Search,
  X,
  ChevronLeft,
  ChevronRight,
  ChevronDown,
  Server,
  type LucideIcon,
} from 'lucide-react'
import { ClaudeCodeIcon, OpenCodeIcon } from '@/components/overview/brand-icons'
import { Card } from '@/components/ui/card'
import { Badge } from '@/components/ui/badge'
import { Select } from '@/components/ui/select'
import { Skeleton } from '@/components/ui/skeleton'
import { StatCard, type StatAccent } from '@/components/ui/stat-card'
import { RadialGauge } from '@/components/overview/RadialGauge'
import { AreaTrendChart } from '@/components/overview/AreaTrendChart'
import {
  useUsageOverview,
  useUsageTimeseries,
  useUsageByModel,
  useUsageByCredential,
  useUsageRecent,
  useUsageMachines,
} from '@/hooks/use-usage'
import type {
  WindowSummary,
  SeriesPoint,
  GroupStat,
  MachineRpm,
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
  // 图标组件：lucide 通用图标或自定义品牌 SVG（brand-icons.tsx），二者接口兼容（接收 className）
  icon: React.ComponentType<{ className?: string }>
  label: string
  // 徽标 tinted 底色（低饱和背景 + 同色文字 + 细边框）
  badge: string
  // 分布条填充色（实色，用于横条渐变起止）
  bar: string
}

// 规范取值 → 元数据。取值集合与后端 classify_device 契约一致。
// 品牌名原样展示；需翻译的用 labelKey，渲染时 t()。
interface DeviceMetaRaw {
  icon: React.ComponentType<{ className?: string }>
  label?: string
  labelKey?: string
  badge: string
  bar: string
}

const DEVICE_META: Record<string, DeviceMetaRaw> = {
  'claude-code': {
    icon: ClaudeCodeIcon,
    label: 'Claude Code',
    // Anthropic 品牌橙：anthropic 官方 SDK 的 UA 后端归入 claude-code 类，故此项用品牌暖橙。
    // 用 8 位 hex（含 alpha）而非 /opacity 修饰符，避开非标准步进值被裁剪。
    badge: 'border-[#d9775740] bg-[#d9775720] text-[#e79c82]',
    bar: '#d97757',
  },
  opencode: {
    icon: OpenCodeIcon,
    label: 'OpenCode',
    // OpenCode 品牌黑白极简：暗色中性徽标 + 浅灰文字。
    badge: 'border-neutral-400/25 bg-neutral-400/10 text-neutral-200',
    bar: '#a3a3a3',
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
    labelKey: 'usagepage.device.browser',
    badge: 'border-indigo-500/25 bg-indigo-500/10 text-indigo-300',
    bar: '#6366f1',
  },
  unknown: {
    icon: HelpCircle,
    labelKey: 'usagepage.device.unknown',
    badge: 'border-border bg-secondary text-muted-foreground',
    bar: '#6b7280',
  },
}

// 归一化取回设备元数据：空值/未收录一律落到 unknown，保证永远有可展示项。
// 渲染时解析 label（labelKey → t）。
function deviceMeta(key: string | null | undefined, t: (k: string) => string): DeviceMeta {
  const raw = !key ? DEVICE_META.unknown : (DEVICE_META[key] ?? DEVICE_META.unknown)
  return {
    icon: raw.icon,
    label: raw.labelKey ? t(raw.labelKey) : (raw.label ?? key ?? ''),
    badge: raw.badge,
    bar: raw.bar,
  }
}

// 设备徽标：图标 + 中文/原文标签，tinted 底色小胶囊，一眼看清请求来源。
function DeviceBadge({ device }: { device: string | null | undefined }) {
  const { t } = useTranslation()
  const meta = deviceMeta(device, t)
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
  const { t } = useTranslation()
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
            <span className="text-muted-foreground">{t('usagepage.window.avgLatency')}</span>
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
  const { t } = useTranslation()
  const sorted = useMemo(
    () => [...rows].sort((a, b) => b.requests - a.requests).slice(0, 8),
    [rows]
  )
  if (sorted.length === 0) {
    return <p className="text-sm text-muted-foreground">{t('usagepage.group.noData')}</p>
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
  const { t } = useTranslation()
  const aggs = useMemo(() => aggregateDevices(rows), [rows])
  if (aggs.length === 0) {
    return <p className="text-sm text-muted-foreground">{t('usagepage.device.noRecords')}</p>
  }
  const total = aggs.reduce((a, d) => a + d.requests, 0)
  const max = Math.max(1, ...aggs.map((d) => d.requests))
  return (
    <ol className="space-y-3">
      {aggs.map((d) => {
        const meta = deviceMeta(d.key, t)
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

/* ============ 按机器分组（以 IP 为主键；同一会话漫游换 IP 才合并）============ */

// 一台机器一张卡:设备指纹 + 见过的所有 IP + RPM + 活跃窗口(可展开看各窗口 RPM)。
// 与「按设备」的区别:这里同一台机器换 IP(DHCP/VPN)不拆分,IP 只作列表展示。
function MachineBreakdown({
  machines,
  sessionFilter,
  onPickSession,
}: {
  machines: MachineRpm[]
  sessionFilter: string | null
  onPickSession: (sessionId: string) => void
}) {
  const { t } = useTranslation()
  // 多开手风琴：改用 Set，可同时展开多台机器（dwgx：不要开一个收起上一个）。
  const [openKeys, setOpenKeys] = useState<Set<string>>(new Set())
  const toggle = (k: string) =>
    setOpenKeys((prev) => {
      const next = new Set(prev)
      next.has(k) ? next.delete(k) : next.add(k)
      return next
    })
  if (machines.length === 0) {
    return <p className="text-sm text-muted-foreground">{t('usagepage.machine.noData')}</p>
  }
  // 按 RPM 降序,正在打的排前面。
  const sorted = [...machines].sort((a, b) => b.rpm - a.rpm)
  return (
    <div className="space-y-2">
      {sorted.map((m) => {
        const open = openKeys.has(m.machineKey)
        const label = [m.device, m.os, m.browser].filter(Boolean).join(' · ') || t('usagepage.machine.unknownDevice')
        return (
          <div key={m.machineKey} className="rounded-lg border border-border bg-secondary/30">
            <button
              onClick={() => toggle(m.machineKey)}
              className="flex w-full items-center gap-2.5 px-3 py-2 text-left"
            >
              <ChevronDown className={`h-3.5 w-3.5 shrink-0 text-muted-foreground/50 transition-transform ${open ? '' : '-rotate-90'}`} />
              <Monitor className="h-4 w-4 shrink-0 text-muted-foreground" />
              <div className="min-w-0 flex-1">
                <div className="truncate text-xs font-medium text-foreground">{label}</div>
                <div className="mt-0.5 flex flex-wrap items-center gap-x-2 gap-y-0.5 text-[11px] text-muted-foreground">
                  <span className="tabular-nums">{m.activeSessions} {t('usagepage.machine.activeSessions')}</span>
                  <span>·</span>
                  <span title={t('usagepage.machine.ipTooltip')}>{m.ips.length} {t('usagepage.machine.ipCount')}</span>
                </div>
              </div>
              {m.rpm > 0 && (
                <span className="shrink-0 rounded bg-sky-500/10 px-1.5 py-0.5 font-mono text-[10px] tabular-nums text-sky-300/90">
                  {m.rpm}<span className="text-[8px] text-sky-300/60">/m</span>
                </span>
              )}
            </button>
            {open && (
              <div className="border-t border-border/50 px-3 py-2.5 text-[11px]">
                <div className="mb-2">
                  <div className="mb-1 text-muted-foreground">{t('usagepage.machine.seenIps')}</div>
                  <div className="flex flex-wrap gap-1.5">
                    {m.ips.length === 0 ? (
                      <span className="text-muted-foreground/60">—</span>
                    ) : (
                      m.ips.map((ip) => (
                        <span key={ip} className="rounded bg-secondary px-1.5 py-0.5 font-mono text-[10px] tabular-nums text-foreground">{ip}</span>
                      ))
                    )}
                  </div>
                </div>
                {m.sessions.length > 0 && (
                  <div>
                    <div className="mb-1 text-muted-foreground">{t('usagepage.machine.sessionRpm')}<span className="ml-1 text-muted-foreground/50">{t('usagepage.machine.sessionRpmHint')}</span></div>
                    <div className="space-y-1">
                      {m.sessions.map((s) => {
                        const picked = sessionFilter === s.sessionId
                        return (
                          <button
                            key={s.sessionId}
                            onClick={() => onPickSession(s.sessionId)}
                            title={picked ? t('usagepage.machine.sessionPickedTitle') : t('usagepage.machine.sessionPickTitle')}
                            className={`flex w-full items-center justify-between gap-2 rounded px-1.5 py-1 text-left transition-colors ${
                              picked ? 'bg-sky-500/15 ring-1 ring-sky-500/40' : 'hover:bg-secondary/60'
                            }`}
                          >
                            <span className={`truncate font-mono text-[10px] ${picked ? 'text-sky-200' : 'text-muted-foreground/80'}`}>{s.sessionId}</span>
                            <span className="shrink-0 font-mono tabular-nums text-sky-300/90">{s.rpm}/m</span>
                          </button>
                        )
                      })}
                    </div>
                  </div>
                )}
              </div>
            )}
          </div>
        )
      })}
    </div>
  )
}

/* ============ 结果徽标 ============ */

// outcome → i18n key（渲染时 t，切语言实时更新）
const OUTCOME_LABEL_KEYS: Record<RequestOutcome, string> = {
  success: 'usagepage.outcome.success',
  rate_limited: 'usagepage.outcome.rateLimited',
  auth_failed: 'usagepage.outcome.authFailed',
  quota_exhausted: 'usagepage.outcome.quotaExhausted',
  account_suspended: 'usagepage.outcome.accountSuspended',
  server_error: 'usagepage.outcome.serverError',
  bad_request: 'usagepage.outcome.badRequest',
  network_error: 'usagepage.outcome.networkError',
  other_error: 'usagepage.outcome.otherError',
}

// 结果分类 → tinted badge 语义色：成功绿 / 限流·额度黄 / 其余红
function outcomeVariant(o: RequestOutcome): 'success' | 'warning' | 'destructive' {
  if (o === 'success') return 'success'
  if (o === 'rate_limited' || o === 'quota_exhausted') return 'warning'
  return 'destructive'
}

function OutcomeBadge({ outcome }: { outcome: RequestOutcome }) {
  const { t } = useTranslation()
  const key = OUTCOME_LABEL_KEYS[outcome]
  return (
    <Badge variant={outcomeVariant(outcome)} className="text-[10px]">
      {key ? t(key) : outcome}
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
  const { t } = useTranslation()
  const dev = deviceMeta(r.client_device, t)
  // 设备三维拼一行：主类 + OS + 浏览器，各自有值才拼。
  const deviceLine = [dev.label, r.client_os, r.client_browser].filter(Boolean).join(' · ')
  return (
    <div className="w-[280px] max-w-[80vw] space-y-1.5 text-[11px] leading-relaxed">
      <div className="mb-1 flex items-center justify-between gap-3 border-b border-border/60 pb-1.5">
        <span className="font-medium text-foreground">{t('usagepage.detail.title')}</span>
        <OutcomeBadge outcome={r.outcome} />
      </div>
      <DetailRow label={t('usagepage.detail.time')} value={new Date(r.ts_ms).toLocaleString()} />
      <DetailRow label={t('usagepage.detail.model')} value={<span className="font-medium">{r.model}</span>} mono={false} />
      <DetailRow label={t('usagepage.detail.device')} value={deviceLine} mono={false} />
      <DetailRow label={t('usagepage.detail.clientIp')} value={r.client_ip} />
      <DetailRow label={t('usagepage.detail.credential')} value={r.credential_id != null ? `#${r.credential_id}` : null} />
      <DetailRow
        label={t('usagepage.detail.tokenInOut')}
        value={`${r.input_tokens.toLocaleString()} / ${r.output_tokens.toLocaleString()}`}
      />
      {r.credits_used != null && (
        <DetailRow label="Credits" value={r.credits_used.toFixed(2)} />
      )}
      <DetailRow
        label={t('usagepage.detail.latency')}
        value={
          <>
            {r.latency_ms.toLocaleString()}ms
            {r.first_token_ms != null && (
              <span className="text-muted-foreground"> · {t('usagepage.detail.firstToken')} {r.first_token_ms.toLocaleString()}ms</span>
            )}
          </>
        }
      />
      <DetailRow
        label={t('usagepage.detail.retryStream')}
        value={`${r.retries} ${t('usagepage.detail.timesUnit')} · ${r.is_streaming ? t('usagepage.detail.yes') : t('usagepage.detail.no')}`}
      />
      {r.session_id && <DetailRow label={t('usagepage.detail.sessionWindow')} value={r.session_id} />}
      <DetailRow label={t('usagepage.detail.requestId')} value={r.request_id} />
      {r.error_message && (
        <div className="mt-1.5 border-t border-border/60 pt-1.5">
          <div className="mb-0.5 text-muted-foreground">{t('usagepage.detail.errorMessage')}</div>
          <div className="break-all text-red-400">{r.error_message}</div>
        </div>
      )}
    </div>
  )
}

// 内联展开详情:横向铺开(像上面的表格 bar,左到右),响应式网格自然折成 4-5 行。
// dwgx:左键展开不要竖卡片,要"和上面 bar 一样在下面铺开几行"。
function RequestDetailSpread({ record: r }: { record: RequestRecord }) {
  const { t } = useTranslation()
  const dev = deviceMeta(r.client_device, t)
  const deviceLine = [dev.label, r.client_os, r.client_browser].filter(Boolean).join(' · ')
  const cacheR = (r as { cache_read_tokens?: number }).cache_read_tokens
  const cacheW = (r as { cache_creation_tokens?: number }).cache_creation_tokens
  // 字段项:label + value,自动流式排布(auto-fill 网格,窄屏少列宽屏多列,自然铺成几行)。
  const items: { label: string; value: React.ReactNode; mono?: boolean }[] = [
    { label: t('usagepage.detail.time'), value: new Date(r.ts_ms).toLocaleString() },
    { label: t('usagepage.detail.model'), value: r.model, mono: false },
    { label: t('usagepage.detail.result'), value: <OutcomeBadge outcome={r.outcome} />, mono: false },
    { label: t('usagepage.detail.credential'), value: r.credential_id != null ? `#${r.credential_id}` : '—' },
    { label: t('usagepage.detail.device'), value: deviceLine || '—', mono: false },
    { label: t('usagepage.detail.clientIp'), value: r.client_ip || '—' },
    { label: t('usagepage.detail.tokenInOutShort'), value: `${r.input_tokens.toLocaleString()} / ${r.output_tokens.toLocaleString()}` },
    ...(cacheR != null || cacheW != null ? [{ label: t('usagepage.detail.cacheReadWrite'), value: `${(cacheR ?? 0).toLocaleString()} / ${(cacheW ?? 0).toLocaleString()}` }] : []),
    ...(r.credits_used != null ? [{ label: 'Credits', value: r.credits_used.toFixed(2) }] : []),
    { label: t('usagepage.detail.latency'), value: `${r.latency_ms.toLocaleString()}ms${r.first_token_ms != null ? ` · ${t('usagepage.detail.firstToken')} ${r.first_token_ms.toLocaleString()}ms` : ''}` },
    { label: t('usagepage.detail.retryStream'), value: `${r.retries} ${t('usagepage.detail.timesUnit')} · ${r.is_streaming ? t('usagepage.detail.yes') : t('usagepage.detail.no')}` },
    ...(r.session_id ? [{ label: t('usagepage.detail.sessionWindow'), value: r.session_id }] : []),
    { label: t('usagepage.detail.requestId'), value: r.request_id },
  ]
  return (
    <div className="animate-rise-in">
      <div
        className="grid gap-x-6 gap-y-2.5"
        style={{ gridTemplateColumns: 'repeat(auto-fill, minmax(180px, 1fr))' }}
      >
        {items.map((it, i) => (
          <div key={i} className="flex flex-col gap-0.5 min-w-0">
            <span className="text-[10px] uppercase tracking-wide text-muted-foreground/70">{it.label}</span>
            <span className={`truncate text-xs text-foreground ${it.mono === false ? '' : 'tabular-nums'}`} title={typeof it.value === 'string' ? it.value : undefined}>
              {it.value}
            </span>
          </div>
        ))}
      </div>
      {r.error_message && (
        <div className="mt-2.5 rounded border border-red-500/20 bg-red-500/5 px-2.5 py-1.5">
          <span className="text-[10px] uppercase tracking-wide text-red-400/70">{t('usagepage.detail.errorMessage')}</span>
          <div className="mt-0.5 break-all text-xs text-red-400">{r.error_message}</div>
        </div>
      )}
    </div>
  )
}

// 右键浮窗：锚定到文档位置（absolute + pageX/pageY），随页面滚动一起滚走，不固定跟随视口。
// clamp 防右/下越界。展示整条请求详情。点外部/Esc 关闭。
function RequestPopover({ record, x, y, onClose }: { record: RequestRecord; x: number; y: number; onClose: () => void }) {
  const { t } = useTranslation()
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') onClose() }
    const onDown = () => onClose()
    window.addEventListener('keydown', onKey)
    // 延迟挂 pointerdown,避免触发浮窗的这次右键点击立刻把它关掉。
    const t = setTimeout(() => window.addEventListener('pointerdown', onDown), 0)
    return () => {
      window.removeEventListener('keydown', onKey)
      window.removeEventListener('pointerdown', onDown)
      clearTimeout(t)
    }
  }, [onClose])
  // clamp:估算浮窗尺寸,右/下越界则向左/上翻。x/y 是文档坐标(pageX/pageY),
  // 越界判断用视口宽高 + 当前滚动量换算,保证首次定位不出屏。
  const W = 300, H = 380
  const maxLeft = window.scrollX + window.innerWidth - W - 8
  const maxTop = window.scrollY + window.innerHeight - H - 8
  const left = Math.max(window.scrollX + 8, Math.min(x + 14, maxLeft))
  const top = Math.max(window.scrollY + 8, Math.min(y + 14, maxTop))
  return createPortal(
    <div
      className="absolute z-50 rounded-lg border border-border bg-popover px-3 py-2.5 shadow-xl animate-rise-in"
      style={{ left, top }}
      onPointerDown={(e) => e.stopPropagation()}
      onContextMenu={(e) => e.preventDefault()}
    >
      <RequestDetail record={record} />
      <div className="mt-2 border-t border-border/60 pt-1.5 text-center text-[10px] text-muted-foreground">{t('usagepage.popover.closeHint')}</div>
    </div>,
    document.body,
  )
}

// 列：时间 / 模型 / 设备 / 凭据 / 结果 / In-Out / 延迟。
// 交互(dwgx):左键点击行=行下方内联展开详情(手风琴);右键行=跟随鼠标浮窗看全部。
function RecentTable({ rows }: { rows: RequestRecord[] }) {
  const { t } = useTranslation()
  // 多开手风琴：改用 Set，可同时展开多行详情（dwgx：不要开一个收起上一个）。
  const [expandedIds, setExpandedIds] = useState<Set<string>>(new Set())
  const toggleRow = (id: string) =>
    setExpandedIds((prev) => {
      const next = new Set(prev)
      next.has(id) ? next.delete(id) : next.add(id)
      return next
    })
  const [popover, setPopover] = useState<{ record: RequestRecord; x: number; y: number } | null>(null)

  if (rows.length === 0) {
    return <p className="text-sm text-muted-foreground">{t('usagepage.device.noRecords')}</p>
  }
  return (
    <div className="overflow-x-auto">
      <table className="w-full text-sm">
        <thead>
          <tr className="border-b border-border text-xs text-muted-foreground">
            <th className="py-2 pr-3 text-left font-medium">{t('usagepage.detail.time')}</th>
            <th className="py-2 pr-3 text-left font-medium">{t('usagepage.detail.model')}</th>
            <th className="py-2 pr-3 text-left font-medium">{t('usagepage.detail.device')}</th>
            <th className="py-2 pr-3 text-right font-medium">{t('usagepage.detail.credential')}</th>
            <th className="py-2 pr-3 text-center font-medium">{t('usagepage.detail.result')}</th>
            <th className="py-2 pr-3 text-right font-medium">In/Out</th>
            <th className="py-2 text-right font-medium">{t('usagepage.detail.latency')}</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((r) => {
            const open = expandedIds.has(r.request_id)
            return (
              <Fragment key={r.request_id}>
                <tr
                  className={`cursor-pointer border-b border-border/40 transition-colors last:border-0 hover:bg-secondary/40 ${open ? 'bg-secondary/60' : ''}`}
                  onClick={() => toggleRow(r.request_id)}
                  onContextMenu={(e) => { e.preventDefault(); setPopover({ record: r, x: e.pageX, y: e.pageY }) }}
                  title={t('usagepage.table.rowTitle')}
                >
                  <td className="whitespace-nowrap py-2 pr-3 tabular-nums text-muted-foreground">
                    <span className="inline-flex items-center gap-1">
                      <ChevronDown className={`h-3 w-3 shrink-0 transition-transform ${open ? '' : '-rotate-90'} text-muted-foreground/50`} />
                      {new Date(r.ts_ms).toLocaleString()}
                    </span>
                  </td>
                  <td className="max-w-[160px] truncate py-2 pr-3">{r.model}</td>
                  <td className="py-2 pr-3 align-top"><DeviceCell record={r} /></td>
                  <td className="py-2 pr-3 text-right tabular-nums text-muted-foreground">
                    {r.credential_id != null ? `#${r.credential_id}` : '—'}
                  </td>
                  <td className="py-2 pr-3 text-center"><OutcomeBadge outcome={r.outcome} /></td>
                  <td className="py-2 pr-3 text-right tabular-nums">{r.input_tokens}/{r.output_tokens}</td>
                  <td className="py-2 text-right tabular-nums text-muted-foreground">{r.latency_ms}ms</td>
                </tr>
                {open && (
                  <tr className="border-b border-border/40 bg-secondary/20">
                    <td colSpan={7} className="px-3 py-3">
                      <RequestDetailSpread record={r} />
                    </td>
                  </tr>
                )}
              </Fragment>
            )
          })}
        </tbody>
      </table>
      {popover && (
        <RequestPopover record={popover.record} x={popover.x} y={popover.y} onClose={() => setPopover(null)} />
      )}
    </div>
  )
}

// 最近请求面板:搜索 + 按 IP 筛选 + 每 IP 总计 + 分页,包裹 RecentTable。
const PAGE_SIZE = 20

function RecentRequestsPanel({
  rows,
  sessionFilter,
  onClearSession,
}: {
  rows: RequestRecord[]
  // 会话预选联动（T3）：由父级传入的当前会话过滤（点机器分组里的会话行设置），null=不筛。
  sessionFilter?: string | null
  onClearSession?: () => void
}) {
  const { t } = useTranslation()
  const [query, setQuery] = useState('')
  const [ipFilter, setIpFilter] = useState<string | null>(null)
  const [page, setPage] = useState(0)

  // 可筛选的 IP 列表(去重,按出现次数降序),供下拉筛选。
  const ipOptions = useMemo(() => {
    const m = new Map<string, number>()
    for (const r of rows) {
      const ip = r.client_ip || ''
      if (ip) m.set(ip, (m.get(ip) ?? 0) + 1)
    }
    return [...m.entries()].sort((a, b) => b[1] - a[1]).map(([ip]) => ip)
  }, [rows])

  // 过滤:先按 IP,再按搜索词(model/ip/凭据/request_id/error/session/device 全文)。
  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase()
    return rows.filter((r) => {
      if (sessionFilter && r.session_id !== sessionFilter) return false
      if (ipFilter && (r.client_ip || '') !== ipFilter) return false
      if (!q) return true
      const hay = [
        r.model, r.client_ip, r.client_device, r.client_os, r.client_browser,
        r.credential_id != null ? `#${r.credential_id}` : '', r.request_id,
        r.session_id, r.error_message, r.outcome,
      ].filter(Boolean).join(' ').toLowerCase()
      return hay.includes(q)
    })
  }, [rows, query, ipFilter, sessionFilter])

  // 当前筛选集的总计(条数/成功失败/token/缓存读写)。
  const totals = useMemo(() => {
    let ok = 0, fail = 0, inTok = 0, outTok = 0, cacheR = 0, cacheW = 0
    for (const r of filtered) {
      if (r.outcome === 'success') ok++; else fail++
      inTok += r.input_tokens || 0
      outTok += r.output_tokens || 0
      cacheR += (r as { cache_read_tokens?: number }).cache_read_tokens || 0
      cacheW += (r as { cache_creation_tokens?: number }).cache_creation_tokens || 0
    }
    return { count: filtered.length, ok, fail, inTok, outTok, cacheR, cacheW }
  }, [filtered])

  // 筛选/搜索变化时回到第一页。
  useEffect(() => { setPage(0) }, [query, ipFilter, sessionFilter])

  const pageCount = Math.max(1, Math.ceil(filtered.length / PAGE_SIZE))
  const clampedPage = Math.min(page, pageCount - 1)
  const paged = filtered.slice(clampedPage * PAGE_SIZE, clampedPage * PAGE_SIZE + PAGE_SIZE)

  const fmtNum = (n: number) => (n >= 1_000_000 ? `${(n / 1_000_000).toFixed(1)}M` : n >= 1000 ? `${(n / 1000).toFixed(1)}k` : String(n))

  return (
    <div className="space-y-3">
      {/* 工具栏:搜索 + IP 筛选 */}
      <div className="flex flex-wrap items-center gap-2">
        <div className="relative flex-1 min-w-[180px]">
          <Search className="pointer-events-none absolute left-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted-foreground" />
          <input
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder={t('usagepage.recent.searchPlaceholder')}
            className="h-8 w-full rounded-md border border-border bg-secondary/40 pl-8 pr-7 text-xs outline-none focus:border-border-hover"
          />
          {query && (
            <button onClick={() => setQuery('')} className="absolute right-2 top-1/2 -translate-y-1/2 text-muted-foreground hover:text-foreground" aria-label={t('usagepage.recent.clearSearch')}>
              <X className="h-3.5 w-3.5" />
            </button>
          )}
        </div>
        {ipOptions.length > 0 && (
          <div className="flex items-center gap-1.5">
            <Server className="h-3.5 w-3.5 text-muted-foreground" />
            <Select
              value={ipFilter ?? ''}
              onChange={(v) => setIpFilter(v || null)}
              className="w-36"
              aria-label={t('usagepage.recent.filterByIp')}
              options={[{ value: '', label: t('usagepage.recent.allIp') }, ...ipOptions.map((ip) => ({ value: ip, label: ip }))]}
            />
          </div>
        )}
      </div>

      {/* 当前筛选集总计 */}
      <div className="flex flex-wrap items-center gap-x-4 gap-y-1 rounded-md bg-secondary/30 px-3 py-2 text-[11px] tabular-nums text-muted-foreground">
        <span>{t('usagepage.recent.totalPrefix')} <span className="font-medium text-foreground">{totals.count}</span> {t('usagepage.recent.totalSuffix')}</span>
        <span className="text-emerald-400">{t('usagepage.recent.successCount')} {totals.ok}</span>
        {totals.fail > 0 && <span className="text-red-400">{t('usagepage.recent.failCount')} {totals.fail}</span>}
        <span>In/Out {fmtNum(totals.inTok)}/{fmtNum(totals.outTok)}</span>
        <span title={t('usagepage.recent.cacheReadTooltip')}>{t('usagepage.recent.cacheRead')} {fmtNum(totals.cacheR)}</span>
        <span title={t('usagepage.recent.cacheWriteTooltip')}>{t('usagepage.recent.cacheWrite')} {fmtNum(totals.cacheW)}</span>
        {ipFilter && <span className="text-primary">· {t('usagepage.recent.filteredBy')} {ipFilter}</span>}
        {sessionFilter && (
          <button
            onClick={onClearSession}
            title={t('usagepage.recent.clearSessionFilter')}
            className="inline-flex items-center gap-1 rounded bg-sky-500/15 px-1.5 py-0.5 text-sky-300 ring-1 ring-sky-500/30 hover:bg-sky-500/25"
          >
            <span>· {t('usagepage.recent.session')} {sessionFilter.slice(0, 8)}…</span>
            <X className="h-3 w-3" />
          </button>
        )}
      </div>

      {filtered.length === 0 ? (
        <p className="py-4 text-center text-sm text-muted-foreground">{t('usagepage.recent.noMatch')}</p>
      ) : (
        <RecentTable rows={paged} />
      )}

      {/* 分页 */}
      {pageCount > 1 && (
        <div className="flex items-center justify-between text-xs text-muted-foreground">
          <span className="tabular-nums">{t('usagepage.recent.pagePrefix')} {clampedPage + 1} / {pageCount} {t('usagepage.recent.pageSuffix')}</span>
          <div className="flex items-center gap-1">
            <button
              onClick={() => setPage(Math.max(0, clampedPage - 1))}
              disabled={clampedPage === 0}
              className="inline-flex h-7 items-center gap-1 rounded border border-border px-2 disabled:opacity-40 hover:bg-secondary/40"
            >
              <ChevronLeft className="h-3.5 w-3.5" /> {t('usagepage.recent.prevPage')}
            </button>
            <button
              onClick={() => setPage(Math.min(pageCount - 1, clampedPage + 1))}
              disabled={clampedPage >= pageCount - 1}
              className="inline-flex h-7 items-center gap-1 rounded border border-border px-2 disabled:opacity-40 hover:bg-secondary/40"
            >
              {t('usagepage.recent.nextPage')} <ChevronRight className="h-3.5 w-3.5" />
            </button>
          </div>
        </div>
      )}
    </div>
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
  const { t } = useTranslation()
  return (
    <Card className="p-12 text-center text-sm text-muted-foreground">
      {t('usagepage.statsDisabled.text')}
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
  const { t } = useTranslation()
  const [granularity, setGranularity] = useState<'hourly' | 'daily'>('hourly')
  // 会话预选联动（T3）：点机器分组里的会话行 → 设此 filter → 最近请求按该会话过滤 + 滚动定位。
  const [sessionFilter, setSessionFilter] = useState<string | null>(null)
  const recentPanelRef = useRef<HTMLDivElement>(null)
  const onPickSession = (sessionId: string) => {
    setSessionFilter((prev) => (prev === sessionId ? null : sessionId)) // 再点同一会话=取消
    // 定位到最近请求面板，让联动结果立刻可见。
    requestAnimationFrame(() => recentPanelRef.current?.scrollIntoView({ behavior: 'smooth', block: 'start' }))
  }
  const overview = useUsageOverview()
  const timeseries = useUsageTimeseries(granularity)
  const byModel = useUsageByModel()
  const byCredential = useUsageByCredential()
  // 最近请求条数（dwgx：可切换,不止 200）。0="全部"，后端取到硬上限(5万)的真全量；表格分页渲染不炸 DOM。
  const [recentLimit, setRecentLimit] = useState<number>(200)
  const recent = useUsageRecent(recentLimit)
  // 机器维度聚合（按设备指纹分组，IP 变化不拆分）——后端 /usage/machines。
  const machines = useUsageMachines()

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
    const unit = granularity === 'hourly' ? t('usagepage.unit.hour') : t('usagepage.unit.day')
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
        <h2 className="text-xl font-semibold text-gradient-brand">{t('usagepage.title')}</h2>
        <StatsDisabled />
      </div>
    )
  }

  return (
    <div className="space-y-6">
      <h2 className="text-xl font-semibold text-gradient-brand">{t('usagepage.title')}</h2>

      {/* 1) 三窗口 KPI 墙：大数字 + 成功率环 + 分项。数据未到位时逐卡骨架占位。 */}
      <div className="grid gap-4 md:grid-cols-3">
        {overview.data ? (
          <>
            <WindowCard label={t('usagepage.window.last24h')} icon={Activity} w={overview.data.last_24h} />
            <WindowCard label={t('usagepage.window.last7d')} icon={CalendarDays} w={overview.data.last_7d} />
            <WindowCard label={t('usagepage.window.last30d')} icon={CalendarRange} w={overview.data.last_30d} />
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
          <h3 className="text-sm font-medium text-foreground">{t('usagepage.trend.title')}</h3>
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
                {g === 'hourly' ? t('usagepage.trend.hourly') : t('usagepage.trend.daily')}
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
                  label={`${t('usagepage.trend.peakRequest')} ${trendMetrics.unit}`}
                  value={compact(trendMetrics.peak.requests)}
                  sub={trendMetrics.peakLabel}
                />
                <MiniMetric
                  icon={Clock}
                  label={t('usagepage.trend.activePeriod')}
                  value={`${trendMetrics.activeBuckets}`}
                  sub={`/ ${chartSeries.length} ${trendMetrics.unit}`}
                />
                <MiniMetric
                  icon={TrendingUp}
                  label={t('usagepage.trend.intervalRate')}
                  value={trendMetrics.rate === null ? '—' : `${trendMetrics.rate}%`}
                  sub={trendMetrics.rate === null ? undefined : `${compact(trendMetrics.totalOk)} ${t('usagepage.trend.successSuffix')}`}
                />
                <MiniMetric
                  icon={Coins}
                  label={t('usagepage.trend.intervalRequests')}
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
                <span className="h-0.5 w-4 rounded bg-primary" /> {t('usagepage.trend.legendRequests')}
              </span>
              <span className="flex items-center gap-1.5">
                <span className="h-0 w-4 border-t-2 border-dashed border-emerald-500" /> {t('usagepage.trend.legendRate')}
              </span>
              <span className="flex items-center gap-1.5">
                <span className="h-1.5 w-1.5 rounded-full bg-red-500/60" /> {t('usagepage.trend.legendHasFailure')}
              </span>
            </div>
          </>
        )}
      </Card>

      {/* 3) 三分组：按模型 / 按凭据 / 按设备 */}
      <div className="grid gap-4 lg:grid-cols-3">
        <Card className="p-5">
          <SectionTitle hint={t('usagepage.group.byModelHint')}>{t('usagepage.group.byModel')}</SectionTitle>
          {byModel.isLoading ? <RankListSkeleton /> : <GroupRankList rows={byModel.data ?? []} />}
        </Card>
        <Card className="p-5">
          <SectionTitle hint={t('usagepage.group.byCredentialHint')}>{t('usagepage.group.byCredential')}</SectionTitle>
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
            hint={recent.isLoading ? t('usagepage.loading') : `${deviceKinds} ${t('usagepage.group.deviceKindsHint')} ${recentRows.length} ${t('usagepage.recent.countSuffix')}`}
          >
            {t('usagepage.group.byDevice')}
          </SectionTitle>
          {recent.isLoading ? <RankListSkeleton /> : <DeviceDistribution rows={recentRows} />}
        </Card>
      </div>

      {/* 3.5) 按机器分组：同一台机器换 IP 也不拆分（设备指纹 + 会话粘滞） */}
      <Card className="p-5">
        <SectionTitle
          hint={machines.isLoading ? t('usagepage.loading') : `${(machines.data ?? []).length} ${t('usagepage.machine.countHint')}`}
        >
          {t('usagepage.machine.sectionTitle')}
        </SectionTitle>
        {machines.isLoading ? <RankListSkeleton /> : <MachineBreakdown machines={machines.data ?? []} sessionFilter={sessionFilter} onPickSession={onPickSession} />}
      </Card>

      {/* 4) 最近请求明细（搜索 + 按IP筛选 + 会话联动筛选 + 每IP总计 + 分页 + 左键展开/右键浮窗 + 条数切换） */}
      <Card className="p-5" ref={recentPanelRef}>
        <div className="mb-4 flex items-center justify-between gap-3">
          <h3 className="text-sm font-medium text-foreground">{t('usagepage.recent.title')}</h3>
          <div className="flex shrink-0 items-center gap-2">
            <span className="text-xs text-muted-foreground">
              {recent.isLoading ? t('usagepage.loading') : `${recentRows.length} ${t('usagepage.recent.countSuffix')}`}
            </span>
            {/* 条数切换：200/500/1000/全部。"全部"传 limit=0，后端解释为取到硬上限(5万)的真全量。dwgx：不止最近 200 条 */}
            <Select
              value={String(recentLimit)}
              onChange={(v) => setRecentLimit(Number(v))}
              className="w-28"
              aria-label={t('usagepage.recent.limitLabel')}
              options={[
                { value: '200', label: t('usagepage.recent.limit200') },
                { value: '500', label: t('usagepage.recent.limit500') },
                { value: '1000', label: t('usagepage.recent.limit1000') },
                { value: '0', label: t('usagepage.recent.limitAll') },
              ]}
            />
          </div>
        </div>
        {recent.isLoading ? (
          <RecentTableSkeleton />
        ) : (
          <RecentRequestsPanel
            rows={recentRows}
            sessionFilter={sessionFilter}
            onClearSession={() => setSessionFilter(null)}
          />
        )}
      </Card>
    </div>
  )
}
