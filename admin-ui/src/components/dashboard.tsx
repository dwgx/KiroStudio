import { useState, useEffect, useRef } from 'react'
import { RefreshCw, LogOut, Moon, Sun, Server, Plus, Trash2, RotateCcw, CheckCircle2, Database, Zap, Ban, Power, FlaskConical } from 'lucide-react'
import { useQueryClient } from '@tanstack/react-query'
import { toast } from 'sonner'
import { storage } from '@/lib/storage'
import { Card, CardContent } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { CredentialCard } from '@/components/credential-card'
import { StatCard } from '@/components/ui/stat-card'
import { BalanceDialog } from '@/components/balance-dialog'
import { AddCredentialDialog } from '@/components/add-credential-dialog'
import { BatchVerifyDialog, type VerifyResult } from '@/components/batch-verify-dialog'
import { ModelTestDialog } from '@/components/model-test-dialog'
import { useCredentials, useDeleteCredential, useResetFailure, useLoadBalancingMode, useSetLoadBalancingMode, useSetDisabled } from '@/hooks/use-credentials'
import { getCredentialBalance, getCachedBalances, forceRefreshToken, deepVerifyCredential, probeAvailableModels, setCredentialAllowedModels } from '@/api/credentials'
import { extractErrorMessage } from '@/lib/utils'
import { PageSkeleton } from '@/components/ui/page-skeleton'
import type { BalanceResponse } from '@/types/api'

interface DashboardProps {
  onLogout: () => void
  /** 内嵌到多页框架时为 true：隐藏自身顶栏，操作按钮移入工具行 */
  embedded?: boolean
}

/**
 * 文本过长时横向滚动（跑马灯），短文本静态显示——用于"当前活跃"卡片的邮箱：
 * 长邮箱不再截断成省略号，而是在原地缓慢来回滚动看全。
 * 测量内容宽 vs 容器宽，仅溢出时挂 .marquee-scrolling 并按溢出量算位移/时长。
 */
function ScrollOnOverflow({ text, className }: { text: string; className?: string }) {
  const boxRef = useRef<HTMLSpanElement>(null)
  const innerRef = useRef<HTMLSpanElement>(null)
  const [shift, setShift] = useState(0)

  useEffect(() => {
    const box = boxRef.current
    const inner = innerRef.current
    if (!box || !inner) return
    const measure = () => {
      const overflow = inner.scrollWidth - box.clientWidth
      // +8px 间隔，滚到底能完整看到末尾字符；不溢出则不滚。
      setShift(overflow > 1 ? overflow + 8 : 0)
    }
    measure()
    const ro = new ResizeObserver(measure)
    ro.observe(box)
    ro.observe(inner)
    return () => ro.disconnect()
  }, [text])

  // 位移越大滚得越久，保持匀速观感（约 40px/s，clamp 到 [6s, 20s]）。
  const duration = Math.min(20, Math.max(6, shift / 40))

  return (
    <span ref={boxRef} className={`block overflow-hidden ${className ?? ''}`}>
      <span
        ref={innerRef}
        className={`inline-block whitespace-nowrap ${shift > 0 ? 'marquee-scrolling' : ''}`}
        style={
          shift > 0
            ? ({
                ['--marquee-shift' as string]: `${shift}px`,
                ['--marquee-duration' as string]: `${duration}s`,
              } as React.CSSProperties)
            : undefined
        }
      >
        {text}
      </span>
    </span>
  )
}

