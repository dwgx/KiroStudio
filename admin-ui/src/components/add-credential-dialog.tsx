import { useState } from 'react'
import { useTranslation } from 'react-i18next'
import { toast } from 'sonner'
import { CheckCircle2, XCircle, AlertCircle, AlertTriangle, Loader2 } from 'lucide-react'
import { useQueryClient } from '@tanstack/react-query'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogFooter,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { NumberStepper } from '@/components/ui/number-stepper'
import { Select } from '@/components/ui/select'
import { ProxyTestButton } from '@/components/proxy-test-button'
import { useAddCredential, useCredentials } from '@/hooks/use-credentials'
import { extractErrorMessage, sha256Hex } from '@/lib/utils'
import { LoginDialog } from '@/components/login-dialog'
import type { AddCredentialRequest } from '@/types/api'

interface AddCredentialDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

type AuthMethod = 'social' | 'idc' | 'external_idp' | 'api_key' | 'custom_api'
type Tab = 'manual' | 'paste' | 'login'

// 从字符串中挑第一个非空值
const pickString = (...values: unknown[]): string | undefined => {
  for (const value of values) {
    if (typeof value === 'string' && value.trim()) return value.trim()
  }
  return undefined
}

// 归一化认证方式字段
const normalizeAuthMethod = (
  value: string | undefined
): AuthMethod | undefined => {
  if (!value) return undefined
  const n = value.trim().toLowerCase().replace(/-/g, '_')
  if (n === 'apikey' || n === 'api_key') return 'api_key'
  if (n === 'externalidp' || n === 'external_idp' || n === 'azuread' || n === 'azure_ad') {
    return 'external_idp'
  }
  if (n === 'idc' || n === 'builder_id' || n === 'iam') return 'idc'
  if (n === 'social') return 'social'
  return undefined
}

// 容错 JSON 解析：尽力把「就算写错的 JSON」也纠正成可解析结构。
// 依次尝试：直接 parse → 逐步修复（去尾逗号 / 单引号转双引号 / 截取首个 {…} 或 […] 片段 / 补齐缺失括号）
function tolerantJsonParse(raw: string): unknown {
  const attempts: string[] = []
  const text = raw.trim()
  attempts.push(text)

  // 截取第一个 { 或 [ 到对应的最后一个 } 或 ]，剥掉前后杂物（如粘贴带上的说明文字）
  const firstBrace = text.indexOf('{')
  const firstBracket = text.indexOf('[')
  let sliceStart = -1
  let closeChar = ''
  if (firstBracket !== -1 && (firstBrace === -1 || firstBracket < firstBrace)) {
    sliceStart = firstBracket
    closeChar = ']'
  } else if (firstBrace !== -1) {
    sliceStart = firstBrace
    closeChar = '}'
  }
  let sliced = text
  if (sliceStart !== -1) {
    const lastClose = text.lastIndexOf(closeChar)
    sliced = lastClose > sliceStart ? text.slice(sliceStart, lastClose + 1) : text.slice(sliceStart)
    attempts.push(sliced)
  }

  // 修复函数：去尾逗号 + 单引号转双引号 + 给裸键补引号
  const repair = (s: string): string => {
    let out = s
    // 单引号字符串 → 双引号（简单场景：'...' 且内部无双引号）
    out = out.replace(/'([^'\\]*(?:\\.[^'\\]*)*)'/g, (_m, inner) => `"${inner.replace(/"/g, '\\"')}"`)
    // 去掉对象/数组结尾多余逗号： ,}  ,]
    out = out.replace(/,\s*([}\]])/g, '$1')
    // 给未加引号的对象键补双引号： {key:  或 ,key:
    out = out.replace(/([{,]\s*)([A-Za-z_$][\w$]*)(\s*:)/g, '$1"$2"$3')
    return out
  }

  attempts.push(repair(text))
  if (sliceStart !== -1) attempts.push(repair(sliced))

  // 补齐缺失的收尾括号（统计未闭合的 { [ 依序补回）
  const balance = (s: string): string => {
    let inStr = false
    let esc = false
    const stack: string[] = []
    for (const ch of s) {
      if (inStr) {
        if (esc) esc = false
        else if (ch === '\\') esc = true
        else if (ch === '"') inStr = false
        continue
      }
      if (ch === '"') inStr = true
      else if (ch === '{') stack.push('}')
      else if (ch === '[') stack.push(']')
      else if (ch === '}' || ch === ']') stack.pop()
    }
    return s + stack.reverse().join('')
  }

  const base = sliceStart !== -1 ? sliced : text
  attempts.push(balance(repair(base)))

  let lastErr: unknown
  for (const candidate of attempts) {
    if (!candidate || !candidate.trim()) continue
    try {
      return JSON.parse(candidate)
    } catch (e) {
      lastErr = e
    }
  }
  throw lastErr instanceof Error ? lastErr : new Error('无法解析 JSON')
}

