/**
 * Settings form mapping: ConfigSnapshot ↔ FormState and the save-path diff.
 *
 * Defaults live here once (`toForm` `??` and the matching `diffForm` baselines).
 * The 8 setting cards stay in `settings-page.tsx`; that file imports `toForm`,
 * `diffForm`, and `FormState` from here.
 */

import type { UiLayoutPrefs, PoolSortMode, CardSize } from '@/hooks/use-ui-layout-prefs'
import type { ConfigSnapshotResponse, UpdateConfigRequest, SchedulingMode } from '@/types/api'

/** 可编辑表单的本地状态（字符串化便于受控输入） */
export interface FormState {
  host: string
  port: string
  region: string
  kiroVersion: string
  systemVersion: string
  nodeVersion: string
  tlsBackend: string
  loadBalancingMode: string
  defaultEndpoint: string
  extractThinking: boolean
  ccAutoBuffer: boolean
  importKeysEnabled: boolean
  // 上游 429 吸收层（数值字段同样字符串化，与其余受控数字输入一致）
  upstreamRetryAbsorbEnabled: boolean
  upstreamRetryAbsorbBudgetSecs: string
  upstreamRetryAbsorbMaxRounds: string
  upstreamRetryAbsorbMinDelayMs: string
  upstreamRetryAbsorbMaxDelaySecs: string
  upstreamRetryAbsorbSuspended: boolean
  upstreamRetryAbsorbServerError: boolean
  upstreamRetryAbsorbCapacity400: boolean
  upstreamRetryAbsorbSwapBudgetSecs: string
  /** 勾选 = 耗尽时返回 429（默认 503）；变量名保留 exhausted503 历史命名 */
  upstreamRetryAbsorbExhausted503: boolean
  stripEnvNoise: boolean
  toolCleanLeakedTokens: boolean
  toolReclaimTextifiedInvoke: boolean
  toolStrayRepeatGuard: boolean
  toolStreamAlignFailure: boolean
  toolExposeErrorToClient: boolean
  toolRepairJson: boolean
  toolTruncationRecovery: boolean
  toolDescriptionMaxChars: string
  // CLI 端点协议/指纹三开关（详见各自 hint 与后端 config 字段注释）
  cliOriginKiroCli: boolean
  cliCodewhispererOptoutFalse: boolean
  cliUaAlignRealClient: boolean
  // prompt cache 记账下发开关（估算值，非上游真值）
  promptCacheEnabled: boolean
  // 模拟缓存：透传响应注入模拟 cache_read（比例以整数百分比存储，保存时换算 0..1）
  mockCacheEnabled: boolean
  mockCacheReadRatioPct: string
  selfHealBaseBackoffSecs: string
  selfHealMaxBackoffSecs: string
  selfHealMaxShift: string
  nativeThinkingEffortEnabled: boolean
  // CC↔Kiro 工具名/参数映射开关（默认开；热更即时生效）
  toolCompatMapping: boolean
  encryptCredentialsAtRest: boolean
  cooldownEnabled: boolean
  autoDisableSuspicious: boolean
  // 内存态开关：不进 config.json、重启回默认，只能通过 PUT /config 改（详见后端内存开关清单）
  autoDisableQuotaExceeded: boolean
  socksAutoHealth: boolean
  otaAutoCheck: boolean
  allCoolingFastFail: boolean
  rateLimitEnabled: boolean
  rateLimitDailyMax: string
  rateLimitMinIntervalMs: string
  affinityEnabled: boolean
  priorityInBalanced: boolean
  // 智能调度（0.7.23/0.7.24，均热更即时生效）
  credentialRpmLimit: string
  rpmHeadroomFactor: string
  rpmReserveSlots: string
  rpmHardGateOverloadWait: boolean
  cooldownScalePct: string
  rateLimitJitterPct: string
  // 调度模式（三按钮，2026-08-16）：smart/stable/manual。后端映射到 ThrottleProfile 写矩阵；
  // throttleProfile 不再是 UI 字段（下拉已由三按钮取代）。
  schedulingMode: SchedulingMode
  inboundThrottleEnabled: boolean
  inboundRpmAuto: boolean
  inboundTargetRpm: string
  inboundRpmMin: string
  inboundRpmMax: string
  inboundBurstSecs: string
  inboundQueueMaxWaitSecs: string
  inboundQueueTimeoutPassthrough: boolean
  balanceWeightEnabled: boolean
  balanceWeightFloor: string
  health429WeightEnabled: boolean
  proxyUrl: string
  proxyUsername: string
  proxyPassword: string
  apiKey: string
  callbackBaseUrl: string
  // 反代安全（批次3）：列表用换行分隔的多行文本承载
  corsAllowedOrigins: string
  ipAllowlist: string
  ipBlocklist: string
  machineCodeBlocklist: string
  trustForwardedHeader: boolean
  ingressRateLimitPerMin: string
  maxBodyBytes: string
  // 主动 token 预刷新（批次4.4）
  proactiveTokenRefresh: boolean
  tokenRefreshLeadMinutes: string
  tokenRefreshIntervalSecs: string
  // Admin UI 登录页背景（立即生效）
  loginBackgroundEnabled: boolean
  loginBackgroundR18: boolean
  // UI 排版自定义（纯前端 localStorage，纳入统一保存流程：切换改 form，保存时才落地）
  poolSort: PoolSortMode
  poolShowDisabled: boolean
  showPerfDashboard: boolean
  cardSize: CardSize
  // 全局模型映射（JSON 文本，双口径用量的 requested→upstream；空对象 = 不映射）
  modelMapping: string
}

