import { useState, useEffect, useRef, useCallback, useMemo, type ReactNode } from 'react'
import { useTranslation } from 'react-i18next'
import { useQuery } from '@tanstack/react-query'
import { toast } from 'sonner'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { getRecoveryMetrics, getLogs, type RecoveryMetrics, type LogEntry } from '@/api/ops'
import { PROBE_MODEL_CATALOG } from '@/api/credentials'
import { useRatelimitInsights } from '@/hooks/use-usage'
import { useLiveStream } from '@/hooks/use-live-stream'
import {
  useSetDisabled,
  useResetFailure,
  useForceRefreshToken,
  useCredentials,
  useSetPriority,
  useSetRpmLimit,
  useSetAllowedModels,
  useDeleteCredential,
} from '@/hooks/use-credentials'
import {
  useDeepVerify,
  useProbeModels,
  useEnableOverage,
  useDisableOverage,
  useSetName,
  useSetProxy,
} from '@/hooks/use-credential-ops'
import {
  useRestartService,
  useStorageStats,
  useCleanupStorage,
  useCheckUpdate,
  usePerformUpdate,
  useUpdateStatus,
} from '@/hooks/use-ops'
import type {
  RateLimitInsight,
  CredentialStatusItem,
  StoragePartition,
  StorageCleanupTarget,
} from '@/types/api'
import { cn } from '@/lib/utils'
import { storage } from '@/lib/storage'
import {
  Download,
  RefreshCw,
  Activity,
  Search,
  X,
  Copy,
  ShieldAlert,
  Zap,
  Power,
  RotateCcw,
  Inbox,
  SearchX,
  ServerCrash,
  ShieldCheck,
  Boxes,
  MoreHorizontal,
  Server,
  Database,
  Trash,
  Loader2,
  CheckCircle2,
  Clock,
  AlertTriangle,
  Gauge,
  Layers,
  Cpu,
  Timer,
  ChevronDown,
  ChevronUp,
  Eye,
} from 'lucide-react'
import { Select } from '@/components/ui/select'
import { Input } from '@/components/ui/input'
import { RegionSwitcher } from '@/components/region-switcher'
import { ProxyTestButton } from '@/components/proxy-test-button'
import { EmptyState } from '@/components/ui/empty-state'
import { Callout } from '@/components/ui/callout'
import { Skeleton } from '@/components/ui/skeleton'
import { StatCard } from '@/components/ui/stat-card'
import { AnimatedNumber } from '@/components/ui/animated-number'
import { Badge, type BadgeProps } from '@/components/ui/badge'
import { Progress } from '@/components/ui/progress'
import { Checkbox } from '@/components/ui/checkbox'
import { NumberStepper } from '@/components/ui/number-stepper'
import { ConfirmDialog } from '@/components/ui/confirm-dialog'
import { AnimatedHeight } from '@/components/ui/animated-height'
import {
  Tooltip,
  TooltipTrigger,
  TooltipContent,
  TooltipProvider,
} from '@/components/ui/tooltip'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
  DialogFooter,
} from '@/components/ui/dialog'
import {
  TraceDetailDialog,
  UsageDetailDialog,
  TrashDetailDialog,
  BgCacheDetailDialog,
} from '@/components/ops-detail-dialogs'

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
  { key: 'reclaimedInvokeCalls', label: 'invoke 重组捞回' },
  { key: 'strayGuardTripped', label: 'stray 熔断触发', warn: true },
  { key: 'strayStandaloneRequests', label: 'stray 独占行(观测)', warn: true },
  { key: 'strayInlineRequests', label: 'stray 句中泄漏(观测)', warn: true },
]

// 后端日志 ts 是 UTC RFC3339(带 Z)。转成浏览器本地时区的 HH:MM:SS.mmm 显示——
// 此前直接 slice(11,23) 切原始字符串,展示的是 UTC 时分秒(比 +0900 慢 9 小时,看着像凌晨/上午)。
// 解析失败(异常格式)回退原切片,绝不因单条坏时间戳整行崩。
function formatLocalTime(ts: string): string {
  const d = new Date(ts)
  if (Number.isNaN(d.getTime())) return ts.slice(11, 23)
  const p2 = (n: number) => String(n).padStart(2, '0')
  const ms = String(d.getMilliseconds()).padStart(3, '0')
  return `${p2(d.getHours())}:${p2(d.getMinutes())}:${p2(d.getSeconds())}.${ms}`
}

function formatUptime(ms: number): string {
  const s = Math.floor(ms / 1000)
  const d = Math.floor(s / 86400)
  const h = Math.floor((s % 86400) / 3600)
  const m = Math.floor((s % 3600) / 60)
  if (d > 0) return `${d}天 ${h}小时`
  if (h > 0) return `${h}小时 ${m}分`
  return `${m}分`
}

// 实时日志卡收缩状态持久化 key（纯前端偏好,localStorage 直存,与 use-ui-layout-prefs 同 try/catch 惯例）。
const LOGVIEWER_COLLAPSED_KEY = 'ops.logviewer.collapsed'

const LEVEL_COLORS: Record<string, string> = {
  ERROR: 'text-red-400',
  WARN: 'text-amber-400',
  INFO: 'text-sky-400',
  DEBUG: 'text-[#888]',
  TRACE: 'text-[#666]',
}

export function OpsPage() {
  // 单个 SSE /stream/live 连接在页级共享,分给实时指标条 + 号池健康卡,避免同页开两条流翻倍服务端推送。
  const live = useLiveStream(true)
  // 日志聚焦信号:OTA 检查/升级时,让实时日志展开并过滤到 update 活动(流动日志)。
  // token 每次自增触发 LogViewer 的 effect;term 是要搜的关键字([Update])。
  const [logFocus, setLogFocus] = useState<{ token: number; term: string }>({ token: 0, term: '' })
  const focusLog = useCallback((term: string) => {
    setLogFocus((prev) => ({ token: prev.token + 1, term }))
  }, [])
  return (
    <TooltipProvider delayDuration={200}>
      <div className="space-y-6">
        <LiveMetricsBar live={live} />
        <PoolHealthCard live={live} />
        <RecoveryMetricsCard />
        <LogViewer focusToken={logFocus.token} focusTerm={logFocus.term} />
        <OpsAggregationCard onFocusLog={focusLog} />
      </div>
    </TooltipProvider>
  )
}

// 落盘字节人类可读（与 settings-page 同实现，存储卡复用）。
function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return '0 B'
  const units = ['B', 'KB', 'MB', 'GB', 'TB']
  const i = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1)
  const v = bytes / Math.pow(1024, i)
  return `${i === 0 ? String(v) : v.toFixed(1)} ${units[i]}`
}