// 把任意识别到的原始对象拉平成一个统一的凭据请求。兼容 camelCase / snake_case /
// KAM 平铺(refreshToken 直接在对象上) / KAM 嵌套(credentials.refreshToken)。
function toAddRequest(raw: Record<string, unknown>): AddCredentialRequest | null {
  // KAM 嵌套结构：真正的凭据字段在 credentials 里，外层可能带 email/machineId
  const nested =
    raw.credentials && typeof raw.credentials === 'object'
      ? (raw.credentials as Record<string, unknown>)
      : null
  const g = (...keys: string[]): unknown => {
    for (const k of keys) {
      if (nested && nested[k] !== undefined) return nested[k]
      if (raw[k] !== undefined) return raw[k]
    }
    return undefined
  }

  const kiroApiKey = pickString(g('kiroApiKey', 'kiro_api_key', 'apiKey', 'api_key'))
  const refreshToken = pickString(g('refreshToken', 'refresh_token'))
  const explicitMethod = normalizeAuthMethod(pickString(g('authMethod', 'auth_method')))

  // 无 token 也无 apiKey → 不是有效凭据，跳过
  if (!refreshToken && !kiroApiKey) return null

  if (kiroApiKey && !refreshToken) {
    return {
      authMethod: 'api_key',
      kiroApiKey,
      priority: typeof g('priority') === 'number' ? (g('priority') as number) : undefined,
      authRegion: pickString(g('authRegion', 'auth_region', 'region')),
      apiRegion: pickString(g('apiRegion', 'api_region')),
      machineId: pickString(g('machineId', 'machine_id')),
      endpoint: pickString(g('endpoint')),
    }
  }

  const clientId = pickString(g('clientId', 'client_id'))
  const clientSecret = pickString(g('clientSecret', 'client_secret'))
  const tokenEndpoint = pickString(g('tokenEndpoint', 'token_endpoint'))

  // 判定认证方式：显式声明优先，其次按字段推断
  const authMethod: AuthMethod =
    explicitMethod === 'external_idp' || tokenEndpoint
      ? 'external_idp'
      : explicitMethod === 'idc' || (clientId && clientSecret)
        ? 'idc'
        : 'social'

  return {
    authMethod,
    refreshToken,
    accessToken: pickString(g('accessToken', 'access_token')),
    clientId,
    clientSecret,
    tokenEndpoint: authMethod === 'external_idp' ? tokenEndpoint : undefined,
    issuerUrl: authMethod === 'external_idp' ? pickString(g('issuerUrl', 'issuer_url')) : undefined,
    scopes: authMethod === 'external_idp' ? pickString(g('scopes')) : undefined,
    profileArn: pickString(g('profileArn', 'profile_arn')),
    expiresAt: pickString(g('expiresAt', 'expires_at', 'expired')),
    authRegion: pickString(g('authRegion', 'auth_region', 'region')),
    apiRegion: pickString(g('apiRegion', 'api_region')),
    priority: typeof g('priority') === 'number' ? (g('priority') as number) : undefined,
    machineId: pickString(g('machineId', 'machine_id')),
    endpoint: pickString(g('endpoint')),
  }
}

// 从解析出的任意结构里抽取一批凭据请求。
// 兼容：数组 / {credentials:[...]} / {accounts:[...]}(KAM) / 单对象
function extractCredentials(parsed: unknown): AddCredentialRequest[] {
  let items: unknown[]
  if (Array.isArray(parsed)) {
    items = parsed
  } else if (parsed && typeof parsed === 'object') {
    const obj = parsed as Record<string, unknown>
    if (Array.isArray(obj.accounts)) items = obj.accounts
    else if (Array.isArray(obj.credentials)) items = obj.credentials
    else items = [obj]
  } else {
    return []
  }

  const reqs: AddCredentialRequest[] = []
  for (const item of items) {
    if (item && typeof item === 'object') {
      const req = toAddRequest(item as Record<string, unknown>)
      if (req) reqs.push(req)
    }
  }
  return reqs
}

