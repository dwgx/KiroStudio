import { useState, useEffect } from 'react'
import { useQueryClient } from '@tanstack/react-query'
import { toast } from 'sonner'
import { Settings, RefreshCw, Wallet, Trash2, Loader2, ClipboardCopy, ShieldAlert, Gauge, Check, Ban, Power } from 'lucide-react'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import { Checkbox } from '@/components/ui/checkbox'
import { Skeleton } from '@/components/ui/skeleton'
import { NumberStepper } from '@/components/ui/number-stepper'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import type { CredentialStatusItem, BalanceResponse } from '@/types/api'
import { cn, copyToClipboard } from '@/lib/utils'
import { enableOverage, disableOverage, setCredentialName, setCredentialProxy } from '@/api/credentials'
import { authShortLabel, disabledReasonLabel, subscriptionLabel } from '@/lib/i18n-labels'
import {
  useSetDisabled,
  useSetPriority,
  useSetRpmLimit,
  useResetFailure,
  useDeleteCredential,
  useForceRefreshToken,
  useCachedBalances,
} from '@/hooks/use-credentials'
import { useCtrlHeld } from '@/hooks/use-ctrl-held'

interface CredentialCardProps {
  credential: CredentialStatusItem
  onViewBalance: (id: number) => void
  selected: boolean
  /** 勾选框切换选中：additive=true 表示加/减选（保留其它选中项） */
  onToggleSelect: (additive?: boolean) => void
  /** 按需（hover/“查询信息”）拉取的余额；若存在则优先于自动缓存快照展示。可为 null。 */
  balance: BalanceResponse | null
  loadingBalance: boolean
}

function formatLastUsed(lastUsedAt: string | null): string {
  if (!lastUsedAt) return '从未使用'
  const date = new Date(lastUsedAt)
  const now = new Date()
  const diff = now.getTime() - date.getTime()
  if (diff < 0) return '刚刚'
  const seconds = Math.floor(diff / 1000)
  if (seconds < 60) return `${seconds} 秒前`
  const minutes = Math.floor(seconds / 60)
  if (minutes < 60) return `${minutes} 分钟前`
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return `${hours} 小时前`
  const days = Math.floor(hours / 24)
  return `${days} 天前`
}

// 缓存新鲜度：把 cachedAt（Unix 秒）转成“截至 X 分钟前”，不抹掉数字，只标注时效。
function formatCachedAt(cachedAt: number): string {
  const diffMs = Date.now() - cachedAt * 1000
  if (diffMs < 0) return '刚刚'
  const minutes = Math.floor(diffMs / 60000)
  if (minutes < 1) return '刚刚'
  if (minutes < 60) return `${minutes} 分钟前`
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return `${hours} 小时前`
  const days = Math.floor(hours / 24)
  return `${days} 天前`
}

// 代理 URL 脱敏：隐藏 user:pass@ 凭据段，仅保留协议 + 主机:端口。
// socks5://user:pass@1.2.3.4:1080 -> socks5://…@1.2.3.4:1080
function maskProxyUrl(url: string): string {
  try {
    const u = new URL(url)
    const host = u.host || u.hostname
    if (u.username || u.password) {
      return `${u.protocol}//…@${host}`
    }
    return `${u.protocol}//${host}`
  } catch {
    // 非标准 URL：正则兜底去掉 //cred@ 段
    return url.replace(/\/\/[^@/]*@/, '//…@')
  }
}

// 金额数字格式化：整数时不带小数（6484），有小数时保留一位（87.5）。
function formatAmount(n: number): string {
  return Number.isInteger(n) ? String(n) : n.toFixed(1)
}

