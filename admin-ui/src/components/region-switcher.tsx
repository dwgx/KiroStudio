import { useState } from 'react'
import { useTranslation } from 'react-i18next'
import { useQueryClient } from '@tanstack/react-query'
import { toast } from 'sonner'
import { Globe, Loader2, CheckCircle2, XCircle } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { RegionSelect } from '@/components/ui/region-select'
import type { CredentialRegionProfile } from '@/types/api'
import { cn, extractErrorMessage, extractDiagnosis } from '@/lib/utils'
import { probeCredentialRegions, switchProfileRegion } from '@/api/credentials'
import { subscriptionLabel } from '@/lib/i18n-labels'
import { regionLabel } from '@/lib/regions'

interface RegionSwitcherProps {
  /** 目标凭据 id：探测/切换该号各区域 profile。 */
  credentialId: number
}

/**
 * Profile ARN 区域切换共享组件：探测该账号各 region 的 profile，卡片式单选列表选一个切过去
 * （切对话走哪个上游 profile/端点，非改全局 region）。凭据管理页齿轮设置 + 运维页 CredOpsDialog
 * 复用同一份逻辑与视觉。切换成功后同时 invalidate ['credentials']（凭据页刷新）与
 * ['usage','ratelimit-insights']（运维页号池健康刷新）。
 *
 * 可见性由调用方 gate（external_idp || idc 才渲染）——本组件不再判断 authMethod。
 */
