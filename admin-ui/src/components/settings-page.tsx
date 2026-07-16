import { createContext, useContext, useEffect, useMemo, useState } from 'react'
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

/* ============ 分区导航 + 搜索 基础设施 ============ */

// 设置分区（顶部 tab）。id 用于 tab 切换与卡片归属；label 为中文标题。
type SectionId = 'basic' | 'security' | 'scheduling' | 'storage' | 'service' | 'privacy' | 'appearance' | 'export' | 'trash'

const SECTIONS: { id: SectionId; label: string; icon: React.ComponentType<{ className?: string }> }[] = [
  { id: 'basic', label: '基础', icon: SlidersHorizontal },
  { id: 'security', label: '安全', icon: ShieldCheck },
  { id: 'scheduling', label: '调度', icon: Activity },
  { id: 'storage', label: '存储', icon: Database },
  { id: 'service', label: '服务管理', icon: Server },
  { id: 'privacy', label: '隐私', icon: Fingerprint },
  { id: 'appearance', label: 'UI 排版', icon: LayoutGrid },
  { id: 'export', label: '令牌导出', icon: Download },
  { id: 'trash', label: '回收站', icon: Trash2 },
]

// 每张卡片的可搜索索引（标题 + 关键词）。既驱动搜索命中，也用于判断“无匹配”空态。
// keywords 覆盖该卡片内所有设置项的文案，保证按关键词能跨区定位。
const CARD_INDEX: { section: SectionId; title: string; keywords: string[] }[] = [
  { section: 'basic', title: '服务信息', keywords: ['监听地址', 'host', '端口', 'port', '区域', 'region', 'tls 后端', 'rustls', '默认 endpoint', '配置文件'] },
  { section: 'basic', title: '客户端伪装', keywords: ['kiro 版本', '系统版本', 'node 版本', '伪装', '版本号'] },
  { section: 'basic', title: '协议与转发', keywords: ['提取 thinking', 'thinking', 'claude code 自动切协议', 'cc_auto_buffer', '缓冲分发', '剥离环境噪音', 'env', 'git', '省 token', '缓存'] },
  { section: 'basic', title: '工具调用容错', keywords: ['invalid tool parameters', '工具拼装非法', '工具错误', '失败态对齐', 'json 修复', '修复层', '修非法转义', '清洗泄漏控制 token', '截断跨轮恢复', '工具描述字符上限', 'tool_repair', 'tool_truncation'] },
  { section: 'basic', title: '网络与上号', keywords: ['全局代理', 'proxy', '上号回调地址', 'callback', '回调模式', 'admin key'] },
  { section: 'basic', title: '登录页背景', keywords: ['登录背景图', '登录页背景', '背景图', 'r18', '图源', 'lolicon', '关闭登录背景图'] },
  { section: 'security', title: '反代安全', keywords: ['cors 允许来源', 'ip 白名单', 'cidr', '信任 x-forwarded-for', 'xff', '入口限流', '请求体上限', '413', '429'] },
  { section: 'scheduling', title: '负载均衡模式', keywords: ['负载均衡', '优先级模式', '均衡负载', 'priority', 'balanced'] },
  { section: 'scheduling', title: '防关联 / 限流', keywords: ['冷却机制', '速率限制', '每日上限', '最小请求间隔', '会话亲和性', 'affinity', '冷却时长缩放', '间隔抖动', 'jitter', '拟人'] },
  { section: 'scheduling', title: '主动 token 预刷新', keywords: ['启用预刷新', '提前量', '扫描间隔', 'token 刷新'] },
  { section: 'storage', title: '存储占用', keywords: ['存储', '清理', '落盘', '分区', 'traces', 'usage', 'trash', 'bg_cache', '磁盘'] },
  { section: 'service', title: '服务管理', keywords: ['一键重启', '重启服务', 'restart'] },
  { section: 'service', title: '客户端 RPM', keywords: ['客户端 rpm', '窗口', 'session', '吞吐', '活跃客户端'] },
  { section: 'privacy', title: '隐私 / 客户端指纹', keywords: ['采集下游客户端指纹', '设备', 'ip', '系统', '浏览器', '隐私', 'fingerprint'] },
  { section: 'appearance', title: 'UI 排版自定义', keywords: ['ui 排版', '排版', '号池排序', '卡片大小', '卡片尺寸', '禁用号', '布局', 'layout', '排序模式'] },
  { section: 'export', title: '令牌导出', keywords: ['令牌导出', '凭据 json', '导出单个', '导出全部', 'token 导出', 'export'] },
  { section: 'trash', title: '回收站', keywords: ['回收站', '已删除', '清空', '恢复', 'trash'] },
]

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
  title,
  keywords = [],
  children,
}: {
  section: SectionId
  title: string
  keywords?: string[]
  children: React.ReactNode
}) {
  const { query } = useContext(SearchContext)
  const active = useContext(ActiveSectionContext)
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
        toast.success(resp.message || '重启中，约 3 秒后自动恢复')
        setConfirmOpen(false)
      },
      onError: () => {
        // 连接被重启中断而抛错属预期，仍提示已发起
        toast.warning('重启中，约 3 秒后自动恢复（本次连接已中断，属正常）')
        setConfirmOpen(false)
      },
    })
  }

  const handleCheck = () => {
    checkUpd(undefined, {
      onSuccess: (r) => {
        if (r.error) toast.error(`检查更新失败：${r.error}`)
        else if (r.has_update) toast.success(`发现新版本 ${r.latest_version}（当前 ${r.local_version}）`)
        else toast.success(`已是最新版本 ${r.local_version}`)
      },
      onError: (e) => toast.error(`检查更新失败：${(e as Error).message}`),
    })
  }

  const handleUpgrade = () => {
    performUpd(undefined, {
      onSuccess: (r) => {
        toast.success(r.message || '升级中，数秒后自动重启恢复')
        setUpgradeConfirm(false)
      },
      onError: () => {
        // 升级成功后会自动重启导致本次连接中断，抛错也当"已发起"
        toast.warning('升级已发起，若成功将自动重启（本次连接可能中断，属正常）')
        setUpgradeConfirm(false)
      },
    })
  }

  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="text-base flex items-center gap-2">
          <Server className="h-4 w-4 text-muted-foreground" />
          <Highlight text="服务管理" />
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-3">
        <p className="text-sm text-muted-foreground">
          一键重启网关服务。重启瞬间会短暂断服数秒，期间请求失败——若有客户端正经由本网关，务必确认无进行中的关键请求。重启后约 3 秒自动恢复。
        </p>
        <Button variant="destructive" size="sm" onClick={() => setConfirmOpen(true)} disabled={isPending}>
          <RotateCcw className="mr-1.5 h-4 w-4" />
          一键重启服务
        </Button>

        {/* OTA 更新：检查 GitHub 最新版本 + 一键升级（多镜像回退 + sha256 校验 + 换二进制 + 自动重启） */}
        <div className="border-t pt-3 space-y-2">
          <p className="text-sm text-muted-foreground">
            从 GitHub 检查并一键升级到最新版本（多镜像加速 + sha256 校验，升级后自动重启）。
          </p>
          <div className="flex flex-wrap items-center gap-2">
            <Button variant="outline" size="sm" onClick={handleCheck} disabled={checking || upgrading}>
              {checking ? <Loader2 className="mr-1.5 h-4 w-4 animate-spin" /> : <RefreshCw className="mr-1.5 h-4 w-4" />}
              检查更新
            </Button>
            {updInfo && !updInfo.error && (
              <span className="text-xs text-muted-foreground">
                当前 <span className="font-mono">{updInfo.local_version}</span>
                {updInfo.latest_version && (
                  <> · 最新 <span className="font-mono">{updInfo.latest_version}</span></>
                )}
              </span>
            )}
            {updInfo?.has_update && (
              <Button size="sm" onClick={() => setUpgradeConfirm(true)} disabled={upgrading}>
                {upgrading ? <Loader2 className="mr-1.5 h-4 w-4 animate-spin" /> : <RotateCcw className="mr-1.5 h-4 w-4" />}
                升级到 {updInfo.latest_version}
              </Button>
            )}
          </div>
          {/* commit 快照：展示"这版改了啥"（GitHub compare API 拉的 commit 列表），升级前先看清改动 */}
          {updInfo?.has_update && updInfo.commits.length > 0 && (
            <div className="mt-1 rounded-md border border-border/60 bg-secondary/30 p-2">
              <div className="mb-1 text-xs font-medium text-muted-foreground">
                本次更新包含 {updInfo.commits.length} 个提交：
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
                  ✓ 本版已稳定确认
                </span>
              ) : (
                <span className="text-amber-500">⏳ 本版尚未确认稳定（升级后运行一段时间自动确认）</span>
              )}
              {updStatus.rollbackPointPresent && (
                <span className="text-muted-foreground">回滚点仍在(可回退)</span>
              )}
              {updStatus.rolledBackBinaryPresent && (
                <span className="text-red-400" title="守卫脚本曾执行过回滚">⚠ 检测到曾发生回滚</span>
              )}
            </div>
          )}
        </div>
      </CardContent>

      <ConfirmDialog
        open={confirmOpen}
        onOpenChange={setConfirmOpen}
        title="确认重启服务？"
        description="重启会导致网关短暂断服数秒，期间所有请求失败（含正在进行的对话）。约 3 秒后自动恢复。确定继续？"
        confirmLabel="确认重启"
        destructive
        loading={isPending}
        onConfirm={handleConfirm}
      />
      <ConfirmDialog
        open={upgradeConfirm}
        onOpenChange={setUpgradeConfirm}
        title={`确认升级到 ${updInfo?.latest_version ?? '最新版本'}？`}
        description="将从 GitHub 下载新二进制、校验 sha256 后替换并自动重启。重启期间短暂断服数秒。确定继续？"
        confirmLabel="确认升级"
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
        {viewable && (
          <Button variant="outline" size="sm" onClick={() => onView(p)}>
            <Eye className="mr-1 h-3.5 w-3.5" />
            查看
          </Button>
        )}
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
          <Highlight text="存储占用" />
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
                <StoragePartitionRow key={p.key} p={p} onCleanup={openCleanup} onView={openView} />
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
          <Highlight text="客户端 RPM" />
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