// 实时指标条:全局 RPM / 在途 / 当前 RPS / Tokens-s,~1.5s SSE 实时刷新(AnimatedNumber 滚动)。
// 连接态脉冲点如实反映流是否活着;首帧未到(连接中)用 Skeleton 占位而非闪 0。
function LiveMetricsBar({ live }: { live: ReturnType<typeof useLiveStream> }) {
  const { t } = useTranslation()
  const { frame, connected } = live

  // 首帧未到达且未连上:骨架占位(避免闪现 0 值误导)。
  if (!frame && !connected) {
    return (
      <div className="grid grid-cols-2 gap-3 lg:grid-cols-4">
        {Array.from({ length: 4 }).map((_, i) => (
          <Skeleton key={i} className="h-[92px]" />
        ))}
      </div>
    )
  }

  const rps = frame?.throughput?.currentRps ?? 0
  const tps = frame?.throughput?.tokensPerSec ?? 0
  const dot = (
    <span className="flex items-center gap-1.5 text-xs text-muted-foreground">
      <span
        className={`inline-block h-1.5 w-1.5 rounded-full ${connected ? 'animate-pulse bg-emerald-400' : 'bg-[#666]'}`}
      />
      {connected ? t('opspage.live.realtime') : t('opspage.live.reconnecting')}
    </span>
  )
  return (
    <div className="grid grid-cols-2 gap-3 lg:grid-cols-4">
      <StatCard
        label={t('opspage.stat.globalRpm')}
        value={<AnimatedNumber value={frame?.globalRpm ?? 0} />}
        icon={Gauge}
        accent="primary"
        hint={dot}
      />
      <StatCard
        label={t('opspage.stat.inflight')}
        value={<AnimatedNumber value={frame?.globalInflight ?? 0} />}
        icon={Layers}
        accent={frame && frame.globalInflight > 0 ? 'warning' : 'neutral'}
      />
      <StatCard
        label={t('opspage.stat.currentRps')}
        value={<AnimatedNumber value={rps} format={(n) => n.toFixed(1)} />}
        icon={Cpu}
        accent="neutral"
      />
      <StatCard
        label={t('opspage.stat.tokensPerSec')}
        value={<AnimatedNumber value={tps} format={(n) => Math.round(n).toLocaleString()} />}
        icon={Timer}
        accent="neutral"
      />
    </div>
  )
}

function RecoveryMetricsCard() {
  const { t } = useTranslation()
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
          {t('opspage.recovery.title')}
          {data && (
            <span className="text-xs font-normal text-muted-foreground">
              {t('opspage.recovery.uptime', { uptime: formatUptime(data.uptimeMs) })}
            </span>
          )}
        </CardTitle>
        <Button variant="ghost" size="sm" onClick={() => refetch()} className="h-7 px-2">
          <RefreshCw className="h-3.5 w-3.5" />
        </Button>
      </CardHeader>
      <CardContent>
        {data && data.atRestHealthy === false && (
          <Callout variant="danger" className="mb-3">
            {t('opspage.recovery.atRestWarning')}
          </Callout>
        )}
        {isLoading ? (
          <div className="grid grid-cols-2 gap-3 sm:grid-cols-3 lg:grid-cols-5">
            {Array.from({ length: METRIC_ITEMS.length }).map((_, i) => (
              <Skeleton key={i} className="h-16" />
            ))}
          </div>
        ) : data ? (
          <div className="grid grid-cols-2 gap-3 sm:grid-cols-3 lg:grid-cols-5">
            {METRIC_ITEMS.map((it) => {
              const v = data[it.key] as number
              return (
                <StatCard
                  key={it.key}
                  label={it.label}
                  value={<AnimatedNumber value={v} />}
                  accent={it.warn && v > 0 ? 'warning' : 'neutral'}
                />
              )
            })}
          </div>
        ) : (
          <EmptyState
            icon={ServerCrash}
            tone="destructive"
            title={t('opspage.recovery.readFailTitle')}
            description={t('opspage.recovery.readFailDesc')}
            action={
              <Button variant="outline" size="sm" onClick={() => refetch()}>
                {t('opspage.common.retry')}
              </Button>
            }
          />
        )}
      </CardContent>
    </Card>
  )
}

// 图标按钮 + Tooltip 的小包装(替代裸 button title,统一 ghost icon 尺寸)。
function IconAction({
  icon: Icon,
  label,
  onClick,
  disabled,
  pending,
  className,
}: {
  icon: typeof Zap
  label: string
  onClick: () => void
  disabled?: boolean
  pending?: boolean
  className?: string
}) {
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <Button
          variant="ghost"
          size="icon"
          className={`h-7 w-7 ${className ?? ''}`}
          onClick={onClick}
          disabled={disabled}
          aria-label={label}
        >
          {pending ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <Icon className="h-3.5 w-3.5" />}
        </Button>
      </TooltipTrigger>
      <TooltipContent>{label}</TooltipContent>
    </Tooltip>
  )
}

