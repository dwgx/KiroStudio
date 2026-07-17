import { createContext, useContext, useEffect, useMemo, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { useQuery, useQueryClient } from '@tanstack/react-query'
import { toast } from 'sonner'
import {
  Server,
  Database,
  Trash,
  Trash2,
  Activity,
  ChevronDown,
  ChevronUp,
  RotateCcw,
  RefreshCw,
  Loader,
  Loader2,
  Search,
  Fingerprint,
  ShieldCheck,
  SlidersHorizontal,
  Download,
  FileJson,
  KeyRound,
  ClipboardCopy,
  Image as ImageIcon,
  LayoutGrid,
  X,
} from 'lucide-react'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import { ProxyTestButton } from '@/components/proxy-test-button'
import { ConfirmDialog } from '@/components/ui/confirm-dialog'
import { Skeleton } from '@/components/ui/skeleton'
import { PageSkeleton } from '@/components/ui/page-skeleton'
import { Checkbox } from '@/components/ui/checkbox'
import { StatCard } from '@/components/ui/stat-card'
import { useConfigSnapshot, useUpdateConfig, useCredentials } from '@/hooks/use-credentials'
import { useRestartService, useStorageStats, useCleanupStorage, useCheckUpdate, usePerformUpdate, useUpdateStatus } from '@/hooks/use-ops'
import { useUsageClients } from '@/hooks/use-usage'
import { useUiLayoutPrefs, type PoolSortMode, type CardSize, type UiLayoutPrefs } from '@/hooks/use-ui-layout-prefs'
import {
  exportCredential,
  listTrash,
  restoreCredential,
  purgeCredential,
  purgeTrashBatch,
} from '@/api/credentials'
import { extractErrorMessage, copyToClipboard } from '@/lib/utils'
import { RegionSelect } from '@/components/ui/region-select'
import { NumberStepper } from '@/components/ui/number-stepper'
import { ComboInput } from '@/components/ui/combo-input'
import { SettingGearCard } from '@/components/setting-gear-card'
import {
  TraceDetailDialog,
  UsageDetailDialog,
  TrashDetailDialog,
  BgCacheDetailDialog,
} from '@/components/ops-detail-dialogs'
import { Eye } from 'lucide-react'

// 版本字段的常见预设（可点选，也可自定义输入）。与 Kiro IDE 实际发行的标识对齐，
// 便于伪装成主流客户端指纹；不在列表里的值直接手敲即可。
const KIRO_VERSION_PRESETS = ['0.3.16', '0.3.15', '0.3.14', '0.3.13', '0.2.28', '0.1.25']
const SYSTEM_VERSION_PRESETS = ['win32', 'darwin', 'linux', '10.0.22631', '10.0.19045', '14.5', '13.6']
const NODE_VERSION_PRESETS = ['20.11.1', '20.18.1', '18.20.4', '22.11.0', '18.18.2']
import type {
  ConfigSnapshotResponse,
  UpdateConfigRequest,
  StoragePartition,
  StorageCleanupTarget,
  ClientRpm,
  TrashItem,
} from '@/types/api'
import type { TFunction } from 'i18next'

// 人性化字节数：1536 → "1.5 KB"，0 → "0 B"（1024 进制）。
function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return '0 B'
  const units = ['B', 'KB', 'MB', 'GB', 'TB']
  const i = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1)
  const v = bytes / Math.pow(1024, i)
  return `${i === 0 ? String(v) : v.toFixed(1)} ${units[i]}`
}

// 触发浏览器下载一段 JSON（Blob + a.download），用于令牌导出。
function downloadJson(filename: string, data: unknown) {
  const blob = new Blob([JSON.stringify(data, null, 2)], { type: 'application/json' })
  const url = URL.createObjectURL(blob)
  const a = document.createElement('a')
  a.href = url
  a.download = filename
  document.body.appendChild(a)
  a.click()
  a.remove()
  URL.revokeObjectURL(url)
}

// 友好相对时间："X 秒/分钟/小时/天 前"。无法解析时回退原字符串。
function timeAgo(iso: string | null | undefined, t: TFunction): string {
  if (!iso) return t('settingspage.common.emDash')
  const ts = new Date(iso).getTime()
  if (!Number.isFinite(ts)) return iso
  const diff = Date.now() - ts
  if (diff < 0) return t('settingspage.time.justNow')
  const sec = Math.floor(diff / 1000)
  if (sec < 60) return t('settingspage.time.secondsAgo', { n: sec })
  const min = Math.floor(sec / 60)
  if (min < 60) return t('settingspage.time.minutesAgo', { n: min })
  const hour = Math.floor(min / 60)
  if (hour < 24) return t('settingspage.time.hoursAgo', { n: hour })
  const day = Math.floor(hour / 24)
  if (day < 30) return t('settingspage.time.daysAgo', { n: day })
  const month = Math.floor(day / 30)
  if (month < 12) return t('settingspage.time.monthsAgo', { n: month })
  return t('settingspage.time.yearsAgo', { n: Math.floor(month / 12) })
}

/* ============ 分区导航 + 搜索 基础设施 ============ */

// 设置分区（顶部 tab）。id 用于 tab 切换与卡片归属；label 经 i18n 在渲染时解析。
type SectionId = 'basic' | 'security' | 'scheduling' | 'storage' | 'service' | 'privacy' | 'appearance' | 'export' | 'trash'

const SECTION_DEFS: { id: SectionId; labelKey: string; icon: React.ComponentType<{ className?: string }> }[] = [
  { id: 'basic', labelKey: 'settingspage.section.basic', icon: SlidersHorizontal },
  { id: 'security', labelKey: 'settingspage.section.security', icon: ShieldCheck },
  { id: 'scheduling', labelKey: 'settingspage.section.scheduling', icon: Activity },
  { id: 'storage', labelKey: 'settingspage.section.storage', icon: Database },
  { id: 'service', labelKey: 'settingspage.section.service', icon: Server },
  { id: 'privacy', labelKey: 'settingspage.section.privacy', icon: Fingerprint },
  { id: 'appearance', labelKey: 'settingspage.section.appearance', icon: LayoutGrid },
  { id: 'export', labelKey: 'settingspage.section.export', icon: Download },
  { id: 'trash', labelKey: 'settingspage.section.trash', icon: Trash2 },
]

// 每张卡片的可搜索索引（titleKey + kwKey）。kw 为各语言逗号分隔同义词，运行时 split。
// 含智能调度（原先仅 SectionGate 有、CARD_INDEX 缺，导致命中计数不准）。
const CARD_INDEX_DEFS: { section: SectionId; titleKey: string; kwKey: string }[] = [
  { section: 'basic', titleKey: 'settingspage.card.serviceInfo', kwKey: 'settingspage.card.serviceInfo.kw' },
  { section: 'basic', titleKey: 'settingspage.card.clientSpoof', kwKey: 'settingspage.card.clientSpoof.kw' },
  { section: 'basic', titleKey: 'settingspage.card.protocol', kwKey: 'settingspage.card.protocol.kw' },
  { section: 'basic', titleKey: 'settingspage.card.toolFault', kwKey: 'settingspage.card.toolFault.kw' },
  { section: 'basic', titleKey: 'settingspage.card.network', kwKey: 'settingspage.card.network.kw' },
  { section: 'basic', titleKey: 'settingspage.card.loginBg', kwKey: 'settingspage.card.loginBg.kw' },
  { section: 'security', titleKey: 'settingspage.card.security', kwKey: 'settingspage.card.security.kw' },
  { section: 'scheduling', titleKey: 'settingspage.card.loadBalance', kwKey: 'settingspage.card.loadBalance.kw' },
  { section: 'scheduling', titleKey: 'settingspage.card.antiAssoc', kwKey: 'settingspage.card.antiAssoc.kw' },
  { section: 'scheduling', titleKey: 'settingspage.card.smartSchedule', kwKey: 'settingspage.card.smartSchedule.kw' },
  { section: 'scheduling', titleKey: 'settingspage.card.tokenRefresh', kwKey: 'settingspage.card.tokenRefresh.kw' },
  { section: 'storage', titleKey: 'settingspage.card.storage', kwKey: 'settingspage.card.storage.kw' },
  { section: 'service', titleKey: 'settingspage.card.service', kwKey: 'settingspage.card.service.kw' },
  { section: 'service', titleKey: 'settingspage.card.clientRpm', kwKey: 'settingspage.card.clientRpm.kw' },
  { section: 'privacy', titleKey: 'settingspage.card.privacy', kwKey: 'settingspage.card.privacy.kw' },
  { section: 'appearance', titleKey: 'settingspage.card.appearance', kwKey: 'settingspage.card.appearance.kw' },
  { section: 'export', titleKey: 'settingspage.card.export', kwKey: 'settingspage.card.export.kw' },
  { section: 'trash', titleKey: 'settingspage.card.trash', kwKey: 'settingspage.card.trash.kw' },
]

function parseKeywords(raw: string): string[] {
  return raw
    .split(',')
    .map((s) => s.trim().toLowerCase())
    .filter(Boolean)
}

// 搜索上下文：query 为小写去空白后的关键词，'' 表示未搜索。
const SearchContext = createContext<{ query: string }>({ query: '' })
// 当前激活的分区 tab（未搜索时按此过滤卡片显示）。
const ActiveSectionContext = createContext<SectionId>('basic')

// 高亮命中片段（大小写不敏感，只高亮首个命中）。
function Highlight({ text }: { text: string }) {
  const { query } = useContext(SearchContext)
  if (!query) return <>{text}</>
  const idx = text.toLowerCase().indexOf(query)
  if (idx === -1) return <>{text}</>
  return (
    <>
      {text.slice(0, idx)}
      <mark className="rounded bg-yellow-400/30 px-0.5 text-inherit">{text.slice(idx, idx + query.length)}</mark>
      {text.slice(idx + query.length)}
    </>
  )
}

