import { useMemo, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { toast } from 'sonner'
import {
  CheckCircle2,
  ChevronDown,
  ChevronUp,
  Copy,
  Inbox,
  KeyRound,
  RefreshCw,
  RotateCw,
  ServerCrash,
  ShieldOff,
} from 'lucide-react'
import { getImportStats, type ImportRecord } from '@/api/ops'
import { updateConfig } from '@/api/credentials'
import { copyToClipboard, extractErrorMessage } from '@/lib/utils'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Callout } from '@/components/ui/callout'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { ConfirmDialog } from '@/components/ui/confirm-dialog'
import { EmptyState } from '@/components/ui/empty-state'
import { Skeleton } from '@/components/ui/skeleton'
import { StatCard } from '@/components/ui/stat-card'
import { ImportPushCard } from '@/components/import-push-card'

type ConfirmAction = 'rotate' | 'disable' | null

function generateSecret(): string {
  const bytes = new Uint8Array(32)
  crypto.getRandomValues(bytes)
  return `relay-${Array.from(bytes, (value) => value.toString(16).padStart(2, '0')).join('')}`
}

export function PushChannelPage() {
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  const [revealedSecret, setRevealedSecret] = useState<string | null>(null)
  const [confirmAction, setConfirmAction] = useState<ConfirmAction>(null)
  const stats = useQuery({
    queryKey: ['import-stats'],
    queryFn: getImportStats,
    refetchInterval: 10000,
  })
  const endpoint = useMemo(
    () => (typeof window === 'undefined' ? '/api/import/push' : `${window.location.origin}/api/import/push`),
    [],
  )
  const configure = useMutation({
    mutationFn: (secret: string) => updateConfig({ relayApiKey: secret }),
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: ['import-stats'] })
    },
  })

  const setSecret = async (secret: string) => {
    try {
      await configure.mutateAsync(secret)
      setRevealedSecret(secret || null)
      setConfirmAction(null)
      toast.success(t(secret ? 'pushchannel.toast.enabled' : 'pushchannel.toast.disabled'))
    } catch (error) {
      toast.error(extractErrorMessage(error))
    }
  }

  const copy = async (value: string) => {
    const ok = await copyToClipboard(value)
    toast[ok ? 'success' : 'error'](t(ok ? 'pushchannel.toast.copied' : 'pushchannel.toast.copyFailed'))
  }

  const enabled = stats.data?.relayEnabled ?? false
  const sampleSecret = revealedSecret ?? '<RELAY_SECRET>'
  const curlSample = [
    `curl -X POST '${endpoint}' \\`,
    `  -H 'Content-Type: application/json' \\`,
    `  -H 'x-relay-secret: ${sampleSecret}' \\`,
    `  -d '{"key":"ksk_xxx","region":"us-east-1","delivery_id":"order-001"}'`,
  ].join('\n')

  return (
    <div className="space-y-6">
      <Card>
        <CardHeader className="flex flex-row items-center justify-between pb-3">
          <div className="space-y-1">
            <CardTitle className="flex items-center gap-2 text-base">
              <Inbox className="h-4 w-4" />
              {t('pushchannel.config.title')}
              <Badge variant={enabled ? 'success' : 'secondary'}>
                {t(enabled ? 'pushchannel.status.enabled' : 'pushchannel.status.disabled')}
              </Badge>
            </CardTitle>
            <p className="text-xs text-muted-foreground">{t('pushchannel.config.description')}</p>
          </div>
          <Button variant="ghost" size="icon" onClick={() => stats.refetch()} title={t('pushchannel.action.refresh')}>
            <RefreshCw className="h-4 w-4" />
          </Button>
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="space-y-1.5">
            <label className="text-xs font-medium text-muted-foreground">{t('pushchannel.config.endpoint')}</label>
            <div className="flex min-w-0 items-center gap-2">
              <code className="min-w-0 flex-1 overflow-x-auto rounded-md border border-[#2e2e2e] bg-[#0d0d0d] px-3 py-2 text-xs text-foreground">
                {endpoint}
              </code>
              <Button variant="outline" size="icon" onClick={() => copy(endpoint)} title={t('pushchannel.action.copyEndpoint')}>
                <Copy className="h-4 w-4" />
              </Button>
            </div>
          </div>

          {revealedSecret && (
            <Callout variant="warning">
              <div className="space-y-2">
                <p>{t('pushchannel.secret.once')}</p>
                <div className="flex min-w-0 items-center gap-2">
                  <code className="min-w-0 flex-1 break-all text-xs text-foreground">{revealedSecret}</code>
                  <Button variant="outline" size="icon" onClick={() => copy(revealedSecret)} title={t('pushchannel.action.copySecret')}>
                    <Copy className="h-4 w-4" />
                  </Button>
                </div>
              </div>
            </Callout>
          )}

          <div className="flex flex-wrap gap-2">
            {!enabled ? (
              <Button onClick={() => setSecret(generateSecret())} disabled={configure.isPending}>
                <KeyRound className="h-4 w-4" />
                {t('pushchannel.action.enable')}
              </Button>
            ) : (
              <>
                <Button variant="outline" onClick={() => setConfirmAction('rotate')} disabled={configure.isPending}>
                  <RotateCw className="h-4 w-4" />
                  {t('pushchannel.action.rotate')}
                </Button>
                <Button variant="destructive" onClick={() => setConfirmAction('disable')} disabled={configure.isPending}>
                  <ShieldOff className="h-4 w-4" />
                  {t('pushchannel.action.disable')}
                </Button>
              </>
            )}
          </div>

          <div className="space-y-1.5">
            <label className="text-xs font-medium text-muted-foreground">{t('pushchannel.config.example')}</label>
            <div className="relative">
              <pre className="overflow-x-auto rounded-md border border-[#2e2e2e] bg-[#0d0d0d] p-3 pr-12 text-xs leading-5 text-muted-foreground">
                {curlSample}
              </pre>
              <Button className="absolute right-2 top-2" variant="ghost" size="icon" onClick={() => copy(curlSample)} title={t('pushchannel.action.copyExample')}>
                <Copy className="h-4 w-4" />
              </Button>
            </div>
          </div>
        </CardContent>
      </Card>

      <ChannelStats
        data={stats.data}
        loading={stats.isLoading}
        onRetry={() => stats.refetch()}
      />

      <ImportPushCard
        data={stats.data}
        isLoading={stats.isLoading}
        onRefresh={() => void stats.refetch()}
      />

      <ConfirmDialog
        open={confirmAction !== null}
        onOpenChange={(open) => !open && setConfirmAction(null)}
        title={t(confirmAction === 'disable' ? 'pushchannel.confirm.disableTitle' : 'pushchannel.confirm.rotateTitle')}
        description={t(confirmAction === 'disable' ? 'pushchannel.confirm.disableDescription' : 'pushchannel.confirm.rotateDescription')}
        confirmLabel={t(confirmAction === 'disable' ? 'pushchannel.action.disable' : 'pushchannel.action.rotate')}
        destructive={confirmAction === 'disable'}
        loading={configure.isPending}
        onConfirm={() => setSecret(confirmAction === 'disable' ? '' : generateSecret())}
      />
    </div>
  )
}

