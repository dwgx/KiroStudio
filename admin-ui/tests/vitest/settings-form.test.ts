/**
 * `toForm` / `diffForm` — settings snapshot ↔ form mapping (no React page).
 *
 * Round-trip empty-diff uses a snapshot that **already has** absorb / mock / tool /
 * login fields (same values as the `??` baselines). A missing-fields snapshot is
 * only for `toForm` defaults: several numeric diffs compare `config.foo` without
 * `??` (e.g. `rpmHeadroomFactor`), so sparse input would emit those baselines.
 *
 * # 跑法
 *
 * ```bash
 * cd admin-ui && corepack pnpm test:vitest
 * ```
 */
import { describe, expect, test } from 'vitest'
import { toForm, diffForm } from '@/lib/settings-form'
import type { UiLayoutPrefs } from '@/hooks/use-ui-layout-prefs'
import type { ConfigSnapshotResponse } from '@/types/api'

const UI: UiLayoutPrefs = {
  poolSort: 'health',
  poolShowDisabled: true,
  showPerfDashboard: true,
  cardSize: 'standard',
  credentialView: 'card',
}

/** Full snapshot: required fields present, absorb/mock/tool/login at the `??` baselines. */
function snapshot(overrides: Partial<ConfigSnapshotResponse> = {}): ConfigSnapshotResponse {
  return {
    serverVersion: '1.1.2',
    host: '127.0.0.1',
    port: 8080,
    region: 'us-east-1',
    kiroVersion: '0.3.16',
    systemVersion: 'win32',
    nodeVersion: '20.11.1',
    tlsBackend: 'rustls',
    loadBalancingMode: 'priority',
    defaultEndpoint: 'ide',
    endpointNames: ['ide', 'cli'],
    extractThinking: true,
    ccAutoBuffer: true,
    selfHealBaseBackoffSecs: 60,
    selfHealMaxBackoffSecs: 900,
    selfHealMaxShift: 4,
    promptCacheEnabled: true,
    mockCacheEnabled: false,
    mockCacheReadRatio: 0.7,
    nativeThinkingEffortEnabled: false,
    toolCompatMapping: true,
    importKeysEnabled: true,
    upstreamRetryAbsorbEnabled: false,
    upstreamRetryAbsorbBudgetSecs: 45,
    upstreamRetryAbsorbMaxRounds: 3,
    upstreamRetryAbsorbMinDelayMs: 150,
    upstreamRetryAbsorbMaxDelaySecs: 15,
    upstreamRetryAbsorbSuspended: false,
    upstreamRetryAbsorbServerError: false,
    upstreamRetryAbsorbCapacity400: false,
    upstreamRetryAbsorbSwapBudgetSecs: 0,
    upstreamRetryAbsorbExhaustedStatus: 503,
    stripEnvNoise: true,
    toolCleanLeakedTokens: true,
    toolReclaimTextifiedInvoke: true,
    toolStrayRepeatGuard: true,
    toolStreamAlignFailure: true,
    toolExposeErrorToClient: true,
    toolRepairJson: true,
    toolTruncationRecovery: false,
    toolDescriptionMaxChars: 10000,
    cliOriginKiroCli: false,
    cliCodewhispererOptoutFalse: false,
    cliUaAlignRealClient: false,
    upstreamPerCredentialLimit: 2,
    encryptCredentialsAtRest: false,
    cooldownEnabled: true,
    autoDisableSuspicious: true,
    autoDisableQuotaExceeded: true,
    socksAutoHealth: true,
    otaAutoCheck: false,
    cloneDefaultEnabled: false,
    allCoolingFastFail: true,
    rateLimitEnabled: false,
    rateLimitDailyMax: 0,
    rateLimitMinIntervalMs: 0,
    affinityEnabled: true,
    priorityInBalanced: false,
    credentialRpmLimit: 0,
    rpmHeadroomFactor: 85,
    rpmReserveSlots: 0,
    rpmHardGateOverloadWait: false,
    cooldownScalePct: 100,
    rateLimitJitterPct: 20,
    throttleProfile: 'manual',
    schedulingMode: 'smart',
    inboundThrottleEnabled: true,
    inboundRpmAuto: true,
    inboundTargetRpm: 100,
    inboundRpmMin: 20,
    inboundRpmMax: 300,
    inboundBurstSecs: 2,
    inboundQueueMaxWaitSecs: 30,
    inboundQueueTimeoutPassthrough: true,
    inboundCurrentRpm: 0,
    balanceWeightEnabled: true,
    balanceWeightFloor: 50,
    health429WeightEnabled: true,
    hasProxy: false,
    hasAdminKey: true,
    hasApiKey: false,
    callbackMode: 'none',
    corsAllowedOrigins: [],
    ipAllowlist: [],
    ipBlocklist: [],
    machineCodeBlocklist: [],
    trustForwardedHeader: false,
    ingressRateLimitPerMin: 0,
    maxBodyBytes: 10485760,
    proactiveTokenRefresh: true,
    tokenRefreshLeadMinutes: 5,
    tokenRefreshIntervalSecs: 60,
    balanceRefreshIntervalSecs: 0,
    loginBackgroundEnabled: true,
    loginBackgroundR18: false,
    modelMapping: {},
    ...overrides,
  }
}

