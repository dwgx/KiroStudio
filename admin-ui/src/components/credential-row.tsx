import { useState, useRef, useCallback, useMemo } from 'react'
import { useTranslation } from 'react-i18next'
import { useQuery, useQueryClient } from '@tanstack/react-query'
import { toast } from 'sonner'
import {
  ChevronRight,
  ShieldCheck,
  Wallet,
  MoreHorizontal,
  Loader2,
  Pencil,
  Copy,
  Ban,
  Power,
  Trash2,
  Eye,
  Boxes,
  Globe,
  Network,
  CopyPlus,
  ClipboardCopy,
} from 'lucide-react'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Checkbox } from '@/components/ui/checkbox'
import { NumberStepper } from '@/components/ui/number-stepper'
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuSub,
  DropdownMenuSubContent,
  DropdownMenuSubTrigger,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import type { BalanceResponse, CredentialStatusItem } from '@/types/api'
import { cn, copyToClipboard, extractErrorMessage } from '@/lib/utils'
import { formatAmount, formatCachedAt, formatCredits, formatLastUsed, maskProxyUrl } from '@/lib/credential-format'
import { authShortLabel, disabledReasonLabel, subscriptionLabel } from '@/lib/i18n-labels'
import {
  cloneCredential,
  listSocksNodes,
  setCredentialAllowedModels,
  PROBE_MODEL_CATALOG,
  exportCredential,
} from '@/api/credentials'
import {
  useDeleteCredential,
  useSetCredentialEndpoint,
  useSetDisabled,
} from '@/hooks/use-credentials'
import { useDeepVerify, useSetProxy } from '@/hooks/use-credential-ops'
import { useRatelimitInsights, useUsageByCredential } from '@/hooks/use-usage'

/**
 * 紧凑行视图的行主体 —— 一行一个号，适合 50+ 号横向对比。
 *
 * 与卡片视图的关系：**卡片行为逐字不变**。`credential-card.tsx` 只在 `view === 'row'`
 * 时用本组件替换 `<Card>` 主体，三个弹框（设置 / 删除确认 / 超额确认）仍由卡片持有，
 * 故"编辑…"通过 `onEdit` 回调复用卡片那份设置弹框，不在此处重造 400 行表单。
 *
 * 数据来源（三条现成通道，**零后端改动**）：
 * - `useCredentials()`（父级已有）→ id / 端点 / 在飞 / rpm / 禁用态 / 冷却
 * - `useRatelimitInsights()`（10s 轮询，读内存零上游）→ 真实熔断态 / recent429 / 健康分
 * - `useUsageByCredential()`（30s 轮询，读本地统计）→ 成功率
 * 三个 query 的 key 与运维页/统计页相同 → react-query 去重，N 行只有 1 份网络请求。
 */
export interface CredentialRowBodyProps {
  credential: CredentialStatusItem
  selected: boolean
  onToggleSelect: () => void
  /**
   * Shift+左键区间选：把 [锚点, 本行] 闭区间并入选区。
   *
   * 刻意传**回调**而不是 `orderedIds: number[]`：区间顺序与「哪些可选」是调用方
   * （`dashboard.tsx`）的知识 —— 它才知道当前页有哪些行、哪些是 disabled
   * （store 契约要求 `orderedIds` 已剔除 disabled，见 use-credential-selection.ts:76-78）。
   * 传数组会让每行都拿到同一份数组、且每次轮询换引用；传回调则 12 行零额外开销。
   * 未提供时 Shift+左键什么都不做（安全默认）。
   */
  onRangeSelect?: () => void
  /** 打开卡片持有的「设置」弹框（复用，不重造表单）。 */
  onEdit: () => void
  /** 打开余额弹框（与卡片「查看余额」同一入口）。 */
  onViewBalance: (id: number) => void
  /** 展示用余额：按需拉取优先，否则后台缓存快照。由卡片算好传入，避免两处口径分叉。 */
  shownBalance: BalanceResponse | null
  balancePending: boolean
  /** 缓存快照时刻（Unix 秒）；按需拉取的实时余额为 null。 */
  cachedAt: number | null
  /** 可选端点名（后端注册表驱动，不硬编码 ide/cli）。 */
  endpointNames: string[]
  /** 多选批量：选中 >1 个时右键菜单文案改为「禁用选中的 N 个」。 */
  batch?: {
    count: number
    onBatchDisable: () => void
    onBatchDelete: () => void
  }
}

/** 行状态四态。与卡片视图同口径（卡片的判据散在 badge/pill/ring 里，这里合成一个点）。 */
type RowStatus = 'healthy' | 'cooldown' | 'disabled' | 'halfOpen'

/**
 * 状态点：**形状**区分状态，**颜色**沿用卡片视图（全站同一状态同色）。
 *
 * 颜色对齐依据（`credential-card.tsx`）：
 * - 禁用 → `Badge variant="destructive"` 是红 ⇒ 这里红
 * - 冷却「速率限制」→ pill `text-amber-400` ⇒ 琥珀；其它冷却原因 → `text-red-400` ⇒ 红
 * - 健康/当前活跃 → `ring-emerald-500` ⇒ 翠绿
 *
 * ⚠️ 禁用与"非速率限制冷却"在卡片里**本来就同为红**，只靠颜色分不开。故形状必须承担
 * 区分职责：● 实心=健康 / ◐ 半填=冷却 / ○ 空心=禁用 / ◒ 虚线环=熔断半开。
 * 另配 `title` + `aria-label` 给读屏与 hover。
 */
function StatusDot({ status, rateLimited }: { status: RowStatus; rateLimited: boolean }) {
  const { t } = useTranslation()
  const label = t(`credentialrow.status.${status}`)
  // 冷却：速率限制琥珀、其它红（与卡片 cooldown pill 完全同判据）。
  const cooldownTone = rateLimited ? 'amber' : 'red'
  const cls =
    status === 'healthy'
      ? 'bg-emerald-500 border-emerald-500'
      : status === 'disabled'
        ? 'bg-transparent border-red-500'
        : status === 'halfOpen'
          ? 'bg-transparent border-dashed border-amber-400'
          : cooldownTone === 'amber'
            ? 'border-amber-400 bg-gradient-to-r from-amber-400 from-50% to-transparent to-50%'
            : 'border-red-500 bg-gradient-to-r from-red-500 from-50% to-transparent to-50%'
  return (
    <span
      role="img"
      aria-label={label}
      title={label}
      className={cn('inline-block h-2.5 w-2.5 shrink-0 rounded-full border-[1.5px]', cls)}
    />
  )
}

