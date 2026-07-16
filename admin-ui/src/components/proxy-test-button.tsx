import { useState } from 'react'
import { useTranslation } from 'react-i18next'
import { toast } from 'sonner'
import { Loader2, Wifi } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { testProxy } from '@/api/ops'

interface ProxyTestButtonProps {
  /** 待测代理 URL；留空/"direct" 表示测直连。 */
  proxyUrl: string
  /** 可选账密（覆盖 URL 内嵌账密，用于表单里账密单独输入的场景）。 */
  proxyUsername?: string
  proxyPassword?: string
  /** 追加类名（微调尺寸/间距）。 */
  className?: string
}

/**
 * 代理测活小按钮：点一下让后端走该代理请求固定探测端点拿出口 IP + 延迟。
 * 成功 toast「代理可用 · 延迟{ms}ms · 出口 {ip}」；失败 toast 具体错误。
 * proxyUrl 留空则测直连（后端识别空串/"direct"）。适合紧贴代理输入框放。
 */
export function ProxyTestButton({
  proxyUrl,
  proxyUsername,
  proxyPassword,
  className,
}: ProxyTestButtonProps) {
  const { t } = useTranslation()
  const [pending, setPending] = useState(false)

  const handleTest = async () => {
    setPending(true)
    // 说明：阻止冒泡/默认提交在 onClick 里完成（见下），此处只负责测活逻辑。
    const url = proxyUrl.trim()
    const pendingToast = toast.loading(
      url && url !== 'direct'
        ? t('proxytestbutton.toast.testingProxy')
        : t('proxytestbutton.toast.testingDirect'),
    )
    try {
      const res = await testProxy({
        proxyUrl: url,
        proxyUsername: proxyUsername?.trim() || undefined,
        proxyPassword: proxyPassword || undefined,
      })
      if (res.ok) {
        toast.success(
          t('proxytestbutton.toast.ok', {
            latencyMs: res.latencyMs,
            exitIp: res.exitIp ?? t('proxytestbutton.toast.unknownIp'),
          }),
          { id: pendingToast },
        )
      } else {
        toast.error(
          t('proxytestbutton.toast.unavailable', {
            error: res.error || t('proxytestbutton.toast.unknownError'),
          }),
          { id: pendingToast },
        )
      }
    } catch (err) {
      toast.error(
        t('proxytestbutton.toast.failed', { message: (err as Error).message }),
        { id: pendingToast },
      )
    } finally {
      setPending(false)
    }
  }

  return (
    <Button
      type="button"
      size="sm"
      variant="outline"
      className={className}
      onClick={(e) => {
        // 防冒泡：本按钮常被放进 <form>（如 add-credential-dialog）或 Dialog 里，
        // 裸 onClick 会冒泡触发外层 form submit / onClick，导致「点测活弹 toast 后
        // dialog 直接关闭 / 表单跳转」。preventDefault 挡默认提交，stopPropagation 断冒泡。
        e.preventDefault()
        e.stopPropagation()
        void handleTest()
      }}
      disabled={pending}
      title={t('proxytestbutton.title')}
    >
      {pending ? <Loader2 className="h-4 w-4 animate-spin" /> : <Wifi className="h-4 w-4" />}
      <span className="ml-1">{t('proxytestbutton.label')}</span>
    </Button>
  )
}