function ChannelStats({
  data,
  loading,
  onRetry,
}: {
  data: Awaited<ReturnType<typeof getImportStats>> | undefined
  loading: boolean
  onRetry: () => void
}) {
  const { t } = useTranslation()
  return (
    <Card>
      <CardHeader className="pb-3">
        <CardTitle className="text-base">{t('pushchannel.records.title')}</CardTitle>
      </CardHeader>
      <CardContent>
        {loading ? (
          <div className="grid grid-cols-2 gap-3 lg:grid-cols-5">
            {Array.from({ length: 5 }).map((_, index) => <Skeleton key={index} className="h-24" />)}
          </div>
        ) : !data ? (
          <EmptyState icon={ServerCrash} tone="destructive" title={t('pushchannel.records.loadFailed')} action={<Button variant="outline" onClick={onRetry}>{t('pushchannel.action.retry')}</Button>} />
        ) : (
          <>
            <div className="grid grid-cols-2 gap-3 lg:grid-cols-5">
              <StatCard label={t('pushchannel.stat.pushes')} value={data.relayPushes} icon={Inbox} />
              <StatCard label={t('pushchannel.stat.total')} value={data.relayKeysTotal} />
              <StatCard label={t('pushchannel.stat.imported')} value={data.relayKeysImported} icon={CheckCircle2} accent={data.relayKeysImported ? 'success' : 'neutral'} />
              <StatCard label={t('pushchannel.stat.duplicates')} value={data.relayKeysDuplicate} />
              <StatCard label={t('pushchannel.stat.failed')} value={data.relayKeysFailed} accent={data.relayKeysFailed ? 'warning' : 'neutral'} />
            </div>
            {data.relayRecords.length ? (
              <div className="mt-4 space-y-2">
                {data.relayRecords.map((record, index) => <RecordRow key={`${record.atMs}-${index}`} record={record} />)}
              </div>
            ) : (
              <EmptyState icon={Inbox} title={t('pushchannel.records.empty')} description={t('pushchannel.records.emptyDescription')} />
            )}
          </>
        )}
      </CardContent>
    </Card>
  )
}

