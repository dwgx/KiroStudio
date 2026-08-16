import { useState } from 'react'
import { toast } from 'sonner'
import { useTranslation } from 'react-i18next'
import { Check, Copy } from 'lucide-react'
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { Callout } from '@/components/ui/callout'
import { storage } from '@/lib/storage'
import { copyToClipboard } from '@/lib/utils'
import { useConfigSnapshot } from '@/hooks/use-credentials'

// curl 示例里的模型名取网关自带模型目录的稳定别名（非硬编码假名）。
const EXAMPLE_MODEL = 'claude-opus-4-6'

export function ConnPage() {
  const { t } = useTranslation()
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
      toast.success(t('connpage.copied'))
      setTimeout(() => setCopied(null), 2000)
    } else {
      toast.error(t('connpage.copyFailed'))
    }
  }

  return (
    <div className="space-y-4">
      <p className="text-sm text-[#888]">{t('connpage.subtitle')}</p>

      <Callout variant="info">
        <div className="flex items-start gap-2">
          <span className="min-w-0 flex-1">{t('connpage.callout.gatewayKey')}</span>
          <Badge
            variant={cfg?.hasApiKey ? 'default' : 'secondary'}
            className="shrink-0 whitespace-nowrap"
          >
            {cfg?.hasApiKey ? t('connpage.keyStatus.set') : t('connpage.keyStatus.unset')}
          </Badge>
        </div>
      </Callout>

      <div className="grid gap-4 md:grid-cols-2">
        <Card>
          <CardHeader className="pb-2">
            <CardTitle className="text-base">{t('connpage.card.anthropic.title')}</CardTitle>
            <CardDescription className="text-xs">
              {t('connpage.card.anthropic.desc')}
            </CardDescription>
          </CardHeader>
          <CardContent className="space-y-3 py-3">
            <InfoRow
              label={t('connpage.field.baseUrl')}
              value={origin}
              copied={copied === 'baseUrl'}
              onCopy={() => handleCopy('baseUrl', origin)}
            />
            <InfoRow
              label={t('connpage.field.apiKey')}
              value={maskedKey || t('connpage.keyEmpty')}
              hint={t('connpage.keyHint')}
              copied={copied === 'apiKey'}
              onCopy={loginKey ? () => handleCopy('apiKey', loginKey) : undefined}
            />
            <CodeBlock
              title={t('connpage.curl.title')}
              code={curlAnthropic}
              copied={copied === 'curlAnth'}
              onCopy={() => handleCopy('curlAnth', curlAnthropic)}
            />
            <CodeBlock
              title={t('connpage.env.title')}
              code={envExports}
              copied={copied === 'envAnth'}
              onCopy={() => handleCopy('envAnth', envExports)}
            />
            <p className="text-[11px] text-[#666]">{t('connpage.modelsHint')}</p>
          </CardContent>
        </Card>

        <Card>
          <CardHeader className="pb-2">
            <CardTitle className="text-base">{t('connpage.card.openai.title')}</CardTitle>
            <CardDescription className="text-xs">
              {t('connpage.card.openai.desc')}
            </CardDescription>
          </CardHeader>
          <CardContent className="space-y-3 py-3">
            <InfoRow
              label={t('connpage.field.baseUrl')}
              value={origin}
              copied={copied === 'baseUrlOa'}
              onCopy={() => handleCopy('baseUrlOa', origin)}
            />
            <InfoRow
              label={t('connpage.field.apiKey')}
              value={maskedKey || t('connpage.keyEmpty')}
              copied={copied === 'apiKeyOa'}
              onCopy={loginKey ? () => handleCopy('apiKeyOa', loginKey) : undefined}
            />
            <CodeBlock
              title={t('connpage.curl.title')}
              code={curlOpenAi}
              copied={copied === 'curlOa'}
              onCopy={() => handleCopy('curlOa', curlOpenAi)}
            />
            <p className="text-[11px] text-[#666]">{t('connpage.modelsHint')}</p>
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
  const { t } = useTranslation()
  return (
    <Button
      variant="ghost"
      size="sm"
      className="h-7 gap-1 px-2 text-xs"
      onClick={onClick}
    >
      {copied ? <Check className="h-3.5 w-3.5 text-emerald-400" /> : <Copy className="h-3.5 w-3.5" />}
      {copied ? t('connpage.copied') : t('connpage.copy')}
    </Button>
  )
}
