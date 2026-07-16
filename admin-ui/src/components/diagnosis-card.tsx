import { useState } from 'react'
import { useTranslation } from 'react-i18next'
import { AlertCircle, ChevronDown, ChevronRight, RefreshCw, LogIn } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { cn } from '@/lib/utils'
import type { OnboardingDiagnosis } from '@/types/api'

/** 归因方 → 配色（用户填错=琥珀可改，账号问题=红需重新上号，上游/瞬时=蓝可重试，网关=紫需反馈）。标签走 i18n。 */
const FAULT_CLS: Record<OnboardingDiagnosis['fault'], string> = {
  user_input: 'border-amber-500/40 bg-amber-500/10 text-amber-400',
  account_state: 'border-red-500/40 bg-red-500/10 text-red-400',
  upstream: 'border-sky-500/40 bg-sky-500/10 text-sky-400',
  gateway: 'border-violet-500/40 bg-violet-500/10 text-violet-400',
  transient: 'border-sky-500/40 bg-sky-500/10 text-sky-400',
}

const FAULT_LABEL_KEYS: Record<OnboardingDiagnosis['fault'], string> = {
  user_input: 'diagnosiscard.fault.userInput',
  account_state: 'diagnosiscard.fault.accountState',
  upstream: 'diagnosiscard.fault.upstream',
  gateway: 'diagnosiscard.fault.gateway',
  transient: 'diagnosiscard.fault.transient',
}

interface DiagnosisCardProps {
  diagnosis: OnboardingDiagnosis
  /** 可重试时的回调（retriable 且提供才显示「重试」按钮）。 */
  onRetry?: () => void
  /** 账号需重新上号时的回调（REFRESH_TOKEN_INVALID / CLIENT_OR_TOKEN_MISMATCH / AUTH_EXPIRED 才显示）。 */
  onReLogin?: () => void
  className?: string
}

/** 上号诊断卡片：主行 summary + 归因徽标 + 有序引导步骤 + 折叠原始信息 + 可操作按钮。 */
export function DiagnosisCard({ diagnosis, onRetry, onReLogin, className }: DiagnosisCardProps) {
  const { t } = useTranslation()
  const [rawOpen, setRawOpen] = useState(false)
  const faultKey = FAULT_LABEL_KEYS[diagnosis.fault] ?? FAULT_LABEL_KEYS.gateway
  const faultCls = FAULT_CLS[diagnosis.fault] ?? FAULT_CLS.gateway
  const needReLogin =
    diagnosis.code === 'REFRESH_TOKEN_INVALID' ||
    diagnosis.code === 'CLIENT_OR_TOKEN_MISMATCH' ||
    diagnosis.code === 'AUTH_EXPIRED'

  return (
    <div className={cn('rounded-md border border-red-500/30 bg-red-500/5 p-3 text-sm', className)}>
      <div className="flex items-start gap-2">
        <AlertCircle className="mt-0.5 h-4 w-4 shrink-0 text-red-400" />
        <div className="min-w-0 flex-1 space-y-2">
          <div className="flex flex-wrap items-center gap-2">
            <span className={cn('rounded border px-1.5 py-0.5 text-xs', faultCls)}>{t(faultKey)}</span>
            <span className="rounded border border-border bg-secondary/40 px-1.5 py-0.5 font-mono text-[11px] text-muted-foreground">
              {diagnosis.code}
            </span>
          </div>
          <div className="font-medium text-foreground">{diagnosis.summary}</div>

          {diagnosis.guidance.length > 0 && (
            <ol className="list-decimal space-y-1 pl-5 text-xs text-muted-foreground">
              {diagnosis.guidance.map((g, i) => (
                <li key={i}>{g}</li>
              ))}
            </ol>
          )}

          <div className="flex flex-wrap items-center gap-2 pt-1">
            {diagnosis.retriable && onRetry && (
              <Button size="sm" variant="outline" className="h-7 px-2.5" onClick={onRetry}>
                <RefreshCw className="mr-1 h-3.5 w-3.5" />
                {t('diagnosiscard.button.retry')}
              </Button>
            )}
            {needReLogin && onReLogin && (
              <Button size="sm" variant="outline" className="h-7 px-2.5" onClick={onReLogin}>
                <LogIn className="mr-1 h-3.5 w-3.5" />
                {t('diagnosiscard.button.reLogin')}
              </Button>
            )}
          </div>

          {diagnosis.raw && (
            <div>
              <button
                type="button"
                className="flex items-center gap-1 text-xs text-muted-foreground hover:text-foreground"
                onClick={() => setRawOpen((v) => !v)}
              >
                {rawOpen ? <ChevronDown className="h-3.5 w-3.5" /> : <ChevronRight className="h-3.5 w-3.5" />}
                {t('diagnosiscard.rawInfo')}
              </button>
              {rawOpen && (
                <pre className="mt-1 max-h-40 overflow-auto rounded bg-secondary/40 p-2 font-mono text-[11px] text-muted-foreground whitespace-pre-wrap break-all">
                  {diagnosis.raw}
                </pre>
              )}
            </div>
          )}
        </div>
      </div>
    </div>
  )
}
