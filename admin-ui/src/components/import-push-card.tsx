import { useState } from 'react'
import { useTranslation } from 'react-i18next'
import { toast } from 'sonner'
import { ChevronDown, ChevronUp, Copy, Inbox, RefreshCw, ServerCrash } from 'lucide-react'
import type { ImportRecord, ImportStats } from '@/api/ops'
import { copyToClipboard } from '@/lib/utils'
import { Button } from '@/components/ui/button'
import { Callout } from '@/components/ui/callout'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { EmptyState } from '@/components/ui/empty-state'
import { Skeleton } from '@/components/ui/skeleton'
import { StatCard } from '@/components/ui/stat-card'
import { AnimatedNumber } from '@/components/ui/animated-number'

// 外部凭据推送(POST /api/import/keys)可观测卡。
//
// 这个功能相对独立:凭据由外部系统主动推来,不经上号流程,失败原因此前只在容器日志里。
// 本卡把累计计数 + 最近几批明细摆到面板上,让"对方推了什么、成了几个、为什么失败"可自查。
// 数据源是进程级内存计数(后端 common/import_stats.rs),零上游、重启归零,故 10s 轮询足够。
export function ImportPushCard({
  data,
  isLoading,
  onRefresh,
}: {
  data: ImportStats | undefined
  isLoading: boolean
  onRefresh: () => void
}) {
  const { t } = useTranslation()

  return (
    <Card>
      <CardHeader className="flex flex-row items-center justify-between pb-2">
        <CardTitle className="flex items-center gap-2 text-base">
          <Inbox className="h-4 w-4" />
          {t('opspage.import.title')}
          {data && (
            <span
              className={`rounded px-1.5 py-0.5 text-[10px] font-normal ${
                data.enabled
                  ? 'bg-emerald-500/10 text-emerald-400'
                  : 'bg-secondary text-muted-foreground'
              }`}
            >
              {data.enabled ? t('opspage.import.enabled') : t('opspage.import.disabled')}
            </span>
          )}
          {data?.lastAtMs && (
            <span className="text-xs font-normal text-muted-foreground">
              {t('opspage.import.lastAt', { time: formatClock(data.lastAtMs) })}
            </span>
          )}
        </CardTitle>
        <Button variant="ghost" size="sm" onClick={onRefresh} className="h-7 px-2">
          <RefreshCw className="h-3.5 w-3.5" />
        </Button>
      </CardHeader>
      <CardContent>
        {isLoading ? (
          <div className="grid grid-cols-2 gap-3 sm:grid-cols-3 lg:grid-cols-5">
            {Array.from({ length: 5 }).map((_, i) => (
              <Skeleton key={i} className="h-16" />
            ))}
          </div>
        ) : !data ? (
          <EmptyState
            icon={ServerCrash}
            tone="destructive"
            title={t('opspage.import.readFailTitle')}
            description={t('opspage.import.readFailDesc')}
            action={
              <Button variant="outline" size="sm" onClick={onRefresh}>
                {t('opspage.common.retry')}
              </Button>
            }
          />
        ) : (
          <>
            {!data.enabled && (
              <Callout variant="warning" className="mb-3">
                {t('opspage.import.disabledHint')}
              </Callout>
            )}
            <div className="grid grid-cols-2 gap-3 sm:grid-cols-3 lg:grid-cols-5">
              <StatCard
                label={t('opspage.import.stat.pushes')}
                value={<AnimatedNumber value={data.pushes} />}
              />
              <StatCard
                label={t('opspage.import.stat.total')}
                value={<AnimatedNumber value={data.keysTotal} />}
              />
              <StatCard
                label={t('opspage.import.stat.imported')}
                value={<AnimatedNumber value={data.keysImported} />}
                accent={data.keysImported > 0 ? 'success' : 'neutral'}
              />
              <StatCard
                label={t('opspage.import.stat.duplicate')}
                value={<AnimatedNumber value={data.keysDuplicate} />}
              />
              <StatCard
                label={t('opspage.import.stat.failed')}
                value={<AnimatedNumber value={data.keysFailed} />}
                accent={data.keysFailed > 0 ? 'warning' : 'neutral'}
              />
            </div>
            {data.records.length > 0 ? (
              <div className="mt-4 space-y-2">
                <div className="text-xs text-muted-foreground">{t('opspage.import.recent')}</div>
                {data.records.map((rec, idx) => (
                  <ImportRecordRow key={`${rec.atMs}-${idx}`} rec={rec} />
                ))}
              </div>
            ) : (
              data.enabled && (
                <div className="mt-4">
                  <EmptyState
                    icon={Inbox}
                    title={t('opspage.import.never')}
                    description={t('opspage.import.neverHint')}
                  />
                </div>
              )
            )}
          </>
        )}
      </CardContent>
    </Card>
  )
}

