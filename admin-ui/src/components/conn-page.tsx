import { useState } from 'react'
import { toast } from 'sonner'
import { Check, Copy } from 'lucide-react'
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { Callout } from '@/components/ui/callout'
import { storage } from '@/lib/storage'
import { copyToClipboard } from '@/lib/utils'
import { useConfigSnapshot } from '@/hooks/use-credentials'

// 文案常量：i18n 键名已内联为 Record 键，主会话按键名补 en/zh/ja 三语，此处中文直写。
const zh = {
  'connpage.subtitle': '新客户端接入所需的地址与密钥，一键复制即可使用。',
  'connpage.callout.gatewayKey':
    '客户端实际使用的是网关密钥（sk- 前缀，config.json 中 apiKey 字段），其明文不回显面板：仅在首次启动的打印信息中可见，可在「设置 → 网络」更新。下方 API Key 为登录面板时输入的密钥。',
  'connpage.keyStatus.set': '网关密钥已设置',
  'connpage.keyStatus.unset': '网关密钥未设置',
  'connpage.field.baseUrl': 'Base URL',
  'connpage.field.apiKey': 'API Key',
  'connpage.keyHint': '登录面板时输入的密钥（面板不存网关密钥原文）',
  'connpage.keyEmpty': '未获取到密钥（请确认已登录）',
  'connpage.curl.title': 'curl 最小示例',
  'connpage.env.title': '环境变量示例',
  'connpage.modelsHint': '可用模型见 GET /v1/models',
  'connpage.card.anthropic.title': 'Anthropic 兼容',
  'connpage.card.anthropic.desc': 'Claude Code / Claude SDK / Anthropic 系客户端',
  'connpage.card.openai.title': 'OpenAI 兼容',
  'connpage.card.openai.desc': '/v1/chat/completions（与 /v1/responses）',
  'connpage.copy': '复制',
  'connpage.copied': '已复制',
  'connpage.copyFailed': '复制失败，请手动选择',
} as const

// curl 示例里的模型名取网关自带模型目录的稳定别名（非硬编码假名）。
const EXAMPLE_MODEL = 'claude-opus-4-6'

