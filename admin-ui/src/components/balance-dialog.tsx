import { useTranslation } from 'react-i18next'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { Progress } from '@/components/ui/progress'
import { Skeleton } from '@/components/ui/skeleton'
import { useCredentialBalance } from '@/hooks/use-credentials'
import { parseError } from '@/lib/utils'

interface BalanceDialogProps {
  credentialId: number | null
  open: boolean
  onOpenChange: (open: boolean) => void
}

function localeForLang(lang: string): string {
  if (lang.startsWith('ja')) return 'ja-JP'
  if (lang.startsWith('en')) return 'en-US'
  return 'zh-CN'
}

export function BalanceDialog({ credentialId, open, onOpenChange }: BalanceDialogProps) {
  const { t, i18n } = useTranslation()
  const { data: balance, isLoading, error } = useCredentialBalance(credentialId)
  const locale = localeForLang(i18n.language)

  const formatDate = (timestamp: number | null) => {
    if (!timestamp) return t('balancedialog.unknown')
    return new Date(timestamp * 1000).toLocaleString(locale)
  }

  const formatNumber = (num: number) => {
    return num.toLocaleString(locale, { minimumFractionDigits: 2, maximumFractionDigits: 2 })
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>
            {t('balancedialog.title', { credentialId })}
          </DialogTitle>
        </DialogHeader>

        {isLoading && (
          <div className="space-y-4 py-2">
            {/* 骨架屏：贴合余额内容形状(订阅标题 + 使用进度条 + 详情栅格)，替代蓝色转圈圈 */}
            <div className="flex justify-center">
              <Skeleton className="h-5 w-32" />
            </div>
            <div className="space-y-2">
              <div className="flex justify-between">
                <Skeleton className="h-3 w-20" />
                <Skeleton className="h-3 w-20" />
              </div>
              <Skeleton className="h-2 w-full rounded-full" />
              <div className="flex justify-center">
                <Skeleton className="h-3 w-16" />
              </div>
            </div>
            <div className="grid grid-cols-2 gap-4 pt-4">
              <Skeleton className="h-8 w-full" />
              <Skeleton className="h-8 w-full" />
            </div>
          </div>
        )}

        {error && (() => {
          const parsed = parseError(error)
          return (
            <div className="py-6 space-y-3">
              <div className="flex items-center justify-center gap-2 text-red-500">
                <svg className="h-5 w-5" viewBox="0 0 20 20" fill="currentColor">
                  <path fillRule="evenodd" d="M10 18a8 8 0 100-16 8 8 0 000 16zM8.707 7.293a1 1 0 00-1.414 1.414L8.586 10l-1.293 1.293a1 1 0 101.414 1.414L10 11.414l1.293 1.293a1 1 0 001.414-1.414L11.414 10l1.293-1.293a1 1 0 00-1.414-1.414L10 8.586 8.707 7.293z" clipRule="evenodd" />
                </svg>
                <span className="font-medium">{parsed.title}</span>
              </div>
              {parsed.detail && (
                <div className="text-sm text-muted-foreground text-center px-4">
                  {parsed.detail}
                </div>
              )}
            </div>
          )
        })()}

        {balance && (
          <div className="space-y-4">
            {/* 订阅类型 */}
            <div className="text-center">
              <span className="text-lg font-semibold">
                {balance.subscriptionTitle || t('balancedialog.unknownSubscription')}
              </span>
            </div>

            {/* 使用进度 */}
            <div className="space-y-2">
              <div className="flex justify-between text-sm">
                <span>{t('balancedialog.used', { amount: formatNumber(balance.currentUsage) })}</span>
                <span>{t('balancedialog.limit', { amount: formatNumber(balance.usageLimit) })}</span>
              </div>
              <Progress value={balance.usagePercentage} />
              <div className="text-center text-sm text-muted-foreground">
                {t('balancedialog.usedPercent', { percent: balance.usagePercentage.toFixed(1) })}
              </div>
            </div>

            {/* 详细信息 */}
            <div className="grid grid-cols-2 gap-4 pt-4 border-t text-sm">
              <div>
                <span className="text-muted-foreground">{t('balancedialog.remaining')}</span>
                <span className="font-medium text-green-600">
                  ${formatNumber(balance.remaining)}
                </span>
              </div>
              <div>
                <span className="text-muted-foreground">{t('balancedialog.nextReset')}</span>
                <span className="font-medium">
                  {formatDate(balance.nextResetAt)}
                </span>
              </div>
            </div>
          </div>
        )}
      </DialogContent>
    </Dialog>
  )
}
