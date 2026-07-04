import { useEffect, useMemo, useState } from 'react'
import { toast } from 'sonner'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import { useConfigSnapshot, useUpdateConfig } from '@/hooks/use-credentials'
import { extractErrorMessage } from '@/lib/utils'
import { RegionSelect } from '@/components/ui/region-select'
import { NumberStepper } from '@/components/ui/number-stepper'
import type { ConfigSnapshotResponse, UpdateConfigRequest } from '@/types/api'

// 可编辑表单的本地状态（字符串化便于受控输入）
interface FormState {
  host: string
  port: string
  region: string
  kiroVersion: string
  systemVersion: string
  nodeVersion: string
  tlsBackend: string
  loadBalancingMode: string
  defaultEndpoint: string
  extractThinking: boolean
  cooldownEnabled: boolean
  rateLimitEnabled: boolean
  rateLimitDailyMax: string
  rateLimitMinIntervalMs: string
  affinityEnabled: boolean
  proxyUrl: string
  callbackBaseUrl: string
  // 反代安全（批次3）：列表用换行分隔的多行文本承载
  corsAllowedOrigins: string
  ipAllowlist: string
  trustForwardedHeader: boolean
  ingressRateLimitPerMin: string
  maxBodyBytes: string
  // 主动 token 预刷新（批次4.4）
  proactiveTokenRefresh: boolean
  tokenRefreshLeadMinutes: string
  tokenRefreshIntervalSecs: string
}

// 多行文本 <-> 字符串列表（去空白、去空行）
function linesToList(s: string): string[] {
  return s
    .split('\n')
    .map((l) => l.trim())
    .filter((l) => l.length > 0)
}

function listToLines(list: string[]): string {
  return list.join('\n')
}

// 比较两个字符串列表是否等价（顺序敏感）
function sameList(a: string[], b: string[]): boolean {
  return a.length === b.length && a.every((v, i) => v === b[i])
}

function toForm(c: ConfigSnapshotResponse): FormState {
  return {
    host: c.host,
    port: String(c.port),
    region: c.region,
    kiroVersion: c.kiroVersion,
    systemVersion: c.systemVersion,
    nodeVersion: c.nodeVersion,
    tlsBackend: c.tlsBackend,
    loadBalancingMode: c.loadBalancingMode,
    defaultEndpoint: c.defaultEndpoint,
    extractThinking: c.extractThinking,
    cooldownEnabled: c.cooldownEnabled,
    rateLimitEnabled: c.rateLimitEnabled,
    rateLimitDailyMax: String(c.rateLimitDailyMax),
    rateLimitMinIntervalMs: String(c.rateLimitMinIntervalMs),
    affinityEnabled: c.affinityEnabled,
    proxyUrl: c.proxyUrl ?? '',
    callbackBaseUrl: c.callbackBaseUrl ?? '',
    corsAllowedOrigins: listToLines(c.corsAllowedOrigins ?? []),
    ipAllowlist: listToLines(c.ipAllowlist ?? []),
    trustForwardedHeader: c.trustForwardedHeader,
    ingressRateLimitPerMin: String(c.ingressRateLimitPerMin),
    maxBodyBytes: String(c.maxBodyBytes),
    proactiveTokenRefresh: c.proactiveTokenRefresh,
    tokenRefreshLeadMinutes: String(c.tokenRefreshLeadMinutes),
    tokenRefreshIntervalSecs: String(c.tokenRefreshIntervalSecs),
  }
}

// 一行可编辑/只读项布局
function Field({ label, hint, children }: { label: string; hint?: string; children: React.ReactNode }) {
  return (
    <div className="flex items-start justify-between gap-4 py-3 border-b last:border-0">
      <div className="shrink-0 min-w-[40%]">
        <div className="text-sm">{label}</div>
        {hint && <div className="text-xs text-muted-foreground mt-0.5">{hint}</div>}
      </div>
      <div className="flex-1 flex justify-end">{children}</div>
    </div>
  )
}

