import { useTranslation } from 'react-i18next'
import type { CredentialStatusItem } from '@/types/api'
import { useHoverCard } from '@/components/overview/credViz'

/** 单个凭据的实时活动信息（由概览页短轮询 /usage/recent 派生）。 */
export interface CellActivity {
  /** 该凭据最近一次请求的 ts_ms（用于 tooltip “最近命中”展示） */
  lastTs: number
  /** 命中脉冲计数：每检测到一次新请求 +1，作为 React key 触发闪动重放 */
  pulse: number
}

export interface StatusHeatmapProps {
  credentials: CredentialStatusItem[]
  /** credential_id -> 实时活动；发现新命中时对应方块闪一下（体现请求流向 / 并发） */
  activity?: Map<number, CellActivity>
  className?: string
}

/**
 * 凭据健康热力图（GitHub 贡献图式网格）：每个凭据一个小方块，
 * 绿=健康 / 红=已禁用 / 琥珀=有失败计数但仍启用。
 * 实时：短轮询 /usage/recent，发现请求打到某凭据 → 该方块一次性“命中脉冲”（克制的高光快速衰减），
 * 多个方块近乎同时脉冲即体现并发。isCurrent 用安静的常驻边缘高光标记（去掉张扬的呼吸+扫光）。
 * hover 弹出「鼠标跟随悬浮卡」展示账户免费字段（#id / email / 鉴权 / 成功·失败次数 / 状态 / 最近命中）；
 * 卡片黏着鼠标走（不再是固定 side 边缘翻转）。正文走共用 CredTooltipBody。
 * 纯 CSS，无图表库；motion-reduce 降级。
 */
export function StatusHeatmap({ credentials, activity, className }: StatusHeatmapProps) {
  const { t } = useTranslation()
  // 鼠标跟随悬浮卡（替代 Radix Tooltip 固定 side 的边缘翻转，卡片黏着鼠标走）。
  const hoverCard = useHoverCard()

  if (credentials.length === 0) {
    return <p className={className}>{t('overviewpage.kpi.totalCreds.empty')}</p>
  }

  const cellClass = (c: CredentialStatusItem): string => {
    if (c.disabled) return 'bg-red-500/80'
    if (c.failureCount > 0) return 'bg-amber-500/80'
    return 'bg-emerald-500/80'
  }

  return (
      <div className={className}>
        <div
          className="grid gap-1"
          style={{ gridTemplateColumns: 'repeat(auto-fill, minmax(14px, 1fr))' }}
        >
          {credentials.map((c) => {
            const act = activity?.get(c.id)
            return (
                  <div
                    key={c.id}
                    onMouseEnter={(e) => hoverCard.show(c, e)}
                    onMouseMove={hoverCard.move}
                    onMouseLeave={hoverCard.hide}
                    className={`relative aspect-square cursor-pointer overflow-hidden rounded-[3px] transition-transform duration-200 ease-out-expo hover:scale-[1.35] hover:z-10 ${cellClass(c)} ${
                      c.isCurrent
                        ? 'z-10 ring-1 ring-primary/70 ring-offset-1 ring-offset-card animate-idle-glow motion-reduce:animate-none'
                        : ''
                    }`}
                  >
                    {/* 命中脉冲：pulse 计数变化 → key 变化 → 重挂载重放一次快速高光（体现请求流入） */}
                    {act && act.pulse > 0 && (
                      <span
                        key={act.pulse}
                        className="pointer-events-none absolute inset-0 rounded-[3px] bg-white/85 animate-hit-flash motion-reduce:hidden"
                      />
                    )}
                  </div>
            )
          })}
        </div>
        {/* 图例 */}
        <div className="mt-3 flex flex-wrap gap-x-4 gap-y-1.5 text-xs text-muted-foreground">
          <span className="flex items-center gap-1.5">
            <span className="h-2.5 w-2.5 rounded-[3px] bg-emerald-500/80" /> {t('overviewpage.health.healthy')}
          </span>
          <span className="flex items-center gap-1.5">
            <span className="h-2.5 w-2.5 rounded-[3px] bg-amber-500/80" /> {t('overviewpage.health.withFailure')}
          </span>
          <span className="flex items-center gap-1.5">
            <span className="h-2.5 w-2.5 rounded-[3px] bg-red-500/80" /> {t('overviewpage.health.disabled')}
          </span>
          <span className="flex items-center gap-1.5">
            <span className="h-2.5 w-2.5 rounded-[3px] bg-transparent ring-1 ring-primary/70" /> {t('overviewpage.legend.currentActive')}
          </span>
        </div>
      {/* 鼠标跟随悬浮卡（正文 CredTooltipBody 不变，仅定位改为黏鼠标） */}
      {hoverCard.render((id) => activity?.get(id))}
      </div>
  )
}
