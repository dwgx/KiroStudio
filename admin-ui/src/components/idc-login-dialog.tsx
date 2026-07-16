import { useState, useEffect, useRef } from 'react'
import { useTranslation } from 'react-i18next'
import { toast } from 'sonner'
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
import { ProxyTestButton } from '@/components/proxy-test-button'
import { startIdcLogin, pollIdcLogin } from '@/api/credentials'
import { CheckCircle2 } from 'lucide-react'
import { copyToClipboard, extractErrorMessage, extractDiagnosis } from '@/lib/utils'
import { DiagnosisCard } from '@/components/diagnosis-card'
import type { OnboardingDiagnosis } from '@/types/api'

interface IdcLoginDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  onSuccess?: () => void
}

type Step = 'form' | 'waiting' | 'done'

interface IdcSession {
  sessionId: string
  verificationUri: string
  verificationUriComplete?: string
  userCode: string
  expiresIn: number
}

const POLL_INTERVAL_MS = 5000

export function IdcLoginDialog({ open, onOpenChange, onSuccess }: IdcLoginDialogProps) {
  const { t } = useTranslation()
  const [step, setStep] = useState<Step>('form')
  const [startUrl, setStartUrl] = useState('')
  const [region, setRegion] = useState('us-east-1')
  const [priority, setPriority] = useState('0')
  const [proxyUrl, setProxyUrl] = useState('')
  const [isStarting, setIsStarting] = useState(false)
  const [session, setSession] = useState<IdcSession | null>(null)
  const [countdown, setCountdown] = useState(0)
  const [diagnosis, setDiagnosis] = useState<OnboardingDiagnosis | null>(null)
  const pollTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null)
  const countdownRef = useRef<ReturnType<typeof setInterval> | null>(null)

  const stopPolling = () => {
    if (pollTimerRef.current) {
      clearTimeout(pollTimerRef.current)
      pollTimerRef.current = null
    }
    if (countdownRef.current) {
      clearInterval(countdownRef.current)
      countdownRef.current = null
    }
  }

  useEffect(() => {
    if (!open) {
      stopPolling()
      const t = setTimeout(() => {
        setStep('form')
        setSession(null)
        setCountdown(0)
        setIsStarting(false)
      }, 200)
      return () => clearTimeout(t)
    }
  }, [open])

  useEffect(() => () => stopPolling(), [])

  const poll = (sessionId: string) => {
    pollTimerRef.current = setTimeout(async () => {
      try {
        const result = await pollIdcLogin(sessionId)
        if (result.status === 'pending') {
          poll(sessionId)
        } else if (result.status === 'done') {
          stopPolling()
          setStep('done')
          // 与 login-dialog 一致：字典为前缀文案，凭据 ID 追加拼接（既有 key 无占位符）
          toast.success(`${t('idclogindialog.toast.loginSuccess')}${result.credentialId}`)
          onSuccess?.()
        } else if (result.status === 'expired') {
          stopPolling()
          toast.error(t('idclogindialog.toast.authTimeoutRetry'))
          setStep('form')
        } else {
          stopPolling()
          toast.error(result.message || t('idclogindialog.toast.loginFailed'))
          setStep('form')
        }
      } catch (err) {
        poll(sessionId)
        console.warn('IDC 轮询失败，重试中', err)
      }
    }, POLL_INTERVAL_MS)
  }

  const handleStart = async () => {
    if (!startUrl.trim()) {
      toast.error(t('idclogindialog.toast.startUrlRequired'))
      return
    }
    setIsStarting(true)
    setDiagnosis(null)
    try {
      const resp = await startIdcLogin({
        startUrl: startUrl.trim(),
        region: region.trim() || 'us-east-1',
        priority: Number(priority) || 0,
        proxyUrl: proxyUrl.trim() || undefined,
      })
      setSession(resp)
      setStep('waiting')
      setCountdown(resp.expiresIn)
      // 启动倒计时
      countdownRef.current = setInterval(() => {
        setCountdown(prev => {
          if (prev <= 1) {
            stopPolling()
            setStep('form')
            toast.error(t('idclogindialog.toast.authTimeout'))
            return 0
          }
          return prev - 1
        })
      }, 1000)
      poll(resp.sessionId)
    } catch (err) {
      // 结构化诊断优先(如 REGION_MISMATCH:填错 region 给引导),否则退回 toast。
      const diag = extractDiagnosis(err)
      if (diag) {
        setDiagnosis(diag)
      } else {
        toast.error(extractErrorMessage(err))
      }
    } finally {
      setIsStarting(false)
    }
  }

  const handleOpenVerification = () => {
    if (!session) return
    const url = session.verificationUriComplete || session.verificationUri
    window.open(url, '_blank', 'noopener,noreferrer')
  }

  const handleCopyCode = async () => {
    if (!session) return
    const ok = await copyToClipboard(session.userCode)
    if (ok) toast.success(t('idclogindialog.toast.userCodeCopied'))
    else toast.error(t('idclogindialog.toast.copyFailed'))
  }

  const formatCountdown = (secs: number) => {
    const m = Math.floor(secs / 60)
    const s = secs % 60
    return `${m}:${s.toString().padStart(2, '0')}`
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-[480px]">
        <DialogHeader>
          <DialogTitle>{t('idclogindialog.title')}</DialogTitle>
        </DialogHeader>

        {step === 'form' && (
          <div className="space-y-4 py-2">
            <p className="text-sm text-muted-foreground">
              {t('idclogindialog.form.intro')}
            </p>
            <div className="space-y-2">
              <label className="text-sm font-medium" htmlFor="startUrl">
                {t('idclogindialog.form.startUrlLabel')}
              </label>
              <Input
                id="startUrl"
                value={startUrl}
                onChange={(e) => setStartUrl(e.target.value)}
                placeholder="https://d-xxxxxxxxxx.awsapps.com/start"
                disabled={isStarting}
              />
              <p className="text-xs text-muted-foreground">{t('idclogindialog.form.startUrlHelp')}</p>
            </div>
            <div className="space-y-2">
              <label className="text-sm font-medium" htmlFor="idcRegion">
                {t('idclogindialog.form.regionLabel')}
              </label>
              <Input
                id="idcRegion"
                value={region}
                onChange={(e) => setRegion(e.target.value)}
                placeholder="us-east-1"
                disabled={isStarting}
              />
              <p className="text-xs text-muted-foreground">
                {t('idclogindialog.form.regionHelp')}
              </p>
            </div>
            <div className="space-y-2">
              <label className="text-sm font-medium" htmlFor="idcPriority">
                {t('idclogindialog.form.priorityLabel')}
              </label>
              <NumberStepper
                value={Number(priority) || 0}
                onChange={(n) => setPriority(String(n))}
                min={0}
                disabled={isStarting}
                className="w-full"
                aria-label={t('idclogindialog.form.priorityAriaLabel')}
              />
              <p className="text-xs text-muted-foreground">{t('idclogindialog.form.priorityHelp')}</p>
            </div>
            <div className="space-y-2">
              <label className="text-sm font-medium" htmlFor="idcProxy">
                {t('idclogindialog.form.proxyLabel')}
              </label>
              <div className="flex items-center gap-2">
                <Input
                  id="idcProxy"
                  className="flex-1"
                  value={proxyUrl}
                  onChange={(e) => setProxyUrl(e.target.value)}
                  placeholder={t('idclogindialog.form.proxyPlaceholder')}
                  disabled={isStarting}
                />
                <ProxyTestButton proxyUrl={proxyUrl} />
              </div>
            </div>
            {diagnosis && <DiagnosisCard diagnosis={diagnosis} />}
          </div>
        )}

        {step === 'waiting' && session && (
          <div className="space-y-4 py-2">
            <p className="text-sm text-muted-foreground">
              {t('idclogindialog.waiting.intro')}
            </p>
            <div className="rounded-lg border bg-muted/50 p-4 text-center space-y-2">
              <p className="text-xs text-muted-foreground">{t('idclogindialog.waiting.codeLabel')}</p>
              <p className="text-2xl font-mono font-bold tracking-widest">{session.userCode}</p>
              <Button type="button" variant="ghost" size="sm" onClick={handleCopyCode}>
                {t('idclogindialog.waiting.copyCode')}
              </Button>
            </div>
            <Button type="button" className="w-full" onClick={handleOpenVerification}>
              {t('idclogindialog.waiting.openAuthPage')}
            </Button>
            <div className="flex items-center justify-between text-sm text-muted-foreground">
              <div className="flex items-center gap-2">
                <span className="inline-block h-2 w-2 animate-pulse rounded-full bg-primary" />
                {t('idclogindialog.waiting.waitingAuth')}
              </div>
              <span className="tabular-nums">{formatCountdown(countdown)}</span>
            </div>
          </div>
        )}

        {step === 'done' && (
          <div className="space-y-3 py-4 text-center">
            <CheckCircle2 className="mx-auto h-12 w-12 text-green-600 dark:text-green-400" />
            <p className="text-sm font-medium">{t('idclogindialog.done.title')}</p>
            <p className="text-xs text-muted-foreground">{t('idclogindialog.done.desc')}</p>
          </div>
        )}

        <DialogFooter>
          {step === 'form' && (
            <>
              <Button
                type="button"
                variant="outline"
                onClick={() => onOpenChange(false)}
                disabled={isStarting}
              >
                {t('idclogindialog.footer.cancel')}
              </Button>
              <Button type="button" onClick={handleStart} disabled={isStarting}>
                {isStarting ? t('idclogindialog.footer.starting') : t('idclogindialog.footer.startLogin')}
              </Button>
            </>
          )}
          {step === 'waiting' && (
            <Button type="button" variant="outline" onClick={() => onOpenChange(false)}>
              {t('idclogindialog.footer.cancel')}
            </Button>
          )}
          {step === 'done' && (
            <Button type="button" onClick={() => onOpenChange(false)}>
              {t('idclogindialog.footer.done')}
            </Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
