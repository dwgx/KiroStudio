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
import { startSocialLogin, pollSocialLogin } from '@/api/credentials'
import { CheckCircle2 } from 'lucide-react'
import { copyToClipboard, extractErrorMessage } from '@/lib/utils'
import type { StartSocialLoginResponse } from '@/types/api'

interface SocialLoginDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  /** 上号成功后回调（用于刷新凭据列表） */
  onSuccess?: () => void
}

type Step = 'form' | 'waiting' | 'done'

const POLL_INTERVAL_MS = 2000

export function SocialLoginDialog({ open, onOpenChange, onSuccess }: SocialLoginDialogProps) {
  const { t } = useTranslation()
  const [step, setStep] = useState<Step>('form')
  const [priority, setPriority] = useState('0')
  const [proxyUrl, setProxyUrl] = useState('')
  const [isStarting, setIsStarting] = useState(false)
  const [session, setSession] = useState<StartSocialLoginResponse | null>(null)
  const [resultEmail, setResultEmail] = useState<string | null>(null)
  const pollTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null)

  // 清理轮询定时器
  const stopPolling = () => {
    if (pollTimerRef.current) {
      clearTimeout(pollTimerRef.current)
      pollTimerRef.current = null
    }
  }

  // 关闭或卸载时停止轮询
  useEffect(() => {
    if (!open) {
      stopPolling()
      // 延迟重置，避免关闭动画期间闪烁
      const t = setTimeout(() => {
        setStep('form')
        setSession(null)
        setResultEmail(null)
        setIsStarting(false)
      }, 200)
      return () => clearTimeout(t)
    }
  }, [open])

  useEffect(() => () => stopPolling(), [])

  const poll = (sessionId: string) => {
    pollTimerRef.current = setTimeout(async () => {
      try {
        const result = await pollSocialLogin(sessionId)
        if (result.status === 'pending') {
          poll(sessionId) // 继续轮询
        } else if (result.status === 'done') {
          stopPolling()
          setResultEmail(result.email ?? null)
          setStep('done')
          toast.success(t('sociallogindialog.toast.loginSuccess', { credentialId: result.credentialId }))
          onSuccess?.()
        } else {
          stopPolling()
          toast.error(result.message || t('sociallogindialog.toast.loginFailed'))
          setStep('form')
        }
      } catch (err) {
        // 轮询单次失败不致命，继续重试
        poll(sessionId)
        console.warn('轮询登录状态失败，重试中', err)
      }
    }, POLL_INTERVAL_MS)
  }

  const handleStart = async () => {
    setIsStarting(true)
    try {
      const resp = await startSocialLogin({
        priority: Number(priority) || 0,
        proxyUrl: proxyUrl.trim() || undefined,
      })
      setSession(resp)
      setStep('waiting')
      // 不自动打开网页：用户在 waiting 步骤手动点「打开登录页」
      poll(resp.sessionId)
    } catch (err) {
      toast.error(extractErrorMessage(err))
    } finally {
      setIsStarting(false)
    }
  }

  const handleOpenLogin = () => {
    if (!session) return
    window.open(session.portalUrl, '_blank', 'noopener,noreferrer')
  }

  const handleCopy = async () => {
    if (!session) return
    const ok = await copyToClipboard(session.portalUrl)
    if (ok) {
      toast.success(t('sociallogindialog.toast.linkCopied'))
    } else {
      toast.error(t('sociallogindialog.toast.copyFailedSelectManually'))
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-[480px]">
        <DialogHeader>
          <DialogTitle>{t('sociallogindialog.title')}</DialogTitle>
        </DialogHeader>

        {step === 'form' && (
          <div className="space-y-4 py-2">
            <p className="text-sm text-muted-foreground">
              {t('sociallogindialog.form.intro')}
            </p>
            <div className="space-y-2">
              <label className="text-sm font-medium" htmlFor="priority">
                {t('sociallogindialog.form.priorityLabel')}
              </label>
              <NumberStepper
                value={Number(priority) || 0}
                onChange={(n) => setPriority(String(n))}
                min={0}
                disabled={isStarting}
                className="w-full"
                aria-label={t('sociallogindialog.form.priorityAriaLabel')}
              />
              <p className="text-xs text-muted-foreground">{t('sociallogindialog.form.priorityHelp')}</p>
            </div>
            <div className="space-y-2">
              <label className="text-sm font-medium" htmlFor="proxyUrl">
                {t('sociallogindialog.form.proxyLabel')}
              </label>
              <div className="flex items-center gap-2">
                <Input
                  id="proxyUrl"
                  className="flex-1"
                  value={proxyUrl}
                  onChange={(e) => setProxyUrl(e.target.value)}
                  placeholder={t('sociallogindialog.form.proxyPlaceholder')}
                  disabled={isStarting}
                />
                <ProxyTestButton proxyUrl={proxyUrl} />
              </div>
            </div>
          </div>
        )}

        {step === 'waiting' && session && (
          <div className="space-y-4 py-2">
            <p className="text-sm text-muted-foreground">
              {t('sociallogindialog.waiting.intro')}
            </p>
            <div className="flex items-center gap-2">
              <Input readOnly value={session.portalUrl} className="text-xs" onFocus={(e) => e.currentTarget.select()} />
              <Button type="button" variant="outline" onClick={handleCopy}>
                {t('sociallogindialog.waiting.copy')}
              </Button>
            </div>
            <Button type="button" className="w-full" onClick={handleOpenLogin}>
              {t('sociallogindialog.waiting.openLoginPage')}
            </Button>
            <div className="flex items-center gap-2 text-sm text-muted-foreground">
              <span className="inline-block h-2 w-2 animate-pulse rounded-full bg-primary" />
              {t('sociallogindialog.waiting.waitingBrowser')}
            </div>
          </div>
        )}

        {step === 'done' && (
          <div className="space-y-3 py-4 text-center">
            <CheckCircle2 className="mx-auto h-12 w-12 text-green-600 dark:text-green-400" />
            <p className="text-sm font-medium">{t('sociallogindialog.done.title')}</p>
            {resultEmail && (
              <p className="text-xs text-muted-foreground">{resultEmail}</p>
            )}
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
                {t('sociallogindialog.footer.cancel')}
              </Button>
              <Button type="button" onClick={handleStart} disabled={isStarting}>
                {isStarting ? t('sociallogindialog.footer.starting') : t('sociallogindialog.footer.startLogin')}
              </Button>
            </>
          )}
          {step === 'waiting' && (
            <Button type="button" variant="outline" onClick={() => onOpenChange(false)}>
              {t('sociallogindialog.footer.cancel')}
            </Button>
          )}
          {step === 'done' && (
            <Button type="button" onClick={() => onOpenChange(false)}>
              {t('sociallogindialog.footer.done')}
            </Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
