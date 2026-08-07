# shield 开关 + 吸收统计上面板：前端与 i18n 完整方案

痛点两半：①「内置 shield 开关在设置里都没看到」→ §1 建**独立卡片**而非塞进协议卡；
②「面板完全看不见」→ §2 内置计数器 + §4 外挂 shield 统计。后端契约见
`docs/clone-mgmt-specs-2026-08-03/SPEC-shield-builtin.md`（§1 六项配置 / §5 七个计数器），本文只管前端。

## 1. 六项吸收层配置的开关 UI

**放哪**：`scheduling` 分区新建卡片 `settingspage.card.absorb`，插在「智能调度」(`:1880`) 与
「防关联/限流」(`:2162`) 之间。**不建议**并入 `ccAutoBuffer` 的协议卡（`:2107-2124`）——协议卡讲
报文改写，吸收层讲 429/退避，语义属限流族；且只有独立卡片才有自己的标题与 kw，用户搜
「429」「吸收」「重试」才命中，这正是原痛点。

| 改哪 | file:line | 怎么改 |
|---|---|---|
| 类型（快照） | `admin-ui/src/types/api.ts:349`（`ccAutoBuffer: boolean` 后） | 加 6 项**必填**：`upstreamRetryAbsorbEnabled: boolean` / `...BudgetSecs: number` / `...MaxAttempts: number` / `...MinDelayMs: number` / `...MaxDelaySecs: number` / `...Suspended: boolean` |
| 类型（更新请求） | `admin-ui/src/types/api.ts:431`（`ccAutoBuffer?: boolean` 后） | 同名 6 项全部 `?:` 可选 |
| `FormState` | `settings-page.tsx:230`（`ccAutoBuffer: boolean` 后） | `upstreamRetryAbsorbEnabled: boolean`、`upstreamRetryAbsorbSuspended: boolean`；四个数字项按本文件既有惯例存**字符串**：`upstreamRetryAbsorbBudgetSecs: string` 等 |
| `toForm` | `settings-page.tsx:321`（`ccAutoBuffer: c.ccAutoBuffer,` 后） | `upstreamRetryAbsorbEnabled: c.upstreamRetryAbsorbEnabled ?? false,` / `...BudgetSecs: String(c.upstreamRetryAbsorbBudgetSecs ?? 60),` / `...MaxAttempts: String(... ?? 4),` / `...MinDelayMs: String(... ?? 150),` / `...MaxDelaySecs: String(... ?? 15),` / `...Suspended: c.upstreamRetryAbsorbSuspended ?? false,` |
| `diff` | `settings-page.tsx:1537`（`ccAutoBuffer` 那行后） | 布尔两项照抄 `ccAutoBuffer` 写法但基线带 `?? false`；四个数字项照抄 `:1573-1574` 的 `parseInt` 范式（`const nAbsorbBudget = parseInt(form.upstreamRetryAbsorbBudgetSecs, 10)`，`Number.isFinite && !== (config.x ?? 默认)` 才赋值） |
| 搜索索引 | `settings-page.tsx:151` 后（`smartSchedule` 之后） | `{ section: 'scheduling', titleKey: 'settingspage.card.absorb', kwKey: 'settingspage.card.absorb.kw' },` |
| JSX | `settings-page.tsx:1879`（`smartSchedule` 的 `</SectionGate>` 后） | 新 `<SectionGate section="scheduling" titleKey="settingspage.card.absorb" kwKey="settingspage.card.absorb.kw">` + `<Card>`；内部 6 个 `<Field>`：2 个 `<Switch>`（照 `:2117`）+ 4 个 `<NumberStepper>`（照 `:2155`，`min/step` 见下）+ 1 个 `<Input>`（shieldUrl，§4） |

`NumberStepper` 取值范围建议：budget `min={0} step={10}`、maxAttempts `min={0} max={16} step={1}`、
minDelay `min={50} step={50}`、maxDelay `min={1} step={5}`。hint 一律拼 `hotParen`（`:1709`）——
六项都是 TIER3 热更，无需重启。`SectionId` 联合类型（`:125`）**不用改**，复用 `scheduling`。

## 2. 吸收统计卡片（7 个计数器）

