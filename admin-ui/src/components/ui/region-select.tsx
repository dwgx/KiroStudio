import * as React from 'react'
import { Check, ChevronsUpDown, Search, History, Trash2 } from 'lucide-react'
import { cn } from '@/lib/utils'
import { AWS_REGIONS, filterRegions, getRecentRegions, pushRecentRegion, clearRecentRegions, regionLabel, isRegionCodeShape } from '@/lib/regions'

export interface RegionSelectProps {
  value: string
  onChange: (value: string) => void
  className?: string
  /** 追加到触发按钮的类名（覆盖高度等，用于紧凑场景与邻接元素对齐）。 */
  triggerClassName?: string
  placeholder?: string
  disabled?: boolean
  /** 采用某 region 时是否记入「最近使用」历史（跨入口共享）。默认开启。 */
  recordRecent?: boolean
}

/**
 * 带搜索的 AWS 区域选择器：可输入 code / 中文名 / 城市 / 关键词实时过滤
 * （us / tokyo / 东京 / 弗吉尼亚 都能命中）。点选即填，也允许自由输入非列表值
 * （AWS 新区兼容）。纯手写 combobox，无外部依赖。
 *
 * 智能复用：下拉打开且未输入搜索词时，顶部展示「最近使用」分组（跨设置页 / IdC /
 * 微软 SSO / 凭据卡片自定义切换全局共享的历史，见 lib/regions）；采用任一 region
 * 都会写回历史，下次任何入口都能一键复用。
 */
