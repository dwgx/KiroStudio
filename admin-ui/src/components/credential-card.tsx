import { useState, useEffect, useMemo } from 'react'
import { useTranslation } from 'react-i18next'
import { useQueryClient } from '@tanstack/react-query'
import { toast } from 'sonner'
import { Settings, RefreshCw, Wallet, Trash2, Loader2, ClipboardCopy, ShieldAlert, Gauge, Check, Ban, Power, KeyRound } from 'lucide-react'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import { Checkbox } from '@/components/ui/checkbox'
import { Skeleton } from '@/components/ui/skeleton'
import { NumberStepper } from '@/components/ui/number-stepper'
import { RegionSwitcher } from '@/components/region-switcher'
import { RegionSelect } from '@/components/ui/region-select'
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
import { cooldownReasonLabel, isRateLimitCooldown } from '@/lib/cooldown'
import {
  formatCredits,
  formatLastUsed,
  formatCachedAt,
  maskProxyUrl,
  formatAmount,
} from '@/lib/credential-format'
import { DiagnosisCard } from '@/components/diagnosis-card'
import { CredentialRowBody } from '@/components/credential-row'
import {
  enableOverage,
  disableOverage,
  setCredentialName,
  setCredentialProxy,
  setCredentialAllowedModels,
  probeUpstreamModels,
  reprobeRegion,
  exportCredential,
} from '@/api/credentials'
import { authShortLabel, disabledReasonLabel, subscriptionLabel } from '@/lib/i18n-labels'
import {
  useSetDisabled,
  useSetPriority,
  useSetRpmLimit,
  useSetCustomApiConfig,
  useSetCredentialEndpoint,
  useSetCredentialApiRegion,
  useSetCredentialDeepseekNormalize,
  useSetCredentialModelMappingExempt,
  useResetFailure,
  useDeleteCredential,
  useForceRefreshToken,
  useUpdateRefreshToken,
  useCachedBalances,
  useConfigSnapshot,
} from '@/hooks/use-credentials'
import { useCtrlHeld } from '@/hooks/use-ctrl-held'

interface CredentialCardProps {
  credential: CredentialStatusItem
  onViewBalance: (id: number) => void
  selected: boolean
  /** 勾选框切换选中：additive=true 表示加/减选（保留其它选中项） */
  onToggleSelect: (additive?: boolean) => void
  /**
   * 行视图专用：Shift+左键区间选（把 [锚点, 本行] 闭区间并入选区）。卡片视图忽略。
   * 顺序与「哪些可选」由调用方持有 —— 见 `credential-row.tsx` 的 `onRangeSelect` 注释。
   */
  onRangeSelect?: () => void
  /** 按需（hover/“查询信息”）拉取的余额；若存在则优先于自动缓存快照展示。可为 null。 */
  balance: BalanceResponse | null
  loadingBalance: boolean
  /**
   * 视图形态。缺省 / `'card'` = 原卡片，**行为逐字不变**（全部旧调用点无需改）。
   * `'row'` 时只把 `<Card>` 主体换成 `<CredentialRowBody>`，
   * 三个弹框（设置 / 删除确认 / 超额确认）与全部 handler **原地复用**，不重造。
   */
  view?: 'card' | 'row'
  /** 行视图专用：多选 >1 时右键菜单变批量操作。卡片视图忽略。 */
  rowBatch?: {
    count: number
    onBatchDisable: () => void
    onBatchDelete: () => void
  }
}

// 五个展示格式化函数已提取到 `@/lib/credential-format`（卡片视图 / 行视图共用，
// 避免同一个数字在两个视图里显示不同）。行为逐字不变。

/**
 * region 快捷键：**实测真实命中集**，不是"常用区"。
 *
 * 依据：同一把 `ksk_` 在 eu-central-1 有 98.9% 成功率、在 us-east-1 是 100% 403
 * —— 即真正需要一键切换的只有这两个方向。其余区放搜索选择器里（AWS_REGIONS 20+ 个），
 * 全铺成按钮会把这两个真正有用的埋掉。
 */
const REGION_QUICK_PICKS = ['us-east-1', 'eu-central-1'] as const

/** 端点 → 实际 host（tooltip 展示"打哪个域名"）。与后端 host 构建保持一致：
 * cli=q.* / cli-runtime=runtime.*（两者都是 CLI 协议，host 不同 = 上游独立限流桶）、
 * ide=runtime.* 路径寻址。后端新增端点时需同步补充。
 * 空 region 回退 `us-east-1`：与后端 `effective_upstream_region` 的最终回退一致，
 * 避免 tooltip 显示 `q..amazonaws.com` 这类坏 host。 */