`admin-ui/src/api/ops.ts:141`（`strayInlineRequests` 后、`}` 前）加 7 个 `?: number`——必须可选，
否则旧后端缺字段时类型说谎。`ops-page.tsx:131` 后（`]` 之前）追加：

```ts
  { key: 'absorbAttempts', labelKey: 'opspage.metric.absorbAttempts', warn: true },
  { key: 'absorbRecovered', labelKey: 'opspage.metric.absorbRecovered' },
  { key: 'absorbBudgetExhausted', labelKey: 'opspage.metric.absorbBudgetExhausted', warn: true },
  { key: 'absorbSuspendSkipped', labelKey: 'opspage.metric.absorbSuspendSkipped', warn: true },
  { key: 'absorbRounds1', labelKey: 'opspage.metric.absorbRounds1' },
  { key: 'absorbRounds2', labelKey: 'opspage.metric.absorbRounds2' },
  { key: 'absorbRounds3plus', labelKey: 'opspage.metric.absorbRounds3plus', warn: true },
```

`warn: true` 的判据是「越多越该警惕」：`absorbAttempts`（上游在拒你，量大说明整形阈值虚高）、
`absorbBudgetExhausted`（没救回来，客户端仍见 429）、`absorbSuspendSkipped`（撞上 403 风控）、
`absorbRounds3plus`（3 轮以上才成＝预算接近不够）。`absorbRecovered` 与 `Rounds1/2` 是好消息，不带。
吸收比可心算：`absorbRecovered / (absorbRecovered + absorbBudgetExhausted)`，与外挂的 1.07:1 同口径。

⚠️ **顺带修一处既有缺陷**：`ops-page.tsx:296` 的 `const v = data[it.key] as number` 对可选字段
（现存 `reclaimedInvokeCalls`/`stray*` 已如此，新增 7 项同理）在旧后端下拿到 `undefined` →
`Math.round(undefined)` → 界面显示「NaN」。改为 `(data[it.key] as number | undefined) ?? 0`。

## 3. i18n 完整键表（41 键 × 3 文件）

三份文件**行号完全一致**（各 1523 键）。四个插入点，**必须自下而上插**否则行号漂移：

| 顺序 | 键组（下方 JSON 里按空行分隔，组序＝此表序） | 插入位置（zh/en/ja 同） |
|---|---|---|
| ① | `settingspage.card.absorb` ×2 | 第 1069 行 `"settingspage.card.antiAssoc"` **之前** |
| ② | `settingspage.absorb.*` ×19 | 第 1020 行 `"settingspage.anti.affinity.hint"` **之前** |
| ③ | `opspage.shield.*` ×13 | 第 859 行 `"opspage.row.resetEnable"` **之后**（即 860 `opspage.stat.currentRps` 前） |
| ④ | `opspage.metric.absorb*` ×7 | 第 820 行 `"opspage.metric.cooldownTriggered"` **之前** |

下面每份 JSON 按**最终字母序**给出（组序为 ④③②①，即文件内自上而下的实际顺序，方便逐组剪切）。
全部是带尾逗号的片段，插入后无需改动相邻行。

**zh.json**