/**
 * 微型余额条：一眼看出哪个号快空了（行视图刻意不放数字，数字进 hover title 与展开区）。
 *
 * 阈值与配色**与卡片 `renderBalanceBar` 逐字同口径**：剩余 ≥40% 翠绿 / ≥20% 黄 / 否则红。
 * 语义是"余量"，越满越健康（与"用量条"相反），故不能用 `<Progress>` 的默认反向阈值。
 */
function BalanceMicroBar({
  balance,
  pending,
  cachedAt,
}: {
  balance: BalanceResponse | null
  pending: boolean
  cachedAt: number | null
}) {
  const { t } = useTranslation()
  if (pending) {
    return <div className="h-1.5 w-full animate-pulse rounded-full bg-secondary" />
  }
  if (!balance) {
    return (
      <div
        className="h-1.5 w-full rounded-full border border-dashed border-border"
        title={t('credentialcard.balanceBar.noData')}
      />
    )
  }
  const limit = balance.usageLimit
  const remaining = balance.remaining
  const pct = limit > 0 ? Math.min(Math.max((remaining / limit) * 100, 0), 100) : 0
  const fill = pct >= 40 ? 'bg-emerald-500' : pct >= 20 ? 'bg-yellow-500' : 'bg-red-500'
  const freshness = cachedAt
    ? t('credentialcard.balanceBar.asOf', { time: formatCachedAt(cachedAt) })
    : t('credentialcard.balanceBar.realtime')
  return (
    <div
      className="h-1.5 w-full overflow-hidden rounded-full bg-secondary"
      title={`${formatAmount(remaining)} / ${formatAmount(limit)} · ${pct.toFixed(1)}% · ${freshness}`}
    >
      <div className={cn('h-full transition-all duration-500 ease-out-expo', fill)} style={{ width: `${pct}%` }} />
    </div>
  )
}

