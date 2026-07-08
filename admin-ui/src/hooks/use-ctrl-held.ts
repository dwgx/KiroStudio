import { useSyncExternalStore } from 'react'

// 全局「是否按住 Ctrl / Cmd(Meta)」状态。
//
// 用于凭据卡：按住 Ctrl 时卡片显示可点击手型光标 + 左键即多选。
// 用 useSyncExternalStore + 单一全局监听器，避免每张卡各挂一对 keydown/keyup
// 监听(号多时上百个监听器)。所有订阅者共享同一份状态。

let ctrlHeld = false
const listeners = new Set<() => void>()

function emit() {
  for (const l of listeners) l()
}

function setHeld(v: boolean) {
  if (v !== ctrlHeld) {
    ctrlHeld = v
    emit()
  }
}

let installed = false
function ensureInstalled() {
  if (installed || typeof window === 'undefined') return
  installed = true
  const onKey = (e: KeyboardEvent) => setHeld(e.ctrlKey || e.metaKey)
  // keydown/keyup 覆盖按下与松开;window blur 时清零(切走窗口不会收到 keyup)
  window.addEventListener('keydown', onKey, true)
  window.addEventListener('keyup', onKey, true)
  window.addEventListener('blur', () => setHeld(false), true)
}

function subscribe(cb: () => void) {
  ensureInstalled()
  listeners.add(cb)
  return () => {
    listeners.delete(cb)
  }
}

/** 是否正按住 Ctrl 或 Cmd(Meta)。多订阅者共享单一全局监听。 */
export function useCtrlHeld(): boolean {
  return useSyncExternalStore(
    subscribe,
    () => ctrlHeld,
    () => false,
  )
}