/* ============ promptCacheEnabled 的本地类型补丁 ============ */
// 后端两侧都已有该字段（`src/admin/types.rs:898` 响应 / `:1031` 请求，serde camelCase），
// 但 `src/types/api.ts` 的两个接口里还没有它。api.ts 此刻正被其他会话改，
// 不在本次可改文件内，故在本文件内做最小补丁；等 api.ts 补上字段后删掉这两个别名即可。
type ConfigWithCache = ConfigSnapshotResponse & {
  promptCacheEnabled?: boolean
  autoDisableQuotaExceeded?: boolean
  socksAutoHealth?: boolean
  otaAutoCheck?: boolean
}
type UpdateWithCache = UpdateConfigRequest & {
  promptCacheEnabled?: boolean
  autoDisableQuotaExceeded?: boolean
  socksAutoHealth?: boolean
  otaAutoCheck?: boolean
}

// 后端 `default_prompt_cache_enabled()`（`src/model/config.rs:885`）返回 true，
// 字段缺失时按 true 兜底，避免面板把"未下发"显示成"已关闭"。
const PROMPT_CACHE_DEFAULT = true

// 模拟缓存默认值（与后端 `default_mock_cache_*()` 对齐）：开关关、比例 0.7。
// 字段缺失时按此兜底，避免面板把"未下发"显示成"已关闭/0%"。
const MOCK_CACHE_ENABLED_DEFAULT = false
const MOCK_CACHE_RATIO_DEFAULT = 0.7

// 工具映射开关默认值（与后端 `default_tool_compat_mapping()` 对齐）：开。
// 字段缺失时按此兜底，避免面板把"未下发"显示成"已关闭"。
const TOOL_COMPAT_MAPPING_DEFAULT = true

// 多行文本 <-> 字符串列表（去空白、去空行）
function linesToList(s: string): string[] {
  return s
    .split('\n')
    .map((l) => l.trim())
    .filter((l) => l.length > 0)
}

function listToLines(list: string[]): string {
  return list.join('\n')
}

// 比较两个字符串列表是否等价（顺序敏感）
function sameList(a: string[], b: string[]): boolean {
  return a.length === b.length && a.every((v, i) => v === b[i])
}