interface PasteResult {
  index: number
  status: 'pending' | 'adding' | 'success' | 'duplicate' | 'failed'
  email?: string
  credentialId?: number
  error?: string
}

export function AddCredentialDialog({ open, onOpenChange }: AddCredentialDialogProps) {
  const { t } = useTranslation()
  const [tab, setTab] = useState<Tab>('manual')

  // 手动添加表单
  const [refreshToken, setRefreshToken] = useState('')
  const [kiroApiKey, setKiroApiKey] = useState('')
  const [authMethod, setAuthMethod] = useState<AuthMethod>('social')
  const [authRegion, setAuthRegion] = useState('')
  const [apiRegion, setApiRegion] = useState('')
  const [clientId, setClientId] = useState('')
  const [clientSecret, setClientSecret] = useState('')
  const [tokenEndpoint, setTokenEndpoint] = useState('')
  const [issuerUrl, setIssuerUrl] = useState('')
  const [scopes, setScopes] = useState('')
  const [profileArn, setProfileArn] = useState('')
  // 自定义 API 代挂透传字段
  const [baseUrl, setBaseUrl] = useState('')
  const [customApiKey, setCustomApiKey] = useState('')
  const [requestLimit, setRequestLimit] = useState('')
  const [priority, setPriority] = useState('0')
  const [machineId, setMachineId] = useState('')
  const [proxyUrl, setProxyUrl] = useState('')
  const [proxyUsername, setProxyUsername] = useState('')
  const [proxyPassword, setProxyPassword] = useState('')
  const [endpoint, setEndpoint] = useState('')

  // 导入（粘贴）
  const [pasteInput, setPasteInput] = useState('')
  const [importing, setImporting] = useState(false)
  const [pasteResults, setPasteResults] = useState<PasteResult[]>([])

  const { mutate, isPending } = useAddCredential()
  const { mutateAsync: addCredentialAsync } = useAddCredential()
  const { data: existingCredentials } = useCredentials()
  const queryClient = useQueryClient()

  const resetManual = () => {
    setRefreshToken('')
    setKiroApiKey('')
    setAuthMethod('social')
    setAuthRegion('')
    setApiRegion('')
    setClientId('')
    setClientSecret('')
    setTokenEndpoint('')
    setIssuerUrl('')
    setScopes('')
    setProfileArn('')
    setPriority('0')
    setMachineId('')
    setProxyUrl('')
    setProxyUsername('')
    setProxyPassword('')
    setEndpoint('')
    setBaseUrl('')
    setCustomApiKey('')
    setRequestLimit('')
  }

  const resetPaste = () => {
    setPasteInput('')
    setPasteResults([])
  }

  const isApiKey = authMethod === 'api_key'

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault()

    if (isApiKey) {
      if (!kiroApiKey.trim()) {
        toast.error(t('addcredentialdialog.validate.apiKeyRequired'))
        return
      }
    } else if (authMethod === 'custom_api') {
      // 自定义 API 代挂:只需 base URL(下方校验),不需要 Refresh Token。
      if (!baseUrl.trim()) {
        toast.error(t('addcredentialdialog.validate.baseUrlRequired'))
        return
      }
    } else {
      if (!refreshToken.trim()) {
        toast.error(t('addcredentialdialog.validate.refreshTokenRequired'))
        return
      }
      if (authMethod === 'idc' && (!clientId.trim() || !clientSecret.trim())) {
        toast.error(t('addcredentialdialog.validate.idcFieldsRequired'))
        return
      }
      if (authMethod === 'external_idp' && (!clientId.trim() || !tokenEndpoint.trim())) {
        toast.error(t('addcredentialdialog.validate.externalIdpFieldsRequired'))
        return
      }
    }

    mutate(
      {
        authMethod,
        refreshToken: isApiKey ? undefined : refreshToken.trim(),
        kiroApiKey: isApiKey ? kiroApiKey.trim() : undefined,
        authRegion: authRegion.trim() || undefined,
        apiRegion: apiRegion.trim() || undefined,
        clientId: isApiKey ? undefined : clientId.trim() || undefined,
        clientSecret: isApiKey ? undefined : clientSecret.trim() || undefined,
        tokenEndpoint: authMethod === 'external_idp' ? tokenEndpoint.trim() || undefined : undefined,
        issuerUrl: authMethod === 'external_idp' ? issuerUrl.trim() || undefined : undefined,
        scopes: authMethod === 'external_idp' ? scopes.trim() || undefined : undefined,
        profileArn: authMethod === 'external_idp' ? profileArn.trim() || undefined : undefined,
        baseUrl: authMethod === 'custom_api' ? baseUrl.trim() || undefined : undefined,
        apiKey: authMethod === 'custom_api' ? customApiKey.trim() || undefined : undefined,
        requestLimit: authMethod === 'custom_api' ? (parseInt(requestLimit) || undefined) : undefined,
        priority: parseInt(priority) || 0,
        machineId: machineId.trim() || undefined,
        proxyUrl: proxyUrl.trim() || undefined,
        proxyUsername: proxyUsername.trim() || undefined,
        proxyPassword: proxyPassword.trim() || undefined,
        endpoint: endpoint.trim() || undefined,
      },
      {
        onSuccess: (data) => {
          toast.success(data.message)
          onOpenChange(false)
          resetManual()
        },
        onError: (error: unknown) => {
          toast.error(t('addcredentialdialog.toast.addFailed') + extractErrorMessage(error))
        },
      }
    )
  }

  // 导入（粘贴）：容错解析 → 逐个添加，一条失败不影响其它
  const handlePasteImport = async () => {
    let reqs: AddCredentialRequest[]
    try {
      const parsed = tolerantJsonParse(pasteInput)
      reqs = extractCredentials(parsed)
    } catch (error) {
      toast.error(t('addcredentialdialog.toast.jsonUnrecognized') + extractErrorMessage(error))
      return
    }

    if (reqs.length === 0) {
      toast.error(t('addcredentialdialog.toast.noCredentials'))
      return
    }

    setImporting(true)
    setPasteResults(reqs.map((_, i) => ({ index: i + 1, status: 'pending' as const })))

    // 已有凭据 hash，用于本地去重
    const existingOauth = new Set(
      existingCredentials?.credentials
        .map(c => c.refreshTokenHash)
        .filter((h): h is string => Boolean(h)) || []
    )
    const existingApiKey = new Set(
      existingCredentials?.credentials
        .map(c => c.apiKeyHash)
        .filter((h): h is string => Boolean(h)) || []
    )

    let success = 0
    let dup = 0
    let fail = 0

    for (let i = 0; i < reqs.length; i++) {
      const req = reqs[i]
      setPasteResults(prev => {
        const next = [...prev]
        next[i] = { ...next[i], status: 'adding' }
        return next
      })

      try {
        // 本地去重
        const isKey = req.authMethod === 'api_key'
        const hash = await sha256Hex(isKey ? req.kiroApiKey || '' : req.refreshToken || '')
        if ((isKey ? existingApiKey : existingOauth).has(hash)) {
          dup++
          setPasteResults(prev => {
            const next = [...prev]
            next[i] = { ...next[i], status: 'duplicate', error: t('addcredentialdialog.paste.duplicateExists') }
            return next
          })
          continue
        }

        const added = await addCredentialAsync(req)
        success++
        if (isKey) existingApiKey.add(hash)
        else existingOauth.add(hash)
        setPasteResults(prev => {
          const next = [...prev]
          next[i] = {
            ...next[i],
            status: 'success',
            email: added.email || undefined,
            credentialId: added.credentialId,
          }
          return next
        })
      } catch (error) {
        fail++
        setPasteResults(prev => {
          const next = [...prev]
          next[i] = { ...next[i], status: 'failed', error: extractErrorMessage(error) }
          return next
        })
      }
    }

    setImporting(false)
    queryClient.invalidateQueries({ queryKey: ['credentials'] })

    if (fail === 0 && dup === 0) {
      toast.success(t('addcredentialdialog.toast.importSuccess', { success }))
    } else {
      toast.info(t('addcredentialdialog.toast.importDone', { success, dup, fail }))
    }
  }

  const pasteIcon = (status: PasteResult['status']) => {
    switch (status) {
      case 'pending':
        return <div className="w-4 h-4 rounded-full border-2 border-gray-300" />
      case 'adding':
        return <Loader2 className="w-4 h-4 animate-spin text-blue-500" />
      case 'success':
        return <CheckCircle2 className="w-4 h-4 text-green-500" />
      case 'duplicate':
        return <AlertCircle className="w-4 h-4 text-yellow-500" />
      case 'failed':
        return <XCircle className="w-4 h-4 text-red-500" />
    }
  }

  return (
    <>
      <Dialog
        open={open && tab !== 'login'}
        onOpenChange={(o) => {
          if (!o && !importing) {
            resetPaste()
          }
          onOpenChange(o)
        }}
      >
        <DialogContent className="sm:max-w-lg max-h-[85vh] flex flex-col">
          <DialogHeader>
            <DialogTitle>{t('addcredentialdialog.title')}</DialogTitle>
          </DialogHeader>

          {/* 模式切换 tab */}
          <div className="flex border-b border-[#2e2e2e]">
            <button
              type="button"
              onClick={() => setTab('manual')}
              className={`flex-1 py-2 text-sm font-medium border-b-2 transition-colors ${
                tab === 'manual'
                  ? 'border-[#0070f3] text-[#ededed]'
                  : 'border-transparent text-[#888] hover:text-[#ededed]'
              }`}
            >
              {t('addcredentialdialog.tab.manual')}
            </button>
            <button
              type="button"
              onClick={() => setTab('paste')}
              className={`flex-1 py-2 text-sm font-medium border-b-2 transition-colors ${
                tab === 'paste'
                  ? 'border-[#0070f3] text-[#ededed]'
                  : 'border-transparent text-[#888] hover:text-[#ededed]'
              }`}
            >
              {t('addcredentialdialog.tab.paste')}
            </button>
            <button
              type="button"
              onClick={() => setTab('login')}
              className={`flex-1 py-2 text-sm font-medium border-b-2 transition-colors ${
                tab === 'login'
                  ? 'border-[#0070f3] text-[#ededed]'
                  : 'border-transparent text-[#888] hover:text-[#ededed]'
              }`}
            >
              {t('addcredentialdialog.tab.login')}
            </button>
          </div>

          {tab === 'manual' && (
          <form onSubmit={handleSubmit} className="flex flex-col min-h-0 flex-1">
            <div className="space-y-4 py-4 overflow-y-auto flex-1 pr-1">
              {/* 认证方式 */}
              <div className="space-y-2">
                <label htmlFor="authMethod" className="text-sm font-medium">
                  {t('addcredentialdialog.field.authMethod.label')}
                </label>
                <Select<AuthMethod>
                  id="authMethod"
                  value={authMethod}
                  onChange={setAuthMethod}
                  disabled={isPending}
                  options={[
                    { value: 'social', label: 'Social' },
                    { value: 'idc', label: 'IdC/Builder-ID/IAM' },
                    { value: 'external_idp', label: 'External IdP' },
                    { value: 'api_key', label: 'API Key' },
                    { value: 'custom_api', label: t('addcredentialdialog.field.authMethod.customApi') },
                  ]}
                />
              </div>

              {/* Kiro API Key (API Key 模式) */}
              {isApiKey && (
                <div className="space-y-2">
                  <label htmlFor="kiroApiKey" className="text-sm font-medium">
                    Kiro API Key <span className="text-red-500">*</span>
                  </label>
                  <Input
                    id="kiroApiKey"
                    type="password"
                    placeholder={t('addcredentialdialog.field.kiroApiKey.placeholder')}
                    value={kiroApiKey}
                    onChange={(e) => setKiroApiKey(e.target.value)}
                    disabled={isPending}
                  />
                </div>
              )}

              {/* Refresh Token (OAuth 模式；自定义 API 不需要) */}
              {!isApiKey && authMethod !== 'custom_api' && (
                <div className="space-y-2">
                  <label htmlFor="refreshToken" className="text-sm font-medium">
                    Refresh Token <span className="text-red-500">*</span>
                  </label>
                  <Input
                    id="refreshToken"
                    type="password"
                    placeholder={t('addcredentialdialog.field.refreshToken.placeholder')}
                    value={refreshToken}
                    onChange={(e) => setRefreshToken(e.target.value)}
                    disabled={isPending}
                  />
                </div>
              )}

              {/* 自定义 API 代挂透传：上游地址 + 密钥 + 请求上限 */}
              {authMethod === 'custom_api' && (
                <div className="space-y-3 rounded-md border border-border bg-secondary/20 p-3">
                  <div className="text-xs text-muted-foreground">
                    {t('addcredentialdialog.customApi.desc')}
                  </div>
                  <div className="space-y-2">
                    <label htmlFor="baseUrl" className="text-sm font-medium">
                      {t('addcredentialdialog.field.baseUrl.label')} <span className="text-red-500">*</span>
                    </label>
                    <Input
                      id="baseUrl"
                      placeholder={t('addcredentialdialog.field.baseUrl.placeholder')}
                      value={baseUrl}
                      onChange={(e) => setBaseUrl(e.target.value)}
                      disabled={isPending}
                    />
                  </div>
                  <div className="space-y-2">
                    <label htmlFor="customApiKey" className="text-sm font-medium">{t('addcredentialdialog.field.customApiKey.label')}</label>
                    <Input
                      id="customApiKey"
                      type="password"
                      placeholder={t('addcredentialdialog.field.customApiKey.placeholder')}
                      value={customApiKey}
                      onChange={(e) => setCustomApiKey(e.target.value)}
                      disabled={isPending}
                    />
                  </div>
                  <div className="space-y-2">
                    <label htmlFor="requestLimit" className="text-sm font-medium">{t('addcredentialdialog.field.requestLimit.label')}</label>
                    <Input
                      id="requestLimit"
                      type="number"
                      placeholder={t('addcredentialdialog.field.requestLimit.placeholder')}
                      value={requestLimit}
                      onChange={(e) => setRequestLimit(e.target.value)}
                      disabled={isPending}
                    />
                  </div>
                </div>
              )}

              {/* Region 配置(Kiro 专属:Token 刷新/API 请求 region)。自定义 API 代挂透传不适用,不显示 */}
              {authMethod !== 'custom_api' && (
              <div className="space-y-2">
                <label className="text-sm font-medium">{t('addcredentialdialog.field.region.label')}</label>
                <div className="grid grid-cols-2 gap-2">
                  <div>
                    <Input
                      id="authRegion"
                      placeholder="Auth Region"
                      value={authRegion}
                      onChange={(e) => setAuthRegion(e.target.value)}
                      disabled={isPending}
                    />
                  </div>
                  <div>
                    <Input
                      id="apiRegion"
                      placeholder="API Region"
                      value={apiRegion}
                      onChange={(e) => setApiRegion(e.target.value)}
                      disabled={isPending}
                    />
                  </div>
                </div>
                <p className="text-xs text-muted-foreground">
                  {t('addcredentialdialog.field.region.help')}
                </p>
              </div>
              )}
              {/* IdC/Builder-ID/IAM 额外字段 */}
              {authMethod === 'idc' && (
                <>
                  <div className="space-y-2">
                    <label htmlFor="clientId" className="text-sm font-medium">
                      Client ID <span className="text-red-500">*</span>
                    </label>
                    <Input
                      id="clientId"
                      placeholder={t('addcredentialdialog.field.clientId.placeholder')}
                      value={clientId}
                      onChange={(e) => setClientId(e.target.value)}
                      disabled={isPending}
                    />
                  </div>
                  <div className="space-y-2">
                    <label htmlFor="clientSecret" className="text-sm font-medium">
                      Client Secret <span className="text-red-500">*</span>
                    </label>
                    <Input
                      id="clientSecret"
                      type="password"
                      placeholder={t('addcredentialdialog.field.clientSecret.placeholder')}
                      value={clientSecret}
                      onChange={(e) => setClientSecret(e.target.value)}
                      disabled={isPending}
                    />
                  </div>
                </>
              )}

              {/* External IdP 额外字段 */}
              {authMethod === 'external_idp' && (
                <>
                  <div className="space-y-2">
                    <label htmlFor="externalClientId" className="text-sm font-medium">
                      Client ID <span className="text-red-500">*</span>
                    </label>
                    <Input
                      id="externalClientId"
                      placeholder="8dd3db0b-980a-4af5-8bd2-1efc66497d98"
                      value={clientId}
                      onChange={(e) => setClientId(e.target.value)}
                      disabled={isPending}
                    />
                  </div>
                  <div className="space-y-2">
                    <label htmlFor="tokenEndpoint" className="text-sm font-medium">
                      Token Endpoint <span className="text-red-500">*</span>
                    </label>
                    <Input
                      id="tokenEndpoint"
                      placeholder="https://login.microsoftonline.com/.../oauth2/v2.0/token"
                      value={tokenEndpoint}
                      onChange={(e) => setTokenEndpoint(e.target.value)}
                      disabled={isPending}
                    />
                  </div>
                  <div className="space-y-2">
                    <label htmlFor="issuerUrl" className="text-sm font-medium">
                      Issuer URL
                    </label>
                    <Input
                      id="issuerUrl"
                      placeholder="https://login.microsoftonline.com/.../v2.0"
                      value={issuerUrl}
                      onChange={(e) => setIssuerUrl(e.target.value)}
                      disabled={isPending}
                    />
                  </div>
                  <div className="space-y-2">
                    <label htmlFor="externalScopes" className="text-sm font-medium">
                      Scopes
                    </label>
                    <Input
                      id="externalScopes"
                      placeholder="api://.../codewhisperer:conversations offline_access"
                      value={scopes}
                      onChange={(e) => setScopes(e.target.value)}
                      disabled={isPending}
                    />
                  </div>
                  <div className="space-y-2">
                    <label htmlFor="profileArn" className="text-sm font-medium">
                      Profile ARN
                    </label>
                    <Input
                      id="profileArn"
                      placeholder="arn:aws:codewhisperer:us-east-1:...:profile/..."
                      value={profileArn}
                      onChange={(e) => setProfileArn(e.target.value)}
                      disabled={isPending}
                    />
                  </div>
                </>
              )}

              {/* 优先级 */}
              <div className="space-y-2">
                <label htmlFor="priority" className="text-sm font-medium">
                  {t('addcredentialdialog.field.priority.label')}
                </label>
                <NumberStepper
                  value={Number(priority) || 0}
                  onChange={(n) => setPriority(String(n))}
                  min={0}
                  disabled={isPending}
                  className="w-full"
                  aria-label={t('addcredentialdialog.field.priority.label')}
                />
                <p className="text-xs text-muted-foreground">
                  {t('addcredentialdialog.field.priority.help')}
                </p>
              </div>

              {/* Machine ID + 端点 均为 Kiro 专属(设备指纹/Kiro API 路由)。
                  自定义 API 代挂透传号无 refreshToken、直接打上游 base_url,不适用,不显示。 */}
              {authMethod !== 'custom_api' && (
              <>
              {/* Machine ID */}
              <div className="space-y-2">
                <label htmlFor="machineId" className="text-sm font-medium">
                  Machine ID
                </label>
                <Input
                  id="machineId"
                  placeholder={t('addcredentialdialog.field.machineId.placeholder')}
                  value={machineId}
                  onChange={(e) => setMachineId(e.target.value)}
                  disabled={isPending}
                />
                <p className="text-xs text-muted-foreground">
                  {t('addcredentialdialog.field.machineId.help')}
                </p>
              </div>

              {/* 端点 */}
              <div className="space-y-2">
                <label htmlFor="endpoint" className="text-sm font-medium">
                  {t('addcredentialdialog.field.endpoint.label')}
                </label>
                <Input
                  id="endpoint"
                  placeholder={t('addcredentialdialog.field.endpoint.placeholder')}
                  value={endpoint}
                  onChange={(e) => setEndpoint(e.target.value)}
                  disabled={isPending}
                />
                <p className="text-xs text-muted-foreground">
                  {t('addcredentialdialog.field.endpoint.help')}
                </p>
              </div>
              </>
              )}

              {/* 代理配置 */}
              <div className="space-y-2">
                <label className="text-sm font-medium">{t('addcredentialdialog.field.proxy.label')}</label>
                <div className="flex items-center gap-2">
                  <Input
                    id="proxyUrl"
                    className="flex-1"
                    placeholder={t('addcredentialdialog.field.proxyUrl.placeholder')}
                    value={proxyUrl}
                    onChange={(e) => setProxyUrl(e.target.value)}
                    disabled={isPending}
                  />
                  <ProxyTestButton proxyUrl={proxyUrl} proxyUsername={proxyUsername} proxyPassword={proxyPassword} />
                </div>
                <div className="grid grid-cols-2 gap-2">
                  <Input
                    id="proxyUsername"
                    placeholder={t('addcredentialdialog.field.proxyUsername.placeholder')}
                    value={proxyUsername}
                    onChange={(e) => setProxyUsername(e.target.value)}
                    disabled={isPending}
                  />
                  <Input
                    id="proxyPassword"
                    type="password"
                    placeholder={t('addcredentialdialog.field.proxyPassword.placeholder')}
                    value={proxyPassword}
                    onChange={(e) => setProxyPassword(e.target.value)}
                    disabled={isPending}
                  />
                </div>
                <p className="text-xs text-muted-foreground">
                  {t('addcredentialdialog.field.proxy.help')}
                </p>
              </div>
            </div>

            <DialogFooter>
              <Button
                type="button"
                variant="outline"
                onClick={() => onOpenChange(false)}
                disabled={isPending}
              >
                {t('addcredentialdialog.button.cancel')}
              </Button>
              <Button type="submit" disabled={isPending}>
                {isPending ? t('addcredentialdialog.button.adding') : t('addcredentialdialog.button.add')}
              </Button>
            </DialogFooter>
          </form>
          )}

          {tab === 'paste' && (
            <div className="flex flex-col min-h-0 flex-1">
              <div className="space-y-4 py-4 overflow-y-auto flex-1 pr-1">
                <div className="space-y-2">
                  <label className="text-sm font-medium">{t('addcredentialdialog.paste.label')}</label>
                  <textarea
                    value={pasteInput}
                    onChange={(e) => setPasteInput(e.target.value)}
                    disabled={importing}
                    placeholder={t('addcredentialdialog.paste.placeholder')}
                    className="flex min-h-[220px] w-full rounded-md border border-input bg-background px-3 py-2 text-sm ring-offset-background placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 disabled:cursor-not-allowed disabled:opacity-50 font-mono"
                  />
                  <p className="text-xs text-muted-foreground">
                    {t('addcredentialdialog.paste.help')}
                  </p>
                </div>

                {pasteResults.length > 0 && (
                  <>
                    <div className="flex gap-4 text-sm">
                      <span className="inline-flex items-center gap-1 text-green-600 dark:text-green-400">
                        <CheckCircle2 className="h-3.5 w-3.5" />
                        {t('addcredentialdialog.paste.countSuccess')}{pasteResults.filter(r => r.status === 'success').length}
                      </span>
                      <span className="inline-flex items-center gap-1 text-yellow-600 dark:text-yellow-400">
                        <AlertTriangle className="h-3.5 w-3.5" />
                        {t('addcredentialdialog.paste.countDuplicate')}{pasteResults.filter(r => r.status === 'duplicate').length}
                      </span>
                      <span className="inline-flex items-center gap-1 text-red-600 dark:text-red-400">
                        <XCircle className="h-3.5 w-3.5" />
                        {t('addcredentialdialog.paste.countFailed')}{pasteResults.filter(r => r.status === 'failed').length}
                      </span>
                    </div>
                    <div className="border rounded-md divide-y max-h-[220px] overflow-y-auto">
                      {pasteResults.map((r) => (
                        <div key={r.index} className="p-2.5 flex items-start gap-2.5">
                          {pasteIcon(r.status)}
                          <div className="flex-1 min-w-0">
                            <div className="flex items-center gap-2">
                              <span className="text-sm font-medium">
                                {r.email || (r.credentialId ? t('addcredentialdialog.paste.credentialItem', { credentialId: r.credentialId }) : t('addcredentialdialog.paste.rowItem', { index: r.index }))}
                              </span>
                            </div>
                            {r.error && (
                              <div className="text-xs text-red-600 dark:text-red-400 mt-0.5">
                                {r.error}
                              </div>
                            )}
                          </div>
                        </div>
                      ))}
                    </div>
                  </>
                )}
              </div>

              <DialogFooter>
                <Button
                  type="button"
                  variant="outline"
                  onClick={() => onOpenChange(false)}
                  disabled={importing}
                >
                  {importing ? t('addcredentialdialog.button.importing') : pasteResults.length > 0 ? t('addcredentialdialog.button.close') : t('addcredentialdialog.button.cancel')}
                </Button>
                <Button
                  type="button"
                  onClick={handlePasteImport}
                  disabled={importing || !pasteInput.trim()}
                >
                  {importing ? t('addcredentialdialog.button.importing') : t('addcredentialdialog.button.recognizeImport')}
                </Button>
              </DialogFooter>
            </div>
          )}
        </DialogContent>
      </Dialog>

      {/* 上号：复用现有 LoginDialog（网页 / IDC / 微软SSO 三种模式） */}
      <LoginDialog
        open={open && tab === 'login'}
        onOpenChange={(o) => {
          if (!o) {
            // 关闭上号弹窗时整体关掉「添加凭据」
            setTab('manual')
            onOpenChange(false)
          }
        }}
        onSuccess={() => queryClient.invalidateQueries({ queryKey: ['credentials'] })}
      />
    </>
  )
}

