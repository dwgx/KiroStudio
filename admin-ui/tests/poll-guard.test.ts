/**
 * `src/lib/poll-guard.ts` 的单测（上号弹窗轮询链泄漏修复的核心守卫逻辑）。
 *
 * # 跑法
 *
 * ```bash
 * cd admin-ui && node --test 'tests/*.test.ts'
 * ```
 *
 * 被测模块只 import 类型擦除后消失的类型，`@/` 别名不涉及，可直接跑。
 *
 * # 测的是什么
 *
 * login-dialog 的 pollWeb/pollIdc 泄漏根因：关闭弹窗后 in-flight 请求 resolve，
 * 回调无条件重新排期（且无 open 检查），重开叠加第二条链。守卫的验收标准：
 * 1. close 后 isCurrent 立即为 false（在途回调自尽）；
 * 2. close 后再 open，旧代次依然失效（重开不得复活旧链）；
 * 3. 未 close 的存活链不受影响（代次不变、isCurrent 保持 true）。
 */
import { test } from 'node:test'
import assert from 'node:assert/strict'

import { createPollGuard } from '../src/lib/poll-guard.ts'

test('打开后：捕获的代次有效，回调可继续', () => {
  const g = createPollGuard()
  g.open()
  const e = g.epoch()
  assert.equal(g.isCurrent(e), true)
})

test('关闭后：在途回调捕获的代次立即失效（自尽，不再排期）', () => {
  const g = createPollGuard()
  g.open()
  const e = g.epoch() // 轮询排期前捕获
  g.close() // 用户在 await 期间关闭对话框
  assert.equal(g.isCurrent(e), false)
})

test('关闭再重开：旧链代次失效，新链代次有效（重开不叠加旧链）', () => {
  const g = createPollGuard()
  g.open()
  const oldEpoch = g.epoch()
  g.close()
  g.open() // 用户重开对话框
  assert.equal(g.isCurrent(oldEpoch), false)
  assert.equal(g.isCurrent(g.epoch()), true)
})

test('连续 close 多次安全（代次单调递增）', () => {
  const g = createPollGuard()
  g.open()
  const e = g.epoch()
  g.close()
  g.close()
  assert.equal(g.isCurrent(e), false)
})

test('初始态（未 open）：任何代次都无效', () => {
  const g = createPollGuard()
  assert.equal(g.isCurrent(0), false)
  assert.equal(g.isCurrent(g.epoch()), false)
})

test('open 不递增代次（幂等）：同一轮打开内多次捕获的代次一致', () => {
  const g = createPollGuard()
  g.open()
  const e1 = g.epoch()
  g.open()
  assert.equal(g.epoch(), e1)
  assert.equal(g.isCurrent(e1), true)
})

// --- IDC countdown 超时路径（2026-08-15 对抗审查 MAJOR）---
// login-dialog 的 countdown 到 0 时只调 stopPolling()（现在内部 bump）不关对话框：
// 在飞 pollIdc 回调 await 返回后 isCurrent(旧代次) 必须为 false（自尽、不复活、
// 不弹 toast、不 setStep），而用户重试发起的新链捕获新代次必须仍有效。
test('bump（countdown 超时终止链）：旧代次失效、guard 保持打开、重试新代次有效', () => {
  const g = createPollGuard()
  g.open()
  const inFlightEpoch = g.epoch() // pollIdc 排期前捕获（await pollIdcLogin 在飞）
  g.bump() // stopPolling：countdown 到 0，终止当前链（对话框仍开着，open 不变）
  assert.equal(g.isCurrent(inFlightEpoch), false) // 在飞回调返回后自尽，不再排期
  const retryEpoch = g.epoch() // 用户重试：pollIdc 捕获新代次
  assert.equal(g.isCurrent(retryEpoch), true) // 重试发起的链不受影响（无需重新 open）
})

test('bump 与 close 叠加安全（正常关闭路径 stopPolling 后 close，代次单调递增）', () => {
  const g = createPollGuard()
  g.open()
  const e = g.epoch()
  g.bump()
  g.close()
  assert.equal(g.isCurrent(e), false)
  g.open()
  assert.equal(g.isCurrent(g.epoch()), true)
})
