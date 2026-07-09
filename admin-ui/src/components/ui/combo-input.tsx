import { useId } from 'react'
import { cn } from '@/lib/utils'

/**
 * 带预设下拉的可编辑输入（combobox）。
 *
 * 用原生 `<input list>` + `<datalist>`:既能从预设里点选(如常见的 Kiro/系统/Node 版本号),
 * 又能自由输入自定义值。零新依赖、无障碍友好、暗色主题下与站内 Input 观感一致。
 *
 * 用于设置页"客户端伪装"的版本字段——dwgx 要能自定义选择而非只能手敲。
 */
export function ComboInput({
  value,
  onChange,
  options,
  placeholder,
  className,
  'aria-label': ariaLabel,
}: {
  value: string
  onChange: (v: string) => void
  options: string[]
  placeholder?: string
  className?: string
  'aria-label'?: string
}) {
  const listId = useId()
  return (
    <>
      <input
        list={listId}
        value={value}
        placeholder={placeholder}
        aria-label={ariaLabel}
        onChange={(e) => onChange(e.target.value)}
        className={cn(
          'flex h-9 w-full rounded-md border border-input bg-background px-3 py-1 text-sm shadow-sm transition-colors',
          'placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring',
          'disabled:cursor-not-allowed disabled:opacity-50',
          className
        )}
      />
      <datalist id={listId}>
        {options.map((opt) => (
          <option key={opt} value={opt} />
        ))}
      </datalist>
    </>
  )
}