/* ============ 4. 隐私：下游客户端指纹采集开关（立即生效） ============ */
// 独立于底部批量保存：切换即调用 updateConfig，立即生效、无需重启。
function PrivacyCard() {
  const { data: config } = useConfigSnapshot()
  const { mutate: save, isPending } = useUpdateConfig()

  // 缺省视为开启（后端字段可能尚未下发时不误显示为关闭）
  const enabled = config?.collectClientFingerprint ?? true

  const toggle = (v: boolean) => {
    save(
      { collectClientFingerprint: v },
      {
        onSuccess: () =>
          toast.success(v ? '已开启下游客户端指纹采集' : '已关闭下游客户端指纹采集，不再采集与存储'),
        onError: (err) => toast.error(extractErrorMessage(err)),
      }
    )
  }

  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="text-base flex items-center gap-2">
          <Fingerprint className="h-4 w-4 text-muted-foreground" />
          <Highlight text="隐私 / 客户端指纹" />
        </CardTitle>
      </CardHeader>
      <CardContent className="py-0">
        <Field
          label="采集下游客户端指纹（设备/IP/系统/浏览器）"
          hint="关闭后不再采集、不存储下游客户端的设备/IP/系统/浏览器信息，隐私性更好（不影响请求转发与用量统计）"
        >
          <Switch checked={enabled} disabled={isPending} onCheckedChange={toggle} />
        </Field>
      </CardContent>
    </Card>
  )
}

