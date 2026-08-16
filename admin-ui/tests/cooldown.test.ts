/**
 * `src/lib/cooldown.ts` 的单测（冷却原因稳定枚举码 helper）。
 *
 * # 跑法
 *
 * ```bash
 * cd admin-ui && node --import ./tests/tsx-loader-register.mjs --test 'tests/*.test.ts'
 * ```
 *
 * # 测的是什么
 *
 * 语言耦合改造（docs/cooldown-reason-i18n-design.md）的核心判定逻辑：
 * 1. rate_limited / suspicious 判定只认稳定枚举码（改后端中文文案不破坏判定）；
 * 2. 9 个 code 必须与后端 CooldownReason 变体一一映射，且三语字典都含对应 key；
 * 3. 未知 code / 老后端（缺失 cooldownCode）无害降级：判定 false、展示 fallback 原串。
 */
import { test } from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { join, dirname } from 'node:path'
import { fileURLToPath } from 'node:url'

import {
  isRateLimitCooldown,
  isSuspiciousCooldown,
  cooldownReasonKey,
  cooldownReasonLabel,
} from '../src/lib/cooldown.ts'

const root = join(dirname(fileURLToPath(import.meta.url)), '..')

test('isRateLimitCooldown：只认 rate_limited 码', () => {
  assert.equal(isRateLimitCooldown('rate_limited'), true)
  assert.equal(isRateLimitCooldown('suspicious'), false)
  assert.equal(isRateLimitCooldown('server_error'), false)
  assert.equal(isRateLimitCooldown('未知码'), false)
  assert.equal(isRateLimitCooldown(undefined), false) // 老后端无 cooldownCode → 无害降级
  assert.equal(isRateLimitCooldown(''), false)
})

test('isSuspiciousCooldown：只认 suspicious 码', () => {
  assert.equal(isSuspiciousCooldown('suspicious'), true)
  assert.equal(isSuspiciousCooldown('rate_limited'), false)
  assert.equal(isSuspiciousCooldown('account_suspended'), false)
  assert.equal(isSuspiciousCooldown(undefined), false)
  assert.equal(isSuspiciousCooldown('可疑活动风控'), false) // 中文文案不再是判据
})

test('cooldownReasonKey：9 个稳定码全部映射到 i18n key', () => {
  const codes = [
    'rate_limited',
    'suspicious',
    'account_suspended',
    'quota_exhausted',
    'token_refresh_failed',
    'authentication_failed',
    'auth_transient',
    'server_error',
    'model_unavailable',
  ]
  for (const code of codes) {
    assert.equal(
      cooldownReasonKey(code),
      `credentialcard.cooldown.reason.${code}`,
      `code=${code} 必须映射到对应 i18n key`
    )
  }
  assert.equal(cooldownReasonKey('unknown_code'), undefined)
  assert.equal(cooldownReasonKey(undefined), undefined)
  assert.equal(cooldownReasonKey(''), undefined)
})

test('三语字典必须含全部 9 个冷却原因 key（后端 9 变体一一对应，不漏翻译）', () => {
  const langs = ['zh', 'en', 'ja']
  const codes = [
    'rate_limited',
    'suspicious',
    'account_suspended',
    'quota_exhausted',
    'token_refresh_failed',
    'authentication_failed',
    'auth_transient',
    'server_error',
    'model_unavailable',
  ]
  for (const lang of langs) {
    const dict = JSON.parse(
      readFileSync(join(root, `src/i18n/resources/${lang}.json`), 'utf8')
    )
    for (const code of codes) {
      const key = `credentialcard.cooldown.reason.${code}`
      assert.ok(
        typeof dict[key] === 'string' && dict[key].length > 0,
        `${lang}.json 缺冷却原因 key: ${key}`
      )
    }
  }
})

test('cooldownReasonLabel：已知 code 走注入翻译，未知/缺失 fallback 后端原串', () => {
  const t = (key: string) => `T:${key}`
  assert.equal(cooldownReasonLabel('rate_limited', '速率限制', t), 'T:credentialcard.cooldown.reason.rate_limited')
  assert.equal(cooldownReasonLabel('suspicious', '可疑活动风控', t), 'T:credentialcard.cooldown.reason.suspicious')
  assert.equal(cooldownReasonLabel('未知码', '速率限制', t), '速率限制')
  assert.equal(cooldownReasonLabel(undefined, '服务器错误', t), '服务器错误') // 老后端
  assert.equal(cooldownReasonLabel(undefined, undefined, t), '')
})