function omit(
  c: ConfigSnapshotResponse,
  keys: (keyof ConfigSnapshotResponse)[],
): ConfigSnapshotResponse {
  const out = { ...c } as Record<string, unknown>
  for (const k of keys) delete out[k]
  return out as ConfigSnapshotResponse
}

const MISSING_ABSORB_MOCK_TOOL: (keyof ConfigSnapshotResponse)[] = [
  'upstreamRetryAbsorbEnabled',
  'upstreamRetryAbsorbBudgetSecs',
  'upstreamRetryAbsorbMaxRounds',
  'upstreamRetryAbsorbMinDelayMs',
  'upstreamRetryAbsorbMaxDelaySecs',
  'upstreamRetryAbsorbSuspended',
  'upstreamRetryAbsorbServerError',
  'upstreamRetryAbsorbCapacity400',
  'upstreamRetryAbsorbSwapBudgetSecs',
  'mockCacheEnabled',
  'mockCacheReadRatio',
  'toolCleanLeakedTokens',
  'toolReclaimTextifiedInvoke',
  'toolStrayRepeatGuard',
  'toolStreamAlignFailure',
  'toolExposeErrorToClient',
  'toolRepairJson',
  'toolTruncationRecovery',
  'toolDescriptionMaxChars',
  'promptCacheEnabled',
  'toolCompatMapping',
  'loginBackgroundEnabled',
  'loginBackgroundR18',
]

describe('toForm', () => {
  test('missing absorb / mock / tool / login fields use the old inline ?? defaults', () => {
    const form = toForm(omit(snapshot(), MISSING_ABSORB_MOCK_TOOL), UI)
    expect(form.upstreamRetryAbsorbEnabled).toBe(false)
    expect(form.upstreamRetryAbsorbBudgetSecs).toBe('45')
    expect(form.upstreamRetryAbsorbMaxRounds).toBe('3')
    expect(form.upstreamRetryAbsorbMinDelayMs).toBe('150')
    expect(form.upstreamRetryAbsorbMaxDelaySecs).toBe('15')
    expect(form.upstreamRetryAbsorbSwapBudgetSecs).toBe('0')
    expect(form.upstreamRetryAbsorbSuspended).toBe(false)
    expect(form.mockCacheEnabled).toBe(false)
    expect(form.mockCacheReadRatioPct).toBe('70')
    expect(form.toolStreamAlignFailure).toBe(true)
    expect(form.toolCleanLeakedTokens).toBe(true)
    expect(form.toolReclaimTextifiedInvoke).toBe(true)
    expect(form.toolStrayRepeatGuard).toBe(true)
    expect(form.toolExposeErrorToClient).toBe(true)
    expect(form.toolRepairJson).toBe(true)
    expect(form.toolTruncationRecovery).toBe(false)
    expect(form.toolDescriptionMaxChars).toBe('10000')
    expect(form.promptCacheEnabled).toBe(true)
    expect(form.toolCompatMapping).toBe(true)
    expect(form.loginBackgroundEnabled).toBe(true)
    expect(form.loginBackgroundR18).toBe(false)
  })
})

describe('diffForm', () => {
  test('round-trip of a snapshot that already has baseline fields is {}', () => {
    const config = snapshot()
    expect(diffForm(config, toForm(config, UI))).toEqual({})
  })

  test('absorb budget 45 → 60 is the only key in the patch', () => {
    const config = snapshot()
    const form = { ...toForm(config, UI), upstreamRetryAbsorbBudgetSecs: '60' }
    expect(diffForm(config, form)).toEqual({ upstreamRetryAbsorbBudgetSecs: 60 })
  })

  test('empty mockCacheReadRatioPct does not submit 0 (Number("") === 0 trap)', () => {
    const config = snapshot()
    const base = toForm(config, UI)
    expect(diffForm(config, { ...base, mockCacheReadRatioPct: '' })).toEqual({})
    expect(diffForm(config, { ...base, mockCacheReadRatioPct: '  ' })).toEqual({})
    // Explicit 0 is a real edit against the 70% baseline — only empty is skipped.
    expect(diffForm(config, { ...base, mockCacheReadRatioPct: '0' })).toEqual({
      mockCacheReadRatio: 0,
    })
  })

  test('illegal / array modelMapping is not submitted; clearing a mapped baseline submits {}', () => {
    const config = snapshot({ modelMapping: { 'claude-sonnet': 'kiro-sonnet' } })
    const base = toForm(config, UI)
    expect(diffForm(config, { ...base, modelMapping: 'not-json' })).toEqual({})
    expect(diffForm(config, { ...base, modelMapping: '["a"]' })).toEqual({})
    expect(diffForm(config, { ...base, modelMapping: 'null' })).toEqual({})
    expect(diffForm(config, { ...base, modelMapping: '{"a":123}' })).toEqual({})
    expect(diffForm(config, { ...base, modelMapping: '' })).toEqual({ modelMapping: {} })
    expect(diffForm(config, { ...base, modelMapping: '  \n' })).toEqual({ modelMapping: {} })
  })
})
