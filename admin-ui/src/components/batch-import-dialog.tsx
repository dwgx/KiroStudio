import { useState } from 'react'
import { useTranslation } from 'react-i18next'
import { toast } from 'sonner'
import { CheckCircle2, XCircle, AlertCircle, Loader2 } from 'lucide-react'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogFooter,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { useCredentials, useAddCredential, useDeleteCredential } from '@/hooks/use-credentials'
import { getCredentialBalance, setCredentialDisabled } from '@/api/credentials'
import { extractErrorMessage, sha256Hex } from '@/lib/utils'

interface BatchImportDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

interface CredentialInput {
  refreshToken?: string
  refresh_token?: string
  accessToken?: string
  access_token?: string
  clientId?: string
  client_id?: string
  clientSecret?: string
  client_secret?: string
  tokenEndpoint?: string
  token_endpoint?: string
  issuerUrl?: string
  issuer_url?: string
  scopes?: string
  profileArn?: string
  profile_arn?: string
  expiresAt?: string
  expires_at?: string
  expired?: string
  region?: string
  authRegion?: string
  auth_region?: string
  apiRegion?: string
  api_region?: string
  priority?: number
  machineId?: string
  machine_id?: string
  kiroApiKey?: string
  kiro_api_key?: string
  authMethod?: string
  auth_method?: string
  endpoint?: string
}

interface VerificationResult {
  index: number
  status: 'pending' | 'checking' | 'verifying' | 'verified' | 'duplicate' | 'failed'
  error?: string
  usage?: string
  email?: string
  credentialId?: number
  rollbackStatus?: 'success' | 'failed' | 'skipped'
  rollbackError?: string
}

const pickString = (...values: unknown[]): string | undefined => {
  for (const value of values) {
    if (typeof value === 'string' && value.trim()) {
      return value.trim()
    }
  }
  return undefined
}

const normalizeAuthMethod = (value: string | undefined): 'social' | 'idc' | 'external_idp' | 'api_key' | undefined => {
  if (!value) return undefined
  const normalized = value.trim().toLowerCase().replace(/-/g, '_')
  if (normalized === 'apikey') return 'api_key'
  if (normalized === 'externalidp' || normalized === 'external_idp' || normalized === 'azuread' || normalized === 'azure_ad') {
    return 'external_idp'
  }
  if (normalized === 'idc' || normalized === 'builder_id' || normalized === 'iam') return 'idc'
  if (normalized === 'social') return 'social'
  return undefined
}

const normalizeCredentialInput = (raw: CredentialInput): CredentialInput => {
  const method = normalizeAuthMethod(pickString(raw.authMethod, raw.auth_method))
  return {
    refreshToken: pickString(raw.refreshToken, raw.refresh_token),
    accessToken: pickString(raw.accessToken, raw.access_token),
    clientId: pickString(raw.clientId, raw.client_id),
    clientSecret: pickString(raw.clientSecret, raw.client_secret),
    tokenEndpoint: pickString(raw.tokenEndpoint, raw.token_endpoint),
    issuerUrl: pickString(raw.issuerUrl, raw.issuer_url),
    scopes: pickString(raw.scopes),
    profileArn: pickString(raw.profileArn, raw.profile_arn),
    expiresAt: pickString(raw.expiresAt, raw.expires_at, raw.expired),
    region: pickString(raw.region),
    authRegion: pickString(raw.authRegion, raw.auth_region, raw.region),
    apiRegion: pickString(raw.apiRegion, raw.api_region),
    priority: typeof raw.priority === 'number' ? raw.priority : undefined,
    machineId: pickString(raw.machineId, raw.machine_id),
    kiroApiKey: pickString(raw.kiroApiKey, raw.kiro_api_key),
    authMethod: method,
    endpoint: pickString(raw.endpoint),
  }
}