export function Dashboard({ onLogout, embedded = false }: DashboardProps) {
  const [selectedCredentialId, setSelectedCredentialId] = useState<number | null>(null)
  const [balanceDialogOpen, setBalanceDialogOpen] = useState(false)
  const [addDialogOpen, setAddDialogOpen] = useState(false)
  const [selectedIds, setSelectedIds] = useState<Set<number>>(new Set())
  const [verifyDialogOpen, setVerifyDialogOpen] = useState(false)
  const [verifying, setVerifying] = useState(false)
  const [verifyProgress, setVerifyProgress] = useState({ current: 0, total: 0 })
  const [verifyResults, setVerifyResults] = useState<Map<number, VerifyResult>>(new Map())
  const [balanceMap, setBalanceMap] = useState<Map<number, BalanceResponse>>(new Map())
  const [loadingBalanceIds, setLoadingBalanceIds] = useState<Set<number>>(new Set())
  const [queryingInfo, setQueryingInfo] = useState(false)
  const [queryInfoProgress, setQueryInfoProgress] = useState({ current: 0, total: 0 })
  const [batchRefreshing, setBatchRefreshing] = useState(false)
  const [batchRefreshProgress, setBatchRefreshProgress] = useState({ current: 0, total: 0 })
  const cancelVerifyRef = useRef(false)
  // 模型测试：勾选凭据后打开独立弹窗（弹窗内自选模型、可反复测、保留上次结果）
  const [modelTestOpen, setModelTestOpen] = useState(false)
  const [modelTestIds, setModelTestIds] = useState<number[]>([])
  const [currentPage, setCurrentPage] = useState(1)
  const itemsPerPage = 12
  const [darkMode, setDarkMode] = useState(() => {
    if (typeof window !== 'undefined') {
      return document.documentElement.classList.contains('dark')
    }
    return false
  })

  const queryClient = useQueryClient()
  const { data, isLoading, error, refetch } = useCredentials()
  const { mutate: deleteCredential } = useDeleteCredential()
  const { mutate: resetFailure } = useResetFailure()
  const { mutateAsync: setDisabledAsync } = useSetDisabled()
  const { data: loadBalancingData, isLoading: isLoadingMode } = useLoadBalancingMode()
  const { mutate: setLoadBalancingMode, isPending: isSettingMode } = useSetLoadBalancingMode()

  // 计算分页
  const totalPages = Math.ceil((data?.credentials.length || 0) / itemsPerPage)
  const startIndex = (currentPage - 1) * itemsPerPage
  const endIndex = startIndex + itemsPerPage
  const currentCredentials = data?.credentials.slice(startIndex, endIndex) || []
  const disabledCredentialCount = data?.credentials.filter(credential => credential.disabled).length || 0
  const selectedDisabledCount = Array.from(selectedIds).filter(id => {
    const credential = data?.credentials.find(c => c.id === id)
    return Boolean(credential?.disabled)
  }).length

  // 当前活跃凭据及其余额（用于 KPI 卡展示简要状态）
  const currentCredential = data?.currentId
    ? data.credentials.find(c => c.id === data.currentId)
    : undefined
  const currentBalance = data?.currentId ? balanceMap.get(data.currentId) ?? null : null

  // 当凭据列表变化时重置到第一页
  useEffect(() => {
    setCurrentPage(1)
  }, [data?.credentials.length])

  // 只保留当前仍存在的凭据缓存，避免删除后残留旧数据
  useEffect(() => {
    if (!data?.credentials) {
      setBalanceMap(new Map())
      setLoadingBalanceIds(new Set())
      return
    }

    const validIds = new Set(data.credentials.map(credential => credential.id))

    setBalanceMap(prev => {
      const next = new Map<number, BalanceResponse>()
      prev.forEach((value, id) => {
        if (validIds.has(id)) {
          next.set(id, value)
        }
      })
      return next.size === prev.size ? prev : next
    })

    setLoadingBalanceIds(prev => {
      if (prev.size === 0) {
        return prev
      }
      const next = new Set<number>()
      prev.forEach(id => {
        if (validIds.has(id)) {
          next.add(id)
        }
      })
      return next.size === prev.size ? prev : next
    })
  }, [data?.credentials])

  const toggleDarkMode = () => {
    setDarkMode(!darkMode)
    document.documentElement.classList.toggle('dark')
  }

  const handleViewBalance = (id: number) => {
    setSelectedCredentialId(id)
    setBalanceDialogOpen(true)
  }

  const handleRefresh = () => {
    refetch()
    toast.success('已刷新凭据列表')
  }

  const handleLogout = () => {
    storage.removeApiKey()
    queryClient.clear()
    onLogout()
  }

  // 选择管理：选中只通过卡片左上角勾选框，永远是加/减选（多选语义）。
  const toggleSelect = (id: number) => {
    setSelectedIds(prev => {
      const next = new Set(prev)
      if (next.has(id)) {
        next.delete(id)
      } else {
        next.add(id)
      }
      return next
    })
  }

  const deselectAll = () => {
    setSelectedIds(new Set())
  }

  // 批量删除（仅删除已禁用项）
  const handleBatchDelete = async () => {
    if (selectedIds.size === 0) {
      toast.error('请先选择要删除的凭据')
      return
    }

    const disabledIds = Array.from(selectedIds).filter(id => {
      const credential = data?.credentials.find(c => c.id === id)
      return Boolean(credential?.disabled)
    })

    if (disabledIds.length === 0) {
      toast.error('选中的凭据中没有已禁用项')
      return
    }

    const skippedCount = selectedIds.size - disabledIds.length
    const skippedText = skippedCount > 0 ? `（将跳过 ${skippedCount} 个未禁用凭据）` : ''

    if (!confirm(`确定要删除 ${disabledIds.length} 个已禁用凭据吗？此操作无法撤销。${skippedText}`)) {
      return
    }

    let successCount = 0
    let failCount = 0

    for (const id of disabledIds) {
      try {
        await new Promise<void>((resolve, reject) => {
          deleteCredential(id, {
            onSuccess: () => {
              successCount++
              resolve()
            },
            onError: (err) => {
              failCount++
              reject(err)
            }
          })
        })
      } catch (error) {
        // 错误已在 onError 中处理
      }
    }

    const skippedResultText = skippedCount > 0 ? `，已跳过 ${skippedCount} 个未禁用凭据` : ''

    if (failCount === 0) {
      toast.success(`成功删除 ${successCount} 个已禁用凭据${skippedResultText}`)
    } else {
      toast.warning(`删除已禁用凭据：成功 ${successCount} 个，失败 ${failCount} 个${skippedResultText}`)
    }

    deselectAll()
  }

  // 批量恢复异常
  const handleBatchResetFailure = async () => {
    if (selectedIds.size === 0) {
      toast.error('请先选择要恢复的凭据')
      return
    }

    const failedIds = Array.from(selectedIds).filter(id => {
      const cred = data?.credentials.find(c => c.id === id)
      return cred && cred.failureCount > 0
    })

    if (failedIds.length === 0) {
      toast.error('选中的凭据中没有失败的凭据')
      return
    }

    let successCount = 0
    let failCount = 0

    for (const id of failedIds) {
      try {
        await new Promise<void>((resolve, reject) => {
          resetFailure(id, {
            onSuccess: () => {
              successCount++
              resolve()
            },
            onError: (err) => {
              failCount++
              reject(err)
            }
          })
        })
      } catch (error) {
        // 错误已在 onError 中处理
      }
    }

    if (failCount === 0) {
      toast.success(`成功恢复 ${successCount} 个凭据`)
    } else {
      toast.warning(`成功 ${successCount} 个，失败 ${failCount} 个`)
    }

    deselectAll()
  }

  // 批量启用 / 禁用：对选中项统一设为目标状态（逐个调用，跳过已是目标态的）
  const handleBatchSetDisabled = async (disabled: boolean) => {
    if (selectedIds.size === 0) {
      toast.error(disabled ? '请先选择要禁用的凭据' : '请先选择要启用的凭据')
      return
    }
    const targetIds = Array.from(selectedIds).filter(id => {
      const cred = data?.credentials.find(c => c.id === id)
      return cred && cred.disabled !== disabled
    })
    if (targetIds.length === 0) {
      toast.error(disabled ? '选中的凭据都已是禁用状态' : '选中的凭据都已是启用状态')
      return
    }

    let successCount = 0
    let failCount = 0
    for (const id of targetIds) {
      try {
        await setDisabledAsync({ id, disabled })
        successCount++
      } catch {
        failCount++
      }
    }

    const action = disabled ? '禁用' : '启用'
    if (failCount === 0) {
      toast.success(`成功${action} ${successCount} 个凭据`)
    } else {
      toast.warning(`${action}：成功 ${successCount} 个，失败 ${failCount} 个`)
    }
    deselectAll()
  }

  // 批量刷新 Token
  const handleBatchForceRefresh = async () => {
    if (selectedIds.size === 0) {
      toast.error('请先选择要刷新的凭据')
      return
    }

    const enabledIds = Array.from(selectedIds).filter(id => {
      const cred = data?.credentials.find(c => c.id === id)
      return cred && !cred.disabled
    })

    if (enabledIds.length === 0) {
      toast.error('选中的凭据中没有启用的凭据')
      return
    }

    setBatchRefreshing(true)
    setBatchRefreshProgress({ current: 0, total: enabledIds.length })

    let successCount = 0
    let failCount = 0

    for (let i = 0; i < enabledIds.length; i++) {
      try {
        await forceRefreshToken(enabledIds[i])
        successCount++
      } catch {
        failCount++
      }
      setBatchRefreshProgress({ current: i + 1, total: enabledIds.length })
    }

    setBatchRefreshing(false)
    queryClient.invalidateQueries({ queryKey: ['credentials'] })

    if (failCount === 0) {
      toast.success(`成功刷新 ${successCount} 个凭据的 Token`)
    } else {
      toast.warning(`刷新 Token：成功 ${successCount} 个，失败 ${failCount} 个`)
    }

    deselectAll()
  }

  // 测试可用模型：把勾选的凭据 id 定格，打开独立弹窗（弹窗内自选模型、可反复测、保留结果）。
  const handleTestModels = () => {
    const ids = Array.from(selectedIds)
    if (ids.length === 0) {
      toast.error('请先勾选要测试的凭据')
      return
    }
    setModelTestIds(ids)
    setModelTestOpen(true)
  }

  // 一键清除所有已禁用凭据
  const handleClearAll = async () => {
    if (!data?.credentials || data.credentials.length === 0) {
      toast.error('没有可清除的凭据')
      return
    }

    const disabledCredentials = data.credentials.filter(credential => credential.disabled)

    if (disabledCredentials.length === 0) {
      toast.error('没有可清除的已禁用凭据')
      return
    }

    if (!confirm(`确定要清除所有 ${disabledCredentials.length} 个已禁用凭据吗？此操作无法撤销。`)) {
      return
    }

    let successCount = 0
    let failCount = 0

    for (const credential of disabledCredentials) {
      try {
        await new Promise<void>((resolve, reject) => {
          deleteCredential(credential.id, {
            onSuccess: () => {
              successCount++
              resolve()
            },
            onError: (err) => {
              failCount++
              reject(err)
            }
          })
        })
      } catch (error) {
        // 错误已在 onError 中处理
      }
    }

    if (failCount === 0) {
      toast.success(`成功清除所有 ${successCount} 个已禁用凭据`)
    } else {
      toast.warning(`清除已禁用凭据：成功 ${successCount} 个，失败 ${failCount} 个`)
    }

    deselectAll()
  }

  // 查询当前页凭据信息（只读已缓存余额快照，一次拉取、绝不触发上游调用）。
  // 封号红线：绝不批量主动拉 per-account balance。后端后台每 30 分钟温和刷新缓存，
  // 这里读的是最近已知值 + cachedAt 新鲜度，零上游、零风控风险。
  const handleQueryCurrentPageInfo = async () => {
    if (currentCredentials.length === 0) {
      toast.error('当前页没有可查询的凭据')
      return
    }

    const ids = currentCredentials
      .filter(credential => !credential.disabled)
      .map(credential => credential.id)

    if (ids.length === 0) {
      toast.error('当前页没有可查询的启用凭据')
      return
    }

    setQueryingInfo(true)
    setQueryInfoProgress({ current: 0, total: ids.length })

    try {
      // 单次拉取全部已缓存余额快照（后端只读缓存，不打上游）。
      const { balances } = await getCachedBalances()

      // 命中的（id, 缓存快照）对——在 setState 之外算好，避免依赖更新器执行时机。
      const hits = ids
        .map(id => [id, balances[String(id)]] as const)
        .filter(([, cached]) => !!cached)

      setBalanceMap(prev => {
        const next = new Map(prev)
        for (const [id, cached] of hits) {
          next.set(id, cached)
        }
        return next
      })

      setQueryInfoProgress({ current: ids.length, total: ids.length })

      const hitCount = hits.length
      const missCount = ids.length - hitCount
      if (missCount === 0) {
        toast.success(`已读取缓存余额：${hitCount}/${ids.length}`)
      } else {
        toast.warning(`已读取缓存余额：${hitCount}/${ids.length}（${missCount} 个尚无缓存，后台会温和刷新）`)
      }
    } catch (error) {
      toast.error('读取缓存余额失败')
    } finally {
      setQueryingInfo(false)
    }
  }

  // 批量验活
  const handleBatchVerify = async () => {
    if (selectedIds.size === 0) {
      toast.error('请先选择要验活的凭据')
      return
    }

    // 初始化状态
    setVerifying(true)
    cancelVerifyRef.current = false
    const ids = Array.from(selectedIds)
    setVerifyProgress({ current: 0, total: ids.length })

    let successCount = 0

    // 初始化结果，所有凭据状态为 pending
    const initialResults = new Map<number, VerifyResult>()
    ids.forEach(id => {
      initialResults.set(id, { id, status: 'pending' })
    })
    setVerifyResults(initialResults)
    setVerifyDialogOpen(true)

    // 开始验活
    for (let i = 0; i < ids.length; i++) {
      // 检查是否取消
      if (cancelVerifyRef.current) {
        toast.info('已取消验活')
        break
      }

      const id = ids[i]

      // 更新当前凭据状态为 verifying
      setVerifyResults(prev => {
        const newResults = new Map(prev)
        newResults.set(id, { id, status: 'verifying' })
        return newResults
      })

      try {
        // 深度验活：发真实 API 请求检测 suspend 状态
        await deepVerifyCredential(id)
        const balance = await getCredentialBalance(id)
        successCount++

        // 更新为成功状态
        setVerifyResults(prev => {
          const newResults = new Map(prev)
          newResults.set(id, {
            id,
            status: 'success',
            usage: `${balance.currentUsage}/${balance.usageLimit}`
          })
          return newResults
        })
      } catch (error) {
        // 更新为失败状态
        setVerifyResults(prev => {
          const newResults = new Map(prev)
          newResults.set(id, {
            id,
            status: 'failed',
            error: extractErrorMessage(error)
          })
          return newResults
        })
      }

      // 更新进度
      setVerifyProgress({ current: i + 1, total: ids.length })

      // 添加延迟防止封号（最后一个不需要延迟）
      if (i < ids.length - 1 && !cancelVerifyRef.current) {
        await new Promise(resolve => setTimeout(resolve, 2000))
      }
    }

    setVerifying(false)

    if (!cancelVerifyRef.current) {
      toast.success(`验活完成：成功 ${successCount}/${ids.length}`)
    }
  }

  // 取消验活
  const handleCancelVerify = () => {
    cancelVerifyRef.current = true
    setVerifying(false)
  }

  // 切换负载均衡模式
  const handleToggleLoadBalancing = () => {
    const currentMode = loadBalancingData?.mode || 'priority'
    const newMode = currentMode === 'priority' ? 'balanced' : 'priority'

    setLoadBalancingMode(newMode, {
      onSuccess: () => {
        const modeName = newMode === 'priority' ? '优先级模式' : '均衡负载模式'
        toast.success(`已切换到${modeName}`)
      },
      onError: (error) => {
        toast.error(`切换失败: ${extractErrorMessage(error)}`)
      }
    })
  }

  if (isLoading) {
    // 骨架屏替代蓝色转圈圈：贴合凭据管理页布局(统计卡 + 凭据卡网格)
    return (
      <div className={embedded ? "" : "min-h-screen bg-background p-8"}>
        <PageSkeleton kind="credentials" />
      </div>
    )
  }

  if (error) {
    return (
      <div className={embedded ? "flex items-center justify-center py-24 p-4" : "min-h-screen flex items-center justify-center bg-background p-4"}>
        <Card className="w-full max-w-md">
          <CardContent className="pt-6 text-center">
            <div className="text-red-500 mb-4">加载失败</div>
            <p className="text-muted-foreground mb-4">{(error as Error).message}</p>
            <div className="space-x-2">
              <Button onClick={() => refetch()}>重试</Button>
              <Button variant="outline" onClick={handleLogout}>重新登录</Button>
            </div>
          </CardContent>
        </Card>
      </div>
    )
  }

  return (
    <div className={embedded ? "" : "min-h-screen bg-background"}>
      {/* 顶部导航（仅独立模式显示；内嵌时由 AppShell 提供） */}
      {!embedded && (
      <header className="sticky top-0 z-50 w-full border-b bg-background/95 backdrop-blur supports-[backdrop-filter]:bg-background/60">
        <div className="container flex h-14 items-center justify-between px-4 md:px-8">
          <div className="flex items-center gap-2">
            <Server className="h-5 w-5" />
            <span className="font-semibold">Kiro Admin</span>
          </div>
          <div className="flex items-center gap-2">
            <Button
              variant="outline"
              size="sm"
              onClick={handleToggleLoadBalancing}
              disabled={isLoadingMode || isSettingMode}
              title="切换负载均衡模式"
            >
              {isLoadingMode ? '加载中...' : (loadBalancingData?.mode === 'priority' ? '优先级模式' : '均衡负载')}
            </Button>
            <Button variant="ghost" size="icon" onClick={toggleDarkMode}>
              {darkMode ? <Sun className="h-5 w-5" /> : <Moon className="h-5 w-5" />}
            </Button>
            <Button variant="ghost" size="icon" onClick={handleRefresh}>
              <RefreshCw className="h-5 w-5" />
            </Button>
            <Button variant="ghost" size="icon" onClick={handleLogout}>
              <LogOut className="h-5 w-5" />
            </Button>
          </div>
        </div>
      </header>
      )}

      {/* 主内容 */}
      <main className={embedded ? "" : "container mx-auto px-4 md:px-8 py-6"}>
        {/* 统计卡片 */}
        <div className="grid gap-4 md:grid-cols-3 mb-6">
          <StatCard
            label="凭据总数"
            value={data?.total ?? 0}
            hint={`${disabledCredentialCount} 个已禁用`}
            icon={Database}
            accent="neutral"
          />
          <StatCard
            label="可用凭据"
            value={data?.available ?? 0}
            hint={data && data.total > 0 ? `占总数 ${Math.round((data.available / data.total) * 100)}%` : '暂无凭据'}
            icon={CheckCircle2}
            accent="success"
          />
          <StatCard
            label="当前活跃"
            value={data?.currentId ? `#${data.currentId}` : '—'}
            icon={Zap}
            accent={data?.currentId ? 'primary' : 'neutral'}
            hint={
              data?.currentId ? (
                <span className="flex min-w-0 items-center gap-1.5">
                  <span className="relative flex h-2 w-2 shrink-0">
                    <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-primary/60" />
                    <span className="relative inline-flex h-2 w-2 rounded-full bg-primary" />
                  </span>
                  <ScrollOnOverflow
                    className="min-w-0 flex-1"
                    text={
                      currentCredential?.email
                        ? currentCredential.email
                        : currentBalance?.subscriptionTitle
                          ? currentBalance.subscriptionTitle
                          : '正在处理请求'
                    }
                  />
                </span>
              ) : (
                '暂无活跃凭据'
              )
            }
          />
        </div>

        {/* 凭据列表 */}
        <div className="space-y-4">
          {/* 工具栏：固定横向 flex-wrap + 最小高度，选中前后结构稳定不跳动 */}
          <div className="flex flex-wrap items-center justify-between gap-3 min-h-[2.5rem]">
            <div className="flex items-center gap-3">
              <h2 className="text-xl font-semibold">凭据管理</h2>
              {/* 选中信息固定占位：选中数为 0 时也占位，避免整块横竖流向跳动 */}
              <div className="flex items-center gap-2 min-h-[2rem]">
                {selectedIds.size > 0 ? (
                  <>
                    <Badge variant="secondary">已选择 {selectedIds.size} 个</Badge>
                    <Button onClick={deselectAll} size="sm" variant="ghost">
                      取消选择
                    </Button>
                  </>
                ) : (
                  <span className="text-sm text-muted-foreground">
                    勾选复选框 或 Ctrl+左键 选择；右键卡片打开设置
                  </span>
                )}
              </div>
            </div>
            <div className="flex gap-2 flex-wrap">
              {embedded && (
                <>
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={handleToggleLoadBalancing}
                    disabled={isLoadingMode || isSettingMode}
                    title="切换负载均衡模式"
                  >
                    {isLoadingMode ? '加载中...' : (loadBalancingData?.mode === 'priority' ? '优先级模式' : '均衡负载')}
                  </Button>
                  <Button variant="outline" size="sm" onClick={handleRefresh} title="刷新列表">
                    <RefreshCw className="h-4 w-4" />
                  </Button>
                </>
              )}
              {selectedIds.size > 0 && (
                <>
                  <Button onClick={handleBatchVerify} size="sm" variant="outline">
                    <CheckCircle2 className="h-4 w-4 mr-2" />
                    批量验活
                  </Button>
                  <Button onClick={handleTestModels} size="sm" variant="outline">
                    <FlaskConical className="h-4 w-4 mr-2" />
                    测试可用模型
                  </Button>
                  <Button
                    onClick={handleBatchForceRefresh}
                    size="sm"
                    variant="outline"
                    disabled={batchRefreshing}
                  >
                    <RefreshCw className={`h-4 w-4 mr-2 ${batchRefreshing ? 'animate-spin' : ''}`} />
                    {batchRefreshing ? `刷新中... ${batchRefreshProgress.current}/${batchRefreshProgress.total}` : '批量刷新 Token'}
                  </Button>
                  <Button onClick={handleBatchResetFailure} size="sm" variant="outline">
                    <RotateCcw className="h-4 w-4 mr-2" />
                    恢复异常
                  </Button>
                  <Button
                    onClick={() => handleBatchSetDisabled(true)}
                    size="sm"
                    variant="outline"
                    disabled={selectedIds.size - selectedDisabledCount === 0}
                    title={selectedIds.size - selectedDisabledCount === 0 ? '选中的凭据都已禁用' : undefined}
                  >
                    <Ban className="h-4 w-4 mr-2" />
                    批量禁用
                  </Button>
                  <Button
                    onClick={() => handleBatchSetDisabled(false)}
                    size="sm"
                    variant="outline"
                    disabled={selectedDisabledCount === 0}
                    title={selectedDisabledCount === 0 ? '选中的凭据都已启用' : undefined}
                  >
                    <Power className="h-4 w-4 mr-2" />
                    批量启用
                  </Button>
                  <Button
                    onClick={handleBatchDelete}
                    size="sm"
                    variant="destructive"
                    disabled={selectedDisabledCount === 0}
                    title={selectedDisabledCount === 0 ? '只能删除已禁用凭据' : undefined}
                  >
                    <Trash2 className="h-4 w-4 mr-2" />
                    批量删除
                  </Button>
                </>
              )}
              {verifying && !verifyDialogOpen && (
                <Button onClick={() => setVerifyDialogOpen(true)} size="sm" variant="secondary">
                  <CheckCircle2 className="h-4 w-4 mr-2 animate-spin" />
                  验活中... {verifyProgress.current}/{verifyProgress.total}
                </Button>
              )}
              {data?.credentials && data.credentials.length > 0 && (
                <Button
                  onClick={handleQueryCurrentPageInfo}
                  size="sm"
                  variant="outline"
                  disabled={queryingInfo}
                >
                  <RefreshCw className={`h-4 w-4 mr-2 ${queryingInfo ? 'animate-spin' : ''}`} />
                  {queryingInfo ? `查询中... ${queryInfoProgress.current}/${queryInfoProgress.total}` : '查询信息'}
                </Button>
              )}
              {data?.credentials && data.credentials.length > 0 && (
                <Button
                  onClick={handleClearAll}
                  size="sm"
                  variant="outline"
                  className="text-destructive hover:text-destructive"
                  disabled={disabledCredentialCount === 0}
                  title={disabledCredentialCount === 0 ? '没有可清除的已禁用凭据' : undefined}
                >
                  <Trash2 className="h-4 w-4 mr-2" />
                  清除已禁用
                </Button>
              )}
              {/* KAM 导入 / 批量导入 / 上号 已合并进「添加凭据」弹框的分页（手动/导入粘贴/上号），
                  这里只保留单一入口，简化工具栏。 */}
              <Button onClick={() => setAddDialogOpen(true)} size="sm" variant="default">
                <Plus className="h-4 w-4 mr-2" />
                添加凭据
              </Button>
            </div>
          </div>
          {data?.credentials.length === 0 ? (
            <Card>
              <CardContent className="py-8 text-center text-muted-foreground">
                暂无凭据
              </CardContent>
            </Card>
          ) : (
            <>
              <div className="grid gap-4 md:grid-cols-2 lg:grid-cols-3">
                {currentCredentials.map((credential) => (
                  <CredentialCard
                    key={credential.id}
                    credential={credential}
                    onViewBalance={handleViewBalance}
                    selected={selectedIds.has(credential.id)}
                    onToggleSelect={() => toggleSelect(credential.id)}
                    balance={balanceMap.get(credential.id) || null}
                    loadingBalance={loadingBalanceIds.has(credential.id)}
                  />
                ))}
              </div>

              {/* 分页控件 */}
              {totalPages > 1 && (
                <div className="flex justify-center items-center gap-4 mt-6">
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() => setCurrentPage(p => Math.max(1, p - 1))}
                    disabled={currentPage === 1}
                  >
                    上一页
                  </Button>
                  <span className="text-sm text-muted-foreground">
                    第 {currentPage} / {totalPages} 页（共 {data?.credentials.length} 个凭据）
                  </span>
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() => setCurrentPage(p => Math.min(totalPages, p + 1))}
                    disabled={currentPage === totalPages}
                  >
                    下一页
                  </Button>
                </div>
              )}
            </>
          )}
        </div>
      </main>

      {/* 余额对话框 */}
      <BalanceDialog
        credentialId={selectedCredentialId}
        open={balanceDialogOpen}
        onOpenChange={setBalanceDialogOpen}
      />

      {/* 添加凭据对话框（已合并 手动添加 / 导入粘贴 / 上号 三种模式，
          原独立的 上号 / 批量导入 / KAM 导入 弹框已并入此处） */}
      <AddCredentialDialog
        open={addDialogOpen}
        onOpenChange={setAddDialogOpen}
      />

      {/* 批量验活对话框 */}
      <BatchVerifyDialog
        open={verifyDialogOpen}
        onOpenChange={setVerifyDialogOpen}
        verifying={verifying}
        progress={verifyProgress}
        results={verifyResults}
        onCancel={handleCancelVerify}
      />
      <ModelTestDialog
        open={modelTestOpen}
        onOpenChange={setModelTestOpen}
        credentialIds={modelTestIds}
        credentials={data?.credentials ?? []}
        onProbe={(id, models) => probeAvailableModels(id, models)}
        onSetWhitelist={async (id, models) => {
          await setCredentialAllowedModels(id, models)
          queryClient.invalidateQueries({ queryKey: ['credentials'] })
        }}
      />
    </div>
  )
}
