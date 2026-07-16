import { useState, useEffect } from 'react'
import { useTranslation } from 'react-i18next'
import i18n from '@/i18n'
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
import { RegionSwitcher } from '@/components/region-switcher'
import { ProxyTestButton } from '@/components/proxy-test-button'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import type { CredentialStatusItem, BalanceResponse, OnboardingDiagnosis } from '@/types/api'
import { cn, copyToClipboard, extractErrorMessage, extractDiagnosis } from '@/lib/utils'
import { DiagnosisCard } from '@/components/diagnosis-card'
import { enableOverage, disableOverage, setCredentialName, setCredentialProxy } from '@/api/credentials'
import { authShortLabel, disabledReasonLabel, subscriptionLabel } from '@/lib/i18n-labels'
import {
  useSetDisabled,
  useSetPriority,
  useSetRpmLimit,
  useSetCustomApiConfig,
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

/** 累计花费展示：0 显示 0，小数保留两位，过千用 k 简写，避免长号占满卡片。 */
function formatCredits(v: number | undefined | null): string {
  const n = typeof v === 'number' && isFinite(v) ? v : 0
  if (n === 0) return '0'
  if (n >= 10000) return `${(n / 1000).toFixed(1)}k`
  return n.toFixed(2)
}

// 每次渲染调用：i18n 单例取当前语言。
function formatLastUsed(lastUsedAt: string | null): string {
  if (!lastUsedAt) return i18n.t('credentialcard.lastUsed.never')
  const date = new Date(lastUsedAt)
  const now = new Date()
  const diff = now.getTime() - date.getTime()
  if (diff < 0) return i18n.t('credentialcard.lastUsed.justNow')
  const seconds = Math.floor(diff / 1000)
  if (seconds < 60) return i18n.t('credentialcard.lastUsed.secondsAgo', { n: seconds })
  const minutes = Math.floor(seconds / 60)
  if (minutes < 60) return i18n.t('credentialcard.lastUsed.minutesAgo', { n: minutes })
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return i18n.t('credentialcard.lastUsed.hoursAgo', { n: hours })
  const days = Math.floor(hours / 24)
  return i18n.t('credentialcard.lastUsed.daysAgo', { n: days })
}

// 缓存新鲜度：把 cachedAt（Unix 秒）转成“截至 X 分钟前”，不抹掉数字，只标注时效。
function formatCachedAt(cachedAt: number): string {
  const diffMs = Date.now() - cachedAt * 1000
  if (diffMs < 0) return i18n.t('credentialcard.lastUsed.justNow')
  const minutes = Math.floor(diffMs / 60000)
  if (minutes < 1) return i18n.t('credentialcard.lastUsed.justNow')
  if (minutes < 60) return i18n.t('credentialcard.lastUsed.minutesAgo', { n: minutes })
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return i18n.t('credentialcard.lastUsed.hoursAgo', { n: hours })
  const days = Math.floor(hours / 24)
  return i18n.t('credentialcard.lastUsed.daysAgo', { n: days })
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
  const { t } = useTranslation()
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

  // 自定义 API 代挂配置编辑(仅 custom_api 号):上游地址 / 上游密钥(留空不改) / 请求上限 / 换key清零计数。
  const [customBaseUrl, setCustomBaseUrl] = useState(credential.baseUrl ?? '')
  const [customApiKeyInput, setCustomApiKeyInput] = useState('')
  const [customRequestLimit, setCustomRequestLimit] = useState(credential.requestLimit ?? 0)
  const [customResetCount, setCustomResetCount] = useState(false)
  const [savingCustomApi, setSavingCustomApi] = useState(false)

  // 刷新 Token 失败诊断（结构化，如 client 过期引导重新上号）。
  const [refreshDiagnosis, setRefreshDiagnosis] = useState<OnboardingDiagnosis | null>(null)

  const queryClient = useQueryClient()
  // 是否按住 Ctrl/Cmd:按住时卡片显示可点击手型 + 左键即多选(松开则普通左键不选中)
  const ctrlHeld = useCtrlHeld()

  const setDisabled = useSetDisabled()
  const setPriority = useSetPriority()
  const setRpmLimit = useSetRpmLimit()
  const setCustomApiConfig = useSetCustomApiConfig()
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
      toast.success(trimmed === '' ? t('credentialcard.toast.nameCleared') : t('credentialcard.toast.nameSaved'))
    } catch (err) {
      toast.error(t('credentialcard.toast.nameSaveFailed') + (err as Error).message)
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
      toast.success(url === '' ? t('credentialcard.toast.proxyCleared') : t('credentialcard.toast.proxySaved'))
    } catch (err) {
      toast.error(t('credentialcard.toast.proxySaveFailed') + (err as Error).message)
    } finally {
      setSavingProxy(false)
    }
  }

  // 保存自定义 API 代挂配置(base_url / api_key 留空不改 / 请求上限 / 可选清零计数)。
  const handleSaveCustomApi = async () => {
    const url = customBaseUrl.trim()
    if (!url) {
      toast.error(t('credentialcard.toast.baseUrlRequired'))
      return
    }
    setSavingCustomApi(true)
    try {
      await setCustomApiConfig.mutateAsync({
        id: credential.id,
        input: {
          baseUrl: url,
          // 留空=不改;非空=更新(明文不回显,只在用户输入新值时提交)。
          apiKey: customApiKeyInput.trim() ? customApiKeyInput.trim() : undefined,
          requestLimit: customRequestLimit,
          resetCount: customResetCount,
        },
      })
      setCustomApiKeyInput('')
      setCustomResetCount(false)
      toast.success(t('credentialcard.toast.customApiSaved'))
    } catch (err) {
      toast.error(t('credentialcard.toast.saveFailed') + (err as Error).message)
    } finally {
      setSavingCustomApi(false)
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

  // 自定义 API 代挂号:不是 Kiro 号,订阅/余额/profileArn/刷新Token 全无意义,卡片显示专属信息。
  // 判据与后端 is_custom_api_credential + StatusBars 对齐(authMethod 优先,baseUrl 兜底旧数据)。
  const isCustomApi = credential.authMethod === 'custom_api' || !!credential.baseUrl
  // 「Profile ARN 区域」探测/切换:External IdP(微软 M365 等,同账号多 region 各有独立 profile 只部分
  // 开通)+ IdC(AWS SSO)。后端 probe_regions_for/switch 已放开到 external_idp||idc(排除 social/api_key
  // /custom_api)。**IdC 实例通常绑单一 region,探测多用于确认/重新解析该号 profileArn,一般只返回一个
  // region**(非多 region 选择器)。故对这两类显示区块。
  const isExternalIdp = credential.authMethod === 'external_idp'
  const isIdc = credential.authMethod === 'idc'
  const canProbeRegion = isExternalIdp || isIdc

  const handleToggleDisabled = () => {
    setDisabled.mutate(
      { id: credential.id, disabled: !credential.disabled },
      {
        onSuccess: (res) => toast.success(res.message),
        onError: (err) => toast.error(t('credentialcard.toast.operationFailed') + (err as Error).message),
      }
    )
  }

  const handlePriorityChange = () => {
    const newPriority = priorityValue
    if (isNaN(newPriority) || newPriority < 0) {
      toast.error(t('credentialcard.toast.priorityInvalid'))
      return
    }
    setPriority.mutate(
      { id: credential.id, priority: newPriority },
      {
        onSuccess: (res) => toast.success(res.message),
        onError: (err) => toast.error(t('credentialcard.toast.operationFailed') + (err as Error).message),
      }
    )
  }

  const handleRpmLimitChange = () => {
    const v = rpmLimitValue
    if (isNaN(v) || v < 0) {
      toast.error(t('credentialcard.toast.rpmInvalid'))
      return
    }
    setRpmLimit.mutate(
      { id: credential.id, rpmLimit: v },
      {
        onSuccess: (res) => toast.success(res.message),
        onError: (err) => toast.error(t('credentialcard.toast.operationFailed') + (err as Error).message),
      }
    )
  }

  const handleReset = () => {
    resetFailure.mutate(credential.id, {
      onSuccess: (res) => toast.success(res.message),
      onError: (err) => toast.error(t('credentialcard.toast.operationFailed') + (err as Error).message),
    })
  }

  const handleForceRefresh = () => {
    setRefreshDiagnosis(null)
    forceRefresh.mutate(credential.id, {
      onSuccess: (res) => {
        setRefreshDiagnosis(null)
        toast.success(res.message)
      },
      onError: (err) => {
        // 结构化诊断优先(如 #98 的 CLIENT_OR_TOKEN_MISMATCH:引导重新上号而非裸 502),否则 toast。
        const diag = extractDiagnosis(err)
        if (diag) {
          setRefreshDiagnosis(diag)
          toast.error(t('credentialcard.toast.refreshFailedDiag') + diag.summary)
        } else {
          toast.error(t('credentialcard.toast.refreshFailed') + extractErrorMessage(err))
        }
      },
    })
  }


  const handleDelete = () => {
    if (!credential.disabled) {
      toast.error(t('credentialcard.toast.disableBeforeDelete'))
      setShowDeleteDialog(false)
      return
    }
    deleteCredential.mutate(credential.id, {
      onSuccess: (res) => {
        toast.success(res.message)
        setShowDeleteDialog(false)
        setShowSettings(false)
      },
      onError: (err) => toast.error(t('credentialcard.toast.deleteFailed') + (err as Error).message),
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
        toast.warning(res.note || t('credentialcard.toast.disableOverageUnconfirmed'))
      } else {
        toast.success(t('credentialcard.toast.overageDisabled'))
      }
    } catch (err) {
      toast.error(t('credentialcard.toast.disableOverageFailed') + (err as Error).message)
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
        toast.warning(res.note || t('credentialcard.toast.enableOverageUnconfirmed'))
      } else {
        toast.success(t('credentialcard.toast.overageEnabled'))
      }
    } catch (err) {
      toast.error(t('credentialcard.toast.enableOverageFailed') + (err as Error).message)
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
            <span className="text-xs text-muted-foreground">{t('credentialcard.balanceBar.remainingUsage')}</span>
            <span className="text-xs text-muted-foreground">{t('credentialcard.balanceBar.loading')}</span>
          </div>
          <Skeleton className="h-6 w-full rounded-md" />
        </div>
      )
    }

    if (!shownBalance) {
      return (
        <div className="space-y-1.5">
          <div className="flex items-center justify-between">
            <span className="text-xs text-muted-foreground">{t('credentialcard.balanceBar.remainingUsage')}</span>
            <span className="text-xs text-muted-foreground">{t('credentialcard.balanceBar.noCache')}</span>
          </div>
          <div className="relative h-6 w-full overflow-hidden rounded-md border border-dashed border-border bg-secondary/40">
            <div className="absolute inset-0 flex items-center justify-center text-xs text-muted-foreground">
              {t('credentialcard.balanceBar.noData')}
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
          <span className="text-xs text-muted-foreground">{t('credentialcard.balanceBar.remainingUsage')}</span>
          <span className="text-xs text-muted-foreground">
            {cachedAt ? t('credentialcard.balanceBar.asOf', { time: formatCachedAt(cachedAt) }) : t('credentialcard.balanceBar.realtime')}
            {' · '}
            <span className={cn('font-semibold tabular-nums', pctTextColor)}>
              {t('credentialcard.balanceBar.remainingPct', { n: remainingPct.toFixed(1) })}
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
                  title={credential.name ? (credential.email || t('credentialcard.title.fallback', { id: credential.id })) : (credential.email || undefined)}
                >
                  {credential.name || credential.email || t('credentialcard.title.fallback', { id: credential.id })}
                </span>
                {/* 设了别名时，标题旁补一个次级真实身份标注（email 或 #id），便于识别 */}
                {credential.name && (
                  <span className="shrink-0 text-xs font-normal text-muted-foreground">
                    {credential.email || `#${credential.id}`}
                  </span>
                )}
                {credential.isCurrent && <Badge variant="success">{t('credentialcard.badge.current')}</Badge>}
                {credential.disabled && <Badge variant="destructive">{t('credentialcard.badge.disabled')}</Badge>}
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
                setRpmLimitValue(credential.rpmLimit ?? 0)
                setNameValue(credential.name ?? '')
                setShowSettings(true)
              }}
              title={t('credentialcard.gearButton.title')}
              aria-label={t('credentialcard.gearButton.ariaLabel')}
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
                {t('credentialcard.cooldown.label')}
                {credential.cooldownReason ? ` · ${credential.cooldownReason}` : ''}
                {' · '}{t('credentialcard.cooldown.remaining')}
                <span className="tabular-nums">{cooldownSeconds}</span>s
              </span>
            </div>
          )}

          {/* 自定义 API 代挂:一体紧凑块(上游地址/请求用量/优先级/成功·失败/最后调用/密钥/代理),
              不显示 Kiro 的订阅/余额网格——避免信息被劈成上下两坨 + 奇数格留白(卡片瘦身)。 */}
          {isCustomApi ? (
            <div className="space-y-1.5 text-sm">
              {/* 上游地址:主视觉,吃满宽度不硬截断 */}
              <div className="flex items-center gap-2">
                <span className="shrink-0 text-xs text-muted-foreground">{t('credentialcard.customApi.baseUrl')}</span>
                <span className="min-w-0 flex-1 truncate text-right font-mono text-xs text-foreground" title={credential.baseUrl}>
                  {credential.baseUrl || '—'}
                </span>
              </div>
              {/* 请求用量:达上限变琥珀 + 小徽章"已满"(替代长中文) */}
              <div className="flex items-center justify-between gap-2">
                <span className="text-xs text-muted-foreground">{t('credentialcard.customApi.requestUsage')}</span>
                <span className="text-xs">
                  {credential.requestLimit && credential.requestLimit > 0 ? (
                    <span className={
                      (credential.requestCount ?? 0) >= credential.requestLimit
                        ? 'font-medium text-amber-400'
                        : 'text-foreground'
                    }>
                      {credential.requestCount ?? 0} / {credential.requestLimit}
                      {(credential.requestCount ?? 0) >= credential.requestLimit && (
                        <span className="ml-1 rounded bg-amber-500/15 px-1 py-0.5 text-[10px] text-amber-300">{t('credentialcard.customApi.full')}</span>
                      )}
                    </span>
                  ) : (
                    <span className="text-foreground">{credential.requestCount ?? 0} <span className="text-muted-foreground">{t('credentialcard.customApi.unlimited')}</span></span>
                  )}
                </span>
              </div>
              {/* 优先级 + 成功·失败 + 最后调用:一行内紧凑排布,弱化次要信息 */}
              <div className="flex items-center justify-between gap-2 text-xs">
                <span className="text-muted-foreground">{t('credentialcard.customApi.priority')} <span className="font-medium text-foreground">{credential.priority}</span></span>
                <span className="text-muted-foreground">
                  {t('credentialcard.customApi.success')} <span className="font-medium text-emerald-400/90">{credential.successCount}</span>
                  {credential.failureCount > 0 && (
                    <> · {t('credentialcard.customApi.failure')} <span className="font-medium text-red-400/80">{credential.failureCount}</span></>
                  )}
                </span>
              </div>
              <div className="flex items-center justify-between gap-2 text-xs text-muted-foreground">
                <span>{t('credentialcard.customApi.lastCall')}</span>
                <span>{formatLastUsed(credential.lastUsedAt)}</span>
              </div>
              {/* 上游密钥掩码(有则显) */}
              {credential.maskedApiKey && (
                <div className="flex items-center justify-between gap-2 text-xs">
                  <span className="text-muted-foreground">{t('credentialcard.customApi.upstreamKey')}</span>
                  <span className="font-mono text-foreground">{credential.maskedApiKey}</span>
                </div>
              )}
              {/* 代理(有则显,复用掩码) */}
              {credential.hasProxy && credential.proxyUrl && (
                <div className="flex min-w-0 items-center gap-2 text-xs">
                  <span className="shrink-0 text-muted-foreground">{t('credentialcard.customApi.proxy')}</span>
                  <span className="min-w-0 flex-1 truncate text-right font-mono text-foreground" title={maskProxyUrl(credential.proxyUrl)}>
                    {maskProxyUrl(credential.proxyUrl)}
                  </span>
                </div>
              )}
            </div>
          ) : (
          /* 订阅等级 + 余额状态条（自动加载缓存，无需手动点查询） */
          <div className="space-y-2">
            <div className="flex items-center justify-between gap-2">
              <span className="text-xs text-muted-foreground">{t('credentialcard.info.subscriptionLevel')}</span>
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
          )}

          {/* 信息网格(Kiro 号专用;custom_api 已由上方一体紧凑块覆盖所有有意义字段,不再重复渲染) */}
          {!isCustomApi && (
          <div className="grid grid-cols-2 gap-x-4 gap-y-3 text-sm">
            <div>
              <span className="text-muted-foreground">{t('credentialcard.info.priority')}</span>
              <span className="font-medium">{credential.priority}</span>
            </div>
            <div>
              <span className="text-muted-foreground">{t('credentialcard.info.failureCount')}</span>
              <span className={credential.failureCount > 0 ? 'text-red-500 font-medium' : ''}>
                {credential.failureCount}
              </span>
            </div>
            {/* 刷新失败是 Token 刷新概念,自定义 API 代挂号无 token 刷新,不显示 */}
            {!isCustomApi && (
            <div>
              <span className="text-muted-foreground">{t('credentialcard.info.refreshFailure')}</span>
              <span className={credential.refreshFailureCount > 0 ? 'text-red-500 font-medium' : ''}>
                {credential.refreshFailureCount}
              </span>
            </div>
            )}
            <div>
              <span className="text-muted-foreground">{t('credentialcard.info.successCount')}</span>
              <span className="font-medium">{credential.successCount}</span>
              {(credential.inflight ?? 0) > 0 && (
                <span
                  className="ml-2 inline-flex items-center gap-1 text-xs font-medium text-sky-600"
                  title={t('credentialcard.info.inflightTitle')}
                >
                  <span className="w-1.5 h-1.5 rounded-full bg-sky-500 animate-pulse" />
                  {t('credentialcard.info.inflight', { n: credential.inflight })}
                </span>
              )}
            </div>
            {/* 累计花费=上游 credit 计量,仅 Kiro 号有。自定义 API 透传不解析上游拿不到 credit,
                改由上方"请求用量"块展示调用次数,此行对 custom_api 不渲染(避免"0 credits"误导)。 */}
            {!isCustomApi && (
            <div className="col-span-2">
              <span className="text-muted-foreground">{t('credentialcard.info.totalCredits')}</span>
              <span
                className="font-medium"
                title={t('credentialcard.info.totalCreditsTitle')}
              >
                {formatCredits(credential.totalCreditsUsed)} credits
              </span>
            </div>
            )}
            <div className="col-span-2">
              <span className="text-muted-foreground">{t('credentialcard.info.lastCall')}</span>
              <span className="font-medium">{formatLastUsed(credential.lastUsedAt)}</span>
            </div>
            {credential.allowedModels && credential.allowedModels.length > 0 && (
              <div className="col-span-2">
                <span className="text-muted-foreground">{t('credentialcard.info.allowedModels')}</span>
                <span
                  className="font-medium text-primary"
                  title={t('credentialcard.info.allowedModelsTitle') + '\n' + credential.allowedModels.join('\n')}
                >
                  {t('credentialcard.info.allowedModelsCount', { n: credential.allowedModels.length })}
                </span>
              </div>
            )}
            {credential.maskedApiKey && (
              <div className="col-span-2">
                <span className="text-muted-foreground">{t('credentialcard.info.apiKey')}</span>
                <span className="font-mono font-medium">{credential.maskedApiKey}</span>
              </div>
            )}
            {/* 超额（Overage）开关已移入「设置」弹框（齿轮），保持卡片主体信息网格干净。 */}
            {credential.hasProxy && (
              <div className="col-span-2 flex min-w-0 items-center gap-1">
                <span className="shrink-0 text-muted-foreground">{t('credentialcard.info.proxy')}</span>
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
                      title={t('credentialcard.info.copyProxyTitle')}
                      onClick={async (e) => {
                        e.stopPropagation()
                        const ok = await copyToClipboard(credential.proxyUrl!)
                        ok ? toast.success(t('credentialcard.toast.proxyCopied')) : toast.error(t('credentialcard.toast.copyFailed'))
                      }}
                    >
                      <ClipboardCopy className="h-3.5 w-3.5" />
                    </Button>
                  </>
                ) : (
                  <Badge variant="secondary">{t('credentialcard.info.proxyConfigured')}</Badge>
                )}
              </div>
            )}
            {credential.hasProfileArn && (
              <div className="col-span-2">
                <Badge variant="secondary">{t('credentialcard.info.hasProfileArn')}</Badge>
              </div>
            )}
          </div>
          )}

          {/* 常用操作（重活收进设置齿轮；这里只留高频只读/查看类）。
              「测活」「允许模型」已移到勾选后工具栏的批量操作(批量验活 / 允许模型),
              勾一个号即可对单号操作,卡片正面不再重复这两个按钮,保持清爽。 */}
          <div className="flex flex-wrap gap-2 pt-2 border-t">
            {/* 刷新 Token / 查看余额 是 Kiro 专属,自定义 API 代挂号不显示(它无 token/余额概念) */}
            {!isCustomApi && (
            <Button
              size="sm"
              variant="outline"
              onClick={handleForceRefresh}
              disabled={forceRefresh.isPending || credential.authMethod === 'api_key'}
              title={credential.authMethod === 'api_key' ? t('credentialcard.action.refreshTokenApiKeyTitle') : t('credentialcard.action.refreshTokenTitle')}
            >
              <RefreshCw className={`h-4 w-4 mr-1 ${forceRefresh.isPending ? 'animate-spin' : ''}`} />
              {t('credentialcard.action.refreshToken')}
            </Button>
            )}
            {!isCustomApi && (
            /* 查看余额：改用青蓝信息色（与主色/禁用色区分开，语义=只读查询） */
            <Button
              size="sm"
              variant="outline"
              className="border-sky-500/40 bg-sky-500/10 text-sky-300 hover:bg-sky-500/20 hover:text-sky-200"
              onClick={() => onViewBalance(credential.id)}
            >
              <Wallet className="h-4 w-4 mr-1" />
              {t('credentialcard.action.viewBalance')}
            </Button>
            )}
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
              title={credential.disabled ? t('credentialcard.action.enableTitle') : t('credentialcard.action.disableTitle')}
            >
              {credential.disabled ? (
                <>
                  <Power className="h-4 w-4 mr-1" />
                  {t('credentialcard.action.enable')}
                </>
              ) : (
                <>
                  <Ban className="h-4 w-4 mr-1" />
                  {t('credentialcard.action.disable')}
                </>
              )}
            </Button>
          </div>
        </CardContent>
      </Card>
      {/* 设置对话框：集中别名/代理/超额/优先级/RPM/启用/删除。
          紧凑化：调度参数与开关双列并排、次要项(删除)收进底部危险区、
          弹框限高 max-h 内部滚动而非整页滚。 */}
      <Dialog open={showSettings} onOpenChange={setShowSettings}>
        {/* flex 纵向 + p-0：头/尾固定，中段 body 独立滚动；限高 85vh 防超屏。 */}
        <DialogContent className="flex max-h-[85vh] flex-col gap-0 p-0">
          <DialogHeader className="shrink-0 border-b px-5 py-4">
            <DialogTitle className="truncate">
              {t('credentialcard.settings.title', { id: credential.id })}
              {credential.email ? ` · ${credential.email}` : ''}
            </DialogTitle>
            <DialogDescription>{t('credentialcard.settings.description')}</DialogDescription>
          </DialogHeader>

          {/* 可滚动内容区：内容超高时仅此区域滚动 */}
          <div className="min-h-0 flex-1 space-y-4 overflow-y-auto px-5 py-4">
            {/* 别名/备注：自定义卡片标题，留空清除后回落 email/#id */}
            <div className="space-y-1.5">
              <label className="text-sm font-medium">{t('credentialcard.settings.aliasLabel')}</label>
              <div className="flex items-center gap-2">
                <Input
                  value={nameValue}
                  onChange={(e) => setNameValue(e.target.value)}
                  placeholder={t('credentialcard.settings.aliasPlaceholder')}
                  maxLength={64}
                  className="h-9"
                  aria-label={t('credentialcard.settings.aliasAria')}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter' && !savingName) handleSaveName()
                  }}
                />
                <Button
                  size="sm"
                  className="h-9 shrink-0"
                  onClick={handleSaveName}
                  disabled={savingName || nameValue.trim() === (credential.name ?? '')}
                >
                  {savingName ? (
                    <Loader2 className="h-4 w-4 animate-spin" />
                  ) : (
                    <Check className="h-4 w-4" />
                  )}
                  <span className="ml-1">{t('credentialcard.settings.save')}</span>
                </Button>
              </div>
            </div>

            {/* 单凭证代理：URL 留空=回退全局代理，"direct"=强制不走代理；账密留空=不改。立即生效无需重启。 */}
            <div className="space-y-1.5 border-t pt-4">
              <label className="text-sm font-medium">{t('credentialcard.settings.proxyLabel')}</label>
              <p className="text-xs text-muted-foreground">
                {t('credentialcard.settings.proxyHint')}
              </p>
              <div className="flex items-center gap-2">
                <Input
                  value={proxyValue}
                  onChange={(e) => setProxyValue(e.target.value)}
                  placeholder={t('credentialcard.settings.proxyPlaceholder')}
                  className="h-9 font-mono text-xs"
                  aria-label={t('credentialcard.settings.proxyUrlAria')}
                />
                <ProxyTestButton
                  proxyUrl={proxyValue}
                  proxyUsername={proxyUser}
                  proxyPassword={proxyPass}
                  className="h-9 shrink-0"
                />
                <Button
                  size="sm"
                  className="h-9 shrink-0"
                  onClick={handleSaveProxy}
                  disabled={savingProxy}
                >
                  {savingProxy ? <Loader2 className="h-4 w-4 animate-spin" /> : <Check className="h-4 w-4" />}
                  <span className="ml-1">{t('credentialcard.settings.save')}</span>
                </Button>
              </div>
              {/* 代理账号 + 密码并排一行 */}
              <div className="grid grid-cols-2 gap-2">
                <Input
                  value={proxyUser}
                  onChange={(e) => setProxyUser(e.target.value)}
                  placeholder={t('credentialcard.settings.proxyUserPlaceholder')}
                  className="h-9 text-xs"
                  autoComplete="off"
                  aria-label={t('credentialcard.settings.proxyUserAria')}
                />
                <Input
                  type="password"
                  value={proxyPass}
                  onChange={(e) => setProxyPass(e.target.value)}
                  placeholder={t('credentialcard.settings.proxyPassPlaceholder')}
                  className="h-9 text-xs"
                  autoComplete="new-password"
                  aria-label={t('credentialcard.settings.proxyPassAria')}
                />
              </div>
            </div>

            {/* 自定义 API 代挂配置(仅 custom_api 号):上游地址 / 上游密钥 / 请求上限 */}
            {isCustomApi && (
              <div className="space-y-2 border-t pt-4">
                <label className="text-sm font-medium">{t('credentialcard.settings.customApiLabel')}</label>
                <p className="text-xs text-muted-foreground">
                  {t('credentialcard.settings.customApiHint')}
                </p>
                <div className="space-y-1.5">
                  <label className="text-xs text-muted-foreground">{t('credentialcard.settings.baseUrlLabel')}</label>
                  <Input
                    value={customBaseUrl}
                    onChange={(e) => setCustomBaseUrl(e.target.value)}
                    placeholder="https://your-relay.example.com/v1"
                    className="h-9 font-mono text-xs"
                    aria-label={t('credentialcard.settings.baseUrlAria')}
                  />
                </div>
                <div className="space-y-1.5">
                  <label className="text-xs text-muted-foreground">{t('credentialcard.settings.upstreamKeyLabel')}</label>
                  <Input
                    type="password"
                    value={customApiKeyInput}
                    onChange={(e) => setCustomApiKeyInput(e.target.value)}
                    placeholder={t('credentialcard.settings.upstreamKeyPlaceholder')}
                    className="h-9 font-mono text-xs"
                    autoComplete="new-password"
                    aria-label={t('credentialcard.settings.upstreamKeyAria')}
                  />
                </div>
                <div className="space-y-1.5">
                  <label className="text-xs text-muted-foreground">{t('credentialcard.settings.requestLimitLabel')}</label>
                  <NumberStepper
                    value={customRequestLimit}
                    onChange={setCustomRequestLimit}
                    min={0}
                    step={100}
                    className="w-full"
                    aria-label={t('credentialcard.settings.requestLimitAria')}
                  />
                </div>
                <label className="flex cursor-pointer items-center gap-2 text-xs text-muted-foreground">
                  <Checkbox
                    checked={customResetCount}
                    onCheckedChange={(v) => setCustomResetCount(v === true)}
                    className="h-3.5 w-3.5"
                    aria-label={t('credentialcard.settings.resetCountAria')}
                  />
                  {t('credentialcard.settings.resetCountLabel')}
                </label>
                <Button
                  size="sm"
                  className="h-9 w-full"
                  onClick={handleSaveCustomApi}
                  disabled={savingCustomApi}
                >
                  {savingCustomApi ? (
                    <Loader2 className="h-4 w-4 animate-spin" />
                  ) : (
                    <Check className="h-4 w-4" />
                  )}
                  <span className="ml-1">{t('credentialcard.settings.saveCustomApi')}</span>
                </Button>
              </div>
            )}

            {/* 调度参数：优先级 + RPM 容量并排两列，各自独立步进器 + 保存。
                自定义 API 号不参与 RPM 饱和判定(按 优先级+在途 选号),只显优先级单列。 */}
            <div className={cn('grid gap-3 border-t pt-4', isCustomApi ? 'grid-cols-1' : 'grid-cols-2')}>
              <div className="space-y-1.5">
                <div className="text-sm font-medium">{t('credentialcard.settings.priorityLabel')}</div>
                <div className="text-xs text-muted-foreground">{t('credentialcard.settings.priorityHint')}</div>
                <div className="flex items-center gap-1.5">
                  <NumberStepper
                    value={priorityValue}
                    onChange={setPriorityValue}
                    min={0}
                    className="w-full"
                    aria-label={t('credentialcard.settings.priorityAria')}
                  />
                  <Button
                    size="sm"
                    className="h-9 shrink-0 px-2"
                    onClick={handlePriorityChange}
                    disabled={setPriority.isPending || priorityValue === credential.priority}
                    title={t('credentialcard.settings.savePriorityTitle')}
                  >
                    {setPriority.isPending ? (
                      <Loader2 className="h-4 w-4 animate-spin" />
                    ) : (
                      <Check className="h-4 w-4" />
                    )}
                  </Button>
                </div>
              </div>
              {!isCustomApi && (
              <div className="space-y-1.5">
                <div className="text-sm font-medium">{t('credentialcard.settings.rpmLabel')}</div>
                <div className="text-xs text-muted-foreground">{t('credentialcard.settings.rpmHint')}</div>
                <div className="flex items-center gap-1.5">
                  <NumberStepper
                    value={rpmLimitValue}
                    onChange={setRpmLimitValue}
                    min={0}
                    step={10}
                    className="w-full"
                    aria-label={t('credentialcard.settings.rpmAria')}
                  />
                  <Button
                    size="sm"
                    className="h-9 shrink-0 px-2"
                    onClick={handleRpmLimitChange}
                    disabled={setRpmLimit.isPending || rpmLimitValue === (credential.rpmLimit ?? 0)}
                    title={t('credentialcard.settings.saveRpmTitle')}
                  >
                    {setRpmLimit.isPending ? (
                      <Loader2 className="h-4 w-4 animate-spin" />
                    ) : (
                      <Check className="h-4 w-4" />
                    )}
                  </Button>
                </div>
              </div>
              )}
            </div>

            {/* 刷新 Token 失败诊断卡片（如 #98 的 client 过期 → 引导重新上号，而非裸 502）。 */}
            {refreshDiagnosis && (
              <DiagnosisCard
                diagnosis={refreshDiagnosis}
                onRetry={refreshDiagnosis.retriable ? handleForceRefresh : undefined}
                className="mt-1"
              />
            )}

            {/* Profile ARN 区域切换：列出该账号各 region 的 profile，卡片式单选列表展示每个 region 的
                ARN + 是否可用 + 订阅等级，选中即切过去（切对话走哪个上游 profile/端点，非改全局 region）。
                external_idp（多 region profile 选择）+ idc（通常单 region，用于确认/重新解析 profileArn）
                显示；social/api_key/custom_api 无 profile 概念不显示。逻辑抽到共享 RegionSwitcher，与运维页复用同款。 */}
            {canProbeRegion && (
            <div className="space-y-2 border-t pt-4">
              <div className="min-w-0">
                <div className="text-sm font-medium">{t('credentialcard.settings.profileArnLabel')}</div>
                <div className="text-xs text-muted-foreground">
                  {isIdc
                    ? t('credentialcard.settings.profileArnIdcHint')
                    : t('credentialcard.settings.profileArnHint')}
                </div>
              </div>
              <RegionSwitcher credentialId={credential.id} />
            </div>
            )}

            {/* 开关组：超额(仅Kiro号) + 启用凭据。自定义 API 无 base 额度概念,不显示超额,只留启用。 */}
            <div className={cn('grid gap-3 border-t pt-4', isCustomApi ? 'grid-cols-1' : 'grid-cols-2')}>
              {/* 超额（Overage）：接后端真开关，开启前二次确认（按量付费）。自定义 API 号不适用。 */}
              {!isCustomApi && (
              <div className="flex items-center justify-between gap-2 rounded-md border bg-secondary/30 px-3 py-2.5">
                <div className="flex min-w-0 items-center gap-1.5">
                  <Gauge className="h-4 w-4 shrink-0 text-muted-foreground" />
                  <div className="min-w-0">
                    <div className="text-sm font-medium">{t('credentialcard.settings.overageLabel')}</div>
                    <div className="truncate text-xs text-muted-foreground">
                      {overageEnabled ? t('credentialcard.settings.overageOn') : t('credentialcard.settings.overageOff')}
                    </div>
                  </div>
                </div>
                <Switch
                  checked={!!overageEnabled}
                  disabled={overageBusy}
                  onCheckedChange={handleOverageToggle}
                  aria-label={t('credentialcard.settings.overageAria')}
                />
              </div>
              )}
              {/* 启用 / 禁用 */}
              <div className="flex items-center justify-between gap-2 rounded-md border bg-secondary/30 px-3 py-2.5">
                <div className="flex min-w-0 items-center gap-1.5">
                  <Power className="h-4 w-4 shrink-0 text-muted-foreground" />
                  <div className="min-w-0">
                    <div className="text-sm font-medium">{t('credentialcard.settings.enableLabel')}</div>
                    <div className="truncate text-xs text-muted-foreground">
                      {credential.disabled ? t('credentialcard.settings.enableStatusDisabled') : t('credentialcard.settings.enableStatusScheduling')}
                    </div>
                  </div>
                </div>
                <Switch
                  checked={!credential.disabled}
                  onCheckedChange={handleToggleDisabled}
                  disabled={setDisabled.isPending}
                  aria-label={t('credentialcard.settings.enableAria')}
                />
              </div>
            </div>

            {/* 重置失败（Kiro 失败计数概念，从卡片正面移进此处）：清零该号失败/刷新失败计数。
                自定义 API 代挂号不走 Kiro 失败处置，不显示。 */}
            {!isCustomApi && (
            <div className="flex items-center justify-between gap-3 rounded-md border bg-secondary/30 px-3 py-3">
              <div className="min-w-0">
                <div className="text-sm font-medium">{t('credentialcard.settings.resetFailureLabel')}</div>
                <div className="text-xs text-muted-foreground">
                  {t('credentialcard.settings.resetFailureHint', { failures: credential.failureCount, refreshFailures: credential.refreshFailureCount })}
                </div>
              </div>
              <Button
                size="sm"
                variant="outline"
                className="h-9 shrink-0"
                onClick={handleReset}
                disabled={resetFailure.isPending || (credential.failureCount === 0 && credential.refreshFailureCount === 0)}
              >
                {resetFailure.isPending ? (
                  <Loader2 className="h-4 w-4 mr-1 animate-spin" />
                ) : (
                  <RefreshCw className="h-4 w-4 mr-1" />
                )}
                {t('credentialcard.settings.resetFailure')}
              </Button>
            </div>
            )}

            {/* 危险区：删除凭据收进底部，红色描边区隔，需先禁用 */}
            <div className="space-y-2 rounded-md border border-destructive/30 bg-destructive/[0.04] px-3 py-3">
              <div className="flex items-center justify-between gap-3">
                <div className="min-w-0">
                  <div className="text-sm font-medium text-destructive">{t('credentialcard.settings.deleteLabel')}</div>
                  <div className="text-xs text-muted-foreground">{t('credentialcard.settings.deleteHint')}</div>
                </div>
                <Button
                  size="sm"
                  variant="destructive"
                  className="h-9 shrink-0"
                  onClick={() => setShowDeleteDialog(true)}
                  disabled={!credential.disabled}
                  title={!credential.disabled ? t('credentialcard.settings.deleteDisabledTitle') : undefined}
                >
                  <Trash2 className="h-4 w-4 mr-1" />
                  {t('credentialcard.settings.delete')}
                </Button>
              </div>
              {!credential.disabled && (
                <p className="text-xs text-amber-500">{t('credentialcard.settings.deleteWarning')}</p>
              )}
            </div>
          </div>

          <DialogFooter className="shrink-0 border-t px-5 py-3">
            <Button variant="outline" size="sm" onClick={() => setShowSettings(false)}>
              {t('credentialcard.settings.close')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* 删除二次确认对话框 */}
      <Dialog open={showDeleteDialog} onOpenChange={setShowDeleteDialog}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>{t('credentialcard.deleteDialog.title', { id: credential.id })}</DialogTitle>
            <DialogDescription>
              {t('credentialcard.deleteDialog.description')}
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button
              variant="outline"
              onClick={() => setShowDeleteDialog(false)}
              disabled={deleteCredential.isPending}
            >
              {t('credentialcard.deleteDialog.cancel')}
            </Button>
            <Button
              variant="destructive"
              onClick={handleDelete}
              disabled={deleteCredential.isPending || !credential.disabled}
            >
              {deleteCredential.isPending && <Loader2 className="h-4 w-4 mr-1 animate-spin" />}
              {t('credentialcard.deleteDialog.confirm')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* 开启超额二次确认对话框 */}
      <Dialog open={showOverageConfirm} onOpenChange={setShowOverageConfirm}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>{t('credentialcard.overageDialog.title')}</DialogTitle>
            <DialogDescription>
              {t('credentialcard.overageDialog.description')}
            </DialogDescription>
          </DialogHeader>
          <div className="flex items-start gap-2 rounded-md border border-amber-500/20 bg-amber-500/10 px-3 py-2 text-xs text-amber-400">
            <ShieldAlert className="mt-0.5 h-3.5 w-3.5 shrink-0" />
            <span>{t('credentialcard.overageDialog.warning')}</span>
          </div>
          <DialogFooter>
            <Button
              variant="outline"
              onClick={() => setShowOverageConfirm(false)}
              disabled={overageBusy}
            >
              {t('credentialcard.overageDialog.cancel')}
            </Button>
            <Button onClick={handleConfirmEnableOverage} disabled={overageBusy}>
              {overageBusy && <Loader2 className="h-4 w-4 mr-1 animate-spin" />}
              {t('credentialcard.overageDialog.confirm')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

    </>
  )
}