function RecordRow({ record }: { record: ImportRecord }) {
  const { t } = useTranslation()
  const [open, setOpen] = useState(false)
  return (
    <div className="rounded-md border border-[#2e2e2e] bg-[#111]">
      <button type="button" onClick={() => setOpen((value) => !value)} className="flex w-full items-center gap-3 px-3 py-2 text-left hover:bg-[#161616]">
        <span className="font-mono text-xs text-muted-foreground">{new Date(record.atMs).toLocaleString()}</span>
        <span className="flex-1 text-xs text-muted-foreground">{t('pushchannel.records.summary', { total: record.total, seconds: (record.elapsedMs / 1000).toFixed(1) })}</span>
        {record.imported > 0 && <span className="text-xs text-emerald-400">+{record.imported}</span>}
        {record.duplicates > 0 && <span className="text-xs text-muted-foreground">={record.duplicates}</span>}
        {record.failed > 0 && <span className="text-xs text-amber-400">!{record.failed}</span>}
        {open ? <ChevronUp className="h-4 w-4" /> : <ChevronDown className="h-4 w-4" />}
      </button>
      {open && (
        <div className="space-y-2 border-t border-[#2e2e2e] px-3 py-2">
          {record.items.map((item, index) => (
            <div key={`${item.fingerprint}-${index}`} className="space-y-1 text-xs">
              <div className="flex flex-wrap items-center gap-2">
                <Badge variant={!item.ok ? 'warning' : item.duplicate ? 'secondary' : 'success'}>
                  {t(!item.ok ? 'pushchannel.result.failed' : item.duplicate ? 'pushchannel.result.duplicate' : 'pushchannel.result.imported')}
                </Badge>
                <code className="select-all break-all text-foreground">{item.key}</code>
                <Button
                  size="sm"
                  variant="ghost"
                  className="h-5 shrink-0 px-1"
                  title={t('opspage.import.copyKey')}
                  onClick={async () => {
                    const ok = await copyToClipboard(item.key)
                    toast[ok ? 'success' : 'error'](
                      t(ok ? 'opspage.import.keyCopied' : 'pushchannel.toast.copyFailed'),
                    )
                  }}
                >
                  <Copy className="h-3 w-3" />
                </Button>
                <span className="font-mono text-muted-foreground">sha256:{item.fingerprint}</span>
                {item.deliveryId && <span className="font-mono text-muted-foreground">delivery:{item.deliveryId}</span>}
                {item.credentialId != null && <span className="text-muted-foreground">#{item.credentialId}</span>}
              </div>
              <div className="font-mono text-[10px] text-muted-foreground">
                region={item.region ?? '—'} · endpoint={item.endpoint ?? 'default'}
              </div>
              {item.error && <div className="break-all text-amber-400">{item.error}</div>}
            </div>
          ))}
        </div>
      )}
    </div>
  )
}
