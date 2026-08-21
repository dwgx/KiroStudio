/**
 * `shouldClearAdminSession` — axios 拦截器是否清 sessionStorage 并 reload。
 *
 * # 跑法
 *
 * ```bash
 * cd admin-ui && node --import ./tests/tsx-loader-register.mjs --test 'tests/*.test.ts'
 * ```
 *
 * 纯函数、无 `@/` 别名，可单独 `node --test tests/should-clear-admin-session.test.ts`。
 */
import { test } from 'node:test'
import assert from 'node:assert/strict'

import { shouldClearAdminSession } from '../src/api/should-clear-admin-session.ts'

test('401 一律清会话（不论 error.type）', () => {
  assert.equal(shouldClearAdminSession(401, undefined), true)
  assert.equal(shouldClearAdminSession(401, 'authentication_error'), true)
  assert.equal(shouldClearAdminSession(401, 'invalid_request'), true)
})

test('403 无 authentication_error 不清会话（业务拒绝交给 toast）', () => {
  assert.equal(shouldClearAdminSession(403, undefined), false)
  assert.equal(shouldClearAdminSession(403, 'invalid_request'), false)
  assert.equal(shouldClearAdminSession(403, 'permission_error'), false)
})

test('403 + authentication_error 清会话', () => {
  assert.equal(shouldClearAdminSession(403, 'authentication_error'), true)
})

test('其它状态不清会话', () => {
  assert.equal(shouldClearAdminSession(undefined, 'authentication_error'), false)
  assert.equal(shouldClearAdminSession(200, 'authentication_error'), false)
  assert.equal(shouldClearAdminSession(404, 'authentication_error'), false)
  assert.equal(shouldClearAdminSession(500, undefined), false)
})
