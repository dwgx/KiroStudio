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
import {
  startSocialLogin,
  pollSocialLogin,
  startIdcLogin,
  pollIdcLogin,
  startExternalIdpLogin,
  submitExternalIdpLeg1,
  submitExternalIdpLeg2,
  submitExternalIdpLeg2Select,
} from '@/api/credentials'
import { copyToClipboard, extractErrorMessage } from '@/lib/utils'
import { CheckCircle2, XCircle, Loader2 } from 'lucide-react'
import { AnimatedHeight } from '@/components/ui/animated-height'
import { RegionSelect } from '@/components/ui/region-select'
import { regionLabel } from '@/lib/regions'
import type { StartSocialLoginResponse, ExternalIdpProfileOption } from '@/types/api'

interface LoginDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  onSuccess?: () => void
}

type Mode = 'web' | 'idc' | 'external-idp'
type Step = 'form' | 'waiting' | 'done'
// 微软 SSO 独立的 3 步引导：0=表单 1=粘回登录 URL 2=粘回授权 URL 3=完成
type EidpStep = 0 | 1 | 2 | 3 | 'select'

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
  const { t } = useTranslation()
  const [mode, setMode] = useState<Mode>('idc')
  const [step, setStep] = useState<Step>('form')

  // Shared
  const [priority, setPriority] = useState('0')
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
  // 优先探测区域（可选）：微软号 region 由授权后探测发现，此值会被并入探测候选并排头，
  // 让冷门 region（如只在 eu-central-1 开通的号）也能被探到。留空则用默认候选表。
  const [eidpRegion, setEidpRegion] = useState('')
  const [eidpBusy, setEidpBusy] = useState(false)
  const [eidpSessionId, setEidpSessionId] = useState('')
  const [eidpSigninUrl, setEidpSigninUrl] = useState('')
  const [eidpAuthorizeUrl, setEidpAuthorizeUrl] = useState('')
  const [eidpUrl1, setEidpUrl1] = useState('')
  const [eidpUrl2, setEidpUrl2] = useState('')
  // 多 region profile 选择：leg2 返回多个 profile 时填充，用户选一个后调 select 建号。
  const [eidpProfiles, setEidpProfiles] = useState<ExternalIdpProfileOption[]>([])
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
        setEidpRegion('')
        setEidpBusy(false)
        setEidpSessionId('')
        setEidpSigninUrl('')
        setEidpAuthorizeUrl('')
        setEidpUrl1('')
        setEidpUrl2('')
        setEidpCredId(null)
        setEidpProfiles([])
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
        priority: Number(priority) || 0,
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
      } catch {
        pollIdc(sessionId)
      }
    }, IDC_POLL_MS)
  }

  const handleStartIdc = async () => {
    if (!startUrl.trim()) {
      toast.error(t('idclogindialog.toast.startUrlRequired'))
      return
    }
    setIsStarting(true)
    try {
      const resp = await startIdcLogin({
        startUrl: startUrl.trim(),
        region: region.trim() || 'us-east-1',
        priority: Number(priority) || 0,
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
            toast.error(t('idclogindialog.toast.authTimeout'))
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
        priority: Number(priority) || 0,
        proxyUrl: proxyUrl.trim() || undefined,
        region: eidpRegion.trim() || undefined,
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
      // 恰 1 个 profile：后端已自动建号，直接完成。
      if (resp.credentialId != null) {
        setEidpCredId(resp.credentialId)
        setEidpStep(3)
        toast.success(`微软上号成功，凭据 #${resp.credentialId}`)
        onSuccess?.()
        return
      }
      // 多个 region profile：进选择步，让用户选一个。
      if (resp.profiles.length > 0) {
        setEidpProfiles(resp.profiles)
        setEidpStep('select')
        return
      }
      toast.error('该账号未探测到可用 profile（可能未开通 Kiro/CodeWhisperer）')
    } catch (err) {
      toast.error(extractErrorMessage(err))
    } finally {
      setEidpBusy(false)
    }
  }

  const handleEidpSelect = async (arn: string) => {
    setEidpBusy(true)
    try {
      const resp = await submitExternalIdpLeg2Select(eidpSessionId, arn)
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
              <label className="text-sm font-medium">{t('idclogindialog.form.priorityLabel')}</label>
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
              <label className="text-sm font-medium">{t('idclogindialog.form.proxyLabel')}</label>
              <div className="flex items-center gap-2">
                <Input
                  className="flex-1"
                  value={proxyUrl}
                  onChange={(e) => setProxyUrl(e.target.value)}
                  placeholder={t('idclogindialog.form.proxyPlaceholder')}
                  disabled={isStarting}
                />
                <ProxyTestButton proxyUrl={proxyUrl} />
              </div>
            </div>
          </div>
        )}

        {step === 'form' && mode === 'idc' && (
          <div className="space-y-4 py-2">
            <p className="text-sm text-muted-foreground">
              {t('idclogindialog.form.intro')}
            </p>
            <div className="space-y-2">
              <label className="text-sm font-medium">{t('idclogindialog.form.startUrlLabel')}</label>
              <Input
                value={startUrl}
                onChange={(e) => setStartUrl(e.target.value)}
                placeholder="https://d-xxxxxxxxxx.awsapps.com/start"
                disabled={isStarting}
              />
            </div>
            <div className="space-y-2">
              <label className="text-sm font-medium">{t('idclogindialog.form.regionLabel')}</label>
              <RegionSelect
                value={region}
                onChange={setRegion}
                placeholder="us-east-1"
                disabled={isStarting}
              />
              <p className="text-xs text-muted-foreground">
                {t('idclogindialog.form.regionHelp')}
              </p>
            </div>
            <div className="space-y-2">
              <label className="text-sm font-medium">{t('idclogindialog.form.priorityLabel')}</label>
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
              <label className="text-sm font-medium">{t('idclogindialog.form.proxyLabel')}</label>
              <div className="flex items-center gap-2">
                <Input
                  className="flex-1"
                  value={proxyUrl}
                  onChange={(e) => setProxyUrl(e.target.value)}
                  placeholder={t('idclogindialog.form.proxyPlaceholder')}
                  disabled={isStarting}
                />
                <ProxyTestButton proxyUrl={proxyUrl} />
              </div>
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
              <label className="text-sm font-medium">{t('idclogindialog.form.priorityLabel')}</label>
              <NumberStepper
                value={Number(priority) || 0}
                onChange={(n) => setPriority(String(n))}
                min={0}
                disabled={eidpBusy}
                className="w-full"
                aria-label={t('idclogindialog.form.priorityAriaLabel')}
              />
              <p className="text-xs text-muted-foreground">{t('idclogindialog.form.priorityHelp')}</p>
            </div>
            <div className="space-y-2">
              <label className="text-sm font-medium">优先探测区域（可选）</label>
              <RegionSelect
                value={eidpRegion}
                onChange={setEidpRegion}
                placeholder="留空按默认候选探测"
                disabled={eidpBusy}
              />
              <p className="text-xs text-muted-foreground">
                微软号区域会在授权后自动探测。若你的账号只在冷门区域（如 eu-central-1）开通，
                填这里可优先探测该区域，避免漏掉。不确定就留空。
              </p>
            </div>
            <div className="space-y-2">
              <label className="text-sm font-medium">{t('idclogindialog.form.proxyLabel')}</label>
              <div className="flex items-center gap-2">
                <Input
                  className="flex-1"
                  value={proxyUrl}
                  onChange={(e) => setProxyUrl(e.target.value)}
                  placeholder={t('idclogindialog.form.proxyPlaceholder')}
                  disabled={eidpBusy}
                />
                <ProxyTestButton proxyUrl={proxyUrl} />
              </div>
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

        {/* 微软 SSO：多 region profile 选择（账号在多个 region 各有独立 profile 时）。
            批量验活式卡片列表：展示每个 profile 的可用状态 + 订阅等级，可用的高亮/排前，
            不可用的置灰标注「该区域未开通」。不用 toast，直接卡片内呈现。 */}
        {mode === 'external-idp' && eidpStep === 'select' && (
          <div className="space-y-3 py-2">
            <p className="text-sm text-muted-foreground">
              该账号在多个区域各有一个 profile，请选择要使用的区域（决定对话走哪个上游端点，务必选账号真实开通的区域）。
            </p>
            <div className="space-y-2">
              {[...eidpProfiles]
                .sort((a, b) => Number(b.usable ?? true) - Number(a.usable ?? true))
                .map((p) => {
                  // usable 缺省视为可用（旧后端未下发时不误置灰）。
                  const usable = p.usable ?? true
                  return (
                    <button
                      key={p.arn}
                      type="button"
                      disabled={eidpBusy || !usable}
                      onClick={() => handleEidpSelect(p.arn)}
                      className={`flex w-full items-start justify-between gap-2 rounded-md border px-3 py-2 text-left transition-colors disabled:cursor-not-allowed ${
                        usable
                          ? 'border-input bg-background hover:border-primary hover:bg-accent'
                          : 'border-border bg-secondary/30 opacity-60'
                      }`}
                    >
                      <div className="min-w-0 flex-1">
                        <div className="flex items-center gap-1.5 text-sm font-medium">
                          {usable ? (
                            <CheckCircle2 className="h-3.5 w-3.5 shrink-0 text-emerald-500 dark:text-emerald-400" />
                          ) : (
                            <XCircle className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
                          )}
                          <span className="truncate">{regionLabel(p.region)}</span>
                          {!usable && (
                            <span className="shrink-0 rounded bg-white/5 px-1 py-0.5 text-[10px] text-muted-foreground">
                              该区域未开通
                            </span>
                          )}
                        </div>
                        <div className="mt-0.5 truncate text-xs text-muted-foreground">
                          {p.region}
                          {p.subscriptionTitle ? ` · ${p.subscriptionTitle}` : ''}
                          {p.account ? ` · 账号 ${p.account}` : ''}
                        </div>
                      </div>
                      {eidpBusy && <Loader2 className="mt-0.5 h-4 w-4 shrink-0 animate-spin text-primary" />}
                    </button>
                  )
                })}
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
              {t('idclogindialog.waiting.intro')}
            </p>
            <div className="rounded-lg border bg-muted/50 p-4 text-center space-y-2">
              <p className="text-xs text-muted-foreground">{t('idclogindialog.waiting.codeLabel')}</p>
              <p className="text-2xl font-mono font-bold tracking-widest">{idcSession.userCode}</p>
              <Button
                type="button"
                variant="ghost"
                size="sm"
                onClick={() => copyToClipboard(idcSession.userCode).then(ok => {
                  if (ok) toast.success(t('idclogindialog.toast.userCodeCopied'))
                  else toast.error(t('idclogindialog.toast.copyFailed'))
                })}
              >
                {t('idclogindialog.waiting.copyCode')}
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

        {/* Done step（网页 / IDC） */}
        {step === 'done' && mode !== 'external-idp' && (
          <div className="space-y-3 py-4 text-center">
            <CheckCircle2 className="mx-auto h-12 w-12 text-green-600 dark:text-green-400" />
            <p className="text-sm font-medium">
              {mode === 'web' ? '网页上号成功' : t('idclogindialog.done.title')}
            </p>
            {resultEmail && (
              <p className="text-xs text-muted-foreground">{resultEmail}</p>
            )}
            {mode === 'idc' && (
              <p className="text-xs text-muted-foreground">{t('idclogindialog.done.desc')}</p>
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
                {t('idclogindialog.footer.cancel')}
              </Button>
              <Button type="button" onClick={handleStart} disabled={isStarting}>
                {isStarting ? t('idclogindialog.footer.starting') : t('idclogindialog.footer.startLogin')}
              </Button>
            </>
          )}
          {mode !== 'external-idp' && step === 'waiting' && (
            <Button type="button" variant="outline" onClick={() => onOpenChange(false)}>
              {t('idclogindialog.footer.cancel')}
            </Button>
          )}
          {mode !== 'external-idp' && step === 'done' && (
            <Button type="button" onClick={() => onOpenChange(false)}>
              {t('idclogindialog.footer.done')}
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
                {t('idclogindialog.footer.cancel')}
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
                {t('idclogindialog.footer.cancel')}
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
                {t('idclogindialog.footer.cancel')}
              </Button>
              <Button type="button" onClick={handleEidpLeg2} disabled={eidpBusy}>
                {eidpBusy ? '提交中…' : '完成上号'}
              </Button>
            </>
          )}
          {mode === 'external-idp' && eidpStep === 'select' && (
            <Button
              type="button"
              variant="outline"
              onClick={() => onOpenChange(false)}
              disabled={eidpBusy}
            >
              {t('idclogindialog.footer.cancel')}
            </Button>
          )}
          {mode === 'external-idp' && eidpStep === 3 && (
            <Button type="button" onClick={() => onOpenChange(false)}>
              {t('idclogindialog.footer.done')}
            </Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