export function ConnPage() {
  const { data: cfg } = useConfigSnapshot()
  const [copied, setCopied] = useState<string | null>(null)

  // 面板与网关同源同端口（/admin 与 /v1 由同一 axum 应用服务），
  // 生产环境直接用 window.location.origin 即客户端需要的 Base URL。
  const origin = window.location.origin
  const loginKey = storage.getApiKey() ?? ''
  const apiKey = loginKey || '<API_KEY>'
  const maskedKey = loginKey
    ? loginKey.length <= 8
      ? '••••'
      : `${loginKey.slice(0, 6)}••••••${loginKey.slice(-4)}`
    : ''

  const curlAnthropic = [
    `curl ${origin}/v1/messages \\`,
    `  -H "x-api-key: ${apiKey}" \\`,
    `  -H "anthropic-version: 2023-06-01" \\`,
    `  -H "content-type: application/json" \\`,
    `  -d '{"model":"${EXAMPLE_MODEL}","max_tokens":1024,"messages":[{"role":"user","content":"hi"}]}'`,
  ].join('\n')

  const curlOpenAi = [
    `curl ${origin}/v1/chat/completions \\`,
    `  -H "Authorization: Bearer ${apiKey}" \\`,
    `  -H "content-type: application/json" \\`,
    `  -d '{"model":"${EXAMPLE_MODEL}","messages":[{"role":"user","content":"hi"}]}'`,
  ].join('\n')

  const envExports = [
    `export ANTHROPIC_BASE_URL=${origin}`,
    `export ANTHROPIC_AUTH_TOKEN=${apiKey}`,
  ].join('\n')

  const handleCopy = async (label: string, text: string) => {
    const ok = await copyToClipboard(text)
    if (ok) {
      setCopied(label)
      toast.success(zh['connpage.copied'])
      setTimeout(() => setCopied(null), 2000)
    } else {
      toast.error(zh['connpage.copyFailed'])
    }
  }

  return (
    <div className="space-y-4">
      <p className="text-sm text-[#888]">{zh['connpage.subtitle']}</p>

      <Callout variant="info">
        <div className="flex items-start gap-2">
          <span className="min-w-0 flex-1">{zh['connpage.callout.gatewayKey']}</span>
          <Badge
            variant={cfg?.hasApiKey ? 'default' : 'secondary'}
            className="shrink-0 whitespace-nowrap"
          >
            {cfg?.hasApiKey ? zh['connpage.keyStatus.set'] : zh['connpage.keyStatus.unset']}
          </Badge>
        </div>
      </Callout>

      <div className="grid gap-4 md:grid-cols-2">
        <Card>
          <CardHeader className="pb-2">
            <CardTitle className="text-base">{zh['connpage.card.anthropic.title']}</CardTitle>
            <CardDescription className="text-xs">
              {zh['connpage.card.anthropic.desc']}
            </CardDescription>
          </CardHeader>
          <CardContent className="space-y-3 py-3">
            <InfoRow
              label={zh['connpage.field.baseUrl']}
              value={origin}
              copied={copied === 'baseUrl'}
              onCopy={() => handleCopy('baseUrl', origin)}
            />
            <InfoRow
              label={zh['connpage.field.apiKey']}
              value={maskedKey || zh['connpage.keyEmpty']}
              hint={zh['connpage.keyHint']}
              copied={copied === 'apiKey'}
              onCopy={loginKey ? () => handleCopy('apiKey', loginKey) : undefined}
            />
            <CodeBlock
              title={zh['connpage.curl.title']}
              code={curlAnthropic}
              copied={copied === 'curlAnth'}
              onCopy={() => handleCopy('curlAnth', curlAnthropic)}
            />
            <CodeBlock
              title={zh['connpage.env.title']}
              code={envExports}
              copied={copied === 'envAnth'}
              onCopy={() => handleCopy('envAnth', envExports)}
            />
            <p className="text-[11px] text-[#666]">{zh['connpage.modelsHint']}</p>
          </CardContent>
        </Card>

        <Card>
          <CardHeader className="pb-2">
            <CardTitle className="text-base">{zh['connpage.card.openai.title']}</CardTitle>
            <CardDescription className="text-xs">
              {zh['connpage.card.openai.desc']}
            </CardDescription>
          </CardHeader>
          <CardContent className="space-y-3 py-3">
            <InfoRow
              label={zh['connpage.field.baseUrl']}
              value={origin}
              copied={copied === 'baseUrlOa'}
              onCopy={() => handleCopy('baseUrlOa', origin)}
            />
            <InfoRow
              label={zh['connpage.field.apiKey']}
              value={maskedKey || zh['connpage.keyEmpty']}
              copied={copied === 'apiKeyOa'}
              onCopy={loginKey ? () => handleCopy('apiKeyOa', loginKey) : undefined}
            />
            <CodeBlock
              title={zh['connpage.curl.title']}
              code={curlOpenAi}
              copied={copied === 'curlOa'}
              onCopy={() => handleCopy('curlOa', curlOpenAi)}
            />
            <p className="text-[11px] text-[#666]">{zh['connpage.modelsHint']}</p>
          </CardContent>
        </Card>
      </div>
    </div>
  )
}

interface InfoRowProps {
  label: string
  value: string
  hint?: string
  copied: boolean
  onCopy?: () => void
}

function InfoRow({ label, value, hint, copied, onCopy }: InfoRowProps) {
  return (
    <div>
      <div className="flex items-center justify-between gap-2">
        <span className="text-xs text-[#888]">{label}</span>
        {onCopy && <CopyButton copied={copied} onClick={onCopy} />}
      </div>
      <div className="mt-1 flex items-center gap-2 rounded bg-secondary/40 px-2.5 py-1.5">
        <code className="min-w-0 flex-1 break-all font-mono text-xs">{value}</code>
      </div>
      {hint && <p className="mt-1 text-[11px] text-[#666]">{hint}</p>}
    </div>
  )
}

interface CodeBlockProps {
  title: string
  code: string
  copied: boolean
  onCopy: () => void
}

function CodeBlock({ title, code, copied, onCopy }: CodeBlockProps) {
  return (
    <div>
      <div className="flex items-center justify-between gap-2">
        <span className="text-xs text-[#888]">{title}</span>
        <CopyButton copied={copied} onClick={onCopy} />
      </div>
      <pre className="mt-1 overflow-auto whitespace-pre-wrap break-all rounded bg-[#161616] p-3 font-mono text-[11px] leading-relaxed text-[#c9d1d9]">
        {code}
      </pre>
    </div>
  )
}

function CopyButton({ copied, onClick }: { copied: boolean; onClick: () => void }) {
  return (
    <Button
      variant="ghost"
      size="sm"
      className="h-7 gap-1 px-2 text-xs"
      onClick={onClick}
    >
      {copied ? <Check className="h-3.5 w-3.5 text-emerald-400" /> : <Copy className="h-3.5 w-3.5" />}
      {copied ? zh['connpage.copied'] : zh['connpage.copy']}
    </Button>
  )
}
