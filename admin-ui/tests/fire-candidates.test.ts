/**
 * `src/components/overview/FireCanvas.tsx` 里 `pickFireCandidates` 的单测。
 *
 * # 跑法
 *
 * ```bash
 * cd admin-ui && node --import ./tests/tsx-loader-register.mjs --test 'tests/*.test.ts'
 * ```
 *
 * ⚠️ 必须带 `--import ./tests/tsx-loader-register.mjs`：被测对象在 `.tsx` 里，Node 原生类型擦除
 * 不认这个扩展名（`ERR_UNKNOWN_FILE_EXTENSION`）。钩子对 `.ts` 透传，所以同一条命令也跑另外两个
 * 测试文件。理由与实现见 `tests/tsx-loader.mjs`。
 *
 * # 这里测的是「数量上限 + 排序优先级 + 滞后」，不是观感
 *
 * 事故背景：实测 WebKit.GPU 进程 102% 而 WebContent 0.0%（忙的是合成器不是 JS），根因是够条件
 * 点火的号有多少就挂多少个 WebGL 上下文，超过浏览器硬上限（~16）后最老的被强制丢弃 ——
 * canvas 静默变空白但 RAF 照常每帧对死上下文发一整套 GL 命令。**数量才是根因**，故这批断言里
 * 「≤ 4」那条是承重的：它被回退（去掉 cap）时必须 FAIL。
 *
 * 滞后那条同样承重：`getContext('webgl2')` 要编译三套 shader + 建四个 FBO，若名额按瞬时强度
 * 硬排，inflight/rpm 的正常抖动会让火焰在几个号之间反复挂载/卸载 —— 比一直开着更贵。
 */
import { test } from 'node:test'
import assert from 'node:assert/strict'

import {
  MAX_FIRE_INSTANCES,
  isFireEligible,
  pickFireCandidates,
  type FireCandidate,
} from '../src/components/overview/FireCanvas.tsx'

/** 造一个候选；默认非禁用、不饱和、零负载（即默认**不**够资格点火）。 */
function cand(id: number, over: Partial<FireCandidate> = {}): FireCandidate {
  return { id, saturated: false, inflight: 0, rpm: 0, lit: true, ...over }
}

test('并发实例数硬顶在 MAX_FIRE_INSTANCES（回退 cap 时此条 FAIL）', () => {
  // 20 个号全部够资格且强度递增 —— 没有上限的实现会全返回 20 个。
  const many = Array.from({ length: 20 }, (_, i) => cand(i + 1, { inflight: i + 2, rpm: 30 + i }))
  const picked = pickFireCandidates(many)
  assert.equal(MAX_FIRE_INSTANCES, 4, '上限改了要连带复核本文件的期望值')
  assert.equal(
    picked.length,
    MAX_FIRE_INSTANCES,
    `并发 WebGL 上下文必须 ≤ ${MAX_FIRE_INSTANCES}，实得 ${picked.length}（=${picked}）`,
  )
  // 取的必须是最猛的那 4 个（inflight 21..18 → id 20,19,18,17），不是数组里靠前的 4 个。
  assert.deepEqual(picked, [20, 19, 18, 17])
})

test('调用方传再大的 max 也突破不了硬顶（预算是 FireCanvas 的责任）', () => {
  const many = Array.from({ length: 30 }, (_, i) => cand(i + 1, { rpm: 100 }))
  assert.equal(pickFireCandidates(many, new Set(), 99).length, MAX_FIRE_INSTANCES)
  assert.equal(pickFireCandidates(many, new Set(), Number.POSITIVE_INFINITY).length, MAX_FIRE_INSTANCES)
  // 反向：更小的 max 要被尊重（0 = 一个都不点，用于将来加"关闭火焰"开关）。
  assert.equal(pickFireCandidates(many, new Set(), 2).length, 2)
  assert.deepEqual(pickFireCandidates(many, new Set(), 0), [])
  assert.deepEqual(pickFireCandidates(many, new Set(), -5), [], '负数按 0 处理，不得抛异常')
})

test('排序优先级：饱和 > 在途 > RPM > id', () => {
  // 饱和压过一切：saturated 但零负载的号排在 inflight=9/rpm=200 的非饱和号前面。
  const a = pickFireCandidates([
    cand(1, { inflight: 9, rpm: 200 }),
    cand(2, { saturated: true }),
  ])
  assert.deepEqual(a, [2, 1], 'saturated 是后端结论，比前端两个瞬时数更可信 → 必须最优先')

  // 同饱和度时 inflight 压过 rpm。
  const b = pickFireCandidates([
    cand(1, { inflight: 2, rpm: 999 }),
    cand(2, { inflight: 5, rpm: 20 }),
  ])
  assert.deepEqual(b, [2, 1], 'inflight 是"此刻在打"，比 60s 滑窗 rpm 更即时')

  // inflight 相同时按 rpm。
  const c = pickFireCandidates([cand(1, { inflight: 3, rpm: 30 }), cand(2, { inflight: 3, rpm: 80 })])
  assert.deepEqual(c, [2, 1])

  // 三键全等 → id 升序兜底。这条保证是**全序**：结果不随输入顺序变（抖 = 反复重建上下文）。
  const tied = [cand(7, { rpm: 50 }), cand(3, { rpm: 50 }), cand(5, { rpm: 50 })]
  assert.deepEqual(pickFireCandidates(tied), [3, 5, 7])
  assert.deepEqual(
    pickFireCandidates([...tied].reverse()),
    [3, 5, 7],
    '输入顺序颠倒后结果必须一致，否则每次渲染都会重挂 WebGL 上下文',
  )
})

