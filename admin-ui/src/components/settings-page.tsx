import { useEffect, useMemo, useState } from 'react'
import { toast } from 'sonner'
import {
  Server,
  Database,
  Trash,
  Activity,
  ChevronDown,
  ChevronUp,
  RotateCcw,
  Loader,
  AlertTriangle,
} from 'lucide-react'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogFooter,
  DialogTitle,
  DialogDescription,
} from '@/components/ui/dialog'
import { Skeleton } from '@/components/ui/skeleton'
import { StatCard } from '@/components/ui/stat-card'
import { useConfigSnapshot, useUpdateConfig } from '@/hooks/use-credentials'
import { useRestartService, useStorageStats, useCleanupStorage } from '@/hooks/use-ops'
import { useUsageClients } from '@/hooks/use-usage'
import { extractErrorMessage } from '@/lib/utils'
import { RegionSelect } from '@/components/ui/region-select'
import { NumberStepper } from '@/components/ui/number-stepper'
import type {
  ConfigSnapshotResponse,
  UpdateConfigRequest,
  StoragePartition,
  StorageCleanupTarget,
  ClientRpm,
} from '@/types/api'

// 人性化字节数：1536 → "1.5 KB"，0 → "0 B"（1024 进制）。
function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return '0 B'
  const units = ['B', 'KB', 'MB', 'GB', 'TB']
  const i = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1)
  const v = bytes / Math.pow(1024, i)
  return `${i === 0 ? String(v) : v.toFixed(1)} ${units[i]}`
}

// 可编辑表单的本地状态（字符串化便于受控输入）
interface FormState {
  host: string
  port: string
  region: string
  kiroVersion: string
  systemVersion: string
  nodeVersion: string
  tlsBackend: string
  loadBalancingMode: string
  defaultEndpoint: string
  extractThinking: boolean
  cooldownEnabled: boolean
  rateLimitEnabled: boolean
  rateLimitDailyMax: string
  rateLimitMinIntervalMs: string
  affinityEnabled: boolean
  proxyUrl: string
  callbackBaseUrl: string
  // 反代安全（批次3）：列表用换行分隔的多行文本承载
  corsAllowedOrigins: string
  ipAllowlist: string
  trustForwardedHeader: boolean
  ingressRateLimitPerMin: string
  maxBodyBytes: string
  // 主动 token 预刷新（批次4.4）
  proactiveTokenRefresh: boolean
  tokenRefreshLeadMinutes: string
  tokenRefreshIntervalSecs: string
}

// 多行文本 <-> 字符串列表（去空白、去空行）
function linesToList(s: string): string[] {
  return s
    .split('\n')
    .map((l) => l.trim())
    .filter((l) => l.length > 0)
}

function listToLines(list: string[]): string {
  return list.join('\n')
}

// 比较两个字符串列表是否等价（顺序敏感）
function sameList(a: string[], b: string[]): boolean {
  return a.length === b.length && a.every((v, i) => v === b[i])
}

function toForm(c: ConfigSnapshotResponse): FormState {
  return {
    host: c.host,
    port: String(c.port),
    region: c.region,
    kiroVersion: c.kiroVersion,
    systemVersion: c.systemVersion,
    nodeVersion: c.nodeVersion,
    tlsBackend: c.tlsBackend,
    loadBalancingMode: c.loadBalancingMode,
    defaultEndpoint: c.defaultEndpoint,
    extractThinking: c.extractThinking,
    cooldownEnabled: c.cooldownEnabled,
    rateLimitEnabled: c.rateLimitEnabled,
    rateLimitDailyMax: String(c.rateLimitDailyMax),
    rateLimitMinIntervalMs: String(c.rateLimitMinIntervalMs),
    affinityEnabled: c.affinityEnabled,
    proxyUrl: c.proxyUrl ?? '',
    callbackBaseUrl: c.callbackBaseUrl ?? '',
    corsAllowedOrigins: listToLines(c.corsAllowedOrigins ?? []),
    ipAllowlist: listToLines(c.ipAllowlist ?? []),
    trustForwardedHeader: c.trustForwardedHeader,
    ingressRateLimitPerMin: String(c.ingressRateLimitPerMin),
    maxBodyBytes: String(c.maxBodyBytes),
    proactiveTokenRefresh: c.proactiveTokenRefresh,
    tokenRefreshLeadMinutes: String(c.tokenRefreshLeadMinutes),
    tokenRefreshIntervalSecs: String(c.tokenRefreshIntervalSecs),
  }
}