// 单批推送明细行。失败项优先展示(后端已按失败优先裁剪),便于直接看到原因。
function ImportRecordRow({ rec }: { rec: ImportRecord }) {
  const { t } = useTranslation()
  const [open, setOpen] = useState(false)
  return (
    <div className="rounded-md border border-[#2e2e2e] bg-[#111]">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex w-full items-center gap-3 px-3 py-2 text-left hover:bg-[#161616]"
      >
        <span className="font-mono text-xs text-muted-foreground">{formatClock(rec.atMs)}</span>
        <span className="flex-1 text-xs text-muted-foreground">
          {t('opspage.import.batchSummary', {
            total: rec.total,
            elapsed: `${(rec.elapsedMs / 1000).toFixed(1)}s`,
          })}
        </span>
        {rec.imported > 0 && (
          <span className="text-xs text-emerald-400">+{rec.imported}</span>
        )}
        {rec.duplicates > 0 && (
          <span className="text-xs text-muted-foreground">={rec.duplicates}</span>
        )}
        {rec.failed > 0 && <span className="text-xs text-amber-400">!{rec.failed}</span>}
        {open ? (
          <ChevronUp className="h-3.5 w-3.5 text-muted-foreground" />
        ) : (
          <ChevronDown className="h-3.5 w-3.5 text-muted-foreground" />
        )}
      </button>
      {open && (
        <div className="space-y-1 border-t border-[#2e2e2e] px-3 py-2">
          {rec.items.map((it, i) => (
            <div key={`${it.key}-${i}`} className="space-y-0.5 py-0.5 text-xs">
              {/* 第一行:处置结果 + 明文 key + 复制 + 落库 id。
                  明文 key:面板走 admin 鉴权,与既有「导出凭据」同防护级别;运维要直接核对/取用
                  刚入池的号,打码反而挡住正事(回给推送方的响应仍打码)。 */}
              <div className="flex items-start gap-2">
                <span
                  className={
                    !it.ok
                      ? 'shrink-0 text-amber-400'
                      : it.duplicate
                        ? 'shrink-0 text-muted-foreground'
                        : 'shrink-0 text-emerald-400'
                  }
                >
                  {!it.ok
                    ? t('opspage.import.itemFail')
                    : it.duplicate
                      ? t('opspage.import.itemDup')
                      : t('opspage.import.itemOk')}
                </span>
                <span
                  className="select-all break-all font-mono text-foreground"
                  title={t('opspage.import.copyKey')}
                >
                  {it.key}
                </span>
                <Button
                  size="sm"
                  variant="ghost"
                  className="h-4 shrink-0 px-1"
                  title={t('opspage.import.copyKey')}
                  onClick={async () => {
                    const ok = await copyToClipboard(it.key)
                    toast[ok ? 'success' : 'error'](
                      t(ok ? 'opspage.import.keyCopied' : 'opspage.log.copyFail'),
                    )
                  }}
                >
                  <Copy className="h-3 w-3" />
                </Button>
                {it.credentialId != null && (
                  <span className="shrink-0 text-muted-foreground">#{it.credentialId}</span>
                )}
              </div>
              {/* 第二行:推送方**发来的**原值 → 我们**落库的**值。
                  分两栏的理由:只看落库值无法区分「对方指定了 us-east-1」与「对方没发、我们探测出
                  us-east-1」。而 93 号那次悄悄落到 endpoint=cli 导致整号不可用,面板上却看不出来。 */}
              <div className="flex flex-wrap items-center gap-x-1.5 gap-y-0.5 pl-[3.2rem] font-mono text-[10px] text-muted-foreground">
                <span className="not-italic">{t('opspage.import.sentLabel')}</span>
                <span className="rounded bg-secondary px-1 py-0.5">
                  region={it.sentRegion ?? '—'}
                </span>
                <span className="rounded bg-secondary px-1 py-0.5">
                  groups=[{(it.sentGroups ?? []).join(',')}]
                </span>
                <span className="rounded bg-secondary px-1 py-0.5">
                  endpoint={it.sentEndpoint ?? 'null'}
                </span>
                {it.ok && (
                  <>
                    <span className="text-muted-foreground/60">→</span>
                    <span className="not-italic">{t('opspage.import.landedLabel')}</span>
                    <span className="rounded bg-primary/10 px-1 py-0.5 text-primary">
                      {it.region ?? '—'}
                    </span>
                    <span
                      className="rounded bg-primary/10 px-1 py-0.5 text-primary"
                      title={t('opspage.import.endpointTitle')}
                    >
                      {it.endpoint ?? t('opspage.import.endpointDefault')}
                    </span>
                  </>
                )}
              </div>
              {it.error && (
                <div className="break-all pl-[3.2rem] text-amber-400/80">{it.error}</div>
              )}
            </div>
          ))}
          {rec.omitted > 0 && (
            <div className="text-xs text-muted-foreground">
              {t('opspage.import.omitted', { count: rec.omitted })}
            </div>
          )}
        </div>
      )}
    </div>
  )
}

// Unix 毫秒 → 本地时钟(HH:MM:SS)。推送是低频事件,只显示时刻即可定位。
function formatClock(ms: number): string {
  return new Date(ms).toLocaleTimeString()
}
