import { useEffect, useState } from 'react'
import { useQueryClient } from '@tanstack/react-query'
import { toast } from 'sonner'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { ConfirmDialog } from '@/components/ui/confirm-dialog'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
} from '@/components/ui/dialog'
import {
  usePortalStatus,
  usePortalUsers,
  usePortalAudit,
  usePortalPricing,
  usePortalUserWallet,
  useCreatePortalUser,
  useDeletePortalUser,
  useResetPortalPassword,
  useSetPortalUserDisabled,
  useTopupPortalUser,
  usePortalAdminAuthStatus,
  useSetupPortalAdmin,
  useLoginPortalAdmin,
  useLogoutPortalAdmin,
  useChangePortalAdminPassword,
} from '@/hooks/use-portal'
import { PORTAL_ADMIN_AUTH_EXPIRED_EVENT, type PortalUser } from '@/api/portal'
import {
  UserPlus,
  Trash2,
  KeyRound,
  Ban,
  CheckCircle2,
  ShieldAlert,
  ExternalLink,
  RefreshCw,
  Coins,
  Receipt,
  Lock,
  ShieldCheck,
  WandSparkles,
} from 'lucide-react'

/**
 * Portal 拼车管理页。
 *
 * 管理的是「独立凭据查看页」的用户：建号、停用、重置密码、删号、看审计。
 * 明文凭据不在这里显示——那是 portal 用户自己登录后看的东西，管理员要看明文
 * 有现成的凭据管理页，没必要在这里开第二个出口。
 */

function PortalAdminGate({
  configured,
  secureTransport,
  pending,
  onSubmit,
}: {
  configured: boolean
  secureTransport: boolean
  pending: boolean
  onSubmit: (password: string) => void
}) {
  const [password, setPassword] = useState('')
  const [confirm, setConfirm] = useState('')

  const submit = () => {
    if (!password) {
      toast.error('请输入管理密码')
      return
    }
    if (!configured && password !== confirm) {
      toast.error('两次输入的管理密码不一致')
      return
    }
    onSubmit(password)
  }

  const generate = () => {
    const alphabet = 'ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789!@#$%^&*_-+'
    const bytes = new Uint32Array(28)
    crypto.getRandomValues(bytes)
    const value = Array.from(bytes, (n) => alphabet[n % alphabet.length]).join('')
    setPassword(value)
    setConfirm(value)
  }

  return (
    <Card className="card-metal max-w-xl mx-auto">
      <CardHeader>
        <div className="flex items-center gap-2">
          <ShieldCheck className="h-5 w-5 text-emerald-400" />
          <CardTitle className="text-base">{configured ? '解锁拼车管理' : '初始化拼车管理密码'}</CardTitle>
        </div>
      </CardHeader>
      <CardContent className="space-y-4">
        <p className="text-sm text-[#888] leading-relaxed">
          {configured
            ? '此页面包含用户、余额和审计操作，需要独立于后台密钥的二次验证。'
            : '首次进入需要设置独立管理密码。服务端只保存 Argon2id 哈希；忘记后只能在服务器本地恢复。'}
        </p>
        {!secureTransport && (
          <Notice tone="warn">
            当前不是 HTTPS。服务端只允许本机访问在 HTTP 下验证；远程访问必须先配置 HTTPS 反代。
          </Notice>
        )}
        <PasswordInput
          value={password}
          onChange={setPassword}
          placeholder={configured ? '管理密码' : '至少 16 位，包含至少三类字符'}
          autoComplete={configured ? 'current-password' : 'new-password'}
          onKeyDown={(event) => event.key === 'Enter' && submit()}
        />
        {!configured && (
          <>
            <PasswordInput value={confirm} onChange={setConfirm} placeholder="再次输入管理密码" autoComplete="new-password" />
            <Button type="button" variant="outline" className="w-full" onClick={generate}>
              <WandSparkles className="h-4 w-4" />
              生成 28 位随机强密码
            </Button>
            <p className="text-xs text-amber-300/80">请在提交前复制保存。初始化成功后页面不会再次显示该密码。</p>
          </>
        )}
        <Button className="w-full" onClick={submit} disabled={pending}>
          {pending ? '验证中…' : configured ? '解锁' : '设置并进入'}
        </Button>
      </CardContent>
    </Card>
  )
}