// 一行可编辑/只读项布局
function Field({ label, hint, children }: { label: string; hint?: string; children: React.ReactNode }) {
  return (
    <div className="flex items-start justify-between gap-4 py-3 border-b last:border-0">
      <div className="shrink-0 min-w-[40%]">
        <div className="text-sm">{label}</div>
        {hint && <div className="text-xs text-muted-foreground mt-0.5">{hint}</div>}
      </div>
      <div className="flex-1 flex justify-end">{children}</div>
    </div>
  )
}

function ReadonlyRow({ label, value, mono }: { label: string; value: React.ReactNode; mono?: boolean }) {
  return (
    <div className="flex items-start justify-between gap-4 py-2 border-b last:border-0">
      <span className="text-sm text-muted-foreground shrink-0">{label}</span>
      <span className={`text-sm text-right break-all ${mono ? 'font-mono text-xs' : ''}`}>{value}</span>
    </div>
  )
}

/* ============ 通用二次确认弹框 ============ */
// 危险操作前的确认：标题 + 描述 + 可选额外内容（如保留天数输入），确认色可选危险红。
function ConfirmDialog({
  open,
  onOpenChange,
  title,
  description,
  confirmLabel = '确定',
  destructive = false,
  loading = false,
  onConfirm,
  children,
}: {
  open: boolean
  onOpenChange: (v: boolean) => void
  title: string
  description: React.ReactNode
  confirmLabel?: string
  destructive?: boolean
  loading?: boolean
  onConfirm: () => void
  children?: React.ReactNode
}) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-md">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            {destructive && <AlertTriangle className="h-4 w-4 text-red-400" />}
            {title}
          </DialogTitle>
          <DialogDescription>{description}</DialogDescription>
        </DialogHeader>
        {children}
        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)} disabled={loading}>
            取消
          </Button>
          <Button
            variant={destructive ? 'destructive' : 'default'}
            onClick={onConfirm}
            disabled={loading}
          >
            {loading ? '处理中…' : confirmLabel}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

/* ============ 1. 服务管理：一键重启 ============ */
function ServiceManagementCard() {
  const [confirmOpen, setConfirmOpen] = useState(false)
  const { mutate: restart, isPending } = useRestartService()

  const handleConfirm = () => {
    restart(undefined, {
      // 重启会掐断本次连接，成功/失败都当作"已发起"提示——真正结果看服务是否恢复。
      onSuccess: (resp) => {
        toast.success(resp.message || '重启中，数秒后恢复')
        setConfirmOpen(false)
      },
      onError: () => {
        // 连接被重启中断而抛错属预期，仍提示已发起
        toast.warning('重启中，数秒后恢复（本次连接已中断，属正常）')
        setConfirmOpen(false)
      },
    })
  }

  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="text-base flex items-center gap-2">
          <Server className="h-4 w-4 text-muted-foreground" />
          服务管理
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-3">
        <p className="text-sm text-muted-foreground">
          一键重启网关服务。重启瞬间会短暂断服数秒，期间请求失败——Claude Code 自身也走本网关，务必确认无进行中的关键请求。
        </p>
        <Button variant="destructive" size="sm" onClick={() => setConfirmOpen(true)} disabled={isPending}>
          <RotateCcw className="mr-1.5 h-4 w-4" />
          一键重启服务
        </Button>
      </CardContent>

      <ConfirmDialog
        open={confirmOpen}
        onOpenChange={setConfirmOpen}
        title="确认重启服务？"
        description="重启会导致网关短暂断服数秒，期间所有请求失败（含正在进行的对话）。数秒后自动恢复。确定继续？"
        confirmLabel="确认重启"
        destructive
        loading={isPending}
        onConfirm={handleConfirm}
      />
    </Card>
  )
}

/* ============ 2. 分区存储统计 + 清理 ============ */
// 分区键 → 是否需要保留天数输入（bg_cache/trash 是全清，无时间维度更自然，但后端也接受 olderThanDays）
const CLEANABLE_KEYS: StorageCleanupTarget[] = ['traces', 'usage_jsonl', 'trash', 'bg_cache']

