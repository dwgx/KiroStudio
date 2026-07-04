import { useState } from 'react'
import { toast } from 'sonner'
import { RefreshCw, ChevronUp, ChevronDown, Wallet, Trash2, Loader2, Download, FileJson, KeyRound, ClipboardCopy, ShieldAlert, Gauge } from 'lucide-react'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { Switch } from '@/components/ui/switch'
import { Checkbox } from '@/components/ui/checkbox'
import { NumberStepper } from '@/components/ui/number-stepper'
import { Tooltip, TooltipTrigger, TooltipContent, TooltipProvider } from '@/components/ui/tooltip'
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
import { exportCredential } from '@/api/credentials'
import { authShortLabel, disabledReasonLabel, subscriptionLabel } from '@/lib/i18n-labels'
import {
  useSetDisabled,
  useSetPriority,
  useResetFailure,
  useDeleteCredential,
  useForceRefreshToken,
} from '@/hooks/use-credentials'

interface CredentialCardProps {
  credential: CredentialStatusItem
  onViewBalance: (id: number) => void
  selected: boolean
  onToggleSelect: () => void
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

export function CredentialCard({
  credential,
  onViewBalance,
  selected,
  onToggleSelect,
  balance,
  loadingBalance,
}: CredentialCardProps) {
  const [editingPriority, setEditingPriority] = useState(false)
  const [priorityValue, setPriorityValue] = useState(credential.priority)
  const [showDeleteDialog, setShowDeleteDialog] = useState(false)
  const [showExportDialog, setShowExportDialog] = useState(false)
  const [exporting, setExporting] = useState(false)

  const setDisabled = useSetDisabled()
  const setPriority = useSetPriority()
  const resetFailure = useResetFailure()
  const deleteCredential = useDeleteCredential()
  const forceRefresh = useForceRefreshToken()

  const handleToggleDisabled = () => {
    setDisabled.mutate(
      { id: credential.id, disabled: !credential.disabled },
      {
        onSuccess: (res) => {
          toast.success(res.message)
        },
        onError: (err) => {
          toast.error('操作失败: ' + (err as Error).message)
        },
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
        onSuccess: (res) => {
          toast.success(res.message)
          setEditingPriority(false)
        },
        onError: (err) => {
          toast.error('操作失败: ' + (err as Error).message)
        },
      }
    )
  }

  const handleReset = () => {
    resetFailure.mutate(credential.id, {
      onSuccess: (res) => {
        toast.success(res.message)
      },
      onError: (err) => {
        toast.error('操作失败: ' + (err as Error).message)
      },
    })
  }

  const handleForceRefresh = () => {
    forceRefresh.mutate(credential.id, {
      onSuccess: (res) => {
        toast.success(res.message)
      },
      onError: (err) => {
        toast.error('刷新失败: ' + (err as Error).message)
      },
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
      },
      onError: (err) => {
        toast.error('删除失败: ' + (err as Error).message)
      },
    })
  }

  // 触发浏览器下载
  const triggerDownload = (content: string, filename: string, mime: string) => {
    const blob = new Blob([content], { type: mime })
    const url = URL.createObjectURL(blob)
    const a = document.createElement('a')
    a.href = url
    a.download = filename
    document.body.appendChild(a)
    a.click()
    document.body.removeChild(a)
    URL.revokeObjectURL(url)
  }

  // 从 export 端点取原始凭据对象，字段随认证方式不同
  const fetchExport = async (): Promise<Record<string, unknown>> => {
    setExporting(true)
    try {
      return await exportCredential(credential.id)
    } finally {
      setExporting(false)
    }
  }

  // (a) 下载完整凭据 JSON（可重新导入）
  const handleExportJson = async () => {
    try {
      const raw = await fetchExport()
      triggerDownload(
        JSON.stringify(raw, null, 2),
        `credential-${credential.id}.json`,
        'application/json'
      )
      toast.success('已导出凭据 JSON')
      setShowExportDialog(false)
    } catch (err) {
      toast.error('导出失败: ' + (err as Error).message)
    }
  }

