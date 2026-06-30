import { toast } from 'sonner'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { useConfigSnapshot, useLoadBalancingMode, useSetLoadBalancingMode } from '@/hooks/use-credentials'
import { extractErrorMessage } from '@/lib/utils'

// 一行只读配置项
function Row({ label, value, mono }: { label: string; value: React.ReactNode; mono?: boolean }) {
  return (
    <div className="flex items-start justify-between gap-4 py-2 border-b last:border-0">
      <span className="text-sm text-muted-foreground shrink-0">{label}</span>
      <span className={`text-sm text-right break-all ${mono ? 'font-mono text-xs' : ''}`}>{value}</span>
    </div>
  )
}

function BoolBadge({ on, onText = '已启用', offText = '已关闭' }: { on: boolean; onText?: string; offText?: string }) {
  return (
    <Badge variant={on ? 'default' : 'secondary'}>{on ? onText : offText}</Badge>
  )
}

export function SettingsPage() {
  const { data: config, isLoading, error, refetch } = useConfigSnapshot()
  const { data: lbData } = useLoadBalancingMode()
  const { mutate: setLb, isPending: isSettingLb } = useSetLoadBalancingMode()

  const currentMode = lbData?.mode ?? config?.loadBalancingMode ?? 'priority'

  const handleSetMode = (mode: 'priority' | 'balanced') => {
    if (mode === currentMode) return
    setLb(mode, {
      onSuccess: () => toast.success(mode === 'priority' ? '已切换为优先级模式' : '已切换为均衡负载'),
      onError: (err) => toast.error(extractErrorMessage(err)),
    })
  }

  if (isLoading) {
    return (
      <div className="flex items-center justify-center py-24">
        <div className="animate-spin rounded-full h-10 w-10 border-b-2 border-primary" />
      </div>
    )
  }

  if (error || !config) {
    return (
      <div className="flex items-center justify-center py-24">
        <Card className="w-full max-w-md">
          <CardContent className="pt-6 text-center">
            <div className="text-red-500 mb-4">加载配置失败</div>
            <p className="text-muted-foreground mb-4">{error ? (error as Error).message : '无数据'}</p>
            <Button onClick={() => refetch()}>重试</Button>
          </CardContent>
        </Card>
      </div>
    )
  }

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <h2 className="text-xl font-semibold">设置</h2>
        <Button variant="outline" size="sm" onClick={() => refetch()}>
          刷新
        </Button>
      </div>

      {/* 可调整：负载均衡 */}
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base">负载均衡模式</CardTitle>
        </CardHeader>
        <CardContent className="space-y-3">
          <p className="text-sm text-muted-foreground">
            优先级模式：按 priority 顺序使用凭据；均衡负载：在可用凭据间轮换分摊请求。
          </p>
          <div className="flex gap-2">
            <Button
              variant={currentMode === 'priority' ? 'default' : 'outline'}
              size="sm"
              disabled={isSettingLb}
              onClick={() => handleSetMode('priority')}
            >
              优先级模式
            </Button>
            <Button
              variant={currentMode === 'balanced' ? 'default' : 'outline'}
              size="sm"
              disabled={isSettingLb}
              onClick={() => handleSetMode('balanced')}
            >
              均衡负载
            </Button>
          </div>
        </CardContent>
      </Card>

      {/* 服务信息（只读） */}
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base">服务信息</CardTitle>
        </CardHeader>
        <CardContent className="py-0">
          <Row label="监听地址" value={`${config.host}:${config.port}`} mono />
          <Row label="区域 (region)" value={config.region} mono />
          <Row label="TLS 后端" value={config.tlsBackend} />
          <Row label="默认 endpoint" value={config.defaultEndpoint} mono />
          <Row
            label="可用 endpoints"
            value={config.endpointNames.length > 0 ? config.endpointNames.join(', ') : '—'}
            mono
          />
          {config.configPath && <Row label="配置文件" value={config.configPath} mono />}
        </CardContent>
      </Card>

      {/* 版本伪装信息（只读） */}
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base">客户端伪装</CardTitle>
        </CardHeader>
        <CardContent className="py-0">
          <Row label="Kiro 版本" value={config.kiroVersion} mono />
          <Row label="系统版本" value={config.systemVersion} mono />
          <Row label="Node 版本" value={config.nodeVersion} mono />
          <Row label="提取 thinking" value={<BoolBadge on={config.extractThinking} />} />
        </CardContent>
      </Card>

      {/* 防关联 / 限流（只读） */}
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base">防关联 / 限流</CardTitle>
        </CardHeader>
        <CardContent className="py-0">
          <Row label="冷却机制" value={<BoolBadge on={config.cooldownEnabled} />} />
          <Row label="速率限制" value={<BoolBadge on={config.rateLimitEnabled} />} />
          <Row label="会话亲和性" value={<BoolBadge on={config.affinityEnabled} />} />
          <Row label="每日上限" value={config.rateLimitDailyMax > 0 ? config.rateLimitDailyMax : '无限制'} />
          <Row label="最小请求间隔" value={`${config.rateLimitMinIntervalMs} ms`} />
        </CardContent>
      </Card>

      {/* 网络 / 上号（只读） */}
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base">网络与上号</CardTitle>
        </CardHeader>
        <CardContent className="py-0">
          <Row
            label="全局代理"
            value={config.hasProxy ? <span className="font-mono text-xs">{config.proxyUrl || '已配置'}</span> : <BoolBadge on={false} offText="未配置" />}
          />
          <Row label="Admin Key" value={<BoolBadge on={config.hasAdminKey} onText="已设置" offText="未设置" />} />
          <Row
            label="上号回调模式"
            value={
              <Badge variant="outline">
                {config.callbackMode === 'remote' ? '远程（公网回调）' : '本地（临时端口）'}
              </Badge>
            }
          />
          {config.callbackBaseUrl && <Row label="回调地址" value={config.callbackBaseUrl} mono />}
        </CardContent>
      </Card>

      <p className="text-xs text-muted-foreground">
        除负载均衡模式外，其余配置项在服务端配置文件中修改后重启生效。敏感字段（密钥、密码）已脱敏不在此显示。
      </p>
    </div>
  )
}
