import { useState } from 'react'
import { toast } from 'sonner'
import { Settings, RefreshCw, Wallet, Trash2, Loader2, Download, FileJson, KeyRound, ClipboardCopy, ShieldAlert, Gauge, Check } from 'lucide-react'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { Switch } from '@/components/ui/switch'
import { Checkbox } from '@/components/ui/checkbox'
import { Skeleton } from '@/components/ui/skeleton'
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
  useCachedBalances,
} from '@/hooks/use-credentials'

interface CredentialCardProps {
  credential: CredentialStatusItem
  onViewBalance: (id: number) => void
  selected: boolean
  onToggleSelect: () => void
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
  const [showDeleteDialog, setShowDeleteDialog] = useState(false)
  const [showExportDialog, setShowExportDialog] = useState(false)
  const [exporting, setExporting] = useState(false)

  const setDisabled = useSetDisabled()
  const setPriority = useSetPriority()
  const resetFailure = useResetFailure()
  const deleteCredential = useDeleteCredential()
  const forceRefresh = useForceRefreshToken()

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
    // 缓存快照带 cachedAt（按需拉取的 balance prop 没有），据此标注新鲜度。
    const cachedAt = balance ? null : cached?.cachedAt ?? null

    return (
      <div className="space-y-1.5">
        <div className="flex items-center justify-between">
          <span className="text-xs text-muted-foreground">剩余用量</span>
          <span className="text-xs text-muted-foreground">
            {cachedAt ? `截至 ${formatCachedAt(cachedAt)}` : '实时'}
            {' · '}
            {remainingPct.toFixed(1)}% 剩余
          </span>
        </div>
        <div className="relative h-6 w-full overflow-hidden rounded-md bg-secondary">
          <div
            className={cn('h-full transition-all duration-500 ease-out-expo', barColor)}
            style={{ width: `${remainingPct}%` }}
          />
          {/* 条上叠加数字金额（居中，混合模式保证深浅背景都可读） */}
          <div className="pointer-events-none absolute inset-0 flex items-center justify-center">
            <span className="text-xs font-semibold tabular-nums mix-blend-difference text-white">
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
        role="button"
        aria-selected={selected}
        aria-pressed={selected}
        tabIndex={0}
        onClick={handleCardClick}
        onKeyDown={handleCardKeyDown}
        className={cn(
          'cursor-pointer transition-all duration-250 ease-out-expo hover:-translate-y-0.5 hover:border-border-hover hover:shadow-lg hover:shadow-black/20 focus:outline-none focus-visible:ring-2 focus-visible:ring-ring motion-reduce:transform-none',
          selected && 'ring-2 ring-primary bg-primary/[0.04]',
          credential.isCurrent && !selected && 'ring-2 ring-emerald-500/60'
        )}
      >
        <CardHeader className="pb-2">
          <div className="flex items-center justify-between gap-2">
            <div className="flex min-w-0 items-center gap-2">
              <Checkbox checked={selected} onCheckedChange={onToggleSelect} />
              <CardTitle className="text-lg flex min-w-0 flex-wrap items-center gap-2">
                <span className="min-w-0 max-w-full truncate" title={credential.email || undefined}>
                  {credential.email || `凭据 #${credential.id}`}
                </span>
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
            {/* Overage（超额）开关：只读展示，后端 BE-overage 落地前 disabled */}
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
            <Button size="sm" variant="default" onClick={() => onViewBalance(credential.id)}>
              <Wallet className="h-4 w-4 mr-1" />
              查看余额
            </Button>
            <Button size="sm" variant="outline" onClick={() => setShowExportDialog(true)}>
              <Download className="h-4 w-4 mr-1" />
              下载令牌
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
            {/* 优先级：输入框（数字步进器）替代旧“点击编辑” */}
            <div className="flex items-center justify-between gap-4">
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