  // (b) 仅下载 refreshToken 纯文本
  const handleExportRefreshToken = async () => {
    try {
      const raw = await fetchExport()
      const token = raw.refreshToken
      if (typeof token !== 'string' || !token) {
        toast.error('该凭据不包含 refreshToken（可能是 API Key 凭据）')
        return
      }
      triggerDownload(
        token,
        `credential-${credential.id}-refreshtoken.txt`,
        'text/plain'
      )
      toast.success('已导出 refreshToken')
      setShowExportDialog(false)
    } catch (err) {
      toast.error('导出失败: ' + (err as Error).message)
    }
  }

  // (c) 复制完整 JSON 到剪贴板
  const handleCopyJson = async () => {
    try {
      const raw = await fetchExport()
      const ok = await copyToClipboard(JSON.stringify(raw, null, 2))
      if (ok) {
        toast.success('已复制凭据 JSON 到剪贴板')
        setShowExportDialog(false)
      } else {
        toast.error('复制失败，请重试')
      }
    } catch (err) {
      toast.error('导出失败: ' + (err as Error).message)
    }
  }

  // 点击整卡切换选中；命中内部交互控件（按钮/输入/开关/复选框/链接/对话框）时不触发
  const INTERACTIVE_SELECTOR =
    'button, input, textarea, select, a, [role="switch"], [role="checkbox"], [role="dialog"], [contenteditable="true"]'

  const handleCardClick = (e: React.MouseEvent<HTMLDivElement>) => {
    if ((e.target as HTMLElement).closest(INTERACTIVE_SELECTOR)) return
    onToggleSelect()
  }

