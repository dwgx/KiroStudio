import * as React from 'react'
import { Check, ChevronsUpDown, Search } from 'lucide-react'
import { cn } from '@/lib/utils'
import { AWS_REGIONS, filterRegions } from '@/lib/regions'

export interface RegionSelectProps {
  value: string
  onChange: (value: string) => void
  className?: string
  placeholder?: string
  disabled?: boolean
}

/**
 * 带搜索的 AWS 区域选择器：可输入 code / 中文名 / 城市 / 关键词实时过滤
 * （us / tokyo / 东京 / 弗吉尼亚 都能命中）。点选即填，也允许自由输入非列表值
 * （AWS 新区兼容）。纯手写 combobox，无外部依赖。
 */
export function RegionSelect({
  value,
  onChange,
  className,
  placeholder = '选择或输入区域',
  disabled = false,
}: RegionSelectProps) {
  const [open, setOpen] = React.useState(false)
  const [query, setQuery] = React.useState('')
  const [highlight, setHighlight] = React.useState(0)
  const rootRef = React.useRef<HTMLDivElement>(null)
  const inputRef = React.useRef<HTMLInputElement>(null)

  const results = React.useMemo(() => filterRegions(query), [query])
  const selected = React.useMemo(
    () => AWS_REGIONS.find((r) => r.code === value),
    [value]
  )

  // 点击外部关闭
  React.useEffect(() => {
    if (!open) return
    const onDown = (e: MouseEvent) => {
      if (rootRef.current && !rootRef.current.contains(e.target as Node)) {
        setOpen(false)
      }
    }
    document.addEventListener('mousedown', onDown)
    return () => document.removeEventListener('mousedown', onDown)
  }, [open])

  React.useEffect(() => {
    if (open) {
      setQuery('')
      setHighlight(0)
      // 打开后聚焦搜索框
      requestAnimationFrame(() => inputRef.current?.focus())
    }
  }, [open])

  const pick = (code: string) => {
    onChange(code)
    setOpen(false)
  }

  // 自由输入：query 是一个非列表值时，允许直接采用
  const commitFreeInput = () => {
    const q = query.trim()
    if (results.length > 0) {
      pick(results[Math.min(highlight, results.length - 1)].code)
    } else if (q) {
      pick(q)
    } else {
      setOpen(false)
    }
  }

  return (
    <div ref={rootRef} className={cn('relative', className)}>
      <button
        type="button"
        disabled={disabled}
        onClick={() => setOpen((v) => !v)}
        className={cn(
          'flex h-10 w-full items-center justify-between gap-2 rounded-md border border-input bg-background px-3 py-2 text-sm',
          'ring-offset-background transition-colors duration-200 ease-out-expo',
          'hover:border-border-hover focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring',
          'disabled:cursor-not-allowed disabled:opacity-50'
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
                    <span className="truncate text-foreground">{r.label}</span>
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