// 卡片可见性 + 搜索上下文闸门：
// - 未搜索：仅当卡片所属分区 == 当前激活 tab 时显示。
// - 搜索中：标题命中则整卡显示（内部设置项全部展示）；仅关键词命中则显示卡片但把 query 下传，
//   让内部 Field/ReadonlyRow 自行过滤，只留匹配项；都不命中则整卡隐藏。
function SectionGate({
  section,
  titleKey,
  kwKey,
  children,
}: {
  section: SectionId
  titleKey: string
  kwKey: string
  children: React.ReactNode
}) {
  const { t } = useTranslation()
  const { query } = useContext(SearchContext)
  const active = useContext(ActiveSectionContext)
  const title = t(titleKey)
  const keywords = parseKeywords(t(kwKey))
  if (query) {
    const titleMatch = title.toLowerCase().includes(query)
    const kwMatch = keywords.some((k) => k.toLowerCase().includes(query))
    if (!titleMatch && !kwMatch) return null
    return <SearchContext.Provider value={{ query: titleMatch ? '' : query }}>{children}</SearchContext.Provider>
  }
  return section === active ? <>{children}</> : null
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
  ccAutoBuffer: boolean
  stripEnvNoise: boolean
  toolCleanLeakedTokens: boolean
  toolReclaimTextifiedInvoke: boolean
  toolStrayRepeatGuard: boolean
  toolStreamAlignFailure: boolean
  toolExposeErrorToClient: boolean
  toolRepairJson: boolean
  toolTruncationRecovery: boolean
  toolDescriptionMaxChars: string
  encryptCredentialsAtRest: boolean
  cooldownEnabled: boolean
  allCoolingFastFail: boolean
  rateLimitEnabled: boolean
  rateLimitDailyMax: string
  rateLimitMinIntervalMs: string
  affinityEnabled: boolean
  priorityInBalanced: boolean
  // 智能调度（0.7.23/0.7.24，均热更即时生效）
  credentialRpmLimit: string
  rpmHeadroomFactor: string
  rpmReserveSlots: string
  rpmHardGateOverloadWait: boolean
  cooldownScalePct: string
  rateLimitJitterPct: string
  inboundThrottleEnabled: boolean
  inboundRpmAuto: boolean
  inboundTargetRpm: string
  inboundRpmMin: string
  inboundRpmMax: string
  inboundBurstSecs: string
  inboundQueueMaxWaitSecs: string
  inboundQueueTimeoutPassthrough: boolean
  balanceWeightEnabled: boolean
  balanceWeightFloor: string
  health429WeightEnabled: boolean
  proxyUrl: string
  proxyUsername: string
  proxyPassword: string
  apiKey: string
  callbackBaseUrl: string
  // 反代安全（批次3）：列表用换行分隔的多行文本承载
  corsAllowedOrigins: string
  ipAllowlist: string
  ipBlocklist: string
  machineCodeBlocklist: string
  trustForwardedHeader: boolean
  ingressRateLimitPerMin: string
  maxBodyBytes: string
  // 主动 token 预刷新（批次4.4）
  proactiveTokenRefresh: boolean
  tokenRefreshLeadMinutes: string
  tokenRefreshIntervalSecs: string
  // Admin UI 登录页背景（立即生效）
  loginBackgroundEnabled: boolean
  loginBackgroundR18: boolean
  // UI 排版自定义（纯前端 localStorage，纳入统一保存流程：切换改 form，保存时才落地）
  poolSort: PoolSortMode
  poolShowDisabled: boolean
  cardSize: CardSize
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

function toForm(c: ConfigSnapshotResponse, ui: UiLayoutPrefs): FormState {
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
    ccAutoBuffer: c.ccAutoBuffer,
    stripEnvNoise: c.stripEnvNoise,
    toolCleanLeakedTokens: c.toolCleanLeakedTokens ?? true,
    toolReclaimTextifiedInvoke: c.toolReclaimTextifiedInvoke ?? true,
    toolStrayRepeatGuard: c.toolStrayRepeatGuard ?? true,
    toolStreamAlignFailure: c.toolStreamAlignFailure ?? true,
    toolExposeErrorToClient: c.toolExposeErrorToClient ?? true,
    toolRepairJson: c.toolRepairJson ?? true,
    toolTruncationRecovery: c.toolTruncationRecovery ?? false,
    toolDescriptionMaxChars: String(c.toolDescriptionMaxChars ?? 10000),
    encryptCredentialsAtRest: c.encryptCredentialsAtRest ?? false,
    cooldownEnabled: c.cooldownEnabled,
    allCoolingFastFail: c.allCoolingFastFail ?? true,
    rateLimitEnabled: c.rateLimitEnabled,
    rateLimitDailyMax: String(c.rateLimitDailyMax),
    rateLimitMinIntervalMs: String(c.rateLimitMinIntervalMs),
    affinityEnabled: c.affinityEnabled,
    priorityInBalanced: c.priorityInBalanced,
    credentialRpmLimit: String(c.credentialRpmLimit ?? 0),
    rpmHeadroomFactor: String(c.rpmHeadroomFactor ?? 85),
    rpmReserveSlots: String(c.rpmReserveSlots ?? 0),
    rpmHardGateOverloadWait: c.rpmHardGateOverloadWait ?? false,
    cooldownScalePct: String(c.cooldownScalePct ?? 100),
    rateLimitJitterPct: String(c.rateLimitJitterPct ?? 20),
    inboundThrottleEnabled: c.inboundThrottleEnabled ?? true,
    inboundRpmAuto: c.inboundRpmAuto ?? true,
    inboundTargetRpm: String(c.inboundTargetRpm ?? 100),
    inboundRpmMin: String(c.inboundRpmMin ?? 20),
    inboundRpmMax: String(c.inboundRpmMax ?? 300),
    inboundBurstSecs: String(c.inboundBurstSecs ?? 2),
    inboundQueueMaxWaitSecs: String(c.inboundQueueMaxWaitSecs ?? 30),
    inboundQueueTimeoutPassthrough: c.inboundQueueTimeoutPassthrough ?? true,
    balanceWeightEnabled: c.balanceWeightEnabled ?? true,
    balanceWeightFloor: String(c.balanceWeightFloor ?? 50),
    health429WeightEnabled: c.health429WeightEnabled ?? true,
    proxyUrl: c.proxyUrl ?? '',
    // 代理账密出于安全后端不下发,UI 留空占位:留空=不改,填了=更新。
    proxyUsername: '',
    proxyPassword: '',
    // userKey(对话 api_key)后端不下发明文,留空=不改,填了=更新(需重启生效)。
    apiKey: '',
    callbackBaseUrl: c.callbackBaseUrl ?? '',
    corsAllowedOrigins: listToLines(c.corsAllowedOrigins ?? []),
    ipAllowlist: listToLines(c.ipAllowlist ?? []),
    ipBlocklist: listToLines(c.ipBlocklist ?? []),
    machineCodeBlocklist: listToLines(c.machineCodeBlocklist ?? []),
    trustForwardedHeader: c.trustForwardedHeader,
    ingressRateLimitPerMin: String(c.ingressRateLimitPerMin),
    maxBodyBytes: String(c.maxBodyBytes),
    proactiveTokenRefresh: c.proactiveTokenRefresh,
    tokenRefreshLeadMinutes: String(c.tokenRefreshLeadMinutes),
    tokenRefreshIntervalSecs: String(c.tokenRefreshIntervalSecs),
    // 缺省视为开启（后端字段可能尚未下发时不误显示为关闭）
    loginBackgroundEnabled: c.loginBackgroundEnabled ?? true,
    loginBackgroundR18: c.loginBackgroundR18 ?? false,
    // UI 排版偏好（纯前端 localStorage，作为 form 基线纳入统一保存）
    poolSort: ui.poolSort,
    poolShowDisabled: ui.poolShowDisabled,
    cardSize: ui.cardSize,
  }
}

// 搜索命中判定：query 为空恒真；否则 label/hint 任一包含 query 才算命中（用于逐项过滤）。
function rowMatches(query: string, label: string, hint?: string): boolean {
  if (!query) return true
  return label.toLowerCase().includes(query) || (hint?.toLowerCase().includes(query) ?? false)
}

// 一行可编辑/只读项布局。搜索态下若本项不命中则隐藏，命中则高亮 label。
function Field({ label, hint, children }: { label: string; hint?: string; children: React.ReactNode }) {
  const { query } = useContext(SearchContext)
  if (!rowMatches(query, label, hint)) return null
  return (
    <div className="flex items-start justify-between gap-4 py-3 border-b last:border-0">
      <div className="min-w-0 flex-1">
        <div className="text-sm"><Highlight text={label} /></div>
        {hint && <div className="text-xs text-muted-foreground mt-0.5 leading-relaxed">{hint}</div>}
      </div>
      <div className="shrink-0 flex justify-end pt-0.5">{children}</div>
    </div>
  )
}

// 分段选择按钮组(UI 排版分区用):一排互斥按钮,选中态高亮。
function SegChoice({
  value,
  onChange,
  options,
}: {
  value: string
  onChange: (v: string) => void
  options: { value: string; label: string }[]
}) {
  return (
    <div className="inline-flex flex-wrap gap-1 rounded-lg border border-white/10 bg-white/5 p-1">
      {options.map((o) => {
        const on = o.value === value
        return (
          <button
            key={o.value}
            type="button"
            onClick={() => onChange(o.value)}
            className={`rounded-md px-3 py-1 text-xs font-medium transition-colors ${
              on
                ? 'bg-primary/20 text-primary'
                : 'text-muted-foreground hover:bg-white/5 hover:text-foreground'
            }`}
          >
            {o.label}
          </button>
        )
      })}
    </div>
  )
}

function ReadonlyRow({ label, value, mono }: { label: string; value: React.ReactNode; mono?: boolean }) {
  const { query } = useContext(SearchContext)
  if (!rowMatches(query, label)) return null
  return (
    <div className="flex items-start justify-between gap-4 py-2 border-b last:border-0">
      <span className="text-sm text-muted-foreground shrink-0"><Highlight text={label} /></span>
      <span className={`text-sm text-right break-all ${mono ? 'font-mono text-xs' : ''}`}>{value}</span>
    </div>
  )
}

/* ============ 通用二次确认弹框 ============ */
// 危险操作前的确认：标题 + 描述 + 可选额外内容（如保留天数输入），确认色可选危险红。
// ConfirmDialog 已抽到 @/components/ui/confirm-dialog(供运维页共享),此处从该模块 import。