function endpointHost(name: string, region: string): string {
  const r = region || 'us-east-1'
  switch (name) {
    case 'cli':
      return `q.${r}.amazonaws.com`
    case 'cli-runtime':
      return `runtime.${r}.kiro.dev`
    case 'ide':
      return `runtime.${r}.kiro.dev/generateAssistantResponse`
    default:
      return name
  }
}

export function CredentialCard({
  credential,
  onViewBalance,
  selected,
  onToggleSelect,
  onRangeSelect,
  balance,
  loadingBalance,
  view = 'card',
  rowBatch,
}: CredentialCardProps) {
  const { t } = useTranslation()
  const [showSettings, setShowSettings] = useState(false)
  const [priorityValue, setPriorityValue] = useState(credential.priority)
  const [rpmLimitValue, setRpmLimitValue] = useState(credential.rpmLimit ?? 0)
  // 打开弹框瞬间把本地编辑值同步到最新 prop：上次打开改了值未保存就关闭的遗留值
  // 不得留在输入框（否则用户没动输入框点保存 = 提交遗留旧值覆盖远端新值）。
  // 依赖数组刻意只留 showSettings：打开期间轮询刷新的新值不得改写正在编辑的输入框。
  useEffect(() => {
    if (showSettings) {
      setPriorityValue(credential.priority)
      setRpmLimitValue(credential.rpmLimit ?? 0)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [showSettings])
  const [showDeleteDialog, setShowDeleteDialog] = useState(false)
  // 超额（Overage）开关：真开关接线状态
  const [overageBusy, setOverageBusy] = useState(false)
  const [showOverageConfirm, setShowOverageConfirm] = useState(false)
  // 别名/备注编辑：设置弹框内输入框的本地值 + 保存中状态
  const [nameValue, setNameValue] = useState(credential.name ?? '')
  const [savingName, setSavingName] = useState(false)
  // 点击掩码复制完整 Key：防重复点击（敏感导出端点，与设置页 copyOne 同模式）。
  const [copyKeyBusy, setCopyKeyBusy] = useState(false)

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
  const [customDeepseek, setCustomDeepseek] = useState(credential.deepseekNormalize ?? false)
  const [customMappingExempt, setCustomMappingExempt] = useState(credential.modelMappingExempt ?? false)
  // 上游模型探测：模型只能从上游获取（不硬编码）。探测结果 + 勾选（写 allowed_models）。
  const [upstreamModels, setUpstreamModels] = useState<string[] | null>(null)
  const [probeLoading, setProbeLoading] = useState(false)
  const [probeError, setProbeError] = useState('')
  const [upstreamSelected, setUpstreamSelected] = useState<Set<string>>(new Set())
  const [savingCustomApi, setSavingCustomApi] = useState(false)

  // 刷新 Token 失败诊断（结构化，如 client 过期引导重新上号）。
  const [refreshDiagnosis, setRefreshDiagnosis] = useState<OnboardingDiagnosis | null>(null)
  // 「更新 Token」弹框：粘贴新的 refreshToken（InvalidRefreshToken 禁用后的自助恢复通道）。
  const [showUpdateTokenDialog, setShowUpdateTokenDialog] = useState(false)
  const [updateTokenValue, setUpdateTokenValue] = useState('')
  // 「重新探测 region」在途状态（探测会打真实上游往返，期间按钮转圈 + 禁用其它 region 操作）。
  const [reprobeBusy, setReprobeBusy] = useState(false)

  const queryClient = useQueryClient()
  // 是否按住 Ctrl/Cmd:按住时卡片显示可点击手型 + 左键即多选(松开则普通左键不选中)
  const ctrlHeld = useCtrlHeld()

  const setDisabled = useSetDisabled()
  const setPriority = useSetPriority()
  const setRpmLimit = useSetRpmLimit()
  const setCustomApiConfig = useSetCustomApiConfig()
  const setEndpoint = useSetCredentialEndpoint()
  const setApiRegion = useSetCredentialApiRegion()
  const setDeepseekNormalize = useSetCredentialDeepseekNormalize()
  const setMappingExempt = useSetCredentialModelMappingExempt()
  // 可选端点由后端注册表给出（config.endpointNames），不在前端硬编码 ide/cli——
  // 后端加了新端点，面板自动多一个按钮。
  const configSnapshot = useConfigSnapshot()
  const allEndpointNames = configSnapshot.data?.endpointNames ?? []
  const resetFailure = useResetFailure()
  const deleteCredential = useDeleteCredential()
  const forceRefresh = useForceRefreshToken()
  const updateRefreshTokenMut = useUpdateRefreshToken()

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
  // 判据走稳定枚举码 cooldownCode（缺失时返回 false 走红色分支，无害降级）。
  const cooldownIsRateLimit = isRateLimitCooldown(credential.cooldownCode)
  // 冷却原因展示文案：已知 code 走 i18n，未知/老后端 fallback 后端中文原串。
  const cooldownReasonText = cooldownReasonLabel(credential.cooldownCode, credential.cooldownReason, t)

  // 点击掩码复制完整 Key：exportCredential 拿真值（与设置页 copyOne 同模式），
  // 取 kiroApiKey 字段（后端 export 返回 camelCase KiroCredentials，只有 api_key 号有掩码）。
  const handleCopyFullKey = async () => {
    if (copyKeyBusy) return
    setCopyKeyBusy(true)
    try {
      const obj = await exportCredential(credential.id)
      const key = typeof obj.kiroApiKey === 'string' ? obj.kiroApiKey : ''
      if (!key) {
        toast.error(t('credentialcard.toast.apiKeyMissing'))
        return
      }
      const ok = await copyToClipboard(key)
      ok ? toast.success(t('credentialcard.toast.apiKeyCopied')) : toast.error(t('credentialcard.toast.apiKeyCopyFailed'))
    } catch (err) {
      toast.error(extractErrorMessage(err))
    } finally {
      setCopyKeyBusy(false)
    }
  }

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

  // 探测代挂上游可用模型：模型只能从上游获取，网关不硬编码。结果填充 checkbox 供勾选。
  const handleProbeUpstream = async () => {
    if (!customBaseUrl.trim()) {
      toast.error(t('credentialcard.toast.baseUrlRequired'))
      return
    }
    // ⚠️ 2026-08-13：上游地址有未保存变更时，后端探测用的是**已保存**的 base_url——
    // 直接探会拿到旧上游的模型列表，误导勾选白名单。先让用户保存。
    if (customBaseUrl !== (credential.baseUrl ?? '')) {
      toast.warning(t('credentialcard.toast.saveBaseUrlFirst'))
      return
    }
    setProbeLoading(true)
    setProbeError('')
    try {
      const models = await probeUpstreamModels(credential.id)
      setUpstreamModels(models)
      if (models.length === 0) {
        setProbeError(t('credentialcard.toast.upstreamNoModels'))
      }
      // 初始勾选 = 当前已设的白名单（若已存在）。
      setUpstreamSelected(new Set(credential.allowedModels ?? []))
    } catch (err) {
      setUpstreamModels(null)
      setProbeError(extractErrorMessage(err))
    } finally {
      setProbeLoading(false)
    }
  }

  // 保存勾选的模型白名单（复用现有 allowed_models 硬门；空 = 不限制）。
  const handleSaveUpstreamModels = async () => {
    try {
      await setCredentialAllowedModels(credential.id, Array.from(upstreamSelected))
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
      toast.success(t('credentialcard.toast.allowedModelsSaved'))
    } catch (err) {
      toast.error(extractErrorMessage(err))
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
  // ksk_ API Key 号：自动端点 = q.*(cli) 优先、runtime.*(cli-runtime) 回退（两个独立限流桶）。
  const isApiKeyCred = credential.authMethod === 'api_key'
  // 🔴 ksk_ 号的端点按钮里**隐藏** `codewhisperer` / `amazonq`：
  // 两者已被 2026-08-08 部署实测证否 —— 真协议下对 ksk_ key 返
  // `400 ValidationException "The provided credential is invalid"`（此前用畸形 body
  // 探测得到的 200 是误导，见 `credentials.rs` 的 `API_KEY_ENDPOINT_ORDER` 注释）。
  // 它们仍注册在后端（经 API 显式设 `endpoint=codewhisperer` 依然可以），只是不在
  // 面板上诱人误选 —— 选中即必然 400，且该失败会占用重试预算、把本来能成功的尝试挤掉。
  //
  // 只对 ksk 号过滤：OAuth 号（social/idc/M365）不受此限，它们本就走自己的默认端点，
  // 不该被一起藏掉。
  const endpointNames = useMemo(
    () =>
      isApiKeyCred
        ? allEndpointNames.filter((n) => n !== 'codewhisperer' && n !== 'amazonq')
        : allEndpointNames,
    [allEndpointNames, isApiKeyCred]
  )
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
    // 与最新 prop 比对：无净变化不提交。改回「打开时的快照值」而远端已在弹框期间
    // 变化 → 视为有净变化照常提交（所见即所存，用户明确按了保存）；打开瞬间本地
    // state 已同步为最新 prop，没动输入框时两者恒等，不会拿旧值覆盖远端新值。
    if (priorityValue === credential.priority) return
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
    // 同上：与最新 prop 比对，无净变化不提交。
    if (rpmLimitValue === (credential.rpmLimit ?? 0)) return
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

  // endpoint=null → 清除固定，回到自动路由（ksk_ 号自动 cli）
  const handleEndpointChange = (endpoint: string | null) => {
    setEndpoint.mutate(
      { id: credential.id, endpoint },
      {
        onSuccess: (res) => toast.success(res.message),
        onError: (err) => toast.error(t('credentialcard.toast.operationFailed') + (err as Error).message),
      }
    )
  }

  /**
   * 手动指定该号的上游 region；`null` → 清除，回退全局默认。
   *
   * 为什么必须有这个入口：`ksk_` 是**按区授权**的 token，打错区上游恒 403
   * （实测同一把 key 在 eu-central-1 98.9% 成功、在 us-east-1 100% 403），
   * 而自动探测可能探错；`RegionSwitcher` 那条路对 api_key 号直接报「仅
   * External IdP / IdC 凭据支持」⇒ 在此之前面板上没有任何能改 ksk_ 号 region 的地方，
   * 探错了只能手改 credentials.json。
   *
   * 非法值由后端白名单拦（`is_supported_region`），错误原样 toast 出来 ——
   * 前端不再自己校验一遍，两份白名单迟早分叉。
   */
  const handleApiRegionChange = (apiRegion: string | null) => {
    setApiRegion.mutate(
      { id: credential.id, apiRegion },
      {
        onSuccess: (res) => toast.success(res.message),
        onError: (err) => toast.error(t('credentialcard.toast.operationFailed') + extractErrorMessage(err)),
      }
    )
  }

  /**
   * 重新探测该号上游实际生效的 region 并写回凭据（救「自动探测探错」的最后一招）。
   *
   * 探测是一次真实上游往返、可能耗时，按钮转圈；成功用**返回的 region** 提示
   * （而非刷新后自己猜），并 invalidate 凭据列表让卡片显示新值。
   */
  const handleReprobeRegion = async () => {
    setReprobeBusy(true)
    try {
      const res = await reprobeRegion(credential.id)
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
      // region 为 null = 探测未执行（已带 region / 非 api_key 号）——toast 用后端
      // message 说明原因，避免显示「成功：」空值误导（对抗审查 M1，2026-08-11）。
      if (res.region) {
        toast.success(t('credentialcard.toast.reprobeOk', { region: res.region }))
      } else {
        toast.info(res.message || t('credentialcard.toast.reprobeSkipped'))
      }
    } catch (err) {
      toast.error(t('credentialcard.toast.reprobeFailed') + extractErrorMessage(err))
    } finally {
      setReprobeBusy(false)
    }
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

  /**
   * 手动更新 refreshToken：号被 InvalidRefreshToken 禁用后（或需要轮换 token 时），
   * 从 Kiro IDE 拷贝新 token 粘贴提交。成功 toast 后端 message + 关弹框 + 清输入，
   * 失败 extractErrorMessage 原样提示（错误体字段名与刷新/诊断路径一致）。
   */
  const handleUpdateToken = () => {
    const token = updateTokenValue.trim()
    if (!token) {
      toast.error(t('credentialcard.updateToken.emptyError'))
      return
    }
    updateRefreshTokenMut.mutate(
      { id: credential.id, refreshToken: token },
      {
        onSuccess: (res) => {
          setShowUpdateTokenDialog(false)
          setUpdateTokenValue('')
          // 后端更新后不清 disabled：新 token 是否有效要等下次刷新才验证，
          // 必须提示用户手动重新启用该凭据，否则会一直停在禁用态。
          toast.success(res.message + t('credentialcard.updateToken.successHint'))
        },
        onError: (err) => {
          toast.error(t('credentialcard.toast.operationFailed') + extractErrorMessage(err))
        },
      }
    )
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

  // 打开本卡设置弹框（齿轮 / 右键 / 行视图「编辑…」三处共用同一份初始化）。
  // priority/rpmLimit 本地编辑值统一由 showSettings effect 在打开瞬间同步到最新 prop。
  const openSettings = () => {
    setNameValue(credential.name ?? '')
    setShowSettings(true)
  }

  return (
    <>
      {view === 'row' ? (
        <CredentialRowBody
          credential={credential}
          selected={selected}
          onToggleSelect={() => onToggleSelect(true)}
          onRangeSelect={onRangeSelect}
          onEdit={openSettings}
          onViewBalance={onViewBalance}
          shownBalance={shownBalance}
          balancePending={balancePending}
          // 缓存快照带 cachedAt（按需拉取的 balance prop 没有）→ 与卡片余额条同判据。
          cachedAt={balance ? null : cached?.cachedAt ?? null}
          endpointNames={endpointNames}
          batch={rowBatch}
        />
      ) : (
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
                {/* 端点徽标展示**实际生效**值；未被显式固定时加 "·auto" 后缀，
                    让「系统替我选了 cli」与「我固定了 cli」一眼可辨。 */}
                {credential.endpoint && (
                  <Badge
                    variant="outline"
                    title={
                      credential.endpointPinned
                        ? t('credentialcard.endpoint.pinnedTitle', { name: credential.endpoint })
                        : t('credentialcard.endpoint.autoTitle', { name: credential.endpoint })
                    }
                  >
                    {credential.endpoint}
                    {credential.endpointPinned === false && (
                      <span className="ml-1 opacity-60">·auto</span>
                    )}
                  </Badge>
                )}
              </CardTitle>
            </div>
            {/* 设置齿轮：集中优先级/启用/删除等操作，让卡片主体更干净 */}
            <Button
              size="sm"
              variant="ghost"
              className="h-8 w-8 shrink-0 p-0"
              onClick={openSettings}
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
                {cooldownReasonText ? ` · ${cooldownReasonText}` : ''}
                {' · '}{t('credentialcard.cooldown.remaining')}
                <span className="tabular-nums">{cooldownSeconds}</span>s
              </span>
            </div>
          )}

          {/* InvalidRefreshToken 禁用引导：自助恢复通道入口（用户不用再删号重加）。
              醒目琥珀横幅 + 提示去 Kiro IDE 拷新 token，点按钮直达「更新 Token」弹框。 */}
          {credential.disabled && credential.disabledReason === 'InvalidRefreshToken' && (
            <div className="flex items-start gap-2 rounded-md border border-amber-500/30 bg-amber-500/10 px-3 py-2 text-sm text-amber-300">
              <ShieldAlert className="h-4 w-4 shrink-0 mt-0.5" />
              <div className="min-w-0 flex-1 space-y-1.5">
                <p className="font-medium">{t('credentialcard.updateToken.invalidHint')}</p>
                <Button
                  size="sm"
                  variant="outline"
                  className="h-7 border-amber-500/40 bg-transparent text-amber-300 hover:bg-amber-500/20 hover:text-amber-200"
                  onClick={() => { setUpdateTokenValue(''); setShowUpdateTokenDialog(true) }}
                >
                  <KeyRound className="h-3.5 w-3.5 mr-1" />
                  {t('credentialcard.updateToken.action')}
                </Button>
              </div>
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
                {/* 点击掩码复制完整 Key（exportCredential 拿真值，与设置页 copyOne 同模式）。 */}
                <span
                  className={cn('font-mono font-medium', copyKeyBusy ? 'cursor-wait opacity-60' : 'cursor-pointer')}
                  title={copyKeyBusy ? t('credentialcard.info.copyKeyBusy') : t('credentialcard.info.copyKeyTitle')}
                  aria-label={copyKeyBusy ? t('credentialcard.info.copyKeyBusy') : t('credentialcard.info.copyKeyTitle')}
                  role="button"
                  tabIndex={0}
                  onClick={handleCopyFullKey}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter' || e.key === ' ') {
                      e.preventDefault()
                      handleCopyFullKey()
                    }
                  }}
                >
                  {credential.maskedApiKey}
                </span>
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
                      aria-label={t('credentialcard.info.copyProxyTitle')}
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
            /* 更新 Token：手动粘贴新 refreshToken（InvalidRefreshToken 禁用后的自助恢复通道，
               与上方「强制刷新」不同——那是让后端用旧 token 向上游换新，这直接写入新 token）。 */
            <Button
              size="sm"
              variant="outline"
              onClick={() => { setUpdateTokenValue(''); setShowUpdateTokenDialog(true) }}
              disabled={updateRefreshTokenMut.isPending || credential.authMethod === 'api_key'}
              title={credential.authMethod === 'api_key' ? t('credentialcard.action.refreshTokenApiKeyTitle') : t('credentialcard.updateToken.actionTitle')}
            >
              <KeyRound className={`h-4 w-4 mr-1 ${updateRefreshTokenMut.isPending ? 'animate-spin' : ''}`} />
              {t('credentialcard.updateToken.action')}
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
      )}
      {/* 更新 Token 对话框：粘贴新 refreshToken（InvalidRefreshToken 禁用后的自助恢复通道）。
          粘贴样式复用 add-credential-dialog 的 textarea 惯例（font-mono + 等宽大输入区）。 */}
      <Dialog open={showUpdateTokenDialog} onOpenChange={setShowUpdateTokenDialog}>
        <DialogContent className="max-w-lg">
          <DialogHeader>
            <DialogTitle>{t('credentialcard.updateToken.dialogTitle', { id: credential.id })}</DialogTitle>
            <DialogDescription>{t('credentialcard.updateToken.description')}</DialogDescription>
          </DialogHeader>
          <textarea
            value={updateTokenValue}
            onChange={(e) => setUpdateTokenValue(e.target.value)}
            disabled={updateRefreshTokenMut.isPending}
            placeholder={t('credentialcard.updateToken.placeholder')}
            aria-label={t('credentialcard.updateToken.aria')}
            className="flex min-h-[160px] w-full rounded-md border border-input bg-background px-3 py-2 text-sm ring-offset-background placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 disabled:cursor-not-allowed disabled:opacity-50 font-mono"
          />
          <DialogFooter>
            <Button
              type="button"
              variant="outline"
              onClick={() => setShowUpdateTokenDialog(false)}
              disabled={updateRefreshTokenMut.isPending}
            >
              {t('credentialcard.updateToken.cancel')}
            </Button>
            <Button onClick={handleUpdateToken} disabled={updateRefreshTokenMut.isPending}>
              {updateRefreshTokenMut.isPending
                ? t('credentialcard.updateToken.submitting')
                : t('credentialcard.updateToken.submit')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
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
                  id="cred-alias"
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
                  id="cred-proxy-url"
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
                  id="cred-proxy-user"
                  value={proxyUser}
                  onChange={(e) => setProxyUser(e.target.value)}
                  placeholder={t('credentialcard.settings.proxyUserPlaceholder')}
                  className="h-9 text-xs"
                  autoComplete="off"
                  aria-label={t('credentialcard.settings.proxyUserAria')}
                />
                <Input
                  id="cred-proxy-pass"
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
                    id="cred-base-url"
                    value={customBaseUrl}
                    onChange={(e) => {
                      setCustomBaseUrl(e.target.value)
                      // ⚠️ 2026-08-13：上游地址变更即失效旧探测结果 —— 面板的
                      // 「上游可用模型」是**旧 base_url 的探测产物**，继续展示会让
                      // 用户对着旧模型列表勾白名单（新上游可能根本没有这些模型）。
                      // 保存新上游后再点「探测上游模型」重探。
                      setUpstreamModels(null)
                      setUpstreamSelected(new Set())
                      setProbeError('')
                    }}
                    placeholder="https://your-relay.example.com/v1"
                    className="h-9 font-mono text-xs"
                    aria-label={t('credentialcard.settings.baseUrlAria')}
                  />
                </div>
                <div className="space-y-1.5">
                  <label className="text-xs text-muted-foreground">{t('credentialcard.settings.upstreamKeyLabel')}</label>
                  <Input
                    id="cred-upstream-key"
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
                {/* deepseek 协议归一化：开启后透传前修复请求体（模型名→deepseek-v4-flash、
                    thinking/effort 归一化、多轮 thinking 注入等），兼容 opencodezen 类上游。
                    即时保存（开关变化即调后端，不并入「保存自定义 API」按钮）。 */}
                <label className="flex cursor-pointer items-center gap-2 text-xs text-muted-foreground">
                  <Checkbox
                    checked={customDeepseek}
                    onCheckedChange={(v) => {
                      const next = v === true
                      setCustomDeepseek(next)
                      setDeepseekNormalize.mutate(
                        { id: credential.id, enabled: next },
                        {
                          onSuccess: () => toast.success(t('credentialcard.toast.deepseekSaved')),
                          onError: (err) =>
                            toast.error(t('credentialcard.toast.operationFailed') + (err as Error).message),
                        }
                      )
                    }}
                    className="h-3.5 w-3.5"
                    aria-label={t('credentialcard.settings.deepseekNormalizeAria')}
                  />
                  {t('credentialcard.settings.deepseekNormalizeLabel')}
                </label>

                {/* 上游模型探测：模型只能从上游获取（不硬编码）。探测结果勾选 = allowed_models 白名单。 */}
                <div className="space-y-1.5 border-t pt-3">
                  <div className="flex items-center justify-between gap-2">
                    <label className="text-xs text-muted-foreground">
                      {t('credentialcard.settings.upstreamModelsLabel')}
                    </label>
                    <Button
                      size="sm"
                      variant="outline"
                      className="h-7 text-xs"
                      onClick={handleProbeUpstream}
                      disabled={probeLoading}
                    >
                      {probeLoading ? (
                        <Loader2 className="h-3.5 w-3.5 animate-spin" />
                      ) : (
                        <RefreshCw className="h-3.5 w-3.5" />
                      )}
                      <span className="ml-1">{t('credentialcard.settings.probeUpstream')}</span>
                    </Button>
                  </div>
                  {probeError && <p className="text-xs text-red-400">{probeError}</p>}
                  {upstreamModels && (
                    <>
                      <div className="max-h-40 overflow-y-auto rounded-md border border-border/60 p-2">
                        {upstreamModels.map((m) => (
                          <label
                            key={m}
                            className="flex cursor-pointer items-center gap-2 py-0.5 text-xs"
                          >
                            <Checkbox
                              checked={upstreamSelected.has(m)}
                              onCheckedChange={(v) => {
                                setUpstreamSelected((prev) => {
                                  const next = new Set(prev)
                                  if (v) next.add(m)
                                  else next.delete(m)
                                  return next
                                })
                              }}
                              className="h-3.5 w-3.5"
                            />
                            <span className="min-w-0 truncate font-mono">{m}</span>
                          </label>
                        ))}
                      </div>
                      <Button
                        size="sm"
                        className="h-7 w-full text-xs"
                        onClick={handleSaveUpstreamModels}
                      >
                        <Check className="mr-1 h-3.5 w-3.5" />
                        {t('credentialcard.settings.saveUpstreamModels')}
                      </Button>
                    </>
                  )}
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

            {/* 全局模型映射豁免：开启后该号发上游时保持客户端原始模型名，跳过全局
                model_mapping。安全阀 —— 覆盖「映射后名该号上游不认」的场景。
                ⚠️ 刻意放在 isCustomApi 块**外**：后端该字段对 Kiro 号与 custom_api 号都生效
                （provider.rs 透传/主路径都按 `model_mapping_exempt` 跳过映射，无 custom_api
                限定），原实现把它塞进 custom_api 块导致 ksk/social 号永远够不到这个开关。
                即时保存（开关变化即调后端）。 */}
            <label className="flex cursor-pointer items-center gap-2 text-xs text-muted-foreground">
              <Checkbox
                checked={customMappingExempt}
                onCheckedChange={(v) => {
                  const next = v === true
                  setCustomMappingExempt(next)
                  setMappingExempt.mutate(
                    { id: credential.id, enabled: next },
                    {
                      onSuccess: () => toast.success(t('credentialcard.toast.mappingExemptSaved')),
                      onError: (err) =>
                        toast.error(t('credentialcard.toast.operationFailed') + (err as Error).message),
                    }
                  )
                }}
                className="h-3.5 w-3.5"
                aria-label={t('credentialcard.settings.mappingExemptAria')}
              />
              {t('credentialcard.settings.mappingExemptLabel')}
            </label>

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
                    aria-label={t('credentialcard.settings.savePriorityTitle')}
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
                    aria-label={t('credentialcard.settings.saveRpmTitle')}
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
              {/* 上游 region：与端点同款「实际生效值 + 是否被固定」二元组。
                  两个快捷键是**实测命中集**（us-east-1 / eu-central-1），其余走搜索选择器。
                  custom_api 透传号直接打 base_url，不拼 Kiro host，故隐藏。 */}
              {!isCustomApi && (
              <div className="space-y-1.5 sm:col-span-2">
                <div className="text-sm font-medium">{t('credentialcard.settings.regionLabel')}</div>
                <div className="text-xs text-muted-foreground">
                  {t('credentialcard.settings.regionHint', {
                    current: credential.effectiveRegion || t('labels.region.unset'),
                    mode: credential.regionPinned
                      ? t('credentialcard.settings.regionModePinned')
                      : t('credentialcard.settings.regionModeAuto'),
                  })}
                </div>
                <div className="flex flex-wrap items-center gap-1.5">
                  <Button
                    size="sm"
                    variant={credential.regionPinned === false ? 'default' : 'outline'}
                    className="h-9"
                    onClick={() => handleApiRegionChange(null)}
                    disabled={setApiRegion.isPending || credential.regionPinned === false}
                    title={t('credentialcard.settings.regionAutoTitle')}
                  >
                    {t('credentialcard.settings.regionAuto')}
                  </Button>
                  {REGION_QUICK_PICKS.map((code) => (
                    <Button
                      key={code}
                      size="sm"
                      variant={
                        credential.regionPinned && credential.effectiveRegion === code
                          ? 'default'
                          : 'outline'
                      }
                      className="h-9 font-mono text-xs"
                      onClick={() => handleApiRegionChange(code)}
                      disabled={
                        setApiRegion.isPending ||
                        (credential.regionPinned === true && credential.effectiveRegion === code)
                      }
                      title={t('credentialcard.settings.regionPinTitle', { name: code })}
                    >
                      {code}
                    </Button>
                  ))}
                  {setApiRegion.isPending && <Loader2 className="h-4 w-4 animate-spin" />}
                </div>
                {/* 其余 20+ 个区走搜索选择器。value 只在**已固定**时回填 ——
                    auto 态回填现值会让人以为已经定过区了（那正是探错时看不出来的原因）。 */}
                <RegionSelect
                  value={credential.regionPinned ? credential.effectiveRegion ?? '' : ''}
                  onChange={(code) => {
                    const c = code.trim()
                    if (c) handleApiRegionChange(c)
                  }}
                  disabled={setApiRegion.isPending || reprobeBusy}
                  triggerClassName="h-9"
                  placeholder={t('credentialcard.settings.regionSelectPlaceholder')}
                />
                {/* 重新探测：手动指定还是信不过时，让服务端重新探测真实生效 region 并写回。
                    探测是真实上游往返，期间转圈并禁用上面全部 region 操作，避免并发写。 */}
                <Button
                  size="sm"
                  variant="outline"
                  className="h-9"
                  onClick={handleReprobeRegion}
                  disabled={setApiRegion.isPending || reprobeBusy}
                  title={t('credentialcard.settings.reprobeRegionTitle')}
                >
                  {reprobeBusy ? (
                    <Loader2 className="h-4 w-4 animate-spin" />
                  ) : (
                    <RefreshCw className="h-4 w-4" />
                  )}
                  <span className="ml-1">{t('credentialcard.settings.reprobeRegion')}</span>
                </Button>
              </div>
              )}
              {/* 端点切换：默认「自动」——ksk_ 号 q.*(cli) 优先、runtime.*(cli-runtime) 回退
                  （两个 host = 上游独立限流桶），其余回退全局默认。
                  固定成具体端点是**救急旋钮**（上游协议变化时不改代码即可切）。
                  custom_api 透传号不走 Kiro 端点体系，故隐藏。 */}
              {!isCustomApi && (
              <div className="space-y-1.5 sm:col-span-2">
                <div className="text-sm font-medium">{t('credentialcard.settings.endpointLabel')}</div>
                <div className="text-xs text-muted-foreground">
                  {t('credentialcard.settings.endpointHint', {
                    current: credential.endpoint,
                    mode: credential.endpointPinned
                      ? t('credentialcard.settings.endpointModePinned')
                      : t('credentialcard.settings.endpointModeAuto'),
                  })}
                </div>
                <div className="flex flex-wrap items-center gap-1.5">
                  <Button
                    size="sm"
                    variant={credential.endpointPinned === false ? 'default' : 'outline'}
                    className="h-9"
                    onClick={() => handleEndpointChange(null)}
                    disabled={setEndpoint.isPending || credential.endpointPinned === false}
                    title={t('credentialcard.settings.endpointAutoTitle')}
                  >
                    {isApiKeyCred
                      ? t('credentialcard.settings.endpointAutoBucket')
                      : t('credentialcard.settings.endpointAuto')}
                  </Button>
                  {endpointNames.map((name) => (
                    <Button
                      key={name}
                      size="sm"
                      variant={
                        credential.endpointPinned && credential.endpoint === name
                          ? 'default'
                          : 'outline'
                      }
                      className="h-9 font-mono text-xs"
                      onClick={() => handleEndpointChange(name)}
                      disabled={
                        setEndpoint.isPending ||
                        (credential.endpointPinned === true && credential.endpoint === name)
                      }
                      title={t('credentialcard.settings.endpointHostTitle', {
                        host: endpointHost(name, credential.effectiveRegion ?? ''),
                      })}
                    >
                      {name}
                    </Button>
                  ))}
                  {setEndpoint.isPending && <Loader2 className="h-4 w-4 animate-spin" />}
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