function StoragePartitionRow({
  p,
  onCleanup,
}: {
  p: StoragePartition
  onCleanup: (p: StoragePartition) => void
}) {
  const cleanable = (CLEANABLE_KEYS as string[]).includes(p.key)
  return (
    <div className="flex items-center justify-between gap-4 border-b border-border/40 py-3 last:border-0">
      <div className="min-w-0">
        <div className="flex items-center gap-2 text-sm">
          <span className="truncate">{p.label}</span>
          {p.inMemory && (
            <Badge variant="outline" className="text-[10px]">
              内存
            </Badge>
          )}
        </div>
        {p.path && (
          <div className="mt-0.5 truncate font-mono text-[11px] text-muted-foreground" title={p.path}>
            {p.path}
          </div>
        )}
      </div>
      <div className="flex shrink-0 items-center gap-4">
        <div className="text-right">
          <div className="text-sm font-semibold tabular-nums">{formatBytes(p.bytes)}</div>
          <div className="text-[11px] text-muted-foreground tabular-nums">{p.items} 项</div>
        </div>
        {cleanable && (
          <Button variant="outline" size="sm" onClick={() => onCleanup(p)}>
            <Trash className="mr-1 h-3.5 w-3.5" />
            清理
          </Button>
        )}
      </div>
    </div>
  )
}

function StorageStatsCard() {
  const { data, isLoading, error, refetch } = useStorageStats()
  const { mutate: cleanup, isPending } = useCleanupStorage()

  // 清理弹框状态：target 分区 + 可选保留天数（空=按配置默认保留期）
  const [target, setTarget] = useState<StoragePartition | null>(null)
  const [keepDays, setKeepDays] = useState<string>('')

  const openCleanup = (p: StoragePartition) => {
    setTarget(p)
    setKeepDays('')
  }

  // 时间维度仅对落盘按天数据有意义（traces / usage_jsonl）；trash/bg_cache 为全清
  const supportsDays = target ? target.key === 'traces' || target.key === 'usage_jsonl' : false

  const handleConfirm = () => {
    if (!target) return
    const days = keepDays.trim() === '' ? undefined : Number(keepDays)
    cleanup(
      {
        target: target.key as StorageCleanupTarget,
        olderThanDays: supportsDays && Number.isFinite(days) ? days : undefined,
      },
      {
        onSuccess: (resp) => {
          toast.success(resp.message)
          setTarget(null)
        },
        onError: (err) => toast.error(extractErrorMessage(err)),
      }
    )
  }

  return (
    <Card>
      <CardHeader className="pb-2 flex-row items-center justify-between space-y-0">
        <CardTitle className="text-base flex items-center gap-2">
          <Database className="h-4 w-4 text-muted-foreground" />
          存储占用
        </CardTitle>
        <Button variant="outline" size="sm" onClick={() => refetch()} disabled={isLoading}>
          刷新
        </Button>
      </CardHeader>
      <CardContent className="space-y-4">
        {isLoading ? (
          <div className="space-y-3">
            <Skeleton className="h-14 w-full" />
            <Skeleton className="h-14 w-full" />
            <Skeleton className="h-14 w-full" />
          </div>
        ) : error ? (
          <div className="py-4 text-center text-sm text-red-400">
            加载存储统计失败：{extractErrorMessage(error)}
          </div>
        ) : data ? (
          <>
            <StatCard
              label="落盘占用合计"
              value={formatBytes(data.totalDiskBytes)}
              icon={Database}
              accent="primary"
              hint={data.usageEnabled ? '用量统计已启用' : '用量统计未启用（部分分区缺失）'}
            />
            <div>
              {data.partitions.map((p) => (
                <StoragePartitionRow key={p.key} p={p} onCleanup={openCleanup} />
              ))}
              {data.partitions.length === 0 && (
                <p className="py-4 text-center text-sm text-muted-foreground">暂无可统计的分区</p>
              )}
            </div>
          </>
        ) : null}
      </CardContent>

      <ConfirmDialog
        open={target !== null}
        onOpenChange={(v) => !v && setTarget(null)}
        title={`清理「${target?.label ?? ''}」？`}
        description={
          <span>
            此操作<strong className="text-red-400">不可逆</strong>，将永久删除对应数据。
            {supportsDays
              ? '可指定保留天数：仅删除早于该天数的数据；留空按服务配置的默认保留期。'
              : '该分区为整体清理，将清空全部内容。'}
          </span>
        }
        confirmLabel="确认清理"
        destructive
        loading={isPending}
        onConfirm={handleConfirm}
      >
        {supportsDays && (
          <div className="flex items-center justify-between gap-3 rounded-md border border-border/60 bg-secondary/30 px-3 py-2">
            <span className="text-sm">保留天数</span>
            <div className="flex items-center gap-2">
              <Input
                className="w-24 text-right"
                type="number"
                min={0}
                value={keepDays}
                onChange={(e) => setKeepDays(e.target.value)}
                placeholder="默认"
              />
              <span className="text-xs text-muted-foreground">天</span>
            </div>
          </div>
        )}
      </ConfirmDialog>
    </Card>
  )
}