test('滞后生效：在位者不被小差距顶掉（回退滞后时此条 FAIL）', () => {
  // 4 个在烧的号（rpm 50），挑战者 rpm 60 —— 只多 10，低于 15 的滞后余量 ⇒ 不许换。
  const pool = [
    cand(1, { rpm: 50 }),
    cand(2, { rpm: 50 }),
    cand(3, { rpm: 50 }),
    cand(4, { rpm: 50 }),
    cand(5, { rpm: 60 }),
  ]
  const burning = new Set([1, 2, 3, 4])
  assert.deepEqual(
    pickFireCandidates(pool, burning),
    [1, 2, 3, 4],
    'rpm 只多 10（< 余量 15）不该换人：换一次要编译三套 shader + 建四个 FBO',
  )
  // 对照组：冷启动（burning 为空、无在位者可保护）时同一批输入会选到 #5 并挤掉 #4 ——
  // 证明上面那条差异确实来自滞后，而不是排序本身就不选 #5。
  assert.deepEqual(pickFireCandidates(pool), [5, 1, 2, 3])
})

test('滞后不是死锁：明显更猛 / 饱和翻转都换得掉', () => {
  const burning = new Set([1, 2, 3, 4])
  const base = [cand(1, { rpm: 50 }), cand(2, { rpm: 50 }), cand(3, { rpm: 50 }), cand(4, { rpm: 50 })]

  // rpm 多 15（够余量）⇒ 顶掉最弱的在位者（三键全等时最弱=id 最大的 4）。
  const byRpm = pickFireCandidates([...base, cand(9, { rpm: 65 })], burning)
  assert.ok(byRpm.includes(9), `rpm 65 vs 50 差 15 达余量，应换入，实得 ${byRpm}`)
  assert.ok(!byRpm.includes(4), `应顶掉最弱在位者 #4，实得 ${byRpm}`)
  assert.equal(byRpm.length, MAX_FIRE_INSTANCES)

  // 在途多 2（够余量）⇒ 同样换得掉。
  const byInflight = pickFireCandidates(
    [...base.map((c) => ({ ...c, inflight: 2 })), cand(9, { inflight: 4 })],
    burning,
  )
  assert.ok(byInflight.includes(9), `在途 4 vs 2 达余量，应换入，实得 ${byInflight}`)

  // 饱和翻转不设滞后：正在挨限流的号是用户最该看见的那个，立刻换。
  const bySat = pickFireCandidates([...base, cand(9, { saturated: true })], burning)
  assert.ok(bySat.includes(9), `饱和号应立刻抢到名额，实得 ${bySat}`)
  assert.equal(bySat[0], 9, '饱和号还应排在最前')
})

test('滞后不阻挡"名额本来就空着"的情况', () => {
  // 只有 2 个在烧、又来 2 个新号 ⇒ 名额够，不需要抢，滞后不该把新号挡在外面。
  const pool = [cand(1, { rpm: 50 }), cand(2, { rpm: 50 }), cand(3, { rpm: 21 }), cand(4, { rpm: 21 })]
  assert.deepEqual(pickFireCandidates(pool, new Set([1, 2])), [1, 2, 3, 4])
})

test('在位者掉出资格（负载归零）时名额立刻释放', () => {
  // #1..#4 在烧但已全部降到门槛下 ⇒ 不再够资格，名额让给真在打的 #9。
  const pool = [cand(1), cand(2), cand(3), cand(4), cand(9, { rpm: 25 })]
  assert.deepEqual(pickFireCandidates(pool, new Set([1, 2, 3, 4])), [9])
})

test('空输入 / 全不够资格 / 禁用号 → 返回空', () => {
  assert.deepEqual(pickFireCandidates([]), [])
  // 门槛之下：rpm 19 < 20、inflight 1 < 2、非饱和。
  assert.deepEqual(pickFireCandidates([cand(1, { rpm: 19, inflight: 1 })]), [])
  // 禁用号即使饱和 + 满负载也不点火（healthOf === 'disabled'）。
  assert.deepEqual(
    pickFireCandidates([cand(1, { lit: false, saturated: true, inflight: 9, rpm: 300 })]),
    [],
    '禁用号不该烧 WebGL 上下文',
  )
})

test('资格判定的门槛边界（与原 StatusBars 的 onFire 判据同口径）', () => {
  assert.equal(isFireEligible(cand(1, { rpm: 20 })), true, 'RPM ≥20 是含等号的')
  assert.equal(isFireEligible(cand(1, { rpm: 19 })), false)
  assert.equal(isFireEligible(cand(1, { inflight: 2 })), true, '在途 ≥2 是含等号的')
  assert.equal(isFireEligible(cand(1, { inflight: 1 })), false)
  assert.equal(isFireEligible(cand(1, { saturated: true })), true, '饱和单独即够资格')
  assert.equal(isFireEligible(cand(1, { lit: false, rpm: 300 })), false, 'lit 是与关系，一票否决')
})