export function RegionSwitcher({ credentialId }: RegionSwitcherProps) {
  const { t } = useTranslation()
  // 探测结果（null=未探测）+ 加载/错误态 + 正在切换的 ARN + 自定义 region 手填值。
  const [regions, setRegions] = useState<CredentialRegionProfile[] | null>(null)
  const [regionsLoading, setRegionsLoading] = useState(false)
  const [regionsError, setRegionsError] = useState<string | null>(null)
  const [switchingArn, setSwitchingArn] = useState<string | null>(null)
  const [customRegion, setCustomRegion] = useState('')

  const queryClient = useQueryClient()

  // 探测该账号各 region 的 profile（切 Profile ARN 用）。实时 notification 反馈探测过程：
  // 开始「探测中」→ 找到即报可用数量 / 没找到给详细错误报告（不吞成裸失败）。
  const loadRegions = async () => {
    setRegionsLoading(true)
    setRegionsError(null)
    const pending = toast.loading(t('regionswitcher.toast.probing'))
    try {
      const res = await probeCredentialRegions(credentialId)
      const list = res.regions ?? []
      setRegions(list)
      const usableCount = list.filter((r) => r.usable).length
      if (usableCount > 0) {
        toast.success(
          t('regionswitcher.toast.probeOkUsable', { usableCount, total: list.length }),
          { id: pending },
        )
      } else if (list.length > 0) {
        toast.warning(
          t('regionswitcher.toast.probeOkNoneUsable', { total: list.length }),
          { id: pending },
        )
      } else {
        toast.warning(t('regionswitcher.toast.probeOkEmpty'), { id: pending })
      }
    } catch (err) {
      // 详细错误报告：把后端 bail 的具体原因透传到卡片红框 + toast，不是裸 502。
      const msg = extractErrorMessage(err)
      setRegionsError(msg)
      setRegions(null)
      toast.error(t('regionswitcher.toast.probeFailed', { message: msg }), { id: pending })
    } finally {
      setRegionsLoading(false)
    }
  }

  // 切换当前使用的 Profile ARN（切区域，非改全局 region）。成功后刷新凭据列表 + 号池健康 + 重新探测标记当前项。
  const handleSwitchRegion = async (arn: string) => {
    setSwitchingArn(arn)
    try {
      const res = await switchProfileRegion(credentialId, arn)
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
      queryClient.invalidateQueries({ queryKey: ['usage', 'ratelimit-insights'] })
      toast.success(res.message || t('regionswitcher.toast.switchSuccess'))
      await loadRegions()
    } catch (err) {
      const diag = extractDiagnosis(err)
      toast.error(
        t('regionswitcher.toast.switchFailed', {
          message: diag ? diag.summary : extractErrorMessage(err),
        }),
      )
    } finally {
      setSwitchingArn(null)
    }
  }

  // 自定义 region 切换：用户手填 region，用当前号的 account 构造 ARN 直接切过去（绕候选表，
  // 覆盖冷门 region）。account 从已探测候选或当前 profileArn 提取；构造后走同一 switch（验活可用才生效）。
  const handleCustomRegionSwitch = async () => {
    const region = customRegion.trim()
    if (!region) {
      toast.error(t('regionswitcher.toast.customRegionRequired'))
      return
    }
    // 前端拿不到原始 ARN（安全:只暴露 hasProfileArn），故 account/profile 名从**已探测候选**取。
    // 需先「探测区域」拿到至少一个候选,才能构造同账号的其它 region ARN。
    const sample = regions?.find((r) => r.account && r.arn)
    if (!sample) {
      toast.error(t('regionswitcher.toast.probeFirst'))
      return
    }
    // 构造 ARN：arn:aws:codewhisperer:{region}:{account}:{profileSeg}（同账号同 profile 名，换 region）。
    const profileSeg = sample.arn.split(':').slice(5).join(':')
    const arn = `arn:aws:codewhisperer:${region}:${sample.account}:${profileSeg}`
    await handleSwitchRegion(arn)
  }

  return (
    <div className="space-y-2">
      <div className="flex items-center justify-end">
        <Button
          size="sm"
          variant="outline"
          className="h-8 shrink-0 px-2.5"
          onClick={loadRegions}
          disabled={regionsLoading}
          title={t('regionswitcher.button.probeTitle')}
        >
          {regionsLoading ? (
            <Loader2 className="mr-1 h-4 w-4 animate-spin" />
          ) : (
            <Globe className="mr-1 h-4 w-4" />
          )}
          {regions === null ? t('regionswitcher.button.probe') : t('regionswitcher.button.reprobe')}
        </Button>
      </div>
      {regionsError && (
        <div className="rounded-md border border-red-500/30 bg-red-500/10 px-3 py-2 text-xs text-red-400">
          {t('regionswitcher.error.probeFailed', { message: regionsError })}
        </div>
      )}
      {regions !== null && regions.length === 0 && !regionsLoading && (
        <div className="rounded-md border border-dashed border-border px-3 py-3 text-center text-xs text-muted-foreground">
          {t('regionswitcher.empty.noProfile')}
        </div>
      )}
      {/* 自定义 region：手填任意 region 直接构造 ARN 切过去（绕候选表，覆盖冷门 region）。
          验活可用才真生效（后端 switch 只在 Usable 写回）。 */}
      <div className="flex items-center gap-2 pt-1">
        <RegionSelect
          value={customRegion}
          onChange={setCustomRegion}
          placeholder={t('regionswitcher.placeholder.customRegion')}
          disabled={switchingArn !== null}
          className="flex-1"
          triggerClassName="h-8 text-xs"
        />
        <Button
          size="sm"
          variant="outline"
          className="h-8 shrink-0 px-2.5"
          onClick={handleCustomRegionSwitch}
          disabled={switchingArn !== null || !customRegion.trim()}
          title={t('regionswitcher.button.customSwitchTitle')}
        >
          {t('regionswitcher.button.switchToRegion')}
        </Button>
      </div>
      {regions !== null && regions.length > 0 && (
        <div className="space-y-1.5">
          {[...regions]
            .sort((a, b) => Number(b.usable) - Number(a.usable))
            .map((r) => {
              const isSwitching = switchingArn === r.arn
              return (
                <button
                  key={r.arn}
                  type="button"
                  disabled={!r.usable || switchingArn !== null || r.current}
                  onClick={() => handleSwitchRegion(r.arn)}
                  className={cn(
                    'flex w-full items-start justify-between gap-2 rounded-md border px-3 py-2 text-left transition-colors',
                    r.current
                      ? 'border-emerald-500/50 bg-emerald-500/10'
                      : r.usable
                        ? 'border-input bg-background hover:border-primary hover:bg-accent'
                        : 'border-border bg-secondary/30 opacity-60',
                    (switchingArn !== null && !isSwitching) && 'opacity-50'
                  )}
                >
                  <div className="min-w-0 flex-1">
                    <div className="flex items-center gap-1.5 text-sm font-medium">
                      {r.usable ? (
                        <CheckCircle2 className="h-3.5 w-3.5 shrink-0 text-emerald-400" />
                      ) : (
                        <XCircle className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
                      )}
                      <span className="truncate">{regionLabel(r.region)}</span>
                      {r.current && (
                        <span className="shrink-0 rounded bg-emerald-500/15 px-1 py-0.5 text-[10px] text-emerald-300">
                          {t('regionswitcher.badge.current')}
                        </span>
                      )}
                      {!r.usable && (
                        <span className="shrink-0 rounded bg-white/5 px-1 py-0.5 text-[10px] text-muted-foreground">
                          {t('regionswitcher.badge.notEnabled')}
                        </span>
                      )}
                    </div>
                    <div className="mt-0.5 truncate font-mono text-[10px] text-muted-foreground" title={r.arn}>
                      {r.region}
                      {r.subscriptionTitle ? ` · ${subscriptionLabel(r.subscriptionTitle)}` : ''}
                      {r.account ? ` · ${t('regionswitcher.meta.account', { account: r.account })}` : ''}
                    </div>
                  </div>
                  {isSwitching && <Loader2 className="mt-0.5 h-4 w-4 shrink-0 animate-spin text-primary" />}
                </button>
              )
            })}
        </div>
      )}
    </div>
  )
}