/* ============ 3. per 客户端/窗口 RPM 面板 ============ */
function ClientRow({ c }: { c: ClientRpm }) {
  const [expanded, setExpanded] = useState(false)
  const canExpand = c.sessions.length > 0
  return (
    <div className="border-b border-border/40 last:border-0">
      <div
        className={`flex items-center justify-between gap-4 py-3 ${canExpand ? 'cursor-pointer hover:bg-secondary/40' : ''} rounded-md px-2 transition-colors`}
        onClick={() => canExpand && setExpanded((v) => !v)}
      >
        <div className="flex min-w-0 items-center gap-2">
          {canExpand ? (
            expanded ? (
              <ChevronUp className="h-4 w-4 shrink-0 text-muted-foreground" />
            ) : (
              <ChevronDown className="h-4 w-4 shrink-0 text-muted-foreground" />
            )
          ) : (
            <span className="w-4 shrink-0" />
          )}
          <div className="min-w-0">
            <div className="truncate font-mono text-sm" title={c.clientKey}>
              {c.clientIp ?? c.clientKey}
            </div>
            {c.device && (
              <div className="mt-0.5 text-[11px] text-muted-foreground">{c.device}</div>
            )}
          </div>
        </div>
        <div className="flex shrink-0 items-center gap-5">
          <div className="text-right">
            <div className="text-sm font-semibold tabular-nums">{c.rpm}</div>
            <div className="text-[11px] text-muted-foreground">RPM</div>
          </div>
          <Badge variant="outline" className="text-[11px]">
            {c.activeSessions} 窗口
          </Badge>
        </div>
      </div>
      {expanded && canExpand && (
        <div className="mb-2 ml-6 space-y-1 rounded-md border border-border/40 bg-secondary/20 p-2">
          {c.sessions.map((s) => (
            <div key={s.sessionId} className="flex items-center justify-between gap-3 text-xs">
              <span className="truncate font-mono text-muted-foreground" title={s.sessionId}>
                {s.sessionId}
              </span>
              <span className="shrink-0 tabular-nums">
                <span className="font-semibold">{s.rpm}</span>
                <span className="text-muted-foreground"> RPM</span>
              </span>
            </div>
          ))}
        </div>
      )}
    </div>
  )
}

function ClientRpmCard() {
  const { data, isLoading, error, isFetching } = useUsageClients()

  const totalRpm = useMemo(() => (data ?? []).reduce((s, c) => s + c.rpm, 0), [data])
  const totalWindows = useMemo(
    () => (data ?? []).reduce((s, c) => s + c.activeSessions, 0),
    [data]
  )

  return (
    <Card>
      <CardHeader className="pb-2 flex-row items-center justify-between space-y-0">
        <CardTitle className="text-base flex items-center gap-2">
          <Activity className="h-4 w-4 text-muted-foreground" />
          客户端 RPM
          {isFetching && <Loader className="h-3.5 w-3.5 animate-spin text-muted-foreground" />}
        </CardTitle>
        <span className="text-xs text-muted-foreground">每 30 秒刷新（读本地统计）</span>
      </CardHeader>
      <CardContent className="space-y-4">
        <div className="grid grid-cols-3 gap-3">
          <StatCard label="客户端数" value={data?.length ?? 0} accent="neutral" />
          <StatCard label="合计 RPM" value={totalRpm} accent="primary" />
          <StatCard label="活跃窗口" value={totalWindows} accent="success" />
        </div>

        {isLoading ? (
          <div className="space-y-3">
            <Skeleton className="h-12 w-full" />
            <Skeleton className="h-12 w-full" />
            <Skeleton className="h-12 w-full" />
          </div>
        ) : error ? (
          <div className="py-4 text-center text-sm text-red-400">
            加载客户端 RPM 失败：{extractErrorMessage(error)}
          </div>
        ) : data && data.length > 0 ? (
          <div>
            <p className="mb-1 px-2 text-xs text-muted-foreground">
              点击展开查看每个窗口（session）的 RPM
            </p>
            {data.map((c) => (
              <ClientRow key={c.clientKey} c={c} />
            ))}
          </div>
        ) : (
          <p className="py-6 text-center text-sm text-muted-foreground">当前无活跃客户端</p>
        )}
      </CardContent>
    </Card>
  )
}