/* ============ 1. 服务管理：一键重启 + OTA 更新 ============ */
function ServiceManagementCard() {
  const { t } = useTranslation()
  const [confirmOpen, setConfirmOpen] = useState(false)
  const { mutate: restart, isPending } = useRestartService()
  // OTA 更新：检查 + 一键升级
  const { mutate: checkUpd, isPending: checking, data: updInfo } = useCheckUpdate()
  const { mutate: performUpd, isPending: upgrading } = usePerformUpdate()
  const { data: updStatus } = useUpdateStatus()
  const [upgradeConfirm, setUpgradeConfirm] = useState(false)

  const handleConfirm = () => {
    restart(undefined, {
      // 重启会掐断本次连接，成功/失败都当作"已发起"提示——真正结果看服务是否恢复。
      onSuccess: (resp) => {
        toast.success(resp.message || t('settingspage.service.toastRestarting'))
        setConfirmOpen(false)
      },
      onError: () => {
        // 连接被重启中断而抛错属预期，仍提示已发起
        toast.warning(t('settingspage.service.toastRestartingDisconnected'))
        setConfirmOpen(false)
      },
    })
  }

  const handleCheck = () => {
    checkUpd(undefined, {
      onSuccess: (r) => {
        if (r.error) toast.error(t('settingspage.service.toastCheckFail', { error: r.error }))
        else if (r.has_update) toast.success(t('settingspage.service.toastNewVersion', { latest: r.latest_version, local: r.local_version }))
        else toast.success(t('settingspage.service.toastLatest', { version: r.local_version }))
      },
      onError: (e) => toast.error(t('settingspage.service.toastCheckFail', { error: (e as Error).message })),
    })
  }

  const handleUpgrade = () => {
    performUpd(undefined, {
      onSuccess: (r) => {
        toast.success(r.message || t('settingspage.service.toastUpgrading'))
        setUpgradeConfirm(false)
      },
      onError: () => {
        // 升级成功后会自动重启导致本次连接中断，抛错也当"已发起"
        toast.warning(t('settingspage.service.toastUpgradingDisconnected'))
        setUpgradeConfirm(false)
      },
    })
  }

  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="text-base flex items-center gap-2">
          <Server className="h-4 w-4 text-muted-foreground" />
          <Highlight text={t('settingspage.card.service')} />
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-3">
        <p className="text-sm text-muted-foreground">
          {t('settingspage.service.descRestart')}
        </p>
        <Button variant="destructive" size="sm" onClick={() => setConfirmOpen(true)} disabled={isPending}>
          <RotateCcw className="mr-1.5 h-4 w-4" />
          {t('settingspage.service.btnRestart')}
        </Button>

        {/* OTA 更新：检查 GitHub 最新版本 + 一键升级（多镜像回退 + sha256 校验 + 换二进制 + 自动重启） */}
        <div className="border-t pt-3 space-y-2">
          <p className="text-sm text-muted-foreground">
            {t('settingspage.service.descOta')}
          </p>
          <div className="flex flex-wrap items-center gap-2">
            <Button variant="outline" size="sm" onClick={handleCheck} disabled={checking || upgrading}>
              {checking ? <Loader2 className="mr-1.5 h-4 w-4 animate-spin" /> : <RefreshCw className="mr-1.5 h-4 w-4" />}
              {t('settingspage.service.btnCheckUpdate')}
            </Button>
            {updInfo && !updInfo.error && (
              <span className="text-xs text-muted-foreground">
                {t('settingspage.service.current')} <span className="font-mono">{updInfo.local_version}</span>
                {updInfo.latest_version && (
                  <> {t('settingspage.service.latest')} <span className="font-mono">{updInfo.latest_version}</span></>
                )}
              </span>
            )}
            {updInfo?.has_update && (
              <Button size="sm" onClick={() => setUpgradeConfirm(true)} disabled={upgrading}>
                {upgrading ? <Loader2 className="mr-1.5 h-4 w-4 animate-spin" /> : <RotateCcw className="mr-1.5 h-4 w-4" />}
                {t('settingspage.service.btnUpgrade', { version: updInfo.latest_version })}
              </Button>
            )}
          </div>
          {/* commit 快照：展示"这版改了啥"（GitHub compare API 拉的 commit 列表），升级前先看清改动 */}
          {updInfo?.has_update && updInfo.commits.length > 0 && (
            <div className="mt-1 rounded-md border border-border/60 bg-secondary/30 p-2">
              <div className="mb-1 text-xs font-medium text-muted-foreground">
                {t('settingspage.service.commitsTitle', { n: updInfo.commits.length })}
              </div>
              <ul className="max-h-40 space-y-0.5 overflow-y-auto text-xs">
                {updInfo.commits.map((c) => (
                  <li key={c.sha} className="flex items-baseline gap-2">
                    <span className="shrink-0 font-mono text-[10px] text-primary/80">{c.sha}</span>
                    <span className="min-w-0 flex-1 truncate text-muted-foreground" title={c.title}>{c.title}</span>
                  </li>
                ))}
              </ul>
            </div>
          )}
          {/* OTA 升级/回滚观测:展示本版是否稳定确认、是否发生过回滚(后端 .health/.bak/*.failed 标记)。 */}
          {updStatus && (
            <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-1 text-xs">
              {updStatus.healthConfirmed ? (
                <span className="text-emerald-500" title={updStatus.healthDetail ?? undefined}>
                  {t('settingspage.service.stableConfirmed')}
                </span>
              ) : (
                <span className="text-amber-500">{t('settingspage.service.stablePending')}</span>
              )}
              {updStatus.rollbackPointPresent && (
                <span className="text-muted-foreground">{t('settingspage.service.rollbackPoint')}</span>
              )}
              {updStatus.rolledBackBinaryPresent && (
                <span className="text-red-400" title={t('settingspage.service.rollbackDetectedTitle')}>{t('settingspage.service.rollbackDetected')}</span>
              )}
            </div>
          )}
        </div>
      </CardContent>

      <ConfirmDialog
        open={confirmOpen}
        onOpenChange={setConfirmOpen}
        title={t('settingspage.service.confirmRestartTitle')}
        description={t('settingspage.service.confirmRestartDesc')}
        confirmLabel={t('settingspage.service.confirmRestartLabel')}
        destructive
        loading={isPending}
        onConfirm={handleConfirm}
      />
      <ConfirmDialog
        open={upgradeConfirm}
        onOpenChange={setUpgradeConfirm}
        title={t('settingspage.service.confirmUpgradeTitle', {
          version: updInfo?.latest_version ?? t('settingspage.service.confirmUpgradeTitleFallback'),
        })}
        description={t('settingspage.service.confirmUpgradeDesc')}
        confirmLabel={t('settingspage.service.confirmUpgradeLabel')}
        loading={upgrading}
        onConfirm={handleUpgrade}
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
  onView,
}: {
  p: StoragePartition
  onCleanup: (p: StoragePartition) => void
  onView: (p: StoragePartition) => void
}) {
  const { t } = useTranslation()
  const cleanable = (CLEANABLE_KEYS as string[]).includes(p.key)
  // 四个可清理分区各有高保真明细弹框（与 CLEANABLE_KEYS 同集，复用运维页模板）。
  const viewable = (CLEANABLE_KEYS as string[]).includes(p.key)
  return (
    <div className="flex items-center justify-between gap-4 border-b border-border/40 py-3 last:border-0">
      <div className="min-w-0">
        <div className="flex items-center gap-2 text-sm">
          <span className="truncate">{p.label}</span>
          {p.inMemory && (
            <Badge variant="outline" className="text-[10px]">
              {t('settingspage.storage.inMemory')}
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
          <div className="text-[11px] text-muted-foreground tabular-nums">{t('settingspage.storage.items', { n: p.items })}</div>
        </div>
        {viewable && (
          <Button variant="outline" size="sm" onClick={() => onView(p)}>
            <Eye className="mr-1 h-3.5 w-3.5" />
            {t('settingspage.common.view')}
          </Button>
        )}
        {cleanable && (
          <Button variant="outline" size="sm" onClick={() => onCleanup(p)}>
            <Trash className="mr-1 h-3.5 w-3.5" />
            {t('settingspage.common.clean')}
          </Button>
        )}
      </div>
    </div>
  )
}

function StorageStatsCard() {
  const { t } = useTranslation()
  const { data, isLoading, error, refetch } = useStorageStats()
  const { mutate: cleanup, isPending } = useCleanupStorage()

  // 清理弹框状态：target 分区 + 可选保留天数（空=按配置默认保留期）
  const [target, setTarget] = useState<StoragePartition | null>(null)
  const [keepDays, setKeepDays] = useState<string>('')
  // 分区明细弹框：按分区 key 打开对应高保真明细（复用运维页 ops-detail-dialogs 模板）。
  const [detail, setDetail] = useState<null | 'traces' | 'usage_jsonl' | 'trash' | 'bg_cache'>(null)
  // bg_cache 分区张数（供背景图缓存弹框渲染 idx 网格）。
  const bgCount = data?.partitions.find((p) => p.key === 'bg_cache')?.items ?? 0

  const openCleanup = (p: StoragePartition) => {
    setTarget(p)
    setKeepDays('')
  }

  const openView = (p: StoragePartition) => {
    setDetail(p.key as 'traces' | 'usage_jsonl' | 'trash' | 'bg_cache')
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
          <Highlight text={t('settingspage.card.storage')} />
        </CardTitle>
        <Button variant="outline" size="sm" onClick={() => refetch()} disabled={isLoading}>
          {t('settingspage.common.refresh')}
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
            {t('settingspage.storage.loadFail', { error: extractErrorMessage(error) })}
          </div>
        ) : data ? (
          <>
            <StatCard
              label={t('settingspage.storage.totalDisk')}
              value={formatBytes(data.totalDiskBytes)}
              icon={Database}
              accent="primary"
              hint={data.usageEnabled ? t('settingspage.storage.usageEnabled') : t('settingspage.storage.usageDisabled')}
            />
            <div>
              {data.partitions.map((p) => (
                <StoragePartitionRow key={p.key} p={p} onCleanup={openCleanup} onView={openView} />
              ))}
              {data.partitions.length === 0 && (
                <p className="py-4 text-center text-sm text-muted-foreground">{t('settingspage.storage.noPartitions')}</p>
              )}
            </div>
          </>
        ) : null}
      </CardContent>

      <ConfirmDialog
        open={target !== null}
        onOpenChange={(v) => !v && setTarget(null)}
        title={t('settingspage.storage.confirmTitle', { label: target?.label ?? '' })}
        description={
          <span>
            {t('settingspage.storage.confirmDescBase')}
            <strong className="text-red-400">{t('settingspage.storage.confirmDescIrreversible')}</strong>
            {t('settingspage.storage.confirmDescMid')}
            {supportsDays
              ? t('settingspage.storage.confirmDescDays')
              : t('settingspage.storage.confirmDescWhole')}
          </span>
        }
        confirmLabel={t('settingspage.storage.confirmClean')}
        destructive
        loading={isPending}
        onConfirm={handleConfirm}
      >
        {supportsDays && (
          <div className="flex items-center justify-between gap-3 rounded-md border border-border/60 bg-secondary/30 px-3 py-2">
            <span className="text-sm">{t('settingspage.storage.keepDays')}</span>
            <div className="flex items-center gap-2">
              <Input
                className="w-24 text-right"
                type="number"
                min={0}
                value={keepDays}
                onChange={(e) => setKeepDays(e.target.value)}
                placeholder={t('settingspage.common.default')}
              />
              <span className="text-xs text-muted-foreground">{t('settingspage.common.days')}</span>
            </div>
          </div>
        )}
      </ConfirmDialog>

      {/* 分区高保真明细弹框（查看按钮触发，复用运维页 ops-detail-dialogs 模板） */}
      <TraceDetailDialog open={detail === 'traces'} onOpenChange={(v) => !v && setDetail(null)} />
      <UsageDetailDialog open={detail === 'usage_jsonl'} onOpenChange={(v) => !v && setDetail(null)} />
      <TrashDetailDialog open={detail === 'trash'} onOpenChange={(v) => !v && setDetail(null)} />
      <BgCacheDetailDialog open={detail === 'bg_cache'} onOpenChange={(v) => !v && setDetail(null)} count={bgCount} />
    </Card>
  )
}

/* ============ 3. per 客户端/窗口 RPM 面板 ============ */
function ClientRow({ c }: { c: ClientRpm }) {
  const { t } = useTranslation()
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
            {t('settingspage.clientRpm.windows', { n: c.activeSessions })}
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
  const { t } = useTranslation()
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
          <Highlight text={t('settingspage.card.clientRpm')} />
          {isFetching && <Loader className="h-3.5 w-3.5 animate-spin text-muted-foreground" />}
        </CardTitle>
        <span className="text-xs text-muted-foreground">{t('settingspage.clientRpm.refreshHint')}</span>
      </CardHeader>
      <CardContent className="space-y-4">
        <div className="grid grid-cols-3 gap-3">
          <StatCard label={t('settingspage.clientRpm.clientCount')} value={data?.length ?? 0} accent="neutral" />
          <StatCard label={t('settingspage.clientRpm.totalRpm')} value={totalRpm} accent="primary" />
          <StatCard label={t('settingspage.clientRpm.activeWindows')} value={totalWindows} accent="success" />
        </div>

        {isLoading ? (
          <div className="space-y-3">
            <Skeleton className="h-12 w-full" />
            <Skeleton className="h-12 w-full" />
            <Skeleton className="h-12 w-full" />
          </div>
        ) : error ? (
          <div className="py-4 text-center text-sm text-red-400">
            {t('settingspage.clientRpm.loadFail', { error: extractErrorMessage(error) })}
          </div>
        ) : data && data.length > 0 ? (
          <div>
            <p className="mb-1 px-2 text-xs text-muted-foreground">
              {t('settingspage.clientRpm.expandHint')}
            </p>
            {data.map((c) => (
              <ClientRow key={c.clientKey} c={c} />
            ))}
          </div>
        ) : (
          <p className="py-6 text-center text-sm text-muted-foreground">{t('settingspage.clientRpm.noClients')}</p>
        )}
      </CardContent>
    </Card>
  )
}

/* ============ 4. 隐私：下游客户端指纹采集开关（立即生效） ============ */
// 独立于底部批量保存：切换即调用 updateConfig，立即生效、无需重启。
function PrivacyCard() {
  const { t } = useTranslation()
  const { data: config } = useConfigSnapshot()
  const { mutate: save, isPending } = useUpdateConfig()

  // 缺省视为开启（后端字段可能尚未下发时不误显示为关闭）
  const enabled = config?.collectClientFingerprint ?? true

  const toggle = (v: boolean) => {
    save(
      { collectClientFingerprint: v },
      {
        onSuccess: () =>
          toast.success(v ? t('settingspage.privacy.toast.on') : t('settingspage.privacy.toast.off')),
        onError: (err) => toast.error(extractErrorMessage(err)),
      }
    )
  }

  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="text-base flex items-center gap-2">
          <Fingerprint className="h-4 w-4 text-muted-foreground" />
          <Highlight text={t('settingspage.card.privacy')} />
        </CardTitle>
      </CardHeader>
      <CardContent className="py-0">
        <Field
          label={t('settingspage.privacy.field.label')}
          hint={t('settingspage.privacy.field.hint')}
        >
          <Switch checked={enabled} disabled={isPending} onCheckedChange={toggle} />
        </Field>
      </CardContent>
    </Card>
  )
}

