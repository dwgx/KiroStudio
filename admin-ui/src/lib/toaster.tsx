// 自研通知系统（弃用 sonner）——dwgx：通知需重写，多条并发时 sonner 的折叠态
// CSS hack 会闪烁/空白/错位。这里用一个极简 pub/sub store + 自绘 Toaster，
// 完全掌控堆叠/上限/去重/动画，行为可预测。
//
// 对外仍暴露与 sonner 兼容的 `toast` API（success/error/warning/info/loading/
// promise/dismiss/message）与 `Toaster` 组件、`ExternalToast` 类型。配合
// vite.config 的 alias（'sonner' → 本文件），现有 `import { toast } from 'sonner'`
// 的所有调用点零改动，照常可用。
//
// 视觉：右下角、纯实色暗底 + 左侧一道细语义色竖条 + 语义图标 + 底部倒计时进度条 +
// 常驻关闭叉叉。刻意去光污染（无光晕/发光）。竖直平铺、硬上限、超出丢最旧，不折叠。

import { useEffect, useRef, useState, type ReactNode } from 'react'
import { CheckCircle2, XCircle, AlertTriangle, Info, Loader2 } from 'lucide-react'

export type ToastType = 'success' | 'error' | 'warning' | 'info' | 'loading' | 'default'

// 与 sonner 的 ExternalToast 子集兼容（本项目实际只用到这几项）
export interface ExternalToast {
  description?: ReactNode
  duration?: number
  id?: string | number
}

export interface ToastRecord {
  id: string | number
  type: ToastType
  title: ReactNode
  description?: ReactNode
  duration: number // ms；Infinity = 不自动消失（loading）
}

// 同时最多可见条数（硬上限，超出丢最旧，防批量刷屏堆爆）
const MAX_VISIBLE = 5

const DEFAULT_DURATION: Record<ToastType, number> = {
  success: 3200,
  info: 3600,
  warning: 4200,
  error: 5000,
  loading: Infinity,
  default: 3600,
}

type Listener = (toasts: ToastRecord[]) => void

class ToastStore {
  private toasts: ToastRecord[] = []
  private listeners = new Set<Listener>()
  private seq = 0

  subscribe(fn: Listener): () => void {
    this.listeners.add(fn)
    fn(this.toasts)
    return () => {
      this.listeners.delete(fn)
    }
  }

  private emit() {
    for (const fn of this.listeners) fn(this.toasts)
  }

  private nextId(): string {
    this.seq += 1
    return `t${Date.now()}_${this.seq}`
  }

  // 新增或（同 id）就地更新。返回 id 供 loading/promise/dismiss 使用。
  upsert(rec: Omit<ToastRecord, 'id'> & { id?: string | number }): string | number {
    const id = rec.id ?? this.nextId()
    const idx = this.toasts.findIndex((t) => t.id === id)
    const full: ToastRecord = { ...rec, id }
    if (idx >= 0) {
      // 就地更新（重置内容/时长；Toaster 端会因 key 不变而重置计时）
      this.toasts = this.toasts.map((t) => (t.id === id ? full : t))
    } else {
      this.toasts = [...this.toasts, full]
      // 硬上限：超出丢最旧（数组头部），防批量事件把栈堆爆
      if (this.toasts.length > MAX_VISIBLE) {
        this.toasts = this.toasts.slice(this.toasts.length - MAX_VISIBLE)
      }
    }
    this.emit()
    return id
  }

  dismiss(id?: string | number) {
    if (id === undefined) this.toasts = []
    else this.toasts = this.toasts.filter((t) => t.id !== id)
    this.emit()
  }
}

export const toastStore = new ToastStore()

// ── 对外 toast API（sonner 兼容子集）───────────────────────────────
type Titleish = ReactNode

function make(type: ToastType) {
  return (title: Titleish, data?: ExternalToast): string | number =>
    toastStore.upsert({
      type,
      title,
      description: data?.description,
      duration: data?.duration ?? DEFAULT_DURATION[type],
      id: data?.id,
    })
}

interface ToastFn {
  (title: Titleish, data?: ExternalToast): string | number
  success: (title: Titleish, data?: ExternalToast) => string | number
  error: (title: Titleish, data?: ExternalToast) => string | number
  warning: (title: Titleish, data?: ExternalToast) => string | number
  info: (title: Titleish, data?: ExternalToast) => string | number
  message: (title: Titleish, data?: ExternalToast) => string | number
  loading: (title: Titleish, data?: ExternalToast) => string | number
  dismiss: (id?: string | number) => void
  promise: <T>(
    promise: Promise<T>,
    opts: {
      loading: Titleish
      success: Titleish | ((v: T) => Titleish)
      error: Titleish | ((e: unknown) => Titleish)
    },
  ) => Promise<T>
}

