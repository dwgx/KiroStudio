import { useMemo, useState } from 'react'
import { useTranslation } from 'react-i18next'
import type { CredentialStatusItem } from '@/types/api'
import type { CellActivity } from '@/components/overview/StatusHeatmap'
import {
  healthOf,
  HEALTH_RGB,
  HEALTH_LABEL_KEYS,
  EmptyPool,
  useHoverCard,
  type Health,
} from '@/components/overview/credViz'
import { useFlip } from '@/hooks/use-flip'
import './glow-grid.css'

export interface GlowGridProps {
  credentials: CredentialStatusItem[]
  /** credential_id -> 实时活动；发现新命中时该核心瞬间点亮再衰减并向相邻核心扩散一圈微光涟漪（体现算力被激活）。 */
  activity?: Map<number, CellActivity>
  className?: string
}

/** 统一圆角：核心本体与各覆盖层（呼吸/命中闪/涟漪）保持一致，避免圆角处露边。 */
const R = 'rounded-[4px]'

// ============================================================
// 密度参数 —— 「绿墙」观感的根因就在这三个数上，改前先读注释。
//
// 原实现是 `minmax(22px, 1fr)` + `auto-fill` 且**无列数上限**：宽屏上一行能塞
// 30 格以上，号池涨到 25+ 份后整块糊成一片密排绿方块（用户原话「密集恐惧症」）。
// 现在三条一起收：格子变大、列数设上限、超出行数折叠。
// ============================================================

/** 单格最小边长（px）。22 → 38：每格看得清，也让同样的号数占更多横向空间、自然产生留白。 */
const CELL_MIN_PX = 38
/** 格间距（px）。必须与下面 grid 的 `gap-2`（0.5rem = 8px）保持一致 —— maxWidth 要按它反算。 */
const GAP_PX = 8
/** 最大列数。宽屏上不再铺满整行；窄屏由 minmax 自然减列。 */
const MAX_COLS = 12
/** 折叠态最多铺几行。 */
const COLLAPSED_ROWS = 4
/**
 * 折叠阈值 = 列上限 × 折叠行数。
 *
 * 为什么用「显示前 N + 折叠」而不是分页 / 内滚：
 * - **分页**：概览页的价值是「一眼看全池」，翻页会把一半状态藏到第二页；且换页等于整墙
 *   重挂载，与 `useFlip` 的平滑重排直接冲突（FLIP 靠对比同 key 节点的前后位置）。
 * - **内滚**：卡片内再套滚动容器要 `overflow:auto`，会**裁掉** hover 的 `scale-[1.18]`
 *   浮起和命中涟漪（涟漪刻意 scale 到 2.6 溢出到邻格），这两个效果当场失效；
 *   而且和外层页面滚动抢滚动链，被裁掉的部分同样看不见，与折叠等价却多一个陷阱。
 * - **折叠**：48 格已覆盖绝大多数池子（现网 25 份），超出时一次点开即恢复全貌，
 *   并把「折叠区里有几个异常号」写在按钮上 —— 默认按健康排序时坏号排在最后，
 *   不提示就会被静默藏起来，那是比密集更严重的问题。
 */
const MAX_VISIBLE = MAX_COLS * COLLAPSED_ROWS

/**
 * 分组色相板（rgb 三元组）。
 *
 * `SOLO_RGB` 单列在外：它是**无分身组的单开号**的颜色，等于保持原观感不变，
 * 小池子看不出任何变化。只有真正成组的分身才吃下面的轮转色相。
 *
 * 刻意避开 amber 与暗红 —— 那两支是 warn / disabled 的语义色，
 * 拿来做分组色会让「哪个号坏了」读不出来。
 */
const SOLO_RGB = HEALTH_RGB.healthy // emerald-500
const GROUP_RGB = [
  '34 211 238', // cyan-400
  '167 139 250', // violet-400
  '244 114 182', // pink-400
  '190 242 100', // lime-300
  '59 130 246', // blue-500
]

/**
 * 呼吸周期。只有「在途 > 0」才呼吸，所以不再按健康态分档 ——
 * 呼吸现在只剩一个语义：这个核心正在跑请求。
 */
const BREATHE_DUR = '2s'

