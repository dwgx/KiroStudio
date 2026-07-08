import * as React from 'react'
import { Check, ChevronsUpDown } from 'lucide-react'
import { cn } from '@/lib/utils'

export interface SelectOption<T extends string = string> {
  value: T
  label: string
  /** 选项下方的次要说明（可选） */
  hint?: string
  disabled?: boolean
}

export interface SelectProps<T extends string = string> {
  value: T
  onChange: (value: T) => void
  options: SelectOption<T>[]
  className?: string
  placeholder?: string
  disabled?: boolean
  id?: string
  'aria-label'?: string
}

/**
 * 项目内置下拉选择器（不用浏览器原生 <select>，样式与暗色主题统一）。
 * 纯手写：触发按钮 + 绝对定位弹层，点击外部/Esc 关闭，上下键 + 回车键盘导航。
 * 无外部依赖，视觉与 RegionSelect 一致。适用于选项固定的枚举场景。
 */
export function Select<T extends string = string>({
  value,
  onChange,
  options,
  className,
  placeholder = '请选择',
  disabled = false,
  id,
  'aria-label': ariaLabel,
}: SelectProps<T>) {
  const [open, setOpen] = React.useState(false)
  const [highlight, setHighlight] = React.useState(0)
  const rootRef = React.useRef<HTMLDivElement>(null)
  const btnRef = React.useRef<HTMLButtonElement>(null)

  const selected = React.useMemo(
    () => options.find((o) => o.value === value),
    [options, value]
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

  // 打开时高亮定位到当前选中项
  React.useEffect(() => {
    if (open) {
      const idx = options.findIndex((o) => o.value === value)
      setHighlight(idx >= 0 ? idx : 0)
    }
  }, [open, options, value])

  const pick = (opt: SelectOption<T>) => {
    if (opt.disabled) return
    onChange(opt.value)
    setOpen(false)
    requestAnimationFrame(() => btnRef.current?.focus())
  }

  // 从 from 起按 dir 找下一个可选项（跳过 disabled）
  const step = (from: number, dir: 1 | -1) => {
    const n = options.length
    for (let i = 1; i <= n; i++) {
      const idx = (from + dir * i + n * i) % n
      if (!options[idx]?.disabled) return idx
    }
    return from
  }

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (!open) {
      if (e.key === 'Enter' || e.key === ' ' || e.key === 'ArrowDown') {
        e.preventDefault()
        setOpen(true)
      }
      return
    }
    if (e.key === 'ArrowDown') {
      e.preventDefault()
      setHighlight((h) => step(h, 1))
    } else if (e.key === 'ArrowUp') {
      e.preventDefault()
      setHighlight((h) => step(h, -1))
    } else if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault()
      const opt = options[highlight]
      if (opt) pick(opt)
    } else if (e.key === 'Escape') {
      e.preventDefault()
      setOpen(false)
    }
  }

  return (
    <div ref={rootRef} className={cn('relative', className)}>
      <button
        ref={btnRef}
        type="button"
        id={id}
        aria-label={ariaLabel}
        aria-haspopup="listbox"
        aria-expanded={open}
        disabled={disabled}
        onClick={() => setOpen((v) => !v)}
        onKeyDown={onKeyDown}
        className={cn(
          'flex h-10 w-full items-center justify-between gap-2 rounded-md border border-input bg-background px-3 py-2 text-sm',
          'ring-offset-background transition-colors duration-200 ease-out-expo',
          'hover:border-border-hover focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring',
          'disabled:cursor-not-allowed disabled:opacity-50',
          open && 'border-ring ring-2 ring-ring'
        )}
      >
        <span className={cn('truncate text-left', !selected && 'text-muted-foreground')}>
          {selected ? selected.label : placeholder}
        </span>
        <ChevronsUpDown className="h-4 w-4 shrink-0 text-muted-foreground" />
      </button>

      {open && (
        <div
          role="listbox"
          className={cn(
            'absolute z-50 mt-1.5 w-full overflow-hidden rounded-md border border-border bg-popover shadow-lg',
            'origin-top animate-rise-in'
          )}
        >
          <div className="max-h-[280px] overflow-y-auto py-1">
            {options.map((opt, i) => (
              <button
                type="button"
                key={opt.value}
                role="option"
                aria-selected={opt.value === value}
                disabled={opt.disabled}
                onMouseEnter={() => setHighlight(i)}
                onClick={() => pick(opt)}
                className={cn(
                  'flex w-full items-center justify-between gap-3 px-3 py-2 text-left text-sm transition-colors duration-150',
                  'disabled:cursor-not-allowed disabled:opacity-40',
                  i === highlight ? 'bg-accent text-foreground' : 'text-muted-foreground'
                )}
              >
                <span className="flex min-w-0 flex-col">
                  <span className="truncate text-foreground">{opt.label}</span>
                  {opt.hint && (
                    <span className="truncate text-xs text-muted-foreground">{opt.hint}</span>
                  )}
                </span>
                {opt.value === value && <Check className="h-3.5 w-3.5 shrink-0 text-primary" />}
              </button>
            ))}
          </div>
        </div>
      )}
    </div>
  )
}
