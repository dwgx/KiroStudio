import * as React from 'react'
import { ChevronUp, ChevronDown } from 'lucide-react'
import { cn } from '@/lib/utils'

export interface NumberStepperProps {
  value: number
  onChange: (value: number) => void
  min?: number
  max?: number
  step?: number
  /** 输入框宽度类，默认 w-16 */
  className?: string
  disabled?: boolean
  'aria-label'?: string
}

/**
 * 暗色主题数字步进器：中间受控输入 + 右侧上下箭头（±step）。
 * 替代原生 <input type=number> 的浏览器默认 spinner。
 * 长按箭头会加速连续步进。value/onChange/min/max/step 通用可复用。
 */
export function NumberStepper({
  value,
  onChange,
  min = -Infinity,
  max = Infinity,
  step = 1,
  className,
  disabled = false,
  'aria-label': ariaLabel,
}: NumberStepperProps) {
  // 输入框显示为字符串，允许中间态（空串、负号）不立刻回写
  const [text, setText] = React.useState(String(value))

  // 外部 value 变化时同步（非编辑中）
  React.useEffect(() => {
    setText(String(value))
  }, [value])

  const clamp = React.useCallback(
    (n: number) => Math.min(max, Math.max(min, n)),
    [min, max]
  )

  const commit = (raw: string) => {
    const n = Number(raw)
    if (raw.trim() === '' || Number.isNaN(n)) {
      setText(String(value)) // 非法输入回退
      return
    }
    const next = clamp(n)
    setText(String(next))
    if (next !== value) onChange(next)
  }

  const bump = React.useCallback(
    (dir: 1 | -1) => {
      const base = Number(text)
      const start = Number.isNaN(base) ? value : base
      const next = clamp(start + dir * step)
      setText(String(next))
      if (next !== value) onChange(next)
    },
    [text, value, step, clamp, onChange]
  )

  // 长按加速：先单步，200ms 后开始每 60ms 连续步进
  const holdTimer = React.useRef<number | null>(null)
  const repeatTimer = React.useRef<number | null>(null)

  const startHold = (dir: 1 | -1) => {
    if (disabled) return
    bump(dir)
    holdTimer.current = window.setTimeout(() => {
      repeatTimer.current = window.setInterval(() => bump(dir), 60)
    }, 200)
  }

  const stopHold = React.useCallback(() => {
    if (holdTimer.current !== null) {
      window.clearTimeout(holdTimer.current)
      holdTimer.current = null
    }
    if (repeatTimer.current !== null) {
      window.clearInterval(repeatTimer.current)
      repeatTimer.current = null
    }
  }, [])

  React.useEffect(() => stopHold, [stopHold])

  const atMax = value >= max
  const atMin = value <= min

  return (
    <div
      className={cn(
        'inline-flex items-stretch overflow-hidden rounded-md border border-input bg-background',
        'transition-colors duration-200 ease-out-expo focus-within:ring-2 focus-within:ring-ring',
        disabled && 'cursor-not-allowed opacity-50',
        className
      )}
    >
      <input
        type="text"
        inputMode="numeric"
        aria-label={ariaLabel}
        disabled={disabled}
        value={text}
        onChange={(e) => setText(e.target.value.replace(/[^0-9.\-]/g, ''))}
        onBlur={(e) => commit(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === 'Enter') {
            e.preventDefault()
            commit((e.target as HTMLInputElement).value)
          } else if (e.key === 'ArrowUp') {
            e.preventDefault()
            bump(1)
          } else if (e.key === 'ArrowDown') {
            e.preventDefault()
            bump(-1)
          }
        }}
        className="w-full min-w-0 bg-transparent px-2 py-1 text-center text-sm tabular-nums outline-none disabled:cursor-not-allowed"
      />
      <div className="flex flex-col border-l border-input">
        <StepBtn
          dir={1}
          disabled={disabled || atMax}
          onStart={() => startHold(1)}
          onStop={stopHold}
          label="增加"
        />
        <div className="h-px bg-input" />
        <StepBtn
          dir={-1}
          disabled={disabled || atMin}
          onStart={() => startHold(-1)}
          onStop={stopHold}
          label="减少"
        />
      </div>
    </div>
  )
}

function StepBtn({
  dir,
  disabled,
  onStart,
  onStop,
  label,
}: {
  dir: 1 | -1
  disabled: boolean
  onStart: () => void
  onStop: () => void
  label: string
}) {
  const Icon = dir === 1 ? ChevronUp : ChevronDown
  return (
    <button
      type="button"
      tabIndex={-1}
      aria-label={label}
      disabled={disabled}
      onMouseDown={(e) => {
        e.preventDefault()
        onStart()
      }}
      onMouseUp={onStop}
      onMouseLeave={onStop}
      onTouchStart={(e) => {
        e.preventDefault()
        onStart()
      }}
      onTouchEnd={onStop}
      className={cn(
        'flex h-3.5 w-6 items-center justify-center text-muted-foreground',
        'transition-colors duration-150 hover:bg-accent hover:text-foreground',
        'active:bg-accent/80 disabled:pointer-events-none disabled:opacity-30'
      )}
    >
      <Icon className="h-3 w-3" />
    </button>
  )
}
