import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import {
  getCredentials,
  setCredentialDisabled,
  setCredentialPriority,
  setCredentialRpmLimit,
  setCredentialAllowedModels,
  resetCredentialFailure,
  forceRefreshToken,
  getCredentialBalance,
  getCachedBalances,
  addCredential,
  deleteCredential,
  getLoadBalancingMode,
  setLoadBalancingMode,
  getConfigSnapshot,
  updateConfig,
} from '@/api/credentials'
import type { AddCredentialRequest, UpdateConfigRequest } from '@/types/api'

// 查询凭据列表
export function useCredentials() {
  return useQuery({
    queryKey: ['credentials'],
    queryFn: getCredentials,
    refetchInterval: 30000, // 每 30 秒刷新一次
  })
}

// 查询服务端配置快照
export function useConfigSnapshot() {
  return useQuery({
    queryKey: ['config-snapshot'],
    queryFn: getConfigSnapshot,
  })
}

// 更新服务端配置
export function useUpdateConfig() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (req: UpdateConfigRequest) => updateConfig(req),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['config-snapshot'] })
      queryClient.invalidateQueries({ queryKey: ['loadBalancingMode'] })
    },
  })
}

// 查询凭据余额
export function useCredentialBalance(id: number | null) {
  return useQuery({
    queryKey: ['credential-balance', id],
    queryFn: () => getCredentialBalance(id!),
    enabled: id !== null,
    retry: false, // 余额查询失败时不重试（避免重复请求被封禁的账号）
  })
}

// 批量读取【已缓存】的余额快照（只读后端缓存，绝不触发上游调用 = 不封号）。
// 卡片挂载即自动加载，让余额/订阅/额度无需手动点“查询信息”即显示。
// 后端后台每 30 分钟温和刷新缓存，这里跟随凭据列表节奏温和轮询即可。
export function useCachedBalances() {
  return useQuery({
    queryKey: ['cached-balances'],
    queryFn: getCachedBalances,
    // 缓存端点只读、零上游，可比凭据列表更从容地刷新（5 分钟一次足够反映后台 30 分钟刷新）。
    refetchInterval: 300000,
    staleTime: 60000,
  })
}

// 设置禁用状态
export function useSetDisabled() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, disabled }: { id: number; disabled: boolean }) =>
      setCredentialDisabled(id, disabled),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 设置优先级
export function useSetPriority() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, priority }: { id: number; priority: number }) =>
      setCredentialPriority(id, priority),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

export function useSetRpmLimit() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, rpmLimit }: { id: number; rpmLimit: number }) =>
      setCredentialRpmLimit(id, rpmLimit),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

export function useSetAllowedModels() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, allowedModels }: { id: number; allowedModels: string[] | null }) =>
      setCredentialAllowedModels(id, allowedModels),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 重置失败计数
export function useResetFailure() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => resetCredentialFailure(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 强制刷新 Token
export function useForceRefreshToken() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => forceRefreshToken(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 添加新凭据
export function useAddCredential() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (req: AddCredentialRequest) => addCredential(req),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 删除凭据
export function useDeleteCredential() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => deleteCredential(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 获取负载均衡模式
export function useLoadBalancingMode() {
  return useQuery({
    queryKey: ['loadBalancingMode'],
    queryFn: getLoadBalancingMode,
  })
}

// 设置负载均衡模式
export function useSetLoadBalancingMode() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: setLoadBalancingMode,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['loadBalancingMode'] })
    },
  })
}