export function CredentialCard({
  credential,
  onViewBalance,
  selected,
  onToggleSelect,
  balance,
  loadingBalance,
}: CredentialCardProps) {
  const [showSettings, setShowSettings] = useState(false)
  const [priorityValue, setPriorityValue] = useState(credential.priority)
  const [rpmLimitValue, setRpmLimitValue] = useState(credential.rpmLimit ?? 0)
  const [showDeleteDialog, setShowDeleteDialog] = useState(false)
  // 超额（Overage）开关：真开关接线状态
  const [overageBusy, setOverageBusy] = useState(false)
  const [showOverageConfirm, setShowOverageConfirm] = useState(false)
  // 别名/备注编辑：设置弹框内输入框的本地值 + 保存中状态
  const [nameValue, setNameValue] = useState(credential.name ?? '')
  const [savingName, setSavingName] = useState(false)

  // 单凭证代理编辑：URL(留空回退全局,"direct"不走代理) + 账密(留空不改)。立即生效无需重启。
  const [proxyValue, setProxyValue] = useState(credential.proxyUrl ?? '')
  const [proxyUser, setProxyUser] = useState('')
  const [proxyPass, setProxyPass] = useState('')
  const [savingProxy, setSavingProxy] = useState(false)

  const queryClient = useQueryClient()
  // 是否按住 Ctrl/Cmd:按住时卡片显示可点击手型 + 左键即多选(松开则普通左键不选中)
  const ctrlHeld = useCtrlHeld()

  const setDisabled = useSetDisabled()
  const setPriority = useSetPriority()
  const setRpmLimit = useSetRpmLimit()
  const resetFailure = useResetFailure()
  const deleteCredential = useDeleteCredential()
  const forceRefresh = useForceRefreshToken()

  // 冷却倒计时：以 query 返回的 cooldownRemainingMs 为基准，本地每秒递减（到 0 后靠下次 query 刷新自然消失）。
  const [cooldownMs, setCooldownMs] = useState(credential.cooldownRemainingMs ?? 0)
  // 每次 query 刷新（coolingDown / cooldownRemainingMs 变化）时，用后端最新值重置本地倒计时基准。
  useEffect(() => {
    setCooldownMs(credential.coolingDown ? credential.cooldownRemainingMs ?? 0 : 0)
  }, [credential.coolingDown, credential.cooldownRemainingMs])
  // 冷却中且剩余 > 0 时启动每秒递减；组件卸载或状态变化时清理 interval。
  useEffect(() => {
    if (!credential.coolingDown || (credential.cooldownRemainingMs ?? 0) <= 0) return
    const timer = setInterval(() => {
      setCooldownMs((prev) => (prev <= 1000 ? 0 : prev - 1000))
    }, 1000)
    return () => clearInterval(timer)
  }, [credential.coolingDown, credential.cooldownRemainingMs])

  // 是否展示冷却徒标：后端标记冷却中且本地倒计时仍 > 0。
  const showCooldown = !!credential.coolingDown && cooldownMs > 0
  // 冷却剩余秒数（向上取整，避免刚进入就显示 0）。
  const cooldownSeconds = Math.ceil(cooldownMs / 1000)
  // 速率限制（429）用琥珀，其它原因（服务错误 / Token 刷新失败等）用红。
  const cooldownIsRateLimit = credential.cooldownReason === '速率限制'

  // 保存别名/备注：空字符串视为清除（传 null）。成功后刷新凭据列表 + toast。
  const handleSaveName = async () => {
    const trimmed = nameValue.trim()
    setSavingName(true)
    try {
      await setCredentialName(credential.id, trimmed === '' ? null : trimmed)
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
      toast.success(trimmed === '' ? '已清除别名' : '已保存别名')
    } catch (err) {
      toast.error('保存别名失败: ' + (err as Error).message)
    } finally {
      setSavingName(false)
    }
  }

  // 保存单凭证代理:URL 空=清除(回退全局);账密仅在填了才发(留空=不改)。立即生效。
  const handleSaveProxy = async () => {
    const url = proxyValue.trim()
    setSavingProxy(true)
    try {
      await setCredentialProxy(
        credential.id,
        url === '' ? null : url,
        proxyUser.trim() || undefined,
        proxyPass || undefined,
      )
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
      setProxyUser('')
      setProxyPass('')
      toast.success(url === '' ? '已清除代理（回退全局）' : '已保存代理，下次请求生效')
    } catch (err) {
      toast.error('保存代理失败: ' + (err as Error).message)
    } finally {
      setSavingProxy(false)
    }
  }

  // 自动加载：读后端【已缓存】余额（零上游、不封号），卡片挂载即显示，无需手动点“查询信息”。
  const { data: cachedBalances, isLoading: cachedLoading } = useCachedBalances()
  const cached = cachedBalances?.balances[String(credential.id)]

  // 展示用余额：按需拉取（balance prop）优先，否则退回后台缓存快照。
  const shownBalance: BalanceResponse | null = balance ?? cached ?? null
  // 是否仍在等待任一来源（按需查询进行中，或缓存首帧加载中且暂无任何数据）。
  const balancePending = loadingBalance || (cachedLoading && !shownBalance)
  // 订阅等级三路优先：按需余额 > 缓存快照 > 凭据列表持久化字段（重启即有）。
  const subscriptionTitle =
    balance?.subscriptionTitle ?? cached?.subscriptionTitle ?? credential.subscriptionTitle ?? null

  const handleToggleDisabled = () => {
    setDisabled.mutate(
      { id: credential.id, disabled: !credential.disabled },
      {
        onSuccess: (res) => toast.success(res.message),
        onError: (err) => toast.error('操作失败: ' + (err as Error).message),
      }
    )
  }

  const handlePriorityChange = () => {
    const newPriority = priorityValue
    if (isNaN(newPriority) || newPriority < 0) {
      toast.error('优先级必须是非负整数')
      return
    }
    setPriority.mutate(
      { id: credential.id, priority: newPriority },
      {
        onSuccess: (res) => toast.success(res.message),
        onError: (err) => toast.error('操作失败: ' + (err as Error).message),
      }
    )
  }

  const handleRpmLimitChange = () => {
    const v = rpmLimitValue
    if (isNaN(v) || v < 0) {
      toast.error('RPM 容量必须是非负整数（0=继承全局）')
      return
    }
    setRpmLimit.mutate(
      { id: credential.id, rpmLimit: v },
      {
        onSuccess: (res) => toast.success(res.message),
        onError: (err) => toast.error('操作失败: ' + (err as Error).message),
      }
    )
  }

  const handleReset = () => {
    resetFailure.mutate(credential.id, {
      onSuccess: (res) => toast.success(res.message),
      onError: (err) => toast.error('操作失败: ' + (err as Error).message),
    })
  }

  const handleForceRefresh = () => {
    forceRefresh.mutate(credential.id, {
      onSuccess: (res) => toast.success(res.message),
      onError: (err) => toast.error('刷新失败: ' + (err as Error).message),
    })
  }

  const handleDelete = () => {
    if (!credential.disabled) {
      toast.error('请先禁用凭据再删除')
      setShowDeleteDialog(false)
      return
    }
    deleteCredential.mutate(credential.id, {
      onSuccess: (res) => {
        toast.success(res.message)
        setShowDeleteDialog(false)
        setShowSettings(false)
      },
      onError: (err) => toast.error('删除失败: ' + (err as Error).message),
    })
  }

  // 超额真实状态：按需余额 > 缓存快照 > 凭据列表持久化字段
  const overageEnabled: boolean | null =
    balance?.overageEnabled ?? cached?.overageEnabled ?? credential.overageEnabled ?? null

  // 操作成功后刷新该卡状态：invalidate 凭据列表 + 缓存余额，两处都会重新拉取
  const refreshOverageState = () => {
    queryClient.invalidateQueries({ queryKey: ['credentials'] })
    queryClient.invalidateQueries({ queryKey: ['cached-balances'] })
    queryClient.invalidateQueries({ queryKey: ['credential-balance', credential.id] })
  }

  // 关闭超额：无需二次确认，直接调用
  const handleDisableOverage = async () => {
    setOverageBusy(true)
    try {
      const res = await disableOverage(credential.id)
      refreshOverageState()
      if (res.confirmed === false) {
        toast.warning(res.note || '已提交关闭请求，但上游尚未确认，请稍后刷新查看')
      } else {
        toast.success('已关闭超额')
      }
    } catch (err) {
      toast.error('关闭超额失败: ' + (err as Error).message)
    } finally {
      setOverageBusy(false)
    }
  }

  // 开启超额：二次确认后调用（明确提示按量付费）
  const handleConfirmEnableOverage = async () => {
    setShowOverageConfirm(false)
    setOverageBusy(true)
    try {
      const res = await enableOverage(credential.id)
      refreshOverageState()
      if (res.confirmed === false) {
        toast.warning(res.note || '已提交开启请求，但上游尚未确认，请稍后刷新查看')
      } else {
        toast.success('已开启超额')
      }
    } catch (err) {
      toast.error('开启超额失败: ' + (err as Error).message)
    } finally {
      setOverageBusy(false)
    }
  }

  // 超额开关切换入口：开启前弹二次确认，关闭直接执行
  const handleOverageToggle = (next: boolean) => {
    if (next) {
      setShowOverageConfirm(true)
    } else {
      handleDisableOverage()
    }
  }

  // 点击整卡切换选中；命中内部交互控件（按钮/输入/开关/复选框/链接/对话框）时不触发
  const INTERACTIVE_SELECTOR =
    'button, input, textarea, select, a, [role="switch"], [role="checkbox"], [role="dialog"], [contenteditable="true"]'

  // 左键点卡片:仅在按住 Ctrl/Cmd 时切换选中(加/减选,保留其它);
  // 普通左键【不选中】(选中只走勾选框)。命中内部交互控件时不触发。
  const handleCardClick = (e: React.MouseEvent<HTMLDivElement>) => {
    if (!(e.ctrlKey || e.metaKey)) return
    if ((e.target as HTMLElement).closest(INTERACTIVE_SELECTOR)) return
    e.preventDefault()
    onToggleSelect(true)
  }

  // 右键卡片：阻止默认菜单，直接打开该卡设置弹框
  const handleCardContextMenu = (e: React.MouseEvent<HTMLDivElement>) => {
    if ((e.target as HTMLElement).closest(INTERACTIVE_SELECTOR)) return
    e.preventDefault()
    setPriorityValue(credential.priority)
    setNameValue(credential.name ?? '')
    setShowSettings(true)
  }


  // 余额状态条：按 剩余/上限 百分比填充，条上叠加数字金额。
  // 剩余越多越健康：>=40% 绿、>=20% 黄、否则红（与“用量条”配色相反，语义是“余量”）。
  const renderBalanceBar = () => {
    if (balancePending) {
      return (
        <div className="space-y-1.5">
          <div className="flex items-center justify-between">
            <span className="text-xs text-muted-foreground">剩余用量</span>
            <span className="text-xs text-muted-foreground">加载中…</span>
          </div>
          <Skeleton className="h-6 w-full rounded-md" />
        </div>
      )
    }

    if (!shownBalance) {
      return (
        <div className="space-y-1.5">
          <div className="flex items-center justify-between">
            <span className="text-xs text-muted-foreground">剩余用量</span>
            <span className="text-xs text-muted-foreground">暂无缓存</span>
          </div>
          <div className="relative h-6 w-full overflow-hidden rounded-md border border-dashed border-border bg-secondary/40">
            <div className="absolute inset-0 flex items-center justify-center text-xs text-muted-foreground">
              暂无数据（后台每 30 分钟温和刷新）
            </div>
          </div>
        </div>
      )
    }

    const limit = shownBalance.usageLimit
    const remaining = shownBalance.remaining
    const remainingPct = limit > 0 ? Math.min(Math.max((remaining / limit) * 100, 0), 100) : 0
    const barColor =
      remainingPct >= 40 ? 'bg-emerald-500' : remainingPct >= 20 ? 'bg-yellow-500' : 'bg-red-500'
    // 剩余百分比数字文字配色：暗色背景下用更亮的 -400 系（500 系偏暗发闷，尤其黄色像橄榄绿）。
    // ≥40 翠绿 / ≥20 琥珀黄 / 否则红，与进度条同口径但更清透亮眼。
    const pctTextColor =
      remainingPct >= 40 ? 'text-emerald-400' : remainingPct >= 20 ? 'text-amber-400' : 'text-red-400'
    // 缓存快照带 cachedAt（按需拉取的 balance prop 没有），据此标注新鲜度。
    const cachedAt = balance ? null : cached?.cachedAt ?? null

    return (
      <div className="space-y-1.5">
        <div className="flex items-center justify-between">
          <span className="text-xs text-muted-foreground">剩余用量</span>
          <span className="text-xs text-muted-foreground">
            {cachedAt ? `截至 ${formatCachedAt(cachedAt)}` : '实时'}
            {' · '}
            <span className={cn('font-semibold tabular-nums', pctTextColor)}>
              {remainingPct.toFixed(1)}% 剩余
            </span>
          </span>
        </div>
        <div className="relative h-6 w-full overflow-hidden rounded-md bg-secondary">
          <div
            className={cn('h-full transition-all duration-500 ease-out-expo', barColor)}
            style={{ width: `${remainingPct}%` }}
          />
          {/* 条上叠加数字金额（居中）。原用 mix-blend-difference 在满条(绿条铺满)时反色发红/发暗、
              被误认为"字被盖住"。改为白字 + 深色描边阴影:无论底下是绿/黄/红条还是空槽都清晰可读。 */}
          <div className="pointer-events-none absolute inset-0 flex items-center justify-center">
            <span
              className="text-xs font-semibold tabular-nums text-white"
              style={{ textShadow: '0 1px 2px rgba(0,0,0,0.85), 0 0 3px rgba(0,0,0,0.7)' }}
            >
              {formatAmount(remaining)} / {formatAmount(limit)}
            </span>
          </div>
        </div>
      </div>
    )
  }

  return (
    <>
      <Card
        aria-selected={selected}
        onClick={handleCardClick}
        onContextMenu={handleCardContextMenu}
        className={cn(
          // 选中不让整卡位移/抖动：只做颜色与边框过渡。
          // 按住 Ctrl/Cmd 时显示可点击手型(此时左键即多选);否则普通指针。
          'transition-[background-color,border-color,box-shadow] duration-200 ease-out-expo hover:border-border-hover hover:shadow-lg hover:shadow-black/20 focus:outline-none',
          ctrlHeld && 'cursor-pointer',
          selected && 'ring-2 ring-primary bg-primary/[0.04]',
          credential.isCurrent && !selected && 'ring-2 ring-emerald-500/60',
          // 冷却时整卡做轻微视觉区分：边框泛色 + 略降透明度（速率限制琥珀、其它红），不喧宾夺主。
          showCooldown && !selected && (cooldownIsRateLimit ? 'border-amber-500/50 opacity-95' : 'border-red-500/50 opacity-95')
        )}
      >
        <CardHeader className="pb-2">
          <div className="flex items-center justify-between gap-2">
            <div className="flex min-w-0 items-center gap-2">
              {/* 复选框始终按多选处理（加/减选，不清空其它） */}
              <Checkbox checked={selected} onCheckedChange={() => onToggleSelect(true)} />
              <CardTitle className="text-lg flex min-w-0 flex-wrap items-center gap-2">
                <span
                  className="min-w-0 max-w-full truncate"
                  title={credential.name ? (credential.email || `凭据 #${credential.id}`) : (credential.email || undefined)}
                >
                  {credential.name || credential.email || `凭据 #${credential.id}`}
                </span>
                {/* 设了别名时，标题旁补一个次级真实身份标注（email 或 #id），便于识别 */}
                {credential.name && (
                  <span className="shrink-0 text-xs font-normal text-muted-foreground">
                    {credential.email || `#${credential.id}`}
                  </span>
                )}
                {credential.isCurrent && <Badge variant="success">当前</Badge>}
                {credential.disabled && <Badge variant="destructive">已禁用</Badge>}
                {credential.disabled && credential.disabledReason && (
                  <Badge variant="outline">{disabledReasonLabel(credential.disabledReason)}</Badge>
                )}
                {credential.authMethod && (
                  <Badge variant="secondary">
                    {credential.authMethod === 'api_key' ? 'API Key' : authShortLabel(credential.authMethod)}
                  </Badge>
                )}
                {credential.endpoint && <Badge variant="outline">{credential.endpoint}</Badge>}
              </CardTitle>
            </div>
            {/* 设置齿轮：集中优先级/启用/删除等操作，让卡片主体更干净 */}
            <Button
              size="sm"
              variant="ghost"
              className="h-8 w-8 shrink-0 p-0"
              onClick={() => {
                setPriorityValue(credential.priority)
                setNameValue(credential.name ?? '')
                setShowSettings(true)
              }}
              title="设置（优先级 / 启用 / 删除）"
              aria-label="设置"
            >
              <Settings className="h-4 w-4" />
            </Button>
          </div>
        </CardHeader>
        <CardContent className="space-y-4">
          {/* 冷却徒标（429/限流/服务错误后短暂跳过调度）：醒目 pill + 本地每秒倒计时。
              速率限制用琥珀，其它原因用红。不冷却时完全不渲染。 */}
          {showCooldown && (
            <div
              className={cn(
                'flex items-center gap-2 rounded-md border px-3 py-2 text-sm font-medium',
                cooldownIsRateLimit
                  ? 'border-amber-500/30 bg-amber-500/10 text-amber-400'
                  : 'border-red-500/30 bg-red-500/10 text-red-400'
              )}
            >
              <Gauge className="h-4 w-4 shrink-0 animate-pulse" />
              <span className="min-w-0 truncate">
                冷却中
                {credential.cooldownReason ? ` · ${credential.cooldownReason}` : ''}
                {' · 剩余'}
                <span className="tabular-nums">{cooldownSeconds}</span>s
              </span>
            </div>
          )}

          {/* 订阅等级 + 余额状态条（自动加载缓存，无需手动点查询） */}
          <div className="space-y-2">
            <div className="flex items-center justify-between gap-2">
              <span className="text-xs text-muted-foreground">订阅等级</span>
              {balancePending && !subscriptionTitle ? (
                <Skeleton className="h-5 w-20 rounded" />
              ) : (
                <Badge variant={subscriptionTitle ? 'secondary' : 'outline'}>
                  {subscriptionLabel(subscriptionTitle)}
                </Badge>
              )}
            </div>
            {renderBalanceBar()}
          </div>

          {/* 信息网格 */}
          <div className="grid grid-cols-2 gap-x-4 gap-y-3 text-sm">
            <div>
              <span className="text-muted-foreground">优先级：</span>
              <span className="font-medium">{credential.priority}</span>
            </div>
            <div>
              <span className="text-muted-foreground">失败次数：</span>
              <span className={credential.failureCount > 0 ? 'text-red-500 font-medium' : ''}>
                {credential.failureCount}
              </span>
            </div>
            <div>
              <span className="text-muted-foreground">刷新失败：</span>
              <span className={credential.refreshFailureCount > 0 ? 'text-red-500 font-medium' : ''}>
                {credential.refreshFailureCount}
              </span>
            </div>
            <div>
              <span className="text-muted-foreground">成功次数：</span>
              <span className="font-medium">{credential.successCount}</span>
              {(credential.inflight ?? 0) > 0 && (
                <span
                  className="ml-2 inline-flex items-center gap-1 text-xs font-medium text-sky-600"
                  title="当前在途请求数（实时负载）"
                >
                  <span className="w-1.5 h-1.5 rounded-full bg-sky-500 animate-pulse" />
                  在途 {credential.inflight}
                </span>
              )}
            </div>
            <div className="col-span-2">
              <span className="text-muted-foreground">最后调用：</span>
              <span className="font-medium">{formatLastUsed(credential.lastUsedAt)}</span>
            </div>
            {credential.maskedApiKey && (
              <div className="col-span-2">
                <span className="text-muted-foreground">API Key：</span>
                <span className="font-mono font-medium">{credential.maskedApiKey}</span>
              </div>
            )}
            {/* 超额（Overage）开关已移入「设置」弹框（齿轮），保持卡片主体信息网格干净。 */}
            {credential.hasProxy && (
              <div className="col-span-2 flex min-w-0 items-center gap-1">
                <span className="shrink-0 text-muted-foreground">代理：</span>
                {credential.proxyUrl ? (
                  <>
                    <span
                      className="min-w-0 flex-1 truncate font-mono text-xs font-medium"
                      title={maskProxyUrl(credential.proxyUrl)}
                    >
                      {maskProxyUrl(credential.proxyUrl)}
                    </span>
                    <Button
                      size="sm"
                      variant="ghost"
                      className="h-6 w-6 shrink-0 p-0"
                      title="复制代理地址（含账号密码）"
                      onClick={async (e) => {
                        e.stopPropagation()
                        const ok = await copyToClipboard(credential.proxyUrl!)
                        ok ? toast.success('已复制代理地址') : toast.error('复制失败，请重试')
                      }}
                    >
                      <ClipboardCopy className="h-3.5 w-3.5" />
                    </Button>
                  </>
                ) : (
                  <Badge variant="secondary">已配置</Badge>
                )}
              </div>
            )}
            {credential.hasProfileArn && (
              <div className="col-span-2">
                <Badge variant="secondary">有 Profile ARN</Badge>
              </div>
            )}
          </div>

          {/* 常用操作（重活收进设置齿轮；这里只留高频只读/查看类） */}
          <div className="flex flex-wrap gap-2 pt-2 border-t">
            <Button
              size="sm"
              variant="outline"
              onClick={handleReset}
              disabled={resetFailure.isPending || (credential.failureCount === 0 && credential.refreshFailureCount === 0)}
            >
              <RefreshCw className="h-4 w-4 mr-1" />
              重置失败
            </Button>
            <Button
              size="sm"
              variant="outline"
              onClick={handleForceRefresh}
              disabled={forceRefresh.isPending || credential.disabled || credential.authMethod === 'api_key'}
              title={credential.authMethod === 'api_key' ? 'API Key 凭据无需刷新 Token' : credential.disabled ? '已禁用的凭据无法刷新 Token' : '强制刷新 Token'}
            >
              <RefreshCw className={`h-4 w-4 mr-1 ${forceRefresh.isPending ? 'animate-spin' : ''}`} />
              刷新 Token
            </Button>
            {/* 查看余额：改用青蓝信息色（与主色/禁用色区分开，语义=只读查询） */}
            <Button
              size="sm"
              variant="outline"
              className="border-sky-500/40 bg-sky-500/10 text-sky-300 hover:bg-sky-500/20 hover:text-sky-200"
              onClick={() => onViewBalance(credential.id)}
            >
              <Wallet className="h-4 w-4 mr-1" />
              查看余额
            </Button>
            {/* 令牌导出已统一移至「设置 · 令牌导出」分区（单个/全部 · JSON/refreshToken/复制）。 */}
            {/* 启用 / 禁用 快捷入口（卡片主体直达，无需再进齿轮设置）。
                禁用=琥珀警示色（非删除的红，只是暂停调度）；启用=翠绿（恢复）。 */}
            <Button
              size="sm"
              variant="outline"
              className={credential.disabled
                ? 'border-emerald-500/40 bg-emerald-500/10 text-emerald-300 hover:bg-emerald-500/20 hover:text-emerald-200'
                : 'border-amber-500/40 bg-amber-500/10 text-amber-300 hover:bg-amber-500/20 hover:text-amber-200'}
              onClick={handleToggleDisabled}
              disabled={setDisabled.isPending}
              title={credential.disabled ? '启用此凭据（重新参与调度）' : '禁用此凭据（暂停调度）'}
            >
              {credential.disabled ? (
                <>
                  <Power className="h-4 w-4 mr-1" />
                  启用
                </>
              ) : (
                <>
                  <Ban className="h-4 w-4 mr-1" />
                  禁用
                </>
              )}
            </Button>
          </div>
        </CardContent>
      </Card>
      {/* 设置对话框：集中优先级 / 启用禁用 / 删除 */}
      <Dialog open={showSettings} onOpenChange={setShowSettings}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>
              凭据设置 · #{credential.id}
              {credential.email ? ` · ${credential.email}` : ''}
            </DialogTitle>
            <DialogDescription>调整优先级、启用状态，或删除此凭据。</DialogDescription>
          </DialogHeader>

          <div className="space-y-5 py-1">
            {/* 别名/备注：自定义卡片标题，留空清除后回落 email/#id */}
            <div className="space-y-2">
              <div className="min-w-0">
                <div className="text-sm font-medium">别名 / 备注</div>
                <div className="text-xs text-muted-foreground">
                  卡片标题优先显示别名；留空并保存即清除，回落到邮箱或 #{credential.id}
                </div>
              </div>
              <div className="flex items-center gap-2">
                <Input
                  value={nameValue}
                  onChange={(e) => setNameValue(e.target.value)}
                  placeholder="例如：主力号 / 备用 / 客户A"
                  maxLength={64}
                  aria-label="别名或备注"
                  onKeyDown={(e) => {
                    if (e.key === 'Enter' && !savingName) handleSaveName()
                  }}
                />
                <Button
                  size="sm"
                  className="shrink-0"
                  onClick={handleSaveName}
                  disabled={savingName || nameValue.trim() === (credential.name ?? '')}
                >
                  {savingName ? (
                    <Loader2 className="h-4 w-4 animate-spin" />
                  ) : (
                    <Check className="h-4 w-4" />
                  )}
                  <span className="ml-1">保存</span>
                </Button>
              </div>
            </div>

            {/* 单凭证代理：URL 留空=回退全局代理，"direct"=强制不走代理；账密留空=不改。立即生效无需重启。 */}
            <div className="space-y-2">
              <div className="min-w-0">
                <div className="text-sm font-medium">代理</div>
                <div className="text-xs text-muted-foreground">
                  http(s)/socks5://host:port，可账密内嵌 URL（socks5://用户:密码@主机:端口）自动识别拆分。
                  留空=用全局代理，“direct”=此号不走代理。保存后下次请求生效。
                </div>
              </div>
              <Input
                value={proxyValue}
                onChange={(e) => setProxyValue(e.target.value)}
                placeholder='例如 socks5://127.0.0.1:1080 或 direct'
                className="font-mono text-xs"
                aria-label="代理 URL"
              />
              <div className="flex items-center gap-2">
                <Input
                  value={proxyUser}
                  onChange={(e) => setProxyUser(e.target.value)}
                  placeholder="代理用户名（留空不改）"
                  className="text-xs"
                  autoComplete="off"
                  aria-label="代理用户名"
                />
                <Input
                  type="password"
                  value={proxyPass}
                  onChange={(e) => setProxyPass(e.target.value)}
                  placeholder="代理密码（留空不改）"
                  className="text-xs"
                  autoComplete="new-password"
                  aria-label="代理密码"
                />
                <Button
                  size="sm"
                  className="shrink-0"
                  onClick={handleSaveProxy}
                  disabled={savingProxy}
                >
                  {savingProxy ? <Loader2 className="h-4 w-4 animate-spin" /> : <Check className="h-4 w-4" />}
                  <span className="ml-1">保存</span>
                </Button>
              </div>
            </div>

            {/* 超额（Overage）：接后端真开关，开启前二次确认（按量付费）。已从卡片主体移入此设置弹框。 */}
            <div className="flex items-center justify-between gap-4 border-t pt-4">
              <div className="flex min-w-0 items-center gap-2">
                <Gauge className="h-4 w-4 shrink-0 text-muted-foreground" />
                <div className="min-w-0">
                  <div className="text-sm font-medium">超额（Overage）</div>
                  <div className="text-xs text-muted-foreground">
                    {overageEnabled
                      ? '已开启 · 用尽 base 额度后按真实用量付费'
                      : '关闭时不突破 base 额度；开启后超量按量付费'}
                  </div>
                </div>
              </div>
              <div className="flex shrink-0 items-center gap-2">
                {overageEnabled != null && (
                  <Badge variant={overageEnabled ? 'success' : 'secondary'}>
                    {overageEnabled ? '开' : '关'}
                  </Badge>
                )}
                <Switch
                  checked={!!overageEnabled}
                  disabled={overageBusy}
                  onCheckedChange={handleOverageToggle}
                  aria-label="超额开关"
                />
              </div>
            </div>

            {/* 优先级：输入框（数字步进器）替代旧“点击编辑” */}
            <div className="flex items-center justify-between gap-4 border-t pt-4">
              <div className="min-w-0">
                <div className="text-sm font-medium">优先级</div>
                <div className="text-xs text-muted-foreground">数值越小越优先被调度</div>
              </div>
              <div className="flex shrink-0 items-center gap-2">
                <NumberStepper
                  value={priorityValue}
                  onChange={setPriorityValue}
                  min={0}
                  className="w-24"
                  aria-label="优先级"
                />
                <Button
                  size="sm"
                  onClick={handlePriorityChange}
                  disabled={setPriority.isPending || priorityValue === credential.priority}
                >
                  {setPriority.isPending ? (
                    <Loader2 className="h-4 w-4 animate-spin" />
                  ) : (
                    <Check className="h-4 w-4" />
                  )}
                  <span className="ml-1">保存</span>
                </Button>
              </div>
            </div>

            {/* RPM 容量上限：本号每分钟请求容量，0=继承全局。体质好的号可设高（如 100）。 */}
            <div className="flex items-center justify-between gap-4 border-t pt-4">
              <div className="min-w-0">
                <div className="text-sm font-medium">RPM 容量上限</div>
                <div className="text-xs text-muted-foreground">
                  每分钟请求容量，0=继承全局。体质好的号可设高（如 100），达上限才溢出到低优先级备份号。
                </div>
              </div>
              <div className="flex shrink-0 items-center gap-2">
                <NumberStepper
                  value={rpmLimitValue}
                  onChange={setRpmLimitValue}
                  min={0}
                  step={10}
                  className="w-24"
                  aria-label="RPM 容量上限"
                />
                <Button
                  size="sm"
                  onClick={handleRpmLimitChange}
                  disabled={setRpmLimit.isPending || rpmLimitValue === (credential.rpmLimit ?? 0)}
                >
                  {setRpmLimit.isPending ? (
                    <Loader2 className="h-4 w-4 animate-spin" />
                  ) : (
                    <Check className="h-4 w-4" />
                  )}
                  <span className="ml-1">保存</span>
                </Button>
              </div>
            </div>

            {/* 启用 / 禁用 */}
            <div className="flex items-center justify-between gap-4 border-t pt-4">
              <div className="min-w-0">
                <div className="text-sm font-medium">启用凭据</div>
                <div className="text-xs text-muted-foreground">
                  {credential.disabled ? '当前已禁用，不参与调度' : '当前启用中'}
                </div>
              </div>
              <Switch
                checked={!credential.disabled}
                onCheckedChange={handleToggleDisabled}
                disabled={setDisabled.isPending}
                aria-label="启用凭据"
              />
            </div>

            {/* 删除（危险） */}
            <div className="flex items-center justify-between gap-4 border-t pt-4">
              <div className="min-w-0">
                <div className="text-sm font-medium text-destructive">删除凭据</div>
                <div className="text-xs text-muted-foreground">
                  移入回收站，不可恢复地删除。需先禁用。
                </div>
              </div>
              <Button
                size="sm"
                variant="destructive"
                onClick={() => setShowDeleteDialog(true)}
                disabled={!credential.disabled}
                title={!credential.disabled ? '需要先禁用凭据才能删除' : undefined}
              >
                <Trash2 className="h-4 w-4 mr-1" />
                删除
              </Button>
            </div>
            {!credential.disabled && (
              <p className="text-xs text-amber-500">提示：删除前请先在上方关闭“启用凭据”。</p>
            )}
          </div>

          <DialogFooter>
            <Button variant="outline" onClick={() => setShowSettings(false)}>
              关闭
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* 删除二次确认对话框 */}
      <Dialog open={showDeleteDialog} onOpenChange={setShowDeleteDialog}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>确认删除凭据 #{credential.id}？</DialogTitle>
            <DialogDescription>
              将不可恢复地移入回收站。此操作无法撤销，删除前需先禁用凭据。
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button
              variant="outline"
              onClick={() => setShowDeleteDialog(false)}
              disabled={deleteCredential.isPending}
            >
              取消
            </Button>
            <Button
              variant="destructive"
              onClick={handleDelete}
              disabled={deleteCredential.isPending || !credential.disabled}
            >
              {deleteCredential.isPending && <Loader2 className="h-4 w-4 mr-1 animate-spin" />}
              确认删除
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* 开启超额二次确认对话框 */}
      <Dialog open={showOverageConfirm} onOpenChange={setShowOverageConfirm}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>确认开启超额（Overage）？</DialogTitle>
            <DialogDescription>
              开启后，此凭据在用尽 base 额度后仍可继续使用，超出部分将按真实用量付费。请确认已了解计费影响再继续。
            </DialogDescription>
          </DialogHeader>
          <div className="flex items-start gap-2 rounded-md border border-amber-500/20 bg-amber-500/10 px-3 py-2 text-xs text-amber-400">
            <ShieldAlert className="mt-0.5 h-3.5 w-3.5 shrink-0" />
            <span>超额 = 超出 base 额度后按真实用量付费，可能产生额外费用。</span>
          </div>
          <DialogFooter>
            <Button
              variant="outline"
              onClick={() => setShowOverageConfirm(false)}
              disabled={overageBusy}
            >
              取消
            </Button>
            <Button onClick={handleConfirmEnableOverage} disabled={overageBusy}>
              {overageBusy && <Loader2 className="h-4 w-4 mr-1 animate-spin" />}
              确认开启
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

    </>
  )
}

