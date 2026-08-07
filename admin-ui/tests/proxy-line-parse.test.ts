/**
 * `src/lib/proxy-line-parse.ts` 的单测。
 *
 * # 跑法
 *
 * ```bash
 * cd admin-ui && node --test tests/
 * ```
 *
 * 用 Node 内置的 `node:test` + 原生 TS 类型擦除（本机 v24），**不引入 vitest/jest** ——
 * 仓库里还没有前端测试框架，为一个纯函数模块拉一整套 runner 进 devDependencies
 * 代价不对等（且会动 pnpm-lock，而工作区常有其他会话在改）。
 *
 * # 为什么这个文件在 `src/` 外面
 *
 * `tsconfig.json` 的 `include` 是 `["src"]`，而本文件 import `node:test` 需要
 * `@types/node`（当前不是依赖）。放在 `tests/` 下则 `pnpm tsc --noEmit` 不会看它，
 * 生产构建也不会把它打进包。
 *
 * # 判据必须与 Rust 侧一致
 *
 * 这里的用例与 `src/http_client.rs` 的 `colon_form_*` 那批**同数据、同期望**。
 * 两边任一侧改判据都要让两批测试都过 —— 前端解析只是预览，后端才是权威，
 * 但预览若与后端不一致就会误导用户（说能进却被拒，或反之）。
 */
import { test } from 'node:test'
import assert from 'node:assert/strict'

import {
  classifyProxyLine,
  maskProxyLine,
  normalizeProxyLine,
  parseProxyLinkStrict,
  previewProxyLines,
} from '../src/lib/proxy-line-parse.ts'

/** 单行便捷解析（把 verdict 摊平成 link|null），只在测试里用。 */
function parse(line: string) {
  const v = classifyProxyLine(line)
  return v.kind === 'parsed' ? v.link : null
}

/** 🔴 用户实际粘的那 10 行（`host:port:user:pass`）必须 10/10 识别。 */
test('ten real pasted lines are all recognized', () => {
  const paste = [
    '130.180.228.34:6318:dwgxdwgx:dwgxdwgx',
    '9.142.211.219:5384:dwgxdwgx:dwgxdwgx',
    '45.56.183.65:8387:dwgxdwgx:dwgxdwgx',
    '104.207.42.11:9021:dwgxdwgx:dwgxdwgx',
    '23.129.64.78:7204:dwgxdwgx:dwgxdwgx',
    '185.199.110.153:6001:dwgxdwgx:dwgxdwgx',
    '198.51.100.24:41003:dwgxdwgx:dwgxdwgx',
    '203.0.113.99:1080:dwgxdwgx:dwgxdwgx',
    '192.0.2.44:8899:dwgxdwgx:dwgxdwgx',
    '172.104.55.7:30001:dwgxdwgx:dwgxdwgx',
  ].join('\n')
  const p = previewProxyLines(paste)
  assert.equal(p.okCount, 10, `应 10/10 识别，实得 ${p.okCount}（skipped=${p.skipped}）`)
  assert.equal(p.skipped, 0)
  assert.equal(p.invalidCount, 0)
  assert.equal(p.duplicateCount, 0)
  assert.equal(p.items.length, 10)
  assert.equal(p.items[0].lineno, 1)
  assert.equal(p.items[0].address, 'socks5://130.180.228.34:6318')
  assert.equal(p.items[0].username, 'dwgxdwgx')
  assert.equal(p.items[9].lineno, 10)
  assert.equal(p.items[9].address, 'socks5://172.104.55.7:30001')
})

/** 三种新增形态各一条正向 + scheme/#name 共存。 */
test('three colon shapes parse', () => {
  const a = parse('130.180.228.34:6318:us1u:pw1')
  assert.deepEqual(a, {
    url: 'socks5://130.180.228.34:6318',
    username: 'us1u',
    password: 'pw1',
    name: undefined,
  })
  const b = parse('us2u:pw2:130.180.228.34:6318')
  assert.equal(b?.url, 'socks5://130.180.228.34:6318')
  assert.equal(b?.username, 'us2u')
  assert.equal(b?.password, 'pw2')
  const c = parse('130.180.228.34:6318@us3u:pw3')
  assert.equal(c?.url, 'socks5://130.180.228.34:6318')
  assert.equal(c?.username, 'us3u')
  assert.equal(c?.password, 'pw3')
  const d = parse('socks://1.2.3.4:1080:us4u:pw4#JP-1')
  assert.equal(d?.url, 'socks5://1.2.3.4:1080')
  assert.equal(d?.username, 'us4u')
  assert.equal(d?.password, 'pw4')
  assert.equal(d?.name, 'JP-1')
})