  const handleCardKeyDown = (e: React.KeyboardEvent<HTMLDivElement>) => {
    // 仅当焦点在卡片本身（非内部控件）时响应，避免抢占控件的键盘操作
    if (e.target !== e.currentTarget) return
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault()
      onToggleSelect()
    }
  }

  return (
    <>
      <Card
        role="button"
        aria-selected={selected}
        aria-pressed={selected}
        tabIndex={0}
        onClick={handleCardClick}
        onKeyDown={handleCardKeyDown}
        className={cn(
          'cursor-pointer transition-all duration-250 ease-out-expo hover:-translate-y-0.5 hover:border-border-hover hover:shadow-lg hover:shadow-black/20 focus:outline-none focus-visible:ring-2 focus-visible:ring-ring motion-reduce:transform-none',
          // 选中态：primary ring + 轻微高亮底色
          selected && 'ring-2 ring-primary bg-primary/[0.04]',
          // 当前活跃：用不同色（emerald）的 ring 与选中区分；未选中时显示
          credential.isCurrent && !selected && 'ring-2 ring-emerald-500/60'
        )}
      >
        <CardHeader className="pb-2">
          <div className="flex items-center justify-between gap-2">
            <div className="flex min-w-0 items-center gap-2">
              <Checkbox
                checked={selected}
                onCheckedChange={onToggleSelect}
              />
              <CardTitle className="text-lg flex min-w-0 flex-wrap items-center gap-2">
                <span className="min-w-0 max-w-full truncate" title={credential.email || undefined}>
                  {credential.email || `凭据 #${credential.id}`}
                </span>
                {credential.isCurrent && (
                  <Badge variant="success">当前</Badge>
                )}
                {credential.disabled && (
                  <Badge variant="destructive">已禁用</Badge>
                )}
                {credential.disabled && credential.disabledReason && (
                  <Badge variant="outline">{disabledReasonLabel(credential.disabledReason)}</Badge>
                )}
                {credential.authMethod && (
                  <Badge variant="secondary">
                    {credential.authMethod === 'api_key' ? 'API Key' :
                     authShortLabel(credential.authMethod)}
                  </Badge>
                )}
                {credential.endpoint && (
                  <Badge variant="outline">{credential.endpoint}</Badge>
                )}
              </CardTitle>
            </div>
            <div className="flex shrink-0 items-center gap-2">
              <span className="text-sm text-muted-foreground">启用</span>
              <Switch
                checked={!credential.disabled}
                onCheckedChange={handleToggleDisabled}
                disabled={setDisabled.isPending}
              />
            </div>
          </div>
        </CardHeader>
        <CardContent className="space-y-4">
          {/* 信息网格 */}
          <div className="grid grid-cols-2 gap-4 text-sm">
            <div>
              <span className="text-muted-foreground">优先级：</span>
              {editingPriority ? (
                <div className="inline-flex items-center gap-1 ml-1 align-middle">
                  <NumberStepper
                    value={priorityValue}
                    onChange={setPriorityValue}
                    min={0}
                    className="w-20"
                    aria-label="优先级"
                  />
                  <Button
                    size="sm"
                    variant="ghost"
                    className="h-7 w-7 p-0"
                    onClick={handlePriorityChange}
                    disabled={setPriority.isPending}
                  >
                    ✓
                  </Button>
                  <Button
                    size="sm"
                    variant="ghost"
                    className="h-7 w-7 p-0"
                    onClick={() => {
                      setEditingPriority(false)
                      setPriorityValue(credential.priority)
                    }}
                  >
                    ✕
                  </Button>
                </div>
              ) : (
                <span
                  className="font-medium cursor-pointer hover:underline ml-1"
                  onClick={(e) => {
                    e.stopPropagation()
                    setEditingPriority(true)
                  }}
                >
                  {credential.priority}
                  <span className="text-xs text-muted-foreground ml-1">(点击编辑)</span>
                </span>
              )}
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
              <span className="text-muted-foreground">订阅等级：</span>
              <span className="font-medium">
                {loadingBalance ? (
                  <Loader2 className="inline w-3 h-3 animate-spin" />
                ) : subscriptionLabel(balance?.subscriptionTitle)}
              </span>
            </div>
            <div>
              <span className="text-muted-foreground">成功次数：</span>
              <span className="font-medium">{credential.successCount}</span>
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
            <div className="col-span-2">
              <span className="text-muted-foreground">剩余用量：</span>
              {loadingBalance ? (
                <span className="text-sm ml-1">
                  <Loader2 className="inline w-3 h-3 animate-spin" /> 加载中...
                </span>
              ) : balance ? (
                <span className="font-medium ml-1">
                  {balance.remaining.toFixed(2)} / {balance.usageLimit.toFixed(2)}
                  <span className="text-xs text-muted-foreground ml-1">
                    ({(100 - balance.usagePercentage).toFixed(1)}% 剩余)
                  </span>
                </span>
              ) : (
                <span className="text-sm text-muted-foreground ml-1">未知</span>
              )}
            </div>
            {/* Overage（超额）开关：KIRO Pro+ 开启后可突破 base 额度（付费能力）。
                后端 BE-overage 批次落地前 —— 只读展示 overageEnabled 状态，开关 disabled，
                tooltip 说明“后端开关开发中”，避免硬塞难看/无效的按钮。 */}
            <div className="col-span-2 flex items-center justify-between rounded-md border border-dashed border-border bg-secondary/30 px-3 py-2">
              <div className="flex min-w-0 items-center gap-2">
                <Gauge className="h-4 w-4 shrink-0 text-muted-foreground" />
                <div className="min-w-0">
                  <div className="text-sm font-medium">超额（Overage）</div>
                  <div className="text-xs text-muted-foreground">
                    {credential.overageEnabled == null
                      ? '状态未知'
                      : credential.overageEnabled
                      ? '已开启 · 可突破 base 额度'
                      : '未开启'}
                  </div>
                </div>
              </div>
              <div className="flex shrink-0 items-center gap-2">
                {credential.overageEnabled != null && (
                  <Badge variant={credential.overageEnabled ? 'success' : 'secondary'}>
                    {credential.overageEnabled ? '开' : '关'}
                  </Badge>
                )}
                <TooltipProvider delayDuration={80}>
                  <Tooltip>
                    <TooltipTrigger asChild>
                      {/* span 包裹：disabled 的 Switch 不派发事件，Radix tooltip 需可触发元素 */}
                      <span className="inline-flex" onClick={(e) => e.stopPropagation()}>
                        <Switch
                          checked={!!credential.overageEnabled}
                          disabled
                          aria-label="超额开关（后端开发中）"
                        />
                      </span>
                    </TooltipTrigger>
                    <TooltipContent>后端开关开发中，敬请期待</TooltipContent>
                  </Tooltip>
                </TooltipProvider>
              </div>
            </div>
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

          {/* 操作按钮 */}
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
            <Button
              size="sm"
              variant="outline"
              onClick={() => {
                const newPriority = Math.max(0, credential.priority - 1)
                setPriority.mutate(
                  { id: credential.id, priority: newPriority },
                  {
                    onSuccess: (res) => toast.success(res.message),
                    onError: (err) => toast.error('操作失败: ' + (err as Error).message),
                  }
                )
              }}
              disabled={setPriority.isPending || credential.priority === 0}
            >
              <ChevronUp className="h-4 w-4 mr-1" />
              提高优先级
            </Button>
            <Button
              size="sm"
              variant="outline"
              onClick={() => {
                const newPriority = credential.priority + 1
                setPriority.mutate(
                  { id: credential.id, priority: newPriority },
                  {
                    onSuccess: (res) => toast.success(res.message),
                    onError: (err) => toast.error('操作失败: ' + (err as Error).message),
                  }
                )
              }}
              disabled={setPriority.isPending}
            >
              <ChevronDown className="h-4 w-4 mr-1" />
              降低优先级
            </Button>
            <Button
              size="sm"
              variant="default"
              onClick={() => onViewBalance(credential.id)}
            >
              <Wallet className="h-4 w-4 mr-1" />
              查看余额
            </Button>
            <Button
              size="sm"
              variant="outline"
              onClick={() => setShowExportDialog(true)}
            >
              <Download className="h-4 w-4 mr-1" />
              下载令牌
            </Button>
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
        </CardContent>
      </Card>

      {/* 删除确认对话框 */}
      <Dialog open={showDeleteDialog} onOpenChange={setShowDeleteDialog}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>确认删除凭据</DialogTitle>
            <DialogDescription>
              您确定要删除凭据 #{credential.id} 吗？此操作无法撤销。
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
              确认删除
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* 下载令牌对话框 */}
      <Dialog open={showExportDialog} onOpenChange={setShowExportDialog}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>下载令牌</DialogTitle>
            <DialogDescription>
              选择导出格式（凭据 #{credential.id}
              {credential.email ? ` · ${credential.email}` : ''}）。
            </DialogDescription>
          </DialogHeader>

          <div className="flex flex-col gap-2">
            <Button
              variant="outline"
              className="h-auto justify-start gap-3 py-3"
              onClick={handleExportJson}
              disabled={exporting}
            >
              <FileJson className="h-4 w-4 shrink-0" />
              <span className="flex flex-col items-start text-left">
                <span className="text-sm font-medium">KiroStudio 凭据 JSON</span>
                <span className="text-xs text-muted-foreground">完整对象，可重新导入</span>
              </span>
            </Button>
            <Button
              variant="outline"
              className="h-auto justify-start gap-3 py-3"
              onClick={handleExportRefreshToken}
              disabled={exporting}
            >
              <KeyRound className="h-4 w-4 shrink-0" />
              <span className="flex flex-col items-start text-left">
                <span className="text-sm font-medium">仅 refreshToken</span>
                <span className="text-xs text-muted-foreground">纯文本（API Key 凭据不含此字段）</span>
              </span>
            </Button>
            <Button
              variant="outline"
              className="h-auto justify-start gap-3 py-3"
              onClick={handleCopyJson}
              disabled={exporting}
            >
              <ClipboardCopy className="h-4 w-4 shrink-0" />
              <span className="flex flex-col items-start text-left">
                <span className="text-sm font-medium">复制到剪贴板</span>
                <span className="text-xs text-muted-foreground">完整 JSON</span>
              </span>
            </Button>
          </div>

          <div className="flex items-start gap-2 rounded-md border border-amber-500/20 bg-amber-500/10 px-3 py-2 text-xs text-amber-400">
            <ShieldAlert className="mt-0.5 h-3.5 w-3.5 shrink-0" />
            <span>令牌是敏感凭据，请妥善保管，切勿泄露或提交到代码仓库。</span>
          </div>

          {exporting && (
            <div className="flex items-center gap-2 text-sm text-muted-foreground">
              <Loader2 className="h-3.5 w-3.5 animate-spin" />
              正在导出...
            </div>
          )}
        </DialogContent>
      </Dialog>
    </>
  )
}
