/**
 * i18n 字典的插值卫生测试。
 *
 * # 跑法
 *
 * ```bash
 * cd admin-ui && node --import ./tests/tsx-loader-register.mjs --test 'tests/*.test.ts'
 * ```
 *
 * # 这里测的是什么（2026-08-09 事故回归）
 *
 * 线上面板多处显示字面 `{{n}}` / `已选 {{n}} 个` —— 用户看到花括号和变量名直接显示、
 * 没被替换。根因：
 * - `src/i18n/index.ts` 把插值前后缀设成单花括号 `{var}`；
 * - 但字典里 22 条 key 误写成**双花括号** `{{n}}`；
 * - 双花括号在单花括号配置下被解析成**变量名 `` `{n}` ``**（带花括号的名字），
 *   与调用点实参 `n` 不匹配 ⇒ i18next 原样输出 `{{n}}`。
 *
 * 已全仓修复（66 处 = 22 key × 3 语言）。本测试钉死两类回归：
 * 1. **不允许再出现双花括号** —— 一旦有人新增/手改 key 时写成 `{{n}}`，立即 FAIL。
 * 2. **key 的变量名必须与调用点实参一致**（处理 ES6 简写 `{ d, h }` 与跨行参数），
 *    防止「key 改了变量名但调用点没同步」这类静默失效。
 *
 * 这两条靠脚本跑一遍三语字典 + 全仓 `t()` 调用点比对，不需要构建。
 */

import { test } from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync, readdirSync } from 'node:fs'
import { join, dirname } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = join(dirname(fileURLToPath(import.meta.url)), '..')
function loadJson(p) {
  return JSON.parse(readFileSync(join(root, p), 'utf8'))
}

function allSourceFiles() {
  const out = []
  const walk = (dir) => {
    for (const e of readdirSync(dir, { withFileTypes: true })) {
      const p = join(dir, e.name)
      if (e.isDirectory()) walk(p)
      else if (/\.(tsx?|mjs)$/.test(e.name)) out.push(p)
    }
  }
  walk(join(root, 'src'))
  walk(join(root, 'tests'))
  return out
}

test('三语字典不含任何双花括号插值', () => {
  const langs = ['zh', 'en', 'ja']
  for (const lang of langs) {
    const dict = loadJson(`src/i18n/resources/${lang}.json`)
    for (const [k, v] of Object.entries(dict)) {
      if (typeof v === 'string' && v.includes('{{')) {
        assert.fail(
          `[${lang}] ${k} 用了双花括号 {{...}} —— 本项目 i18next 配的是单花括号 {var}，` +
            '双花括号会被解析成带花括号的变量名导致字面显示。请改成单花括号。'
        )
      }
    }
  }
})

test('三语字典 key 集合完全一致，且变量名一致', () => {
  const zh = loadJson('src/i18n/resources/zh.json')
  const en = loadJson('src/i18n/resources/en.json')
  const ja = loadJson('src/i18n/resources/ja.json')
  for (const [k, v] of Object.entries(zh)) {
    for (const [lang, dict] of [['en', en], ['ja', ja]]) {
      assert.ok(k in dict, `[${lang}] 缺少 key ${k}`)
      if (typeof v === 'string' && typeof dict[k] === 'string') {
        const vars = (s) => [...new Set([...s.matchAll(/\{([a-zA-Z_][a-zA-Z0-9_]*)\}/g)].map((m) => m[1]))].sort()
        assert.deepEqual(
          vars(dict[k]), vars(v),
          `[${lang}] ${k} 的插值变量与 zh 不一致`
        )
      }
    }
  }
})

test('每个需要变量的 key 调用点都传了对应实参', () => {
  const zh = loadJson('src/i18n/resources/zh.json')
  // key -> 需要的变量集合
  const need = new Map()
  for (const [k, v] of Object.entries(zh)) {
    if (typeof v === 'string') {
      const vars = [...new Set([...v.matchAll(/\{([a-zA-Z_][a-zA-Z0-9_]*)\}/g)].map((m) => m[1]))]
      if (vars.length) need.set(k, vars)
    }
  }
  const src = allSourceFiles().map((f) => readFileSync(f, 'utf8')).join('\n')

  let checked = 0
  // 匹配 t('key', { ... }) / i18n.t('key', { ... }) 且带参数对象
  const re = /(?:[it]18n\.)?t\(\s*'([^']+)'\s*,\s*\{/g
  for (const m of src.matchAll(re)) {
    const key = m[1]
    if (!need.has(key)) continue
    // 括号平衡收集参数块
    let i = m.index + m[0].length - 1 // 停在 '{'
    let depth = 1
    let buf = ''
    while (i + 1 < src.length && depth > 0) {
      const c = src[++i]
      if (c === '{') depth++
      else if (c === '}') depth--
      if (depth > 0) buf += c
    }
    // 传参：`a: x` 与简写 `a` 都算
    const passed = new Set([
      ...[...buf.matchAll(/([a-zA-Z_][a-zA-Z0-9_]*)\s*:/g)].map((x) => x[1]),
      ...buf.split(',').map((s) => s.trim()).filter((s) => /^[a-zA-Z_]\w*$/.test(s)),
    ])
    const missing = need.get(key).filter((v) => !passed.has(v))
    checked++
    assert.deepEqual(
      missing, [],
      `t('${key}') 缺变量 ${missing.join(',')}（key 需要 ${need.get(key).join(',')}，调用点传了 ${[...passed].join(',')}）`
    )
  }
  assert.ok(checked > 0, '至少应检查到一些调用点')
})
