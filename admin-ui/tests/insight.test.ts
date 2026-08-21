/**
 * `src/lib/insight.ts` 的单测（限流 insight 稳定码 + i18n 渲染）。
 *
 * # 跑法
 *
 * ```bash
 * cd admin-ui && node --import ./tests/tsx-loader-register.mjs --test 'tests/*.test.ts'
 * ```
 */
import { test } from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { join, dirname } from 'node:path'
import { fileURLToPath } from 'node:url'

import { INSIGHT_CODES, insightLabel } from '../src/lib/insight.ts'

const root = join(dirname(fileURLToPath(import.meta.url)), '..')

test('三语字典必须含全部 insight 码，且插值变量与 zh 一致', () => {
  const langs = ['zh', 'en', 'ja']
  const vars = (s) =>
    [...new Set([...s.matchAll(/\{([a-zA-Z_][a-zA-Z0-9_]*)\}/g)].map((m) => m[1]))].sort()
  const zh = JSON.parse(readFileSync(join(root, 'src/i18n/resources/zh.json'), 'utf8'))
  for (const lang of langs) {
    const dict = JSON.parse(
      readFileSync(join(root, `src/i18n/resources/${lang}.json`), 'utf8')
    )
    for (const code of INSIGHT_CODES) {
      const key = `insight.${code}`
      assert.ok(
        typeof dict[key] === 'string' && dict[key].length > 0,
        `${lang}.json 缺 insight key: ${key}`
      )
      if (lang !== 'zh') {
        assert.deepEqual(
          vars(dict[key]),
          vars(zh[key]),
          `[${lang}] ${key} 的插值变量与 zh 不一致`
        )
      }
    }
  }
})

test('insightLabel：有 insightCode 走 i18n，冷却 reason 复用 cooldownCode', () => {
  const t = (key, opts) => {
    if (key === 'credentialcard.cooldown.reason.rate_limited') return 'Rate limit'
    if (key === 'insight.cooldown_rate') {
      return `#${opts.id} cooling down (${opts.reason}), ${opts.secs}s left, triggered ${opts.triggerCount} times`
    }
    return key
  }
  const text = insightLabel(
    {
      insightText: '#54 冷却中（速率限制）剩22s，已触发3次',
      insightCode: 'cooldown_rate',
      insightParams: { id: 54, secs: 22, triggerCount: 3, reasonCode: 'rate_limited' },
      cooldown: { reason: '速率限制', code: 'rate_limited', remainingMs: 21500, triggerCount: 3 },
    },
    t
  )
  assert.equal(text, '#54 cooling down (Rate limit), 22s left, triggered 3 times')
})

test('insightLabel：老后端无 insightCode 时 fallback 中文 insightText', () => {
  const t = (key) => key
  assert.equal(
    insightLabel({ insightText: '畅通' }, t),
    '畅通'
  )
})

test('insightLabel：未知 code 回退 insightText', () => {
  const t = (key) => key
  assert.equal(
    insightLabel(
      { insightText: '畅通', insightCode: 'not_a_real_code', insightParams: {} },
      t
    ),
    '畅通'
  )
})
