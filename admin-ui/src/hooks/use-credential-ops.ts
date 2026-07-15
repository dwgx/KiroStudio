import { useMutation, useQueryClient } from '@tanstack/react-query'
import {
  deepVerifyCredential,
  probeAvailableModels,
  probeCredentialRegions,
  switchProfileRegion,
  enableOverage,
  disableOverage,
  setCredentialName,
  setCredentialProxy,
} from '@/api/credentials'

// 变更后同时失效【凭据列表】+【限流健康 insights】——号池健康卡的行数据来自 insights，
// 单纯失效 ['credentials'] 不会让健康行立刻反映（如禁用/切区域），故两者一起失效。
function useInvalidateCredOps() {
  const qc = useQueryClient()
  return () => {
    qc.invalidateQueries({ queryKey: ['credentials'] })
    qc.invalidateQueries({ queryKey: ['usage', 'ratelimit-insights'] })
  }
}

// 深度验活（真实 API 调用检测 suspend）。只读诊断，但可能翻转禁用态 → 失效列表。
export function useDeepVerify() {
  const invalidate = useInvalidateCredOps()
  return useMutation({
    mutationFn: (id: number) => deepVerifyCredential(id),
    onSuccess: invalidate,
  })
}

// 探测可用模型（⚠️逐模型发真实请求，消耗真实积分）。返回 ProbeModelsResponse 供 UI 展示；
// 不做失效（探测不改池状态）。models 省略=探全量候选目录。
export function useProbeModels() {
  return useMutation({
    mutationFn: ({ id, models }: { id: number; models?: string[] }) =>
      probeAvailableModels(id, models),
  })
}

// 探测该号各 region 的 profile（切 Profile ARN 前的列表，向上游探测可能耗时 ~120s）。
// 返回 CredentialRegionsResponse.regions 供选择列表渲染；不做失效。
export function useProbeRegions() {
  return useMutation({
    mutationFn: (id: number) => probeCredentialRegions(id),
  })
}

// 切换该号当前 Profile ARN（切区域，非改全局 region）。成功后失效列表 + insights。
export function useSwitchRegion() {
  const invalidate = useInvalidateCredOps()
  return useMutation({
    mutationFn: ({ id, arn }: { id: number; arn: string }) => switchProfileRegion(id, arn),
    onSuccess: invalidate,
  })
}

// 开启单号超额（破坏性：超 base 额度后按真实用量计费）。返回 OverageStatus（含 confirmed/note）。
export function useEnableOverage() {
  const invalidate = useInvalidateCredOps()
  return useMutation({
    mutationFn: (id: number) => enableOverage(id),
    onSuccess: invalidate,
  })
}

// 关闭单号超额。返回 OverageStatus。
export function useDisableOverage() {
  const invalidate = useInvalidateCredOps()
  return useMutation({
    mutationFn: (id: number) => disableOverage(id),
    onSuccess: invalidate,
  })
}

// 设置别名/备注（传 null 或空串清除）。
export function useSetName() {
  const invalidate = useInvalidateCredOps()
  return useMutation({
    mutationFn: ({ id, name }: { id: number; name: string | null }) => setCredentialName(id, name),
    onSuccess: invalidate,
  })
}

// 设置单号代理（proxyUrl 空清除回退全局，"direct" 强制直连；账密可选，undefined 不改、空串清除）。
export function useSetProxy() {
  const invalidate = useInvalidateCredOps()
  return useMutation({
    mutationFn: ({
      id,
      proxyUrl,
      proxyUsername,
      proxyPassword,
    }: {
      id: number
      proxyUrl: string | null
      proxyUsername?: string
      proxyPassword?: string
    }) => setCredentialProxy(id, proxyUrl, proxyUsername, proxyPassword),
    onSuccess: invalidate,
  })
}
