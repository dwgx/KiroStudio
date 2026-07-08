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
import {
  startSocialLogin,
  pollSocialLogin,
  startIdcLogin,
  pollIdcLogin,
  startExternalIdpLogin,
  submitExternalIdpLeg1,
  submitExternalIdpLeg2,
} from '@/api/credentials'
import { copyToClipboard, extractErrorMessage } from '@/lib/utils'
import { CheckCircle2 } from 'lucide-react'
import { AnimatedHeight } from '@/components/ui/animated-height'
import type { StartSocialLoginResponse } from '@/types/api'

interface LoginDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  onSuccess?: () => void
}

type Mode = 'web' | 'idc' | 'external-idp'
type Step = 'form' | 'waiting' | 'done'
// 微软 SSO 独立的 3 步引导：0=表单 1=粘回登录 URL 2=粘回授权 URL 3=完成
type EidpStep = 0 | 1 | 2 | 3

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

  // 微软 SSO（External IdP）state
  const [eidpStep, setEidpStep] = useState<EidpStep>(0)
  const [eidpBusy, setEidpBusy] = useState(false)
  const [eidpSessionId, setEidpSessionId] = useState('')
  const [eidpSigninUrl, setEidpSigninUrl] = useState('')
  const [eidpAuthorizeUrl, setEidpAuthorizeUrl] = useState('')
  const [eidpUrl1, setEidpUrl1] = useState('')
  const [eidpUrl2, setEidpUrl2] = useState('')
  const [eidpCredId, setEidpCredId] = useState<number | null>(null)

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
        setEidpStep(0)
        setEidpBusy(false)
        setEidpSessionId('')
        setEidpSigninUrl('')
        setEidpAuthorizeUrl('')
        setEidpUrl1('')
        setEidpUrl2('')
        setEidpCredId(null)
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

  // --- 微软 SSO（External IdP）：三步引导，无轮询，全程复制浏览器地址栏 URL ---
  const handleEidpStart = async () => {
    setEidpBusy(true)
    try {
      const resp = await startExternalIdpLogin({
        priority: Number(priority) || 100,
        proxyUrl: proxyUrl.trim() || undefined,
      })
      setEidpSessionId(resp.sessionId)
      setEidpSigninUrl(resp.signinUrl)
      setEidpStep(1)
    } catch (err) {
      toast.error(extractErrorMessage(err))
    } finally {
      setEidpBusy(false)
    }
  }

  const handleEidpLeg1 = async () => {
    if (!eidpUrl1.trim()) {
      toast.error('请粘贴登录后浏览器地址栏的完整 URL')
      return
    }
    setEidpBusy(true)
    try {
      const resp = await submitExternalIdpLeg1(eidpSessionId, eidpUrl1.trim())
      setEidpAuthorizeUrl(resp.authorizeUrl)
      setEidpStep(2)
    } catch (err) {
      toast.error(extractErrorMessage(err))
    } finally {
      setEidpBusy(false)
    }
  }

  const handleEidpLeg2 = async () => {
    if (!eidpUrl2.trim()) {
      toast.error('请粘贴授权后浏览器地址栏的完整 URL')
      return
    }
    setEidpBusy(true)
    try {
      const resp = await submitExternalIdpLeg2(eidpSessionId, eidpUrl2.trim())
      setEidpCredId(resp.credentialId)
      setEidpStep(3)
      toast.success(`微软上号成功，凭据 #${resp.credentialId}`)
      onSuccess?.()
    } catch (err) {
      toast.error(extractErrorMessage(err))
    } finally {
      setEidpBusy(false)
    }
  }

  const handleStart = () => {
    if (mode === 'web') handleStartWeb()
    else if (mode === 'idc') handleStartIdc()
    else handleEidpStart()
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

  // 初始表单态（决定 tab 切换器与底部按钮的展示）
  const atStart = mode === 'external-idp' ? eidpStep === 0 : step === 'form'

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-[500px] max-h-[85vh] overflow-y-auto">
        <DialogHeader>
          <DialogTitle>上号</DialogTitle>
        </DialogHeader>

        {/* Tab switcher - 仅初始表单态可切换。底部一条滑动指示条随选中项平移(丝滑切换)。 */}
        {atStart && (
          <div className="relative mb-2 flex border-b border-[#2e2e2e]">
            {(['web', 'idc', 'external-idp'] as Mode[]).map((m) => (
              <button
                key={m}
                onClick={() => setMode(m)}
                className={`flex-1 py-2 text-sm font-medium transition-colors ${
                  mode === m ? 'text-[#ededed]' : 'text-[#888] hover:text-[#ededed]'
                }`}
              >
                {m === 'web' ? '网页上号' : m === 'idc' ? 'IDC 上号' : '微软SSO'}
              </button>
            ))}
            {/* 滑动指示条:宽度=1/3,left 随选中项平移,transform 过渡丝滑 */}
            <span
              className="pointer-events-none absolute bottom-0 h-0.5 w-1/3 rounded-full bg-[#0070f3] transition-transform duration-300 ease-out-expo motion-reduce:transition-none"
              style={{ transform: `translateX(${mode === 'web' ? 0 : mode === 'idc' ? 100 : 200}%)` }}
            />
          </div>
        )}

        {/* 内容区:AnimatedHeight 平滑过渡高度(切换上号方式不再"一下子拉长",以当前高度为模板延展);
            内层 key=mode 重挂载重放淡入。两者叠加=以第一个卡片高度为起点平滑延展出新内容。 */}
        <AnimatedHeight>
        <div key={mode} className="animate-rise-in">

        {/* Form step */}
        {step === 'form' && mode === 'web' && (
          <div className="space-y-4 py-2">
            <p className="text-sm text-muted-foreground">
              在浏览器中登录你的 Kiro 账号，完成后凭据会自动加入池。
            </p>
            <div className="space-y-2">
              <label className="text-sm font-medium">优先级</label>
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

        {/* 微软 SSO 表单（第 0 步） */}
        {mode === 'external-idp' && eidpStep === 0 && (
          <div className="space-y-4 py-2">
            <div className="rounded-md border border-[#2e2e2e] bg-[#0070f3]/5 p-3 text-xs text-muted-foreground leading-relaxed">
              全程<strong className="text-[#ededed]">零本机运行</strong>：你的机器不装、不跑任何程序，
              只需在浏览器里登录微软账号，再把地址栏里跳转出来的 URL 复制粘贴回来即可。
              下面会分 3 步引导你完成。
            </div>
            <div className="space-y-2">
              <label className="text-sm font-medium">优先级</label>
              <NumberStepper
                value={Number(priority) || 0}
                onChange={(n) => setPriority(String(n))}
                min={0}
                disabled={eidpBusy}
                className="w-full"
                aria-label="优先级"
              />
              <p className="text-xs text-muted-foreground">数字越小优先级越高</p>
            </div>
            <div className="space-y-2">
              <label className="text-sm font-medium">代理（可选）</label>
              <Input
                value={proxyUrl}
                onChange={(e) => setProxyUrl(e.target.value)}
                placeholder="留空使用全局代理"
                disabled={eidpBusy}
              />
            </div>
          </div>
        )}

        {/* 微软 SSO 第 1 步：打开登录链接，粘回登录后地址栏 URL */}
        {mode === 'external-idp' && eidpStep === 1 && (
          <div className="space-y-4 py-2">
            <div className="text-sm font-medium text-[#ededed]">步骤 1 / 3 · 登录微软账号</div>
            <p className="text-sm text-muted-foreground leading-relaxed">
              在浏览器中打开下面的链接，登录你的 Microsoft 账号。
              登录后浏览器会跳到一个<strong className="text-[#ededed]">打不开的 localhost:3128 页面</strong>
              （显示空白或“无法访问”都属正常）。把那个页面
              <strong className="text-[#ededed]">地址栏里的完整 URL</strong> 复制，粘到下面的输入框。
            </p>
            <div className="flex items-center gap-2">
              <Input
                readOnly
                value={eidpSigninUrl}
                className="text-xs"
                onFocus={(e) => e.currentTarget.select()}
              />
              <Button type="button" variant="outline" onClick={() => handleCopyUrl(eidpSigninUrl)}>
                复制
              </Button>
            </div>
            <Button
              type="button"
              className="w-full"
              onClick={() => window.open(eidpSigninUrl, '_blank', 'noopener,noreferrer')}
            >
              打开登录链接
            </Button>
            <div className="space-y-2">
              <label className="text-sm font-medium">粘贴登录后地址栏的完整 URL</label>
              <textarea
                value={eidpUrl1}
                onChange={(e) => setEidpUrl1(e.target.value)}
                placeholder="http://localhost:3128/oauth/callback?code=...&state=..."
                disabled={eidpBusy}
                className="flex min-h-[72px] w-full rounded-md border border-input bg-background px-3 py-2 text-xs font-mono ring-offset-background placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 disabled:cursor-not-allowed disabled:opacity-50"
              />
            </div>
          </div>
        )}

        {/* 微软 SSO 第 2 步：打开授权链接，粘回授权后地址栏 URL */}
        {mode === 'external-idp' && eidpStep === 2 && (
          <div className="space-y-4 py-2">
            <div className="text-sm font-medium text-[#ededed]">步骤 2 / 3 · 完成微软授权</div>
            <p className="text-sm text-muted-foreground leading-relaxed">
              打开下面的链接完成 Microsoft 授权。授权后浏览器同样会跳到一个
              <strong className="text-[#ededed]">打不开的 localhost 页面</strong>（空白或无法访问都正常）。
              再次把该页面<strong className="text-[#ededed]">地址栏里的完整 URL</strong> 复制，粘到下面。
            </p>
            <div className="flex items-center gap-2">
              <Input
                readOnly
                value={eidpAuthorizeUrl}
                className="text-xs"
                onFocus={(e) => e.currentTarget.select()}
              />
              <Button type="button" variant="outline" onClick={() => handleCopyUrl(eidpAuthorizeUrl)}>
                复制
              </Button>
            </div>
            <Button
              type="button"
              className="w-full"
              onClick={() => window.open(eidpAuthorizeUrl, '_blank', 'noopener,noreferrer')}
            >
              打开授权链接
            </Button>
            <div className="space-y-2">
              <label className="text-sm font-medium">粘贴授权后地址栏的完整 URL</label>
              <textarea
                value={eidpUrl2}
                onChange={(e) => setEidpUrl2(e.target.value)}
                placeholder="http://localhost:3128/...?code=...&state=..."
                disabled={eidpBusy}
                className="flex min-h-[72px] w-full rounded-md border border-input bg-background px-3 py-2 text-xs font-mono ring-offset-background placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 disabled:cursor-not-allowed disabled:opacity-50"
              />
            </div>
          </div>
        )}

        {/* 微软 SSO 第 3 步：完成 */}
        {mode === 'external-idp' && eidpStep === 3 && (
          <div className="space-y-3 py-4 text-center">
            <CheckCircle2 className="mx-auto h-12 w-12 text-green-600 dark:text-green-400" />
            <p className="text-sm font-medium">微软上号成功</p>
            {eidpCredId !== null && (
              <p className="text-xs text-muted-foreground">凭据 #{eidpCredId} 已加入池</p>
            )}
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

        {/* Done step（网页 / IDC） */}
        {step === 'done' && mode !== 'external-idp' && (
          <div className="space-y-3 py-4 text-center">
            <CheckCircle2 className="mx-auto h-12 w-12 text-green-600 dark:text-green-400" />
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

        </div>{/* /内容区 key=mode 动画包裹 */}
        </AnimatedHeight>

        <DialogFooter>
          {/* 网页 / IDC 底部按钮 */}
          {mode !== 'external-idp' && step === 'form' && (
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
          {mode !== 'external-idp' && step === 'waiting' && (
            <Button type="button" variant="outline" onClick={() => onOpenChange(false)}>
              取消
            </Button>
          )}
          {mode !== 'external-idp' && step === 'done' && (
            <Button type="button" onClick={() => onOpenChange(false)}>
              完成
            </Button>
          )}

          {/* 微软 SSO 底部按钮 */}
          {mode === 'external-idp' && eidpStep === 0 && (
            <>
              <Button
                type="button"
                variant="outline"
                onClick={() => onOpenChange(false)}
                disabled={eidpBusy}
              >
                取消
              </Button>
              <Button type="button" onClick={handleStart} disabled={eidpBusy}>
                {eidpBusy ? '启动中…' : '开始'}
              </Button>
            </>
          )}
          {mode === 'external-idp' && eidpStep === 1 && (
            <>
              <Button
                type="button"
                variant="outline"
                onClick={() => onOpenChange(false)}
                disabled={eidpBusy}
              >
                取消
              </Button>
              <Button type="button" onClick={handleEidpLeg1} disabled={eidpBusy}>
                {eidpBusy ? '提交中…' : '下一步'}
              </Button>
            </>
          )}
          {mode === 'external-idp' && eidpStep === 2 && (
            <>
              <Button
                type="button"
                variant="outline"
                onClick={() => onOpenChange(false)}
                disabled={eidpBusy}
              >
                取消
              </Button>
              <Button type="button" onClick={handleEidpLeg2} disabled={eidpBusy}>
                {eidpBusy ? '提交中…' : '完成上号'}
              </Button>
            </>
          )}
          {mode === 'external-idp' && eidpStep === 3 && (
            <Button type="button" onClick={() => onOpenChange(false)}>
              完成
            </Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