/**
 * 由凭据 id 派生一个确定性的呼吸相位偏移（0~-1 个周期内）。
 * 让同时在途的多个核心此起彼伏而非齐刷刷同步；纯函数无随机，重渲染不跳变。
 */
function breatheDelay(id: number): string {
  // 取 id 的伪散列落到 [0,1)，映射成负延迟（动画像已运行了一段）。
  const frac = ((id * 2654435761) % 1000) / 1000
  return `-${(frac * 2).toFixed(2)}s`
}

/** 32-bit FNV-1a：把组标识散列成稳定整数。与出现顺序无关，故同一账号的色相不随排序模式漂移。 */
function hashStr(s: string): number {
  let h = 0x811c9dc5
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i)
    h = Math.imul(h, 0x01000193)
  }
  return h >>> 0
}

/** 一格的渲染视图：凭据 + 健康态 + 最终基色。 */
interface CellView {
  c: CredentialStatusItem
  h: Health
  /** 该格基色（rgb 三元组）。 */
  rgb: string
}

/**
 * 按分身组聚簇 + 分配色相。
 *
 * **分组键有两级，且两者绝不字符串拼接**（`cloneGroup` 是裸 UUID、`apiKeyHash` 是裸
 * sha256 hex，拼起来会产生假分组）：分别加 `g:` / `k:` 前缀后再比较。
 * 与 `clone-management-card.tsx` 的 `groupClones` 同口径，包括那一遍
 * 「apiKeyHash → cloneGroup 回填」—— 早于 `cloneGroup` 字段入池的父号自己没有组标识，
 * 不回填的话同一个账号会裂成两个色相，面板上像是多了一个账号。
 *
 * **健康三态口径完全复用 `healthOf`**（与 StatusHeatmap 同源），本函数只在它之上
 * 加色相维度：色相只改**健康格**的基色，warn / disabled 仍是各自的语义色。
 * 这条取舍是刻意的 —— 「哪个号坏了」比「它属于哪组」更要紧，所以一组里若有 warn 成员，
 * 色带上会出现一个琥珀色缺口，那是应该看见的信号而不是瑕疵。
 *
 * **聚簇**：保持传入的排序（概览页的 4 种排序模式），但遇到某个多份组的第一个成员时，
 * 把该组全部成员就地连续排出。于是排序意图（谁排前面）不变，而同组一定相邻 ——
 * 不相邻的话色相只是彩色噪点，读不出「哪几个是同一个账号」。
 */
function layoutCells(credentials: CredentialStatusItem[]): { cells: CellView[]; groupCount: number } {
  // 第一遍：apiKeyHash → cloneGroup。同 key 必然同账号，所以该 key 下任何一份有组标识，
  // 就把整个 key 的成员都归到那个组上。
  const groupOfKey = new Map<string, string>()
  for (const c of credentials) {
    if (c.cloneGroup && c.apiKeyHash && !groupOfKey.has(c.apiKeyHash)) {
      groupOfKey.set(c.apiKeyHash, c.cloneGroup)
    }
  }

  // 第二遍：算每份的分组键，并按键归拢（保持传入顺序）。
  const keyOf = new Map<number, string>()
  const byKey = new Map<string, CredentialStatusItem[]>()
  for (const c of credentials) {
    const adopted = c.cloneGroup ?? (c.apiKeyHash ? groupOfKey.get(c.apiKeyHash) : undefined)
    const key = adopted ? `g:${adopted}` : c.apiKeyHash ? `k:${c.apiKeyHash}` : null
    if (!key) continue
    keyOf.set(c.id, key)
    const arr = byKey.get(key) ?? []
    arr.push(c)
    byKey.set(key, arr)
  }

  // 只有「真的有多份」才算一组并吃色相。单份不是分身，给它上色只是凭空多一个色相。
  const multiKeys = [...byKey.entries()].filter(([, m]) => m.length > 1).map(([k]) => k)

  // 色相分配：按键名字典序遍历（与排序模式无关 → 色相稳定），各自取散列偏好位，
  // 被占则线性探测下一个。避免两组撞到同一支色相后被误读成同一个账号。
  const hueOf = new Map<string, string>()
  const taken = new Set<number>()
  for (const k of [...multiKeys].sort()) {
    let idx = hashStr(k) % GROUP_RGB.length
    for (let step = 0; step < GROUP_RGB.length && taken.has(idx); step++) {
      idx = (idx + 1) % GROUP_RGB.length
    }
    taken.add(idx)
    // 组数超过色相板容量时必然复用（`taken` 已满，探测一圈回到原位）：
    // 5 支色相下要 6 组以上才会发生，届时相邻两组仍多半不同色，可接受。
    if (taken.size >= GROUP_RGB.length) taken.clear()
    hueOf.set(k, GROUP_RGB[idx])
  }

  const view = (c: CredentialStatusItem, groupKey: string | null): CellView => {
    const h = healthOf(c)
    const hue = groupKey ? hueOf.get(groupKey) : undefined
    return { c, h, rgb: h === 'healthy' ? (hue ?? SOLO_RGB) : HEALTH_RGB[h] }
  }

  const emitted = new Set<number>()
  const cells: CellView[] = []
  for (const c of credentials) {
    if (emitted.has(c.id)) continue
    const key = keyOf.get(c.id)
    const members = key ? byKey.get(key) : undefined
    if (key && members && members.length > 1) {
      for (const m of members) {
        emitted.add(m.id)
        cells.push(view(m, key))
      }
    } else {
      emitted.add(c.id)
      cells.push(view(c, null))
    }
  }
  return { cells, groupCount: multiKeys.length }
}

