/**
 * `src/lib/pool-event-classify.ts` 的单测。
 *
 * # 跑法
 *
 * ```bash
 * cd admin-ui && node --test 'tests/*.test.ts'
 * ```
 *
 * ⚠️ 用 glob 而不是 `node --test tests/`：后者在本机 Node v24.16.0 上会把目录当
 * CJS 入口去 require，直接 `MODULE_NOT_FOUND`（`proxy-line-parse.test.ts` 头注释里
 * 写的是目录形式，已过期）。
 *
 * 与 `proxy-line-parse.test.ts` 同一套：Node 内置 `node:test` + 原生 TS 类型擦除，
 * 不引入 vitest/jest（理由见那个文件的头注释）。
 *
 * # 这里测的是**分支顺序**，不是分支内容
 *
 * `classifyDisabledReason` 里 `QuotaExceeded` / 两条 region 都必须排在兜底
 * `'disabled'` 之前。只断言"某个原因返回某个类别"会同时被正确实现与错误实现通过
 * （错误实现里那几条压根到不了自己的分支）—— 所以下面显式断言它们**不等于**兜底类。
 *
 * # 判据必须与 Rust 侧一致
 *
 * 原因字面量取自 `src/kiro/token_manager.rs` 的 `DisabledReason::as_str()`
 * （那里的注释写明"改动这些字面量等于改 API 契约"）。全 14 个变体都在下面列了 ——
 * 后端加变体时这批测试会提醒补前端映射。
 */
import { test } from 'node:test'
import assert from 'node:assert/strict'

import { classifyDisabledReason } from '../src/lib/pool-event-classify.ts'

/** 后端 `DisabledReason::as_str()` 的全部 14 个字面量。 */
const ALL_BACKEND_REASONS = [
  'Manual',
  'TooManyFailures',
  'TooManyRefreshFailures',
  'QuotaExceeded',
  'AccountSuspended',
  'SuspiciousActivityAuto',
  'InvalidRefreshToken',
  'InvalidConfig',
  'RequestLimitReached',
  'PassthroughFailed',
  'PassthroughOverloaded',
  'RegionProbeFailed',
  'RegionProbeTokenDead',
  'Unknown',
]

test('region 探测两条各自成类，而不是落兜底 disabled', () => {
  // 反向断言是重点：修复被回退（两条 case 删掉）时它们会落 'disabled'，此处即 FAIL。
  assert.notEqual(classifyDisabledReason('RegionProbeFailed'), 'disabled')
  assert.notEqual(classifyDisabledReason('RegionProbeTokenDead'), 'disabled')
  assert.equal(classifyDisabledReason('RegionProbeFailed'), 'regionProbe')
  assert.equal(classifyDisabledReason('RegionProbeTokenDead'), 'regionTokenDead')
})

test('region 两条互不混淆（处置动作不同，混了等于没给方向）', () => {
  assert.notEqual(
    classifyDisabledReason('RegionProbeFailed'),
    classifyDisabledReason('RegionProbeTokenDead'),
  )
})

test('QuotaExceeded 仍走 quota（原有行为不被新分支挤掉）', () => {
  assert.equal(classifyDisabledReason('QuotaExceeded'), 'quota')
})

test('其余后端原因一律落兜底 disabled', () => {
  const special = new Set(['QuotaExceeded', 'RegionProbeFailed', 'RegionProbeTokenDead'])
  for (const r of ALL_BACKEND_REASONS) {
    if (special.has(r)) continue
    assert.equal(classifyDisabledReason(r), 'disabled', `${r} 应落兜底`)
  }
})

test('缺失 / 未知原因落兜底，不抛异常', () => {
  assert.equal(classifyDisabledReason(undefined), 'disabled')
  assert.equal(classifyDisabledReason(''), 'disabled')
  // 旧版本读到新版本写的变体名时也必须有归属（后端有 #[serde(other)] 兜底，前端不能崩）。
  assert.equal(classifyDisabledReason('SomeFutureReason'), 'disabled')
})