```json
  "opspage.metric.absorbAttempts": "吸收重试轮次",
  "opspage.metric.absorbBudgetExhausted": "吸收预算耗尽",
  "opspage.metric.absorbRecovered": "吸收成功(客户端未见 429)",
  "opspage.metric.absorbRounds1": "1 轮即成",
  "opspage.metric.absorbRounds2": "2 轮才成",
  "opspage.metric.absorbRounds3plus": "≥3 轮才成",
  "opspage.metric.absorbSuspendSkipped": "403 风控跳过不吸收",

  "opspage.shield.absorbed": "已吸收",
  "opspage.shield.byStatus": "按状态码",
  "opspage.shield.coolWaits": "冷却等待",
  "opspage.shield.gaveUp": "放弃",
  "opspage.shield.notConfigured": "未配置外置 shield 统计地址",
  "opspage.shield.ratio": "吸收比 {ratio}",
  "opspage.shield.requests": "请求总数",
  "opspage.shield.retries": "重试次数",
  "opspage.shield.subtitle": "外置进程自身计数，与上方网关计数器彼此独立；shield 退役后此卡自动消失",
  "opspage.shield.swapGaveUp": "换号放弃",
  "opspage.shield.swapWaits": "换号等待",
  "opspage.shield.title": "外置 shield（旁挂）",
  "opspage.shield.unreachable": "已配置但拉取失败，外置 shield 可能已停止",

  "settingspage.absorb.budget.aria": "吸收层总预算秒数",
  "settingspage.absorb.budget.hint": "从进入请求处理起算的绝对 deadline，与凭据池内部 12 次换号 / 45s 闸门串联记账。60s ⇒ 最坏 2 轮 ≈ 24 次上游调用。调大买到的是延迟不是成功率（外挂 shield 用 600s 换来 p50 73.2s）。",
  "settingspage.absorb.budget.label": "总预算（秒）",
  "settingspage.absorb.enabled.hint": "客户端还未收到任何字节时，网关就地退避重打可恢复的 429（全池冷却 / 上游速率限流），不把 429 吐给客户端。默认关＝逐字节等价旧行为。⚠️ 只压客户端可见的 429，不减少打向上游的请求量。",
  "settingspage.absorb.enabled.label": "启用上游 429 吸收层",
  "settingspage.absorb.maxAttempts.aria": "吸收层最大额外轮次",
  "settingspage.absorb.maxAttempts.hint": "额外重试轮次上限，与总预算取先到者。0＝只打一次，等于关闭吸收。",
  "settingspage.absorb.maxAttempts.label": "最大轮次",
  "settingspage.absorb.maxDelay.aria": "吸收层退避上限秒数",
  "settingspage.absorb.maxDelay.hint": "退避上限。凭据池给出的真实恢复秒数再大也 clamp 到此值，防单请求长挂。",
  "settingspage.absorb.maxDelay.label": "退避上限（秒）",
  "settingspage.absorb.minDelay.aria": "吸收层退避下限毫秒",
  "settingspage.absorb.minDelay.hint": "退避下限。凭据池冷却常几十~几百毫秒即恢复，外挂 shield 的 1s 硬下限会把 50ms 的恢复睡满 1s。低于 50ms 无意义且接近忙等。",
  "settingspage.absorb.minDelay.label": "退避下限（毫秒）",
  "settingspage.absorb.shieldUrl.aria": "外置 shield 统计地址",
  "settingspage.absorb.shieldUrl.hint": "可选。仍有外置 kiro_shield.py 旁挂时填它的统计地址（如 http://127.0.0.1:8993/_shield/stats），运维页即显示它的吸收统计。留空＝不采集、不显示该卡。仅接受环回地址，且网关绝不因它拉取失败而影响任何请求。",
  "settingspage.absorb.shieldUrl.label": "外置 shield 统计地址（选填）",
  "settingspage.absorb.suspended.hint": "⚠️ 不建议，默认关。403「temporarily is suspended」是账号级限时态，窗口约 10 分钟 ≫ 单请求预算，窗口内重试成功率接近 0，只会把必然失败推迟到预算耗尽再返回；更严重的是与自愈退避冲突——往刚被 403 的账号继续打会加深封禁（实测 41 分钟触发 36 次）。族级连坐已让同族全退，外层再打只是扩大受害面。",
  "settingspage.absorb.suspended.label": "同时吸收 403 临时风控（不建议）",

  "settingspage.card.absorb": "上游 429 吸收层",
  "settingspage.card.absorb.kw": "429 吸收,吸收层,内置 shield,shield,重试,退避,总预算,最大轮次,退避下限,退避上限,403 风控,temporarily suspended,客户端不见 429,absorb,retry,backoff",
```

**en.json**

