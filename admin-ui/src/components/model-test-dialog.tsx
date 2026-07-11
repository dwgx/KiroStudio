import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { CheckCircle2, XCircle, Loader2, MoreHorizontal, HelpCircle, Lightbulb } from 'lucide-react'
import type { ProbedModel } from '@/api/credentials'

export interface ModelTestResult {
  id: number
  status: 'pending' | 'testing' | 'done' | 'failed'
  /** 逐模型明细（done 时有值） */
  models?: ProbedModel[]
  /** 本号探测总花费 credits */
  totalCredits?: number
  error?: string
}

interface ModelTestDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  testing: boolean
  progress: { current: number; total: number }
  results: Map<number, ModelTestResult>
  /** 所有已完成号的总花费合计 */
  grandTotalCredits: number
  onCancel: () => void
}

/** 单模型状态 → 短标签样式 */
function modelChip(m: ProbedModel) {
  const cls =
    m.status === 'supported'
      ? 'bg-emerald-500/10 text-emerald-300 border border-emerald-500/30'
      : m.status === 'unsupported'
        ? 'bg-white/5 text-muted-foreground border border-white/10 line-through'
        : 'bg-amber-500/10 text-amber-300 border border-amber-500/30'
  const tip =
    m.status === 'supported'
      ? `可用 · 本次 ${m.credits.toFixed(4)} credits`
      : m.status === 'unsupported'
        ? '不支持（订阅不含 / INVALID_MODEL_ID）'
        : '探测时上游异常，无法判定（可重试）'
  return (
    <span
      key={m.model}
      className={`inline-flex items-center gap-1 rounded px-1.5 py-0.5 text-[11px] font-medium ${cls}`}
      title={tip}
    >
      {m.model}
      {m.status === 'unknown' && ' ?'}
    </span>
  )
}

export function ModelTestDialog({
  open,
  onOpenChange,
  testing,
  progress,
  results,
  grandTotalCredits,
  onCancel,
}: ModelTestDialogProps) {
  const arr = Array.from(results.values())
  const doneCount = arr.filter((r) => r.status === 'done').length
  const failedCount = arr.filter((r) => r.status === 'failed').length

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>测试可用模型</DialogTitle>
        </DialogHeader>

        <div className="space-y-4 py-4">
          {testing && (
            <div className="space-y-2">
              <div className="flex justify-between text-sm">
                <span>测试进度</span>
                <span>{progress.current} / {progress.total}</span>
              </div>
              <div className="w-full bg-secondary rounded-full h-2">
                <div
                  className="bg-primary h-2 rounded-full transition-all"
                  style={{ width: `${progress.total ? (progress.current / progress.total) * 100 : 0}%` }}
                />
              </div>
            </div>
          )}

          {results.size > 0 && (
            <div className="flex justify-between text-sm font-medium">
              <span>完成: {doneCount} / 失败: {failedCount}</span>
              <span className="text-amber-300">本轮总花费 {grandTotalCredits.toFixed(4)} credits</span>
            </div>
          )}

          {results.size > 0 && (
            <div className="max-h-[420px] overflow-y-auto border rounded-md p-2 space-y-2">
              {arr.map((r) => (
                <div key={r.id} className="text-sm p-2 rounded bg-black/20 border border-white/5">
                  <div className="flex items-start justify-between gap-2">
                    <div className="flex items-center gap-2">
                      <span className="font-medium">凭据 #{r.id}</span>
                      {r.status === 'done' && r.totalCredits != null && (
                        <Badge variant="secondary" className="text-xs">花费 {r.totalCredits.toFixed(4)} credits</Badge>
                      )}
                    </div>
                    <span className="inline-flex items-center">
                      {r.status === 'done' && <CheckCircle2 className="h-4 w-4 text-emerald-400" />}
                      {r.status === 'failed' && <XCircle className="h-4 w-4 text-red-400" />}
                      {r.status === 'testing' && <Loader2 className="h-4 w-4 animate-spin text-sky-400" />}
                      {r.status === 'pending' && <MoreHorizontal className="h-4 w-4 text-muted-foreground" />}
                    </span>
                  </div>
                  {r.status === 'done' && r.models && (
                    <div className="flex flex-wrap gap-1.5 mt-2">
                      {r.models.map((m) => modelChip(m))}
                    </div>
                  )}
                  {r.error && <div className="text-xs mt-1 text-red-300">错误: {r.error}</div>}
                </div>
              ))}
            </div>
          )}

          <p className="flex items-start gap-1.5 text-xs text-muted-foreground">
            <Lightbulb className="mt-0.5 h-3.5 w-3.5 shrink-0" />
            <span>
              测试对每个候选模型发一个<b>无提示词的真实请求</b>并消耗真实积分（能用的模型才计费），
              逐个间隔以防风控。<HelpCircle className="inline h-3 w-3" /> 判定依赖上游对无权限模型返回
              INVALID_MODEL_ID 的行为，个别情况可能偏乐观。可关闭窗口后台继续。
            </span>
          </p>
        </div>

        <div className="flex justify-end gap-2">
          {testing ? (
            <>
              <Button type="button" variant="outline" onClick={() => onOpenChange(false)}>后台运行</Button>
              <Button type="button" variant="destructive" onClick={onCancel}>取消测试</Button>
            </>
          ) : (
            <Button type="button" onClick={() => onOpenChange(false)}>关闭</Button>
          )}
        </div>
      </DialogContent>
    </Dialog>
  )
}

