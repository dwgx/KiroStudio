/**
 * 轮询链生命周期守卫：对话框关闭后，in-flight 轮询回调自尽（不再重新排期），
 * 重开后旧链（上一代次）也不得复活 —— 否则关闭的对话框会持续打后端，
 * 重开叠加第二条链，甚至关闭状态下弹 toast / 触发 onSuccess。
 *
 * 用法：
 * - 对话框 open 时调 open()，关闭时调 close()（close 递增代次，所有已捕获的旧代次立即失效）。
 * - 每次排期前 epoch() 捕获当前代次；回调里（含 await 之后）用 isCurrent(epoch) 复查，
 *   不满足即 return 自尽，不再排期。
 */
export interface PollGuard {
  open: () => void
  close: () => void
  /**
   * 终止当前轮询链：递增代次，使所有已捕获旧代次的在途回调（含 await 返回后）自尽，
   * 但保持 open（对话框未关）—— 之后重试排期捕获新代次依然有效。
   * 与 close 的区别：close = bump + 置 open=false（对话框已关）；bump 是「链终止」
   * 不涉及对话框开关状态（如 IDC countdown 超时只终止轮询、弹框还开着）。
   */
  bump: () => void
  epoch: () => number
  isCurrent: (epoch: number) => boolean
}

export function createPollGuard(): PollGuard {
  let open = false
  let epoch = 0
  const bump = () => {
    epoch += 1
  }
  return {
    open: () => {
      open = true
    },
    close: () => {
      bump()
      open = false
    },
    bump,
    epoch: () => epoch,
    isCurrent: (e) => open && e === epoch,
  }
}