```json
  "opspage.metric.absorbAttempts": "Absorb retry rounds",
  "opspage.metric.absorbBudgetExhausted": "Absorb budget exhausted",
  "opspage.metric.absorbRecovered": "Absorbed (client saw no 429)",
  "opspage.metric.absorbRounds1": "Recovered in 1 round",
  "opspage.metric.absorbRounds2": "Recovered in 2 rounds",
  "opspage.metric.absorbRounds3plus": "Recovered in 3+ rounds",
  "opspage.metric.absorbSuspendSkipped": "403 suspension skipped",

  "opspage.shield.absorbed": "Absorbed",
  "opspage.shield.byStatus": "By status",
  "opspage.shield.coolWaits": "Cooldown waits",
  "opspage.shield.gaveUp": "Gave up",
  "opspage.shield.notConfigured": "No external shield stats URL configured",
  "opspage.shield.ratio": "Absorb ratio {ratio}",
  "opspage.shield.requests": "Requests",
  "opspage.shield.retries": "Retries",
  "opspage.shield.subtitle": "Counters from the external process, independent of the gateway counters above. This card disappears once the shield is retired.",
  "opspage.shield.swapGaveUp": "Swap gave up",
  "opspage.shield.swapWaits": "Swap waits",
  "opspage.shield.title": "External shield (sidecar)",
  "opspage.shield.unreachable": "Configured but unreachable — the external shield may have stopped",

  "settingspage.absorb.budget.aria": "Absorption total budget in seconds",
  "settingspage.absorb.budget.hint": "Absolute deadline from request entry, accounted in series with the pool's 12 credential hops / 45s gate. 60s means at most 2 rounds (~24 upstream calls). Raising it buys latency, not success rate (the external shield's 600s bought a p50 of 73.2s).",
  "settingspage.absorb.budget.label": "Total budget (s)",
  "settingspage.absorb.enabled.hint": "While the client has received zero bytes, the gateway retries recoverable 429s in place (pool-wide cooldown / upstream rate limit) instead of returning 429. Off by default = byte-for-byte the old behaviour. Note: this only hides client-visible 429s, it does not reduce upstream load.",
  "settingspage.absorb.enabled.label": "Enable upstream 429 absorption",
  "settingspage.absorb.maxAttempts.aria": "Absorption max extra rounds",
  "settingspage.absorb.maxAttempts.hint": "Cap on extra retry rounds; whichever hits first, this or the total budget. 0 = single attempt, i.e. absorption off.",
  "settingspage.absorb.maxAttempts.label": "Max rounds",
  "settingspage.absorb.maxDelay.aria": "Absorption max backoff in seconds",
  "settingspage.absorb.maxDelay.hint": "Backoff ceiling. Even a larger real recovery time from the pool is clamped here so a single request never hangs long.",
  "settingspage.absorb.maxDelay.label": "Max backoff (s)",
  "settingspage.absorb.minDelay.aria": "Absorption min backoff in milliseconds",
  "settingspage.absorb.minDelay.hint": "Backoff floor. Pool cooldowns often clear in tens to hundreds of milliseconds; the external shield's hard 1s floor sleeps a full second on a 50ms recovery. Below 50ms is pointless and close to busy-waiting.",
  "settingspage.absorb.minDelay.label": "Min backoff (ms)",
  "settingspage.absorb.shieldUrl.aria": "External shield stats URL",
  "settingspage.absorb.shieldUrl.hint": "Optional. If an external kiro_shield.py is still in front, point this at its stats endpoint (e.g. http://127.0.0.1:8993/_shield/stats) to show its absorption counters on the Ops page. Empty = not collected, card hidden. Loopback addresses only, and a fetch failure never affects any request.",
  "settingspage.absorb.shieldUrl.label": "External shield stats URL (optional)",
  "settingspage.absorb.suspended.hint": "Not recommended, off by default. A 403 \"temporarily is suspended\" is an account-level timed state whose window (~10 min) far exceeds any per-request budget, so in-window retries succeed near never — it just defers an inevitable failure until the budget runs out. Worse, it fights the self-heal backoff: hammering an account that was just 403'd deepens the ban (measured 36 hits in 41 minutes). Family-level cooldown already parks the whole family; retrying outward only widens the blast radius.",
  "settingspage.absorb.suspended.label": "Also absorb 403 temporary suspension (not recommended)",

  "settingspage.card.absorb": "Upstream 429 absorption",
  "settingspage.card.absorb.kw": "429 absorption,absorb,builtin shield,shield,retry,backoff,total budget,max rounds,min backoff,max backoff,403 suspension,temporarily suspended,hide 429 from client,吸收",
```

**ja.json**