/* ============ 5. 令牌导出：单个 / 全部凭据 JSON 下载 ============ */
function TokenExportCard() {
  const { t } = useTranslation()
  const { data: creds, isLoading, error, refetch } = useCredentials()
  // 记录正在导出的凭据 id（单个），以及“导出全部”进行中标志，避免重复点击。
  const [exportingId, setExportingId] = useState<number | null>(null)
  const [exportingAll, setExportingAll] = useState(false)

  const list = creds?.credentials ?? []

  const stamp = () => new Date().toISOString().slice(0, 19).replace(/[:T]/g, '-')

  // 导出单个凭据完整 JSON 文件（可重新导入）。
  const exportOne = async (id: number) => {
    setExportingId(id)
    try {
      const obj = await exportCredential(id)
      downloadJson(`credential-${id}-${stamp()}.json`, obj)
      toast.success(t('settingspage.export.toast.exportedOne', { id }))
    } catch (err) {
      toast.error(extractErrorMessage(err))
    } finally {
      setExportingId(null)
    }
  }

  // 仅导出 refreshToken 纯文本（API Key 凭据不含此字段）。
  const exportRefreshTokenOne = async (id: number) => {
    setExportingId(id)
    try {
      const obj = await exportCredential(id)
      const token = obj.refreshToken
      if (typeof token !== 'string' || !token) {
        toast.error(t('settingspage.export.toast.noRt'))
        return
      }
      const blob = new Blob([token], { type: 'text/plain' })
      const url = URL.createObjectURL(blob)
      const a = document.createElement('a')
      a.href = url
      a.download = `credential-${id}-refreshtoken-${stamp()}.txt`
      document.body.appendChild(a)
      a.click()
      document.body.removeChild(a)
      URL.revokeObjectURL(url)
      toast.success(t('settingspage.export.toast.exportedRt', { id }))
    } catch (err) {
      toast.error(extractErrorMessage(err))
    } finally {
      setExportingId(null)
    }
  }

  // 复制单个凭据完整 JSON 到剪贴板。
  const copyOne = async (id: number) => {
    setExportingId(id)
    try {
      const obj = await exportCredential(id)
      const ok = await copyToClipboard(JSON.stringify(obj, null, 2))
      if (ok) {
        toast.success(t('settingspage.export.toast.copied', { id }))
      } else {
        toast.error(t('settingspage.export.toast.copyFail'))
      }
    } catch (err) {
      toast.error(extractErrorMessage(err))
    } finally {
      setExportingId(null)
    }
  }

  // 导出全部：逐个串行拉取（避免并发压上游导出端点），打包成数组一次性下载。
  const exportAll = async () => {
    if (list.length === 0) return
    setExportingAll(true)
    try {
      const all: Record<string, unknown>[] = []
      for (const c of list) {
        all.push(await exportCredential(c.id))
      }
      downloadJson(`credentials-all-${stamp()}.json`, all)
      toast.success(t('settingspage.export.toast.exportedAll', { n: all.length }))
    } catch (err) {
      toast.error(extractErrorMessage(err))
    } finally {
      setExportingAll(false)
    }
  }

  return (
    <Card>
      <CardHeader className="pb-2 flex-row items-center justify-between space-y-0">
        <CardTitle className="text-base flex items-center gap-2">
          <Download className="h-4 w-4 text-muted-foreground" />
          <Highlight text={t('settingspage.card.export')} />
        </CardTitle>
        <Button
          variant="outline"
          size="sm"
          onClick={exportAll}
          disabled={exportingAll || isLoading || list.length === 0}
        >
          {exportingAll ? (
            <Loader className="mr-1.5 h-4 w-4 animate-spin" />
          ) : (
            <Download className="mr-1.5 h-4 w-4" />
          )}
          {t('settingspage.export.exportAll', { n: list.length })}
        </Button>
      </CardHeader>
      <CardContent className="space-y-3">
        <p className="text-sm text-muted-foreground">
          {t('settingspage.export.desc')}
        </p>
        {isLoading ? (
          <div className="space-y-2">
            <Skeleton className="h-10 w-full" />
            <Skeleton className="h-10 w-full" />
          </div>
        ) : error ? (
          <div className="py-3 text-center text-sm text-red-400">
            {t('settingspage.export.loadFail', { error: extractErrorMessage(error) })}
            <Button variant="outline" size="sm" className="ml-2" onClick={() => refetch()}>
              {t('settingspage.common.retry')}
            </Button>
          </div>
        ) : list.length === 0 ? (
          <p className="py-4 text-center text-sm text-muted-foreground">{t('settingspage.export.noCreds')}</p>
        ) : (
          <div>
            {list.map((c) => (
              <div
                key={c.id}
                className="flex items-center justify-between gap-4 border-b border-border/40 py-2.5 last:border-0"
              >
                <div className="min-w-0">
                  <div className="truncate text-sm">
                    #{c.id}
                    {c.email ? ` · ${c.email}` : ''}
                  </div>
                  <div className="mt-0.5 text-[11px] text-muted-foreground">
                    {c.subscriptionTitle || c.authMethod || t('settingspage.common.credential')}
                  </div>
                </div>
                <div className="flex shrink-0 items-center gap-1.5">
                  {exportingId === c.id ? (
                    <div className="flex h-8 items-center px-2 text-xs text-muted-foreground">
                      <Loader className="mr-1 h-3.5 w-3.5 animate-spin" />
                      {t('settingspage.export.exporting')}
                    </div>
                  ) : (
                    <>
                      <Button
                        variant="outline"
                        size="sm"
                        onClick={() => exportOne(c.id)}
                        disabled={exportingAll}
                        title={t('settingspage.export.jsonTitle')}
                      >
                        <FileJson className="mr-1 h-3.5 w-3.5" />
                        JSON
                      </Button>
                      <Button
                        variant="outline"
                        size="icon"
                        className="h-8 w-8"
                        onClick={() => exportRefreshTokenOne(c.id)}
                        disabled={exportingAll}
                        title={t('settingspage.export.refreshTokenTitle')}
                      >
                        <KeyRound className="h-3.5 w-3.5" />
                      </Button>
                      <Button
                        variant="outline"
                        size="icon"
                        className="h-8 w-8"
                        onClick={() => copyOne(c.id)}
                        disabled={exportingAll}
                        title={t('settingspage.export.copyTitle')}
                      >
                        <ClipboardCopy className="h-3.5 w-3.5" />
                      </Button>
                    </>
                  )}
                </div>
              </div>
            ))}
          </div>
        )}
      </CardContent>
    </Card>
  )
}

/* ============ 6. 回收站：已删除凭据的恢复 / 永久清除 ============ */
// 待确认的危险操作：单条永久删除 / 清空选中 / 清空全部。
type TrashConfirm =
  | { kind: 'one'; item: TrashItem }
  | { kind: 'selected'; ids: number[] }
  | { kind: 'all' }

// 回收站单行：勾选 + 元信息 + 恢复 / 永久删除。
function TrashRow({
  item,
  checked,
  onToggle,
  onRestore,
  onPurge,
  busy,
}: {
  item: TrashItem
  checked: boolean
  onToggle: (v: boolean) => void
  onRestore: () => void
  onPurge: () => void
  busy: boolean
}) {
  const { t } = useTranslation()
  return (
    <div className="flex items-center justify-between gap-4 border-b border-border/40 py-2.5 last:border-0">
      <div className="flex min-w-0 items-center gap-3">
        <Checkbox
          checked={checked}
          onCheckedChange={(v) => onToggle(v === true)}
          aria-label={t('settingspage.trash.selectAria', { id: item.id })}
        />
        <div className="min-w-0">
          <div className="truncate text-sm">
            #{item.id}
            {item.email ? ` · ${item.email}` : ''}
          </div>
          <div className="mt-0.5 flex flex-wrap items-center gap-x-2 gap-y-0.5 text-[11px] text-muted-foreground">
            <span>{item.authMethod || t('settingspage.common.credential')}</span>
            <span>·</span>
            <span title={item.deletedAt}>{t('settingspage.trash.deletedAt', { when: timeAgo(item.deletedAt, t) })}</span>
            <span>·</span>
            <span>{t('settingspage.trash.successCount', { n: item.successCount })}</span>
          </div>
        </div>
      </div>
      <div className="flex shrink-0 items-center gap-2">
        <Button variant="outline" size="sm" onClick={onRestore} disabled={busy}>
          <RotateCcw className="mr-1 h-3.5 w-3.5" />
          {t('settingspage.trash.btnRestore')}
        </Button>
        <Button variant="destructive" size="sm" onClick={onPurge} disabled={busy}>
          <Trash className="mr-1 h-3.5 w-3.5" />
          {t('settingspage.trash.btnPurge')}
        </Button>
      </div>
    </div>
  )
}

function TrashCard() {
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  const { data, isLoading, error, refetch, isFetching } = useQuery({
    queryKey: ['trash'],
    queryFn: listTrash,
    refetchInterval: 30000,
  })

  const list = useMemo(() => data?.trash ?? [], [data])

  // 本地勾选态；随列表变化剔除已不存在的 id。
  const [selected, setSelected] = useState<Set<number>>(new Set())
  useEffect(() => {
    setSelected((prev) => {
      const valid = new Set(list.map((t) => t.id))
      const next = new Set<number>()
      prev.forEach((id) => valid.has(id) && next.add(id))
      return next.size === prev.size ? prev : next
    })
  }, [list])

  const [confirm, setConfirm] = useState<TrashConfirm | null>(null)
  const [busy, setBusy] = useState(false)

  const allChecked = list.length > 0 && selected.size === list.length
  const someChecked = selected.size > 0 && !allChecked

  const toggleAll = (v: boolean) => {
    setSelected(v ? new Set(list.map((t) => t.id)) : new Set())
  }

  const toggleOne = (id: number, v: boolean) => {
    setSelected((prev) => {
      const next = new Set(prev)
      if (v) next.add(id)
      else next.delete(id)
      return next
    })
  }

  const invalidate = () => {
    queryClient.invalidateQueries({ queryKey: ['trash'] })
    queryClient.invalidateQueries({ queryKey: ['credentials'] })
  }

  // 恢复无需二次确认（非破坏性）。
  const handleRestore = async (item: TrashItem) => {
    setBusy(true)
    try {
      await restoreCredential(item.id)
      toast.success(t('settingspage.trash.toast.restored', { id: item.id }))
      invalidate()
    } catch (err) {
      toast.error(extractErrorMessage(err))
    } finally {
      setBusy(false)
    }
  }

  // 执行已确认的破坏性操作。
  const runConfirmed = async () => {
    if (!confirm) return
    setBusy(true)
    try {
      if (confirm.kind === 'one') {
        await purgeCredential(confirm.item.id)
        toast.success(t('settingspage.trash.toast.purgedOne', { id: confirm.item.id }))
      } else if (confirm.kind === 'selected') {
        await purgeTrashBatch(confirm.ids)
        toast.success(t('settingspage.trash.toast.purgedSelected', { n: confirm.ids.length }))
      } else {
        await purgeTrashBatch()
        toast.success(t('settingspage.trash.toast.cleared'))
      }
      setSelected(new Set())
      setConfirm(null)
      invalidate()
    } catch (err) {
      toast.error(extractErrorMessage(err))
    } finally {
      setBusy(false)
    }
  }

  const total = data?.total ?? list.length

  const confirmTitle =
    confirm?.kind === 'one'
      ? t('settingspage.trash.confirmOneTitle', { id: confirm.item.id })
      : confirm?.kind === 'selected'
        ? t('settingspage.trash.confirmSelectedTitle', { n: confirm.ids.length })
        : t('settingspage.trash.confirmAllTitle')

  return (
    <Card>
      <CardHeader className="pb-2 flex-row items-center justify-between space-y-0">
        <CardTitle className="text-base flex items-center gap-2">
          <Trash2 className="h-4 w-4 text-muted-foreground" />
          <Highlight text={t('settingspage.card.trash')} />
          {isFetching && <Loader className="h-3.5 w-3.5 animate-spin text-muted-foreground" />}
        </CardTitle>
        <Button variant="outline" size="sm" onClick={() => refetch()} disabled={isLoading}>
          {t('settingspage.common.refresh')}
        </Button>
      </CardHeader>
      <CardContent className="space-y-4">
        <p className="text-sm text-muted-foreground">
          {t('settingspage.trash.desc')}
          <strong className="text-red-400">{t('settingspage.trash.descStrong')}</strong>
          {t('settingspage.trash.descTail')}
        </p>

        {isLoading ? (
          <div className="space-y-2">
            <Skeleton className="h-12 w-full" />
            <Skeleton className="h-12 w-full" />
            <Skeleton className="h-12 w-full" />
          </div>
        ) : error ? (
          <div className="py-4 text-center text-sm text-red-400">
            {t('settingspage.trash.loadFail', { error: extractErrorMessage(error) })}
            <Button variant="outline" size="sm" className="ml-2" onClick={() => refetch()}>
              {t('settingspage.common.retry')}
            </Button>
          </div>
        ) : list.length === 0 ? (
          <div className="flex flex-col items-center gap-2 py-12 text-center text-sm text-muted-foreground">
            <Trash2 className="h-8 w-8 opacity-40" />
            {t('settingspage.trash.empty')}
          </div>
        ) : (
          <>
            {/* 工具栏：全选 + 批量操作 */}
            <div className="flex flex-wrap items-center justify-between gap-3 rounded-md border border-border/60 bg-secondary/30 px-3 py-2">
              <label className="flex cursor-pointer items-center gap-2 text-sm">
                <Checkbox
                  checked={allChecked ? true : someChecked ? 'indeterminate' : false}
                  onCheckedChange={(v) => toggleAll(v === true)}
                  aria-label={t('settingspage.trash.selectAll')}
                />
                <span>
                  {t('settingspage.trash.selectAll')}
                  {selected.size > 0 && (
                    <span className="ml-1 text-muted-foreground">
                      {t('settingspage.trash.selectedCount', { selected: selected.size, total })}
                    </span>
                  )}
                </span>
              </label>
              <div className="flex items-center gap-2">
                <Button
                  variant="destructive"
                  size="sm"
                  disabled={selected.size === 0 || busy}
                  onClick={() => setConfirm({ kind: 'selected', ids: Array.from(selected) })}
                >
                  <Trash className="mr-1 h-3.5 w-3.5" />
                  {t('settingspage.trash.btnPurgeSelected')}
                </Button>
                <Button
                  variant="destructive"
                  size="sm"
                  disabled={busy}
                  onClick={() => setConfirm({ kind: 'all' })}
                >
                  <Trash2 className="mr-1 h-3.5 w-3.5" />
                  {t('settingspage.trash.btnPurgeAll')}
                </Button>
              </div>
            </div>

            <div>
              {list.map((item) => (
                <TrashRow
                  key={item.id}
                  item={item}
                  checked={selected.has(item.id)}
                  onToggle={(v) => toggleOne(item.id, v)}
                  onRestore={() => handleRestore(item)}
                  onPurge={() => setConfirm({ kind: 'one', item })}
                  busy={busy}
                />
              ))}
            </div>
          </>
        )}
      </CardContent>

      <ConfirmDialog
        open={confirm !== null}
        onOpenChange={(v) => !v && setConfirm(null)}
        title={confirmTitle}
        description={
          <span>
            {t('settingspage.trash.confirmDesc')}
            <strong className="text-red-400">{t('settingspage.trash.confirmDescStrong')}</strong>
            {t('settingspage.trash.confirmDescTail')}
            {confirm?.kind === 'all'
              ? t('settingspage.trash.confirmAllDesc')
              : t('settingspage.trash.confirmSelectedDesc')}
          </span>
        }
        confirmLabel={t('settingspage.trash.confirmLabel')}
        destructive
        loading={busy}
        onConfirm={runConfirmed}
      />
    </Card>
  )
}