const base = ((title: Titleish, data?: ExternalToast) =>
  make('default')(title, data)) as ToastFn

base.success = make('success')
base.error = make('error')
base.warning = make('warning')
base.info = make('info')
base.message = make('default')
base.loading = make('loading')
base.dismiss = (id?: string | number) => toastStore.dismiss(id)
base.promise = <T,>(
  promise: Promise<T>,
  opts: { loading: Titleish; success: Titleish | ((v: T) => Titleish); error: Titleish | ((e: unknown) => Titleish) },
) => {
  const id = base.loading(opts.loading)
  promise.then(
    (v) => toastStore.upsert({ id, type: 'success', title: typeof opts.success === 'function' ? (opts.success as (v: T) => Titleish)(v) : opts.success, duration: DEFAULT_DURATION.success }),
    (e) => toastStore.upsert({ id, type: 'error', title: typeof opts.error === 'function' ? (opts.error as (e: unknown) => Titleish)(e) : opts.error, duration: DEFAULT_DURATION.error }),
  )
  return promise
}

export const toast = base

// ── 视觉（去光污染：纯实色暗底 + 左侧语义竖条 + 底部倒计时条）──────────
const ICONS: Record<ToastType, ReactNode> = {
  success: <CheckCircle2 size={18} strokeWidth={2.25} />,
  error: <XCircle size={18} strokeWidth={2.25} />,
  warning: <AlertTriangle size={18} strokeWidth={2.25} />,
  info: <Info size={18} strokeWidth={2.25} />,
  loading: <Loader2 size={18} strokeWidth={2.25} className="ks-toast-spin" />,
  default: <Info size={18} strokeWidth={2.25} />,
}

const TONE: Record<ToastType, string> = {
  success: '#2ecc9b',
  error: '#ff5c54',
  warning: '#f0a92e',
  info: '#4c9dff',
  loading: '#9aa0aa',
  default: '#9aa0aa',
}

const CSS = `
.ks-toaster {
  position: fixed;
  bottom: 20px;
  right: 20px;
  z-index: 999999;
  display: flex;
  flex-direction: column;
  gap: 12px;
  width: 400px;
  max-width: calc(100vw - 40px);
  pointer-events: none;
}
.ks-toast {
  --tone: #9aa0aa;
  position: relative;
  overflow: hidden;
  width: 100%;
  background: #1c1c1f;
  border: 1px solid rgba(255,255,255,0.10);
  border-left: 3px solid var(--tone);
  border-radius: 10px;
  box-shadow: 0 8px 24px -8px rgba(0,0,0,0.5);
  padding: 13px 40px 13px 14px;
  color: #ededed;
  display: flex;
  align-items: flex-start;
  pointer-events: auto;
  animation: ks-toast-in 220ms cubic-bezier(0.21,1.02,0.73,1);
}
.ks-toast[data-removing="true"] { animation: ks-toast-out 180ms ease forwards; }
.ks-toast-icon { color: var(--tone); margin-right: 10px; margin-top: 1px; flex-shrink: 0; display: flex; }
.ks-toast-body { flex: 1 1 auto; min-width: 0; }
.ks-toast-title {
  font-size: 13.5px; font-weight: 600; letter-spacing: -0.01em; color: #f4f4f5;
  line-height: 1.4; white-space: normal; word-break: break-word; overflow-wrap: anywhere;
}
.ks-toast-desc {
  font-size: 12.5px; color: #a0a0a6; line-height: 1.5; margin-top: 3px;
  white-space: normal; word-break: break-word; overflow-wrap: anywhere;
}
.ks-toast-close {
  position: absolute; top: 10px; right: 8px; width: 22px; height: 22px;
  border-radius: 6px; background: rgba(255,255,255,0.10); border: 1px solid rgba(255,255,255,0.18);
  color: #e4e4e7; display: flex; align-items: center; justify-content: center;
  cursor: pointer; padding: 0; transition: background 150ms ease, color 150ms ease;
}
.ks-toast-close:hover { background: rgba(255,255,255,0.2); color: #fff; }
.ks-toast-bar {
  position: absolute; left: 0; bottom: 0; height: 2px; background: var(--tone);
  opacity: 0.75; transform-origin: left; animation: ks-toast-bar linear forwards;
}
.ks-toast-spin { animation: ks-toast-spin 0.9s linear infinite; }
@keyframes ks-toast-in { from { opacity: 0; transform: translateX(16px) scale(0.98); } to { opacity: 1; transform: none; } }
@keyframes ks-toast-out { to { opacity: 0; transform: translateX(16px) scale(0.98); } }
@keyframes ks-toast-bar { from { transform: scaleX(1); } to { transform: scaleX(0); } }
@keyframes ks-toast-spin { to { transform: rotate(360deg); } }
@media (prefers-reduced-motion: reduce) {
  .ks-toast, .ks-toast[data-removing="true"] { animation: none !important; }
  .ks-toast-bar, .ks-toast-spin { animation: none !important; }
  .ks-toast-close { transition: none !important; }
}
`