```json
  "opspage.metric.absorbAttempts": "吸収リトライ回数",
  "opspage.metric.absorbBudgetExhausted": "吸収予算の枯渇",
  "opspage.metric.absorbRecovered": "吸収成功（クライアントは 429 未見）",
  "opspage.metric.absorbRounds1": "1 回で成功",
  "opspage.metric.absorbRounds2": "2 回で成功",
  "opspage.metric.absorbRounds3plus": "3 回以上で成功",
  "opspage.metric.absorbSuspendSkipped": "403 制限はスキップ",

  "opspage.shield.absorbed": "吸収済み",
  "opspage.shield.byStatus": "ステータス別",
  "opspage.shield.coolWaits": "クールダウン待ち",
  "opspage.shield.gaveUp": "断念",
  "opspage.shield.notConfigured": "外部 shield の統計 URL が未設定",
  "opspage.shield.ratio": "吸収比 {ratio}",
  "opspage.shield.requests": "リクエスト総数",
  "opspage.shield.retries": "リトライ回数",
  "opspage.shield.subtitle": "外部プロセス自身のカウンタ。上のゲートウェイ側カウンタとは独立。shield 廃止後はこのカードも消えます。",
  "opspage.shield.swapGaveUp": "号切替の断念",
  "opspage.shield.swapWaits": "号切替の待ち",
  "opspage.shield.title": "外部 shield（サイドカー）",
  "opspage.shield.unreachable": "設定済みだが取得失敗。外部 shield が停止している可能性があります",

  "settingspage.absorb.budget.aria": "吸収層の総予算（秒）",
  "settingspage.absorb.budget.hint": "リクエスト受付時点から数える絶対 deadline。プール内部の 12 回の号切替 / 45s ゲートと直列で計上されます。60s なら最悪 2 ラウンド（上流呼び出し約 24 回）。大きくして買えるのは遅延であり成功率ではありません（外部 shield は 600s で p50 73.2s）。",
  "settingspage.absorb.budget.label": "総予算（秒）",
  "settingspage.absorb.enabled.hint": "クライアントへ 1 バイトも送っていない間、回復可能な 429（プール全冷却 / 上流レート制限）をゲートウェイ内で待って再試行し、429 をクライアントへ返しません。既定オフ＝従来と完全同一の挙動。※ 隠せるのはクライアントに見える 429 だけで、上流への送信量は減りません。",
  "settingspage.absorb.enabled.label": "上流 429 吸収層を有効化",
  "settingspage.absorb.maxAttempts.aria": "吸収層の最大追加回数",
  "settingspage.absorb.maxAttempts.hint": "追加リトライ回数の上限。総予算とどちらか先に達した方で打ち切ります。0＝1 回のみ＝吸収なし。",
  "settingspage.absorb.maxAttempts.label": "最大回数",
  "settingspage.absorb.maxDelay.aria": "吸収層の最大待機（秒）",
  "settingspage.absorb.maxDelay.hint": "待機の上限。プールが返す実際の回復秒数がこれより大きくてもこの値に clamp し、単一リクエストの長時間ハングを防ぎます。",
  "settingspage.absorb.maxDelay.label": "最大待機（秒）",
  "settingspage.absorb.minDelay.aria": "吸収層の最小待機（ミリ秒）",
  "settingspage.absorb.minDelay.hint": "待機の下限。プールの冷却は数十〜数百ミリ秒で明けることが多く、外部 shield の 1 秒固定下限では 50ms の回復でも 1 秒寝てしまいます。50ms 未満は無意味でビジーウェイトに近づきます。",
  "settingspage.absorb.minDelay.label": "最小待機（ミリ秒）",
  "settingspage.absorb.shieldUrl.aria": "外部 shield の統計 URL",
  "settingspage.absorb.shieldUrl.hint": "任意。外部 kiro_shield.py がまだ前段にある場合、その統計エンドポイント（例 http://127.0.0.1:8993/_shield/stats）を指定すると運用ページに吸収統計を表示します。空＝収集せずカード非表示。ループバックのみ許可し、取得失敗がリクエストに影響することはありません。",
  "settingspage.absorb.shieldUrl.label": "外部 shield の統計 URL（任意）",
  "settingspage.absorb.suspended.hint": "※ 非推奨・既定オフ。403「temporarily is suspended」はアカウント単位の一時状態で、その窓（約 10 分）は単一リクエストの予算をはるかに超えるため、窓内の再試行はほぼ成功しません。避けられない失敗を予算切れまで遅らせるだけです。さらに自己修復のバックオフと衝突し、403 を受けた直後のアカウントへ打ち続けると制限が深まります（実測 41 分で 36 回）。ファミリー連座で同族は既に退避済みで、外側から打つのは被害範囲を広げるだけです。",
  "settingspage.absorb.suspended.label": "403 一時制限も吸収する（非推奨）",

  "settingspage.card.absorb": "上流 429 吸収層",
  "settingspage.card.absorb.kw": "429 吸収,吸収層,内蔵 shield,shield,リトライ,バックオフ,総予算,最大回数,最小待機,最大待機,403 制限,temporarily suspended,クライアントに 429 を見せない,absorb,retry,backoff",
```