function PasswordInput({
  value,
  onChange,
  ...props
}: Omit<React.InputHTMLAttributes<HTMLInputElement>, 'value' | 'onChange' | 'type'> & {
  value: string
  onChange: (value: string) => void
}) {
  return (
    <input
      {...props}
      type="password"
      value={value}
      onChange={(event) => onChange(event.target.value)}
      className="w-full rounded-md bg-[#111] border border-[#2e2e2e] px-3 py-2 text-sm text-[#ededed] placeholder:text-[#555] focus:outline-none focus:border-[#0070f3]"
    />
  )
}

/** 毫秒时间戳 → 本地时间字符串。0/undefined 显示占位符。 */
function fmtTime(ms?: number | null): string {
  if (!ms) return '—'
  return new Date(ms).toLocaleString()
}

export function PortalPage() {
  const auth = usePortalAdminAuthStatus()
  const setup = useSetupPortalAdmin()
  const login = useLoginPortalAdmin()
  const logout = useLogoutPortalAdmin()
  const changePassword = useChangePortalAdminPassword()
  const queryClient = useQueryClient()
  const [changeOpen, setChangeOpen] = useState(false)
  const [currentPassword, setCurrentPassword] = useState('')
  const [nextPassword, setNextPassword] = useState('')
  const [confirmNextPassword, setConfirmNextPassword] = useState('')

  useEffect(() => {
    const relock = () => {
      for (const key of ['portal-status', 'portal-users', 'portal-audit', 'portal-pricing', 'portal-wallet']) {
        queryClient.removeQueries({ queryKey: [key] })
      }
      void auth.refetch()
    }
    window.addEventListener(PORTAL_ADMIN_AUTH_EXPIRED_EVENT, relock)
    return () => window.removeEventListener(PORTAL_ADMIN_AUTH_EXPIRED_EVENT, relock)
  }, [auth.refetch, queryClient])

  if (auth.isLoading) {
    return (
      <Card className="card-metal max-w-xl mx-auto">
        <CardContent className="p-8 text-sm text-[#888]">正在检查拼车管理会话…</CardContent>
      </Card>
    )
  }

  if (auth.isError || !auth.data) {
    return (
      <Card className="card-metal max-w-xl mx-auto">
        <CardHeader><CardTitle className="text-base">无法检查拼车管理权限</CardTitle></CardHeader>
        <CardContent className="space-y-4">
          <p className="text-sm text-[#888]">{errText(auth.error, '认证状态读取失败')}</p>
          <Button variant="outline" onClick={() => auth.refetch()}>重试</Button>
        </CardContent>
      </Card>
    )
  }

  if (!auth.data.authenticated) {
    return (
      <PortalAdminGate
        configured={auth.data.configured}
        secureTransport={auth.data.secureTransport}
        pending={setup.isPending || login.isPending}
        onSubmit={(password) => {
          const mutation = auth.data?.configured ? login : setup
          mutation.mutate(password, {
            onSuccess: () => toast.success(auth.data?.configured ? '拼车管理已解锁' : '管理密码已初始化，请妥善保存'),
            onError: (error) => toast.error(errText(error, '认证失败')),
          })
        }}
      />
    )
  }

  const submitChangePassword = () => {
    if (!currentPassword || !nextPassword) {
      toast.error('当前密码和新密码都要填写')
      return
    }
    if (nextPassword !== confirmNextPassword) {
      toast.error('两次输入的新密码不一致')
      return
    }
    changePassword.mutate(
      { currentPassword, newPassword: nextPassword },
      {
        onSuccess: () => {
          toast.success('管理密码已修改，其他拼车管理会话已失效')
          setCurrentPassword('')
          setNextPassword('')
          setConfirmNextPassword('')
          setChangeOpen(false)
        },
        onError: (error) => toast.error(errText(error, '修改失败')),
      }
    )
  }

  return (
    <>
      <PortalBusinessPage
        onLock={() => logout.mutate(undefined, {
          onSuccess: () => toast.success('拼车管理已锁定'),
          onError: (error) => toast.error(errText(error, '锁定失败')),
        })}
        onChangePassword={() => setChangeOpen(true)}
      />
      <Dialog open={changeOpen} onOpenChange={setChangeOpen}>
        <DialogContent className="max-w-md">
          <DialogHeader>
            <DialogTitle>修改拼车管理密码</DialogTitle>
            <DialogDescription>修改后会撤销其他设备上的拼车管理会话。</DialogDescription>
          </DialogHeader>
          <div className="space-y-3">
            <PasswordInput value={currentPassword} onChange={setCurrentPassword} placeholder="当前管理密码" autoComplete="current-password" />
            <PasswordInput value={nextPassword} onChange={setNextPassword} placeholder="新密码（至少 16 位，至少三类字符）" autoComplete="new-password" />
            <PasswordInput value={confirmNextPassword} onChange={setConfirmNextPassword} placeholder="再次输入新密码" autoComplete="new-password" />
            <Button className="w-full" onClick={submitChangePassword} disabled={changePassword.isPending}>
              {changePassword.isPending ? '修改中…' : '确认修改'}
            </Button>
          </div>
        </DialogContent>
      </Dialog>
    </>
  )
}