function ToastItem({ rec }: { rec: ToastRecord }) {
  const [removing, setRemoving] = useState(false)
  // hover 暂停：记录剩余时长，离开时按剩余续跑
  const remainingRef = useRef(rec.duration)
  const startRef = useRef(Date.now())
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null)
  const [paused, setPaused] = useState(false)

  const finite = Number.isFinite(rec.duration)

  const beginClose = () => {
    setRemoving(true)
    setTimeout(() => toastStore.dismiss(rec.id), 170)
  }

  // 同 id 就地更新(loading→success/error)时,rec.duration 会从 Infinity 变成有限值。
  // remainingRef 是 useRef 初始化的,不会自动跟随 rec.duration 变化——若不同步,下方计时 effect
  // 会用**旧的 Infinity** 去 setTimeout(beginClose, Infinity),浏览器把 Infinity/溢出延时当 0 处理
  // → 成功 toast 一闪即消失(这正是"测活 notification 立马消失"的根因)。故 duration 变化即重同步。
  useEffect(() => {
    remainingRef.current = rec.duration
    startRef.current = Date.now()
  }, [rec.duration])

  useEffect(() => {
    if (!finite) return
    const clear = () => {
      if (timerRef.current) clearTimeout(timerRef.current)
    }
    if (paused) {
      clear()
      remainingRef.current -= Date.now() - startRef.current
      return clear
    }
    startRef.current = Date.now()
    timerRef.current = setTimeout(beginClose, Math.max(0, remainingRef.current))
    return clear
    // rec.duration 变化（同 id 更新）时重置计时
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [paused, rec.duration, finite])

  return (
    <div
      className="ks-toast"
      data-type={rec.type}
      data-removing={removing}
      style={{ ['--tone' as string]: TONE[rec.type] }}
      onMouseEnter={() => setPaused(true)}
      onMouseLeave={() => setPaused(false)}
      role="status"
      aria-live={rec.type === 'error' ? 'assertive' : 'polite'}
    >
      <span className="ks-toast-icon">{ICONS[rec.type]}</span>
      <div className="ks-toast-body">
        <div className="ks-toast-title">{rec.title}</div>
        {rec.description != null && rec.description !== '' && (
          <div className="ks-toast-desc">{rec.description}</div>
        )}
      </div>
      <button className="ks-toast-close" aria-label="关闭" onClick={beginClose}>
        <XCircle size={14} strokeWidth={2.4} style={{ display: 'none' }} />
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.4" strokeLinecap="round"><path d="M18 6 6 18M6 6l12 12"/></svg>
      </button>
      {finite && !paused && !removing && (
        <span
          key={`${rec.id}-${rec.duration}-${startRef.current}`}
          className="ks-toast-bar"
          style={{ animationDuration: `${remainingRef.current}ms` }}
        />
      )}
    </div>
  )
}

export interface ToasterProps {
  position?: string // 兼容 sonner 的 position 传参（本组件固定右下角，忽略其余值）
}

export function Toaster(_props: ToasterProps) {
  const [toasts, setToasts] = useState<ToastRecord[]>([])
  useEffect(() => toastStore.subscribe(setToasts), [])
  return (
    <>
      <style>{CSS}</style>
      <div className="ks-toaster" aria-live="polite" aria-relevant="additions">
        {toasts.map((t) => (
          <ToastItem key={t.id} rec={t} />
        ))}
      </div>
    </>
  )
}

export default toast



