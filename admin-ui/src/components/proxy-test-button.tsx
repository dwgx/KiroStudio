import { useState } from 'react'
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
  const [pending, setPending] = useState(false)

  const handleTest = async () => {
    setPending(true)
    const url = proxyUrl.trim()
    const pendingToast = toast.loading(url && url !== 'direct' ? '正在测试代理连通性…' : '正在测试直连…')
    try {
      const res = await testProxy({
        proxyUrl: url,
        proxyUsername: proxyUsername?.trim() || undefined,
        proxyPassword: proxyPassword || undefined,
      })
      if (res.ok) {
        toast.success(
          `代理可用 · 延迟 ${res.latencyMs}ms · 出口 ${res.exitIp ?? '未知'}`,
          { id: pendingToast },
        )
      } else {
        toast.error('代理不可用：' + (res.error || '未知错误'), { id: pendingToast })
      }
    } catch (err) {
      toast.error('测活失败：' + (err as Error).message, { id: pendingToast })
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
      onClick={handleTest}
      disabled={pending}
      title="测试该代理连通性（后端探测出口 IP + 延迟）"
    >
      {pending ? <Loader2 className="h-4 w-4 animate-spin" /> : <Wifi className="h-4 w-4" />}
      <span className="ml-1">测活</span>
    </Button>
  )
}
