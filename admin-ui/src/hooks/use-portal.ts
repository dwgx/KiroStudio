import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import {
  getPortalStatus,
  listPortalUsers,
  createPortalUser,
  deletePortalUser,
  resetPortalPassword,
  setPortalUserDisabled,
  getPortalAudit,
  topupPortalUser,
  getPortalUserWallet,
  getPortalPricing,
  getPortalAdminAuthStatus,
  setupPortalAdminPassword,
  loginPortalAdmin,
  logoutPortalAdmin,
  changePortalAdminPassword,
} from '@/api/portal'


const PORTAL_BUSINESS_QUERY_KEYS = [
  ['portal-status'],
  ['portal-users'],
  ['portal-audit'],
  ['portal-pricing'],
  ['portal-wallet'],
] as const

function clearPortalBusinessQueries(qc: ReturnType<typeof useQueryClient>) {
  for (const key of PORTAL_BUSINESS_QUERY_KEYS) {
    qc.removeQueries({ queryKey: key })
  }
}

/** 只检查 HttpOnly 管理会话，不读取任何拼车业务数据。 */
export function usePortalAdminAuthStatus() {
  return useQuery({
    queryKey: ['portal-admin-auth'],
    queryFn: getPortalAdminAuthStatus,
    staleTime: 0,
    refetchOnMount: 'always',
  })
}

function usePortalAuthMutation(
  mutationFn: (value: string) => Promise<{ ok: boolean }>
) {
  const qc = useQueryClient()
  return useMutation({
    mutationFn,
    onSuccess: async () => {
      clearPortalBusinessQueries(qc)
      await qc.invalidateQueries({ queryKey: ['portal-admin-auth'] })
    },
  })
}

export function useSetupPortalAdmin() {
  return usePortalAuthMutation(setupPortalAdminPassword)
}

export function useLoginPortalAdmin() {
  return usePortalAuthMutation(loginPortalAdmin)
}

export function useLogoutPortalAdmin() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: logoutPortalAdmin,
    onSuccess: async () => {
      clearPortalBusinessQueries(qc)
      await qc.invalidateQueries({ queryKey: ['portal-admin-auth'] })
    },
  })
}

export function useChangePortalAdminPassword() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: ({ currentPassword, newPassword }: { currentPassword: string; newPassword: string }) =>
      changePortalAdminPassword(currentPassword, newPassword),
    onSuccess: async () => {
      clearPortalBusinessQueries(qc)
      await qc.invalidateQueries({ queryKey: ['portal-admin-auth'] })
    },
  })
}

/**
 * Portal 频道的数据钩子。
 *
 * 全部按需拉取、不做轮询：portal 用户数是个位数量级、审计只在排查时看，
 * 轮询除了增加 SQLite 读没有任何收益。
 */

/** 频道概览（启用状态 / 注册码是否配置 / 用户数 / 凭据数）。 */
export function usePortalStatus() {
  return useQuery({
    queryKey: ['portal-status'],
    queryFn: getPortalStatus,
  })
}

/** 用户列表。 */
export function usePortalUsers() {
  return useQuery({
    queryKey: ['portal-users'],
    queryFn: listPortalUsers,
  })
}

/** 审计日志（最近 200 条）。 */
export function usePortalAudit() {
  return useQuery({
    queryKey: ['portal-audit'],
    queryFn: getPortalAudit,
  })
}

/**
 * 当前车费规则。
 *
 * 规则只在管理员改配置时变，所以缓存久一点（5 分钟）——它不像余额那样需要即时。
 */
export function usePortalPricing() {
  return useQuery({
    queryKey: ['portal-pricing'],
    queryFn: getPortalPricing,
    staleTime: 5 * 60 * 1000,
  })
}

/**
 * 某个用户的钱包 + 流水。
 *
 * 【queryKey 必须带 id】漏掉 id 会让所有用户共用同一份缓存：点开 A 的流水、
 * 再点开 B 的，看到的还是 A 的账——而界面上写着 B 的名字。这类错误不会报任何异常。
 *
 * `enabled` 由调用方控制：只有弹窗真的打开时才拉，避免为列表里每一行都发一次请求。
 */
export function usePortalUserWallet(id: number | null) {
  return useQuery({
    queryKey: ['portal-wallet', id],
    queryFn: () => getPortalUserWallet(id as number),
    enabled: id != null,
  })
}

/**
 * 用户增删改后统一刷新的 key 集合。
 *
 * 三个都要刷：列表变了、status 里的 userCount 变了、审计多了一条。
 * 漏刷任何一个都会让界面显示与真实状态不一致——这类不一致最难被发现，
 * 因为它看起来只是"数字没更新"。
 */
function useInvalidatePortal() {
  const qc = useQueryClient()
  return () => {
    qc.invalidateQueries({ queryKey: ['portal-users'] })
    qc.invalidateQueries({ queryKey: ['portal-status'] })
    qc.invalidateQueries({ queryKey: ['portal-audit'] })
    // 钱包也刷：删号会连带清掉余额、充值会改余额，而钱包弹窗可能正开着。
    // 不带第二段 id 时 TanStack Query 按前缀匹配，会把所有用户的钱包缓存一起失效——
    // 这里正是想要的效果（改的是哪个用户不确定，宁可全刷）。
    qc.invalidateQueries({ queryKey: ['portal-wallet'] })
  }
}

export function useCreatePortalUser() {
  const invalidate = useInvalidatePortal()
  return useMutation({
    mutationFn: ({ username, password }: { username: string; password: string }) =>
      createPortalUser(username, password),
    onSuccess: invalidate,
  })
}

export function useDeletePortalUser() {
  const invalidate = useInvalidatePortal()
  return useMutation({
    mutationFn: (id: number) => deletePortalUser(id),
    onSuccess: invalidate,
  })
}

export function useResetPortalPassword() {
  const invalidate = useInvalidatePortal()
  return useMutation({
    mutationFn: ({ id, password }: { id: number; password: string }) =>
      resetPortalPassword(id, password),
    onSuccess: invalidate,
  })
}

export function useSetPortalUserDisabled() {
  const invalidate = useInvalidatePortal()
  return useMutation({
    mutationFn: ({ id, disabled }: { id: number; disabled: boolean }) =>
      setPortalUserDisabled(id, disabled),
    onSuccess: invalidate,
  })
}

/**
 * 充值 / 扣减。
 *
 * 成功后走同一套 invalidate：余额在列表里显示，流水弹窗可能正开着，
 * 审计也多了一条 `admin_topup`。少刷一个就会出现「充完了但列表还是旧余额」，
 * 而管理员的下一步动作往往就基于那个数字。
 */
export function useTopupPortalUser() {
  const invalidate = useInvalidatePortal()
  return useMutation({
    mutationFn: ({ id, amount, note }: { id: number; amount: number; note?: string }) =>
      topupPortalUser(id, amount, note),
    onSuccess: invalidate,
  })
}