// 单号健康行:状态 Badge + 健康分 Progress + rpm/冷却 + 快捷操作(强刷/重置/启禁 + 验活/探模型 + 更多)。
function PoolHealthRow({
  it,
  cred,
  busy,
  onRefresh,
  onReset,
  onToggleDisabled,
  onVerify,
  onProbe,
  onMore,
  verifying,
  probing,
}: {
  it: RateLimitInsight
  cred?: CredentialStatusItem
  busy: boolean
  onRefresh: () => void
  onReset: () => void
  onToggleDisabled: () => void
  onVerify: () => void
  onProbe: () => void
  onMore: () => void
  verifying: boolean
  probing: boolean
}) {
  const { t } = useTranslation()
  const st = circuitStateOf(it)
  const meta = CIRCUIT_META[st]
  const badge = CIRCUIT_BADGE[st]
  // 健康分百分比(无健康记录=满血 100%)。
  const healthPct = Math.round((it.health?.health ?? 1) * 100)
  // custom_api 号(有 baseUrl)无 region/overage/探模型概念。
  const isCustom = !!cred?.baseUrl
  return (
    <div className="grid grid-cols-[8px_48px_84px_1fr_minmax(140px,1.5fr)_auto] items-center gap-3 rounded-md border border-[#2e2e2e] bg-[#111] px-3 py-2">
      <span className={`inline-block h-2 w-2 shrink-0 rounded-full ${meta.dot}`} />
      <span className="font-mono text-xs text-[#aaa]">#{it.id}</span>
      <Badge variant={badge} className="justify-center">{t(`opspage.circuit.${st}`)}</Badge>
      {/* 健康分条 */}
      <div className="flex items-center gap-1.5" title={t('opspage.row.healthScore', { pct: healthPct })}>
        <Progress value={healthPct} invert className="h-1.5 flex-1" />
        <span className="w-8 text-right text-[10px] tabular-nums text-[#888]">{healthPct}%</span>
      </div>
      {/* 关键指标:rpm + 熔断/冷却剩余 */}
      <span className="truncate text-xs text-[#888]">
        <span className="tabular-nums">rpm {it.rpm}{it.rpmLimit > 0 ? `/${it.rpmLimit}` : ''}</span>
        {it.health?.circuitOpen && it.health.openRemainingSecs > 0 && (
          <span className="ml-2 text-red-400">{t('opspage.row.circuitRemaining', { s: it.health.openRemainingSecs })}</span>
        )}
        {it.cooldown && (
          <span className="ml-2 text-sky-400">{t('opspage.row.cooldown', { s: Math.ceil(it.cooldown.remainingMs / 1000) })}</span>
        )}
        {it.recent429 > 0 && <span className="ml-2 text-amber-400">429×{it.recent429}</span>}
      </span>
      {/* 快捷操作 */}
      <div className="flex shrink-0 items-center gap-0.5">
        <IconAction icon={Zap} label={t('opspage.row.forceRefresh')} onClick={onRefresh} disabled={busy} className="text-[#888] hover:text-sky-400" />
        <IconAction icon={RotateCcw} label={t('opspage.row.resetEnable')} onClick={onReset} disabled={busy} className="text-[#888] hover:text-emerald-400" />
        <IconAction icon={ShieldCheck} label={t('opspage.row.deepVerify')} onClick={onVerify} disabled={busy} pending={verifying} className="text-[#888] hover:text-emerald-400" />
        {!isCustom && (
          <IconAction icon={Boxes} label={t('opspage.row.probeModels')} onClick={onProbe} disabled={busy} pending={probing} className="text-[#888] hover:text-amber-400" />
        )}
        <IconAction
          icon={Power}
          label={it.disabled ? t('opspage.row.enable') : t('opspage.row.disable')}
          onClick={onToggleDisabled}
          disabled={busy}
          className={it.disabled ? 'text-[#777] hover:text-emerald-400' : 'text-emerald-400 hover:text-red-400'}
        />
        <IconAction icon={MoreHorizontal} label={t('opspage.row.moreOps')} onClick={onMore} disabled={busy} className="text-[#888] hover:text-foreground" />
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

// 熔断态 → Badge 变体(与 CIRCUIT_META 同键,展示层用 Badge 承载状态色)。
const CIRCUIT_BADGE: Record<CircuitState, BadgeProps['variant']> = {
  open: 'destructive',
  halfOpen: 'warning',
  cooldown: 'default',
  disabled: 'secondary',
  warn: 'warning',
  healthy: 'success',
}

// 号池健康总览:每号真实熔断态 + 健康分 + 冷却剩余 + 快捷运维操作(强刷/重置/启用禁用)。
// 数据双源:insights(10s 轮询,给全量字段)+ SSE live 帧(~1.5s,实时覆盖 rpm/inflight/熔断/健康分),
// 让实时指标跟手、又不丢 insights 的推断文案/软上限。
function PoolHealthCard({ live }: { live: ReturnType<typeof useLiveStream> }) {
  const { t } = useTranslation()
  const { data, isLoading } = useRatelimitInsights()
  const { data: credsResp } = useCredentials()
  const { frame, connected } = live
  const setDisabled = useSetDisabled()
  const resetFailure = useResetFailure()
  const forceRefresh = useForceRefreshToken()
  const deepVerify = useDeepVerify()
  const probeModels = useProbeModels()

  // 「更多」操作面板 + 探模型确认弹框的目标 id。
  const [moreId, setMoreId] = useState<number | null>(null)
  const [probeConfirmId, setProbeConfirmId] = useState<number | null>(null)

  // 凭据列表按 id 索引(CredOpsDialog 取 allowedModels/overageEnabled/name/baseUrl 等 insights 缺的字段)。
  const credById = useMemo(
    () => new Map((credsResp?.credentials ?? []).map((c) => [c.id, c])),
    [credsResp],
  )

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

  // 全局 busy(禁用类互斥操作时锁整行);验活/探模型走 per-id pending(见下),不进 busy。
  const busy = setDisabled.isPending || resetFailure.isPending || forceRefresh.isPending

  const runVerify = (id: number) =>
    deepVerify.mutate(id, {
      onSuccess: () => toast.success(t('opspage.toast.verifyOk', { id })),
      onError: () => toast.error(t('opspage.toast.verifyFail', { id })),
    })
  const runProbe = (id: number) =>
    probeModels.mutate(
      { id },
      {
        onSuccess: (r) =>
          toast.success(
            t('opspage.toast.probeOk', {
              id,
              ok: r.models.filter((m) => m.status === 'supported').length,
              total: r.models.length,
              credits: r.totalCredits,
            }),
          ),
        onError: () => toast.error(t('opspage.toast.probeFail', { id })),
      },
    )

  const moreCred = moreId !== null ? credById.get(moreId) : undefined

  return (
    <Card>
      <CardHeader className="flex flex-row items-center justify-between pb-2">
        <CardTitle className="flex items-center gap-2 text-base">
          <ShieldAlert className="h-4 w-4" />
          {t('opspage.pool.title')}
          <span className="flex items-center gap-1.5 text-xs font-normal text-muted-foreground">
            <span
              className={`inline-block h-1.5 w-1.5 rounded-full ${connected ? 'animate-pulse bg-emerald-400' : 'bg-[#666]'}`}
              title={connected ? t('opspage.pool.streamConnected') : t('opspage.pool.streamDisconnected')}
            />
            {connected ? t('opspage.pool.subtitleRealtime') : t('opspage.pool.subtitlePolling')}
          </span>
        </CardTitle>
      </CardHeader>
      <CardContent>
        {isLoading ? (
          <div className="space-y-1.5">
            {Array.from({ length: 3 }).map((_, i) => (
              <Skeleton key={i} className="h-11" />
            ))}
          </div>
        ) : sorted.length === 0 ? (
          <EmptyState icon={ShieldAlert} title={t('opspage.pool.emptyTitle')} description={t('opspage.pool.emptyDesc')} />
        ) : (
          <div className="space-y-1.5">
            {sorted.map((it) => (
              <PoolHealthRow
                key={it.id}
                it={it}
                cred={credById.get(it.id)}
                busy={busy}
                verifying={deepVerify.isPending && deepVerify.variables === it.id}
                probing={probeModels.isPending && probeModels.variables?.id === it.id}
                onRefresh={() => forceRefresh.mutate(it.id, {
                  onSuccess: () => toast.success(t('opspage.toast.refreshTriggered', { id: it.id })),
                  onError: () => toast.error(t('opspage.toast.refreshFail', { id: it.id })),
                })}
                onReset={() => resetFailure.mutate(it.id, {
                  onSuccess: () => toast.success(t('opspage.toast.resetEnabled', { id: it.id })),
                  onError: () => toast.error(t('opspage.toast.resetFail', { id: it.id })),
                })}
                onToggleDisabled={() => setDisabled.mutate(
                  { id: it.id, disabled: !it.disabled },
                  {
                    onSuccess: () => toast.success(it.disabled ? t('opspage.toast.toggledEnabled', { id: it.id }) : t('opspage.toast.toggledDisabled', { id: it.id })),
                    onError: () => toast.error(t('opspage.toast.opFail')),
                  },
                )}
                onVerify={() => runVerify(it.id)}
                onProbe={() => setProbeConfirmId(it.id)}
                onMore={() => setMoreId(it.id)}
              />
            ))}
          </div>
        )}
      </CardContent>

      {/* 探模型二次确认(耗真实积分) */}
      <ConfirmDialog
        open={probeConfirmId !== null}
        onOpenChange={(v) => !v && setProbeConfirmId(null)}
        title={t('opspage.probeConfirm.title', { id: probeConfirmId ?? '' })}
        description={t('opspage.probeConfirm.desc')}
        confirmLabel={t('opspage.probeConfirm.confirm')}
        loading={probeModels.isPending}
        onConfirm={() => {
          if (probeConfirmId !== null) {
            runProbe(probeConfirmId)
            setProbeConfirmId(null)
          }
        }}
      />

      {/* 更多运维操作面板 */}
      {moreCred && (
        <CredOpsDialog
          cred={moreCred}
          open={moreId !== null}
          onOpenChange={(v) => !v && setMoreId(null)}
        />
      )}
    </Card>
  )
}

// 面板内的分区小标题。
function OpsSection({ title, hint, children }: { title: string; hint?: string; children: ReactNode }) {
  return (
    <div className="space-y-2 border-t border-border/50 py-3 first:border-t-0 first:pt-0">
      <div className="flex items-baseline justify-between gap-2">
        <span className="text-sm font-medium">{title}</span>
        {hint && <span className="text-[11px] text-muted-foreground">{hint}</span>}
      </div>
      {children}
    </div>
  )
}

// 「更多」运维操作面板:切 region / 优先级 / RPM 上限 / 允许模型 / 代理 / 别名 + 破坏性(超额/禁用/删除)。
// custom_api 号(有 baseUrl)隐藏 region / overage / 探模型相关区。数据来自 useCredentials 按 id 取。
function CredOpsDialog({
  cred,
  open,
  onOpenChange,
}: {
  cred: CredentialStatusItem
  open: boolean
  onOpenChange: (v: boolean) => void
}) {
  const { t } = useTranslation()
  const id = cred.id
  const isCustom = !!cred.baseUrl

  const setPriority = useSetPriority()
  const setRpmLimit = useSetRpmLimit()
  const setAllowed = useSetAllowedModels()
  const setName = useSetName()
  const setProxy = useSetProxy()
  const enableOv = useEnableOverage()
  const disableOv = useDisableOverage()
  const deleteCred = useDeleteCredential()
  const setDisabled = useSetDisabled()

  // 本地编辑态(初值取当前值;open 变化时重置)。region 切换逻辑已抽到共享 RegionSwitcher(自持探测态)。
  const [priority, setPriorityVal] = useState(cred.priority)
  const [rpmLimit, setRpmLimitVal] = useState(cred.rpmLimit ?? 0)
  const [name, setNameVal] = useState(cred.name ?? '')
  const [proxyUrl, setProxyUrl] = useState(cred.proxyUrl ?? '')
  const [allowed, setAllowed_] = useState<string[]>(cred.allowedModels ?? [])

  // 破坏性二次确认目标。
  const [confirm, setConfirm] = useState<null | 'overage' | 'disable' | 'delete'>(null)

  useEffect(() => {
    if (open) {
      setPriorityVal(cred.priority)
      setRpmLimitVal(cred.rpmLimit ?? 0)
      setNameVal(cred.name ?? '')
      setProxyUrl(cred.proxyUrl ?? '')
      setAllowed_(cred.allowedModels ?? [])
    }
  }, [open, cred])

  const toggleModel = (m: string) =>
    setAllowed_((prev) => (prev.includes(m) ? prev.filter((x) => x !== m) : [...prev, m]))

  const label = cred.name || cred.email || `#${id}`

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-h-[85vh] max-w-lg overflow-y-auto">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <MoreHorizontal className="h-4 w-4" />
            {t('opspage.credops.title', { label })}
          </DialogTitle>
          <DialogDescription>
            {isCustom ? t('opspage.credops.descCustom') : t('opspage.credops.desc')}
          </DialogDescription>
        </DialogHeader>

        {/* 切 region(仅非 custom_api):复用凭据管理页同款 RegionSwitcher(探测/自定义 region/卡片列表)。 */}
        {!isCustom && (
          <OpsSection title={t('opspage.credops.regionTitle')} hint={t('opspage.credops.regionHint')}>
            <RegionSwitcher credentialId={id} />
          </OpsSection>
        )}

        {/* 优先级 */}
        <OpsSection title={t('opspage.credops.priorityTitle')} hint={t('opspage.credops.priorityHint')}>
          <div className="flex items-center gap-2">
            <NumberStepper value={priority} onChange={setPriorityVal} min={0} max={999} aria-label={t('opspage.credops.priorityTitle')} className="w-24" />
            <Button
              size="sm"
              variant="outline"
              disabled={setPriority.isPending || priority === cred.priority}
              onClick={() =>
                setPriority.mutate(
                  { id, priority },
                  { onSuccess: () => toast.success(t('opspage.toast.priorityUpdated')), onError: () => toast.error(t('opspage.toast.updateFail')) },
                )
              }
            >
              {t('opspage.common.save')}
            </Button>
          </div>
        </OpsSection>

        {/* RPM 上限 */}
        <OpsSection title={t('opspage.credops.rpmTitle')} hint={t('opspage.credops.rpmHint')}>
          <div className="flex items-center gap-2">
            <NumberStepper value={rpmLimit} onChange={setRpmLimitVal} min={0} max={100000} step={10} aria-label={t('opspage.credops.rpmTitle')} className="w-28" />
            <Button
              size="sm"
              variant="outline"
              disabled={setRpmLimit.isPending || rpmLimit === (cred.rpmLimit ?? 0)}
              onClick={() =>
                setRpmLimit.mutate(
                  { id, rpmLimit },
                  { onSuccess: () => toast.success(t('opspage.toast.rpmUpdated')), onError: () => toast.error(t('opspage.toast.updateFail')) },
                )
              }
            >
              {t('opspage.common.save')}
            </Button>
          </div>
        </OpsSection>

        {/* 允许模型白名单(仅非 custom_api) */}
        {!isCustom && (
          <OpsSection title={t('opspage.credops.allowedTitle')} hint={t('opspage.credops.allowedHint')}>
            <div className="grid grid-cols-2 gap-x-3 gap-y-1.5">
              {PROBE_MODEL_CATALOG.map((m) => {
                const isSel = allowed.includes(m.id)
                return (
                  <label
                    key={m.id}
                    data-state={isSel ? 'checked' : 'unchecked'}
                    className={cn(
                      // 平滑过渡:选中行高亮底色 + 边框 + 轻微上浮阴影;未选中 hover 给反馈。
                      'group flex cursor-pointer items-center gap-2 rounded-md border px-2 py-1.5 text-xs',
                      'transition-[background-color,border-color,box-shadow,transform] duration-200 ease-out',
                      isSel
                        ? 'border-primary/50 bg-primary/10 text-foreground shadow-sm shadow-primary/10'
                        : 'border-transparent hover:border-border hover:bg-accent/40',
                    )}
                  >
                    <Checkbox
                      checked={isSel}
                      onCheckedChange={() => toggleModel(m.id)}
                      className="transition-colors duration-200"
                    />
                    <span className={cn('truncate font-mono transition-colors duration-200', isSel && 'text-primary')}>{m.id}</span>
                    <span className="ml-auto shrink-0 text-[10px] text-muted-foreground">{m.mult}</span>
                  </label>
                )
              })}
            </div>
            <Button
              size="sm"
              variant="outline"
              className="mt-1"
              disabled={setAllowed.isPending}
              onClick={() =>
                setAllowed.mutate(
                  { id, allowedModels: allowed.length ? allowed : null },
                  { onSuccess: () => toast.success(t('opspage.toast.allowedUpdated')), onError: () => toast.error(t('opspage.toast.updateFail')) },
                )
              }
            >
              {allowed.length > 0 ? t('opspage.credops.saveWhitelist', { n: allowed.length }) : t('opspage.credops.saveWhitelistUnlimited')}
            </Button>
          </OpsSection>
        )}

        {/* 代理 */}
        <OpsSection title={t('opspage.credops.proxyTitle')} hint={t('opspage.credops.proxyHint')}>
          <div className="flex items-center gap-2">
            <Input
              value={proxyUrl}
              onChange={(e) => setProxyUrl(e.target.value)}
              placeholder={t('opspage.credops.proxyPlaceholder')}
              className="h-8 flex-1 text-xs"
            />
            <ProxyTestButton proxyUrl={proxyUrl} className="h-8" />
            <Button
              size="sm"
              variant="outline"
              disabled={setProxy.isPending || proxyUrl === (cred.proxyUrl ?? '')}
              onClick={() =>
                setProxy.mutate(
                  { id, proxyUrl: proxyUrl.trim() === '' ? null : proxyUrl.trim() },
                  { onSuccess: () => toast.success(t('opspage.toast.proxyUpdated')), onError: () => toast.error(t('opspage.toast.updateFail')) },
                )
              }
            >
              {t('opspage.common.save')}
            </Button>
          </div>
        </OpsSection>

        {/* 别名 */}
        <OpsSection title={t('opspage.credops.nameTitle')} hint={t('opspage.credops.nameHint')}>
          <div className="flex items-center gap-2">
            <Input
              value={name}
              onChange={(e) => setNameVal(e.target.value)}
              placeholder={t('opspage.credops.namePlaceholder')}
              className="h-8 flex-1 text-xs"
            />
            <Button
              size="sm"
              variant="outline"
              disabled={setName.isPending || name === (cred.name ?? '')}
              onClick={() =>
                setName.mutate(
                  { id, name: name.trim() === '' ? null : name.trim() },
                  { onSuccess: () => toast.success(t('opspage.toast.nameUpdated')), onError: () => toast.error(t('opspage.toast.updateFail')) },
                )
              }
            >
              {t('opspage.common.save')}
            </Button>
          </div>
        </OpsSection>

        {/* 超额(仅非 custom_api):开启破坏性(真花钱)走确认;关闭非破坏直接执行 */}
        {!isCustom && (
          <OpsSection title={t('opspage.credops.overageTitle')} hint={t('opspage.credops.overageHint')}>
            <div className="flex items-center gap-2">
              <Button
                size="sm"
                variant="destructive"
                disabled={enableOv.isPending}
                onClick={() => setConfirm('overage')}
              >
                {t('opspage.credops.enableOverage')}
              </Button>
              <Button
                size="sm"
                variant="outline"
                disabled={disableOv.isPending}
                onClick={() =>
                  disableOv.mutate(id, {
                    onSuccess: () => toast.success(t('opspage.toast.overageDisabled')),
                    onError: () => toast.error(t('opspage.toast.opFail')),
                  })
                }
              >
                {t('opspage.credops.disableOverage')}
              </Button>
              {cred.overageEnabled && <Badge variant="warning">{t('opspage.credops.overageOn')}</Badge>}
            </div>
          </OpsSection>
        )}

        {/* 危险区:禁用 / 删除 */}
        <OpsSection title={t('opspage.credops.dangerTitle')}>
          <div className="flex items-center gap-2">
            <Button
              size="sm"
              variant="outline"
              disabled={setDisabled.isPending}
              onClick={() => {
                // 启用非破坏,直接执行;禁用走二次确认。
                if (cred.disabled) {
                  setDisabled.mutate(
                    { id, disabled: false },
                    { onSuccess: () => toast.success(t('opspage.toast.enabled')), onError: () => toast.error(t('opspage.toast.opFail')) },
                  )
                } else {
                  setConfirm('disable')
                }
              }}
            >
              {cred.disabled ? t('opspage.credops.enableCred') : t('opspage.credops.disableCred')}
            </Button>
            <Button size="sm" variant="destructive" disabled={deleteCred.isPending} onClick={() => setConfirm('delete')}>
              <Trash className="mr-1 h-3.5 w-3.5" />
              {t('opspage.credops.deleteCred')}
            </Button>
          </div>
        </OpsSection>

        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>{t('opspage.common.close')}</Button>
        </DialogFooter>
      </DialogContent>

      {/* 破坏性二次确认(共享 ConfirmDialog) */}
      <ConfirmDialog
        open={confirm === 'overage'}
        onOpenChange={(v) => !v && setConfirm(null)}
        title={t('opspage.confirm.overageTitle', { id })}
        description={t('opspage.confirm.overageDesc')}
        confirmLabel={t('opspage.confirm.overageConfirm')}
        destructive
        loading={enableOv.isPending}
        onConfirm={() =>
          enableOv.mutate(id, {
            onSuccess: (s) => {
              toast.success(s.confirmed === false ? (s.note || t('opspage.toast.overagePending')) : t('opspage.toast.overageEnabled'))
              setConfirm(null)
            },
            onError: () => toast.error(t('opspage.toast.overageEnableFail')),
          })
        }
      />
      <ConfirmDialog
        open={confirm === 'disable'}
        onOpenChange={(v) => !v && setConfirm(null)}
        title={t('opspage.confirm.disableTitle', { id })}
        description={t('opspage.confirm.disableDesc')}
        confirmLabel={t('opspage.confirm.disableConfirm')}
        destructive
        loading={setDisabled.isPending}
        onConfirm={() =>
          setDisabled.mutate(
            { id, disabled: true },
            {
              onSuccess: () => {
                toast.success(t('opspage.toast.disabled'))
                setConfirm(null)
              },
              onError: () => toast.error(t('opspage.toast.opFail')),
            },
          )
        }
      />
      <ConfirmDialog
        open={confirm === 'delete'}
        onOpenChange={(v) => !v && setConfirm(null)}
        title={t('opspage.confirm.deleteTitle', { id })}
        description={t('opspage.confirm.deleteDesc')}
        confirmLabel={t('opspage.confirm.deleteConfirm')}
        destructive
        loading={deleteCred.isPending}
        onConfirm={() =>
          deleteCred.mutate(id, {
            onSuccess: () => {
              toast.success(t('opspage.toast.deleted'))
              setConfirm(null)
              onOpenChange(false)
            },
            onError: () => toast.error(t('opspage.toast.deleteFail')),
          })
        }
      />
    </Dialog>
  )
}

// 存储清理目标(与后端 target 白名单一致;bg_cache/trash 为全清)。
const CLEANABLE_KEYS: StorageCleanupTarget[] = ['traces', 'usage_jsonl', 'trash', 'bg_cache']

// 聚合运维卡:重启服务 / OTA 升级回滚观测 / 存储占用清理。
// 与 settings 页调用同一批 hooks(同 queryKey,两处并存无冲突);此处复用 hooks 不复用组件。
function OpsAggregationCard({ onFocusLog }: { onFocusLog?: (term: string) => void } = {}) {
  const { t } = useTranslation()
  const restart = useRestartService()
  const checkUpd = useCheckUpdate()
  const performUpd = usePerformUpdate()
  const { data: updStatus } = useUpdateStatus()
  const { data: storage, isLoading: storageLoading, refetch: refetchStorage } = useStorageStats()
  const cleanup = useCleanupStorage()

  const [confirm, setConfirm] = useState<null | 'restart' | 'upgrade' | { kind: 'cleanup'; p: StoragePartition }>(null)
  // 存储分区详情弹框:按分区 key 打开对应高保真明细(traces/usage_jsonl/trash/bg_cache)。
  const [detail, setDetail] = useState<null | 'traces' | 'usage_jsonl' | 'trash' | 'bg_cache'>(null)
  const updInfo = checkUpd.data

  // bg_cache 分区张数(供背景图缓存弹框渲染 idx 网格)。
  const bgCount = storage?.partitions.find((p) => p.key === 'bg_cache')?.items ?? 0

  const handleCheck = () => {
    // 聚焦实时日志到 update 活动(检查也会发 [Update] 日志:镜像探测/tags 拉取)。
    onFocusLog?.('[Update]')
    checkUpd.mutate(undefined, {
      onSuccess: (r) => {
        if (r.error) toast.error(t('opspage.toast.checkUpdFail', { error: r.error }))
        else if (r.has_update) toast.success(t('opspage.toast.updFound', { latest: r.latest_version, local: r.local_version }))
        else toast.success(t('opspage.toast.updLatest', { local: r.local_version }))
      },
      onError: (e) => toast.error(t('opspage.toast.checkUpdFail', { error: (e as Error).message })),
    })
  }

  const cleanupTarget = confirm && typeof confirm === 'object' ? confirm.p : null
  const cleanupSupportsDays = cleanupTarget ? cleanupTarget.key === 'traces' || cleanupTarget.key === 'usage_jsonl' : false

  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="flex items-center gap-2 text-base">
          <Server className="h-4 w-4" />
          {t('opspage.agg.title')}
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-4">
        {/* 重启 + OTA */}
        <div className="space-y-2">
          <div className="flex flex-wrap items-center gap-2">
            <Button variant="destructive" size="sm" disabled={restart.isPending} onClick={() => setConfirm('restart')}>
              <RotateCcw className="mr-1.5 h-4 w-4" />
              {t('opspage.agg.restart')}
            </Button>
            <Button variant="outline" size="sm" onClick={handleCheck} disabled={checkUpd.isPending || performUpd.isPending}>
              {checkUpd.isPending ? <Loader2 className="mr-1.5 h-4 w-4 animate-spin" /> : <RefreshCw className="mr-1.5 h-4 w-4" />}
              {t('opspage.agg.checkUpdate')}
            </Button>
            {updInfo && !updInfo.error && (
              <span className="text-xs text-muted-foreground">
                {t('opspage.agg.currentVersion', { local: updInfo.local_version })}
                {updInfo.latest_version && <> {t('opspage.agg.latestVersion', { latest: updInfo.latest_version })}</>}
              </span>
            )}
            {updInfo?.has_update && (
              <Button size="sm" disabled={performUpd.isPending} onClick={() => setConfirm('upgrade')}>
                {performUpd.isPending ? <Loader2 className="mr-1.5 h-4 w-4 animate-spin" /> : <RotateCcw className="mr-1.5 h-4 w-4" />}
                {t('opspage.agg.upgradeTo', { latest: updInfo.latest_version })}
              </Button>
            )}
          </div>
          {/* OTA 升级/回滚观测(后端 .health/.bak/*.failed 标记) */}
          {updStatus && (
            <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-xs">
              {updStatus.healthConfirmed ? (
                <span className="flex items-center gap-1 text-emerald-400" title={updStatus.healthDetail ?? undefined}>
                  <CheckCircle2 className="h-3.5 w-3.5" /> {t('opspage.agg.stableConfirmed')}
                </span>
              ) : (
                <span className="flex items-center gap-1 text-amber-400">
                  <Clock className="h-3.5 w-3.5" /> {t('opspage.agg.stablePending')}
                </span>
              )}
              {updStatus.rollbackPointPresent && <span className="text-muted-foreground">{t('opspage.agg.rollbackPoint')}</span>}
              {updStatus.rolledBackBinaryPresent && (
                <span className="flex items-center gap-1 text-red-400" title={t('opspage.agg.guardRolledBack')}>
                  <AlertTriangle className="h-3.5 w-3.5" /> {t('opspage.agg.rollbackDetected')}
                </span>
              )}
            </div>
          )}
        </div>

        {/* 存储占用 + 清理 */}
        <div className="border-t border-border/50 pt-3">
          <div className="mb-2 flex items-center justify-between">
            <div className="flex items-center gap-2 text-sm font-medium">
              <Database className="h-4 w-4 text-muted-foreground" />
              {t('opspage.storage.title')}
              {storage && <span className="text-xs font-normal text-muted-foreground">{t('opspage.storage.diskTotal', { bytes: formatBytes(storage.totalDiskBytes) })}</span>}
            </div>
            <Button variant="ghost" size="sm" className="h-7 px-2" onClick={() => refetchStorage()} disabled={storageLoading}>
              <RefreshCw className="h-3.5 w-3.5" />
            </Button>
          </div>
          {storageLoading ? (
            <div className="space-y-2">
              {Array.from({ length: 3 }).map((_, i) => (
                <Skeleton key={i} className="h-10" />
              ))}
            </div>
          ) : storage && storage.partitions.length > 0 ? (
            <div className="space-y-1">
              {storage.partitions.map((p) => {
                const cleanable = (CLEANABLE_KEYS as string[]).includes(p.key)
                // 四个分区各有高保真明细弹框(与 CLEANABLE_KEYS 同集)。
                const viewable = (CLEANABLE_KEYS as string[]).includes(p.key)
                return (
                  <div key={p.key} className="flex items-center justify-between gap-3 rounded-md border border-[#2e2e2e] bg-[#111] px-3 py-1.5">
                    <div className="flex min-w-0 items-center gap-2">
                      <span className="truncate text-xs">{p.label}</span>
                      {p.inMemory && <Badge variant="outline" className="text-[10px]">{t('opspage.storage.inMemory')}</Badge>}
                    </div>
                    <div className="flex shrink-0 items-center gap-3">
                      <span className="text-xs tabular-nums text-muted-foreground">{t('opspage.storage.itemStat', { bytes: formatBytes(p.bytes), items: p.items })}</span>
                      {viewable && (
                        <Button
                          variant="outline"
                          size="sm"
                          className="h-7 px-2 text-xs"
                          onClick={() => setDetail(p.key as 'traces' | 'usage_jsonl' | 'trash' | 'bg_cache')}
                        >
                          <Eye className="mr-1 h-3 w-3" />
                          {t('opspage.storage.view')}
                        </Button>
                      )}
                      {cleanable && (
                        <Button variant="outline" size="sm" className="h-7 px-2 text-xs" onClick={() => setConfirm({ kind: 'cleanup', p })}>
                          <Trash className="mr-1 h-3 w-3" />
                          {t('opspage.storage.cleanup')}
                        </Button>
                      )}
                    </div>
                  </div>
                )
              })}
            </div>
          ) : (
            <EmptyState icon={Database} title={t('opspage.storage.emptyPartitions')} />
          )}
        </div>
      </CardContent>

      {/* 存储分区高保真明细弹框(查看按钮触发,与清理并存) */}
      <TraceDetailDialog open={detail === 'traces'} onOpenChange={(v) => !v && setDetail(null)} />
      <UsageDetailDialog open={detail === 'usage_jsonl'} onOpenChange={(v) => !v && setDetail(null)} />
      <TrashDetailDialog open={detail === 'trash'} onOpenChange={(v) => !v && setDetail(null)} />
      <BgCacheDetailDialog open={detail === 'bg_cache'} onOpenChange={(v) => !v && setDetail(null)} count={bgCount} />

      <ConfirmDialog
        open={confirm === 'restart'}
        onOpenChange={(v) => !v && setConfirm(null)}
        title={t('opspage.confirm.restartTitle')}
        description={t('opspage.confirm.restartDesc')}
        confirmLabel={t('opspage.confirm.restartConfirm')}
        destructive
        loading={restart.isPending}
        onConfirm={() =>
          restart.mutate(undefined, {
            onSuccess: (r) => {
              toast.success(r.message || t('opspage.toast.restarting'))
              setConfirm(null)
            },
            onError: () => {
              toast.warning(t('opspage.toast.restartingWarn'))
              setConfirm(null)
            },
          })
        }
      />
      <ConfirmDialog
        open={confirm === 'upgrade'}
        onOpenChange={(v) => !v && setConfirm(null)}
        title={t('opspage.confirm.upgradeTitle', { latest: updInfo?.latest_version ?? t('opspage.confirm.upgradeTitleFallback') })}
        description={t('opspage.confirm.upgradeDesc')}
        confirmLabel={t('opspage.confirm.upgradeConfirm')}
        loading={performUpd.isPending}
        onConfirm={() => {
          // 聚焦实时日志到升级流程:perform_update 每步发 [Update] 日志(下载/校验/写入/替换)在此流动显示。
          onFocusLog?.('[Update]')
          performUpd.mutate(undefined, {
            onSuccess: (r) => {
              toast.success(r.message || t('opspage.toast.upgrading'))
              setConfirm(null)
            },
            onError: () => {
              toast.warning(t('opspage.toast.upgradingWarn'))
              setConfirm(null)
            },
          })
        }}
      />
      <ConfirmDialog
        open={!!cleanupTarget}
        onOpenChange={(v) => !v && setConfirm(null)}
        title={t('opspage.confirm.cleanupTitle', { label: cleanupTarget?.label ?? '' })}
        description={
          <span>
            {t('opspage.confirm.cleanupIrreversible')}
            {cleanupSupportsDays ? t('opspage.confirm.cleanupByRetention') : t('opspage.confirm.cleanupWhole')}
          </span>
        }
        confirmLabel={t('opspage.confirm.cleanupConfirm')}
        destructive
        loading={cleanup.isPending}
        onConfirm={() => {
          if (!cleanupTarget) return
          cleanup.mutate(
            { target: cleanupTarget.key as StorageCleanupTarget },
            {
              onSuccess: (resp) => {
                toast.success(resp.message)
                setConfirm(null)
              },
              onError: () => toast.error(t('opspage.toast.cleanupFail')),
            },
          )
        }}
      />
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

function LogViewer({ focusToken = 0, focusTerm = '' }: { focusToken?: number; focusTerm?: string } = {}) {
  const { t } = useTranslation()
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
  // 整卡收缩状态（收起只隐藏搜索行 + 日志区,卡头恒显；SSE 流不卸载,日志继续累积)。
  // 初值从 localStorage 读,默认展开(false)；读失败不因偏好崩。
  const [collapsed, setCollapsed] = useState<boolean>(() => {
    try {
      return localStorage.getItem(LOGVIEWER_COLLAPSED_KEY) === '1'
    } catch {
      return false
    }
  })
  // 收缩状态变化即写回 localStorage(与 use-ui-layout-prefs 同 try/catch 惯例)。
  const toggleCollapsed = useCallback(() => {
    setCollapsed((prev) => {
      const next = !prev
      try {
        localStorage.setItem(LOGVIEWER_COLLAPSED_KEY, next ? '1' : '0')
      } catch {
        /* 隐私模式/配额满:偏好写失败不影响功能 */
      }
      return next
    })
  }, [])
  const scrollRef = useRef<HTMLDivElement>(null)
  // OTA 聚焦:检查更新/升级触发(focusToken 自增)时,展开日志 + 把搜索设为 [Update],
  // 让升级步骤日志(perform_update 发的 [Update] tracing)在实时日志里流动显示。
  useEffect(() => {
    if (focusToken <= 0) return
    setCollapsed(false)
    try {
      localStorage.setItem(LOGVIEWER_COLLAPSED_KEY, '0')
    } catch {
      /* 忽略 */
    }
    setSearch(focusTerm)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [focusToken])
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
      .catch(() => toast.error(t('opspage.toast.fetchLogsFail')))
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
      toast.success(t('opspage.toast.logsExported'))
    } catch {
      toast.error(t('opspage.log.exportFail'))
    } finally {
      setDownloading(false)
    }
  }

  return (
    <Card>
      <CardHeader className="flex flex-col gap-2 pb-2">
       <div className="flex flex-row items-center justify-between gap-2">
        <CardTitle className="text-base">
          {t('opspage.log.title')}
          <span className="ml-2 text-xs font-normal text-muted-foreground tabular-nums">
            {visibleLogs.length}
            {visibleLogs.length !== logs.length ? ` / ${logs.length}` : ''} {t('opspage.log.countUnit')}
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
            title={live && !connected ? t('opspage.log.reconnectingTip') : undefined}
          >
            <span
              className={`inline-block h-1.5 w-1.5 rounded-full ${
                !live ? 'bg-[#666]' : connected ? 'animate-pulse bg-white' : 'animate-pulse bg-amber-400'
              }`}
            />
            {!live ? t('opspage.log.paused') : connected ? t('opspage.live.realtime') : t('opspage.live.reconnecting')}
          </Button>
          <Button variant="outline" size="sm" onClick={handleExport} disabled={downloading} className="h-7 gap-1 px-2 text-xs">
            <Download className="h-3.5 w-3.5" />
            {t('opspage.common.export')}
          </Button>
          {/* 收缩/展开切换:只折叠卡体(搜索行 + 日志区),卡头恒显 */}
          <Button
            variant="ghost"
            size="sm"
            onClick={toggleCollapsed}
            className="h-7 w-7 px-0"
            aria-expanded={!collapsed}
            title={collapsed ? t('opspage.log.expand') : t('opspage.log.collapse')}
          >
            {collapsed ? <ChevronDown className="h-4 w-4" /> : <ChevronUp className="h-4 w-4" />}
          </Button>
        </div>
       </div>
      </CardHeader>
      {/* 卡体(搜索行 + 日志滚动区)收缩:AnimatedHeight 平滑过渡；SSE 流不卸载,日志继续累积 */}
      <AnimatedHeight>
       {!collapsed && (
        <>
       {/* 搜索 + 模块过滤行 */}
       <div className="flex flex-row items-center gap-2 px-6 pb-2">
         <div className="relative flex-1">
           <Search className="pointer-events-none absolute left-2 top-1/2 z-10 h-3.5 w-3.5 -translate-y-1/2 text-[#666]" />
           <Input
             value={search}
             onChange={(e) => setSearch(e.target.value)}
             placeholder={t('opspage.log.searchPlaceholder')}
             className="h-7 pl-7 pr-7 text-xs"
           />
           {search && (
             <button
               onClick={() => setSearch('')}
               className="absolute right-1.5 top-1/2 z-10 -translate-y-1/2 text-[#666] hover:text-[#ededed]"
               title={t('opspage.log.clearSearch')}
             >
               <X className="h-3.5 w-3.5" />
             </button>
           )}
         </div>
         <Select
           value={moduleFilter}
           onChange={setModuleFilter}
           aria-label={t('opspage.log.filterByModule')}
           className="w-[180px] shrink-0"
           options={[
             { value: '', label: t('opspage.log.allModules') },
             ...modules.map((m) => ({ value: m, label: m })),
           ]}
         />
       </div>
      <CardContent>
        <div
          ref={scrollRef}
          onScroll={handleScroll}
          className="h-[420px] overflow-y-auto rounded-md border border-[#2e2e2e] bg-[#0a0a0a] p-2 font-mono text-xs leading-relaxed"
        >
          {visibleLogs.length === 0 ? (
            <EmptyState
              icon={logs.length === 0 ? Inbox : SearchX}
              title={logs.length === 0 ? t('opspage.log.emptyTitle') : t('opspage.log.noMatchTitle')}
              description={logs.length === 0 ? undefined : t('opspage.log.noMatchDesc')}
            />
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
        </>
       )}
      </AnimatedHeight>
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
  const { t } = useTranslation()
  const handleCopy = (ev: React.MouseEvent) => {
    ev.stopPropagation()
    // 复制整条（时间 + 级别 + 模块 + 消息），方便贴 issue。
    const line = `${entry.ts} ${entry.level} ${entry.target} ${entry.message}`
    navigator.clipboard.writeText(line).then(
      () => toast.success(t('opspage.log.copied')),
      () => toast.error(t('opspage.log.copyFail')),
    )
  }
  return (
    <div
      onClick={onToggle}
      className="flex cursor-pointer gap-2 border-b border-[#161616] py-0.5 hover:bg-[#141414]"
    >
      <span className="shrink-0 text-[#555]" title={entry.ts}>{formatLocalTime(entry.ts)}</span>
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
          title={t('opspage.log.copyWhole')}
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