export function CredentialRowBody({
  credential,
  selected,
  onToggleSelect,
  onRangeSelect,
  onEdit,
  onViewBalance,
  shownBalance,
  balancePending,
  cachedAt,
  endpointNames,
  batch,
}: CredentialRowBodyProps) {
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  const rowRef = useRef<HTMLDivElement | null>(null)
  // 右键菜单：受控 open + 虚拟锚点坐标（不引 @radix-ui/react-context-menu，见 ui/dropdown-menu.tsx）。
  const [menuOpen, setMenuOpen] = useState(false)
  const [menuPos, setMenuPos] = useState<{ x: number; y: number } | null>(null)
  const [expanded, setExpanded] = useState(false)
  const [showDelete, setShowDelete] = useState(false)
  const [showModels, setShowModels] = useState(false)
  const [modelSel, setModelSel] = useState<Set<string>>(new Set())
  const [savingModels, setSavingModels] = useState(false)
  const [showClone, setShowClone] = useState(false)
  const [cloneCopies, setCloneCopies] = useState(1)
  const [cloneBusy, setCloneBusy] = useState(false)
  // 点击掩码复制完整 Key：防重复点击（敏感导出端点，与设置页 copyOne 同模式）。
  const [copyKeyBusy, setCopyKeyBusy] = useState(false)

  const setDisabled = useSetDisabled()
  const setEndpoint = useSetCredentialEndpoint()
  const deleteCredential = useDeleteCredential()
  const deepVerify = useDeepVerify()
  const setProxy = useSetProxy()

  // 三条现成通道（key 与运维页/统计页一致 → react-query 去重，N 行 1 份请求）。
  const { data: insights } = useRatelimitInsights()
  const { data: byCred } = useUsageByCredential()
  // 出口 IP 子菜单的候选池：只在菜单打开后才拉（12 行同时挂载不会白发 12 次；key 与分身管理页共用）。
  const { data: socksResp } = useQuery({
    queryKey: ['socks-nodes'],
    queryFn: listSocksNodes,
    enabled: menuOpen,
  })

  const insight = useMemo(
    () => insights?.find((it) => it.id === credential.id) ?? null,
    [insights, credential.id]
  )
  // 成功率来自**用量库**（GroupStat.success_rate，0~1），与 successCount/failureCount
  // （凭据生命周期累计）不同源，不可混用。requests=0 时显示 —— 而非 0%（无样本 ≠ 全失败）。
  const usageStat = useMemo(
    () => byCred?.find((g) => g.key === String(credential.id)) ?? null,
    [byCred, credential.id]
  )
  const successPct =
    usageStat && usageStat.requests > 0 ? Math.round(usageStat.success_rate * 100) : null

  // 状态合成。判定顺序即优先级：禁用 > 熔断半开 > 冷却 > 健康。
  // 禁用排最前是刻意的：禁用号不参与调度，它的冷却/熔断态对用户没有决策意义。
  const rateLimited = credential.cooldownReason === '速率限制'
  const status: RowStatus = credential.disabled
    ? 'disabled'
    : insight?.health?.halfOpen
      ? 'halfOpen'
      : credential.coolingDown
        ? 'cooldown'
        : 'healthy'

  /**
   * 最近错误**聚合**（"429 ×14"），而非最后一条原文。
   *
   * 🔴 数据缺口（不改后端）：后端没有"按错误类型分桶"的聚合字段。这里用三个现成计数器
   * 近似，按信息量从高到低取第一个非零：
   * 1. `insight.recent429` —— 速率限制冷却的连续触发计数（最贴近"最近"）
   * 2. `insight.health.consecutive429` —— 族级连续 429
   * 3. `credential.failureCount` —— 生命周期累计失败（**不是"最近"**，故文案用「失败」不用「最近」）
   * 三者口径不同，不能相加。title 里写明取的是哪一个。
   */
  const errAgg: { text: string; cls: string; title: string } | null = (() => {
    const r429 = insight?.recent429 ?? 0
    if (r429 > 0) {
      return {
        text: `429 ×${r429}`,
        cls: 'text-amber-400',
        title: t('credentialrow.err.recent429Title'),
      }
    }
    const c429 = insight?.health?.consecutive429 ?? 0
    if (c429 > 0) {
      return {
        text: `429 ×${c429}`,
        cls: 'text-amber-300',
        title: t('credentialrow.err.consecutive429Title'),
      }
    }
    const fails = credential.failureCount + credential.refreshFailureCount
    if (fails > 0) {
      return {
        text: t('credentialrow.err.failures', { n: fails }),
        cls: 'text-red-400',
        title: t('credentialrow.err.failuresTitle', {
          failures: credential.failureCount,
          refreshFailures: credential.refreshFailureCount,
        }),
      }
    }
    return null
  })()

  const isCustomApi = credential.authMethod === 'custom_api' || !!credential.baseUrl
  // 禁用号的「测活」「刷余额」**灰掉而不隐藏**（隐藏会让菜单项位置漂移、肌肉记忆失效）。
  const probeDisabled = credential.disabled
  const balanceDisabled = credential.disabled || isCustomApi

  // 行内交互控件命中判定：点按钮/勾选框/菜单时不触发行级展开或选中。
  const INTERACTIVE = 'button, input, a, [role="checkbox"], [role="menu"], [role="menuitem"]'

  const openMenuAt = useCallback((x: number, y: number) => {
    setMenuPos({ x, y })
    setMenuOpen(true)
  }, [])

  const handleContextMenu = (e: React.MouseEvent<HTMLDivElement>) => {
    if ((e.target as HTMLElement).closest(INTERACTIVE)) return
    // 🔴 macOS：Ctrl+左键**同时**派发 `contextmenu`（系统级"辅助点击"）。若不在这里让路，
    // Ctrl+左键多选会顺带弹出右键菜单，多选一个号就要按一次 Esc ⇒ 该交互实际不可用。
    // Cmd 一并挡掉：Mac 用户的多选肌肉记忆是 Cmd，让两个修饰键行为一致。
    // 代价是"Ctrl+右键"开不了菜单——裸右键仍然开，且这条组合本来也不是任何平台的约定。
    if (e.ctrlKey || e.metaKey) {
      // 必须 preventDefault：本分支让路后浏览器**仍会派发原生 contextmenu**，
      // 不拦的话 macOS 的"辅助点击"会照弹原生右键菜单 —— 原来的注释声称
      // 修掉了「多选要按 Esc」，实际只是把自定义菜单换成了原生菜单。
      // 与 handleRowClick 的 Ctrl/Cmd 分支配对（那边同样 preventDefault）。
      e.preventDefault()
      return
    }
    e.preventDefault()
    openMenuAt(e.clientX, e.clientY)
  }

  /**
   * 行级左键：**Shift 区间选 > Ctrl/Cmd 加减选 > 裸左键什么都不做**。
   *
   * 裸左键刻意留空（不展开详情）：与卡片档 `handleCardClick` 同口径（卡片也只在按住
   * Ctrl/Cmd 时才响应左键）。展开走行首 ▸ 或 Enter/Space，两者都在 `INTERACTIVE`
   * 判定之外单独接线 —— 若裸左键也展开，则框选起手落在行上时会顺带展开一行。
   *
   * Shift 优先于 Ctrl 是各家一致的约定（Finder/资源管理器/Polaris）：两键同按时
   * 用户意图是"拉区间"，不是"切一个"。
   */
  const handleRowClick = (e: React.MouseEvent<HTMLDivElement>) => {
    if ((e.target as HTMLElement).closest(INTERACTIVE)) return
    if (e.shiftKey) {
      // preventDefault 抑制 shift+点击的原生文本区间选择（会把整片行文本刷蓝）。
      e.preventDefault()
      onRangeSelect?.()
      return
    }
    if (e.ctrlKey || e.metaKey) {
      // 与上面 handleContextMenu 的让路配对：这里也 preventDefault，
      // 避免 macOS 把这一下算成辅助点击后再走浏览器默认行为。
      e.preventDefault()
      onToggleSelect()
    }
  }

  // 键盘可达：Enter/Space 展开；Shift+F10 与「菜单键」开右键菜单（锚点取行的右上角）。
  const handleKeyDown = (e: React.KeyboardEvent<HTMLDivElement>) => {
    if ((e.target as HTMLElement).closest(INTERACTIVE)) return
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault()
      setExpanded((v) => !v)
      return
    }
    if (e.key === 'ContextMenu' || (e.shiftKey && e.key === 'F10')) {
      e.preventDefault()
      const r = rowRef.current?.getBoundingClientRect()
      openMenuAt(r ? r.right - 24 : 0, r ? r.top + r.height : 0)
    }
  }

  const runVerify = () =>
    deepVerify.mutate(credential.id, {
      onSuccess: () => toast.success(t('credentialrow.toast.verifyOk', { id: credential.id })),
      onError: (err) => toast.error(t('credentialrow.toast.verifyFail') + extractErrorMessage(err)),
    })

  // 「刷新余额」= 失效后端【已缓存】余额快照并重拉（零上游、不封号）。
  // 刻意不打 per-account balance 端点：那是封号红线，卡片的「查看余额」才是显式单号查询。
  const refreshBalance = () => {
    queryClient.invalidateQueries({ queryKey: ['cached-balances'] })
    toast.success(t('credentialrow.toast.balanceRefreshing'))
  }

  // 点击掩码复制完整 Key：exportCredential 拿真值（与设置页 copyOne 同模式），
  // 取 kiroApiKey 字段（后端 export 返回 camelCase KiroCredentials，只有 api_key 号有掩码）。
  // i18n: credentialcard.toast.apiKeyCopied / apiKeyCopyFailed / apiKeyMissing（主会话补三语）
  const handleCopyFullKey = async () => {
    if (copyKeyBusy) return
    setCopyKeyBusy(true)
    try {
      const obj = await exportCredential(credential.id)
      const key = typeof obj.kiroApiKey === 'string' ? obj.kiroApiKey : ''
      if (!key) {
        toast.error('该凭据没有可复制的完整 Key')
        return
      }
      const ok = await copyToClipboard(key)
      ok ? toast.success('已复制完整 Key') : toast.error('复制失败')
    } catch (err) {
      toast.error(extractErrorMessage(err))
    } finally {
      setCopyKeyBusy(false)
    }
  }

  const toggleDisabled = () =>
    setDisabled.mutate(
      { id: credential.id, disabled: !credential.disabled },
      {
        onSuccess: (res) => toast.success(res.message),
        onError: (err) => toast.error(extractErrorMessage(err)),
      }
    )

  const handleDelete = () => {
    // 与卡片同一道门：未禁用不允许删（后端也有此门，前端先给可读提示）。
    if (!credential.disabled) {
      toast.error(t('credentialcard.toast.disableBeforeDelete'))
      setShowDelete(false)
      return
    }
    deleteCredential.mutate(credential.id, {
      onSuccess: (res) => {
        toast.success(res.message)
        setShowDelete(false)
      },
      onError: (err) => toast.error(t('credentialcard.toast.deleteFailed') + extractErrorMessage(err)),
    })
  }

  const applyProxy = (url: string | null) =>
    setProxy.mutate(
      { id: credential.id, proxyUrl: url },
      {
        onSuccess: () =>
          toast.success(
            url === null
              ? t('credentialcard.toast.proxyCleared')
              : t('credentialcard.toast.proxySaved')
          ),
        onError: (err) => toast.error(t('credentialcard.toast.proxySaveFailed') + extractErrorMessage(err)),
      }
    )

  const handleSaveModels = async () => {
    setSavingModels(true)
    try {
      const list = Array.from(modelSel)
      await setCredentialAllowedModels(credential.id, list.length === 0 ? null : list)
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
      toast.success(
        list.length === 0
          ? t('credentialrow.models.clearedToast')
          : t('credentialrow.models.savedToast', { n: list.length })
      )
      setShowModels(false)
    } catch (err) {
      toast.error(extractErrorMessage(err))
    } finally {
      setSavingModels(false)
    }
  }

  const handleClone = async () => {
    setCloneBusy(true)
    try {
      // enabled 刻意不传：默认值只由后端一份持有（新分身未绑出口未验活，不该直接入池）。
      const res = await cloneCredential(credential.id, cloneCopies)
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
      toast.success(res.message)
      setShowClone(false)
    } catch (err) {
      toast.error(extractErrorMessage(err))
    } finally {
      setCloneBusy(false)
    }
  }

  const title = credential.name || credential.email || t('credentialcard.title.fallback', { id: credential.id })

  return (
    <>
      <div
        ref={rowRef}
        role="row"
        tabIndex={0}
        aria-selected={selected}
        aria-expanded={expanded}
        // 拖拽框选的命中测试靠这个属性找行 + 读 `getBoundingClientRect()`（见 dashboard.tsx
        // 的 marquee 段）。行视图没有画布那种 store 化几何，12 行量级读 DOM 完全够；
        // 同时它也是「起手点是否落在空白」的判据（`closest('[role="row"]')`）。
        data-cred-id={credential.id}
        onContextMenu={handleContextMenu}
        onClick={handleRowClick}
        onKeyDown={handleKeyDown}
        className={cn(
          'group rounded-lg border bg-card transition-[background-color,border-color] duration-200',
          'hover:border-border-hover focus:outline-none focus-visible:ring-2 focus-visible:ring-ring',
          selected && 'ring-2 ring-primary bg-primary/[0.04]',
          credential.isCurrent && !selected && 'ring-1 ring-emerald-500/60',
          credential.disabled && 'opacity-70'
        )}
      >
        {/* role="presentation"：本 div 只做 flex 布局。去掉它的语义后，内部 role="cell"
            会被最近的真实祖先（外层 role="row"）直接拥有 —— 否则中间夹一层无角色 div 会打断
            row → cell 的 ARIA 归属关系。本元素既不可聚焦也无 aria-* 全局属性，故 presentation 生效。 */}
        <div role="presentation" className="flex items-center gap-2 px-2 py-1.5 text-xs">
          {/* 行首 ▸：内联展开详情（不弹窗，弹窗会丢上下文） */}
          <span role="cell" className="flex shrink-0 items-center">
            <button
              type="button"
              onClick={() => setExpanded((v) => !v)}
              aria-expanded={expanded}
              aria-label={expanded ? t('credentialrow.collapse') : t('credentialrow.expand')}
              title={expanded ? t('credentialrow.collapse') : t('credentialrow.expand')}
              className="flex h-5 w-5 shrink-0 items-center justify-center rounded text-muted-foreground hover:bg-accent hover:text-foreground"
            >
              <ChevronRight className={cn('h-3.5 w-3.5 transition-transform', expanded && 'rotate-90')} />
            </button>
          </span>
          <span role="cell" className="flex shrink-0 items-center">
            <Checkbox checked={selected} onCheckedChange={onToggleSelect} className="shrink-0" />
          </span>
          <span role="cell" className="w-10 shrink-0 font-mono tabular-nums text-muted-foreground">#{credential.id}</span>
          <span role="cell" className="flex shrink-0 items-center">
            <StatusDot status={status} rateLimited={rateLimited} />
          </span>
          {/* 身份 + 关键徽标（占满剩余宽） */}
          <span role="cell" className="flex min-w-0 flex-1 items-center gap-1.5">
            <span className="min-w-0 truncate font-medium" title={credential.email || title}>
              {title}
            </span>
            {credential.isCurrent && (
              <Badge variant="success" className="shrink-0 px-1 py-0 text-[10px]">
                {t('credentialcard.badge.current')}
              </Badge>
            )}
            {credential.disabled && (
              <Badge variant="destructive" className="shrink-0 px-1 py-0 text-[10px]">
                {credential.disabledReason
                  ? disabledReasonLabel(credential.disabledReason)
                  : t('credentialcard.badge.disabled')}
              </Badge>
            )}
          </span>
          {/* 端点：实际生效值 + ·auto 后缀（与卡片同口径，区分"系统选的"与"我固定的"） */}
          <span
            role="cell"
            className="hidden w-20 shrink-0 truncate font-mono text-muted-foreground md:block"
            title={
              credential.endpointPinned
                ? t('credentialcard.endpoint.pinnedTitle', { name: credential.endpoint })
                : t('credentialcard.endpoint.autoTitle', { name: credential.endpoint })
            }
          >
            {credential.endpoint}
            {credential.endpointPinned === false && <span className="opacity-50">·auto</span>}
          </span>
          {/* 区域：显示**实际生效**的 region（真正拼进 host 的值），与端点列同款语义。
              `·auto` = 没人为这个号定过区、现值只是 config 全局回退 —— 这类号正是
              region 探测缺口的受害者（ksk_ 按区授权，打错区恒 403）。 */}
          <span
            role="cell"
            className="hidden w-16 shrink-0 truncate text-muted-foreground xl:block"
            title={
              credential.effectiveRegion
                ? credential.regionPinned === false
                  ? t('credentialrow.col.regionAutoTitle', {
                      region: credential.effectiveRegion,
                    })
                  : credential.effectiveRegion
                : t('credentialrow.col.regionUnavailable')
            }
          >
            {credential.effectiveRegion ? (
              <>
                {credential.effectiveRegion}
                {credential.regionPinned === false && (
                  <span className="opacity-50">·auto</span>
                )}
              </>
            ) : (
              '—'
            )}
          </span>
          {/* RPM：rpm / rpmLimit。⚠️ rpmLimit 是**软上限配置值**，真实有效阈值另含 headroom 折扣，
              前端不自乘（口径见 CLAUDE.md「容量口径是假的」）。 */}
          <span
            role="cell"
            className="hidden w-16 shrink-0 tabular-nums text-muted-foreground lg:block"
            title={t('credentialrow.col.rpmTitle')}
          >
            {credential.rpm ?? 0}
            {(credential.rpmLimit ?? 0) > 0 && <span className="opacity-50">/{credential.rpmLimit}</span>}
          </span>
          {/* 成功率（用量库口径，非 successCount/failureCount） */}
          <span
            role="cell"
            className={cn(
              'hidden w-12 shrink-0 text-right tabular-nums lg:block',
              successPct === null
                ? 'text-muted-foreground'
                : successPct >= 90
                  ? 'text-emerald-400'
                  : successPct >= 60
                    ? 'text-amber-400'
                    : 'text-red-400'
            )}
            title={t('credentialrow.col.successRateTitle', { n: usageStat?.requests ?? 0 })}
          >
            {successPct === null ? '—' : `${successPct}%`}
          </span>
          {/* 余额：微型条（数字进 title 与展开区） */}
          <span role="cell" className="hidden w-24 shrink-0 xl:block">
            {isCustomApi ? (
              <span className="text-muted-foreground" title={t('credentialrow.col.balanceNaCustomApi')}>
                —
              </span>
            ) : (
              <BalanceMicroBar balance={shownBalance} pending={balancePending} cachedAt={cachedAt} />
            )}
          </span>
          {/* 在飞 */}
          <span role="cell" className="hidden w-10 shrink-0 text-right tabular-nums lg:block" title={t('credentialcard.info.inflightTitle')}>
            {(credential.inflight ?? 0) > 0 ? (
              <span className="inline-flex items-center gap-1 text-sky-400">
                <span className="h-1.5 w-1.5 rounded-full bg-sky-500 animate-pulse" />
                {credential.inflight}
              </span>
            ) : (
              <span className="text-muted-foreground">0</span>
            )}
          </span>
          {/* 最近错误聚合 */}
          <span role="cell" className="hidden w-20 shrink-0 truncate text-right xl:block" title={errAgg?.title}>
            {errAgg ? <span className={errAgg.cls}>{errAgg.text}</span> : <span className="text-muted-foreground">—</span>}
          </span>
          {/* 行内快捷操作：hover/focus 时浮出**恰好 3 个**（测活 / 刷余额 / 更多），不常驻。
              第 4 个起一律进右键菜单。`focus-within` 保证键盘 Tab 进来时也能看见（纯 hover 会键盘不可达）。
              占位宽固定（w-[76px]）→ 浮出时右侧列不位移。 */}
          <span role="cell" className="flex w-[76px] shrink-0 items-center justify-end gap-0.5 opacity-0 transition-opacity group-hover:opacity-100 group-focus-within:opacity-100">
            <Button
              size="sm"
              variant="ghost"
              className="h-6 w-6 p-0 text-muted-foreground hover:text-emerald-400"
              onClick={runVerify}
              disabled={probeDisabled || deepVerify.isPending}
              title={probeDisabled ? t('credentialrow.action.verifyDisabledTitle') : t('credentialrow.action.verify')}
              aria-label={t('credentialrow.action.verify')}
            >
              {deepVerify.isPending ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <ShieldCheck className="h-3.5 w-3.5" />}
            </Button>
            <Button
              size="sm"
              variant="ghost"
              className="h-6 w-6 p-0 text-muted-foreground hover:text-sky-400"
              onClick={refreshBalance}
              disabled={balanceDisabled}
              title={balanceDisabled ? t('credentialrow.action.balanceDisabledTitle') : t('credentialrow.action.refreshBalance')}
              aria-label={t('credentialrow.action.refreshBalance')}
            >
              <Wallet className="h-3.5 w-3.5" />
            </Button>
            <Button
              size="sm"
              variant="ghost"
              className="h-6 w-6 p-0 text-muted-foreground hover:text-foreground"
              onClick={(e) => {
                const r = (e.currentTarget as HTMLElement).getBoundingClientRect()
                openMenuAt(r.right, r.bottom)
              }}
              title={t('credentialrow.action.more')}
              aria-label={t('credentialrow.action.more')}
            >
              <MoreHorizontal className="h-3.5 w-3.5" />
            </Button>
          </span>
        </div>
        {/* 内联展开详情：就地铺开，不弹窗（弹窗会丢失"这一行在列表里的位置"这个上下文）。
            只渲染在展开时 —— 50+ 号全部常驻会白挂几百个 DOM 节点。 */}
        {expanded && (
          <div role="cell" className="animate-rise-in border-t px-9 py-2">
            <dl className="grid grid-cols-2 gap-x-6 gap-y-1 text-xs sm:grid-cols-3 lg:grid-cols-4">
              <DetailItem label={t('credentialcard.info.priority')} value={String(credential.priority)} />
              <DetailItem
                label={t('credentialrow.detail.auth')}
                value={
                  credential.authMethod
                    ? credential.authMethod === 'api_key'
                      ? 'API Key'
                      : authShortLabel(credential.authMethod)
                    : '—'
                }
              />
              {!isCustomApi && (
                <DetailItem
                  label={t('credentialcard.info.subscriptionLevel')}
                  value={subscriptionLabel(shownBalance?.subscriptionTitle ?? credential.subscriptionTitle ?? null)}
                />
              )}
              {!isCustomApi && shownBalance && (
                <DetailItem
                  label={t('credentialcard.balanceBar.remainingUsage')}
                  value={`${formatAmount(shownBalance.remaining)} / ${formatAmount(shownBalance.usageLimit)}`}
                />
              )}
              <DetailItem label={t('credentialcard.info.successCount')} value={String(credential.successCount)} />
              <DetailItem
                label={t('credentialcard.info.failureCount')}
                value={String(credential.failureCount)}
                tone={credential.failureCount > 0 ? 'bad' : undefined}
              />
              {!isCustomApi && (
                <DetailItem
                  label={t('credentialcard.info.refreshFailure')}
                  value={String(credential.refreshFailureCount)}
                  tone={credential.refreshFailureCount > 0 ? 'bad' : undefined}
                />
              )}
              {!isCustomApi && (
                <DetailItem
                  label={t('credentialcard.info.totalCredits')}
                  value={`${formatCredits(credential.totalCreditsUsed)} credits`}
                />
              )}
              <DetailItem label={t('credentialcard.info.lastCall')} value={formatLastUsed(credential.lastUsedAt)} />
              {insight?.health && (
                <DetailItem
                  label={t('credentialrow.detail.healthScore')}
                  value={`${Math.round(insight.health.health * 100)}%`}
                  tone={insight.health.health < 0.6 ? 'bad' : undefined}
                />
              )}
              {insight?.insightText && (
                <DetailItem
                  label={t('credentialrow.detail.insight')}
                  value={insight.insightText}
                  className="col-span-2 lg:col-span-2"
                />
              )}
              {credential.coolingDown && credential.cooldownReason && (
                <DetailItem
                  label={t('credentialcard.cooldown.label')}
                  value={`${credential.cooldownReason} · ${Math.ceil((credential.cooldownRemainingMs ?? 0) / 1000)}s`}
                  tone={rateLimited ? 'warn' : 'bad'}
                />
              )}
              {credential.allowedModels && credential.allowedModels.length > 0 && (
                <DetailItem
                  label={t('credentialcard.info.allowedModels')}
                  value={credential.allowedModels.join(', ')}
                  className="col-span-2"
                />
              )}
              {credential.maskedApiKey && (
                /* 点击掩码复制完整 Key（exportCredential 拿真值，与设置页 copyOne 同模式）。
                   i18n: credentialcard.info.copyKeyTitle（主会话补三语） */
                <DetailItem
                  label={t('credentialcard.info.apiKey')}
                  value={credential.maskedApiKey}
                  mono
                  onClick={handleCopyFullKey}
                />
              )}
              {credential.hasProxy && (
                <DetailItem
                  label={t('credentialcard.info.proxy')}
                  value={credential.proxyUrl ? maskProxyUrl(credential.proxyUrl) : t('credentialcard.info.proxyConfigured')}
                  mono
                  action={
                    credential.proxyUrl ? (
                      <Button
                        size="sm"
                        variant="ghost"
                        className="h-5 w-5 shrink-0 self-center p-0"
                        title={t('credentialcard.info.copyProxyTitle')}
                        onClick={async (e) => {
                          e.stopPropagation()
                          const ok = await copyToClipboard(credential.proxyUrl!)
                          ok ? toast.success(t('credentialcard.toast.proxyCopied')) : toast.error(t('credentialcard.toast.copyFailed'))
                        }}
                      >
                        <ClipboardCopy className="h-3 w-3" />
                      </Button>
                    ) : undefined
                  }
                />
              )}
              {isCustomApi && credential.baseUrl && (
                <DetailItem label={t('credentialcard.customApi.baseUrl')} value={credential.baseUrl} mono className="col-span-2" />
              )}
              {isCustomApi && (
                <DetailItem
                  label={t('credentialcard.customApi.requestUsage')}
                  value={
                    credential.requestLimit && credential.requestLimit > 0
                      ? `${credential.requestCount ?? 0} / ${credential.requestLimit}`
                      : String(credential.requestCount ?? 0)
                  }
                />
              )}
              {credential.cloneGroup && credential.cloneSeq && (
                <DetailItem label={t('credentialrow.detail.clone')} value={`#${credential.cloneSeq}`} />
              )}
              {credential.tag && <DetailItem label={t('credentialrow.detail.tag')} value={credential.tag} />}
            </dl>
          </div>
        )}
      </div>
      {/* 右键菜单：受控 open + **虚拟锚点**（0×0 fixed span 放在光标处）让 radix
          dropdown-menu 跟随鼠标定位 —— 从而不必新增 @radix-ui/react-context-menu 依赖。
          关闭时把焦点还给行（onCloseAutoFocus），否则焦点会落到已卸载的锚点上。 */}
      <DropdownMenu open={menuOpen} onOpenChange={setMenuOpen}>
        <DropdownMenuTrigger asChild>
          <span
            aria-hidden
            tabIndex={-1}
            style={{
              position: 'fixed',
              left: menuPos?.x ?? 0,
              top: menuPos?.y ?? 0,
              width: 0,
              height: 0,
            }}
          />
        </DropdownMenuTrigger>
        <DropdownMenuContent
          align="start"
          collisionPadding={8}
          // 🔴 限高 + 内部滚动：菜单 12~16 项 ≈ 421px 高，radix 在视口不够时会 flip 到上方，
          // 但行常在页面中部，上方空间 < 菜单高 ⇒ 菜单顶溢出视口（实测 y=-32），部分菜单项
          // 在屏幕外，看起来"没法用"。限高到视口 65vh 后 flip 总能放下完整菜单。
          className="max-h-[min(65vh,26rem)] overflow-y-auto overscroll-contain"
          onCloseAutoFocus={(e) => {
            e.preventDefault()
            rowRef.current?.focus()
          }}
        >
          {/* 多选后右键 = 批量操作。危险项仍在最下 + 分隔线。 */}
          {batch && batch.count > 1 ? (
            <>
              <DropdownMenuItem onSelect={batch.onBatchDisable}>
                <Ban />
                {t('credentialrow.menu.batchDisable', { count: batch.count })}
              </DropdownMenuItem>
              <DropdownMenuSeparator />
              <DropdownMenuItem destructive onSelect={batch.onBatchDelete}>
                <Trash2 />
                {t('credentialrow.menu.batchDelete', { count: batch.count })}
              </DropdownMenuItem>
            </>
          ) : (
            <>
              {/* 高频只读在最上 */}
              <DropdownMenuItem onSelect={() => setExpanded((v) => !v)}>
                <Eye />
                {t('credentialrow.menu.detail')}
              </DropdownMenuItem>
              <DropdownMenuItem disabled={probeDisabled} onSelect={runVerify}>
                <ShieldCheck />
                {t('credentialrow.menu.verify')}
              </DropdownMenuItem>
              <DropdownMenuItem disabled={balanceDisabled} onSelect={refreshBalance}>
                <Wallet />
                {t('credentialrow.menu.refreshBalance')}
              </DropdownMenuItem>
              {/* 行视图不渲染卡片的「查看余额」按钮 → 余额弹框只能从这里进（… = 需弹窗）。
                  与上一项的区别：这条是**显式单号查询**（打上游），上一条只失效已缓存快照（零上游）。 */}
              <DropdownMenuItem disabled={balanceDisabled} onSelect={() => onViewBalance(credential.id)}>
                <Wallet />
                {t('credentialrow.menu.viewBalance')}
              </DropdownMenuItem>
              <DropdownMenuSeparator />
              {/* 配置类：… = 需弹窗，▸ = 有子菜单 */}
              <DropdownMenuItem onSelect={onEdit}>
                <Pencil />
                {t('credentialrow.menu.edit')}
              </DropdownMenuItem>
              {/* 出口 IP ▸：从「分身管理」维护的代理节点池里挑一个，直接落到该号的单凭证代理。 */}
              <DropdownMenuSub>
                <DropdownMenuSubTrigger>
                  <Network />
                  {t('credentialrow.menu.exitIp')}
                </DropdownMenuSubTrigger>
                <DropdownMenuSubContent>
                  <DropdownMenuItem disabled={setProxy.isPending} onSelect={() => applyProxy(null)}>
                    {t('credentialrow.menu.exitIpGlobal')}
                  </DropdownMenuItem>
                  <DropdownMenuItem disabled={setProxy.isPending} onSelect={() => applyProxy('direct')}>
                    {t('credentialrow.menu.exitIpDirect')}
                  </DropdownMenuItem>
                  {(socksResp?.nodes ?? []).length > 0 && <DropdownMenuSeparator />}
                  {(socksResp?.nodes ?? []).map((n) => (
                    <DropdownMenuItem
                      key={n.id}
                      disabled={setProxy.isPending || !n.enabled}
                      onSelect={() => applyProxy(n.url)}
                      title={n.enabled ? n.url : t('credentialrow.menu.exitIpNodeDisabled')}
                    >
                      {n.label}
                    </DropdownMenuItem>
                  ))}
                  {(socksResp?.nodes ?? []).length === 0 && (
                    <DropdownMenuItem disabled>{t('credentialrow.menu.exitIpEmpty')}</DropdownMenuItem>
                  )}
                </DropdownMenuSubContent>
              </DropdownMenuSub>
              {/* 端点 ▸：自动 + 后端注册表给出的端点名（不硬编码 ide/cli）。custom_api 不走端点体系。 */}
              <DropdownMenuSub>
                <DropdownMenuSubTrigger disabled={isCustomApi}>
                  <Globe />
                  {t('credentialrow.menu.endpoint')}
                </DropdownMenuSubTrigger>
                <DropdownMenuSubContent>
                  <DropdownMenuItem
                    disabled={setEndpoint.isPending || credential.endpointPinned === false}
                    onSelect={() =>
                      setEndpoint.mutate(
                        { id: credential.id, endpoint: null },
                        { onSuccess: (r) => toast.success(r.message), onError: (e) => toast.error(extractErrorMessage(e)) }
                      )
                    }
                  >
                    {t('credentialcard.settings.endpointAuto')}
                  </DropdownMenuItem>
                  {endpointNames.map((name) => (
                    <DropdownMenuItem
                      key={name}
                      disabled={
                        setEndpoint.isPending ||
                        (credential.endpointPinned === true && credential.endpoint === name)
                      }
                      onSelect={() =>
                        setEndpoint.mutate(
                          { id: credential.id, endpoint: name },
                          { onSuccess: (r) => toast.success(r.message), onError: (e) => toast.error(extractErrorMessage(e)) }
                        )
                      }
                    >
                      {name}
                    </DropdownMenuItem>
                  ))}
                </DropdownMenuSubContent>
              </DropdownMenuSub>
              <DropdownMenuItem
                disabled={isCustomApi}
                onSelect={() => {
                  setModelSel(new Set(credential.allowedModels ?? []))
                  setShowModels(true)
                }}
              >
                <Boxes />
                {t('credentialrow.menu.allowedModels')}
              </DropdownMenuItem>
              <DropdownMenuSeparator />
              <DropdownMenuItem
                onSelect={() => {
                  setCloneCopies(1)
                  setShowClone(true)
                }}
              >
                <CopyPlus />
                {t('credentialrow.menu.clone')}
              </DropdownMenuItem>
              <DropdownMenuItem
                onSelect={async () => {
                  const ok = await copyToClipboard(String(credential.id))
                  ok
                    ? toast.success(t('credentialrow.toast.idCopied', { id: credential.id }))
                    : toast.error(t('credentialcard.toast.copyFailed'))
                }}
              >
                <Copy />
                {t('credentialrow.menu.copyId')}
              </DropdownMenuItem>
              <DropdownMenuSeparator />
              {/* 危险区在最下 */}
              <DropdownMenuItem disabled={setDisabled.isPending} onSelect={toggleDisabled}>
                {credential.disabled ? <Power /> : <Ban />}
                {credential.disabled ? t('credentialcard.action.enable') : t('credentialcard.action.disable')}
              </DropdownMenuItem>
              <DropdownMenuItem
                destructive
                disabled={!credential.disabled}
                onSelect={() => setShowDelete(true)}
                title={!credential.disabled ? t('credentialcard.settings.deleteDisabledTitle') : undefined}
              >
                <Trash2 />
                {t('credentialrow.menu.delete')}
              </DropdownMenuItem>
            </>
          )}
        </DropdownMenuContent>
      </DropdownMenu>
      {/* 删除二次确认（与卡片同一道门：需先禁用） */}
      <Dialog open={showDelete} onOpenChange={setShowDelete}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>{t('credentialcard.deleteDialog.title', { id: credential.id })}</DialogTitle>
            <DialogDescription>{t('credentialcard.deleteDialog.description')}</DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setShowDelete(false)} disabled={deleteCredential.isPending}>
              {t('credentialcard.deleteDialog.cancel')}
            </Button>
            <Button variant="destructive" onClick={handleDelete} disabled={deleteCredential.isPending || !credential.disabled}>
              {deleteCredential.isPending && <Loader2 className="h-4 w-4 mr-1 animate-spin" />}
              {t('credentialcard.deleteDialog.confirm')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* 允许的模型…（单号成本白名单硬门；空集 = 不限制） */}
      <Dialog open={showModels} onOpenChange={setShowModels}>
        <DialogContent className="max-w-2xl">
          <DialogHeader>
            <DialogTitle>{t('credentialrow.models.title', { id: credential.id })}</DialogTitle>
            <DialogDescription>
              {modelSel.size === 0
                ? t('credentialrow.models.descUnrestricted')
                : t('credentialrow.models.descHardGate', { count: modelSel.size })}
            </DialogDescription>
          </DialogHeader>
          <div className="flex flex-wrap gap-1.5 py-2">
            {PROBE_MODEL_CATALOG.map((m) => {
              const on = modelSel.has(m.id)
              return (
                <button
                  key={m.id}
                  type="button"
                  aria-pressed={on}
                  onClick={() =>
                    setModelSel((prev) => {
                      const n = new Set(prev)
                      n.has(m.id) ? n.delete(m.id) : n.add(m.id)
                      return n
                    })
                  }
                  className={cn(
                    'inline-flex items-center gap-1 rounded border px-2 py-1 text-[11px] font-medium transition-colors',
                    on
                      ? 'border-primary/40 bg-primary/15 text-primary'
                      : 'border-white/10 bg-white/5 text-muted-foreground hover:border-white/25'
                  )}
                  title={`${m.id} · ${m.mult}`}
                >
                  {m.id} <span className="opacity-60">{m.mult}</span>
                </button>
              )
            })}
          </div>
          <DialogFooter>
            {modelSel.size > 0 && (
              <Button size="sm" variant="ghost" onClick={() => setModelSel(new Set())}>
                {t('dashboard.whitelistDialog.clearSelection')}
              </Button>
            )}
            <Button size="sm" onClick={handleSaveModels} disabled={savingModels}>
              {savingModels && <Loader2 className="h-4 w-4 mr-1 animate-spin" />}
              {t('credentialcard.settings.save')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* 生成分身… */}
      <Dialog open={showClone} onOpenChange={setShowClone}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>{t('credentialrow.clone.title', { id: credential.id })}</DialogTitle>
            <DialogDescription>{t('credentialrow.clone.description')}</DialogDescription>
          </DialogHeader>
          <div className="space-y-1.5 py-1">
            <label className="text-sm font-medium">{t('credentialrow.clone.copiesLabel')}</label>
            <NumberStepper
              value={cloneCopies}
              onChange={setCloneCopies}
              min={1}
              max={16}
              className="w-full"
              aria-label={t('credentialrow.clone.copiesLabel')}
            />
            <p className="text-xs text-muted-foreground">{t('credentialrow.clone.hint')}</p>
          </div>
          <DialogFooter>
            <Button variant="outline" size="sm" onClick={() => setShowClone(false)} disabled={cloneBusy}>
              {t('credentialcard.deleteDialog.cancel')}
            </Button>
            <Button size="sm" onClick={handleClone} disabled={cloneBusy}>
              {cloneBusy && <Loader2 className="h-4 w-4 mr-1 animate-spin" />}
              {t('credentialrow.clone.confirm')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  )
}

/** 展开区的一个 label/value 对。`tone` 只用于失败类数字染色（与卡片同色）。
 *  `onClick`：value 可点击（掩码复制入口，cursor-pointer + title 提示）。
 *  `action`：value 后追加的操作元素（如复制按钮），需自带 self-center 对齐。 */
function DetailItem({
  label,
  value,
  tone,
  mono,
  className,
  onClick,
  action,
}: {
  label: string
  value: string
  tone?: 'bad' | 'warn'
  mono?: boolean
  className?: string
  onClick?: () => void
  action?: React.ReactNode
}) {
  return (
    <div className={cn('flex min-w-0 items-baseline gap-1.5', className)}>
      <dt className="shrink-0 text-muted-foreground">{label}</dt>
      <dd
        className={cn(
          'min-w-0 truncate font-medium',
          mono && 'font-mono',
          onClick && 'cursor-pointer',
          tone === 'bad' && 'text-red-400',
          tone === 'warn' && 'text-amber-400'
        )}
        title={onClick ? '点击复制完整 Key' : value}
        onClick={onClick}
      >
        {value}
      </dd>
      {action}
    </div>
  )
}