export function RegionSelect({
  value,
  onChange,
  className,
  triggerClassName,
  placeholder = '选择或输入区域',
  disabled = false,
  recordRecent = true,
}: RegionSelectProps) {
  const [open, setOpen] = React.useState(false)
  const [query, setQuery] = React.useState('')
  const [highlight, setHighlight] = React.useState(0)
  const [recent, setRecent] = React.useState<string[]>([])
  const rootRef = React.useRef<HTMLDivElement>(null)
  const inputRef = React.useRef<HTMLInputElement>(null)

  const results = React.useMemo(() => filterRegions(query), [query])
  const selected = React.useMemo(
    () => AWS_REGIONS.find((r) => r.code === value),
    [value]
  )

  // 最新 query 的 ref：点击外部的 document 监听器只按 [open] 重订阅，闭包会捕获旧 query，
  // 用 ref 读实时值，确保「关闭时提交」拿到用户最后键入的内容。
  const queryRef = React.useRef(query)
  queryRef.current = query
  // 仅在「打开且无搜索词」时展示最近使用分组。排除当前已选值（避免与展示重复）。
  const showRecent = open && query.trim() === '' && recent.length > 0
  const recentToShow = React.useMemo(
    () => (showRecent ? recent.filter((c) => c !== value).slice(0, 5) : []),
    [showRecent, recent, value]
  )

  // 关闭下拉时提交已键入但未回车/点选的内容（commit-on-close，防丢字）：
  // 用户在搜索框键入 `eu-central-1` 后直接点框外/切换按钮 → 若不提交，query 被丢弃、
  // 外部 value 仍是旧值（曾致「填了 region 但切换按钮还是灰的」交互回归）。
  // 仅当输入**长得像一个 region code**（形状校验）时才回写，避免把「东京」这类
  // 未解析成 code 的搜索关键词污染进 value（保护设置页 region 表单）。
  const closeWithCommit = React.useCallback(() => {
    const q = queryRef.current.trim().toLowerCase()
    if (q && q !== value && isRegionCodeShape(q)) {
      if (recordRecent) pushRecentRegion(q)
      onChange(q)
    }
    setOpen(false)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [value, onChange, recordRecent])

  // 点击外部关闭（提交已键入的 region code）
  React.useEffect(() => {
    if (!open) return
    const onDown = (e: MouseEvent) => {
      if (rootRef.current && !rootRef.current.contains(e.target as Node)) {
        closeWithCommit()
      }
    }
    document.addEventListener('mousedown', onDown)
    return () => document.removeEventListener('mousedown', onDown)
  }, [open, closeWithCommit])

  React.useEffect(() => {
    if (open) {
      setQuery('')
      setHighlight(0)
      setRecent(getRecentRegions()) // 每次打开刷新历史（其它入口刚写入的也能立刻看到）
      // 打开后聚焦搜索框
      requestAnimationFrame(() => inputRef.current?.focus())
    }
  }, [open])

  const pick = (code: string) => {
    if (recordRecent) pushRecentRegion(code)
    onChange(code)
    setOpen(false)
  }

  // 自由输入：query 是一个非列表值时，允许直接采用。
  // AWS region code 恒为小写，归一化后再采用——否则 EU-CENTRAL-1 会被后端白名单精确匹配拒掉，
  // 且与 pushRecentRegion 内部的小写化不一致导致「最近使用」重复展示。
  const commitFreeInput = () => {
    const q = query.trim()
    if (results.length > 0) {
      pick(results[Math.min(highlight, results.length - 1)].code)
    } else if (q) {
      pick(q.toLowerCase())
    } else {
      setOpen(false)
    }
  }

  return (
    <div ref={rootRef} className={cn('relative', className)}>
      <button
        type="button"
        disabled={disabled}
        onClick={() => (open ? closeWithCommit() : setOpen(true))}
        className={cn(
          'flex h-10 w-full items-center justify-between gap-2 rounded-md border border-input bg-background px-3 py-2 text-sm',
          'ring-offset-background transition-colors duration-200 ease-out-expo',
          'hover:border-border-hover focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring',
          'disabled:cursor-not-allowed disabled:opacity-50',
          triggerClassName
        )}
      >
        <span className={cn('truncate text-left', !value && 'text-muted-foreground')}>
          {selected ? (
            <>
              <span>{selected.label}</span>
              <span className="ml-1.5 font-mono text-xs text-muted-foreground">{selected.code}</span>
            </>
          ) : (
            value || placeholder
          )}
        </span>
        <ChevronsUpDown className="h-4 w-4 shrink-0 text-muted-foreground" />
      </button>

      {open && (
        <div
          className={cn(
            'absolute z-50 mt-1.5 w-full min-w-[280px] overflow-hidden rounded-md border border-border bg-popover shadow-lg',
            'origin-top animate-rise-in'
          )}
        >
          {/* 搜索框 */}
          <div className="flex items-center gap-2 border-b border-border px-3">
            <Search className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
            <input
              ref={inputRef}
              value={query}
              onChange={(e) => {
                setQuery(e.target.value)
                setHighlight(0)
              }}
              onKeyDown={(e) => {
                if (e.key === 'ArrowDown') {
                  e.preventDefault()
                  setHighlight((h) => Math.min(h + 1, results.length - 1))
                } else if (e.key === 'ArrowUp') {
                  e.preventDefault()
                  setHighlight((h) => Math.max(h - 1, 0))
                } else if (e.key === 'Enter') {
                  e.preventDefault()
                  commitFreeInput()
                } else if (e.key === 'Escape') {
                  e.preventDefault()
                  setOpen(false)
                }
              }}
              placeholder="搜索：us / tokyo / 东京 / 弗吉尼亚"
              className="h-9 w-full bg-transparent text-sm outline-none placeholder:text-muted-foreground"
            />
          </div>

          {/* 最近使用分组（仅打开且无搜索词时置顶展示，跨入口共享的历史） */}
          {recentToShow.length > 0 && (
            <div className="border-b border-border py-1">
              <div className="flex items-center justify-between px-3 py-1">
                <span className="flex items-center gap-1.5 text-[11px] font-medium text-muted-foreground">
                  <History className="h-3 w-3" />
                  最近使用
                </span>
                {/* 清理钮：清空全局「最近使用」历史并即时收起分组。阻止冒泡，不触发下拉选中/关闭。 */}
                <button
                  type="button"
                  onClick={(e) => {
                    e.preventDefault()
                    e.stopPropagation()
                    clearRecentRegions()
                    setRecent([])
                  }}
                  className="flex items-center rounded p-0.5 text-muted-foreground transition-colors duration-150 hover:text-foreground"
                  title="清空最近使用历史"
                  aria-label="清空最近使用历史"
                >
                  <Trash2 className="h-3 w-3" />
                </button>
              </div>
              {recentToShow.map((code) => (
                  <button
                    type="button"
                    key={`recent-${code}`}
                    onClick={() => pick(code)}
                    className="flex w-full items-center justify-between gap-3 px-3 py-1.5 text-left text-sm text-muted-foreground transition-colors duration-150 hover:bg-accent hover:text-foreground"
                  >
                    <span className="truncate text-foreground">{regionLabel(code)}</span>
                    <span className="font-mono text-xs text-muted-foreground">{code}</span>
                  </button>
              ))}
            </div>
          )}

          {/* 结果列表 */}
          <div className="max-h-[240px] overflow-y-auto py-1">
            {results.length === 0 ? (
              <div className="px-3 py-2.5 text-xs text-muted-foreground">
                无匹配区域，回车可直接使用
                <span className="ml-1 font-mono text-foreground">{query.trim()}</span>
              </div>
            ) : (
              results.map((r, i) => (
                <button
                  type="button"
                  key={r.code}
                  onMouseEnter={() => setHighlight(i)}
                  onClick={() => pick(r.code)}
                  className={cn(
                    'flex w-full items-center justify-between gap-3 px-3 py-2 text-left text-sm transition-colors duration-150',
                    i === highlight ? 'bg-accent text-foreground' : 'text-muted-foreground'
                  )}
                >
                  <span className="flex min-w-0 flex-col">
                    <span className="truncate text-foreground">{regionLabel(r.code)}</span>
                    <span className="truncate text-xs text-muted-foreground">{r.city}</span>
                  </span>
                  <span className="flex shrink-0 items-center gap-2">
                    <span className="font-mono text-xs text-muted-foreground">{r.code}</span>
                    {r.code === value && <Check className="h-3.5 w-3.5 text-primary" />}
                  </span>
                </button>
              ))
            )}
          </div>
        </div>
      )}
    </div>
  )
}