/* ============ 5. 令牌导出：单个 / 全部凭据 JSON 下载 ============ */
function TokenExportCard() {
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
      toast.success(`已导出凭据 #${id}`)
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
        toast.error('该凭据不包含 refreshToken（可能是 API Key 凭据）')
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
      toast.success(`已导出 #${id} 的 refreshToken`)
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
        toast.success(`已复制 #${id} 凭据 JSON 到剪贴板`)
      } else {
        toast.error('复制失败，请重试')
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
      toast.success(`已导出全部 ${all.length} 个凭据`)
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
          <Highlight text="令牌导出" />
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
          导出全部（{list.length}）
        </Button>
      </CardHeader>
      <CardContent className="space-y-3">
        <p className="text-sm text-muted-foreground">
          导出凭据令牌：完整 JSON（可重新导入）、仅 refreshToken 纯文本、或复制到剪贴板。
          含 refreshToken / kiroApiKey 等敏感字段，请妥善保管、勿外泄。
        </p>
        {isLoading ? (
          <div className="space-y-2">
            <Skeleton className="h-10 w-full" />
            <Skeleton className="h-10 w-full" />
          </div>
        ) : error ? (
          <div className="py-3 text-center text-sm text-red-400">
            加载凭据列表失败：{extractErrorMessage(error)}
            <Button variant="outline" size="sm" className="ml-2" onClick={() => refetch()}>
              重试
            </Button>
          </div>
        ) : list.length === 0 ? (
          <p className="py-4 text-center text-sm text-muted-foreground">暂无凭据可导出</p>
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
                    {c.subscriptionTitle || c.authMethod || '凭据'}
                  </div>
                </div>
                <div className="flex shrink-0 items-center gap-1.5">
                  {exportingId === c.id ? (
                    <div className="flex h-8 items-center px-2 text-xs text-muted-foreground">
                      <Loader className="mr-1 h-3.5 w-3.5 animate-spin" />
                      导出中
                    </div>
                  ) : (
                    <>
                      <Button
                        variant="outline"
                        size="sm"
                        onClick={() => exportOne(c.id)}
                        disabled={exportingAll}
                        title="下载完整凭据 JSON（可重新导入）"
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
                        title="仅下载 refreshToken 纯文本"
                      >
                        <KeyRound className="h-3.5 w-3.5" />
                      </Button>
                      <Button
                        variant="outline"
                        size="icon"
                        className="h-8 w-8"
                        onClick={() => copyOne(c.id)}
                        disabled={exportingAll}
                        title="复制完整 JSON 到剪贴板"
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
  return (
    <div className="flex items-center justify-between gap-4 border-b border-border/40 py-2.5 last:border-0">
      <div className="flex min-w-0 items-center gap-3">
        <Checkbox
          checked={checked}
          onCheckedChange={(v) => onToggle(v === true)}
          aria-label={`选择 #${item.id}`}
        />
        <div className="min-w-0">
          <div className="truncate text-sm">
            #{item.id}
            {item.email ? ` · ${item.email}` : ''}
          </div>
          <div className="mt-0.5 flex flex-wrap items-center gap-x-2 gap-y-0.5 text-[11px] text-muted-foreground">
            <span>{item.authMethod || '凭据'}</span>
            <span>·</span>
            <span title={item.deletedAt}>删除于 {timeAgo(item.deletedAt)}</span>
            <span>·</span>
            <span>成功 {item.successCount} 次</span>
          </div>
        </div>
      </div>
      <div className="flex shrink-0 items-center gap-2">
        <Button variant="outline" size="sm" onClick={onRestore} disabled={busy}>
          <RotateCcw className="mr-1 h-3.5 w-3.5" />
          恢复
        </Button>
        <Button variant="destructive" size="sm" onClick={onPurge} disabled={busy}>
          <Trash className="mr-1 h-3.5 w-3.5" />
          永久删除
        </Button>
      </div>
    </div>
  )
}

function TrashCard() {
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
      toast.success(`已恢复凭据 #${item.id}`)
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
        toast.success(`已永久删除凭据 #${confirm.item.id}`)
      } else if (confirm.kind === 'selected') {
        await purgeTrashBatch(confirm.ids)
        toast.success(`已永久删除选中的 ${confirm.ids.length} 项`)
      } else {
        await purgeTrashBatch()
        toast.success('已清空回收站')
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
      ? `永久删除凭据 #${confirm.item.id}？`
      : confirm?.kind === 'selected'
        ? `永久删除选中的 ${confirm.ids.length} 项？`
        : '清空整个回收站？'

  return (
    <Card>
      <CardHeader className="pb-2 flex-row items-center justify-between space-y-0">
        <CardTitle className="text-base flex items-center gap-2">
          <Trash2 className="h-4 w-4 text-muted-foreground" />
          <Highlight text="回收站" />
          {isFetching && <Loader className="h-3.5 w-3.5 animate-spin text-muted-foreground" />}
        </CardTitle>
        <Button variant="outline" size="sm" onClick={() => refetch()} disabled={isLoading}>
          刷新
        </Button>
      </CardHeader>
      <CardContent className="space-y-4">
        <p className="text-sm text-muted-foreground">
          已删除的凭据暂存于此，可恢复回号池或永久清除。永久删除后<strong className="text-red-400">无法恢复</strong>。
        </p>

        {isLoading ? (
          <div className="space-y-2">
            <Skeleton className="h-12 w-full" />
            <Skeleton className="h-12 w-full" />
            <Skeleton className="h-12 w-full" />
          </div>
        ) : error ? (
          <div className="py-4 text-center text-sm text-red-400">
            加载回收站失败：{extractErrorMessage(error)}
            <Button variant="outline" size="sm" className="ml-2" onClick={() => refetch()}>
              重试
            </Button>
          </div>
        ) : list.length === 0 ? (
          <div className="flex flex-col items-center gap-2 py-12 text-center text-sm text-muted-foreground">
            <Trash2 className="h-8 w-8 opacity-40" />
            回收站为空
          </div>
        ) : (
          <>
            {/* 工具栏：全选 + 批量操作 */}
            <div className="flex flex-wrap items-center justify-between gap-3 rounded-md border border-border/60 bg-secondary/30 px-3 py-2">
              <label className="flex cursor-pointer items-center gap-2 text-sm">
                <Checkbox
                  checked={allChecked ? true : someChecked ? 'indeterminate' : false}
                  onCheckedChange={(v) => toggleAll(v === true)}
                  aria-label="全选"
                />
                <span>
                  全选
                  {selected.size > 0 && (
                    <span className="ml-1 text-muted-foreground">（已选 {selected.size} / {total}）</span>
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
                  清空选中
                </Button>
                <Button
                  variant="destructive"
                  size="sm"
                  disabled={busy}
                  onClick={() => setConfirm({ kind: 'all' })}
                >
                  <Trash2 className="mr-1 h-3.5 w-3.5" />
                  清空全部
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
            此操作将<strong className="text-red-400">永久删除，无法恢复</strong>。
            {confirm?.kind === 'all'
              ? '回收站内全部条目都会被清除。'
              : '删除后无法再从回收站找回。'}
          </span>
        }
        confirmLabel="确认永久删除"
        destructive
        loading={busy}
        onConfirm={runConfirmed}
      />
    </Card>
  )
}

export function SettingsPage() {
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
    for (const c of CARD_INDEX) {
      if (c.title.toLowerCase().includes(query) || c.keywords.some((k) => k.includes(query))) {
        s.add(c.section)
      }
    }
    return s
  }, [query])

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
      if (uiPrefsDirty) toast.success('已保存 UI 排版偏好')
      return
    }
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
    <SearchContext.Provider value={{ query }}>
    <ActiveSectionContext.Provider value={activeSection}>
    <div className="space-y-6 pb-24">
      <div className="flex items-center justify-between gap-4">
        <h2 className="text-xl font-semibold text-gradient-brand">设置</h2>
        <div className="flex items-center gap-2">
          {/* 搜索：跨区定位设置项，命中即高亮/过滤 */}
          <div className="relative">
            <Search className="pointer-events-none absolute left-2.5 top-1/2 h-4 w-4 -translate-y-1/2 text-muted-foreground" />
            <Input
              className="w-56 pl-8 pr-8"
              value={searchRaw}
              onChange={(e) => setSearchRaw(e.target.value)}
              placeholder="搜索设置项…"
              aria-label="搜索设置项"
            />
            {searchRaw && (
              <button
                type="button"
                onClick={() => setSearchRaw('')}
                className="absolute right-2 top-1/2 -translate-y-1/2 text-muted-foreground hover:text-foreground"
                aria-label="清除搜索"
              >
                <X className="h-4 w-4" />
              </button>
            )}
          </div>
          <Button variant="outline" size="sm" onClick={() => refetch()} disabled={isSaving}>
            刷新
          </Button>
        </div>
      </div>

      {/* 分区导航 tab：搜索态下隐藏（改为跨区展示命中项） */}
      {!query && (
        <div className="flex flex-wrap gap-2 border-b pb-3">
          {SECTIONS.map((s) => {
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
                {s.label}
              </Button>
            )
          })}
        </div>
      )}

      {/* 搜索态提示条 */}
      {query && (
        <p className="text-sm text-muted-foreground">
          {hasAnyMatch
            ? `“${searchRaw.trim()}” 的匹配结果（命中 ${matchedSections.size} 个分区）`
            : `没有匹配“${searchRaw.trim()}”的设置项`}
        </p>
      )}

      {/* 服务管理分区：一键重启 */}
      <SectionGate section="service" title="服务管理" keywords={['一键重启', '重启服务', 'restart']}>
        <ServiceManagementCard />
      </SectionGate>
      {/* 存储分区：占用统计 + 清理 */}
      <SectionGate section="storage" title="存储占用" keywords={['存储', '清理', '落盘', '分区', 'traces', 'usage', 'trash', 'bg_cache', '磁盘']}>
        <StorageStatsCard />
      </SectionGate>
      {/* 服务管理分区：客户端 RPM */}
      <SectionGate section="service" title="客户端 RPM" keywords={['客户端 rpm', '窗口', 'session', '吞吐', '活跃客户端']}>
        <ClientRpmCard />
      </SectionGate>

      {/* 隐私分区：客户端指纹采集开关（立即生效） */}
      <SectionGate section="privacy" title="隐私 / 客户端指纹" keywords={['采集下游客户端指纹', '设备', 'ip', '系统', '浏览器', '隐私', 'fingerprint']}>
        <PrivacyCard />
      </SectionGate>

      {/* UI 排版自定义分区：号池状态排序/禁用号显隐 + 凭据卡片尺寸。纯前端 localStorage,纳入统一保存流程(切换点亮保存,点保存才落地)。 */}
      <SectionGate section="appearance" title="UI 排版自定义" keywords={['ui 排版', '排版', '号池排序', '卡片大小', '卡片尺寸', '禁用号', '布局', 'layout', '排序模式']}>
        <Field label="号池状态排序" hint="概览页「号池状态」按此排列（实时数据自动排，保存后生效）">
          <SegChoice
            value={form.poolSort}
            onChange={(v) => set('poolSort', v as PoolSortMode)}
            options={[
              { value: 'health', label: '健康度' },
              { value: 'sequence', label: '顺序' },
              { value: 'concurrency', label: '并发数' },
              { value: 'lastUsed', label: '最后调用' },
            ]}
          />
        </Field>
        <Field label="展示已禁用号" hint="关闭后号池状态只显示启用中的号，隐藏已禁用的">
          <Switch
            checked={form.poolShowDisabled}
            onCheckedChange={(v) => set('poolShowDisabled', v)}
          />
        </Field>
        <Field label="凭据卡片尺寸" hint="凭据管理页卡片大小，按尺寸自动决定每行几个（紧凑约5、标准约4、大约3）">
          <SegChoice
            value={form.cardSize}
            onChange={(v) => set('cardSize', v as CardSize)}
            options={[
              { value: 'compact', label: '紧凑' },
              { value: 'standard', label: '标准' },
              { value: 'large', label: '大' },
            ]}
          />
        </Field>
      </SectionGate>

      {/* 令牌导出分区：单个 / 全部凭据 JSON 下载 */}
      <SectionGate section="export" title="令牌导出" keywords={['令牌导出', '凭据 json', '导出单个', '导出全部', 'token 导出', 'export']}>
        <TokenExportCard />
      </SectionGate>

      {/* 回收站分区：已删除凭据的恢复 / 永久清除 */}
      <SectionGate section="trash" title="回收站" keywords={['回收站', '已删除', '清空', '恢复', 'trash']}>
        <TrashCard />
      </SectionGate>

      {/* 调度分区：负载均衡（立即生效） */}
      <SectionGate section="scheduling" title="负载均衡模式" keywords={['负载均衡', '优先级模式', '均衡负载', 'priority', 'balanced']}>
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base"><Highlight text="负载均衡模式" /></CardTitle>
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
          <Field
            label="均衡模式叠加优先级分发"
            hint="开启后:均衡负载也先按 priority 分层(越小越优先),层内仍按健康/负载均衡,整层打满才溢出到下一层。保存即时生效。"
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
      <SectionGate section="scheduling" title="智能调度" keywords={['余额加权', '智能调度', 'headroom', '预留', '背压', '429', '限速感知', 'balance', 'rpm', '每号 rpm 软上限', '全局 rpm']}>
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base"><Highlight text="智能调度" /></CardTitle>
        </CardHeader>
        <CardContent className="py-0">
          <p className="mb-3 mt-1 text-sm text-muted-foreground">
            均衡模式下的动态分流策略。全部保存后立即生效（热更，无需重启）。默认配置已是良好动态化，
            按需微调或一键关闭回退纯负载均衡。
          </p>
          <Field
            label="余额加权分流"
            hint="开启后:同优先级/同健康/同在途时,按各号剩余额度微调选号——余额多的略多用、少的略少用,长期把号池剩余额度拉平,不让某个号先耗干。软偏置不掀翻在途均分。"
          >
            <Switch
              checked={form.balanceWeightEnabled}
              onCheckedChange={(v) => set('balanceWeightEnabled', v)}
            />
          </Field>
          <Field
            label="余额加权强度 FLOOR"
            hint="因子下限(整百分比 0-100)。50=满额号因子1.0、半额0.75、耗尽0.5(差10~20%微调)。越小余额影响越强;100=等于关闭加权。"
          >
            <NumberStepper
              value={Number(form.balanceWeightFloor) || 0}
              onChange={(v) => set('balanceWeightFloor', String(v))}
              min={0}
              max={100}
              step={5}
              className="w-28"
              aria-label="余额加权 FLOOR"
              disabled={!form.balanceWeightEnabled}
            />
          </Field>
          <Field
            label="429/限速感知降权"
            hint="开启后:某号冒 429/被上游软限流时,经 EWMA 拉低其健康分→自动少分配给它,恢复后逐步放回。关闭则偶发 429 不影响分流。"
          >
            <Switch
              checked={form.health429WeightEnabled}
              onCheckedChange={(v) => set('health429WeightEnabled', v)}
            />
          </Field>
          {/* RPM 相关：行尾齿轮点开「RPM 卡」，含全局软上限 + headroom + 预留 + 背压 */}
          <Field
            label="RPM headroom 系数"
            hint="饱和阈值 = 上限 × 系数%(整百分比)。85=预留 15% 缓冲,让分流在撞上游硬限之前提前触发,削弱 60s 滑窗边界爆发。100=不打折(贴硬限)。点齿轮调全局每号 RPM 软上限等。"
          >
            <div className="flex items-center gap-1.5">
              <NumberStepper
                value={Number(form.rpmHeadroomFactor) || 0}
                onChange={(v) => set('rpmHeadroomFactor', String(v))}
                min={0}
                max={100}
                step={5}
                className="w-28"
                aria-label="RPM headroom 系数"
              />
              <SettingGearCard
                title="RPM 细粒度设置"
                description="每号 RPM 软上限与饱和预留策略。全局软上限在单号未单独设置时兜底继承。均热更即时生效。"
              >
                <SearchContext.Provider value={{ query: '' }}>
                  <Field
                    label="全局每号 RPM 软上限"
                    hint="单号未单独设置(=0)时继承此值；此值也为 0 时用内置兜底 30。防单号请求过密撞上游滑窗限。"
                  >
                    <NumberStepper
                      value={Number(form.credentialRpmLimit) || 0}
                      onChange={(v) => set('credentialRpmLimit', String(v))}
                      min={0}
                      max={10000}
                      step={5}
                      className="w-28"
                      aria-label="全局每号 RPM 软上限"
                    />
                  </Field>
                  <Field
                    label="RPM headroom 系数"
                    hint="饱和阈值 = 上限 × 系数%。85=预留 15% 缓冲，提前触发分流削弱滑窗边界爆发。100=贴硬限。"
                  >
                    <NumberStepper
                      value={Number(form.rpmHeadroomFactor) || 0}
                      onChange={(v) => set('rpmHeadroomFactor', String(v))}
                      min={0}
                      max={100}
                      step={5}
                      className="w-28"
                      aria-label="RPM headroom 系数"
                    />
                  </Field>
                  <Field
                    label="RPM 预留名额"
                    hint="在 headroom 折扣后再额外扣掉 N 个名额给突发留固定缓冲。0=不额外预留。与 headroom 叠加。"
                  >
                    <NumberStepper
                      value={Number(form.rpmReserveSlots) || 0}
                      onChange={(v) => set('rpmReserveSlots', String(v))}
                      min={0}
                      max={1000}
                      className="w-28"
                      aria-label="RPM 预留名额"
                    />
                  </Field>
                  <Field
                    label="整池饱和背压等待"
                    hint="⚠️进阶：整池 RPM 全饱和时，选号在网关内等待最短恢复窗口而非立即回退软门。默认关。"
                  >
                    <Switch
                      checked={form.rpmHardGateOverloadWait}
                      onCheckedChange={(v) => set('rpmHardGateOverloadWait', v)}
                    />
                  </Field>
                </SearchContext.Provider>
              </SettingGearCard>
              <SettingGearCard
                title={`入站请求整形 / RPM 自动挡（当前 ${config?.inboundCurrentRpm ?? '—'} RPM）`}
                description="在网关入口用令牌桶把突发削平成受控 RPM，让号不被上游打爆（治 429 雪崩）。自动挡按上游反馈动态升降速率。均热更即时生效。"
              >
                <SearchContext.Provider value={{ query: '' }}>
                  <Field label="启用入站整形" hint="开=请求进上游前先过全局令牌桶，突发被排队削平；关=不限速直发（易被上游 429）。">
                    <Switch checked={form.inboundThrottleEnabled} onCheckedChange={(v) => set('inboundThrottleEnabled', v)} />
                  </Field>
                  <Field label="RPM 自动挡" hint="开=AIMD 动态调速率（无 429 加速/收 429 砍半），自动收敛到上游不限流的最高速；关=固定用目标 RPM（手动挡）。">
                    <Switch checked={form.inboundRpmAuto} onCheckedChange={(v) => set('inboundRpmAuto', v)} />
                  </Field>
                  <Field label="目标 RPM（初值/手动挡固定值）" hint="自动挡：作为起点，之后动态调整；手动挡：固定用此值。">
                    <NumberStepper value={Number(form.inboundTargetRpm) || 0} onChange={(v) => set('inboundTargetRpm', String(v))} min={1} max={10000} step={10} className="w-28" aria-label="目标 RPM" />
                  </Field>
                  <Field label="自动挡 RPM 下限" hint="乘性降档不低于此。">
                    <NumberStepper value={Number(form.inboundRpmMin) || 0} onChange={(v) => set('inboundRpmMin', String(v))} min={1} max={10000} step={5} className="w-28" aria-label="RPM 下限" />
                  </Field>
                  <Field label="自动挡 RPM 上限" hint="加性升档不超过此。加号后可调高。">
                    <NumberStepper value={Number(form.inboundRpmMax) || 0} onChange={(v) => set('inboundRpmMax', String(v))} min={1} max={10000} step={10} className="w-28" aria-label="RPM 上限" />
                  </Field>
                  <Field label="令牌桶突发容量（秒）" hint="允许短时小突发不排队。越大越宽松。默认 2。">
                    <NumberStepper value={Number(form.inboundBurstSecs) || 0} onChange={(v) => set('inboundBurstSecs', String(v))} min={1} max={60} className="w-28" aria-label="突发容量秒" />
                  </Field>
                  <Field label="排队最长等待（秒）" hint="排队超此时长返回带 Retry-After 的 429 让客户端退避。默认 30。">
                    <NumberStepper value={Number(form.inboundQueueMaxWaitSecs) || 0} onChange={(v) => set('inboundQueueMaxWaitSecs', String(v))} min={1} max={300} step={5} className="w-28" aria-label="排队最长等待秒" />
                  </Field>
                </SearchContext.Provider>
              </SettingGearCard>
            </div>
          </Field>
          <Field
            label="RPM 预留名额"
            hint="在 headroom 折扣后再额外扣掉 N 个名额给突发留固定缓冲。0=不额外预留。与 headroom 叠加。"
          >
            <NumberStepper
              value={Number(form.rpmReserveSlots) || 0}
              onChange={(v) => set('rpmReserveSlots', String(v))}
              min={0}
              max={1000}
              className="w-28"
              aria-label="RPM 预留名额"
            />
          </Field>
          <Field
            label="整池饱和背压等待"
            hint="⚠️进阶:开启后当整池 RPM 全饱和时,选号会在网关内等待最短恢复窗口而非立即回退软门。默认关(回退软门选最不坏的号继续,不阻塞)。"
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
      <SectionGate section="basic" title="服务信息" keywords={['监听地址', 'host', '端口', 'port', '区域', 'region', 'tls 后端', 'rustls', '默认 endpoint', '配置文件']}>
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base"><Highlight text="服务信息" /></CardTitle>
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
          {/* TLS 后端固定为 rustls：出厂构建纯 rustls（见 build.bat / release.yml），
              native-tls 已废弃（曾误导用户切换后废网关）。仅作只读展示，不再可切换。 */}
          <ReadonlyRow label="TLS 后端" value="rustls（内置 webpki + 系统根证书）" />
          <Field label="默认 endpoint" hint={`可用：${config.endpointNames.join(', ') || '—'}（需重启生效）`}>
            <Input className={inputCls} value={form.defaultEndpoint} onChange={(e) => set('defaultEndpoint', e.target.value)} />
          </Field>
          {config.configPath && <ReadonlyRow label="配置文件" value={config.configPath} mono />}
        </CardContent>
      </Card>
      </SectionGate>

      {/* 基础分区：客户端伪装（需重启） */}
      <SectionGate section="basic" title="客户端伪装" keywords={['kiro 版本', '系统版本', 'node 版本', '伪装', '版本号']}>
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base"><Highlight text="客户端伪装" /></CardTitle>
        </CardHeader>
        <CardContent className="py-0">
          <Field label="Kiro 版本" hint="可选预设或自定义（需重启生效）">
            <ComboInput className={inputCls} value={form.kiroVersion} onChange={(v) => set('kiroVersion', v)} options={KIRO_VERSION_PRESETS} aria-label="Kiro 版本" />
          </Field>
          <Field label="系统版本" hint="可选预设或自定义（需重启生效）">
            <ComboInput className={inputCls} value={form.systemVersion} onChange={(v) => set('systemVersion', v)} options={SYSTEM_VERSION_PRESETS} aria-label="系统版本" />
          </Field>
          <Field label="Node 版本" hint="可选预设或自定义（需重启生效）">
            <ComboInput className={inputCls} value={form.nodeVersion} onChange={(v) => set('nodeVersion', v)} options={NODE_VERSION_PRESETS} aria-label="Node 版本" />
          </Field>
        </CardContent>
      </Card>
      </SectionGate>

      <SectionGate section="basic" title="协议与转发" keywords={['提取 thinking', 'thinking', 'claude code 自动切协议', 'cc_auto_buffer', '缓冲分发', '剥离环境噪音', 'env', 'git', '省 token', '缓存']}>
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base"><Highlight text="协议与转发" /></CardTitle>
        </CardHeader>
        <CardContent className="py-0">
          <Field label="提取 thinking" hint="非流式响应解析 thinking 块（保存即时生效，无需重启）">
            <Switch checked={form.extractThinking} onCheckedChange={(v) => set('extractThinking', v)} />
          </Field>
          <Field label="Claude Code 自动切协议" hint="识别到 Claude Code 请求时，/v1 流式自动走缓冲分发（准确 input_tokens，等价 /cc/v1），CC 无需手动改端点（保存即时生效，无需重启）">
            <Switch checked={form.ccAutoBuffer} onCheckedChange={(v) => set('ccAutoBuffer', v)} />
          </Field>
          <Field label="剥离环境噪音" hint="转发前剥离 system 里每请求漂移的 env/git/模型名等噪音，省 token 提缓存降关联（保存即时生效，无需重启）">
            <Switch checked={form.stripEnvNoise} onCheckedChange={(v) => set('stripEnvNoise', v)} />
          </Field>
        </CardContent>
      </Card>
      </SectionGate>

      <SectionGate section="basic" title="工具调用容错" keywords={['invalid tool parameters', '工具拼装非法', '工具错误', '失败态对齐', 'json 修复', '修复层', '修非法转义', '清洗泄漏控制 token', '截断跨轮恢复', '工具描述字符上限', 'tool_repair', 'tool_truncation']}>
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base"><Highlight text="工具调用容错" /></CardTitle>
          <p className="text-xs text-muted-foreground">缓解 / 根治 Claude Code 的 Invalid tool parameters，均热更即时生效。</p>
        </CardHeader>
        <CardContent className="py-0">
          <Field label="JSON 修复层（根治 Invalid tool parameters）" hint="工具参数非法 JSON 时先尝试修成合法（转义非法反斜杠/裸控制符、补全截断），修复后强制复验通过才发。只在 JSON 已非法时介入、对正常流零影响，故默认开。客户端不再报 Invalid tool parameters">
            <Switch checked={form.toolRepairJson} onCheckedChange={(v) => set('toolRepairJson', v)} />
          </Field>
          <Field label="工具拼装非法对齐失败态" hint="流式工具调用参数拼成非法 JSON 时置失败态（与非流式一致，不再静默记成功），便于客户端感知重试。默认开，绝不连坐号">
            <Switch checked={form.toolStreamAlignFailure} onCheckedChange={(v) => set('toolStreamAlignFailure', v)} />
          </Field>
          <Field label="工具错误如实暴露客户端" hint="工具参数拼装非法且修复层修不好时不发坏 JSON，改发明确 SSE error 让客户端退避重试（与上一项配对）。默认开">
            <Switch checked={form.toolExposeErrorToClient} onCheckedChange={(v) => set('toolExposeErrorToClient', v)} />
          </Field>
          <Field label="清洗泄漏控制 token" hint="清洗模型泄漏进文本行首的控制 token（course/課/count/care 粘连），保守只剥行首粘连不误删正文。默认开">
            <Switch checked={form.toolCleanLeakedTokens} onCheckedChange={(v) => set('toolCleanLeakedTokens', v)} />
          </Field>
          <Field label="文本化 invoke 重组（根治 court/Invalid tool parameters）" hint="模型把工具调用吐成 <invoke> 文本时，在四道安全门内（行首+非代码围栏+工具名已声明+完整闭合）重组为结构化 tool_use；修不了的碎片/截断安全当文本放过。移植 ZyphrZero 生产方案。默认开">
            <Switch checked={form.toolReclaimTextifiedInvoke} onCheckedChange={(v) => set('toolReclaimTextifiedInvoke', v)} />
          </Field>
          <Field label="stray token 复读熔断" hint="call/count/card/court 连续独占行复读超阈值（32）截断本轮文本，治 Opus 退化刷屏耗尽 max_tokens + 污染历史。默认开">
            <Switch checked={form.toolStrayRepeatGuard} onCheckedChange={(v) => set('toolStrayRepeatGuard', v)} />
          </Field>
          <Field label="截断跨轮恢复" hint="工具参数被上游真截断（缺整段值）且修复层也补不回时，不发半截参数，改置失败态让客户端重试整轮。会把截断从「发半截」变成「整轮失败重试」，改变对话流程，故默认关">
            <Switch checked={form.toolTruncationRecovery} onCheckedChange={(v) => set('toolTruncationRecovery', v)} />
          </Field>
          <Field label="工具描述字符上限" hint="入站工具顶层 description 超此长度按字符边界安全截断（省 token、避开上游单工具描述隐性上限），schema 内嵌描述取此值 1/5。默认 10000，设 0 表示不截断">
            <NumberStepper value={Number(form.toolDescriptionMaxChars) || 0} onChange={(v) => set('toolDescriptionMaxChars', String(v))} min={0} step={1000} className="w-32" aria-label="工具描述字符上限" />
          </Field>
        </CardContent>
      </Card>
      </SectionGate>

      {/* 调度分区：防关联 / 限流（需重启） */}
      <SectionGate section="scheduling" title="防关联 / 限流" keywords={['冷却机制', '速率限制', '每日上限', '最小请求间隔', '会话亲和性', 'affinity']}>
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base"><Highlight text="防关联 / 限流" /></CardTitle>
        </CardHeader>
        <CardContent className="py-0">
          {/* 冷却机制：行尾齿轮点开「冷却卡」做细粒度设置 */}
          <Field label="冷却机制" hint="失败后短暂跳过该凭据（保存即时生效，无需重启）。点齿轮调冷却时长缩放。">
            <div className="flex items-center gap-1.5">
              <Switch checked={form.cooldownEnabled} onCheckedChange={(v) => set('cooldownEnabled', v)} />
              <SettingGearCard
                title="冷却设置"
                description="失败后短暂跳过该凭据，避开风控。冷却时长缩放让重试节奏更激进或更保守。均热更即时生效。"
              >
                <SearchContext.Provider value={{ query: '' }}>
                  <Field label="启用冷却机制" hint="失败后短暂跳过该凭据（保存即时生效，无需重启）">
                    <Switch checked={form.cooldownEnabled} onCheckedChange={(v) => set('cooldownEnabled', v)} />
                  </Field>
                  <Field
                    label="冷却时长缩放 (%)"
                    hint="缩放可恢复的短冷却时长。<100 更激进快重试（省吞吐但更易撞风控），>100 更保守防封。100=原时长。"
                  >
                    <NumberStepper
                      value={Number(form.cooldownScalePct) || 0}
                      onChange={(v) => set('cooldownScalePct', String(v))}
                      min={10}
                      max={500}
                      step={10}
                      className="w-28"
                      disabled={!form.cooldownEnabled}
                      aria-label="冷却时长缩放百分比"
                    />
                  </Field>
                </SearchContext.Provider>
              </SettingGearCard>
            </div>
          </Field>
          {/* 速率限制：行尾齿轮点开「速率卡」做细粒度设置 */}
          <Field label="速率限制" hint="拟人节奏：每日上限 + 请求间隔 + 间隔抖动（保存即时生效，无需重启）。点齿轮细调。">
            <div className="flex items-center gap-1.5">
              <Switch checked={form.rateLimitEnabled} onCheckedChange={(v) => set('rateLimitEnabled', v)} />
              <SettingGearCard
                title="速率限制设置"
                description="拟人节奏，降低账号关联/风控风险。每日上限 + 最小请求间隔 + 间隔抖动。均热更即时生效。"
              >
                <SearchContext.Provider value={{ query: '' }}>
                  <Field label="启用速率限制" hint="拟人节奏：每日上限 + 请求间隔（保存即时生效，无需重启）">
                    <Switch checked={form.rateLimitEnabled} onCheckedChange={(v) => set('rateLimitEnabled', v)} />
                  </Field>
                  <Field label="每日上限" hint="0 表示无限制（保存即时生效，无需重启）">
                    <NumberStepper value={Number(form.rateLimitDailyMax) || 0} onChange={(v) => set('rateLimitDailyMax', String(v))} min={0} step={10} className="w-28" disabled={!form.rateLimitEnabled} aria-label="每日上限" />
                  </Field>
                  <Field label="最小请求间隔 (ms)" hint="保存即时生效，无需重启">
                    <NumberStepper value={Number(form.rateLimitMinIntervalMs) || 0} onChange={(v) => set('rateLimitMinIntervalMs', String(v))} min={0} step={100} className="w-28" disabled={!form.rateLimitEnabled} aria-label="最小请求间隔" />
                  </Field>
                  <Field
                    label="间隔抖动 (%)"
                    hint="在最小请求间隔上叠加随机抖动，让节奏更像人（0-50）。0=固定间隔（更机械）。"
                  >
                    <NumberStepper
                      value={Number(form.rateLimitJitterPct) || 0}
                      onChange={(v) => set('rateLimitJitterPct', String(v))}
                      min={0}
                      max={50}
                      step={5}
                      className="w-28"
                      disabled={!form.rateLimitEnabled}
                      aria-label="间隔抖动百分比"
                    />
                  </Field>
                </SearchContext.Provider>
              </SettingGearCard>
            </div>
          </Field>
          <Field label="每日上限" hint="0 表示无限制（保存即时生效，无需重启）">
            <NumberStepper value={Number(form.rateLimitDailyMax) || 0} onChange={(v) => set('rateLimitDailyMax', String(v))} min={0} step={10} className="w-28" disabled={!form.rateLimitEnabled} aria-label="每日上限" />
          </Field>
          <Field label="最小请求间隔 (ms)" hint="保存即时生效，无需重启">
            <NumberStepper value={Number(form.rateLimitMinIntervalMs) || 0} onChange={(v) => set('rateLimitMinIntervalMs', String(v))} min={0} step={100} className="w-28" disabled={!form.rateLimitEnabled} aria-label="最小请求间隔" />
          </Field>
          <Field label="会话亲和性" hint="同一会话尽量复用同一凭据（保存即时生效，无需重启）">
            <Switch checked={form.affinityEnabled} onCheckedChange={(v) => set('affinityEnabled', v)} />
          </Field>
        </CardContent>
      </Card>
      </SectionGate>

      {/* 基础分区：网络与上号（需重启） */}
      <SectionGate section="basic" title="网络与上号" keywords={['全局代理', 'proxy', '上号回调地址', 'callback', '回调模式', 'admin key']}>
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base"><Highlight text="网络与上号" /></CardTitle>
        </CardHeader>
        <CardContent className="py-0">
          <Field label="全局代理" hint="http(s)://host:port 或 socks5://host:port，留空清除（需重启生效）">
            <div className="flex items-center gap-2">
              <Input className="max-w-[260px] font-mono text-xs" value={form.proxyUrl} onChange={(e) => set('proxyUrl', e.target.value)} placeholder="未配置" />
              <ProxyTestButton proxyUrl={form.proxyUrl} proxyUsername={form.proxyUsername} proxyPassword={form.proxyPassword} />
            </div>
          </Field>
          <Field label="代理用户名" hint="需认证的代理才填。留空=不修改（后端出于安全不回显已存值，需重启生效）">
            <Input className="max-w-[260px] font-mono text-xs" value={form.proxyUsername} onChange={(e) => set('proxyUsername', e.target.value)} placeholder="留空不改" autoComplete="off" />
          </Field>
          <Field label="代理密码" hint="需认证的代理才填。留空=不修改（后端出于安全不回显，需重启生效）">
            <Input type="password" className="max-w-[260px] font-mono text-xs" value={form.proxyPassword} onChange={(e) => set('proxyPassword', e.target.value)} placeholder="留空不改" autoComplete="new-password" />
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
          <Field
            label="userKey（对话 API Key）"
            hint="下游客户端连网关用的 x-api-key。出于安全不回显现值：留空=不改，填写=更新。⚠️需重启服务生效。"
          >
            <div className="flex items-center gap-2">
              <Input
                type="password"
                className="flex-1 min-w-0 max-w-[260px] font-mono text-xs"
                value={form.apiKey}
                onChange={(e) => set('apiKey', e.target.value)}
                placeholder={config.hasApiKey ? '已设置，留空不改' : '未设置，填写以设定'}
                autoComplete="new-password"
              />
              <Badge variant={config.hasApiKey ? 'default' : 'secondary'} className="shrink-0 whitespace-nowrap">
                {config.hasApiKey ? '已设置' : '未设置'}
              </Badge>
            </div>
          </Field>
        </CardContent>
      </Card>
      </SectionGate>

      {/* 基础分区：登录页背景（立即生效，无需重启） */}
      <SectionGate section="basic" title="登录页背景" keywords={['登录背景图', '登录页背景', '背景图', 'r18', '图源', 'lolicon', '关闭登录背景图']}>
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base flex items-center gap-2">
            <ImageIcon className="h-4 w-4 text-muted-foreground" />
            <Highlight text="登录页背景" />
          </CardTitle>
        </CardHeader>
        <CardContent className="py-0">
          <Field
            label="显示登录背景图"
            hint="开启：登录页显示随机背景图；关闭：改用纯渐变背景，不再请求外部图源（保存即时生效，无需重启）"
          >
            <Switch checked={form.loginBackgroundEnabled} onCheckedChange={(v) => set('loginBackgroundEnabled', v)} />
          </Field>
          <Field
            label="R18 图源"
            hint="开启：背景图走 R18 图源（r18=1）；关闭：走全年龄图源（r18=0）。仅在显示背景图开启时有意义（保存即时生效，下一轮预取按新参数取图）"
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
      <SectionGate section="security" title="反代安全" keywords={['cors 允许来源', 'ip 白名单', 'cidr', '信任 x-forwarded-for', 'xff', '入口限流', '请求体上限', '413', '429']}>
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base"><Highlight text="反代安全" /></CardTitle>
        </CardHeader>
        <CardContent className="py-0">
          <Field
            label="凭据落盘加密"
            hint="开启后 credentials.json / trash.json 用机器绑定密钥加密落盘（XChaCha20-Poly1305）。防文件被拷走/误传泄露 token。保存即时生效并立即重写文件；导出/导入走明文不受影响。⚠️换机器后密文解不开（需用明文备份重新导入，与 token 换机失效同理）。默认关。"
          >
            <Switch
              checked={form.encryptCredentialsAtRest}
              onCheckedChange={(v) => set('encryptCredentialsAtRest', v)}
            />
          </Field>
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
          <Field label="请求体上限 (字节)" hint="0 = 不限制（推荐，大请求体走流式转发不占内存）。非 0 时超限返回 413（需重启生效）">
            <NumberStepper value={Number(form.maxBodyBytes) || 0} onChange={(v) => set('maxBodyBytes', String(v))} min={0} step={1048576} className="w-40" aria-label="请求体上限（字节，0=不限制）" />
          </Field>
        </CardContent>
      </Card>
      </SectionGate>

      {/* 调度分区：主动 token 预刷新（TIER2 热重载：保存即时生效，无需重启） */}
      <SectionGate section="scheduling" title="主动 token 预刷新" keywords={['启用预刷新', '提前量', '扫描间隔', 'token 刷新']}>
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base"><Highlight text="主动 token 预刷新" /></CardTitle>
        </CardHeader>
        <CardContent className="py-0">
          <Field label="启用预刷新" hint="后台提前刷新将过期的 token，把刷新移出请求热路径、削掉突发（保存即时生效，无需重启）">
            <Switch checked={form.proactiveTokenRefresh} onCheckedChange={(v) => set('proactiveTokenRefresh', v)} />
          </Field>
          <Field label="提前量 (分钟)" hint="token 剩余有效期低于此值即后台刷新（保存即时生效，无需重启）">
            <NumberStepper value={Number(form.tokenRefreshLeadMinutes) || 0} onChange={(v) => set('tokenRefreshLeadMinutes', String(v))} min={0} className="w-28" disabled={!form.proactiveTokenRefresh} aria-label="提前量分钟" />
          </Field>
          <Field label="扫描间隔 (秒)" hint="后台扫描周期，最小 5 秒（保存即时生效，无需重启）">
            <NumberStepper value={Number(form.tokenRefreshIntervalSecs) || 0} onChange={(v) => set('tokenRefreshIntervalSecs', String(v))} min={5} step={5} className="w-28" disabled={!form.proactiveTokenRefresh} aria-label="扫描间隔秒" />
          </Field>
        </CardContent>
      </Card>
      </SectionGate>

      {/* 搜索且全无匹配时的空态 */}
      {query && !hasAnyMatch && (
        <div className="py-16 text-center text-sm text-muted-foreground">
          未找到匹配的设置项，换个关键词试试。
        </div>
      )}

      {!query && (
        <p className="text-xs text-muted-foreground">
          除负载均衡模式与隐私开关立即生效外，其余字段保存后需重启服务才生效。敏感字段（API/Admin 密钥、代理账密）出于安全不回显已存值：代理账密留空表示保持不变，填入则更新；API/Admin 密钥仍请在配置文件中维护。
        </p>
      )}

      {/* 底部保存栏：仅覆盖 main 内容区（left-[240px] 避开 240px 侧栏，
          否则会盖住侧栏底部“网关在线”状态条造成重叠）；z-30 低于侧栏 z-40。 */}
      <div className="fixed bottom-0 left-0 right-0 z-30 border-t bg-background/95 px-6 py-3 backdrop-blur md:left-[240px]">
        <div className="mx-auto flex max-w-[1200px] items-center justify-end gap-3">
          <span className="mr-auto text-sm text-muted-foreground">
            {dirty ? `${dirtyCount} 项改动待保存` : '无改动'}
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
    </ActiveSectionContext.Provider>
    </SearchContext.Provider>
  )
}