function PortalBusinessPage({
  onLock,
  onChangePassword,
}: {
  onLock: () => void
  onChangePassword: () => void
}) {
  const { data: status, isLoading: statusLoading, refetch: refetchStatus } = usePortalStatus()
  const { data: users, isLoading: usersLoading } = usePortalUsers()
  const { data: audit } = usePortalAudit()
  const { data: pricing } = usePortalPricing()

  const createUser = useCreatePortalUser()
  const deleteUser = useDeletePortalUser()
  const resetPassword = useResetPortalPassword()
  const setDisabled = useSetPortalUserDisabled()
  const topup = useTopupPortalUser()

  // 新建用户表单
  const [newName, setNewName] = useState('')
  const [newPw, setNewPw] = useState('')

  // 待确认的破坏性操作。用 null 表示「没有待确认操作」，
  // 而不是额外开一个 boolean——两个状态可能不一致，一个不会。
  const [pendingDelete, setPendingDelete] = useState<PortalUser | null>(null)
  const [pendingReset, setPendingReset] = useState<PortalUser | null>(null)
  const [resetPw, setResetPw] = useState('')

  // 充值弹窗。金额存字符串而非 number：受控 input 存 number 时，用户删光内容
  // 会拿到 NaN，再输入又要处理 NaN → number 的转换，而字符串只在提交时解析一次。
  const [pendingTopup, setPendingTopup] = useState<PortalUser | null>(null)
  const [topupAmount, setTopupAmount] = useState('')
  const [topupNote, setTopupNote] = useState('')

  // 正在查看谁的流水。null = 弹窗关着，此时 hook 不发请求。
  const [walletOf, setWalletOf] = useState<PortalUser | null>(null)
  const { data: wallet, isLoading: walletLoading } = usePortalUserWallet(walletOf?.id ?? null)

  const handleCreate = () => {
    const username = newName.trim()
    if (!username || !newPw) {
      toast.error('用户名和密码都要填')
      return
    }
    createUser.mutate(
      { username, password: newPw },
      {
        onSuccess: () => {
          toast.success(`已创建用户 ${username}`)
          setNewName('')
          setNewPw('')
        },
        // 后端的校验文案已经是给人看的中文，直接透出，不再包一层「操作失败」。
        onError: (e: unknown) => toast.error(errText(e, '创建失败')),
      }
    )
  }

  const handleReset = () => {
    if (!pendingReset) return
    if (!resetPw) {
      toast.error('请输入新密码')
      return
    }
    resetPassword.mutate(
      { id: pendingReset.id, password: resetPw },
      {
        onSuccess: () => {
          toast.success(`已重置 ${pendingReset.username} 的密码，该用户的登录状态已失效`)
          setPendingReset(null)
          setResetPw('')
        },
        onError: (e: unknown) => toast.error(errText(e, '重置失败')),
      }
    )
  }

  const handleDelete = () => {
    if (!pendingDelete) return
    deleteUser.mutate(pendingDelete.id, {
      onSuccess: () => {
        toast.success(`已删除用户 ${pendingDelete.username}`)
        setPendingDelete(null)
      },
      onError: (e: unknown) => toast.error(errText(e, '删除失败')),
    })
  }

  const handleTopup = () => {
    if (!pendingTopup) return
    // 用 Number 而非 parseInt：parseInt('12abc') 会给出 12，把明显的手误
    // 当成有效输入。Number('12abc') 是 NaN，正好在这里被拦住。
    const amount = Number(topupAmount.trim())
    if (!Number.isInteger(amount) || amount === 0) {
      toast.error('请输入非 0 整数（负数表示扣减）')
      return
    }
    topup.mutate(
      { id: pendingTopup.id, amount, note: topupNote.trim() || undefined },
      {
        onSuccess: (r) => {
          toast.success(
            `${amount > 0 ? '已充值' : '已扣减'} ${Math.abs(amount)} 分，${pendingTopup.username} 现有 ${r.balance} 分`
          )
          setPendingTopup(null)
          setTopupAmount('')
          setTopupNote('')
        },
        // 余额不足时后端回 400 并说明「只剩 X 分」，直接透出比包一层更有用。
        onError: (e: unknown) => toast.error(errText(e, '充值失败')),
      }
    )
  }

  const handleToggleDisabled = (u: PortalUser) => {
    setDisabled.mutate(
      { id: u.id, disabled: !u.disabled },
      {
        onSuccess: () =>
          toast.success(u.disabled ? `已启用 ${u.username}` : `已停用 ${u.username}`),
        onError: (e: unknown) => toast.error(errText(e, '操作失败')),
      }
    )
  }

  return (
    <div className="space-y-6">
      {/* ---- 状态总览 ---- */}
      <Card className="card-metal">
        <CardHeader className="flex flex-row items-center justify-between gap-3">
          <CardTitle className="text-base">拼车状态</CardTitle>
          <div className="flex items-center gap-2">
            <Button variant="outline" size="sm" onClick={onChangePassword}>
              <KeyRound className="h-3.5 w-3.5" />
              修改管理密码
            </Button>
            <Button variant="outline" size="sm" onClick={onLock}>
              <Lock className="h-3.5 w-3.5" />
              锁定
            </Button>
            <Button variant="outline" size="sm" onClick={() => refetchStatus()}>
              <RefreshCw className="h-3.5 w-3.5" />
              刷新
            </Button>
          </div>
        </CardHeader>
        <CardContent>
          {statusLoading ? (
            <p className="text-sm text-[#888]">加载中…</p>
          ) : (
            <>
              <div className="grid grid-cols-2 md:grid-cols-4 gap-4">
                <StatBox
                  label="拼车开关"
                  value={status?.enabled ? '已启用' : '未启用'}
                  tone={status?.enabled ? 'ok' : 'muted'}
                />
                <StatBox
                  label="注册码"
                  value={status?.inviteCodeConfigured ? '已配置' : '未配置'}
                  tone={status?.inviteCodeConfigured ? 'ok' : 'warn'}
                />
                <StatBox label="用户数" value={String(status?.userCount ?? 0)} />
                <StatBox label="凭据记录" value={String(status?.keyCount ?? 0)} />
              </div>

              {/* 三条真正会让人踩坑的提示，只在命中时出现 */}
              <div className="mt-4 space-y-2">
                {!status?.enabled && (
                  <Notice tone="muted">
                    拼车入口未启用：<code>/portal</code> 全部返回 404。到{' '}
                    <strong>设置 → 凭据频道</strong> 打开「启用凭据频道」后即时生效，无需重启。
                  </Notice>
                )}
                {status?.enabled && !status?.inviteCodeConfigured && (
                  <Notice tone="warn">
                    未配置注册码，自助注册通道关闭（已有用户仍可登录）。需要放开注册就到{' '}
                    <strong>设置 → 凭据频道</strong> 填「注册码」，或在下面直接建号。
                  </Notice>
                )}
                {status?.enabled && !status?.requireHttps && (
                  <Notice tone="danger">
                    <strong>会话 cookie 未要求 HTTPS。</strong>
                    网关本身是 HTTP，公网暴露时密码和凭据都会在链路上明文传输。
                    仅内网调试可以这样，对外务必前置 HTTPS 并在{' '}
                    <strong>设置 → 凭据频道</strong> 打开「强制 HTTPS」。
                  </Notice>
                )}
              </div>

              <a
                href="/portal"
                target="_blank"
                rel="noreferrer"
                className="inline-flex items-center gap-1.5 mt-4 text-sm text-[#0070f3] hover:underline"
              >
                <ExternalLink className="h-3.5 w-3.5" />
                打开用户页面
              </a>
            </>
          )}
        </CardContent>
      </Card>

      {/* ---- 车费规则 ----
           只在积分启用时显示。未启用时这张卡整体不出现，而不是显示一堆
           "不生效的规则"——后者会让人以为已经在收费了。 */}
      {pricing?.enabled && (
        <Card className="card-metal">
          <CardHeader>
            <CardTitle className="text-base">车费规则</CardTitle>
          </CardHeader>
          <CardContent className="space-y-3">
            <p className="text-sm text-[#ededed]">
              前 <span className="font-semibold">{pricing.baseCount}</span> 人各{' '}
              <span className="font-semibold">{pricing.basePrice}</span> 分 · 之后按{' '}
              <span className="font-semibold">{pricing.totalPrice}</span>/N 均摊 · 每车上限{' '}
              <span className="font-semibold">{pricing.maxBoarders}</span> 人
            </p>
            {/* 单价表直接用后端算好的 priceTable，不在前端复算那个公式。
                两份实现迟早分叉，而分叉的表现是「面板写 5 分、实际扣 7 分」，
                这种问题在用户投诉时无法自证清白。 */}
            <div className="overflow-x-auto">
              <table className="text-xs">
                <thead>
                  <tr className="text-[#666]">
                    <th className="pr-2 py-1 text-left font-medium">人数</th>
                    {pricing.priceTable.map((_, i) => (
                      <th key={i} className="px-2 py-1 font-medium tabular-nums">
                        {i + 1}
                      </th>
                    ))}
                  </tr>
                </thead>
                <tbody>
                  <tr>
                    <td className="pr-2 py-1 text-[#666]">单价</td>
                    {pricing.priceTable.map((p, i) => (
                      <td key={i} className="px-2 py-1 text-center tabular-nums text-[#ededed]">
                        {p}
                      </td>
                    ))}
                  </tr>
                </tbody>
              </table>
            </div>
            <p className="text-xs text-[#666] leading-relaxed">
              人越多单价越低，差额会自动退还给已在车上的人——任何时刻每人的净支出都相等。
              改这些参数只影响<strong className="text-[#888]">之后才首次被上车</strong>的 key；
              已有车队按当初冻结的规则计价。
            </p>
          </CardContent>
        </Card>
      )}

      {/* ---- 用户管理 ---- */}
      <Card className="card-metal">
        <CardHeader>
          <CardTitle className="text-base">用户</CardTitle>
        </CardHeader>
        <CardContent className="space-y-4">
          {/* 建号表单 */}
          <div className="flex flex-col sm:flex-row gap-2">
            <input
              value={newName}
              onChange={(e) => setNewName(e.target.value)}
              placeholder="用户名（3-64 位，字母/数字/_-.）"
              autoComplete="off"
              className="flex-1 rounded-md bg-[#111] border border-[#2e2e2e] px-3 py-2 text-sm text-[#ededed] placeholder:text-[#555] focus:outline-none focus:border-[#0070f3]"
            />
            <input
              value={newPw}
              onChange={(e) => setNewPw(e.target.value)}
              type="password"
              placeholder="密码（至少 10 位，不能纯数字/纯字母）"
              autoComplete="new-password"
              className="flex-1 rounded-md bg-[#111] border border-[#2e2e2e] px-3 py-2 text-sm text-[#ededed] placeholder:text-[#555] focus:outline-none focus:border-[#0070f3]"
            />
            <Button onClick={handleCreate} disabled={createUser.isPending}>
              <UserPlus className="h-4 w-4 mr-1.5" />
              {createUser.isPending ? '创建中…' : '建号'}
            </Button>
          </div>

          {/* 列表 */}
          {usersLoading ? (
            <p className="text-sm text-[#888]">加载中…</p>
          ) : !users?.length ? (
            <p className="text-sm text-[#888] py-4 text-center">
              还没有用户。上面建一个，或让对方拿注册码自助注册。
            </p>
          ) : (
            <div className="overflow-x-auto">
              <table className="w-full text-sm">
                <thead>
                  <tr className="text-left text-[#666] border-b border-[#2e2e2e]">
                    <th className="py-2 pr-4 font-medium">用户名</th>
                    <th className="py-2 pr-4 font-medium">状态</th>
                    <th className="py-2 pr-4 font-medium text-right">余额</th>
                    <th className="py-2 pr-4 font-medium text-right">已上车</th>
                    <th className="py-2 pr-4 font-medium">创建时间</th>
                    <th className="py-2 pr-4 font-medium">最后登录</th>
                    <th className="py-2 font-medium text-right">操作</th>
                  </tr>
                </thead>
                <tbody>
                  {users.map((u) => (
                    <tr key={u.id} className="border-b border-[#1a1a1a] last:border-0">
                      <td className="py-2.5 pr-4 font-mono">{u.username}</td>
                      <td className="py-2.5 pr-4">
                        {u.disabled ? (
                          <span className="text-[#f5a623]">已停用</span>
                        ) : (
                          <span className="text-[#0cce6b]">正常</span>
                        )}
                      </td>
                      {/* 余额：0 分标灰而非当普通数字。管理员一眼要能看出「这号还没充过、
                          现在什么车都上不了」——那是最常见的求助原因。 */}
                      <td className="py-2.5 pr-4 text-right tabular-nums">
                        <span className={u.balance > 0 ? 'text-[#ededed]' : 'text-[#666]'}>
                          {u.balance.toLocaleString()}
                        </span>
                      </td>
                      <td className="py-2.5 pr-4 text-right tabular-nums text-[#888]">
                        {u.aboardCount}
                      </td>
                      <td className="py-2.5 pr-4 text-[#888]">{fmtTime(u.createdAtMs)}</td>
                      <td className="py-2.5 pr-4 text-[#888]">{fmtTime(u.lastLoginMs)}</td>
                      <td className="py-2.5 text-right whitespace-nowrap">
                        <Button
                          variant="ghost"
                          size="sm"
                          title={u.disabled ? '启用' : '停用（立即踢下线）'}
                          onClick={() => handleToggleDisabled(u)}
                          disabled={setDisabled.isPending}
                        >
                          {u.disabled ? (
                            <CheckCircle2 className="h-3.5 w-3.5" />
                          ) : (
                            <Ban className="h-3.5 w-3.5" />
                          )}
                        </Button>
                        <Button
                          variant="ghost"
                          size="sm"
                          title="充值 / 扣减积分"
                          onClick={() => {
                            setTopupAmount('')
                            setTopupNote('')
                            setPendingTopup(u)
                          }}
                        >
                          <Coins className="h-3.5 w-3.5" />
                        </Button>
                        <Button
                          variant="ghost"
                          size="sm"
                          title="查看积分流水"
                          onClick={() => setWalletOf(u)}
                        >
                          <Receipt className="h-3.5 w-3.5" />
                        </Button>
                        <Button
                          variant="ghost"
                          size="sm"
                          title="重置密码"
                          onClick={() => {
                            setResetPw('')
                            setPendingReset(u)
                          }}
                        >
                          <KeyRound className="h-3.5 w-3.5" />
                        </Button>
                        <Button
                          variant="ghost"
                          size="sm"
                          title="删除"
                          onClick={() => setPendingDelete(u)}
                          className="text-red-400 hover:text-red-300"
                        >
                          <Trash2 className="h-3.5 w-3.5" />
                        </Button>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </CardContent>
      </Card>
      {/* ---- 审计 ---- */}
      <Card className="card-metal">
        <CardHeader>
          <CardTitle className="text-base">审计</CardTitle>
        </CardHeader>
        <CardContent>
          <p className="text-xs text-[#666] mb-3">
            每次登录、每次明文外显都留一条。<span className="text-[#888]">reveal_keys</span>{' '}
            的 count 是那一次看到了几个可用明文。
          </p>
          {!audit?.length ? (
            <p className="text-sm text-[#888] py-4 text-center">还没有记录。</p>
          ) : (
            <div className="max-h-[420px] overflow-y-auto">
              <table className="w-full text-xs">
                <thead className="sticky top-0 bg-[#0f0f0f]">
                  <tr className="text-left text-[#666] border-b border-[#2e2e2e]">
                    <th className="py-2 pr-3 font-medium">时间</th>
                    <th className="py-2 pr-3 font-medium">用户</th>
                    <th className="py-2 pr-3 font-medium">动作</th>
                    <th className="py-2 pr-3 font-medium">来源 IP</th>
                    <th className="py-2 font-medium">详情</th>
                  </tr>
                </thead>
                <tbody>
                  {audit.map((a) => (
                    <tr key={a.id} className="border-b border-[#1a1a1a] last:border-0">
                      <td className="py-2 pr-3 text-[#888] whitespace-nowrap">
                        {fmtTime(a.atMs)}
                      </td>
                      <td className="py-2 pr-3 font-mono">{a.username ?? '—'}</td>
                      <td className="py-2 pr-3">
                        <span
                          className={
                            a.action.includes('fail')
                              ? 'text-[#f5a623]'
                              : a.action === 'reveal_keys'
                                ? 'text-[#0070f3]'
                                : 'text-[#888]'
                          }
                        >
                          {a.action}
                        </span>
                      </td>
                      <td className="py-2 pr-3 font-mono text-[#666]">{a.clientIp ?? '—'}</td>
                      <td className="py-2 text-[#666]">{a.detail ?? '—'}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </CardContent>
      </Card>

      {/* ---- 确认弹窗 ---- */}
      <ConfirmDialog
        open={!!pendingDelete}
        onOpenChange={(v) => !v && setPendingDelete(null)}
        title="删除用户"
        description={
          <>
            将删除用户 <span className="font-mono text-[#ededed]">{pendingDelete?.username}</span>
            ，其会话立即失效。此操作不可撤销，但审计记录会保留。
          </>
        }
        confirmLabel="删除"
        destructive
        loading={deleteUser.isPending}
        onConfirm={handleDelete}
      />

      {/* 充值 / 扣减。
          不标 destructive：加分是常规运营操作。扣分虽然减少余额，但后端拒绝把余额
          打成负数（返回 400），最坏结果是「没扣动」而不是数据损坏，够不上破坏性操作。 */}
      <ConfirmDialog
        open={!!pendingTopup}
        onOpenChange={(v) => !v && setPendingTopup(null)}
        title="充值 / 扣减积分"
        description={
          <>
            调整 <span className="font-mono text-[#ededed]">{pendingTopup?.username}</span>{' '}
            的积分，当前余额{' '}
            <span className="font-mono text-[#ededed]">{pendingTopup?.balance ?? 0}</span> 分。
            填正数是充值，负数是扣减。
          </>
        }
        confirmLabel="提交"
        loading={topup.isPending}
        onConfirm={handleTopup}
      >
        <div className="space-y-2">
          <input
            value={topupAmount}
            onChange={(e) => setTopupAmount(e.target.value)}
            inputMode="numeric"
            placeholder="分数，如 100 或 -50"
            autoComplete="off"
            className="w-full rounded-md bg-[#111] border border-[#2e2e2e] px-3 py-2 text-sm text-[#ededed] placeholder:text-[#555] focus:outline-none focus:border-[#0070f3]"
          />
          <input
            value={topupNote}
            onChange={(e) => setTopupNote(e.target.value)}
            placeholder="备注（可选，写进流水）"
            autoComplete="off"
            className="w-full rounded-md bg-[#111] border border-[#2e2e2e] px-3 py-2 text-sm text-[#ededed] placeholder:text-[#555] focus:outline-none focus:border-[#0070f3]"
          />
          <p className="text-[11px] text-[#666]">
            备注会留在流水里。事后回答「这 500 分是为什么给的」只能靠它。
          </p>
        </div>
      </ConfirmDialog>

      <ConfirmDialog
        open={!!pendingReset}
        onOpenChange={(v) => !v && setPendingReset(null)}
        title="重置密码"
        description={
          <>
            为 <span className="font-mono text-[#ededed]">{pendingReset?.username}</span>{' '}
            设置新密码。该用户的所有会话会被立即清掉，需用新密码重新登录。
          </>
        }
        confirmLabel="重置"
        loading={resetPassword.isPending}
        onConfirm={handleReset}
      >
        <input
          value={resetPw}
          onChange={(e) => setResetPw(e.target.value)}
          type="password"
          placeholder="新密码（至少 10 位，不能纯数字/纯字母）"
          autoComplete="new-password"
          className="w-full rounded-md bg-[#111] border border-[#2e2e2e] px-3 py-2 text-sm text-[#ededed] placeholder:text-[#555] focus:outline-none focus:border-[#0070f3]"
        />
      </ConfirmDialog>

      {/* 积分流水。只读，所以用 Dialog 而不是 ConfirmDialog——后者总带一个
          「确定」按钮，而这里没有任何要确认的动作，多一个按钮只会让人犹豫该不该点。 */}
      <Dialog open={!!walletOf} onOpenChange={(v) => !v && setWalletOf(null)}>
        <DialogContent className="max-w-2xl">
          <DialogHeader>
            <DialogTitle>
              {walletOf?.username} 的积分流水
            </DialogTitle>
            <DialogDescription>
              最近 200 条。差额模型下退款很密（每有新人上车，车上每人各退一次），
              所以 refund 行通常远多于 unlock 行。
            </DialogDescription>
          </DialogHeader>

          {walletLoading ? (
            <p className="text-sm text-[#888] py-4">加载中…</p>
          ) : !wallet ? (
            <p className="text-sm text-[#888] py-4">读不到流水。</p>
          ) : (
            <>
              <div className="grid grid-cols-3 gap-3">
                <StatBox label="当前余额" value={`${wallet.balance} 分`} tone="ok" />
                <StatBox label="累计充值" value={`${wallet.topup} 分`} />
                {/* 净支出：退款冲减它，所以它不是单调递增的。
                    恒等式 balance == topup - spent 对所有流水类型都成立。 */}
                <StatBox label="净支出" value={`${wallet.spent} 分`} />
              </div>

              {!wallet.ledger.length ? (
                <p className="text-sm text-[#888] py-4 text-center">还没有流水。</p>
              ) : (
                <div className="max-h-[360px] overflow-y-auto mt-1">
                  <table className="w-full text-xs">
                    <thead className="sticky top-0 bg-[#0f0f0f]">
                      <tr className="text-left text-[#666] border-b border-[#2e2e2e]">
                        <th className="py-2 pr-3 font-medium">时间</th>
                        <th className="py-2 pr-3 font-medium">类型</th>
                        <th className="py-2 pr-3 font-medium text-right">变动</th>
                        <th className="py-2 pr-3 font-medium text-right">余额</th>
                        <th className="py-2 pr-3 font-medium">车</th>
                        <th className="py-2 font-medium">备注</th>
                      </tr>
                    </thead>
                    <tbody>
                      {wallet.ledger.map((e) => (
                        <tr key={e.id} className="border-b border-[#1a1a1a] last:border-0">
                          <td className="py-2 pr-3 text-[#888] whitespace-nowrap">
                            {fmtTime(e.atMs)}
                          </td>
                          <td className="py-2 pr-3">{kindLabel(e.kind)}</td>
                          {/* 带符号显示：+ 和 - 一眼能分清进出账，只看数字得先想一下 */}
                          <td
                            className={`py-2 pr-3 text-right font-mono ${
                              e.delta > 0 ? 'text-[#0cce6b]' : 'text-[#f5a623]'
                            }`}
                          >
                            {e.delta > 0 ? `+${e.delta}` : e.delta}
                          </td>
                          <td className="py-2 pr-3 text-right font-mono text-[#888]">
                            {e.balanceAfter}
                          </td>
                          <td className="py-2 pr-3 font-mono text-[#666]">
                            {e.credentialId != null ? `#${e.credentialId}` : '—'}
                          </td>
                          <td className="py-2 text-[#666]">{e.note ?? '—'}</td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              )}
            </>
          )}
        </DialogContent>
      </Dialog>
    </div>
  )
}

/** 流水类型 → 中文。未知类型原样显示，不吞掉——出现未知类型说明后端加了新 kind。 */
function kindLabel(kind: string): string {
  return (
    {
      topup: '充值',
      unlock: '上车',
      refund: '退款',
      admin_adjust: '管理员调账',
    }[kind] ?? kind
  )
}

/** 从 axios 错误里取后端的中文文案，取不到时回退给定默认值。 */
function errText(e: unknown, fallback: string): string {
  const msg = (e as { response?: { data?: { error?: string } } })?.response?.data?.error
  return msg || fallback
}

/** 概览里的一格数字。tone 只影响数值颜色，不影响布局。 */
function StatBox({
  label,
  value,
  tone = 'plain',
}: {
  label: string
  value: string
  tone?: 'plain' | 'ok' | 'warn' | 'muted'
}) {
  const toneClass = {
    plain: 'text-[#ededed]',
    ok: 'text-emerald-400',
    warn: 'text-amber-400',
    muted: 'text-[#888]',
  }[tone]
  return (
    <div className="rounded-md border border-[#2e2e2e] bg-[#111] px-3 py-2.5">
      <p className="text-[11px] text-[#666] uppercase tracking-wider">{label}</p>
      <p className={`mt-1 text-base font-semibold ${toneClass}`}>{value}</p>
    </div>
  )
}

/**
 * 一条提示条。
 *
 * `danger` 用于「现在这个配置是不安全的」，不是「操作失败了」——把不安全的默认状态
 * 摆到界面上，比写在文档里有用得多。
 */
function Notice({
  tone,
  children,
}: {
  tone: 'muted' | 'warn' | 'danger'
  children: React.ReactNode
}) {
  const cls = {
    muted: 'border-[#2e2e2e] bg-[#111] text-[#888]',
    warn: 'border-amber-500/30 bg-amber-500/5 text-amber-200/90',
    danger: 'border-red-500/40 bg-red-500/5 text-red-200/90',
  }[tone]
  return (
    <div className={`flex gap-2 rounded-md border px-3 py-2 text-xs leading-relaxed ${cls}`}>
      {tone !== 'muted' && <ShieldAlert className="h-4 w-4 shrink-0 mt-0.5" />}
      <div>{children}</div>
    </div>
  )
}