/**
 * GlowGrid —— GPU / CUDA 核心阵列（算力墙点阵）。
 *
 * 观感要点：
 * - 阵列：方块核心规整排布，一格 = 一个号。格子 38px、最多 12 列、超 4 行折叠 ——
 *   刻意**不**铺满整行，号多时靠换行与留白而不是加密。
 * - 分组色相：同一分身组共享一支色相并在网格里连续排列，25 份分身一眼看出分属几个账号；
 *   warn / disabled 仍用语义色（坏号优先于分组可读性）。
 * - 底光：健康号静态底光；**只有在途 > 0 的号才呼吸** —— 常驻动画量从「号数」降到
 *   「实际在途数」，这是密集感与 GPU 占用的另一半根因。
 * - 命中：请求流过时对应核心瞬间点亮（白芯 + 基色晕）再衰减，并向相邻核心扩散一圈涟漪。
 *   由真实 activity 事件驱动，不常驻。
 * - 当前活跃：安静的品牌色描边环。
 * - hover：轻微浮起 + 外辉光增强，tooltip 展示免费字段（不含 balance，避免触发上游风控）。
 * 纯 CSS/SVG，无图表库；命中/涟漪用 key 重挂载单次重放；motion-reduce 全面降级为静态色块
 * （静态亮度仍区分在途 / 空闲 / 禁用，降级后不会「看不出状态」）。
 */