export function toForm(c: ConfigSnapshotResponse, ui: UiLayoutPrefs): FormState {
  return {
    host: c.host,
    port: String(c.port),
    region: c.region,
    kiroVersion: c.kiroVersion,
    systemVersion: c.systemVersion,
    nodeVersion: c.nodeVersion,
    tlsBackend: c.tlsBackend,
    loadBalancingMode: c.loadBalancingMode,
    defaultEndpoint: c.defaultEndpoint,
    extractThinking: c.extractThinking,
    ccAutoBuffer: c.ccAutoBuffer,
    importKeysEnabled: c.importKeysEnabled,
    upstreamRetryAbsorbEnabled: c.upstreamRetryAbsorbEnabled ?? false,
    upstreamRetryAbsorbBudgetSecs: String(c.upstreamRetryAbsorbBudgetSecs ?? 45),
    upstreamRetryAbsorbMaxRounds: String(c.upstreamRetryAbsorbMaxRounds ?? 3),
    upstreamRetryAbsorbMinDelayMs: String(c.upstreamRetryAbsorbMinDelayMs ?? 150),
    upstreamRetryAbsorbMaxDelaySecs: String(c.upstreamRetryAbsorbMaxDelaySecs ?? 15),
    upstreamRetryAbsorbSuspended: c.upstreamRetryAbsorbSuspended ?? false,
    upstreamRetryAbsorbServerError: c.upstreamRetryAbsorbServerError ?? false,
    upstreamRetryAbsorbCapacity400: c.upstreamRetryAbsorbCapacity400 ?? false,
    upstreamRetryAbsorbSwapBudgetSecs: String(c.upstreamRetryAbsorbSwapBudgetSecs ?? 0),
    upstreamRetryAbsorbExhausted503: c.upstreamRetryAbsorbExhaustedStatus === 429,
    stripEnvNoise: c.stripEnvNoise,
    toolCleanLeakedTokens: c.toolCleanLeakedTokens ?? true,
    toolReclaimTextifiedInvoke: c.toolReclaimTextifiedInvoke ?? true,
    toolStrayRepeatGuard: c.toolStrayRepeatGuard ?? true,
    toolStreamAlignFailure: c.toolStreamAlignFailure ?? true,
    toolExposeErrorToClient: c.toolExposeErrorToClient ?? true,
    toolRepairJson: c.toolRepairJson ?? true,
    toolTruncationRecovery: c.toolTruncationRecovery ?? false,
    toolDescriptionMaxChars: String(c.toolDescriptionMaxChars ?? 10000),
    // 三项默认 false：与后端 serde default 一致，保证「未配置」在前后端读到同一值。
    cliOriginKiroCli: c.cliOriginKiroCli ?? false,
    cliCodewhispererOptoutFalse: c.cliCodewhispererOptoutFalse ?? false,
    cliUaAlignRealClient: c.cliUaAlignRealClient ?? false,
    promptCacheEnabled: (c as ConfigWithCache).promptCacheEnabled ?? PROMPT_CACHE_DEFAULT,
    // 模拟缓存：比例 f64(0..1) → 整数百分比（70% 等），保存时反向换算
    mockCacheEnabled: c.mockCacheEnabled ?? MOCK_CACHE_ENABLED_DEFAULT,
    mockCacheReadRatioPct: String(Math.round((c.mockCacheReadRatio ?? MOCK_CACHE_RATIO_DEFAULT) * 100)),
    selfHealBaseBackoffSecs: String(c.selfHealBaseBackoffSecs ?? 60),
    selfHealMaxBackoffSecs: String(c.selfHealMaxBackoffSecs ?? 900),
    selfHealMaxShift: String(c.selfHealMaxShift ?? 4),
    nativeThinkingEffortEnabled: c.nativeThinkingEffortEnabled ?? false,
    toolCompatMapping: c.toolCompatMapping ?? TOOL_COMPAT_MAPPING_DEFAULT,
    encryptCredentialsAtRest: c.encryptCredentialsAtRest ?? false,
    cooldownEnabled: c.cooldownEnabled,
    autoDisableSuspicious: c.autoDisableSuspicious ?? true,
    autoDisableQuotaExceeded: (c as ConfigWithCache).autoDisableQuotaExceeded ?? true,
    socksAutoHealth: (c as ConfigWithCache).socksAutoHealth ?? true,
    otaAutoCheck: (c as ConfigWithCache).otaAutoCheck ?? false,
    allCoolingFastFail: c.allCoolingFastFail ?? true,
    rateLimitEnabled: c.rateLimitEnabled,
    rateLimitDailyMax: String(c.rateLimitDailyMax),
    rateLimitMinIntervalMs: String(c.rateLimitMinIntervalMs),
    affinityEnabled: c.affinityEnabled,
    priorityInBalanced: c.priorityInBalanced,
    credentialRpmLimit: String(c.credentialRpmLimit ?? 0),
    rpmHeadroomFactor: String(c.rpmHeadroomFactor ?? 85),
    rpmReserveSlots: String(c.rpmReserveSlots ?? 0),
    rpmHardGateOverloadWait: c.rpmHardGateOverloadWait ?? false,
    cooldownScalePct: String(c.cooldownScalePct ?? 100),
    rateLimitJitterPct: String(c.rateLimitJitterPct ?? 20),
    // 旧后端不下发 schedulingMode → 按 smart 兜底（与后端 serde 默认一致）
    schedulingMode: c.schedulingMode ?? 'smart',
    inboundThrottleEnabled: c.inboundThrottleEnabled ?? true,
    inboundRpmAuto: c.inboundRpmAuto ?? true,
    inboundTargetRpm: String(c.inboundTargetRpm ?? 100),
    inboundRpmMin: String(c.inboundRpmMin ?? 20),
    inboundRpmMax: String(c.inboundRpmMax ?? 300),
    inboundBurstSecs: String(c.inboundBurstSecs ?? 2),
    inboundQueueMaxWaitSecs: String(c.inboundQueueMaxWaitSecs ?? 30),
    inboundQueueTimeoutPassthrough: c.inboundQueueTimeoutPassthrough ?? true,
    balanceWeightEnabled: c.balanceWeightEnabled ?? true,
    balanceWeightFloor: String(c.balanceWeightFloor ?? 50),
    health429WeightEnabled: c.health429WeightEnabled ?? true,
    proxyUrl: c.proxyUrl ?? '',
    // 代理账密出于安全后端不下发,UI 留空占位:留空=不改,填了=更新。
    proxyUsername: '',
    proxyPassword: '',
    // userKey(对话 api_key)后端不下发明文,留空=不改,填了=更新(需重启生效)。
    apiKey: '',
    callbackBaseUrl: c.callbackBaseUrl ?? '',
    corsAllowedOrigins: listToLines(c.corsAllowedOrigins ?? []),
    ipAllowlist: listToLines(c.ipAllowlist ?? []),
    ipBlocklist: listToLines(c.ipBlocklist ?? []),
    machineCodeBlocklist: listToLines(c.machineCodeBlocklist ?? []),
    trustForwardedHeader: c.trustForwardedHeader,
    ingressRateLimitPerMin: String(c.ingressRateLimitPerMin),
    maxBodyBytes: String(c.maxBodyBytes),
    proactiveTokenRefresh: c.proactiveTokenRefresh,
    tokenRefreshLeadMinutes: String(c.tokenRefreshLeadMinutes),
    tokenRefreshIntervalSecs: String(c.tokenRefreshIntervalSecs),
    // 缺省视为开启（后端字段可能尚未下发时不误显示为关闭）
    loginBackgroundEnabled: c.loginBackgroundEnabled ?? true,
    loginBackgroundR18: c.loginBackgroundR18 ?? false,
    // UI 排版偏好（纯前端 localStorage，作为 form 基线纳入统一保存）
    poolSort: ui.poolSort,
    poolShowDisabled: ui.poolShowDisabled,
    showPerfDashboard: ui.showPerfDashboard,
    cardSize: ui.cardSize,
    // 全局模型映射：对象序列化成 JSON 文本（空对象 = 不映射）。非法 JSON 后端会拒绝保存。
    modelMapping: JSON.stringify(c.modelMapping ?? {}, null, 2),
  }
}