/** 🔴 消歧：两种读法各一条 + 真正判不定的必须被拒而不是猜。 */
test('disambiguation never guesses', () => {
  assert.equal(parse('130.180.228.34:6318:dwgxdwgx:dwgxdwgx')?.url, 'socks5://130.180.228.34:6318')
  assert.equal(parse('dwgxdwgx:dwgxdwgx:130.180.228.34:6318')?.url, 'socks5://130.180.228.34:6318')
  // 判据 2：两段都是端口，只有第 1 段像 host 字面量 ⇒ 读法 A
  const c = parse('10.0.0.1:1080:12345:8080')
  assert.equal(c?.url, 'socks5://10.0.0.1:1080', '12345 不像 host ⇒ 必须读 A')
  assert.equal(c?.password, '8080')
  // 判据 3：两读法都成立 ⇒ 拒绝
  assert.deepEqual(normalizeProxyLine('1.2.3.4:8080:5.6.7.8:9090'), {
    kind: 'rejected',
    issue: 'ambiguous',
  })
  assert.deepEqual(classifyProxyLine('1.2.3.4:8080:5.6.7.8:9090'), {
    kind: 'invalid',
    issue: 'ambiguous',
  })
})

/** 边界：端口 0 / 65536 / 非数字 / host 非法 / 裸 IPv6。 */
test('boundaries rejected', () => {
  for (const bad of ['1.2.3.4:0:u:p', '1.2.3.4:65536:u:p', '1.2.3.4:99999:u:p']) {
    assert.equal(parse(bad), null, `端口越界不该解析: ${bad}`)
    assert.equal(classifyProxyLine(bad).kind, 'invalid', `端口越界应报错而非跳过: ${bad}`)
  }
  assert.equal(parse('1.2.3.4:65535:u:p')?.url, 'socks5://1.2.3.4:65535', '65535 是上界内')
  assert.equal(parse('1.2.3.4:abcd:u:p'), null)
  assert.equal(parse('有中文:1080:u:p'), null)
  assert.equal(parse('nodothost:1080:u:p'), null, '单标签主机名不走冒号形态')
  assert.equal(parse('2001:db8::1:1080:u:p'), null, '裸 IPv6 无解')
  const v6 = parse('[2001:db8::1]:1080:u:p')
  assert.equal(v6?.url, 'socks5://[2001:db8::1]:1080')
  assert.equal(v6?.password, 'p')
})

/** 密码含 `:` 归密码（第 3 个 `:` 之后全部）。 */
test('password may contain colon', () => {
  const p = parse('1.2.3.4:1080:user:pa:ss')
  assert.equal(p?.url, 'socks5://1.2.3.4:1080')
  assert.equal(p?.username, 'user')
  assert.equal(p?.password, 'pa:ss')
})

/** 🔴 已支持格式的回归：归一化层必须对它们完全无感。 */
test('existing formats unchanged', () => {
  const b = parse(
    'socks://dXMxdTpwZUxBck9sWWNDSWZHUmxzcFEzZ1lkRHBkMGs5Zzd1aA@192.220.50.26:40002#US-1-SOCKS5'
  )
  assert.equal(b?.url, 'socks5://192.220.50.26:40002')
  assert.equal(b?.username, 'us1u', 'base64 userinfo 必须被解开')
  assert.equal(b?.password, 'peLArOlYcCIfGRlspQ3gYdDpd0k9g7uh')
  assert.equal(b?.name, 'US-1-SOCKS5')

  // 🔴 密码含 ':' 的规范形态：有 4 段，若归一化先数冒号就会被重写坏
  const c = parse('socks5://user:p:ss@host:1080')
  assert.equal(c?.url, 'socks5://host:1080')
  assert.equal(c?.username, 'user')
  assert.equal(c?.password, 'p:ss', '含 @ 时绝不能走冒号形态')
  assert.deepEqual(normalizeProxyLine('socks5://user:p:ss@host:1080'), { kind: 'asIs' })

  assert.equal(parse('socks5://user:p@ss@host:1080')?.password, 'p@ss', '按最后一个 @ 切')
  assert.equal(
    parse('1.2.3.4:1080@5.6.7.8:8080')?.url,
    'socks5://5.6.7.8:8080',
    '两侧都合法 ⇒ 标准读法不变'
  )
  assert.equal(parse('socks5://1.2.3.4:1080')?.url, 'socks5://1.2.3.4:1080')
  assert.equal(parse('http://onlyuser@host:3128')?.username, 'onlyuser')
  const f = parse('http://us%40er:p%3Ass@host:3128')
  assert.equal(f?.username, 'us@er')
  assert.equal(f?.password, 'p:ss')
  // 明文密码恰好是合法 base64 时绝不能被解码
  assert.equal(parse('socks5://u:cGFzcw@host:1080')?.password, 'cGFzcw')

  for (const bad of ['', '   ', '# 注释', '端口  : 40002', 'socks5://', 'socks5://nohostcolon']) {
    assert.equal(parse(bad), null, `不该解析出节点: ${JSON.stringify(bad)}`)
  }
})

