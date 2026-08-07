/**
 * `src/lib/socks-node-edit.ts` 的单测。
 *
 * # 跑法
 *
 * ```bash
 * cd admin-ui && node --test 'tests/*.test.ts'
 * ```
 *
 * ⚠️ 目录形式（`node --test tests/`）在本机 Node v24.16.0 上会 `MODULE_NOT_FOUND`，
 * 见 `pool-event-classify.test.ts` 头注释。
 *
 * 同 `proxy-line-parse.test.ts`：Node 内置 `node:test` + 原生 TS 类型擦除。
 * 被测模块只有 `import type`（类型擦除后整行消失），所以 `@/` 别名不需要解析。
 *
 * # 测的是「哪些键被省略」，而不是「值对不对」
 *
 * 后端 `upsert_socks_node` 的三态语义（省略=不改 / `""`=清空 / 有值=设值）意味着
 * **键的存在与否本身就是语义**。断言 `payload.password === undefined` 不够 ——
 * `{password: undefined}` 经 `JSON.stringify` 后确实不发，但用 `'password' in payload`
 * 断言更贴近"这个键到底在不在"这个真实判据。两种都用上。
 */
import { test } from 'node:test'
import assert from 'node:assert/strict'

import {
  buildSocksNodeEditPayload,
  hasSocksNodeEdits,
  type SocksNodeEditForm,
} from '../src/lib/socks-node-edit.ts'
import type { SocksNode } from '../src/types/api.ts'

/** 一个「有密码、有用户名、有名字」的节点 —— 三个坑全都能踩到的形状。 */
function node(over: Partial<SocksNode> = {}): SocksNode {
  return {
    id: 7,
    name: 'US-1',
    url: 'socks5://host.example:40002',
    username: 'u1',
    hasPassword: true,
    enabled: true,
    createdAt: 0,
    label: 'US-1',
    ...over,
  }
}

/** 打开编辑框时的初始表单（= `startEdit` 的回填规则）。 */
function formFor(n: SocksNode): SocksNodeEditForm {
  return {
    url: n.url,
    name: n.name,
    username: n.username ?? '',
    password: '',
    clearPassword: false,
  }
}

test('只改地址：不发 name/username/password 三个键', () => {
  const n = node()
  const p = buildSocksNodeEditPayload(n, { ...formFor(n), url: 'socks5://new.example:1080' })
  assert.equal(p.id, 7)
  assert.equal(p.url, 'socks5://new.example:1080')
  // 🔴 这三条是核心：任何一条变成"发空串"就等于清空该字段。
  assert.equal('name' in p, false)
  assert.equal('username' in p, false)
  assert.equal('password' in p, false)
})

test('密码框留空 = 不发 password（后端不外传密码，回填只能填出空串）', () => {
  const n = node()
  const p = buildSocksNodeEditPayload(n, { ...formFor(n), name: '改了名字' })
  assert.equal(p.name, '改了名字')
  // 回退修复（改成无条件 `payload.password = form.password`）时这条即 FAIL：
  // 那样"改个名字"就会把密码抹成 None，已绑该节点的分身全部认证失败掉线。
  assert.equal('password' in p, false)
})

test('填了新密码 → 原样发送；勾了清除 → 发空串', () => {
  const n = node()
  const changed = buildSocksNodeEditPayload(n, { ...formFor(n), password: 'newpass' })
  assert.equal(changed.password, 'newpass')

  const cleared = buildSocksNodeEditPayload(n, { ...formFor(n), clearPassword: true })
  assert.equal('password' in cleared, true)
  assert.equal(cleared.password, '')
})

test('清除优先于新密码（两者互斥，勾了以清除为准）', () => {
  const n = node()
  const p = buildSocksNodeEditPayload(n, {
    ...formFor(n),
    password: 'ignored',
    clearPassword: true,
  })
  assert.equal(p.password, '')
})

test('用户名清空是有效意图：发空串而不是省略', () => {
  const n = node()
  const p = buildSocksNodeEditPayload(n, { ...formFor(n), username: '' })
  assert.equal('username' in p, true)
  assert.equal(p.username, '')
})

test('用户名未动 → 省略，好让新分享链接里拆出的账密能生效', () => {
  const n = node()
  // 场景：用户把一条新分享链接粘进地址框，其它字段没碰。
  const p = buildSocksNodeEditPayload(n, {
    ...formFor(n),
    url: 'socks://dTI6cDI=@host2.example:1080#US-2',
  })
  // 若这里发了旧 username，后端就不会采用链接里的用户名，而密码却换成了新的
  // ⇒ 半新半旧的组合，必然认证失败。
  assert.equal('username' in p, false)
  assert.equal('name' in p, false)
  assert.equal('password' in p, false)
})

test('名称未动 → 省略（显式 name 会压过链接的 #fragment）', () => {
  const n = node({ name: '' })
  const f = formFor(n)
  // 原名为空、用户也没填 → 仍然不能发 `name: ''`，否则永远读不到链接里的 #name。
  const p = buildSocksNodeEditPayload(n, { ...f, url: 'socks5://h:1#来自链接的名字' })
  assert.equal('name' in p, false)
})

test('username 为 undefined 的节点：表单空串视为"未动"', () => {
  const n = node({ username: undefined })
  const p = buildSocksNodeEditPayload(n, formFor(n))
  assert.equal('username' in p, false)
})

test('url 两侧空白被 trim（避免 host 带空格拼坏）', () => {
  const n = node()
  const p = buildSocksNodeEditPayload(n, { ...formFor(n), url: '  socks5://h:1080  ' })
  assert.equal(p.url, 'socks5://h:1080')
})

test('hasSocksNodeEdits：一个字段都没改时为 false', () => {
  const n = node()
  assert.equal(hasSocksNodeEdits(n, formFor(n)), false)
})

test('hasSocksNodeEdits：任一字段改动都为 true', () => {
  const n = node()
  const f = formFor(n)
  assert.equal(hasSocksNodeEdits(n, { ...f, url: 'socks5://other:1' }), true)
  assert.equal(hasSocksNodeEdits(n, { ...f, name: 'x' }), true)
  assert.equal(hasSocksNodeEdits(n, { ...f, username: 'x' }), true)
  assert.equal(hasSocksNodeEdits(n, { ...f, password: 'x' }), true)
  assert.equal(hasSocksNodeEdits(n, { ...f, clearPassword: true }), true)
})

test('id 恒被带上 —— 这正是「更新」而非「新建」的判据', () => {
  const n = node({ id: 42 })
  const p = buildSocksNodeEditPayload(n, formFor(n))
  assert.equal(p.id, 42)
})