/**
 * Patch of fields that differ from the snapshot. Empty object = nothing to PUT.
 * `config`/`form` missing (settings page still loading) → `{}`.
 */
export function diffForm(
  config: ConfigSnapshotResponse | undefined,
  form: FormState | null,
): UpdateConfigRequest {
  if (!config || !form) return {}
  const d: UpdateWithCache = {}
  if (form.host.trim() !== config.host) d.host = form.host.trim()
  const port = Number(form.port)
  if (Number.isFinite(port) && port !== config.port) d.port = port
  if (form.region.trim() !== config.region) d.region = form.region.trim()
  if (form.kiroVersion.trim() !== config.kiroVersion) d.kiroVersion = form.kiroVersion.trim()
  if (form.systemVersion.trim() !== config.systemVersion) d.systemVersion = form.systemVersion.trim()
  if (form.nodeVersion.trim() !== config.nodeVersion) d.nodeVersion = form.nodeVersion.trim()
  if (form.tlsBackend !== config.tlsBackend) d.tlsBackend = form.tlsBackend
  if (form.loadBalancingMode !== config.loadBalancingMode) d.loadBalancingMode = form.loadBalancingMode
  if (form.defaultEndpoint.trim() !== config.defaultEndpoint) d.defaultEndpoint = form.defaultEndpoint.trim()
  if (form.extractThinking !== config.extractThinking) d.extractThinking = form.extractThinking
  if (form.ccAutoBuffer !== config.ccAutoBuffer) d.ccAutoBuffer = form.ccAutoBuffer
  if (form.importKeysEnabled !== config.importKeysEnabled) d.importKeysEnabled = form.importKeysEnabled
  // 上游 429 吸收层：布尔直比，整数解析后比对（空/非法不发）
  if (form.upstreamRetryAbsorbEnabled !== (config.upstreamRetryAbsorbEnabled ?? false)) d.upstreamRetryAbsorbEnabled = form.upstreamRetryAbsorbEnabled
  const nAbsorbBudget = parseInt(form.upstreamRetryAbsorbBudgetSecs, 10)
  if (Number.isFinite(nAbsorbBudget) && nAbsorbBudget !== (config.upstreamRetryAbsorbBudgetSecs ?? 45)) d.upstreamRetryAbsorbBudgetSecs = nAbsorbBudget
  const nAbsorbRounds = parseInt(form.upstreamRetryAbsorbMaxRounds, 10)
  if (Number.isFinite(nAbsorbRounds) && nAbsorbRounds !== (config.upstreamRetryAbsorbMaxRounds ?? 3)) d.upstreamRetryAbsorbMaxRounds = nAbsorbRounds
  const nAbsorbMinDelay = parseInt(form.upstreamRetryAbsorbMinDelayMs, 10)
  if (Number.isFinite(nAbsorbMinDelay) && nAbsorbMinDelay !== (config.upstreamRetryAbsorbMinDelayMs ?? 150)) d.upstreamRetryAbsorbMinDelayMs = nAbsorbMinDelay
  const nAbsorbMaxDelay = parseInt(form.upstreamRetryAbsorbMaxDelaySecs, 10)
  if (Number.isFinite(nAbsorbMaxDelay) && nAbsorbMaxDelay !== (config.upstreamRetryAbsorbMaxDelaySecs ?? 15)) d.upstreamRetryAbsorbMaxDelaySecs = nAbsorbMaxDelay
  if (form.upstreamRetryAbsorbSuspended !== (config.upstreamRetryAbsorbSuspended ?? false)) d.upstreamRetryAbsorbSuspended = form.upstreamRetryAbsorbSuspended
  if (form.upstreamRetryAbsorbServerError !== (config.upstreamRetryAbsorbServerError ?? false)) d.upstreamRetryAbsorbServerError = form.upstreamRetryAbsorbServerError
  if (form.upstreamRetryAbsorbCapacity400 !== (config.upstreamRetryAbsorbCapacity400 ?? false)) d.upstreamRetryAbsorbCapacity400 = form.upstreamRetryAbsorbCapacity400
  const nAbsorbSwapBudget = parseInt(form.upstreamRetryAbsorbSwapBudgetSecs, 10)
  if (Number.isFinite(nAbsorbSwapBudget) && nAbsorbSwapBudget !== (config.upstreamRetryAbsorbSwapBudgetSecs ?? 0)) d.upstreamRetryAbsorbSwapBudgetSecs = nAbsorbSwapBudget
  if (form.upstreamRetryAbsorbExhausted503 !== (config.upstreamRetryAbsorbExhaustedStatus === 429)) d.upstreamRetryAbsorbExhaustedStatus = form.upstreamRetryAbsorbExhausted503 ? 429 : 503
  if (form.stripEnvNoise !== config.stripEnvNoise) d.stripEnvNoise = form.stripEnvNoise
  if (form.toolCleanLeakedTokens !== (config.toolCleanLeakedTokens ?? true)) d.toolCleanLeakedTokens = form.toolCleanLeakedTokens
  if (form.toolReclaimTextifiedInvoke !== (config.toolReclaimTextifiedInvoke ?? true)) d.toolReclaimTextifiedInvoke = form.toolReclaimTextifiedInvoke
  if (form.toolStrayRepeatGuard !== (config.toolStrayRepeatGuard ?? true)) d.toolStrayRepeatGuard = form.toolStrayRepeatGuard
  if (form.toolStreamAlignFailure !== (config.toolStreamAlignFailure ?? true)) d.toolStreamAlignFailure = form.toolStreamAlignFailure
  if (form.toolExposeErrorToClient !== (config.toolExposeErrorToClient ?? true)) d.toolExposeErrorToClient = form.toolExposeErrorToClient
  if (form.toolRepairJson !== (config.toolRepairJson ?? true)) d.toolRepairJson = form.toolRepairJson
  if (form.toolTruncationRecovery !== (config.toolTruncationRecovery ?? false)) d.toolTruncationRecovery = form.toolTruncationRecovery
  const descMax = Number(form.toolDescriptionMaxChars)
  if (Number.isFinite(descMax) && descMax >= 0 && descMax !== (config.toolDescriptionMaxChars ?? 10000)) d.toolDescriptionMaxChars = descMax
  // CLI 三开关：只在与当前值不同时才进 diff（与本文件既有范式一致，避免无谓写盘）。
  if (form.cliOriginKiroCli !== (config.cliOriginKiroCli ?? false)) d.cliOriginKiroCli = form.cliOriginKiroCli
  if (form.cliCodewhispererOptoutFalse !== (config.cliCodewhispererOptoutFalse ?? false))
    d.cliCodewhispererOptoutFalse = form.cliCodewhispererOptoutFalse
  if (form.cliUaAlignRealClient !== (config.cliUaAlignRealClient ?? false)) d.cliUaAlignRealClient = form.cliUaAlignRealClient
  if (form.promptCacheEnabled !== ((config as ConfigWithCache).promptCacheEnabled ?? PROMPT_CACHE_DEFAULT))
    d.promptCacheEnabled = form.promptCacheEnabled
  // 模拟缓存：布尔直比；比例先取整到百分比再与基线比较，避免 f64 精度差误报 diff。
  // 空串显式跳过：Number('') 为 0，直接提交会把「输入框清空（NumberStepper 中间态）」
  // 静默写成 0%——空串不进 diff，字段保持原值，只靠 NumberStepper 的 commit 兜底不够。
  if (form.mockCacheEnabled !== (config.mockCacheEnabled ?? MOCK_CACHE_ENABLED_DEFAULT))
    d.mockCacheEnabled = form.mockCacheEnabled
  const mockRatioRaw = form.mockCacheReadRatioPct.trim()
  if (mockRatioRaw !== '') {
    const mockPct = Math.round(Number(mockRatioRaw))
    if (
      Number.isFinite(mockPct) &&
      mockPct >= 0 &&
      mockPct <= 100 &&
      mockPct !== Math.round((config.mockCacheReadRatio ?? MOCK_CACHE_RATIO_DEFAULT) * 100)
    )
      d.mockCacheReadRatio = mockPct / 100
  }
  const nSelfHealBase = parseInt(form.selfHealBaseBackoffSecs, 10)
  if (Number.isFinite(nSelfHealBase) && nSelfHealBase !== (config.selfHealBaseBackoffSecs ?? 60)) d.selfHealBaseBackoffSecs = nSelfHealBase
  const nSelfHealMax = parseInt(form.selfHealMaxBackoffSecs, 10)
  if (Number.isFinite(nSelfHealMax) && nSelfHealMax !== (config.selfHealMaxBackoffSecs ?? 900)) d.selfHealMaxBackoffSecs = nSelfHealMax
  const nSelfHealShift = parseInt(form.selfHealMaxShift, 10)
  if (Number.isFinite(nSelfHealShift) && nSelfHealShift !== (config.selfHealMaxShift ?? 4)) d.selfHealMaxShift = nSelfHealShift
  if (form.nativeThinkingEffortEnabled !== (config.nativeThinkingEffortEnabled ?? false))
    d.nativeThinkingEffortEnabled = form.nativeThinkingEffortEnabled
  if (form.toolCompatMapping !== (config.toolCompatMapping ?? TOOL_COMPAT_MAPPING_DEFAULT))
    d.toolCompatMapping = form.toolCompatMapping
  if (form.encryptCredentialsAtRest !== (config.encryptCredentialsAtRest ?? false)) d.encryptCredentialsAtRest = form.encryptCredentialsAtRest
  if (form.cooldownEnabled !== config.cooldownEnabled) d.cooldownEnabled = form.cooldownEnabled
  if (form.autoDisableSuspicious !== config.autoDisableSuspicious)
    d.autoDisableSuspicious = form.autoDisableSuspicious
  // 内存态开关：与基线（含缺省兜底）不同才进 diff，避免无谓写盘（同既有范式）。
  if (form.autoDisableQuotaExceeded !== ((config as ConfigWithCache).autoDisableQuotaExceeded ?? true))
    d.autoDisableQuotaExceeded = form.autoDisableQuotaExceeded
  if (form.socksAutoHealth !== ((config as ConfigWithCache).socksAutoHealth ?? true))
    d.socksAutoHealth = form.socksAutoHealth
  if (form.otaAutoCheck !== ((config as ConfigWithCache).otaAutoCheck ?? false))
    d.otaAutoCheck = form.otaAutoCheck
  if (form.allCoolingFastFail !== (config.allCoolingFastFail ?? true)) d.allCoolingFastFail = form.allCoolingFastFail
  if (form.rateLimitEnabled !== config.rateLimitEnabled) d.rateLimitEnabled = form.rateLimitEnabled
  const daily = Number(form.rateLimitDailyMax)
  if (Number.isFinite(daily) && daily !== config.rateLimitDailyMax) d.rateLimitDailyMax = daily
  const interval = Number(form.rateLimitMinIntervalMs)
  if (Number.isFinite(interval) && interval !== config.rateLimitMinIntervalMs) d.rateLimitMinIntervalMs = interval
  if (form.affinityEnabled !== config.affinityEnabled) d.affinityEnabled = form.affinityEnabled
  if (form.priorityInBalanced !== config.priorityInBalanced) d.priorityInBalanced = form.priorityInBalanced
  // 智能调度:整数字段解析后比对(空/非法回退当前值不发)。
  const nCredRpm = parseInt(form.credentialRpmLimit, 10)
  if (Number.isFinite(nCredRpm) && nCredRpm !== (config.credentialRpmLimit ?? 0)) d.credentialRpmLimit = nCredRpm
  const nHeadroom = parseInt(form.rpmHeadroomFactor, 10)
  if (Number.isFinite(nHeadroom) && nHeadroom !== config.rpmHeadroomFactor) d.rpmHeadroomFactor = nHeadroom
  const nReserve = parseInt(form.rpmReserveSlots, 10)
  if (Number.isFinite(nReserve) && nReserve !== config.rpmReserveSlots) d.rpmReserveSlots = nReserve
  if (form.rpmHardGateOverloadWait !== config.rpmHardGateOverloadWait) d.rpmHardGateOverloadWait = form.rpmHardGateOverloadWait
  const nCooldownScale = parseInt(form.cooldownScalePct, 10)
  if (Number.isFinite(nCooldownScale) && nCooldownScale !== (config.cooldownScalePct ?? 100)) d.cooldownScalePct = nCooldownScale
  const nJitter = parseInt(form.rateLimitJitterPct, 10)
  if (Number.isFinite(nJitter) && nJitter !== (config.rateLimitJitterPct ?? 20)) d.rateLimitJitterPct = nJitter
  // 入站整形
  // 调度模式：三按钮切档（后端收到后映射 ThrottleProfile + 写矩阵；throttleProfile
  // 由后端同步，前端不再直接提交它）。
  if (form.schedulingMode !== (config.schedulingMode ?? 'smart')) d.schedulingMode = form.schedulingMode
  if (form.inboundThrottleEnabled !== (config.inboundThrottleEnabled ?? true)) d.inboundThrottleEnabled = form.inboundThrottleEnabled
  if (form.inboundRpmAuto !== (config.inboundRpmAuto ?? true)) d.inboundRpmAuto = form.inboundRpmAuto
  const nTarget = parseInt(form.inboundTargetRpm, 10)
  if (Number.isFinite(nTarget) && nTarget !== (config.inboundTargetRpm ?? 100)) d.inboundTargetRpm = nTarget
  const nRmin = parseInt(form.inboundRpmMin, 10)
  if (Number.isFinite(nRmin) && nRmin !== (config.inboundRpmMin ?? 20)) d.inboundRpmMin = nRmin
  const nRmax = parseInt(form.inboundRpmMax, 10)
  if (Number.isFinite(nRmax) && nRmax !== (config.inboundRpmMax ?? 300)) d.inboundRpmMax = nRmax
  const nBurst = parseInt(form.inboundBurstSecs, 10)
  if (Number.isFinite(nBurst) && nBurst !== (config.inboundBurstSecs ?? 2)) d.inboundBurstSecs = nBurst
  const nQwait = parseInt(form.inboundQueueMaxWaitSecs, 10)
  if (Number.isFinite(nQwait) && nQwait !== (config.inboundQueueMaxWaitSecs ?? 30)) d.inboundQueueMaxWaitSecs = nQwait
  if (form.inboundQueueTimeoutPassthrough !== (config.inboundQueueTimeoutPassthrough ?? true)) d.inboundQueueTimeoutPassthrough = form.inboundQueueTimeoutPassthrough
  if (form.balanceWeightEnabled !== config.balanceWeightEnabled) d.balanceWeightEnabled = form.balanceWeightEnabled
  const nFloor = parseInt(form.balanceWeightFloor, 10)
  if (Number.isFinite(nFloor) && nFloor !== config.balanceWeightFloor) d.balanceWeightFloor = nFloor
  if (form.health429WeightEnabled !== config.health429WeightEnabled) d.health429WeightEnabled = form.health429WeightEnabled
  if (form.proxyUrl.trim() !== (config.proxyUrl ?? '')) d.proxyUrl = form.proxyUrl.trim()
  // 代理账密:后端不下发(安全),故只在用户填了内容时才发送(留空=保持不变)。
  if (form.proxyUsername.trim() !== '') d.proxyUsername = form.proxyUsername.trim()
  if (form.proxyPassword !== '') d.proxyPassword = form.proxyPassword
  if (form.apiKey.trim() !== '') d.apiKey = form.apiKey.trim()
  if (form.callbackBaseUrl.trim() !== (config.callbackBaseUrl ?? '')) d.callbackBaseUrl = form.callbackBaseUrl.trim()
  // 反代安全
  const origins = linesToList(form.corsAllowedOrigins)
  if (!sameList(origins, config.corsAllowedOrigins ?? [])) d.corsAllowedOrigins = origins
  const allowlist = linesToList(form.ipAllowlist)
  if (!sameList(allowlist, config.ipAllowlist ?? [])) d.ipAllowlist = allowlist
  const blocklist = linesToList(form.ipBlocklist)
  if (!sameList(blocklist, config.ipBlocklist ?? [])) d.ipBlocklist = blocklist
  const mcBlocklist = linesToList(form.machineCodeBlocklist)
  if (!sameList(mcBlocklist, config.machineCodeBlocklist ?? [])) d.machineCodeBlocklist = mcBlocklist
  if (form.trustForwardedHeader !== config.trustForwardedHeader) d.trustForwardedHeader = form.trustForwardedHeader
  const ingress = Number(form.ingressRateLimitPerMin)
  if (Number.isFinite(ingress) && ingress !== config.ingressRateLimitPerMin) d.ingressRateLimitPerMin = ingress
  const maxBody = Number(form.maxBodyBytes)
  if (Number.isFinite(maxBody) && maxBody !== config.maxBodyBytes) d.maxBodyBytes = maxBody
  // 主动 token 预刷新
  if (form.proactiveTokenRefresh !== config.proactiveTokenRefresh) d.proactiveTokenRefresh = form.proactiveTokenRefresh
  const lead = Number(form.tokenRefreshLeadMinutes)
  if (Number.isFinite(lead) && lead !== config.tokenRefreshLeadMinutes) d.tokenRefreshLeadMinutes = lead
  const interval2 = Number(form.tokenRefreshIntervalSecs)
  if (Number.isFinite(interval2) && interval2 !== config.tokenRefreshIntervalSecs) d.tokenRefreshIntervalSecs = interval2
  // Admin UI 登录页背景（缺省视为开启，与 toForm 基线一致）
  if (form.loginBackgroundEnabled !== (config.loginBackgroundEnabled ?? true)) d.loginBackgroundEnabled = form.loginBackgroundEnabled
  if (form.loginBackgroundR18 !== (config.loginBackgroundR18 ?? false)) d.loginBackgroundR18 = form.loginBackgroundR18
  // 全局模型映射：JSON 文本 → 对象；合法且非空时才提交（非法 JSON 不提交，保存时静默忽略，
  // 前端已在输入区给出即时校验提示）。与基线 deep 比较，避免「空对象 ↔ 未配置」误报差异。
  // 校验与输入区 `modelMappingParsed` 同口径：**纯对象**（数组/null 不收 —— `typeof []==='object'`
  // 会放行 `["a"]`）且**值全为 string**（`{"a":123}` 同类问题）。判据不一致会让界面提示
  // 「非法、不提交」却仍把脏值发后端吃 400（只剩通用错误 toast），提示与行为对不上。
  const mmTrim = form.modelMapping.trim()
  if (mmTrim !== '') {
    try {
      const mm = JSON.parse(mmTrim) as unknown
      if (
        mm !== null &&
        typeof mm === 'object' &&
        !Array.isArray(mm) &&
        Object.values(mm).every((v) => typeof v === 'string')
      ) {
        const base = config.modelMapping ?? {}
        const changed =
          Object.keys(mm).length !== Object.keys(base).length ||
          Object.entries(mm).some(([k, v]) => base[k] !== v)
        if (changed) d.modelMapping = mm as Record<string, string>
      }
    } catch {
      // 非法 JSON：不提交。输入区已有红色校验提示。
    }
  } else if ((config.modelMapping ?? {}) && Object.keys(config.modelMapping ?? {}).length > 0) {
    // 用户清空了整个编辑区 → 视为清空映射（提交空对象删除全部规则）。
    d.modelMapping = {}
  }
  return d
}