/** 🔴 说明行必须仍被**安静跳过**（不报错、不造假节点）。 */
test('doc noise lines are silently skipped', () => {
  const noise = [
    '==================================================================',
    '  SOCKS5 节点  ·  4 台美国机',
    '端口 40002  ·  认证 用户名/密码',
    '  地址  : 192.220.50.26',
    '  端口  : 40002',
    '  用户名: us1u',
    '  密码  : peLArOlYcCIfGRlspQ3gYdDpd0k9g7uh',
    'port : 40002',
    'curl:',
  ].join('\n')
  const p = previewProxyLines(noise)
  assert.equal(p.items.length, 0, `说明行不该进预览，实得 ${JSON.stringify(p.items)}`)
  assert.equal(p.skipped, 9)
})

/** 重复：已在池中 与 粘贴内重复 都算 duplicate，但 `dupOf` 要能区分。 */
test('duplicates flagged with source', () => {
  const paste = ['1.2.3.4:1080:u:p', '1.2.3.4:1080:u2:p2', '5.6.7.8:1080:u:p'].join('\n')
  const p = previewProxyLines(paste, ['socks5://5.6.7.8:1080'])
  assert.equal(p.items.length, 3)
  assert.equal(p.items[0].status, 'ok')
  assert.equal(p.items[1].status, 'duplicate')
  assert.equal(p.items[1].dupOf, 'paste')
  assert.equal(p.items[2].status, 'duplicate')
  assert.equal(p.items[2].dupOf, 'pool', '已在池中的要标 pool，与粘贴内重复分开')
  assert.equal(p.okCount, 1)
  assert.equal(p.duplicateCount, 2)
})

/** 🔴 预览里不得出现密码（面板会被投屏/截图，且这段文本进 DOM）。 */
test('preview masks password and never carries it', () => {
  const p = previewProxyLines('1.2.3.4:1080:someuser:s3cr3tpw\n')
  assert.equal(p.items.length, 1)
  const it = p.items[0]
  assert.ok(!it.raw.includes('s3cr3tpw'), `密码不得回显: ${it.raw}`)
  assert.ok(it.raw.includes('someuser'), `用户名保留（诊断需要）: ${it.raw}`)
  assert.ok(it.raw.includes('1.2.3.4:1080'), `地址保留: ${it.raw}`)
  assert.ok(
    !Object.values(it).some((v) => typeof v === 'string' && v.includes('s3cr3tpw')),
    '预览条目的任何字段都不得含密码'
  )
  // base64 userinfo：密码在编码里 ⇒ 整段 userinfo 打掉
  const masked = maskProxyLine(
    'socks://dXMxdTpwZUxBck9sWWNDSWZHUmxzcFEzZ1lkRHBkMGs5Zzd1aA@192.220.50.26:40002#US-1'
  )
  assert.ok(!masked.includes('dXMxdTpwZUxB'), `base64 userinfo 必须打掉: ${masked}`)
  assert.ok(masked.includes('192.220.50.26:40002'))
})

/** 老缺口：无 scheme 的 `user:pass@host:port` 也要能进。 */
test('schemeless userinfo form accepted', () => {
  const p = previewProxyLines('us1u:pw1@1.2.3.4:1080\n')
  assert.equal(p.okCount, 1)
  assert.equal(p.skipped, 0)
  assert.equal(p.items[0].address, 'socks5://1.2.3.4:1080')
})

/** 严格核心单独暴露时的错误码要精确（前端按它查 i18n）。 */
test('strict parser returns precise issue codes', () => {
  assert.deepEqual(parseProxyLinkStrict('socks5://nohostcolon'), {
    ok: false,
    issue: 'no_host_port',
  })
  assert.deepEqual(parseProxyLinkStrict('端口  : 40002'), { ok: false, issue: 'bad_host' })
  assert.deepEqual(parseProxyLinkStrict('1.2.3.4:0'), { ok: false, issue: 'bad_port' })
  assert.deepEqual(parseProxyLinkStrict('# 注释'), { ok: false, issue: 'not_proxy' })
})