export function GlowGrid({ credentials, activity, className }: GlowGridProps) {
  const { t } = useTranslation()
  // 鼠标跟随悬浮卡（替代 Radix Tooltip 固定 side 的边缘翻转，卡片黏着鼠标走）。
  const hoverCard = useHoverCard()
  const [expanded, setExpanded] = useState(false)

  const { cells, groupCount } = useMemo(() => layoutCells(credentials), [credentials])
  const visible = expanded ? cells : cells.slice(0, MAX_VISIBLE)
  const hiddenCount = cells.length - visible.length
  // 折叠区里有几个非健康号：默认健康排序会把坏号排到最后，正好落进折叠区。
  const hiddenAbnormal = cells.slice(visible.length).filter((v) => v.h !== 'healthy').length

  // FLIP 平滑重排:排序/显隐/展开变化时核心从旧位滑到新位。key 必须是**实际渲染顺序**，
  // 否则聚簇导致的位次变化不会触发动画。
  const flipRef = useFlip<HTMLDivElement>([visible.map((v) => v.c.id).join(',')])

  if (credentials.length === 0) {
    return <EmptyPool className={className} />
  }

  return (
    <div className={className}>
        <div
          ref={flipRef}
          className="grid gap-2"
          style={{
            gridTemplateColumns: `repeat(auto-fill, minmax(${CELL_MIN_PX}px, 1fr))`,
            // 列数上限靠 maxWidth 反算：容器最宽 = MAX_COLS 格 + (MAX_COLS-1) 个间距，
            // 于是宽屏上 auto-fill 最多也只塞得进 MAX_COLS 列；窄屏按 minmax 自然减列。
            maxWidth: MAX_COLS * CELL_MIN_PX + (MAX_COLS - 1) * GAP_PX,
          }}
        >
          {visible.map(({ c, h, rgb }) => {
            const act = activity?.get(c.id)
            const lit = h !== 'disabled'
            const inflight = c.inflight ?? 0
            const busy = lit && inflight > 0
            const hit = !!act && act.pulse > 0
            return (
                  <div
                    key={c.id}
                    data-flip-key={c.id}
                    onMouseEnter={(e) => hoverCard.show(c, e)}
                    onMouseMove={hoverCard.move}
                    onMouseLeave={hoverCard.hide}
                    className={`group relative aspect-square cursor-pointer ${R} transition-transform duration-200 ease-out hover:z-10 hover:-translate-y-0.5 hover:scale-[1.18]`}
                    style={{
                      // 核心本体：基色斜面渐变 + 内凹描边，像 die 上一枚微小的算力单元。
                      // 底光的“亮”交给独立层调 opacity（GPU 合成，不重绘本体）。
                      background: lit
                        ? `linear-gradient(150deg, rgb(${rgb} / 0.42), rgb(${rgb} / 0.16))`
                        : `linear-gradient(150deg, rgb(${rgb} / 0.45), rgb(${rgb} / 0.2))`,
                      border: `1px solid rgb(${rgb} / ${lit ? 0.5 : 0.32})`,
                      boxShadow: lit
                        ? `inset 0 1px 0 rgb(255 255 255 / 0.14), inset 0 -1px 2px rgb(0 0 0 / 0.35)`
                        : `inset 0 1px 0 rgb(255 255 255 / 0.03), inset 0 0 6px rgb(0 0 0 / 0.45)`,
                    }}
                  >
                    {/* 底光层（独立层，只动 opacity）：在途才呼吸，空闲是静态底光；禁用号无此层。
                        原实现对每个健康号都常驻呼吸，N 个错相位脉动就是「视觉噪音 ×N」。 */}
                    {lit && (
                      <span
                        className={`${busy ? 'gg-core-breathe' : 'gg-core-idle'} pointer-events-none absolute inset-0 ${R}`}
                        style={{
                          background: `radial-gradient(circle at 50% 42%, rgb(${rgb} / 0.9), rgb(${rgb} / 0.32) 70%, transparent)`,
                          boxShadow: `0 0 6px 0 rgb(${rgb} / ${busy ? 0.5 : 0.32})`,
                          ['--gg-dur' as string]: BREATHE_DUR,
                          ['--gg-delay' as string]: breatheDelay(c.id),
                        }}
                      />
                    )}
                    {/* 顶部一枚白色高光斑（透镜反光），hover 时加强，做出核心的物理厚度。 */}
                    <span className={`pointer-events-none absolute inset-0 overflow-hidden ${R}`}>
                      <span className="absolute inset-x-0.5 top-0.5 h-1/3 rounded-full bg-white/12 blur-[1.5px] transition-opacity duration-200 group-hover:bg-white/28" />
                    </span>
                    {/* hover 外辉光增强（透明→显现，避免常驻炫光）。 */}
                    {lit && (
                      <span
                        className={`pointer-events-none absolute inset-0 ${R} opacity-0 transition-opacity duration-200 group-hover:opacity-100 motion-reduce:transition-none`}
                        style={{ boxShadow: `0 0 12px 1px rgb(${rgb} / 0.6), 0 4px 10px rgb(0 0 0 / 0.45)` }}
                      />
                    )}
                    {/* 当前活跃：安静的品牌色描边环。 */}
                    {c.isCurrent && (
                      <span className={`pointer-events-none absolute inset-0 ${R} ring-1 ring-primary/70 ring-offset-1 ring-offset-card`} />
                    )}
                    {/* 命中点亮闪（不裁剪）：核心被算力激活，白芯 + 基色晕瞬间点亮再衰减，单次重放。 */}
                    {hit && lit && (
                      <span
                        key={`flash-${act!.pulse}`}
                        className={`gg-core-flash pointer-events-none absolute inset-0 ${R} motion-reduce:hidden`}
                        style={{
                          background: `radial-gradient(circle at 50% 42%, rgb(255 255 255 / 0.95), rgb(${rgb} / 0.6) 60%, transparent)`,
                          boxShadow: `0 0 14px 2px rgb(${rgb} / 0.85)`,
                        }}
                      />
                    )}
                    {/* 命中涟漪微光（不裁剪）：自本核心向外扩散一圈细描边并淡出，波及相邻核心。 */}
                    {hit && lit && (
                      <span
                        key={`ripple-${act!.pulse}`}
                        className={`gg-core-ripple pointer-events-none absolute inset-0 ${R} motion-reduce:hidden`}
                        style={{ boxShadow: `0 0 0 1px rgb(${rgb} / 0.9)` }}
                      />
                    )}
                  </div>
            )
          })}
        </div>
        {/* 折叠 / 展开：只有真超出 MAX_VISIBLE 时才出现。 */}
        {(hiddenCount > 0 || expanded) && cells.length > MAX_VISIBLE && (
          <button
            type="button"
            onClick={() => setExpanded((v) => !v)}
            className="mt-2.5 rounded-md border border-border/60 px-2 py-1 text-xs text-muted-foreground transition-colors hover:border-border hover:text-foreground"
          >
            {expanded
              ? t('overviewpage.grid.collapse')
              : t('overviewpage.grid.showMore', { n: hiddenCount }) +
                (hiddenAbnormal > 0 ? t('overviewpage.grid.hiddenAbnormal', { n: hiddenAbnormal }) : '')}
          </button>
        )}
        {/* 图例 */}
        <div className="mt-4 flex flex-wrap items-center gap-x-4 gap-y-1.5 text-xs text-muted-foreground">
          {(['healthy', 'warn', 'disabled'] as const).map((k) => (
            <span key={k} className="flex items-center gap-1.5">
              <span
                className="h-2.5 w-2.5 rounded-[3px]"
                style={{
                  background: `rgb(${HEALTH_RGB[k]} / 0.9)`,
                  boxShadow: k !== 'disabled' ? `0 0 6px rgb(${HEALTH_RGB[k]} / 0.6)` : 'none',
                }}
              />
              {t(HEALTH_LABEL_KEYS[k])}
            </span>
          ))}
          {/* 分组色相说明：只在池里真有分身组时出现，单开号的池子不多这条噪音。 */}
          {groupCount > 0 && (
            <span className="flex items-center gap-1.5">
              <span className="flex items-center gap-0.5">
                {GROUP_RGB.slice(0, 3).map((rgb) => (
                  <span key={rgb} className="h-2.5 w-2.5 rounded-[3px]" style={{ background: `rgb(${rgb} / 0.9)` }} />
                ))}
              </span>
              {t('overviewpage.legend.groupHue')}
            </span>
          )}
          <span className="flex items-center gap-1.5">
            <span className={`h-2.5 w-2.5 rounded-[3px] bg-transparent ring-1 ring-primary/70`} /> {t('overviewpage.legend.currentActive')}
          </span>
          <span className="flex items-center gap-1.5">
            {/* 这枚是**静态示意**而非真实号，故保留常驻呼吸 —— 不动的话图例读不出「处理中」。 */}
            <span
              className="relative h-2.5 w-2.5 rounded-[3px]"
              style={{ background: `rgb(${HEALTH_RGB.healthy} / 0.25)` }}
            >
              <span
                className="gg-core-breathe absolute inset-0 rounded-[3px]"
                style={{
                  background: `radial-gradient(circle at 50% 42%, rgb(${HEALTH_RGB.healthy} / 0.9), transparent 70%)`,
                  ['--gg-dur' as string]: BREATHE_DUR,
                }}
              />
            </span>
            {t('overviewpage.legend.processing')}
          </span>
        </div>
      {/* 鼠标跟随悬浮卡（正文 CredTooltipBody 不变，仅定位改为黏鼠标） */}
      {hoverCard.render((id) => activity?.get(id))}
      </div>
  )
}