## 4. 外挂 shield 统计怎么进面板（不硬依赖它）

浏览器直接 fetch `:8993/_shield/stats` 走不通（跨源 + 该端口通常不对外）。方案是
**网关侧极窄的只读代理 + 前端软失败**：

1. 配置：`shield_stats_url: Option<String>`（默认 `None`），走 §1 同一张卡的 `<Input>`。
   ⚠️ **不能用 `ssrf::validate_outbound_url`** —— 连 `SsrfPolicy::AdminConfigured` 也只豁免
   `198.18.0.0/15`，`127.0.0.0/8` 依旧拦（`src/common/ssrf.rs:61` 一带），照抄必然永远拒绝。
   改用专用窄校验：scheme 限 `http`/`https`、host 解析后**必须环回**、禁重定向、2s 超时、
   经 `common::http_read::read_body_capped` 限 64KiB。
2. 端点：`GET /api/admin/shield-stats`（`src/admin/router.rs:130` 的 `recovery-metrics` 旁），
   **恒返 200**，三态由 body 表达：`{available:false, reason:"not_configured"}` /
   `{available:false, reason:"unreachable"}` / `{available:true, fetchedAtMs, stats:{...}}`。
   恒 200 是为了让前端不必把「没配」当错误弹 toast。
3. 前端：`api/ops.ts` 加 `ShieldStats`（7 个计数全 `?: number` + `byStatus?: Record<string, number>`）
   与 `getShieldStats()`；`ops-page.tsx:181` 的 `<RecoveryMetricsCard />` 后挂 `<ShieldSidecarCard />`，
   `useQuery({ refetchInterval: 10000, retry: false })`。
4. **优雅降级**（三档，缺一档就变成「多一个假故障」）：
   - `not_configured` → **整卡不渲染**（`return null`）。绝大多数用户没有外挂，不该看到空卡。
   - `unreachable` → 渲染卡片 + `<Callout variant="warning">opspage.shield.unreachable`。
     配了却拉不到是真信息（shield 停了），但**只是提示**，不影响任何请求。
   - `available` → 7 个 `<StatCard>`（沿用 §2 的 `?? 0` 兜底）+ `byStatus` 用 `<Badge>` 平铺 +
     副标题 `opspage.shield.subtitle` 明说「外置进程自身计数，与网关计数器独立」。
     必须写这句：两套计数口径不同（外挂是整请求重打，网关是号级换号），混看会得出错误结论。
5. 退役即消失：`SPEC-shield-builtin.md` §6 第 5 步停掉 shield 后，把配置留空即回到
   `not_configured` → 卡片自动消失，无需再改前端。

## 5. 风险文案

六项的中文 hint 已写进 §3 zh 列（风险落在用户看得见的地方，不是只写在文档里）：
`enabled` 标明「不减少上游请求量」防误判无效；`budget` 给 600s→p50 73.2s 实测说明「买到的是
延迟不是成功率」；`minDelay` 标明外挂 1s 下限的具体损失；`suspended` **默认关**、label 直接带
「（不建议）」、hint 三段论说清「窗口 10 分钟 ≫ 预算 / 与自愈退避冲突加深封禁（41 分钟 36 次）/
族级连坐已覆盖」。