export function BatchImportDialog({ open, onOpenChange }: BatchImportDialogProps) {
  const { t } = useTranslation()
  const [jsonInput, setJsonInput] = useState('')
  const [importing, setImporting] = useState(false)
  const [progress, setProgress] = useState({ current: 0, total: 0 })
  const [currentProcessing, setCurrentProcessing] = useState<string>('')
  const [results, setResults] = useState<VerificationResult[]>([])

  const { data: existingCredentials } = useCredentials()
  const { mutateAsync: addCredential } = useAddCredential()
  const { mutateAsync: deleteCredential } = useDeleteCredential()

  const rollbackCredential = async (id: number): Promise<{ success: boolean; error?: string }> => {
    try {
      await setCredentialDisabled(id, true)
    } catch (error) {
      return {
        success: false,
        error: `${t('batchimportdialog.rollback.disableFailed')}${extractErrorMessage(error)}`,
      }
    }

    try {
      await deleteCredential(id)
      return { success: true }
    } catch (error) {
      return {
        success: false,
        error: `${t('batchimportdialog.rollback.deleteFailed')}${extractErrorMessage(error)}`,
      }
    }
  }

  const resetForm = () => {
    setJsonInput('')
    setProgress({ current: 0, total: 0 })
    setCurrentProcessing('')
    setResults([])
  }

  const handleBatchImport = async () => {
    // 先单独解析 JSON，给出精准的错误提示
    let credentials: CredentialInput[]
    try {
      const parsed = JSON.parse(jsonInput)
      credentials = (Array.isArray(parsed) ? parsed : [parsed]).map(normalizeCredentialInput)
    } catch (error) {
      toast.error(t('batchimportdialog.toast.jsonParseError') + extractErrorMessage(error))
      return
    }

    if (credentials.length === 0) {
      toast.error(t('batchimportdialog.toast.noCredentials'))
      return
    }

    try {
      setImporting(true)
      setProgress({ current: 0, total: credentials.length })

      // 2. 初始化结果
      const initialResults: VerificationResult[] = credentials.map((_, i) => ({
        index: i + 1,
        status: 'pending'
      }))
      setResults(initialResults)

      // 3. 检测重复：OAuth 与 API Key 分别使用对应的 hash 集合
      const existingOauthHashes = new Set(
        existingCredentials?.credentials
          .map(c => c.refreshTokenHash)
          .filter((hash): hash is string => Boolean(hash)) || []
      )
      const existingApiKeyHashes = new Set(
        existingCredentials?.credentials
          .map(c => c.apiKeyHash)
          .filter((hash): hash is string => Boolean(hash)) || []
      )

      let successCount = 0
      let duplicateCount = 0
      let failCount = 0
      let rollbackSuccessCount = 0
      let rollbackFailedCount = 0
      let rollbackSkippedCount = 0

      // 4. 导入并验活
      for (let i = 0; i < credentials.length; i++) {
        const cred = credentials[i]
        const isApiKeyCred = !!(cred.kiroApiKey?.trim()) || cred.authMethod === 'api_key'

        // 更新状态为检查中
        setCurrentProcessing(`${t('batchimportdialog.progress.processing')}${i + 1}/${credentials.length}`)
        setResults(prev => {
          const newResults = [...prev]
          newResults[i] = { ...newResults[i], status: 'checking' }
          return newResults
        })

        // 客户端去重：OAuth 基于 refreshToken hash，API Key 基于 kiroApiKey hash
        let credHash = ''
        if (isApiKeyCred) {
          const apiKey = cred.kiroApiKey?.trim() || ''
          if (!apiKey) {
            setResults(prev => {
              const newResults = [...prev]
              newResults[i] = {
                ...newResults[i],
                status: 'failed',
                error: t('batchimportdialog.error.missingApiKey'),
              }
              return newResults
            })
            failCount++
            setProgress({ current: i + 1, total: credentials.length })
            continue
          }
          credHash = await sha256Hex(apiKey)
          if (existingApiKeyHashes.has(credHash)) {
            duplicateCount++
            const existingCred = existingCredentials?.credentials.find(c => c.apiKeyHash === credHash)
            setResults(prev => {
              const newResults = [...prev]
              newResults[i] = {
                ...newResults[i],
                status: 'duplicate',
                error: t('batchimportdialog.error.credentialExists'),
                email: existingCred?.email || undefined
              }
              return newResults
            })
            setProgress({ current: i + 1, total: credentials.length })
            continue
          }
        } else {
          const token = cred.refreshToken?.trim() || ''
          if (!token) {
            setResults(prev => {
              const newResults = [...prev]
              newResults[i] = {
                ...newResults[i],
                status: 'failed',
                error: t('batchimportdialog.error.missingRefreshToken'),
              }
              return newResults
            })
            failCount++
            setProgress({ current: i + 1, total: credentials.length })
            continue
          }
          credHash = await sha256Hex(token)
          if (existingOauthHashes.has(credHash)) {
            duplicateCount++
            const existingCred = existingCredentials?.credentials.find(c => c.refreshTokenHash === credHash)
            setResults(prev => {
              const newResults = [...prev]
              newResults[i] = {
                ...newResults[i],
                status: 'duplicate',
                error: t('batchimportdialog.error.credentialExists'),
                email: existingCred?.email || undefined
              }
              return newResults
            })
            setProgress({ current: i + 1, total: credentials.length })
            continue
          }
        }

        // 更新状态为验活中
        setResults(prev => {
          const newResults = [...prev]
          newResults[i] = { ...newResults[i], status: 'verifying' }
          return newResults
        })

        let addedCredId: number | null = null

        try {
          // 添加凭据
          if (isApiKeyCred) {
            // API Key 凭据
            const addedCred = await addCredential({
              authMethod: 'api_key',
              kiroApiKey: cred.kiroApiKey?.trim(),
              priority: cred.priority || 0,
              authRegion: cred.authRegion?.trim() || cred.region?.trim() || undefined,
              apiRegion: cred.apiRegion?.trim() || undefined,
              machineId: cred.machineId?.trim() || undefined,
              endpoint: cred.endpoint?.trim() || undefined,
            })

            addedCredId = addedCred.credentialId

            // 延迟 1 秒
            await new Promise(resolve => setTimeout(resolve, 1000))

            // 验活
            const balance = await getCredentialBalance(addedCred.credentialId)

            successCount++
            existingApiKeyHashes.add(credHash)
            setCurrentProcessing(addedCred.email ? `${t('batchimportdialog.progress.verifySuccessEmail')}${addedCred.email}` : `${t('batchimportdialog.progress.verifySuccessCred')}${i + 1}`)
            setResults(prev => {
              const newResults = [...prev]
              newResults[i] = {
                ...newResults[i],
                status: 'verified',
                usage: `${balance.currentUsage}/${balance.usageLimit}`,
                email: addedCred.email || undefined,
                credentialId: addedCred.credentialId
              }
              return newResults
            })
            setProgress({ current: i + 1, total: credentials.length })
            continue
          }

          // OAuth 凭据
          const token = cred.refreshToken!.trim()
          const clientId = cred.clientId?.trim() || undefined
          const clientSecret = cred.clientSecret?.trim() || undefined
          const tokenEndpoint = cred.tokenEndpoint?.trim() || undefined
          const explicitMethod = normalizeAuthMethod(cred.authMethod)
          const authMethod = explicitMethod === 'external_idp' || tokenEndpoint
            ? 'external_idp'
            : clientId && clientSecret
              ? 'idc'
              : explicitMethod === 'idc'
                ? 'idc'
                : 'social'

          // idc 模式下必须同时提供 clientId 和 clientSecret
          if (authMethod === 'idc' && (!clientId || !clientSecret)) {
            throw new Error(t('batchimportdialog.error.idcNeedsClientIdSecret'))
          }

          if (authMethod === 'external_idp' && (!clientId || !tokenEndpoint)) {
            throw new Error(t('batchimportdialog.error.externalIdpNeedsClientIdEndpoint'))
          }

          const addedCred = await addCredential({
            refreshToken: token,
            authMethod,
            accessToken: cred.accessToken?.trim() || undefined,
            profileArn: cred.profileArn?.trim() || undefined,
            expiresAt: cred.expiresAt?.trim() || undefined,
            authRegion: cred.authRegion?.trim() || cred.region?.trim() || undefined,
            apiRegion: cred.apiRegion?.trim() || undefined,
            clientId,
            clientSecret,
            tokenEndpoint: authMethod === 'external_idp' ? tokenEndpoint : undefined,
            issuerUrl: authMethod === 'external_idp' ? cred.issuerUrl?.trim() || undefined : undefined,
            scopes: authMethod === 'external_idp' ? cred.scopes?.trim() || undefined : undefined,
            priority: cred.priority || 0,
            machineId: cred.machineId?.trim() || undefined,
            endpoint: cred.endpoint?.trim() || undefined,
          })

          addedCredId = addedCred.credentialId

          // 延迟 1 秒
          await new Promise(resolve => setTimeout(resolve, 1000))

          // 验活
          const balance = await getCredentialBalance(addedCred.credentialId)

          // 验活成功
          successCount++
          existingOauthHashes.add(credHash)
          setCurrentProcessing(addedCred.email ? `${t('batchimportdialog.progress.verifySuccessEmail')}${addedCred.email}` : `${t('batchimportdialog.progress.verifySuccessCred')}${i + 1}`)
          setResults(prev => {
            const newResults = [...prev]
            newResults[i] = {
              ...newResults[i],
              status: 'verified',
              usage: `${balance.currentUsage}/${balance.usageLimit}`,
              email: addedCred.email || undefined,
              credentialId: addedCred.credentialId
            }
            return newResults
          })
        } catch (error) {
          // 验活失败，尝试回滚（先禁用再删除）
          let rollbackStatus: VerificationResult['rollbackStatus'] = 'skipped'
          let rollbackError: string | undefined

          if (addedCredId) {
            const rollbackResult = await rollbackCredential(addedCredId)
            if (rollbackResult.success) {
              rollbackStatus = 'success'
              rollbackSuccessCount++
            } else {
              rollbackStatus = 'failed'
              rollbackFailedCount++
              rollbackError = rollbackResult.error
            }
          } else {
            rollbackSkippedCount++
          }

          failCount++
          setResults(prev => {
            const newResults = [...prev]
            newResults[i] = {
              ...newResults[i],
              status: 'failed',
              error: extractErrorMessage(error),
              email: undefined,
              rollbackStatus,
              rollbackError,
            }
            return newResults
          })
        }

        setProgress({ current: i + 1, total: credentials.length })
      }

      // 显示结果
      if (failCount === 0 && duplicateCount === 0) {
        toast.success(t('batchimportdialog.toast.importSuccess', { count: successCount }))
      } else {
        const failureSummary = failCount > 0
          ? t('batchimportdialog.toast.failureSummary', {
              failCount,
              rollbackSuccess: rollbackSuccessCount,
              rollbackFailed: rollbackFailedCount,
              rollbackSkipped: rollbackSkippedCount,
            })
          : ''
        toast.info(t('batchimportdialog.toast.verifyComplete', {
          successCount,
          duplicateCount,
          failureSummary,
        }))

        if (rollbackFailedCount > 0) {
          toast.warning(t('batchimportdialog.toast.rollbackIncomplete', { count: rollbackFailedCount }))
        }
      }
    } catch (error) {
      toast.error(t('batchimportdialog.toast.importFailed') + extractErrorMessage(error))
    } finally {
      setImporting(false)
    }
  }

  const getStatusIcon = (status: VerificationResult['status']) => {
    switch (status) {
      case 'pending':
        return <div className="w-5 h-5 rounded-full border-2 border-gray-300" />
      case 'checking':
      case 'verifying':
        return <Loader2 className="w-5 h-5 animate-spin text-blue-500" />
      case 'verified':
        return <CheckCircle2 className="w-5 h-5 text-green-500" />
      case 'duplicate':
        return <AlertCircle className="w-5 h-5 text-yellow-500" />
      case 'failed':
        return <XCircle className="w-5 h-5 text-red-500" />
    }
  }

  const getStatusText = (result: VerificationResult) => {
    switch (result.status) {
      case 'pending':
        return t('batchimportdialog.status.pending')
      case 'checking':
        return t('batchimportdialog.status.checking')
      case 'verifying':
        return t('batchimportdialog.status.verifying')
      case 'verified':
        return t('batchimportdialog.status.verified')
      case 'duplicate':
        return t('batchimportdialog.status.duplicate')
      case 'failed':
        if (result.rollbackStatus === 'success') return t('batchimportdialog.status.failedRemoved')
        if (result.rollbackStatus === 'failed') return t('batchimportdialog.status.failedNotRemoved')
        return t('batchimportdialog.status.failedNotCreated')
    }
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(newOpen) => {
        // 关闭时清空表单（但不在导入过程中清空）
        if (!newOpen && !importing) {
          resetForm()
        }
        onOpenChange(newOpen)
      }}
    >
      <DialogContent className="sm:max-w-2xl max-h-[80vh] flex flex-col">
        <DialogHeader>
          <DialogTitle>{t('batchimportdialog.title')}</DialogTitle>
        </DialogHeader>

        <div className="flex-1 overflow-y-auto space-y-4 py-4">
          <div className="space-y-2">
            <label className="text-sm font-medium">
              {t('batchimportdialog.label.jsonCredentials')}
            </label>
            <textarea
              placeholder={t('batchimportdialog.placeholder.jsonInput')}
              value={jsonInput}
              onChange={(e) => setJsonInput(e.target.value)}
              disabled={importing}
              className="flex min-h-[200px] w-full rounded-md border border-input bg-background px-3 py-2 text-sm ring-offset-background placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 disabled:cursor-not-allowed disabled:opacity-50 font-mono"
            />
            <p className="text-xs text-muted-foreground">
              {t('batchimportdialog.hint.autoVerify')}
            </p>
          </div>

          {(importing || results.length > 0) && (
            <>
              {/* 进度条 */}
              <div className="space-y-2">
                <div className="flex justify-between text-sm">
                  <span>{importing ? t('batchimportdialog.progress.inProgress') : t('batchimportdialog.progress.done')}</span>
                  <span>{progress.current} / {progress.total}</span>
                </div>
                <div className="w-full bg-secondary rounded-full h-2">
                  <div
                    className="bg-primary h-2 rounded-full transition-all"
                    style={{ width: `${(progress.current / progress.total) * 100}%` }}
                  />
                </div>
                {importing && currentProcessing && (
                  <div className="text-xs text-muted-foreground">
                    {currentProcessing}
                  </div>
                )}
              </div>

              {/* 统计 */}
              <div className="flex gap-4 text-sm">
                <span className="text-green-600 dark:text-green-400">
                  {t('batchimportdialog.stat.success')}{results.filter(r => r.status === 'verified').length}
                </span>
                <span className="text-yellow-600 dark:text-yellow-400">
                  {t('batchimportdialog.stat.duplicate')}{results.filter(r => r.status === 'duplicate').length}
                </span>
                <span className="text-red-600 dark:text-red-400">
                  {t('batchimportdialog.stat.failed')}{results.filter(r => r.status === 'failed').length}
                </span>
              </div>

              {/* 结果列表 */}
              <div className="border rounded-md divide-y max-h-[300px] overflow-y-auto">
                {results.map((result) => (
                  <div key={result.index} className="p-3">
                    <div className="flex items-start gap-3">
                      {getStatusIcon(result.status)}
                      <div className="flex-1 min-w-0">
                        <div className="flex items-center gap-2">
                          <span className="text-sm font-medium">
                            {result.email || t('batchimportdialog.list.credentialFallback', { index: result.index })}
                          </span>
                          <span className="text-xs text-muted-foreground">
                            {getStatusText(result)}
                          </span>
                        </div>
                        {result.usage && (
                          <div className="text-xs text-muted-foreground mt-1">
                            {t('batchimportdialog.list.usage')}{result.usage}
                          </div>
                        )}
                        {result.error && (
                          <div className="text-xs text-red-600 dark:text-red-400 mt-1">
                            {result.error}
                          </div>
                        )}
                        {result.rollbackError && (
                          <div className="text-xs text-red-600 dark:text-red-400 mt-1">
                            {t('batchimportdialog.list.rollbackFailed')}{result.rollbackError}
                          </div>
                        )}
                      </div>
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
            onClick={() => {
              onOpenChange(false)
              resetForm()
            }}
            disabled={importing}
          >
            {importing ? t('batchimportdialog.button.verifying') : results.length > 0 ? t('batchimportdialog.button.close') : t('batchimportdialog.button.cancel')}
          </Button>
          {results.length === 0 && (
            <Button
              type="button"
              onClick={handleBatchImport}
              disabled={importing || !jsonInput.trim()}
            >
              {t('batchimportdialog.button.startImport')}
            </Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
