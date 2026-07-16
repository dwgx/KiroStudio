import { useTranslation } from 'react-i18next'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { CheckCircle2, XCircle, Loader2, MoreHorizontal, Lightbulb } from 'lucide-react'

export interface VerifyResult {
  id: number
  status: 'pending' | 'verifying' | 'success' | 'failed'
  usage?: string
  error?: string
}

interface BatchVerifyDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  verifying: boolean
  progress: { current: number; total: number }
  results: Map<number, VerifyResult>
  onCancel: () => void
}

export function BatchVerifyDialog({
  open,
  onOpenChange,
  verifying,
  progress,
  results,
  onCancel,
}: BatchVerifyDialogProps) {
  const { t } = useTranslation()
  const resultsArray = Array.from(results.values())
  const successCount = resultsArray.filter(r => r.status === 'success').length
  const failedCount = resultsArray.filter(r => r.status === 'failed').length

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>{t('batchverifydialog.title')}</DialogTitle>
        </DialogHeader>

        <div className="space-y-4 py-4">
          {/* 进度显示 */}
          {verifying && (
            <div className="space-y-2">
              <div className="flex justify-between text-sm">
                <span>{t('batchverifydialog.progress')}</span>
                <span>{progress.current} / {progress.total}</span>
              </div>
              <div className="w-full bg-secondary rounded-full h-2">
                <div
                  className="bg-primary h-2 rounded-full transition-all"
                  style={{ width: `${(progress.current / progress.total) * 100}%` }}
                />
              </div>
            </div>
          )}

          {/* 统计信息 */}
          {results.size > 0 && (
            <div className="flex justify-between text-sm font-medium">
              <span>{t('batchverifydialog.results')}</span>
              <span>
                {t('batchverifydialog.summary', { success: successCount, failed: failedCount })}
              </span>
            </div>
          )}

          {/* 结果列表 */}
          {results.size > 0 && (
            <div className="max-h-[400px] overflow-y-auto border rounded-md p-2 space-y-1">
              {resultsArray.map((result) => (
                <div
                  key={result.id}
                  className={`text-sm p-2 rounded ${
                    result.status === 'success'
                      ? 'bg-green-50 text-green-700 dark:bg-green-950 dark:text-green-300'
                      : result.status === 'failed'
                      ? 'bg-red-50 text-red-700 dark:bg-red-950 dark:text-red-300'
                      : result.status === 'verifying'
                      ? 'bg-blue-50 text-blue-700 dark:bg-blue-950 dark:text-blue-300'
                      : 'bg-gray-50 text-gray-700 dark:bg-gray-950 dark:text-gray-300'
                  }`}
                >
                  <div className="flex items-start justify-between gap-2">
                    <div className="flex items-center gap-2">
                      <span className="font-medium">{t('batchverifydialog.credential', { id: result.id })}</span>
                      {result.status === 'success' && result.usage && (
                        <Badge variant="secondary" className="text-xs">
                          {result.usage}
                        </Badge>
                      )}
                    </div>
                    <span className="inline-flex items-center">
                      {result.status === 'success' && <CheckCircle2 className="h-4 w-4 text-green-600 dark:text-green-400" />}
                      {result.status === 'failed' && <XCircle className="h-4 w-4 text-red-600 dark:text-red-400" />}
                      {result.status === 'verifying' && <Loader2 className="h-4 w-4 animate-spin text-blue-600 dark:text-blue-400" />}
                      {result.status === 'pending' && <MoreHorizontal className="h-4 w-4 text-muted-foreground" />}
                    </span>
                  </div>
                  {result.error && (
                    <div className="text-xs mt-1 opacity-90">
                      {t('batchverifydialog.error', { error: result.error })}
                    </div>
                  )}
                </div>
              ))}
            </div>
          )}

          {/* 提示信息 */}
          {verifying && (
            <p className="flex items-start gap-1.5 text-xs text-muted-foreground">
              <Lightbulb className="mt-0.5 h-3.5 w-3.5 shrink-0" />
              <span>{t('batchverifydialog.hint')}</span>
            </p>
          )}
        </div>

        <div className="flex justify-end gap-2">
          {verifying ? (
            <>
              <Button
                type="button"
                variant="outline"
                onClick={() => onOpenChange(false)}
              >
                {t('batchverifydialog.button.background')}
              </Button>
              <Button
                type="button"
                variant="destructive"
                onClick={onCancel}
              >
                {t('batchverifydialog.button.cancel')}
              </Button>
            </>
          ) : (
            <Button
              type="button"
              onClick={() => onOpenChange(false)}
            >
              {t('batchverifydialog.button.close')}
            </Button>
          )}
        </div>
      </DialogContent>
    </Dialog>
  )
}