export function SettingsPage() {
  const { t, i18n } = useTranslation()
  const { data: config, isLoading, error, refetch } = useConfigSnapshot()
  const { mutate: save, isPending: isSaving } = useUpdateConfig()

  const [form, setForm] = useState<FormState | null>(null)

  // 分区导航当前 tab + 搜索关键词（纯前端）。
  const [activeSection, setActiveSection] = useState<SectionId>('basic')
  // UI 排版偏好(号池排序模式 / 禁用号显隐 / 卡片尺寸)。localStorage 持久,概览页+凭据页读它生效。
  const { prefs: uiPrefs, set: setUiPrefs } = useUiLayoutPrefs()
  const [searchRaw, setSearchRaw] = useState('')
  const query = searchRaw.trim().toLowerCase()

  // 搜索态下命中的分区集合（用于结果提示条的“命中 N 个分区”与空态判断）。
  const matchedSections = useMemo(() => {
    if (!query) return new Set<SectionId>()
    const s = new Set<SectionId>()
    for (const c of CARD_INDEX_DEFS) {
      const title = t(c.titleKey)
      const keywords = parseKeywords(t(c.kwKey))
      if (title.toLowerCase().includes(query) || keywords.some((k) => k.includes(query))) {
        s.add(c.section)
      }
    }
    return s
  }, [query, t, i18n.language])

  const hasAnyMatch = matchedSections.size > 0

  // 配置加载/刷新后，重置表单基线（含 UI 排版偏好）
  useEffect(() => {
    if (config) setForm(toForm(config, uiPrefs))
    // uiPrefs 作为初始基线读取一次即可，其变化不应覆盖用户正在编辑的 form（保存时才回写 localStorage）。
    // eslint-disable-next-line react-hooks/exhaustive-deps
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
    if (form.ccAutoBuffer !== config.ccAutoBuffer) d.ccAutoBuffer = form.ccAutoBuffer
    if (form.stripEnvNoise !== config.stripEnvNoise) d.stripEnvNoise = form.stripEnvNoise
    if (form.toolCleanLeakedTokens !== (config.toolCleanLeakedTokens ?? true)) d.toolCleanLeakedTokens = form.toolCleanLeakedTokens
    if (form.toolReclaimTextifiedInvoke !== (config.toolReclaimTextifiedInvoke ?? true)) d.toolReclaimTextifiedInvoke = form.toolReclaimTextifiedInvoke
    if (form.toolStrayRepeatGuard !== (config.toolStrayRepeatGuard ?? true)) d.toolStrayRepeatGuard = form.toolStrayRepeatGuard
    if (form.toolStreamAlignFailure !== (config.toolStreamAlignFailure ?? true)) d.toolStreamAlignFailure = form.toolStreamAlignFailure
    if (form.toolExposeErrorToClient !== (config.toolExposeErrorToClient ?? true)) d.toolExposeErrorToClient = form.toolExposeErrorToClient
    if (form.toolRepairJson !== (config.toolRepairJson ?? true)) d.toolRepairJson = form.toolRepairJson
    if (form.toolTruncationRecovery !== (config.toolTruncationRecovery ?? false)) d.toolTruncationRecovery = form.toolTruncationRecovery
    const descMax = Number(form.toolDescriptionMaxChars)
    if (Number.isFinite(descMax) && descMax >= 0 && descMax !== (config.toolDescriptionMaxChars ?? 10000)) d.toolDescriptionMaxChars = descMax
    if (form.encryptCredentialsAtRest !== (config.encryptCredentialsAtRest ?? false)) d.encryptCredentialsAtRest = form.encryptCredentialsAtRest
    if (form.cooldownEnabled !== config.cooldownEnabled) d.cooldownEnabled = form.cooldownEnabled
    if (form.allCoolingFastFail !== (config.allCoolingFastFail ?? true)) d.allCoolingFastFail = form.allCoolingFastFail
    if (form.rateLimitEnabled !== config.rateLimitEnabled) d.rateLimitEnabled = form.rateLimitEnabled
    const daily = Number(form.rateLimitDailyMax)
    if (Number.isFinite(daily) && daily !== config.rateLimitDailyMax) d.rateLimitDailyMax = daily
    const interval = Number(form.rateLimitMinIntervalMs)
    if (Number.isFinite(interval) && interval !== config.rateLimitMinIntervalMs) d.rateLimitMinIntervalMs = interval
    if (form.affinityEnabled !== config.affinityEnabled) d.affinityEnabled = form.affinityEnabled
    if (form.priorityInBalanced !== config.priorityInBalanced) d.priorityInBalanced = form.priorityInBalanced
    // 智能调度:整数字段解析后比对(空/非法回退当前值不发)。
    const nCredRpm = parseInt(form.credentialRpmLimit, 10)
    if (Number.isFinite(nCredRpm) && nCredRpm !== (config.credentialRpmLimit ?? 0)) d.credentialRpmLimit = nCredRpm
    const nHeadroom = parseInt(form.rpmHeadroomFactor, 10)
    if (Number.isFinite(nHeadroom) && nHeadroom !== config.rpmHeadroomFactor) d.rpmHeadroomFactor = nHeadroom
    const nReserve = parseInt(form.rpmReserveSlots, 10)
    if (Number.isFinite(nReserve) && nReserve !== config.rpmReserveSlots) d.rpmReserveSlots = nReserve
    if (form.rpmHardGateOverloadWait !== config.rpmHardGateOverloadWait) d.rpmHardGateOverloadWait = form.rpmHardGateOverloadWait
    const nCooldownScale = parseInt(form.cooldownScalePct, 10)
    if (Number.isFinite(nCooldownScale) && nCooldownScale !== (config.cooldownScalePct ?? 100)) d.cooldownScalePct = nCooldownScale
    const nJitter = parseInt(form.rateLimitJitterPct, 10)
    if (Number.isFinite(nJitter) && nJitter !== (config.rateLimitJitterPct ?? 20)) d.rateLimitJitterPct = nJitter
    // 入站整形
    if (form.inboundThrottleEnabled !== (config.inboundThrottleEnabled ?? true)) d.inboundThrottleEnabled = form.inboundThrottleEnabled
    if (form.inboundRpmAuto !== (config.inboundRpmAuto ?? true)) d.inboundRpmAuto = form.inboundRpmAuto
    const nTarget = parseInt(form.inboundTargetRpm, 10)
    if (Number.isFinite(nTarget) && nTarget !== (config.inboundTargetRpm ?? 100)) d.inboundTargetRpm = nTarget
    const nRmin = parseInt(form.inboundRpmMin, 10)
    if (Number.isFinite(nRmin) && nRmin !== (config.inboundRpmMin ?? 20)) d.inboundRpmMin = nRmin
    const nRmax = parseInt(form.inboundRpmMax, 10)
    if (Number.isFinite(nRmax) && nRmax !== (config.inboundRpmMax ?? 300)) d.inboundRpmMax = nRmax
    const nBurst = parseInt(form.inboundBurstSecs, 10)
    if (Number.isFinite(nBurst) && nBurst !== (config.inboundBurstSecs ?? 2)) d.inboundBurstSecs = nBurst
    const nQwait = parseInt(form.inboundQueueMaxWaitSecs, 10)
    if (Number.isFinite(nQwait) && nQwait !== (config.inboundQueueMaxWaitSecs ?? 30)) d.inboundQueueMaxWaitSecs = nQwait
    if (form.inboundQueueTimeoutPassthrough !== (config.inboundQueueTimeoutPassthrough ?? true)) d.inboundQueueTimeoutPassthrough = form.inboundQueueTimeoutPassthrough
    if (form.balanceWeightEnabled !== config.balanceWeightEnabled) d.balanceWeightEnabled = form.balanceWeightEnabled
    const nFloor = parseInt(form.balanceWeightFloor, 10)
    if (Number.isFinite(nFloor) && nFloor !== config.balanceWeightFloor) d.balanceWeightFloor = nFloor
    if (form.health429WeightEnabled !== config.health429WeightEnabled) d.health429WeightEnabled = form.health429WeightEnabled
    if (form.proxyUrl.trim() !== (config.proxyUrl ?? '')) d.proxyUrl = form.proxyUrl.trim()
    // 代理账密:后端不下发(安全),故只在用户填了内容时才发送(留空=保持不变)。
    if (form.proxyUsername.trim() !== '') d.proxyUsername = form.proxyUsername.trim()
    if (form.proxyPassword !== '') d.proxyPassword = form.proxyPassword
    if (form.apiKey.trim() !== '') d.apiKey = form.apiKey.trim()
    if (form.callbackBaseUrl.trim() !== (config.callbackBaseUrl ?? '')) d.callbackBaseUrl = form.callbackBaseUrl.trim()
    // 反代安全
    const origins = linesToList(form.corsAllowedOrigins)
    if (!sameList(origins, config.corsAllowedOrigins ?? [])) d.corsAllowedOrigins = origins
    const allowlist = linesToList(form.ipAllowlist)
    if (!sameList(allowlist, config.ipAllowlist ?? [])) d.ipAllowlist = allowlist
    const blocklist = linesToList(form.ipBlocklist)
    if (!sameList(blocklist, config.ipBlocklist ?? [])) d.ipBlocklist = blocklist
    const mcBlocklist = linesToList(form.machineCodeBlocklist)
    if (!sameList(mcBlocklist, config.machineCodeBlocklist ?? [])) d.machineCodeBlocklist = mcBlocklist
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
    // Admin UI 登录页背景（缺省视为开启，与 toForm 基线一致）
    if (form.loginBackgroundEnabled !== (config.loginBackgroundEnabled ?? true)) d.loginBackgroundEnabled = form.loginBackgroundEnabled
    if (form.loginBackgroundR18 !== (config.loginBackgroundR18 ?? false)) d.loginBackgroundR18 = form.loginBackgroundR18
    return d
  }, [config, form])

  // UI 排版偏好(纯前端 localStorage)是否与当前已落地值有差异。它不进后端 diff,
  // 但纳入统一 dirty/保存流程:点亮保存按钮、保存时才写 localStorage。
  const uiPrefsDirty = !!form && (
    form.poolSort !== uiPrefs.poolSort ||
    form.poolShowDisabled !== uiPrefs.poolShowDisabled ||
    form.cardSize !== uiPrefs.cardSize
  )

  const dirty = Object.keys(diff).length > 0 || uiPrefsDirty

  // 待保存改动计数(后端字段 + UI 排版视为 1 项聚合)。
  const dirtyCount = Object.keys(diff).length + (uiPrefsDirty ? 1 : 0)

  const handleSave = () => {
    if (!dirty || !form) return
    // 先把 UI 排版偏好落地到 localStorage(纯前端,即时生效)。
    if (uiPrefsDirty) {
      setUiPrefs({
        poolSort: form.poolSort,
        poolShowDisabled: form.poolShowDisabled,
        cardSize: form.cardSize,
      })
    }
    // 无后端字段改动时,仅 UI 排版改动:落地后直接提示成功,不打后端。
    if (Object.keys(diff).length === 0) {
      if (uiPrefsDirty) toast.success(t('settingspage.toast.uiPrefsSaved'))
      return
    }
    save(diff, {
      onSuccess: (resp) => {
        if (resp.restartRequired) {
          toast.warning(resp.message, {
            description: t('settingspage.toast.restartFields', { fields: resp.restartFields.join('、') }),
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
    if (config) setForm(toForm(config, uiPrefs))
  }

  if (isLoading || !form) {
    // 骨架屏替代蓝色转圈圈
    return <PageSkeleton kind="settings" />
  }

  if (error || !config) {
    return (
      <div className="flex items-center justify-center py-24">
        <Card className="w-full max-w-md">
          <CardContent className="pt-6 text-center">
            <div className="text-red-500 mb-4">{t('settingspage.page.loadFailed')}</div>
            <p className="text-muted-foreground mb-4">{error ? (error as Error).message : t('settingspage.page.noData')}</p>
            <Button onClick={() => refetch()}>{t('settingspage.common.retry')}</Button>
          </CardContent>
        </Card>
      </div>
    )
  }

  const inputCls = 'max-w-[260px] text-right'
  const hot = t('settingspage.hint.hotReload')
  const hotParen = t('settingspage.hint.hotReloadParen')
  const restart = t('settingspage.hint.restartRequired')
  const presetRestart = t('settingspage.hint.presetOrCustomRestart')
  const emDash = t('settingspage.common.emDash')

  return (
    <SearchContext.Provider value={{ query }}>
    <ActiveSectionContext.Provider value={activeSection}>
    <div className="space-y-6 pb-24">
      <div className="flex items-center justify-between gap-4">
        <h2 className="text-xl font-semibold text-gradient-brand">{t('settingspage.page.title')}</h2>
        <div className="flex items-center gap-2">
          {/* 搜索：跨区定位设置项，命中即高亮/过滤 */}
          <div className="relative">
            <Search className="pointer-events-none absolute left-2.5 top-1/2 h-4 w-4 -translate-y-1/2 text-muted-foreground" />
            <Input
              className="w-56 pl-8 pr-8"
              value={searchRaw}
              onChange={(e) => setSearchRaw(e.target.value)}
              placeholder={t('settingspage.page.searchPlaceholder')}
              aria-label={t('settingspage.page.searchAria')}
            />
            {searchRaw && (
              <button
                type="button"
                onClick={() => setSearchRaw('')}
                className="absolute right-2 top-1/2 -translate-y-1/2 text-muted-foreground hover:text-foreground"
                aria-label={t('settingspage.page.clearSearch')}
              >
                <X className="h-4 w-4" />
              </button>
            )}
          </div>
          <Button variant="outline" size="sm" onClick={() => refetch()} disabled={isSaving}>
            {t('settingspage.common.refresh')}
          </Button>
        </div>
      </div>

      {/* 分区导航 tab：搜索态下隐藏（改为跨区展示命中项） */}
      {!query && (
        <div className="flex flex-wrap gap-2 border-b pb-3">
          {SECTION_DEFS.map((s) => {
            const Icon = s.icon
            const isActive = s.id === activeSection
            return (
              <Button
                key={s.id}
                variant={isActive ? 'default' : 'outline'}
                size="sm"
                onClick={() => setActiveSection(s.id)}
              >
                <Icon className="mr-1.5 h-4 w-4" />
                {t(s.labelKey)}
              </Button>
            )
          })}
        </div>
      )}

      {/* 搜索态提示条 */}
      {query && (
        <p className="text-sm text-muted-foreground">
          {hasAnyMatch
            ? t('settingspage.page.searchMatch', { q: searchRaw.trim(), n: matchedSections.size })
            : t('settingspage.page.searchNone', { q: searchRaw.trim() })}
        </p>
      )}

      {/* 服务管理分区：一键重启 */}
      <SectionGate section="service" titleKey="settingspage.card.service" kwKey="settingspage.card.service.kw">
        <ServiceManagementCard />
      </SectionGate>
      {/* 存储分区：占用统计 + 清理 */}
      <SectionGate section="storage" titleKey="settingspage.card.storage" kwKey="settingspage.card.storage.kw">
        <StorageStatsCard />
      </SectionGate>
      {/* 服务管理分区：客户端 RPM */}
      <SectionGate section="service" titleKey="settingspage.card.clientRpm" kwKey="settingspage.card.clientRpm.kw">
        <ClientRpmCard />
      </SectionGate>

      {/* 隐私分区：客户端指纹采集开关（立即生效） */}
      <SectionGate section="privacy" titleKey="settingspage.card.privacy" kwKey="settingspage.card.privacy.kw">
        <PrivacyCard />
      </SectionGate>

      {/* UI 排版自定义分区：号池状态排序/禁用号显隐 + 凭据卡片尺寸。纯前端 localStorage,纳入统一保存流程(切换点亮保存,点保存才落地)。 */}
      <SectionGate section="appearance" titleKey="settingspage.card.appearance" kwKey="settingspage.card.appearance.kw">
        <Field label={t('settingspage.appearance.poolSort.label')} hint={t('settingspage.appearance.poolSort.hint')}>
          <SegChoice
            value={form.poolSort}
            onChange={(v) => set('poolSort', v as PoolSortMode)}
            options={[
              { value: 'health', label: t('settingspage.appearance.poolSort.health') },
              { value: 'sequence', label: t('settingspage.appearance.poolSort.sequence') },
              { value: 'concurrency', label: t('settingspage.appearance.poolSort.concurrency') },
              { value: 'lastUsed', label: t('settingspage.appearance.poolSort.lastUsed') },
            ]}
          />
        </Field>
        <Field label={t('settingspage.appearance.poolShowDisabled.label')} hint={t('settingspage.appearance.poolShowDisabled.hint')}>
          <Switch
            checked={form.poolShowDisabled}
            onCheckedChange={(v) => set('poolShowDisabled', v)}
          />
        </Field>
        <Field label={t('settingspage.appearance.cardSize.label')} hint={t('settingspage.appearance.cardSize.hint')}>
          <SegChoice
            value={form.cardSize}
            onChange={(v) => set('cardSize', v as CardSize)}
            options={[
              { value: 'compact', label: t('settingspage.appearance.cardSize.compact') },
              { value: 'standard', label: t('settingspage.appearance.cardSize.standard') },
              { value: 'large', label: t('settingspage.appearance.cardSize.large') },
            ]}
          />
        </Field>
      </SectionGate>

      {/* 令牌导出分区：单个 / 全部凭据 JSON 下载 */}
      <SectionGate section="export" titleKey="settingspage.card.export" kwKey="settingspage.card.export.kw">
        <TokenExportCard />
      </SectionGate>

      {/* 回收站分区：已删除凭据的恢复 / 永久清除 */}
      <SectionGate section="trash" titleKey="settingspage.card.trash" kwKey="settingspage.card.trash.kw">
        <TrashCard />
      </SectionGate>

      {/* 调度分区：负载均衡（立即生效） */}
      <SectionGate section="scheduling" titleKey="settingspage.card.loadBalance" kwKey="settingspage.card.loadBalance.kw">
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base"><Highlight text={t('settingspage.card.loadBalance')} /></CardTitle>
        </CardHeader>
        <CardContent className="space-y-3">
          <p className="text-sm text-muted-foreground">
            {t('settingspage.loadBalance.desc')}
          </p>
          <div className="flex gap-2">
            <Button
              variant={form.loadBalancingMode === 'priority' ? 'default' : 'outline'}
              size="sm"
              onClick={() => set('loadBalancingMode', 'priority')}
            >
              {t('settingspage.loadBalance.priority')}
            </Button>
            <Button
              variant={form.loadBalancingMode === 'balanced' ? 'default' : 'outline'}
              size="sm"
              onClick={() => set('loadBalancingMode', 'balanced')}
            >
              {t('settingspage.loadBalance.balanced')}
            </Button>
          </div>
          <Field
            label={t('settingspage.loadBalance.priorityInBalanced.label')}
            hint={t('settingspage.loadBalance.priorityInBalanced.hint')}
          >
            <Switch
              checked={form.priorityInBalanced}
              onCheckedChange={(v) => set('priorityInBalanced', v)}
              disabled={form.loadBalancingMode !== 'balanced'}
            />
          </Field>
        </CardContent>
      </Card>
      </SectionGate>

      {/* 智能调度分区（余额加权 / RPM headroom / 背压 / 429 感知，全部热更即时生效） */}
      <SectionGate section="scheduling" titleKey="settingspage.card.smartSchedule" kwKey="settingspage.card.smartSchedule.kw">
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base"><Highlight text={t('settingspage.card.smartSchedule')} /></CardTitle>
        </CardHeader>
        <CardContent className="py-0">
          <p className="mb-3 mt-1 text-sm text-muted-foreground">
            {t('settingspage.smart.desc')}
          </p>
          <Field
            label={t('settingspage.smart.balanceWeight.label')}
            hint={t('settingspage.smart.balanceWeight.hint')}
          >
            <Switch
              checked={form.balanceWeightEnabled}
              onCheckedChange={(v) => set('balanceWeightEnabled', v)}
            />
          </Field>
          <Field
            label={t('settingspage.smart.balanceWeightFloor.label')}
            hint={t('settingspage.smart.balanceWeightFloor.hint')}
          >
            <NumberStepper
              value={Number(form.balanceWeightFloor) || 0}
              onChange={(v) => set('balanceWeightFloor', String(v))}
              min={0}
              max={100}
              step={5}
              className="w-28"
              aria-label={t('settingspage.smart.balanceWeightFloor.aria')}
              disabled={!form.balanceWeightEnabled}
            />
          </Field>
          <Field
            label={t('settingspage.smart.health429.label')}
            hint={t('settingspage.smart.health429.hint')}
          >
            <Switch
              checked={form.health429WeightEnabled}
              onCheckedChange={(v) => set('health429WeightEnabled', v)}
            />
          </Field>
          {/* RPM 相关：行尾齿轮点开「RPM 卡」，含全局软上限 + headroom + 预留 + 背压 */}
          <Field
            label={t('settingspage.smart.headroom.label')}
            hint={t('settingspage.smart.headroom.hint')}
          >
            <div className="flex items-center gap-1.5">
              <NumberStepper
                value={Number(form.rpmHeadroomFactor) || 0}
                onChange={(v) => set('rpmHeadroomFactor', String(v))}
                min={0}
                max={100}
                step={5}
                className="w-28"
                aria-label={t('settingspage.smart.headroom.aria')}
              />
              <SettingGearCard
                title={t('settingspage.smart.gear.rpmTitle')}
                description={t('settingspage.smart.gear.rpmDesc')}
              >
                <SearchContext.Provider value={{ query: '' }}>
                  <Field
                    label={t('settingspage.smart.credRpm.label')}
                    hint={t('settingspage.smart.credRpm.hint')}
                  >
                    <NumberStepper
                      value={Number(form.credentialRpmLimit) || 0}
                      onChange={(v) => set('credentialRpmLimit', String(v))}
                      min={0}
                      max={10000}
                      step={5}
                      className="w-28"
                      aria-label={t('settingspage.smart.credRpm.aria')}
                    />
                  </Field>
                  <Field
                    label={t('settingspage.smart.headroom.label')}
                    hint={t('settingspage.smart.headroom.hintGear')}
                  >
                    <NumberStepper
                      value={Number(form.rpmHeadroomFactor) || 0}
                      onChange={(v) => set('rpmHeadroomFactor', String(v))}
                      min={0}
                      max={100}
                      step={5}
                      className="w-28"
                      aria-label={t('settingspage.smart.headroom.aria')}
                    />
                  </Field>
                  <Field
                    label={t('settingspage.smart.reserve.label')}
                    hint={t('settingspage.smart.reserve.hint')}
                  >
                    <NumberStepper
                      value={Number(form.rpmReserveSlots) || 0}
                      onChange={(v) => set('rpmReserveSlots', String(v))}
                      min={0}
                      max={1000}
                      className="w-28"
                      aria-label={t('settingspage.smart.reserve.aria')}
                    />
                  </Field>
                  <Field
                    label={t('settingspage.smart.hardGate.label')}
                    hint={t('settingspage.smart.hardGate.hintGear')}
                  >
                    <Switch
                      checked={form.rpmHardGateOverloadWait}
                      onCheckedChange={(v) => set('rpmHardGateOverloadWait', v)}
                    />
                  </Field>
                </SearchContext.Provider>
              </SettingGearCard>
              <SettingGearCard
                title={t('settingspage.smart.gear.inboundTitle', {
                  rpm: config?.inboundCurrentRpm ?? emDash,
                })}
                description={t('settingspage.smart.gear.inboundDesc')}
              >
                <SearchContext.Provider value={{ query: '' }}>
                  <Field label={t('settingspage.smart.inboundEnabled.label')} hint={t('settingspage.smart.inboundEnabled.hint')}>
                    <Switch checked={form.inboundThrottleEnabled} onCheckedChange={(v) => set('inboundThrottleEnabled', v)} />
                  </Field>
                  <Field label={t('settingspage.smart.inboundAuto.label')} hint={t('settingspage.smart.inboundAuto.hint')}>
                    <Switch checked={form.inboundRpmAuto} onCheckedChange={(v) => set('inboundRpmAuto', v)} />
                  </Field>
                  <Field label={t('settingspage.smart.targetRpm.label')} hint={t('settingspage.smart.targetRpm.hint')}>
                    <NumberStepper value={Number(form.inboundTargetRpm) || 0} onChange={(v) => set('inboundTargetRpm', String(v))} min={1} max={10000} step={10} className="w-28" aria-label={t('settingspage.smart.targetRpm.aria')} />
                  </Field>
                  <Field label={t('settingspage.smart.inboundMin.label')} hint={t('settingspage.smart.inboundMin.hint')}>
                    <NumberStepper value={Number(form.inboundRpmMin) || 0} onChange={(v) => set('inboundRpmMin', String(v))} min={1} max={10000} step={5} className="w-28" aria-label={t('settingspage.smart.inboundMin.aria')} />
                  </Field>
                  <Field label={t('settingspage.smart.inboundMax.label')} hint={t('settingspage.smart.inboundMax.hint')}>
                    <NumberStepper value={Number(form.inboundRpmMax) || 0} onChange={(v) => set('inboundRpmMax', String(v))} min={1} max={10000} step={10} className="w-28" aria-label={t('settingspage.smart.inboundMax.aria')} />
                  </Field>
                  <Field label={t('settingspage.smart.burstSecs.label')} hint={t('settingspage.smart.burstSecs.hint')}>
                    <NumberStepper value={Number(form.inboundBurstSecs) || 0} onChange={(v) => set('inboundBurstSecs', String(v))} min={1} max={60} className="w-28" aria-label={t('settingspage.smart.burstSecs.aria')} />
                  </Field>
                  <Field label={t('settingspage.smart.queueWait.label')} hint={t('settingspage.smart.queueWait.hint')}>
                    <NumberStepper value={Number(form.inboundQueueMaxWaitSecs) || 0} onChange={(v) => set('inboundQueueMaxWaitSecs', String(v))} min={1} max={300} step={5} className="w-28" aria-label={t('settingspage.smart.queueWait.aria')} />
                  </Field>
                  <Field
                    label={t('settingspage.smart.queueTimeout.label')}
                    hint={t('settingspage.smart.queueTimeout.hint')}
                  >
                    <Switch checked={form.inboundQueueTimeoutPassthrough} onCheckedChange={(v) => set('inboundQueueTimeoutPassthrough', v)} />
                  </Field>
                </SearchContext.Provider>
              </SettingGearCard>
            </div>
          </Field>
          <Field
            label={t('settingspage.smart.reserve.label')}
            hint={t('settingspage.smart.reserve.hint')}
          >
            <NumberStepper
              value={Number(form.rpmReserveSlots) || 0}
              onChange={(v) => set('rpmReserveSlots', String(v))}
              min={0}
              max={1000}
              className="w-28"
              aria-label={t('settingspage.smart.reserve.aria')}
            />
          </Field>
          <Field
            label={t('settingspage.smart.hardGate.label')}
            hint={t('settingspage.smart.hardGate.hint')}
          >
            <Switch
              checked={form.rpmHardGateOverloadWait}
              onCheckedChange={(v) => set('rpmHardGateOverloadWait', v)}
            />
          </Field>
        </CardContent>
      </Card>
      </SectionGate>

      {/* 基础分区：服务信息（需重启） */}
      <SectionGate section="basic" titleKey="settingspage.card.serviceInfo" kwKey="settingspage.card.serviceInfo.kw">
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base"><Highlight text={t('settingspage.card.serviceInfo')} /></CardTitle>
        </CardHeader>
        <CardContent className="py-0">
          <Field label={t('settingspage.basic.host.label')} hint={restart}>
            <Input className={inputCls} value={form.host} onChange={(e) => set('host', e.target.value)} />
          </Field>
          <Field label={t('settingspage.basic.port.label')} hint={restart}>
            <NumberStepper value={Number(form.port) || 0} onChange={(v) => set('port', String(v))} min={1} max={65535} className="w-28" aria-label={t('settingspage.basic.port.aria')} />
          </Field>
          <Field label={t('settingspage.basic.region.label')} hint={restart}>
            <div className="w-[260px]">
              <RegionSelect value={form.region} onChange={(v) => set('region', v)} />
            </div>
          </Field>
          {/* TLS 后端固定为 rustls：出厂构建纯 rustls（见 build.bat / release.yml），
              native-tls 已废弃（曾误导用户切换后废网关）。仅作只读展示，不再可切换。 */}
          <ReadonlyRow label={t('settingspage.basic.tlsBackend')} value={t('settingspage.basic.tlsBackendValue')} />
          <Field label={t('settingspage.basic.defaultEndpoint.label')} hint={t('settingspage.basic.defaultEndpoint.hint', { names: config.endpointNames.join(', ') || emDash })}>
            <Input className={inputCls} value={form.defaultEndpoint} onChange={(e) => set('defaultEndpoint', e.target.value)} />
          </Field>
          {config.configPath && <ReadonlyRow label={t('settingspage.basic.configPath')} value={config.configPath} mono />}
        </CardContent>
      </Card>
      </SectionGate>

      {/* 基础分区：客户端伪装（需重启） */}
      <SectionGate section="basic" titleKey="settingspage.card.clientSpoof" kwKey="settingspage.card.clientSpoof.kw">
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base"><Highlight text={t('settingspage.card.clientSpoof')} /></CardTitle>
        </CardHeader>
        <CardContent className="py-0">
          <Field label={t('settingspage.spoof.kiro.label')} hint={presetRestart}>
            <ComboInput className={inputCls} value={form.kiroVersion} onChange={(v) => set('kiroVersion', v)} options={KIRO_VERSION_PRESETS} aria-label={t('settingspage.spoof.kiro.aria')} />
          </Field>
          <Field label={t('settingspage.spoof.system.label')} hint={presetRestart}>
            <ComboInput className={inputCls} value={form.systemVersion} onChange={(v) => set('systemVersion', v)} options={SYSTEM_VERSION_PRESETS} aria-label={t('settingspage.spoof.system.aria')} />
          </Field>
          <Field label={t('settingspage.spoof.node.label')} hint={presetRestart}>
            <ComboInput className={inputCls} value={form.nodeVersion} onChange={(v) => set('nodeVersion', v)} options={NODE_VERSION_PRESETS} aria-label={t('settingspage.spoof.node.aria')} />
          </Field>
        </CardContent>
      </Card>
      </SectionGate>

      <SectionGate section="basic" titleKey="settingspage.card.protocol" kwKey="settingspage.card.protocol.kw">
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base"><Highlight text={t('settingspage.card.protocol')} /></CardTitle>
        </CardHeader>
        <CardContent className="py-0">
          <Field label={t('settingspage.protocol.extractThinking.label')} hint={`${t('settingspage.protocol.extractThinking.hint')}${hotParen}`}>
            <Switch checked={form.extractThinking} onCheckedChange={(v) => set('extractThinking', v)} />
          </Field>
          <Field label={t('settingspage.protocol.ccAuto.label')} hint={`${t('settingspage.protocol.ccAuto.hint')}${hotParen}`}>
            <Switch checked={form.ccAutoBuffer} onCheckedChange={(v) => set('ccAutoBuffer', v)} />
          </Field>
          <Field label={t('settingspage.protocol.stripEnv.label')} hint={`${t('settingspage.protocol.stripEnv.hint')}${hotParen}`}>
            <Switch checked={form.stripEnvNoise} onCheckedChange={(v) => set('stripEnvNoise', v)} />
          </Field>
        </CardContent>
      </Card>
      </SectionGate>

      <SectionGate section="basic" titleKey="settingspage.card.toolFault" kwKey="settingspage.card.toolFault.kw">
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base"><Highlight text={t('settingspage.card.toolFault')} /></CardTitle>
          <p className="text-xs text-muted-foreground">{t('settingspage.tool.cardSubtitle')}</p>
        </CardHeader>
        <CardContent className="py-0">
          <Field label={t('settingspage.tool.repairJson.label')} hint={t('settingspage.tool.repairJson.hint')}>
            <Switch checked={form.toolRepairJson} onCheckedChange={(v) => set('toolRepairJson', v)} />
          </Field>
          <Field label={t('settingspage.tool.streamAlign.label')} hint={t('settingspage.tool.streamAlign.hint')}>
            <Switch checked={form.toolStreamAlignFailure} onCheckedChange={(v) => set('toolStreamAlignFailure', v)} />
          </Field>
          <Field label={t('settingspage.tool.exposeError.label')} hint={t('settingspage.tool.exposeError.hint')}>
            <Switch checked={form.toolExposeErrorToClient} onCheckedChange={(v) => set('toolExposeErrorToClient', v)} />
          </Field>
          <Field label={t('settingspage.tool.cleanLeaked.label')} hint={t('settingspage.tool.cleanLeaked.hint')}>
            <Switch checked={form.toolCleanLeakedTokens} onCheckedChange={(v) => set('toolCleanLeakedTokens', v)} />
          </Field>
          <Field label={t('settingspage.tool.reclaim.label')} hint={t('settingspage.tool.reclaim.hint')}>
            <Switch checked={form.toolReclaimTextifiedInvoke} onCheckedChange={(v) => set('toolReclaimTextifiedInvoke', v)} />
          </Field>
          <Field label={t('settingspage.tool.strayGuard.label')} hint={t('settingspage.tool.strayGuard.hint')}>
            <Switch checked={form.toolStrayRepeatGuard} onCheckedChange={(v) => set('toolStrayRepeatGuard', v)} />
          </Field>
          <Field label={t('settingspage.tool.truncation.label')} hint={t('settingspage.tool.truncation.hint')}>
            <Switch checked={form.toolTruncationRecovery} onCheckedChange={(v) => set('toolTruncationRecovery', v)} />
          </Field>
          <Field label={t('settingspage.tool.descMax.label')} hint={t('settingspage.tool.descMax.hint')}>
            <NumberStepper value={Number(form.toolDescriptionMaxChars) || 0} onChange={(v) => set('toolDescriptionMaxChars', String(v))} min={0} step={1000} className="w-32" aria-label={t('settingspage.tool.descMax.aria')} />
          </Field>
        </CardContent>
      </Card>
      </SectionGate>

      {/* 调度分区：防关联 / 限流（需重启） */}
      <SectionGate section="scheduling" titleKey="settingspage.card.antiAssoc" kwKey="settingspage.card.antiAssoc.kw">
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base"><Highlight text={t('settingspage.card.antiAssoc')} /></CardTitle>
        </CardHeader>
        <CardContent className="py-0">
          {/* 冷却机制：行尾齿轮点开「冷却卡」做细粒度设置 */}
          <Field label={t('settingspage.anti.cooldown.label')} hint={`${t('settingspage.anti.cooldown.hint')}${hotParen}`}>
            <div className="flex items-center gap-1.5">
              <Switch checked={form.cooldownEnabled} onCheckedChange={(v) => set('cooldownEnabled', v)} />
              <SettingGearCard
                title={t('settingspage.anti.gear.cooldownTitle')}
                description={t('settingspage.anti.gear.cooldownDesc')}
              >
                <SearchContext.Provider value={{ query: '' }}>
                  <Field label={t('settingspage.anti.cooldownEnabled.label')} hint={`${t('settingspage.anti.cooldownEnabled.hint')}${hotParen}`}>
                    <Switch checked={form.cooldownEnabled} onCheckedChange={(v) => set('cooldownEnabled', v)} />
                  </Field>
                  <Field
                    label={t('settingspage.anti.cooldownScale.label')}
                    hint={t('settingspage.anti.cooldownScale.hint')}
                  >
                    <NumberStepper
                      value={Number(form.cooldownScalePct) || 0}
                      onChange={(v) => set('cooldownScalePct', String(v))}
                      min={10}
                      max={500}
                      step={10}
                      className="w-28"
                      disabled={!form.cooldownEnabled}
                      aria-label={t('settingspage.anti.cooldownScale.aria')}
                    />
                  </Field>
                </SearchContext.Provider>
              </SettingGearCard>
            </div>
          </Field>
          {/* 全池冷却快速失败：全池都在冷却时立即 429 让客户端退避，还是网关内短等重试 */}
          <Field
            label={t('settingspage.anti.allCooling.label')}
            hint={`${t('settingspage.anti.allCooling.hint')}${hotParen}`}
          >
            <Switch checked={form.allCoolingFastFail} onCheckedChange={(v) => set('allCoolingFastFail', v)} />
          </Field>
          {/* 速率限制：行尾齿轮点开「速率卡」做细粒度设置 */}
          <Field label={t('settingspage.anti.rateLimit.label')} hint={`${t('settingspage.anti.rateLimit.hint')}${hotParen}`}>
            <div className="flex items-center gap-1.5">
              <Switch checked={form.rateLimitEnabled} onCheckedChange={(v) => set('rateLimitEnabled', v)} />
              <SettingGearCard
                title={t('settingspage.anti.gear.rateTitle')}
                description={t('settingspage.anti.gear.rateDesc')}
              >
                <SearchContext.Provider value={{ query: '' }}>
                  <Field label={t('settingspage.anti.rateLimitEnabled.label')} hint={`${t('settingspage.anti.rateLimitEnabled.hint')}${hotParen}`}>
                    <Switch checked={form.rateLimitEnabled} onCheckedChange={(v) => set('rateLimitEnabled', v)} />
                  </Field>
                  <Field label={t('settingspage.anti.dailyMax.label')} hint={`${t('settingspage.anti.dailyMax.hint')}${hotParen}`}>
                    <NumberStepper value={Number(form.rateLimitDailyMax) || 0} onChange={(v) => set('rateLimitDailyMax', String(v))} min={0} step={10} className="w-28" disabled={!form.rateLimitEnabled} aria-label={t('settingspage.anti.dailyMax.aria')} />
                  </Field>
                  <Field label={t('settingspage.anti.minInterval.label')} hint={hot}>
                    <NumberStepper value={Number(form.rateLimitMinIntervalMs) || 0} onChange={(v) => set('rateLimitMinIntervalMs', String(v))} min={0} step={100} className="w-28" disabled={!form.rateLimitEnabled} aria-label={t('settingspage.anti.minInterval.aria')} />
                  </Field>
                  <Field
                    label={t('settingspage.anti.jitter.label')}
                    hint={t('settingspage.anti.jitter.hint')}
                  >
                    <NumberStepper
                      value={Number(form.rateLimitJitterPct) || 0}
                      onChange={(v) => set('rateLimitJitterPct', String(v))}
                      min={0}
                      max={50}
                      step={5}
                      className="w-28"
                      disabled={!form.rateLimitEnabled}
                      aria-label={t('settingspage.anti.jitter.aria')}
                    />
                  </Field>
                </SearchContext.Provider>
              </SettingGearCard>
            </div>
          </Field>
          <Field label={t('settingspage.anti.dailyMax.label')} hint={`${t('settingspage.anti.dailyMax.hint')}${hotParen}`}>
            <NumberStepper value={Number(form.rateLimitDailyMax) || 0} onChange={(v) => set('rateLimitDailyMax', String(v))} min={0} step={10} className="w-28" disabled={!form.rateLimitEnabled} aria-label={t('settingspage.anti.dailyMax.aria')} />
          </Field>
          <Field label={t('settingspage.anti.minInterval.label')} hint={hot}>
            <NumberStepper value={Number(form.rateLimitMinIntervalMs) || 0} onChange={(v) => set('rateLimitMinIntervalMs', String(v))} min={0} step={100} className="w-28" disabled={!form.rateLimitEnabled} aria-label={t('settingspage.anti.minInterval.aria')} />
          </Field>
          <Field label={t('settingspage.anti.affinity.label')} hint={`${t('settingspage.anti.affinity.hint')}${hotParen}`}>
            <Switch checked={form.affinityEnabled} onCheckedChange={(v) => set('affinityEnabled', v)} />
          </Field>
        </CardContent>
      </Card>
      </SectionGate>

      {/* 基础分区：网络与上号（需重启） */}
      <SectionGate section="basic" titleKey="settingspage.card.network" kwKey="settingspage.card.network.kw">
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base"><Highlight text={t('settingspage.card.network')} /></CardTitle>
        </CardHeader>
        <CardContent className="py-0">
          <Field label={t('settingspage.network.proxy.label')} hint={t('settingspage.network.proxy.hint')}>
            <div className="flex items-center gap-2">
              <Input className="max-w-[260px] font-mono text-xs" value={form.proxyUrl} onChange={(e) => set('proxyUrl', e.target.value)} placeholder={t('settingspage.network.proxy.ph')} />
              <ProxyTestButton proxyUrl={form.proxyUrl} proxyUsername={form.proxyUsername} proxyPassword={form.proxyPassword} />
            </div>
          </Field>
          <Field label={t('settingspage.network.proxyUser.label')} hint={t('settingspage.network.proxyUser.hint')}>
            <Input className="max-w-[260px] font-mono text-xs" value={form.proxyUsername} onChange={(e) => set('proxyUsername', e.target.value)} placeholder={t('settingspage.network.proxyUser.ph')} autoComplete="off" />
          </Field>
          <Field label={t('settingspage.network.proxyPass.label')} hint={t('settingspage.network.proxyPass.hint')}>
            <Input type="password" className="max-w-[260px] font-mono text-xs" value={form.proxyPassword} onChange={(e) => set('proxyPassword', e.target.value)} placeholder={t('settingspage.network.proxyPass.ph')} autoComplete="new-password" />
          </Field>
          <Field
            label={t('settingspage.network.callback.label')}
            hint={t('settingspage.network.callback.hint')}
          >
            <Input className="max-w-[260px] font-mono text-xs" value={form.callbackBaseUrl} onChange={(e) => set('callbackBaseUrl', e.target.value)} placeholder="http://host:port" />
          </Field>
          <ReadonlyRow
            label={t('settingspage.network.callbackMode')}
            value={
              <Badge variant="outline">
                {config.callbackMode === 'remote' ? t('settingspage.network.callbackModeRemote') : t('settingspage.network.callbackModeLocal')}
              </Badge>
            }
          />
          <ReadonlyRow label={t('settingspage.network.adminKey')} value={<Badge variant={config.hasAdminKey ? 'default' : 'secondary'}>{config.hasAdminKey ? t('settingspage.common.set') : t('settingspage.common.unset')}</Badge>} />
          <Field
            label={t('settingspage.network.apiKey.label')}
            hint={t('settingspage.network.apiKey.hint')}
          >
            <div className="flex items-center gap-2">
              <Input
                type="password"
                className="flex-1 min-w-0 max-w-[260px] font-mono text-xs"
                value={form.apiKey}
                onChange={(e) => set('apiKey', e.target.value)}
                placeholder={config.hasApiKey ? t('settingspage.network.apiKey.phSet') : t('settingspage.network.apiKey.phUnset')}
                autoComplete="new-password"
              />
              <Badge variant={config.hasApiKey ? 'default' : 'secondary'} className="shrink-0 whitespace-nowrap">
                {config.hasApiKey ? t('settingspage.common.set') : t('settingspage.common.unset')}
              </Badge>
            </div>
          </Field>
        </CardContent>
      </Card>
      </SectionGate>

      {/* 基础分区：登录页背景（立即生效，无需重启） */}
      <SectionGate section="basic" titleKey="settingspage.card.loginBg" kwKey="settingspage.card.loginBg.kw">
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base flex items-center gap-2">
            <ImageIcon className="h-4 w-4 text-muted-foreground" />
            <Highlight text={t('settingspage.card.loginBg')} />
          </CardTitle>
        </CardHeader>
        <CardContent className="py-0">
          <Field
            label={t('settingspage.loginBg.enabled.label')}
            hint={`${t('settingspage.loginBg.enabled.hint')}${hotParen}`}
          >
            <Switch checked={form.loginBackgroundEnabled} onCheckedChange={(v) => set('loginBackgroundEnabled', v)} />
          </Field>
          <Field
            label={t('settingspage.loginBg.r18.label')}
            hint={t('settingspage.loginBg.r18.hint')}
          >
            <Switch
              checked={form.loginBackgroundR18}
              onCheckedChange={(v) => set('loginBackgroundR18', v)}
              disabled={!form.loginBackgroundEnabled}
            />
          </Field>
        </CardContent>
      </Card>
      </SectionGate>

      {/* 安全分区：反代安全（需重启） */}
      <SectionGate section="security" titleKey="settingspage.card.security" kwKey="settingspage.card.security.kw">
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base"><Highlight text={t('settingspage.card.security')} /></CardTitle>
        </CardHeader>
        <CardContent className="py-0">
          <Field
            label={t('settingspage.security.encrypt.label')}
            hint={t('settingspage.security.encrypt.hint')}
          >
            <Switch
              checked={form.encryptCredentialsAtRest}
              onCheckedChange={(v) => set('encryptCredentialsAtRest', v)}
            />
          </Field>
          <Field
            label={t('settingspage.security.cors.label')}
            hint={t('settingspage.security.cors.hint')}
          >
            <textarea
              className="flex min-h-[72px] w-full max-w-[260px] rounded-md border border-input bg-background px-3 py-2 font-mono text-xs ring-offset-background placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
              value={form.corsAllowedOrigins}
              onChange={(e) => set('corsAllowedOrigins', e.target.value)}
              placeholder={t('settingspage.security.cors.ph')}
              spellCheck={false}
            />
          </Field>
          <Field
            label={t('settingspage.security.ip.label')}
            hint={t('settingspage.security.ip.hint')}
          >
            <textarea
              className="flex min-h-[72px] w-full max-w-[260px] rounded-md border border-input bg-background px-3 py-2 font-mono text-xs ring-offset-background placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
              value={form.ipAllowlist}
              onChange={(e) => set('ipAllowlist', e.target.value)}
              placeholder={t('settingspage.security.ip.ph')}
              spellCheck={false}
            />
          </Field>
          <Field
            label={t('settingspage.security.ipBlock.label')}
            hint={t('settingspage.security.ipBlock.hint')}
          >
            <textarea
              className="flex min-h-[72px] w-full max-w-[260px] rounded-md border border-input bg-background px-3 py-2 font-mono text-xs ring-offset-background placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
              value={form.ipBlocklist}
              onChange={(e) => set('ipBlocklist', e.target.value)}
              placeholder={t('settingspage.security.ipBlock.ph')}
              spellCheck={false}
            />
          </Field>
          <Field
            label={t('settingspage.security.mcBlock.label')}
            hint={t('settingspage.security.mcBlock.hint')}
          >
            <textarea
              className="flex min-h-[72px] w-full max-w-[260px] rounded-md border border-input bg-background px-3 py-2 font-mono text-xs ring-offset-background placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
              value={form.machineCodeBlocklist}
              onChange={(e) => set('machineCodeBlocklist', e.target.value)}
              placeholder={t('settingspage.security.mcBlock.ph')}
              spellCheck={false}
            />
          </Field>
          <Field
            label={t('settingspage.security.xff.label')}
            hint={t('settingspage.security.xff.hint')}
          >
            <Switch checked={form.trustForwardedHeader} onCheckedChange={(v) => set('trustForwardedHeader', v)} />
          </Field>
          <Field label={t('settingspage.security.ingress.label')} hint={t('settingspage.security.ingress.hint')}>
            <NumberStepper value={Number(form.ingressRateLimitPerMin) || 0} onChange={(v) => set('ingressRateLimitPerMin', String(v))} min={0} step={10} className="w-28" aria-label={t('settingspage.security.ingress.aria')} />
          </Field>
          <Field label={t('settingspage.security.maxBody.label')} hint={t('settingspage.security.maxBody.hint')}>
            <NumberStepper value={Number(form.maxBodyBytes) || 0} onChange={(v) => set('maxBodyBytes', String(v))} min={0} step={1048576} className="w-40" aria-label={t('settingspage.security.maxBody.aria')} />
          </Field>
        </CardContent>
      </Card>
      </SectionGate>

      {/* 调度分区：主动 token 预刷新（TIER2 热重载：保存即时生效，无需重启） */}
      <SectionGate section="scheduling" titleKey="settingspage.card.tokenRefresh" kwKey="settingspage.card.tokenRefresh.kw">
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base"><Highlight text={t('settingspage.card.tokenRefresh')} /></CardTitle>
        </CardHeader>
        <CardContent className="py-0">
          <Field label={t('settingspage.refresh.enable.label')} hint={`${t('settingspage.refresh.enable.hint')}${hotParen}`}>
            <Switch checked={form.proactiveTokenRefresh} onCheckedChange={(v) => set('proactiveTokenRefresh', v)} />
          </Field>
          <Field label={t('settingspage.refresh.lead.label')} hint={`${t('settingspage.refresh.lead.hint')}${hotParen}`}>
            <NumberStepper value={Number(form.tokenRefreshLeadMinutes) || 0} onChange={(v) => set('tokenRefreshLeadMinutes', String(v))} min={0} className="w-28" disabled={!form.proactiveTokenRefresh} aria-label={t('settingspage.refresh.lead.aria')} />
          </Field>
          <Field label={t('settingspage.refresh.interval.label')} hint={`${t('settingspage.refresh.interval.hint')}${hotParen}`}>
            <NumberStepper value={Number(form.tokenRefreshIntervalSecs) || 0} onChange={(v) => set('tokenRefreshIntervalSecs', String(v))} min={5} step={5} className="w-28" disabled={!form.proactiveTokenRefresh} aria-label={t('settingspage.refresh.interval.aria')} />
          </Field>
        </CardContent>
      </Card>
      </SectionGate>

      {/* 搜索且全无匹配时的空态 */}
      {query && !hasAnyMatch && (
        <div className="py-16 text-center text-sm text-muted-foreground">
          {t('settingspage.page.emptySearch')}
        </div>
      )}

      {!query && (
        <p className="text-xs text-muted-foreground">
          {t('settingspage.page.footerNote')}
        </p>
      )}

      {/* 底部保存栏：仅覆盖 main 内容区（left-[240px] 避开 240px 侧栏，
          否则会盖住侧栏底部“网关在线”状态条造成重叠）；z-30 低于侧栏 z-40。 */}
      <div className="fixed bottom-0 left-0 right-0 z-30 border-t bg-background/95 px-6 py-3 backdrop-blur md:left-[240px]">
        <div className="mx-auto flex max-w-[1200px] items-center justify-end gap-3">
          <span className="mr-auto text-sm text-muted-foreground">
            {dirty ? t('settingspage.page.dirtyCount', { n: dirtyCount }) : t('settingspage.page.noChanges')}
          </span>
          <Button variant="outline" onClick={handleReset} disabled={!dirty || isSaving}>
            {t('settingspage.common.undo')}
          </Button>
          <Button onClick={handleSave} disabled={!dirty || isSaving}>
            {isSaving ? t('settingspage.common.saving') : t('settingspage.common.save')}
          </Button>
        </div>
      </div>
    </div>
    </ActiveSectionContext.Provider>
    </SearchContext.Provider>
  )
}