export function SettingsPage() {
  const { data: config, isLoading, error, refetch } = useConfigSnapshot()
  const { mutate: save, isPending: isSaving } = useUpdateConfig()

  const [form, setForm] = useState<FormState | null>(null)

  // 配置加载/刷新后，重置表单基线
  useEffect(() => {
    if (config) setForm(toForm(config))
  }, [config])

  const set = <K extends keyof FormState>(key: K, value: FormState[K]) =>
    setForm((prev) => (prev ? { ...prev, [key]: value } : prev))

  // 计算与基线的差异，只提交改动的字段
  const diff = useMemo<UpdateConfigRequest>(() => {
    if (!config || !form) return {}
    const d: UpdateConfigRequest = {}
    if (form.host.trim() !== config.host) d.host = form.host.trim()
    const port = Number(form.port)
    if (Number.isFinite(port) && port !== config.port) d.port = port
    if (form.region.trim() !== config.region) d.region = form.region.trim()
    if (form.kiroVersion.trim() !== config.kiroVersion) d.kiroVersion = form.kiroVersion.trim()
    if (form.systemVersion.trim() !== config.systemVersion) d.systemVersion = form.systemVersion.trim()
    if (form.nodeVersion.trim() !== config.nodeVersion) d.nodeVersion = form.nodeVersion.trim()
    if (form.tlsBackend !== config.tlsBackend) d.tlsBackend = form.tlsBackend
    if (form.loadBalancingMode !== config.loadBalancingMode) d.loadBalancingMode = form.loadBalancingMode
    if (form.defaultEndpoint.trim() !== config.defaultEndpoint) d.defaultEndpoint = form.defaultEndpoint.trim()
    if (form.extractThinking !== config.extractThinking) d.extractThinking = form.extractThinking
    if (form.cooldownEnabled !== config.cooldownEnabled) d.cooldownEnabled = form.cooldownEnabled
    if (form.rateLimitEnabled !== config.rateLimitEnabled) d.rateLimitEnabled = form.rateLimitEnabled
    const daily = Number(form.rateLimitDailyMax)
    if (Number.isFinite(daily) && daily !== config.rateLimitDailyMax) d.rateLimitDailyMax = daily
    const interval = Number(form.rateLimitMinIntervalMs)
    if (Number.isFinite(interval) && interval !== config.rateLimitMinIntervalMs) d.rateLimitMinIntervalMs = interval
    if (form.affinityEnabled !== config.affinityEnabled) d.affinityEnabled = form.affinityEnabled
    if (form.proxyUrl.trim() !== (config.proxyUrl ?? '')) d.proxyUrl = form.proxyUrl.trim()
    if (form.callbackBaseUrl.trim() !== (config.callbackBaseUrl ?? '')) d.callbackBaseUrl = form.callbackBaseUrl.trim()
    // 反代安全
    const origins = linesToList(form.corsAllowedOrigins)
    if (!sameList(origins, config.corsAllowedOrigins ?? [])) d.corsAllowedOrigins = origins
    const allowlist = linesToList(form.ipAllowlist)
    if (!sameList(allowlist, config.ipAllowlist ?? [])) d.ipAllowlist = allowlist
    if (form.trustForwardedHeader !== config.trustForwardedHeader) d.trustForwardedHeader = form.trustForwardedHeader
    const ingress = Number(form.ingressRateLimitPerMin)
    if (Number.isFinite(ingress) && ingress !== config.ingressRateLimitPerMin) d.ingressRateLimitPerMin = ingress
    const maxBody = Number(form.maxBodyBytes)
    if (Number.isFinite(maxBody) && maxBody !== config.maxBodyBytes) d.maxBodyBytes = maxBody
    // 主动 token 预刷新
    if (form.proactiveTokenRefresh !== config.proactiveTokenRefresh) d.proactiveTokenRefresh = form.proactiveTokenRefresh
    const lead = Number(form.tokenRefreshLeadMinutes)
    if (Number.isFinite(lead) && lead !== config.tokenRefreshLeadMinutes) d.tokenRefreshLeadMinutes = lead
    const interval2 = Number(form.tokenRefreshIntervalSecs)
    if (Number.isFinite(interval2) && interval2 !== config.tokenRefreshIntervalSecs) d.tokenRefreshIntervalSecs = interval2
    return d
  }, [config, form])

  const dirty = Object.keys(diff).length > 0

  const handleSave = () => {
    if (!dirty) return
    save(diff, {
      onSuccess: (resp) => {
        if (resp.restartRequired) {
          toast.warning(resp.message, {
            description: `需重启字段：${resp.restartFields.join('、')}`,
            duration: 8000,
          })
        } else {
          toast.success(resp.message)
        }
        refetch()
      },
      onError: (err) => toast.error(extractErrorMessage(err)),
    })
  }

  const handleReset = () => {
    if (config) setForm(toForm(config))
  }

  if (isLoading || !form) {
    return (
      <div className="flex items-center justify-center py-24">
        <div className="animate-spin rounded-full h-10 w-10 border-b-2 border-primary" />
      </div>
    )
  }

  if (error || !config) {
    return (
      <div className="flex items-center justify-center py-24">
        <Card className="w-full max-w-md">
          <CardContent className="pt-6 text-center">
            <div className="text-red-500 mb-4">加载配置失败</div>
            <p className="text-muted-foreground mb-4">{error ? (error as Error).message : '无数据'}</p>
            <Button onClick={() => refetch()}>重试</Button>
          </CardContent>
        </Card>
      </div>
    )
  }

  const inputCls = 'max-w-[260px] text-right'

  return (
    <div className="space-y-6 pb-24">
      <div className="flex items-center justify-between">
        <h2 className="text-xl font-semibold text-gradient-brand">设置</h2>
        <Button variant="outline" size="sm" onClick={() => refetch()} disabled={isSaving}>
          刷新
        </Button>
      </div>

      {/* 运维：服务管理 / 存储 / 客户端 RPM（对接已就绪后端端点） */}
      <ServiceManagementCard />
      <StorageStatsCard />
      <ClientRpmCard />

      {/* 负载均衡（立即生效） */}
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base">负载均衡模式</CardTitle>
        </CardHeader>
        <CardContent className="space-y-3">
          <p className="text-sm text-muted-foreground">
            优先级模式：按 priority 顺序使用凭据；均衡负载：在可用凭据间轮换分摊请求。此项保存后立即生效。
          </p>
          <div className="flex gap-2">
            <Button
              variant={form.loadBalancingMode === 'priority' ? 'default' : 'outline'}
              size="sm"
              onClick={() => set('loadBalancingMode', 'priority')}
            >
              优先级模式
            </Button>
            <Button
              variant={form.loadBalancingMode === 'balanced' ? 'default' : 'outline'}
              size="sm"
              onClick={() => set('loadBalancingMode', 'balanced')}
            >
              均衡负载
            </Button>
          </div>
        </CardContent>
      </Card>

      {/* 服务信息（需重启） */}
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base">服务信息</CardTitle>
        </CardHeader>
        <CardContent className="py-0">
          <Field label="监听地址 host" hint="需重启生效">
            <Input className={inputCls} value={form.host} onChange={(e) => set('host', e.target.value)} />
          </Field>
          <Field label="端口 port" hint="需重启生效">
            <NumberStepper value={Number(form.port) || 0} onChange={(v) => set('port', String(v))} min={1} max={65535} className="w-28" aria-label="端口" />
          </Field>
          <Field label="区域 region" hint="需重启生效">
            <div className="w-[260px]">
              <RegionSelect value={form.region} onChange={(v) => set('region', v)} />
            </div>
          </Field>
          <Field label="TLS 后端" hint="需重启生效">
            <div className="flex gap-2">
              <Button variant={form.tlsBackend === 'rustls' ? 'default' : 'outline'} size="sm" onClick={() => set('tlsBackend', 'rustls')}>
                rustls
              </Button>
              <Button variant={form.tlsBackend === 'native-tls' ? 'default' : 'outline'} size="sm" onClick={() => set('tlsBackend', 'native-tls')}>
                native-tls
              </Button>
            </div>
          </Field>
          <Field label="默认 endpoint" hint={`可用：${config.endpointNames.join(', ') || '—'}（需重启生效）`}>
            <Input className={inputCls} value={form.defaultEndpoint} onChange={(e) => set('defaultEndpoint', e.target.value)} />
          </Field>
          {config.configPath && <ReadonlyRow label="配置文件" value={config.configPath} mono />}
        </CardContent>
      </Card>

      {/* 客户端伪装（需重启） */}
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base">客户端伪装</CardTitle>
        </CardHeader>
        <CardContent className="py-0">
          <Field label="Kiro 版本" hint="需重启生效">
            <Input className={inputCls} value={form.kiroVersion} onChange={(e) => set('kiroVersion', e.target.value)} />
          </Field>
          <Field label="系统版本" hint="需重启生效">
            <Input className={inputCls} value={form.systemVersion} onChange={(e) => set('systemVersion', e.target.value)} />
          </Field>
          <Field label="Node 版本" hint="需重启生效">
            <Input className={inputCls} value={form.nodeVersion} onChange={(e) => set('nodeVersion', e.target.value)} />
          </Field>
          <Field label="提取 thinking" hint="非流式响应解析 thinking 块（需重启生效）">
            <Switch checked={form.extractThinking} onCheckedChange={(v) => set('extractThinking', v)} />
          </Field>
        </CardContent>
      </Card>

      {/* 防关联 / 限流（需重启） */}
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base">防关联 / 限流</CardTitle>
        </CardHeader>
        <CardContent className="py-0">
          <Field label="冷却机制" hint="失败后短暂跳过该凭据（需重启生效）">
            <Switch checked={form.cooldownEnabled} onCheckedChange={(v) => set('cooldownEnabled', v)} />
          </Field>
          <Field label="速率限制" hint="拟人节奏：每日上限 + 请求间隔（需重启生效）">
            <Switch checked={form.rateLimitEnabled} onCheckedChange={(v) => set('rateLimitEnabled', v)} />
          </Field>
          <Field label="每日上限" hint="0 表示无限制（需重启生效）">
            <NumberStepper value={Number(form.rateLimitDailyMax) || 0} onChange={(v) => set('rateLimitDailyMax', String(v))} min={0} step={10} className="w-28" disabled={!form.rateLimitEnabled} aria-label="每日上限" />
          </Field>
          <Field label="最小请求间隔 (ms)" hint="需重启生效">
            <NumberStepper value={Number(form.rateLimitMinIntervalMs) || 0} onChange={(v) => set('rateLimitMinIntervalMs', String(v))} min={0} step={100} className="w-28" disabled={!form.rateLimitEnabled} aria-label="最小请求间隔" />
          </Field>
          <Field label="会话亲和性" hint="同一会话尽量复用同一凭据（需重启生效）">
            <Switch checked={form.affinityEnabled} onCheckedChange={(v) => set('affinityEnabled', v)} />
          </Field>
        </CardContent>
      </Card>

      {/* 网络与上号（需重启） */}
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base">网络与上号</CardTitle>
        </CardHeader>
        <CardContent className="py-0">
          <Field label="全局代理" hint="http(s)://host:port 或 socks5://host:port，留空清除（需重启生效）">
            <Input className="max-w-[260px] font-mono text-xs" value={form.proxyUrl} onChange={(e) => set('proxyUrl', e.target.value)} placeholder="未配置" />
          </Field>
          <Field
            label="上号回调地址"
            hint="远程模式：浏览器回调打到此地址。服务器部署必须配置，否则远程浏览器上号失败。留空回退本地模式（需重启生效）"
          >
            <Input className="max-w-[260px] font-mono text-xs" value={form.callbackBaseUrl} onChange={(e) => set('callbackBaseUrl', e.target.value)} placeholder="http://host:port" />
          </Field>
          <ReadonlyRow
            label="当前回调模式"
            value={
              <Badge variant="outline">
                {config.callbackMode === 'remote' ? '远程（公网回调）' : '本地（临时端口）'}
              </Badge>
            }
          />
          <ReadonlyRow label="Admin Key" value={<Badge variant={config.hasAdminKey ? 'default' : 'secondary'}>{config.hasAdminKey ? '已设置' : '未设置'}</Badge>} />
        </CardContent>
      </Card>

      {/* 反代安全（需重启） */}
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base">反代安全</CardTitle>
        </CardHeader>
        <CardContent className="py-0">
          <Field
            label="CORS 允许来源"
            hint="每行一个来源，如 https://app.example.com。留空=允许任意来源（公开 API，需重启生效）"
          >
            <textarea
              className="flex min-h-[72px] w-full max-w-[260px] rounded-md border border-input bg-background px-3 py-2 font-mono text-xs ring-offset-background placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
              value={form.corsAllowedOrigins}
              onChange={(e) => set('corsAllowedOrigins', e.target.value)}
              placeholder="留空=允许任意来源"
              spellCheck={false}
            />
          </Field>
          <Field
            label="IP 白名单"
            hint="每行一条 CIDR 或单 IP，如 10.0.0.0/8、127.0.0.1。留空=不限制。非法条目保存时会被拒绝（需重启生效）"
          >
            <textarea
              className="flex min-h-[72px] w-full max-w-[260px] rounded-md border border-input bg-background px-3 py-2 font-mono text-xs ring-offset-background placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
              value={form.ipAllowlist}
              onChange={(e) => set('ipAllowlist', e.target.value)}
              placeholder="留空=不限制"
              spellCheck={false}
            />
          </Field>
          <Field
            label="信任 X-Forwarded-For"
            hint="仅当部署在可信反代（nginx 等）之后才开启，否则客户端可伪造 IP 绕过白名单（需重启生效）"
          >
            <Switch checked={form.trustForwardedHeader} onCheckedChange={(v) => set('trustForwardedHeader', v)} />
          </Field>
          <Field label="入口限流 (次/分钟/IP)" hint="0 表示关闭。超限返回 429（需重启生效）">
            <NumberStepper value={Number(form.ingressRateLimitPerMin) || 0} onChange={(v) => set('ingressRateLimitPerMin', String(v))} min={0} step={10} className="w-28" aria-label="入口限流" />
          </Field>
          <Field label="请求体上限 (字节)" hint="默认 52428800（50MiB）。超限返回 413（需重启生效）">
            <Input className={inputCls} type="number" value={form.maxBodyBytes} onChange={(e) => set('maxBodyBytes', e.target.value)} />
          </Field>
        </CardContent>
      </Card>

      {/* 主动 token 预刷新（需重启） */}
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base">主动 token 预刷新</CardTitle>
        </CardHeader>
        <CardContent className="py-0">
          <Field label="启用预刷新" hint="后台提前刷新将过期的 token，把刷新移出请求热路径、削掉突发（需重启生效）">
            <Switch checked={form.proactiveTokenRefresh} onCheckedChange={(v) => set('proactiveTokenRefresh', v)} />
          </Field>
          <Field label="提前量 (分钟)" hint="token 剩余有效期低于此值即后台刷新（需重启生效）">
            <NumberStepper value={Number(form.tokenRefreshLeadMinutes) || 0} onChange={(v) => set('tokenRefreshLeadMinutes', String(v))} min={0} className="w-28" disabled={!form.proactiveTokenRefresh} aria-label="提前量分钟" />
          </Field>
          <Field label="扫描间隔 (秒)" hint="后台扫描周期，最小 5 秒（需重启生效）">
            <NumberStepper value={Number(form.tokenRefreshIntervalSecs) || 0} onChange={(v) => set('tokenRefreshIntervalSecs', String(v))} min={5} step={5} className="w-28" disabled={!form.proactiveTokenRefresh} aria-label="扫描间隔秒" />
          </Field>
        </CardContent>
      </Card>

      <p className="text-xs text-muted-foreground">
        除负载均衡模式立即生效外，其余字段保存后需重启服务才生效。敏感字段（API/Admin 密钥、代理密码）出于安全不在此显示与修改，请在配置文件中维护。
      </p>

      {/* 底部保存栏：仅覆盖 main 内容区（left-[240px] 避开 240px 侧栏，
          否则会盖住侧栏底部“网关在线”状态条造成重叠）；z-30 低于侧栏 z-40。 */}
      <div className="fixed bottom-0 left-0 right-0 z-30 border-t bg-background/95 px-6 py-3 backdrop-blur md:left-[240px]">
        <div className="mx-auto flex max-w-[1200px] items-center justify-end gap-3">
          <span className="mr-auto text-sm text-muted-foreground">
            {dirty ? `${Object.keys(diff).length} 项改动待保存` : '无改动'}
          </span>
          <Button variant="outline" onClick={handleReset} disabled={!dirty || isSaving}>
            撤销
          </Button>
          <Button onClick={handleSave} disabled={!dirty || isSaving}>
            {isSaving ? '保存中…' : '保存'}
          </Button>
        </div>
      </div>
    </div>
  )
}
