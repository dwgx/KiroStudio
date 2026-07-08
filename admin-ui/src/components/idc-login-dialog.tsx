import { useState, useEffect, useRef } from 'react'
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
import { startIdcLogin, pollIdcLogin } from '@/api/credentials'
import { CheckCircle2 } from 'lucide-react'
import { copyToClipboard, extractErrorMessage } from '@/lib/utils'

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
  const [step, setStep] = useState<Step>('form')
  const [startUrl, setStartUrl] = useState('')
  const [region, setRegion] = useState('us-east-1')
  const [priority, setPriority] = useState('100')
  const [proxyUrl, setProxyUrl] = useState('')
  const [isStarting, setIsStarting] = useState(false)
  const [session, setSession] = useState<IdcSession | null>(null)
  const [countdown, setCountdown] = useState(0)
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
          toast.success(`IDC 上号成功，凭据 #${result.credentialId}`)
          onSuccess?.()
        } else if (result.status === 'expired') {
          stopPolling()
          toast.error('授权已超时，请重新发起')
          setStep('form')
        } else {
          stopPolling()
          toast.error(result.message || 'IDC 登录失败')
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
      toast.error('请输入 Start URL')
      return
    }
    setIsStarting(true)
    try {
      const resp = await startIdcLogin({
        startUrl: startUrl.trim(),
        region: region.trim() || 'us-east-1',
        priority: Number(priority) || 100,
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
            toast.error('授权已超时')
            return 0
          }
          return prev - 1
        })
      }, 1000)
      poll(resp.sessionId)
    } catch (err) {
      toast.error(extractErrorMessage(err))
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
    if (ok) toast.success('User Code 已复制')
    else toast.error('复制失败')
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
          <DialogTitle>IDC 上号（AWS SSO）</DialogTitle>
        </DialogHeader>

        {step === 'form' && (
          <div className="space-y-4 py-2">
            <p className="text-sm text-muted-foreground">
              通过 AWS IAM Identity Center 登录企业账号。输入你的 Start URL，然后在浏览器中完成授权。
            </p>
            <div className="space-y-2">
              <label className="text-sm font-medium" htmlFor="startUrl">
                Start URL
              </label>
              <Input
                id="startUrl"
                value={startUrl}
                onChange={(e) => setStartUrl(e.target.value)}
                placeholder="https://d-xxxxxxxxxx.awsapps.com/start"
                disabled={isStarting}
              />
              <p className="text-xs text-muted-foreground">你的 AWS SSO 入口地址</p>
            </div>
            <div className="space-y-2">
              <label className="text-sm font-medium" htmlFor="idcRegion">
                Region
              </label>
              <Input
                id="idcRegion"
                value={region}
                onChange={(e) => setRegion(e.target.value)}
                placeholder="us-east-1"
                disabled={isStarting}
              />
            </div>
            <div className="space-y-2">
              <label className="text-sm font-medium" htmlFor="idcPriority">
                优先级
              </label>
              <NumberStepper
                value={Number(priority) || 0}
                onChange={(n) => setPriority(String(n))}
                min={0}
                disabled={isStarting}
                className="w-full"
                aria-label="优先级"
              />
              <p className="text-xs text-muted-foreground">数字越小优先级越高</p>
            </div>
            <div className="space-y-2">
              <label className="text-sm font-medium" htmlFor="idcProxy">
                代理（可选）
              </label>
              <Input
                id="idcProxy"
                value={proxyUrl}
                onChange={(e) => setProxyUrl(e.target.value)}
                placeholder="留空使用全局代理"
                disabled={isStarting}
              />
            </div>
          </div>
        )}

        {step === 'waiting' && session && (
          <div className="space-y-4 py-2">
            <p className="text-sm text-muted-foreground">
              请在浏览器中打开授权页面，输入下方验证码完成登录。
            </p>
            <div className="rounded-lg border bg-muted/50 p-4 text-center space-y-2">
              <p className="text-xs text-muted-foreground">验证码</p>
              <p className="text-2xl font-mono font-bold tracking-widest">{session.userCode}</p>
              <Button type="button" variant="ghost" size="sm" onClick={handleCopyCode}>
                复制验证码
              </Button>
            </div>
            <Button type="button" className="w-full" onClick={handleOpenVerification}>
              打开授权页面
            </Button>
            <div className="flex items-center justify-between text-sm text-muted-foreground">
              <div className="flex items-center gap-2">
                <span className="inline-block h-2 w-2 animate-pulse rounded-full bg-primary" />
                等待授权完成…
              </div>
              <span className="tabular-nums">{formatCountdown(countdown)}</span>
            </div>
          </div>
        )}

        {step === 'done' && (
          <div className="space-y-3 py-4 text-center">
            <CheckCircle2 className="mx-auto h-12 w-12 text-green-600 dark:text-green-400" />
            <p className="text-sm font-medium">IDC 上号成功</p>
            <p className="text-xs text-muted-foreground">凭据已加入池</p>
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
                取消
              </Button>
              <Button type="button" onClick={handleStart} disabled={isStarting}>
                {isStarting ? '启动中…' : '开始登录'}
              </Button>
            </>
          )}
          {step === 'waiting' && (
            <Button type="button" variant="outline" onClick={() => onOpenChange(false)}>
              取消
            </Button>
          )}
          {step === 'done' && (
            <Button type="button" onClick={() => onOpenChange(false)}>
              完成
            </Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
