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
import {
  startSocialLogin,
  pollSocialLogin,
  startIdcLogin,
  pollIdcLogin,
} from '@/api/credentials'
import { copyToClipboard, extractErrorMessage } from '@/lib/utils'
import type { StartSocialLoginResponse } from '@/types/api'

interface LoginDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  onSuccess?: () => void
}

type Mode = 'web' | 'idc'
type Step = 'form' | 'waiting' | 'done'

interface IdcSession {
  sessionId: string
  verificationUri: string
  verificationUriComplete?: string
  userCode: string
  expiresIn: number
}

const SOCIAL_POLL_MS = 2000
const IDC_POLL_MS = 5000

export function LoginDialog({ open, onOpenChange, onSuccess }: LoginDialogProps) {
  const [mode, setMode] = useState<Mode>('idc')
  const [step, setStep] = useState<Step>('form')

  // Shared
  const [priority, setPriority] = useState('100')
  const [proxyUrl, setProxyUrl] = useState('')
  const [isStarting, setIsStarting] = useState(false)
  const pollTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null)

  // Web login state
  const [webSession, setWebSession] = useState<StartSocialLoginResponse | null>(null)
  const [resultEmail, setResultEmail] = useState<string | null>(null)

  // IDC state
  const [startUrl, setStartUrl] = useState('')
  const [region, setRegion] = useState('us-east-1')
  const [idcSession, setIdcSession] = useState<IdcSession | null>(null)
  const [countdown, setCountdown] = useState(0)
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
        setWebSession(null)
        setIdcSession(null)
        setResultEmail(null)
        setCountdown(0)
        setIsStarting(false)
      }, 200)
      return () => clearTimeout(t)
    }
  }, [open])

  useEffect(() => () => stopPolling(), [])

  // --- Web login ---
  const pollWeb = (sessionId: string) => {
    pollTimerRef.current = setTimeout(async () => {
      try {
        const result = await pollSocialLogin(sessionId)
        if (result.status === 'pending') {
          pollWeb(sessionId)
        } else if (result.status === 'done') {
          stopPolling()
          setResultEmail(result.email ?? null)
          setStep('done')
          toast.success(`上号成功，凭据 #${result.credentialId}`)
          onSuccess?.()
        } else {
          stopPolling()
          toast.error(result.message || '登录失败')
          setStep('form')
        }
      } catch {
        pollWeb(sessionId)
      }
    }, SOCIAL_POLL_MS)
  }

  const handleStartWeb = async () => {
    setIsStarting(true)
    try {
      const resp = await startSocialLogin({
        priority: Number(priority) || 100,
        proxyUrl: proxyUrl.trim() || undefined,
      })
      setWebSession(resp)
      setStep('waiting')
      pollWeb(resp.sessionId)
    } catch (err) {
      toast.error(extractErrorMessage(err))
    } finally {
      setIsStarting(false)
    }
  }

  // --- IDC login ---
  const pollIdc = (sessionId: string) => {
    pollTimerRef.current = setTimeout(async () => {
      try {
        const result = await pollIdcLogin(sessionId)
        if (result.status === 'pending') {
          pollIdc(sessionId)
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
      } catch {
        pollIdc(sessionId)
      }
    }, IDC_POLL_MS)
  }

  const handleStartIdc = async () => {
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
      setIdcSession(resp)
      setStep('waiting')
      setCountdown(resp.expiresIn)
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
      pollIdc(resp.sessionId)
    } catch (err) {
      toast.error(extractErrorMessage(err))
    } finally {
      setIsStarting(false)
    }
  }

  const handleStart = () => {
    if (mode === 'web') handleStartWeb()
    else handleStartIdc()
  }

  const handleCopyUrl = async (url: string) => {
    const ok = await copyToClipboard(url)
    if (ok) toast.success('链接已复制')
    else toast.error('复制失败，请手动选中链接复制')
  }

  const formatCountdown = (secs: number) => {
    const m = Math.floor(secs / 60)
    const s = secs % 60
    return `${m}:${s.toString().padStart(2, '0')}`
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-[500px]">
        <DialogHeader>
          <DialogTitle>上号</DialogTitle>
        </DialogHeader>

        {/* Tab switcher - only in form step */}
        {step === 'form' && (
          <div className="flex border-b border-[#2e2e2e] mb-2">
            <button
              onClick={() => setMode('web')}
              className={`flex-1 py-2 text-sm font-medium border-b-2 transition-colors ${
                mode === 'web'
                  ? 'border-[#0070f3] text-[#ededed]'
                  : 'border-transparent text-[#888] hover:text-[#ededed]'
              }`}
            >
              网页上号
            </button>
            <button
              onClick={() => setMode('idc')}
              className={`flex-1 py-2 text-sm font-medium border-b-2 transition-colors ${
                mode === 'idc'
                  ? 'border-[#0070f3] text-[#ededed]'
                  : 'border-transparent text-[#888] hover:text-[#ededed]'
              }`}
            >
              IDC 上号
            </button>
          </div>
        )}

        {/* Form step */}
        {step === 'form' && mode === 'web' && (
          <div className="space-y-4 py-2">
            <p className="text-sm text-muted-foreground">
              在浏览器中登录你的 Kiro 账号，完成后凭据会自动加入池。
            </p>
            <div className="space-y-2">
              <label className="text-sm font-medium">优先级</label>
              <Input
                type="number"
                value={priority}
                onChange={(e) => setPriority(e.target.value)}
                placeholder="100"
                disabled={isStarting}
              />
              <p className="text-xs text-muted-foreground">数字越小优先级越高</p>
            </div>
            <div className="space-y-2">
              <label className="text-sm font-medium">代理（可选）</label>
              <Input
                value={proxyUrl}
                onChange={(e) => setProxyUrl(e.target.value)}
                placeholder="留空使用全局代理"
                disabled={isStarting}
              />
            </div>
          </div>
        )}

        {step === 'form' && mode === 'idc' && (
          <div className="space-y-4 py-2">
            <p className="text-sm text-muted-foreground">
              通过 AWS IAM Identity Center 登录。输入 Start URL 后在浏览器中授权。
            </p>
            <div className="space-y-2">
              <label className="text-sm font-medium">Start URL</label>
              <Input
                value={startUrl}
                onChange={(e) => setStartUrl(e.target.value)}
                placeholder="https://d-xxxxxxxxxx.awsapps.com/start"
                disabled={isStarting}
              />
            </div>
            <div className="space-y-2">
              <label className="text-sm font-medium">Region</label>
              <Input
                value={region}
                onChange={(e) => setRegion(e.target.value)}
                placeholder="us-east-1"
                disabled={isStarting}
              />
            </div>
            <div className="space-y-2">
              <label className="text-sm font-medium">优先级</label>
              <Input
                type="number"
                value={priority}
                onChange={(e) => setPriority(e.target.value)}
                placeholder="100"
                disabled={isStarting}
              />
              <p className="text-xs text-muted-foreground">数字越小优先级越高</p>
            </div>
            <div className="space-y-2">
              <label className="text-sm font-medium">代理（可选）</label>
              <Input
                value={proxyUrl}
                onChange={(e) => setProxyUrl(e.target.value)}
                placeholder="留空使用全局代理"
                disabled={isStarting}
              />
            </div>
          </div>
        )}

        {/* Waiting step - Web */}
        {step === 'waiting' && mode === 'web' && webSession && (
          <div className="space-y-4 py-2">
            <p className="text-sm text-muted-foreground">
              点「打开登录页」在新标签页登录 Kiro 账号；登录完成后会自动检测并入池。
            </p>
            <div className="flex items-center gap-2">
              <Input
                readOnly
                value={webSession.portalUrl}
                className="text-xs"
                onFocus={(e) => e.currentTarget.select()}
              />
              <Button type="button" variant="outline" onClick={() => handleCopyUrl(webSession.portalUrl)}>
                复制
              </Button>
            </div>
            <Button
              type="button"
              className="w-full"
              onClick={() => window.open(webSession.portalUrl, '_blank', 'noopener,noreferrer')}
            >
              打开登录页
            </Button>
            <div className="flex items-center gap-2 text-sm text-muted-foreground">
              <span className="inline-block h-2 w-2 animate-pulse rounded-full bg-primary" />
              等待浏览器完成登录…
            </div>
          </div>
        )}

        {/* Waiting step - IDC */}
        {step === 'waiting' && mode === 'idc' && idcSession && (
          <div className="space-y-4 py-2">
            <p className="text-sm text-muted-foreground">
              在浏览器中打开授权页面，输入验证码完成登录。
            </p>
            <div className="rounded-lg border bg-muted/50 p-4 text-center space-y-2">
              <p className="text-xs text-muted-foreground">验证码</p>
              <p className="text-2xl font-mono font-bold tracking-widest">{idcSession.userCode}</p>
              <Button
                type="button"
                variant="ghost"
                size="sm"
                onClick={() => copyToClipboard(idcSession.userCode).then(ok => {
                  if (ok) toast.success('验证码已复制')
                  else toast.error('复制失败')
                })}
              >
                复制验证码
              </Button>
            </div>
            <div className="flex items-center gap-2">
              <Input
                readOnly
                value={idcSession.verificationUriComplete || idcSession.verificationUri}
                className="text-xs"
                onFocus={(e) => e.currentTarget.select()}
              />
              <Button
                type="button"
                variant="outline"
                onClick={() => handleCopyUrl(idcSession.verificationUriComplete || idcSession.verificationUri)}
              >
                复制
              </Button>
            </div>
            <Button
              type="button"
              className="w-full"
              onClick={() => {
                const url = idcSession.verificationUriComplete || idcSession.verificationUri
                window.open(url, '_blank', 'noopener,noreferrer')
              }}
            >
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

        {/* Done step */}
        {step === 'done' && (
          <div className="space-y-3 py-4 text-center">
            <div className="text-3xl">✓</div>
            <p className="text-sm font-medium">
              {mode === 'web' ? '网页上号成功' : 'IDC 上号成功'}
            </p>
            {resultEmail && (
              <p className="text-xs text-muted-foreground">{resultEmail}</p>
            )}
            {mode === 'idc' && (
              <p className="text-xs text-muted-foreground">凭据已加入池</p>
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
