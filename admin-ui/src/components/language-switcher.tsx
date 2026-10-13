import { useTranslation } from 'react-i18next'
import { cn } from '@/lib/utils'

// 语言切换器:侧边栏 footer + 登录页共用。分段按钮组(中/EN/日),按压切换,不用下拉——
// 下拉会往下弹出跑到视口外(侧边栏 footer 空间紧)。选中态由滑动高亮块指示(带弹性动画)。
// 短标签用各语言母语自称的缩写(中/EN/日),不随当前界面语言变(通用习惯)。
const SEG_LANGS = [
  { code: 'zh', short: '中' },
  { code: 'en', short: 'EN' },
  { code: 'ja', short: '日' },
] as const

// 语言主标签：i18n.language 可能带区域后缀（'zh-CN'/'en-US'/'ja-JP'，来自 navigator.language
// 检测，supportedLngs 里只有 'zh'/'en'/'ja' 三支且 nonExplicitSupportedLngs=true 会原样保留
// 区域码）。直接用 === 比较的后果是三个按钮全不亮、而滑动高亮块被 Math.max(0,-1) 顶到第一格
// —— 高亮指着「中」但没有任何按钮是选中态。故两边都按主标签比较（裸码时是恒等变换）。
export function LanguageSwitcher({ className }: { className?: string }) {
  const { i18n } = useTranslation()
  const current = i18n.language.toLowerCase().split('-')[0]
  const idx = Math.max(0, SEG_LANGS.findIndex((l) => l.code === current))
  return (
    <div
      className={cn(
        'relative flex items-center gap-0 rounded-md border border-[#2e2e2e] bg-[#0d0d0d] p-0.5',
        className,
      )}
      role="group"
      aria-label="Language"
    >
      {/* 滑动高亮块:按选中索引平移,弹性缓动 */}
      <span
        aria-hidden
        className="absolute inset-y-0.5 rounded bg-[#2a2a2a]"
        style={{
          width: `calc((100% - 4px) / ${SEG_LANGS.length})`,
          transform: `translateX(${idx * 100}%)`,
          left: 2,
          // 弹性缓动(轻微回弹),内联保证生效(tailwind 无 ease-out-expo)。
          transition: 'transform 320ms cubic-bezier(0.34, 1.56, 0.64, 1)',
        }}
      />
      {SEG_LANGS.map((l) => {
        const active = l.code === current
        return (
          <button
            key={l.code}
            type="button"
            onClick={() => { void i18n.changeLanguage(l.code) }}
            className={cn(
              'relative z-10 flex-1 rounded px-2 py-1 text-xs font-medium transition-colors duration-200',
              active ? 'text-[#ededed]' : 'text-muted-foreground hover:text-[#ededed]',
            )}
            aria-pressed={active}
            title={l.code}
          >
            {l.short}
          </button>
        )
      })}
    </div>
  )
}
