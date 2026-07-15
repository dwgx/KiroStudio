import { useState, useEffect, useRef, useCallback, useMemo, type ReactNode } from 'react'
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
  useProbeRegions,
  useSwitchRegion,
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
  CredentialRegionProfile,
} from '@/types/api'
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
} from 'lucide-react'
import { Select } from '@/components/ui/select'
import { Input } from '@/components/ui/input'
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
  return (
    <TooltipProvider delayDuration={200}>
      <div className="space-y-6">
        <LiveMetricsBar live={live} />
        <PoolHealthCard live={live} />
        <RecoveryMetricsCard />
        <LogViewer />
        <OpsAggregationCard />
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
      {connected ? '实时' : '重连中'}
    </span>
  )
  return (
    <div className="grid grid-cols-2 gap-3 lg:grid-cols-4">
      <StatCard
        label="全局 RPM"
        value={<AnimatedNumber value={frame?.globalRpm ?? 0} />}
        icon={Gauge}
        accent="primary"
        hint={dot}
      />
      <StatCard
        label="在途请求"
        value={<AnimatedNumber value={frame?.globalInflight ?? 0} />}
        icon={Layers}
        accent={frame && frame.globalInflight > 0 ? 'warning' : 'neutral'}
      />
      <StatCard
        label="当前 RPS"
        value={<AnimatedNumber value={rps} format={(n) => n.toFixed(1)} />}
        icon={Cpu}
        accent="neutral"
      />
      <StatCard
        label="Tokens/s"
        value={<AnimatedNumber value={tps} format={(n) => Math.round(n).toLocaleString()} />}
        icon={Timer}
        accent="neutral"
      />
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
          <Callout variant="danger" className="mb-3">
            at-rest 加密已开启,但上次凭据落盘回退成了明文(密钥文件读写失败)。磁盘上的凭据当前未加密——
            请检查密钥文件权限/磁盘可写后重试保存,或查看日志。
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
            title="读取失败"
            description="自愈计数端点无响应"
            action={
              <Button variant="outline" size="sm" onClick={() => refetch()}>
                重试
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
      <Badge variant={badge} className="justify-center">{meta.label}</Badge>
      {/* 健康分条 */}
      <div className="flex items-center gap-1.5" title={`健康分 ${healthPct}%`}>
        <Progress value={healthPct} invert className="h-1.5 flex-1" />
        <span className="w-8 text-right text-[10px] tabular-nums text-[#888]">{healthPct}%</span>
      </div>
      {/* 关键指标:rpm + 熔断/冷却剩余 */}
      <span className="truncate text-xs text-[#888]">
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
      <div className="flex shrink-0 items-center gap-0.5">
        <IconAction icon={Zap} label="强制刷新 Token" onClick={onRefresh} disabled={busy} className="text-[#888] hover:text-sky-400" />
        <IconAction icon={RotateCcw} label="重置失败计数并启用" onClick={onReset} disabled={busy} className="text-[#888] hover:text-emerald-400" />
        <IconAction icon={ShieldCheck} label="深度验活(真实 API 调用)" onClick={onVerify} disabled={busy} pending={verifying} className="text-[#888] hover:text-emerald-400" />
        {!isCustom && (
          <IconAction icon={Boxes} label="探测可用模型(⚠️消耗真实积分)" onClick={onProbe} disabled={busy} pending={probing} className="text-[#888] hover:text-amber-400" />
        )}
        <IconAction
          icon={Power}
          label={it.disabled ? '启用' : '禁用'}
          onClick={onToggleDisabled}
          disabled={busy}
          className={it.disabled ? 'text-[#777] hover:text-emerald-400' : 'text-emerald-400 hover:text-red-400'}
        />
        <IconAction icon={MoreHorizontal} label="更多运维操作" onClick={onMore} disabled={busy} className="text-[#888] hover:text-foreground" />
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
      onSuccess: () => toast.success(`#${id} 验活完成`),
      onError: () => toast.error(`#${id} 验活失败`),
    })
  const runProbe = (id: number) =>
    probeModels.mutate(
      { id },
      {
        onSuccess: (r) =>
          toast.success(
            `#${id} 探测完成:${r.models.filter((m) => m.status === 'supported').length}/${r.models.length} 可用,耗 ${r.totalCredits} credits`,
          ),
        onError: () => toast.error(`#${id} 探模型失败`),
      },
    )

  const moreCred = moreId !== null ? credById.get(moreId) : undefined

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
          <div className="space-y-1.5">
            {Array.from({ length: 3 }).map((_, i) => (
              <Skeleton key={i} className="h-11" />
            ))}
          </div>
        ) : sorted.length === 0 ? (
          <EmptyState icon={ShieldAlert} title="暂无凭据" description="号池为空,先在凭据页添加账号" />
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
        title={`探测 #${probeConfirmId ?? ''} 可用模型？`}
        description="将逐个候选模型发真实生成请求探测可用性——会消耗该号真实积分(通常几分钱到几毛),耗时可达数十秒。确定继续？"
        confirmLabel="确认探测"
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
  const probeRegions = useProbeRegions()
  const switchRegion = useSwitchRegion()

  // 本地编辑态(初值取当前值;open 变化时重置)。
  const [priority, setPriorityVal] = useState(cred.priority)
  const [rpmLimit, setRpmLimitVal] = useState(cred.rpmLimit ?? 0)
  const [name, setNameVal] = useState(cred.name ?? '')
  const [proxyUrl, setProxyUrl] = useState(cred.proxyUrl ?? '')
  const [allowed, setAllowed_] = useState<string[]>(cred.allowedModels ?? [])
  const [regions, setRegions] = useState<CredentialRegionProfile[]>([])
  const [selectedArn, setSelectedArn] = useState('')

  // 破坏性二次确认目标。
  const [confirm, setConfirm] = useState<null | 'overage' | 'disable' | 'delete'>(null)

  useEffect(() => {
    if (open) {
      setPriorityVal(cred.priority)
      setRpmLimitVal(cred.rpmLimit ?? 0)
      setNameVal(cred.name ?? '')
      setProxyUrl(cred.proxyUrl ?? '')
      setAllowed_(cred.allowedModels ?? [])
      setRegions([])
      setSelectedArn('')
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
            运维操作 · {label}
          </DialogTitle>
          <DialogDescription>
            单号级配置,均即时生效、无需重启{isCustom ? '(自定义 API 号:region/超额/探模型不适用)' : ''}。
          </DialogDescription>
        </DialogHeader>

        {/* 切 region(仅非 custom_api) */}
        {!isCustom && (
          <OpsSection title="切换 Region / Profile" hint="切上游 profile,非改全局 region">
            <div className="flex items-center gap-2">
              <Button
                variant="outline"
                size="sm"
                disabled={probeRegions.isPending}
                onClick={() =>
                  probeRegions.mutate(id, {
                    onSuccess: (r) => {
                      setRegions(r.regions)
                      const cur = r.regions.find((x) => x.current)
                      setSelectedArn(cur?.arn ?? r.regions[0]?.arn ?? '')
                      if (r.regions.length === 0) toast.info('未探测到可用 profile')
                    },
                    onError: () => toast.error('探测 region 失败'),
                  })
                }
              >
                {probeRegions.isPending ? <Loader2 className="mr-1.5 h-3.5 w-3.5 animate-spin" /> : <RefreshCw className="mr-1.5 h-3.5 w-3.5" />}
                探测各区域 profile
              </Button>
              {probeRegions.isPending && <span className="text-xs text-muted-foreground">向上游探测,可能耗时…</span>}
            </div>
            {regions.length > 0 && (
              <div className="mt-2 space-y-2">
                <Select
                  value={selectedArn}
                  onChange={setSelectedArn}
                  aria-label="选择 profile"
                  options={regions.map((r) => ({
                    value: r.arn,
                    label: `${r.region}${r.current ? '(当前)' : ''}`,
                    hint: `${r.subscriptionTitle ?? '未知订阅'}${r.usable ? '' : ' · 不可用'}`,
                    disabled: !r.usable,
                  }))}
                />
                <Button
                  size="sm"
                  disabled={switchRegion.isPending || !selectedArn || regions.find((r) => r.arn === selectedArn)?.current}
                  onClick={() =>
                    switchRegion.mutate(
                      { id, arn: selectedArn },
                      {
                        onSuccess: () => toast.success('已切换 profile'),
                        onError: () => toast.error('切换失败'),
                      },
                    )
                  }
                >
                  {switchRegion.isPending && <Loader2 className="mr-1.5 h-3.5 w-3.5 animate-spin" />}
                  应用切换
                </Button>
              </div>
            )}
          </OpsSection>
        )}

        {/* 优先级 */}
        <OpsSection title="优先级" hint="数字越小越优先">
          <div className="flex items-center gap-2">
            <NumberStepper value={priority} onChange={setPriorityVal} min={0} max={999} aria-label="优先级" className="w-24" />
            <Button
              size="sm"
              variant="outline"
              disabled={setPriority.isPending || priority === cred.priority}
              onClick={() =>
                setPriority.mutate(
                  { id, priority },
                  { onSuccess: () => toast.success('优先级已更新'), onError: () => toast.error('更新失败') },
                )
              }
            >
              保存
            </Button>
          </div>
        </OpsSection>

        {/* RPM 上限 */}
        <OpsSection title="RPM 上限" hint="0 = 继承全局">
          <div className="flex items-center gap-2">
            <NumberStepper value={rpmLimit} onChange={setRpmLimitVal} min={0} max={100000} step={10} aria-label="RPM 上限" className="w-28" />
            <Button
              size="sm"
              variant="outline"
              disabled={setRpmLimit.isPending || rpmLimit === (cred.rpmLimit ?? 0)}
              onClick={() =>
                setRpmLimit.mutate(
                  { id, rpmLimit },
                  { onSuccess: () => toast.success('RPM 上限已更新'), onError: () => toast.error('更新失败') },
                )
              }
            >
              保存
            </Button>
          </div>
        </OpsSection>

        {/* 允许模型白名单(仅非 custom_api) */}
        {!isCustom && (
          <OpsSection title="允许模型白名单" hint="空 = 不限制;设了即硬门">
            <div className="grid grid-cols-2 gap-x-3 gap-y-1.5">
              {PROBE_MODEL_CATALOG.map((m) => (
                <label key={m.id} className="flex cursor-pointer items-center gap-2 text-xs">
                  <Checkbox checked={allowed.includes(m.id)} onCheckedChange={() => toggleModel(m.id)} />
                  <span className="truncate font-mono">{m.id}</span>
                  <span className="shrink-0 text-[10px] text-muted-foreground">{m.mult}</span>
                </label>
              ))}
            </div>
            <Button
              size="sm"
              variant="outline"
              className="mt-1"
              disabled={setAllowed.isPending}
              onClick={() =>
                setAllowed.mutate(
                  { id, allowedModels: allowed.length ? allowed : null },
                  { onSuccess: () => toast.success('允许模型已更新'), onError: () => toast.error('更新失败') },
                )
              }
            >
              保存白名单{allowed.length > 0 ? `(${allowed.length})` : '(不限制)'}
            </Button>
          </OpsSection>
        )}

        {/* 代理 */}
        <OpsSection title="代理" hint='空=回退全局,"direct"=强制直连'>
          <div className="flex items-center gap-2">
            <Input
              value={proxyUrl}
              onChange={(e) => setProxyUrl(e.target.value)}
              placeholder="socks5://host:port 或 direct"
              className="h-8 flex-1 text-xs"
            />
            <Button
              size="sm"
              variant="outline"
              disabled={setProxy.isPending || proxyUrl === (cred.proxyUrl ?? '')}
              onClick={() =>
                setProxy.mutate(
                  { id, proxyUrl: proxyUrl.trim() === '' ? null : proxyUrl.trim() },
                  { onSuccess: () => toast.success('代理已更新'), onError: () => toast.error('更新失败') },
                )
              }
            >
              保存
            </Button>
          </div>
        </OpsSection>

        {/* 别名 */}
        <OpsSection title="别名 / 备注" hint="展示优先于 email/#id">
          <div className="flex items-center gap-2">
            <Input
              value={name}
              onChange={(e) => setNameVal(e.target.value)}
              placeholder="留空清除"
              className="h-8 flex-1 text-xs"
            />
            <Button
              size="sm"
              variant="outline"
              disabled={setName.isPending || name === (cred.name ?? '')}
              onClick={() =>
                setName.mutate(
                  { id, name: name.trim() === '' ? null : name.trim() },
                  { onSuccess: () => toast.success('别名已更新'), onError: () => toast.error('更新失败') },
                )
              }
            >
              保存
            </Button>
          </div>
        </OpsSection>

        {/* 超额(仅非 custom_api):开启破坏性(真花钱)走确认;关闭非破坏直接执行 */}
        {!isCustom && (
          <OpsSection title="超额 Overage" hint="超 base 额度后按真实用量计费">
            <div className="flex items-center gap-2">
              <Button
                size="sm"
                variant="destructive"
                disabled={enableOv.isPending}
                onClick={() => setConfirm('overage')}
              >
                开启超额
              </Button>
              <Button
                size="sm"
                variant="outline"
                disabled={disableOv.isPending}
                onClick={() =>
                  disableOv.mutate(id, {
                    onSuccess: () => toast.success('已关闭超额'),
                    onError: () => toast.error('操作失败'),
                  })
                }
              >
                关闭超额
              </Button>
              {cred.overageEnabled && <Badge variant="warning">当前已开启</Badge>}
            </div>
          </OpsSection>
        )}

        {/* 危险区:禁用 / 删除 */}
        <OpsSection title="危险操作">
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
                    { onSuccess: () => toast.success('已启用'), onError: () => toast.error('操作失败') },
                  )
                } else {
                  setConfirm('disable')
                }
              }}
            >
              {cred.disabled ? '启用凭据' : '禁用凭据'}
            </Button>
            <Button size="sm" variant="destructive" disabled={deleteCred.isPending} onClick={() => setConfirm('delete')}>
              <Trash className="mr-1 h-3.5 w-3.5" />
              删除凭据
            </Button>
          </div>
        </OpsSection>

        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>关闭</Button>
        </DialogFooter>
      </DialogContent>

      {/* 破坏性二次确认(共享 ConfirmDialog) */}
      <ConfirmDialog
        open={confirm === 'overage'}
        onOpenChange={(v) => !v && setConfirm(null)}
        title={`开启 #${id} 超额？`}
        description="开启后该号超出 base 额度仍会继续服务,超出部分按真实用量向上游付费——这会产生真实费用。确定开启？"
        confirmLabel="确认开启(会产生费用)"
        destructive
        loading={enableOv.isPending}
        onConfirm={() =>
          enableOv.mutate(id, {
            onSuccess: (s) => {
              toast.success(s.confirmed === false ? (s.note || '已提交,状态待确认') : '已开启超额')
              setConfirm(null)
            },
            onError: () => toast.error('开启失败'),
          })
        }
      />
      <ConfirmDialog
        open={confirm === 'disable'}
        onOpenChange={(v) => !v && setConfirm(null)}
        title={`禁用 #${id}？`}
        description="禁用后该号将从调度池中移除,不再被选中处理请求(可随时重新启用)。确定禁用？"
        confirmLabel="确认禁用"
        destructive
        loading={setDisabled.isPending}
        onConfirm={() =>
          setDisabled.mutate(
            { id, disabled: true },
            {
              onSuccess: () => {
                toast.success('已禁用')
                setConfirm(null)
              },
              onError: () => toast.error('操作失败'),
            },
          )
        }
      />
      <ConfirmDialog
        open={confirm === 'delete'}
        onOpenChange={(v) => !v && setConfirm(null)}
        title={`删除 #${id}？`}
        description="将把该凭据移入回收站(可在回收站恢复,永久清除后不可恢复)。确定删除？"
        confirmLabel="确认删除"
        destructive
        loading={deleteCred.isPending}
        onConfirm={() =>
          deleteCred.mutate(id, {
            onSuccess: () => {
              toast.success('已删除(移入回收站)')
              setConfirm(null)
              onOpenChange(false)
            },
            onError: () => toast.error('删除失败'),
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
function OpsAggregationCard() {
  const restart = useRestartService()
  const checkUpd = useCheckUpdate()
  const performUpd = usePerformUpdate()
  const { data: updStatus } = useUpdateStatus()
  const { data: storage, isLoading: storageLoading, refetch: refetchStorage } = useStorageStats()
  const cleanup = useCleanupStorage()

  const [confirm, setConfirm] = useState<null | 'restart' | 'upgrade' | { kind: 'cleanup'; p: StoragePartition }>(null)
  const updInfo = checkUpd.data

  const handleCheck = () =>
    checkUpd.mutate(undefined, {
      onSuccess: (r) => {
        if (r.error) toast.error(`检查更新失败:${r.error}`)
        else if (r.has_update) toast.success(`发现新版本 ${r.latest_version}(当前 ${r.local_version})`)
        else toast.success(`已是最新版本 ${r.local_version}`)
      },
      onError: (e) => toast.error(`检查更新失败:${(e as Error).message}`),
    })

  const cleanupTarget = confirm && typeof confirm === 'object' ? confirm.p : null
  const cleanupSupportsDays = cleanupTarget ? cleanupTarget.key === 'traces' || cleanupTarget.key === 'usage_jsonl' : false

  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="flex items-center gap-2 text-base">
          <Server className="h-4 w-4" />
          聚合运维
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-4">
        {/* 重启 + OTA */}
        <div className="space-y-2">
          <div className="flex flex-wrap items-center gap-2">
            <Button variant="destructive" size="sm" disabled={restart.isPending} onClick={() => setConfirm('restart')}>
              <RotateCcw className="mr-1.5 h-4 w-4" />
              一键重启服务
            </Button>
            <Button variant="outline" size="sm" onClick={handleCheck} disabled={checkUpd.isPending || performUpd.isPending}>
              {checkUpd.isPending ? <Loader2 className="mr-1.5 h-4 w-4 animate-spin" /> : <RefreshCw className="mr-1.5 h-4 w-4" />}
              检查更新
            </Button>
            {updInfo && !updInfo.error && (
              <span className="text-xs text-muted-foreground">
                当前 <span className="font-mono">{updInfo.local_version}</span>
                {updInfo.latest_version && <> · 最新 <span className="font-mono">{updInfo.latest_version}</span></>}
              </span>
            )}
            {updInfo?.has_update && (
              <Button size="sm" disabled={performUpd.isPending} onClick={() => setConfirm('upgrade')}>
                {performUpd.isPending ? <Loader2 className="mr-1.5 h-4 w-4 animate-spin" /> : <RotateCcw className="mr-1.5 h-4 w-4" />}
                升级到 {updInfo.latest_version}
              </Button>
            )}
          </div>
          {/* OTA 升级/回滚观测(后端 .health/.bak/*.failed 标记) */}
          {updStatus && (
            <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-xs">
              {updStatus.healthConfirmed ? (
                <span className="flex items-center gap-1 text-emerald-400" title={updStatus.healthDetail ?? undefined}>
                  <CheckCircle2 className="h-3.5 w-3.5" /> 本版已稳定确认
                </span>
              ) : (
                <span className="flex items-center gap-1 text-amber-400">
                  <Clock className="h-3.5 w-3.5" /> 本版尚未确认稳定(运行一段时间后自动确认)
                </span>
              )}
              {updStatus.rollbackPointPresent && <span className="text-muted-foreground">回滚点仍在(可回退)</span>}
              {updStatus.rolledBackBinaryPresent && (
                <span className="flex items-center gap-1 text-red-400" title="守卫脚本曾执行过回滚">
                  <AlertTriangle className="h-3.5 w-3.5" /> 检测到曾发生回滚
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
              存储占用
              {storage && <span className="text-xs font-normal text-muted-foreground">落盘合计 {formatBytes(storage.totalDiskBytes)}</span>}
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
                return (
                  <div key={p.key} className="flex items-center justify-between gap-3 rounded-md border border-[#2e2e2e] bg-[#111] px-3 py-1.5">
                    <div className="flex min-w-0 items-center gap-2">
                      <span className="truncate text-xs">{p.label}</span>
                      {p.inMemory && <Badge variant="outline" className="text-[10px]">内存</Badge>}
                    </div>
                    <div className="flex shrink-0 items-center gap-3">
                      <span className="text-xs tabular-nums text-muted-foreground">{formatBytes(p.bytes)} · {p.items} 项</span>
                      {cleanable && (
                        <Button variant="outline" size="sm" className="h-7 px-2 text-xs" onClick={() => setConfirm({ kind: 'cleanup', p })}>
                          <Trash className="mr-1 h-3 w-3" />
                          清理
                        </Button>
                      )}
                    </div>
                  </div>
                )
              })}
            </div>
          ) : (
            <EmptyState icon={Database} title="暂无可统计的分区" />
          )}
        </div>
      </CardContent>

      <ConfirmDialog
        open={confirm === 'restart'}
        onOpenChange={(v) => !v && setConfirm(null)}
        title="确认重启服务？"
        description="重启会导致网关短暂断服数秒,期间所有请求失败(含正在进行的对话)。约 3 秒后自动恢复。确定继续？"
        confirmLabel="确认重启"
        destructive
        loading={restart.isPending}
        onConfirm={() =>
          restart.mutate(undefined, {
            onSuccess: (r) => {
              toast.success(r.message || '重启中,约 3 秒后自动恢复')
              setConfirm(null)
            },
            onError: () => {
              toast.warning('重启中,约 3 秒后自动恢复(本次连接已中断,属正常)')
              setConfirm(null)
            },
          })
        }
      />
      <ConfirmDialog
        open={confirm === 'upgrade'}
        onOpenChange={(v) => !v && setConfirm(null)}
        title={`确认升级到 ${updInfo?.latest_version ?? '最新版本'}？`}
        description="将从 GitHub 下载新二进制、校验 sha256 后替换并自动重启。重启期间短暂断服数秒。确定继续？"
        confirmLabel="确认升级"
        loading={performUpd.isPending}
        onConfirm={() =>
          performUpd.mutate(undefined, {
            onSuccess: (r) => {
              toast.success(r.message || '升级中,数秒后自动重启恢复')
              setConfirm(null)
            },
            onError: () => {
              toast.warning('升级已发起,若成功将自动重启(本次连接可能中断,属正常)')
              setConfirm(null)
            },
          })
        }
      />
      <ConfirmDialog
        open={!!cleanupTarget}
        onOpenChange={(v) => !v && setConfirm(null)}
        title={`清理「${cleanupTarget?.label ?? ''}」？`}
        description={
          <span>
            此操作<strong className="text-red-400">不可逆</strong>,将永久删除对应数据。
            {cleanupSupportsDays ? '按各分区配置的默认保留期清理早于该期的数据。' : '该分区为整体清理,将清空全部内容。'}
          </span>
        }
        confirmLabel="确认清理"
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
              onError: () => toast.error('清理失败'),
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
          {/* 收缩/展开切换:只折叠卡体(搜索行 + 日志区),卡头恒显 */}
          <Button
            variant="ghost"
            size="sm"
            onClick={toggleCollapsed}
            className="h-7 w-7 px-0"
            aria-expanded={!collapsed}
            title={collapsed ? '展开日志' : '收起日志'}
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
             placeholder="搜索消息或模块…"
             className="h-7 pl-7 pr-7 text-xs"
           />
           {search && (
             <button
               onClick={() => setSearch('')}
               className="absolute right-1.5 top-1/2 z-10 -translate-y-1/2 text-[#666] hover:text-[#ededed]"
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
      <CardContent>
        <div
          ref={scrollRef}
          onScroll={handleScroll}
          className="h-[420px] overflow-y-auto rounded-md border border-[#2e2e2e] bg-[#0a0a0a] p-2 font-mono text-xs leading-relaxed"
        >
          {visibleLogs.length === 0 ? (
            <EmptyState
              icon={logs.length === 0 ? Inbox : SearchX}
              title={logs.length === 0 ? '暂无日志' : '无匹配日志'}
              description={logs.length === 0 ? undefined : '调整搜索或模块过滤'}
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
