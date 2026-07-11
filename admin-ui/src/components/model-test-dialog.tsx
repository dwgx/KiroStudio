import { useState } from 'react'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { CheckCircle2, XCircle, Loader2, MoreHorizontal, FlaskConical, Lightbulb } from 'lucide-react'
import { PROBE_MODEL_CATALOG, type ProbedModel, type ProbeModelsResponse } from '@/api/credentials'

export interface ModelTestResult {
  id: number
  status: 'pending' | 'testing' | 'done' | 'failed'
  models?: ProbedModel[]
  totalCredits?: number
  error?: string
}

interface ModelTestDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  /** 本次要测的凭据 id（勾选自 dashboard） */
  credentialIds: number[]
  /** 逐号探测回调：由 dashboard 提供（调用 probeAvailableModels）。 */
  onProbe: (id: number, models: string[]) => Promise<ProbeModelsResponse>
}

/** 单模型状态 → 短标签 */
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

export function ModelTestDialog({ open, onOpenChange, credentialIds, onProbe }: ModelTestDialogProps) {
  // 勾选要测的模型（默认全选常用主力，国产便宜的也默认选上验证机制）
  const [selectedModels, setSelectedModels] = useState<Set<string>>(
    () => new Set(['qwen3-coder-next', 'claude-haiku-4.5', 'claude-sonnet-4.6', 'claude-opus-4.6', 'claude-opus-4.8']),
  )
  const [testing, setTesting] = useState(false)
  const [progress, setProgress] = useState({ current: 0, total: 0 })
  const [results, setResults] = useState<Map<number, ModelTestResult>>(new Map())
  const [grandTotal, setGrandTotal] = useState(0)
  const cancelRef = useState(() => ({ v: false }))[0]

  const toggleModel = (id: string) => {
    setSelectedModels((prev) => {
      const n = new Set(prev)
      if (n.has(id)) n.delete(id)
      else n.add(id)
      return n
    })
  }

  const runTest = async () => {
    if (selectedModels.size === 0 || credentialIds.length === 0) return
    const models = PROBE_MODEL_CATALOG.filter((m) => selectedModels.has(m.id)).map((m) => m.id)
    cancelRef.v = false
    setTesting(true)
    setGrandTotal(0)
    setProgress({ current: 0, total: credentialIds.length })
    const init = new Map<number, ModelTestResult>()
    credentialIds.forEach((id) => init.set(id, { id, status: 'pending' }))
    setResults(new Map(init))

    let grand = 0
    for (let i = 0; i < credentialIds.length; i++) {
      if (cancelRef.v) break
      const id = credentialIds[i]
      setResults((prev) => new Map(prev).set(id, { id, status: 'testing' }))
      try {
        const res = await onProbe(id, models)
        grand += res.totalCredits
        setResults((prev) => new Map(prev).set(id, { id, status: 'done', models: res.models, totalCredits: res.totalCredits }))
        setGrandTotal(grand)
      } catch (e) {
        setResults((prev) => new Map(prev).set(id, { id, status: 'failed', error: (e as Error).message }))
      }
      setProgress({ current: i + 1, total: credentialIds.length })
    }
    setTesting(false)
  }

  const arr = Array.from(results.values())
  const doneCount = arr.filter((r) => r.status === 'done').length
  const failedCount = arr.filter((r) => r.status === 'failed').length

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>测试可用模型（{credentialIds.length} 个凭据）</DialogTitle>
        </DialogHeader>

        <div className="space-y-4 py-3">
          {/* 模型勾选：可自己选要测哪些 */}
          <div>
            <div className="mb-1.5 text-xs text-muted-foreground">选择要测的模型（倍率=计费系数，越低越便宜）</div>
            <div className="flex flex-wrap gap-1.5">
              {PROBE_MODEL_CATALOG.map((m) => {
                const on = selectedModels.has(m.id)
                return (
                  <button
                    key={m.id}
                    type="button"
                    disabled={testing}
                    onClick={() => toggleModel(m.id)}
                    className={`inline-flex items-center gap-1 rounded px-2 py-1 text-[11px] font-medium border transition-colors ${
                      on
                        ? 'bg-primary/15 text-primary border-primary/40'
                        : 'bg-white/5 text-muted-foreground border-white/10 hover:border-white/25'
                    } disabled:opacity-50`}
                    title={`${m.id} · ${m.mult}`}
                  >
                    {on ? <CheckCircle2 className="h-3 w-3" /> : <span className="h-3 w-3 rounded-full border border-current opacity-40" />}
                    {m.id} <span className="opacity-60">{m.mult}</span>
                  </button>
                )
              })}
            </div>
          </div>

          <div className="flex items-center gap-2">
            <Button size="sm" onClick={runTest} disabled={testing || selectedModels.size === 0}>
              <FlaskConical className={`h-4 w-4 mr-1.5 ${testing ? 'animate-pulse' : ''}`} />
              {testing ? '测试中...' : results.size > 0 ? '再测一次（用当前勾选）' : '开始测试'}
            </Button>
            {testing && (
              <Button size="sm" variant="destructive" onClick={() => { cancelRef.v = true }}>取消</Button>
            )}
            <span className="text-xs text-muted-foreground">已选 {selectedModels.size} 个模型</span>
          </div>

          {testing && (
            <div className="w-full bg-secondary rounded-full h-2">
              <div className="bg-primary h-2 rounded-full transition-all" style={{ width: `${progress.total ? (progress.current / progress.total) * 100 : 0}%` }} />
            </div>
          )}

          {results.size > 0 && (
            <div className="flex justify-between text-sm font-medium">
              <span>完成 {doneCount} / 失败 {failedCount}（共 {progress.total}）</span>
              <span className="text-amber-300">本轮总花费 {grandTotal.toFixed(4)} credits</span>
            </div>
          )}

          {results.size > 0 && (
            <div className="max-h-[360px] overflow-y-auto border rounded-md p-2 space-y-2">
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
                    <div className="flex flex-wrap gap-1.5 mt-2">{r.models.map((m) => modelChip(m))}</div>
                  )}
                  {r.error && <div className="text-xs mt-1 text-red-300">错误: {r.error}</div>}
                </div>
              ))}
            </div>
          )}

          <p className="flex items-start gap-1.5 text-xs text-muted-foreground">
            <Lightbulb className="mt-0.5 h-3.5 w-3.5 shrink-0" />
            <span>每个勾选模型发一个<b>无提示词真实请求</b>并消耗真实积分（能用的才计费）。结果保留在此页，可改勾选后再测一次。</span>
          </p>
        </div>

        <div className="flex justify-end">
          <Button type="button" variant="outline" onClick={() => onOpenChange(false)}>返回</Button>
        </div>
      </DialogContent>
    </Dialog>
  )
}