function ReadonlyRow({ label, value, mono }: { label: string; value: React.ReactNode; mono?: boolean }) {
  return (
    <div className="flex items-start justify-between gap-4 py-2 border-b last:border-0">
      <span className="text-sm text-muted-foreground shrink-0">{label}</span>
      <span className={`text-sm text-right break-all ${mono ? 'font-mono text-xs' : ''}`}>{value}</span>
    </div>
  )
}

export function SettingsPage() {
  const { data: config, isLoading, error, refetch } = useConfigSnapshot()
  const { mutate: save, isPending: isSaving } = useUpdateConfig()

  const [form, setForm] = useState<FormState | null>(null)

  // 配置加载/刷新后，重置表单基线
  useEffect(() => {
    if (config) setForm(toForm(config))
  }, [config])

  const set = <K extends keyof FormState>(key: K, value: FormState[K]) =>
    setForm((prev) => (prev ? { ...prev, [key]: value } : prev))

  // 计算与基线的差异，只提交改动的字段
  const diff = useMemo<UpdateConfigRequest>(() => {
    if (!config || !form) return {}
    const d: UpdateConfigRequest = {}
    if (form.host.trim() !== config.host) d.host = form.host.trim()
    const port = Number(form.port)
    if (Number.isFinite(port) && port !== config.port) d.port = port
    if (form.region.trim() !== config.region) d.region = form.region.trim()
    if (form.kiroVersion.trim() !== config.kiroVersion) d.kiroVersion = form.kiroVersion.trim()
    if (form.systemVersion.trim() !== config.systemVersion) d.systemVersion = form.systemVersion.trim()
    if (form.nodeVersion.trim() !== config.nodeVersion) d.nodeVersion = form.nodeVersion.trim()
    if (form.tlsBackend !== config.tlsBackend) d.tlsBackend = form.tlsBackend
    if (form.loadBalancingMode !== config.loadBalancingMode) d.loadBalancingMode = form.loadBalancingMode
    if (form.defaultEndpoint.trim() !== config.defaultEndpoint) d.defaultEndpoint = form.defaultEndpoint.trim()
    if (form.extractThinking !== config.extractThinking) d.extractThinking = form.extractThinking
    if (form.cooldownEnabled !== config.cooldownEnabled) d.cooldownEnabled = form.cooldownEnabled
    if (form.rateLimitEnabled !== config.rateLimitEnabled) d.rateLimitEnabled = form.rateLimitEnabled
    const daily = Number(form.rateLimitDailyMax)
    if (Number.isFinite(daily) && daily !== config.rateLimitDailyMax) d.rateLimitDailyMax = daily
    const interval = Number(form.rateLimitMinIntervalMs)
    if (Number.isFinite(interval) && interval !== config.rateLimitMinIntervalMs) d.rateLimitMinIntervalMs = interval
    if (form.affinityEnabled !== config.affinityEnabled) d.affinityEnabled = form.affinityEnabled
    if (form.proxyUrl.trim() !== (config.proxyUrl ?? '')) d.proxyUrl = form.proxyUrl.trim()
    if (form.callbackBaseUrl.trim() !== (config.callbackBaseUrl ?? '')) d.callbackBaseUrl = form.callbackBaseUrl.trim()
    // 反代安全
    const origins = linesToList(form.corsAllowedOrigins)
    if (!sameList(origins, config.corsAllowedOrigins ?? [])) d.corsAllowedOrigins = origins
    const allowlist = linesToList(form.ipAllowlist)
    if (!sameList(allowlist, config.ipAllowlist ?? [])) d.ipAllowlist = allowlist
    if (form.trustForwardedHeader !== config.trustForwardedHeader) d.trustForwardedHeader = form.trustForwardedHeader
    const ingress = Number(form.ingressRateLimitPerMin)
    if (Number.isFinite(ingress) && ingress !== config.ingressRateLimitPerMin) d.ingressRateLimitPerMin = ingress
    const maxBody = Number(form.maxBodyBytes)
    if (Number.isFinite(maxBody) && maxBody !== config.maxBodyBytes) d.maxBodyBytes = maxBody
    // 主动 token 预刷新
    if (form.proactiveTokenRefresh !== config.proactiveTokenRefresh) d.proactiveTokenRefresh = form.proactiveTokenRefresh
    const lead = Number(form.tokenRefreshLeadMinutes)
    if (Number.isFinite(lead) && lead !== config.tokenRefreshLeadMinutes) d.tokenRefreshLeadMinutes = lead
    const interval2 = Number(form.tokenRefreshIntervalSecs)
    if (Number.isFinite(interval2) && interval2 !== config.tokenRefreshIntervalSecs) d.tokenRefreshIntervalSecs = interval2
    return d
  }, [config, form])

  const dirty = Object.keys(diff).length > 0

  const handleSave = () => {
    if (!dirty) return
    save(diff, {
      onSuccess: (resp) => {
        if (resp.restartRequired) {
          toast.warning(resp.message, {
            description: `需重启字段：${resp.restartFields.join('、')}`,
            duration: 8000,
          })
        } else {
          toast.success(resp.message)
        }
        refetch()
      },
      onError: (err) => toast.error(extractErrorMessage(err)),
    })
  }

  const handleReset = () => {
    if (config) setForm(toForm(config))
  }

  if (isLoading || !form) {
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

  const inputCls = 'max-w-[260px] text-right'

  return (
    <div className="space-y-6 pb-24">
      <div className="flex items-center justify-between">
        <h2 className="text-xl font-semibold">设置</h2>
        <Button variant="outline" size="sm" onClick={() => refetch()} disabled={isSaving}>
          刷新
        </Button>
      </div>

      {/* 负载均衡（立即生效） */}
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base">负载均衡模式</CardTitle>
        </CardHeader>
        <CardContent className="space-y-3">
          <p className="text-sm text-muted-foreground">
            优先级模式：按 priority 顺序使用凭据；均衡负载：在可用凭据间轮换分摊请求。此项保存后立即生效。
          </p>
          <div className="flex gap-2">
            <Button
              variant={form.loadBalancingMode === 'priority' ? 'default' : 'outline'}
              size="sm"
              onClick={() => set('loadBalancingMode', 'priority')}
            >
              优先级模式
            </Button>
            <Button
              variant={form.loadBalancingMode === 'balanced' ? 'default' : 'outline'}
              size="sm"
              onClick={() => set('loadBalancingMode', 'balanced')}
            >
              均衡负载
            </Button>
          </div>
        </CardContent>
      </Card>

      {/* 服务信息（需重启） */}
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base">服务信息</CardTitle>
        </CardHeader>
        <CardContent className="py-0">
          <Field label="监听地址 host" hint="需重启生效">
            <Input className={inputCls} value={form.host} onChange={(e) => set('host', e.target.value)} />
          </Field>
          <Field label="端口 port" hint="需重启生效">
            <NumberStepper value={Number(form.port) || 0} onChange={(v) => set('port', String(v))} min={1} max={65535} className="w-28" aria-label="端口" />
          </Field>
          <Field label="区域 region" hint="需重启生效">
            <div className="w-[260px]">
              <RegionSelect value={form.region} onChange={(v) => set('region', v)} />
            </div>
          </Field>
          <Field label="TLS 后端" hint="需重启生效">
            <div className="flex gap-2">
              <Button variant={form.tlsBackend === 'rustls' ? 'default' : 'outline'} size="sm" onClick={() => set('tlsBackend', 'rustls')}>
                rustls
              </Button>
              <Button variant={form.tlsBackend === 'native-tls' ? 'default' : 'outline'} size="sm" onClick={() => set('tlsBackend', 'native-tls')}>
                native-tls
              </Button>
            </div>
          </Field>
          <Field label="默认 endpoint" hint={`可用：${config.endpointNames.join(', ') || '—'}（需重启生效）`}>
            <Input className={inputCls} value={form.defaultEndpoint} onChange={(e) => set('defaultEndpoint', e.target.value)} />
          </Field>
          {config.configPath && <ReadonlyRow label="配置文件" value={config.configPath} mono />}
        </CardContent>
      </Card>

      {/* 客户端伪装（需重启） */}
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base">客户端伪装</CardTitle>
        </CardHeader>
        <CardContent className="py-0">
          <Field label="Kiro 版本" hint="需重启生效">
            <Input className={inputCls} value={form.kiroVersion} onChange={(e) => set('kiroVersion', e.target.value)} />
          </Field>
          <Field label="系统版本" hint="需重启生效">
            <Input className={inputCls} value={form.systemVersion} onChange={(e) => set('systemVersion', e.target.value)} />
          </Field>
          <Field label="Node 版本" hint="需重启生效">
            <Input className={inputCls} value={form.nodeVersion} onChange={(e) => set('nodeVersion', e.target.value)} />
          </Field>
          <Field label="提取 thinking" hint="非流式响应解析 thinking 块（需重启生效）">
            <Switch checked={form.extractThinking} onCheckedChange={(v) => set('extractThinking', v)} />
          </Field>
        </CardContent>
      </Card>

      {/* 防关联 / 限流（需重启） */}
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base">防关联 / 限流</CardTitle>
        </CardHeader>
        <CardContent className="py-0">
          <Field label="冷却机制" hint="失败后短暂跳过该凭据（需重启生效）">
            <Switch checked={form.cooldownEnabled} onCheckedChange={(v) => set('cooldownEnabled', v)} />
          </Field>
          <Field label="速率限制" hint="拟人节奏：每日上限 + 请求间隔（需重启生效）">
            <Switch checked={form.rateLimitEnabled} onCheckedChange={(v) => set('rateLimitEnabled', v)} />
          </Field>
          <Field label="每日上限" hint="0 表示无限制（需重启生效）">
            <NumberStepper value={Number(form.rateLimitDailyMax) || 0} onChange={(v) => set('rateLimitDailyMax', String(v))} min={0} step={10} className="w-28" disabled={!form.rateLimitEnabled} aria-label="每日上限" />
          </Field>
          <Field label="最小请求间隔 (ms)" hint="需重启生效">
            <NumberStepper value={Number(form.rateLimitMinIntervalMs) || 0} onChange={(v) => set('rateLimitMinIntervalMs', String(v))} min={0} step={100} className="w-28" disabled={!form.rateLimitEnabled} aria-label="最小请求间隔" />
          </Field>
          <Field label="会话亲和性" hint="同一会话尽量复用同一凭据（需重启生效）">
            <Switch checked={form.affinityEnabled} onCheckedChange={(v) => set('affinityEnabled', v)} />
          </Field>
        </CardContent>
      </Card>

      {/* 网络与上号（需重启） */}
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base">网络与上号</CardTitle>
        </CardHeader>
        <CardContent className="py-0">
          <Field label="全局代理" hint="http(s)://host:port 或 socks5://host:port，留空清除（需重启生效）">
            <Input className="max-w-[260px] font-mono text-xs" value={form.proxyUrl} onChange={(e) => set('proxyUrl', e.target.value)} placeholder="未配置" />
          </Field>
          <Field
            label="上号回调地址"
            hint="远程模式：浏览器回调打到此地址。服务器部署必须配置，否则远程浏览器上号失败。留空回退本地模式（需重启生效）"
          >
            <Input className="max-w-[260px] font-mono text-xs" value={form.callbackBaseUrl} onChange={(e) => set('callbackBaseUrl', e.target.value)} placeholder="http://host:port" />
          </Field>
          <ReadonlyRow
            label="当前回调模式"
            value={
              <Badge variant="outline">
                {config.callbackMode === 'remote' ? '远程（公网回调）' : '本地（临时端口）'}
              </Badge>
            }
          />
          <ReadonlyRow label="Admin Key" value={<Badge variant={config.hasAdminKey ? 'default' : 'secondary'}>{config.hasAdminKey ? '已设置' : '未设置'}</Badge>} />
        </CardContent>
      </Card>

      {/* 反代安全（需重启） */}
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base">反代安全</CardTitle>
        </CardHeader>
        <CardContent className="py-0">
          <Field
            label="CORS 允许来源"
            hint="每行一个来源，如 https://app.example.com。留空=允许任意来源（公开 API，需重启生效）"
          >
            <textarea
              className="flex min-h-[72px] w-full max-w-[260px] rounded-md border border-input bg-background px-3 py-2 font-mono text-xs ring-offset-background placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
              value={form.corsAllowedOrigins}
              onChange={(e) => set('corsAllowedOrigins', e.target.value)}
              placeholder="留空=允许任意来源"
              spellCheck={false}
            />
          </Field>
          <Field
            label="IP 白名单"
            hint="每行一条 CIDR 或单 IP，如 10.0.0.0/8、127.0.0.1。留空=不限制。非法条目保存时会被拒绝（需重启生效）"
          >
            <textarea
              className="flex min-h-[72px] w-full max-w-[260px] rounded-md border border-input bg-background px-3 py-2 font-mono text-xs ring-offset-background placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
              value={form.ipAllowlist}
              onChange={(e) => set('ipAllowlist', e.target.value)}
              placeholder="留空=不限制"
              spellCheck={false}
            />
          </Field>
          <Field
            label="信任 X-Forwarded-For"
            hint="仅当部署在可信反代（nginx 等）之后才开启，否则客户端可伪造 IP 绕过白名单（需重启生效）"
          >
            <Switch checked={form.trustForwardedHeader} onCheckedChange={(v) => set('trustForwardedHeader', v)} />
          </Field>
          <Field label="入口限流 (次/分钟/IP)" hint="0 表示关闭。超限返回 429（需重启生效）">
            <NumberStepper value={Number(form.ingressRateLimitPerMin) || 0} onChange={(v) => set('ingressRateLimitPerMin', String(v))} min={0} step={10} className="w-28" aria-label="入口限流" />
          </Field>
          <Field label="请求体上限 (字节)" hint="默认 52428800（50MiB）。超限返回 413（需重启生效）">
            <Input className={inputCls} type="number" value={form.maxBodyBytes} onChange={(e) => set('maxBodyBytes', e.target.value)} />
          </Field>
        </CardContent>
      </Card>

      {/* 主动 token 预刷新（需重启） */}
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base">主动 token 预刷新</CardTitle>
        </CardHeader>
        <CardContent className="py-0">
          <Field label="启用预刷新" hint="后台提前刷新将过期的 token，把刷新移出请求热路径、削掉突发（需重启生效）">
            <Switch checked={form.proactiveTokenRefresh} onCheckedChange={(v) => set('proactiveTokenRefresh', v)} />
          </Field>
          <Field label="提前量 (分钟)" hint="token 剩余有效期低于此值即后台刷新（需重启生效）">
            <NumberStepper value={Number(form.tokenRefreshLeadMinutes) || 0} onChange={(v) => set('tokenRefreshLeadMinutes', String(v))} min={0} className="w-28" disabled={!form.proactiveTokenRefresh} aria-label="提前量分钟" />
          </Field>
          <Field label="扫描间隔 (秒)" hint="后台扫描周期，最小 5 秒（需重启生效）">
            <NumberStepper value={Number(form.tokenRefreshIntervalSecs) || 0} onChange={(v) => set('tokenRefreshIntervalSecs', String(v))} min={5} step={5} className="w-28" disabled={!form.proactiveTokenRefresh} aria-label="扫描间隔秒" />
          </Field>
        </CardContent>
      </Card>

      <p className="text-xs text-muted-foreground">
        除负载均衡模式立即生效外，其余字段保存后需重启服务才生效。敏感字段（API/Admin 密钥、代理密码）出于安全不在此显示与修改，请在配置文件中维护。
      </p>

      {/* 底部保存栏：仅覆盖 main 内容区（left-[240px] 避开 240px 侧栏，
          否则会盖住侧栏底部“网关在线”状态条造成重叠）；z-30 低于侧栏 z-40。 */}
      <div className="fixed bottom-0 left-0 right-0 z-30 border-t bg-background/95 px-6 py-3 backdrop-blur md:left-[240px]">
        <div className="mx-auto flex max-w-[1200px] items-center justify-end gap-3">
          <span className="mr-auto text-sm text-muted-foreground">
            {dirty ? `${Object.keys(diff).length} 项改动待保存` : '无改动'}
          </span>
          <Button variant="outline" onClick={handleReset} disabled={!dirty || isSaving}>
            撤销
          </Button>
          <Button onClick={handleSave} disabled={!dirty || isSaving}>
            {isSaving ? '保存中…' : '保存'}
          </Button>
        </div>
      </div>
    </div>
  )
}
