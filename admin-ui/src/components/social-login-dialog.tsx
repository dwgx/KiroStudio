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
import { startSocialLogin, pollSocialLogin } from '@/api/credentials'
import { extractErrorMessage } from '@/lib/utils'
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
  const [step, setStep] = useState<Step>('form')
  const [priority, setPriority] = useState('100')
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
          toast.success(`上号成功，凭据 #${result.credentialId}`)
          onSuccess?.()
        } else {
          stopPolling()
          toast.error(result.message || '登录失败')
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
        priority: Number(priority) || 100,
        proxyUrl: proxyUrl.trim() || undefined,
      })
      setSession(resp)
      setStep('waiting')
      // 自动打开登录页
      window.open(resp.portalUrl, '_blank', 'noopener,noreferrer')
      poll(resp.sessionId)
    } catch (err) {
      toast.error(extractErrorMessage(err))
    } finally {
      setIsStarting(false)
    }
  }

  const handleCopy = async () => {
    if (!session) return
    try {
      await navigator.clipboard.writeText(session.portalUrl)
      toast.success('登录链接已复制')
    } catch {
      toast.error('复制失败，请手动复制')
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-[480px]">
        <DialogHeader>
          <DialogTitle>网页上号</DialogTitle>
        </DialogHeader>

        {step === 'form' && (
          <div className="space-y-4 py-2">
            <p className="text-sm text-muted-foreground">
              在浏览器中登录你的 Kiro 账号，完成后凭据会自动加入池，无需手动复制 token。
            </p>
            <div className="space-y-2">
              <label className="text-sm font-medium" htmlFor="priority">
                优先级
              </label>
              <Input
                id="priority"
                type="number"
                value={priority}
                onChange={(e) => setPriority(e.target.value)}
                placeholder="100"
                disabled={isStarting}
              />
              <p className="text-xs text-muted-foreground">数字越小优先级越高</p>
            </div>
            <div className="space-y-2">
              <label className="text-sm font-medium" htmlFor="proxyUrl">
                代理（可选）
              </label>
              <Input
                id="proxyUrl"
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
              已打开登录页。若没弹出，请复制下方链接到浏览器手动打开，登录后自动完成。
            </p>
            <div className="flex items-center gap-2">
              <Input readOnly value={session.portalUrl} className="text-xs" />
              <Button type="button" variant="outline" onClick={handleCopy}>
                复制
              </Button>
            </div>
            <div className="flex items-center gap-2 text-sm text-muted-foreground">
              <span className="inline-block h-2 w-2 animate-pulse rounded-full bg-primary" />
              等待浏览器完成登录…
            </div>
          </div>
        )}

        {step === 'done' && (
          <div className="space-y-3 py-4 text-center">
            <div className="text-3xl">✓</div>
            <p className="text-sm font-medium">上号成功</p>
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
