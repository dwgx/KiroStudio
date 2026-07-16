import { useState, useEffect, useRef } from 'react'
import { RefreshCw, LogOut, Moon, Sun, Server, Plus, Trash2, RotateCcw, CheckCircle2, Database, Zap, Ban, Power, FlaskConical, Download } from 'lucide-react'
import { useQueryClient } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
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
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
  DialogFooter,
} from '@/components/ui/dialog'
import { useCredentials, useDeleteCredential, useResetFailure, useLoadBalancingMode, useSetLoadBalancingMode, useSetDisabled } from '@/hooks/use-credentials'
import { getCredentialBalance, getCachedBalances, forceRefreshToken, deepVerifyCredential, probeAvailableModels, setCredentialAllowedModels, PROBE_MODEL_CATALOG, exportCredential } from '@/api/credentials'
import { extractErrorMessage, downloadJson, fileStamp } from '@/lib/utils'
import { PageSkeleton } from '@/components/ui/page-skeleton'
import { useUiLayoutPrefs } from '@/hooks/use-ui-layout-prefs'
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
  const { t } = useTranslation()
  const [selectedCredentialId, setSelectedCredentialId] = useState<number | null>(null)
  const [balanceDialogOpen, setBalanceDialogOpen] = useState(false)
  const [addDialogOpen, setAddDialogOpen] = useState(false)
  const [selectedIds, setSelectedIds] = useState<Set<number>>(new Set())
  // 二次确认弹框(替代浏览器原生 confirm,统一走设计系统控件):
  // 存待执行动作 + 文案,确认后调 onConfirm。批量删除/清除已禁用都走它。
  const [confirmState, setConfirmState] = useState<{
    title: string
    description: string
    confirmText: string
    onConfirm: () => void | Promise<void>
  } | null>(null)
  const [confirmBusy, setConfirmBusy] = useState(false)
  // 批量设"允许模型白名单"本地编辑集合(勾选后弹窗用):空=不限制。
  const [batchAllowedModels, setBatchAllowedModels] = useState<Set<string>>(new Set())
  const [batchWhitelistBusy, setBatchWhitelistBusy] = useState(false)
  // 「允许模型」弹窗开关(勾选后按钮触发,交互对齐「测试可用模型」)。
  const [batchWhitelistOpen, setBatchWhitelistOpen] = useState(false)
  // UI 排版偏好:凭据卡片尺寸档位 → 自适应每行 N 个(设置页配置)。
  const { prefs: uiPrefs } = useUiLayoutPrefs()
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
  const [exportingSelected, setExportingSelected] = useState(false)
  const [exportProgress, setExportProgress] = useState({ current: 0, total: 0 })
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
    toast.success(t('dashboard.toast.refreshed'))
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
      toast.error(t('dashboard.batchDelete.noSelection'))
      return
    }

    const disabledIds = Array.from(selectedIds).filter(id => {
      const credential = data?.credentials.find(c => c.id === id)
      return Boolean(credential?.disabled)
    })

    if (disabledIds.length === 0) {
      toast.error(t('dashboard.batchDelete.noDisabled'))
      return
    }

    const skippedCount = selectedIds.size - disabledIds.length
    const skippedText = skippedCount > 0 ? t('dashboard.batchDelete.skipHint', { count: skippedCount }) : ''

    // 走设计系统 ConfirmDialog(不用浏览器原生 confirm)。确认后再执行删除。
    setConfirmState({
      title: t('dashboard.batchDelete.confirmTitle'),
      description: t('dashboard.batchDelete.confirmDesc', { count: disabledIds.length, skipped: skippedText }),
      confirmText: t('dashboard.batchDelete.confirmBtn'),
      onConfirm: async () => {
        let successCount = 0
        let failCount = 0
        for (const id of disabledIds) {
          try {
            await new Promise<void>((resolve, reject) => {
              deleteCredential(id, {
                onSuccess: () => { successCount++; resolve() },
                onError: (err) => { failCount++; reject(err) },
              })
            })
          } catch (error) {
            // 错误已在 onError 中处理
          }
        }
        const skippedResultText = skippedCount > 0 ? t('dashboard.batchDelete.skippedResult', { count: skippedCount }) : ''
        if (failCount === 0) {
          toast.success(t('dashboard.batchDelete.successToast', { count: successCount, skipped: skippedResultText }))
        } else {
          toast.warning(t('dashboard.batchDelete.warnToast', { ok: successCount, fail: failCount, skipped: skippedResultText }))
        }
        deselectAll()
      },
    })
  }

  // 批量恢复异常
  const handleBatchResetFailure = async () => {
    if (selectedIds.size === 0) {
      toast.error(t('dashboard.batchReset.noSelection'))
      return
    }

    const failedIds = Array.from(selectedIds).filter(id => {
      const cred = data?.credentials.find(c => c.id === id)
      return cred && cred.failureCount > 0
    })

    if (failedIds.length === 0) {
      toast.error(t('dashboard.batchReset.noFailed'))
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
      toast.success(t('dashboard.batchReset.successToast', { count: successCount }))
    } else {
      toast.warning(t('dashboard.batchReset.warnToast', { ok: successCount, fail: failCount }))
    }

    deselectAll()
  }

  // 批量启用 / 禁用：对选中项统一设为目标状态（逐个调用，跳过已是目标态的）
  const handleBatchSetDisabled = async (disabled: boolean) => {
    if (selectedIds.size === 0) {
      toast.error(disabled ? t('dashboard.batchDisable.noSelectionDisable') : t('dashboard.batchDisable.noSelectionEnable'))
      return
    }
    const targetIds = Array.from(selectedIds).filter(id => {
      const cred = data?.credentials.find(c => c.id === id)
      return cred && cred.disabled !== disabled
    })
    if (targetIds.length === 0) {
      toast.error(disabled ? t('dashboard.batchDisable.allDisabled') : t('dashboard.batchDisable.allEnabled'))
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

    const action = disabled ? t('dashboard.batchDisable.actionDisable') : t('dashboard.batchDisable.actionEnable')
    if (failCount === 0) {
      toast.success(t('dashboard.batchDisable.successToast', { action, count: successCount }))
    } else {
      toast.warning(t('dashboard.batchDisable.warnToast', { action, ok: successCount, fail: failCount }))
    }
    deselectAll()
  }

  const toggleBatchAllowedModel = (mid: string) => {
    setBatchAllowedModels((prev) => {
      const n = new Set(prev)
      if (n.has(mid)) n.delete(mid)
      else n.add(mid)
      return n
    })
  }

  // 批量设"允许模型白名单"(成本安全硬门)。破坏性整体覆盖 → 走 confirmState 二次确认。
  // custom_api 号无 Kiro 白名单概念,循环跳过并计入 skipped。空集=清空(设为不限制)。
  const handleBatchSetAllowedModels = () => {
    if (selectedIds.size === 0) {
      toast.error(t('dashboard.batchWhitelist.noSelection'))
      return
    }
    const list = Array.from(batchAllowedModels)
    const targetIds = Array.from(selectedIds)
    const setEmpty = list.length === 0
    setConfirmState({
      title: setEmpty ? t('dashboard.batchWhitelist.clearTitle') : t('dashboard.batchWhitelist.setTitle'),
      description: setEmpty
        ? t('dashboard.batchWhitelist.clearDesc', { count: targetIds.length })
        : t('dashboard.batchWhitelist.setDesc', { count: targetIds.length, n: list.length }),
      confirmText: setEmpty ? t('dashboard.batchWhitelist.clearBtn') : t('dashboard.batchWhitelist.overwriteBtn'),
      onConfirm: async () => {
        setBatchWhitelistBusy(true)
        let ok = 0, fail = 0, skipped = 0
        for (const id of targetIds) {
          const cred = data?.credentials.find(c => c.id === id)
          if (cred && (cred.authMethod === 'custom_api' || cred.baseUrl)) { skipped++; continue }
          try {
            await setCredentialAllowedModels(id, setEmpty ? null : list)
            ok++
          } catch { fail++ }
        }
        queryClient.invalidateQueries({ queryKey: ['credentials'] })
        setBatchWhitelistBusy(false)
        const skipText = skipped > 0 ? t('dashboard.batchWhitelist.skipText', { count: skipped }) : ''
        if (fail === 0) toast.success(t('dashboard.batchWhitelist.successToast', { count: ok, skip: skipText }))
        else toast.warning(t('dashboard.batchWhitelist.warnToast', { ok, fail, skip: skipText }))
      },
    })
  }

  // 批量刷新 Token
  const handleBatchForceRefresh = async () => {
    if (selectedIds.size === 0) {
      toast.error(t('dashboard.batchRefresh.noSelection'))
      return
    }

    const enabledIds = Array.from(selectedIds).filter(id => {
      const cred = data?.credentials.find(c => c.id === id)
      return cred && !cred.disabled
    })

    if (enabledIds.length === 0) {
      toast.error(t('dashboard.batchRefresh.noEnabled'))
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
      toast.success(t('dashboard.batchRefresh.successToast', { count: successCount }))
    } else {
      toast.warning(t('dashboard.batchRefresh.warnToast', { ok: successCount, fail: failCount }))
    }

    deselectAll()
  }

  // 批量导出选中凭据为 JSON（格式与设置页「导出全部」一致，仅过滤成选中项）。
  // 串行调后端 export 端点逐个拉完整导出对象，打包成数组一次性下载。
  const handleExportSelected = async () => {
    const ids = Array.from(selectedIds)
    if (ids.length === 0) {
      toast.error('请先勾选要导出的凭据')
      return
    }
    setExportingSelected(true)
    setExportProgress({ current: 0, total: ids.length })
    try {
      const all: Record<string, unknown>[] = []
      for (let i = 0; i < ids.length; i++) {
        all.push(await exportCredential(ids[i]))
        setExportProgress({ current: i + 1, total: ids.length })
      }
      downloadJson(`credentials-selected-${fileStamp()}.json`, all)
      toast.success(`已导出选中的 ${all.length} 个凭据`)
    } catch (err) {
      toast.error(extractErrorMessage(err))
    } finally {
      setExportingSelected(false)
    }
  }

  // 测试可用模型：把勾选的凭据 id 定格，打开独立弹窗（弹窗内自选模型、可反复测、保留结果）。
  const handleTestModels = () => {
    const ids = Array.from(selectedIds)
    if (ids.length === 0) {
      toast.error(t('dashboard.testModels.noSelection'))
      return
    }
    setModelTestIds(ids)
    setModelTestOpen(true)
  }

  // 一键清除所有已禁用凭据
  const handleClearAll = async () => {
    if (!data?.credentials || data.credentials.length === 0) {
      toast.error(t('dashboard.clearAll.noCredentials'))
      return
    }

    const disabledCredentials = data.credentials.filter(credential => credential.disabled)

    if (disabledCredentials.length === 0) {
      toast.error(t('dashboard.clearAll.noDisabled'))
      return
    }

    setConfirmState({
      title: t('dashboard.clearAll.confirmTitle'),
      description: t('dashboard.clearAll.confirmDesc', { count: disabledCredentials.length }),
      confirmText: t('dashboard.clearAll.confirmBtn'),
      onConfirm: async () => {
        let successCount = 0
        let failCount = 0
        for (const credential of disabledCredentials) {
          try {
            await new Promise<void>((resolve, reject) => {
              deleteCredential(credential.id, {
                onSuccess: () => { successCount++; resolve() },
                onError: (err) => { failCount++; reject(err) },
              })
            })
          } catch (error) {
            // 错误已在 onError 中处理
          }
        }
        if (failCount === 0) {
          toast.success(t('dashboard.clearAll.successToast', { count: successCount }))
        } else {
          toast.warning(t('dashboard.clearAll.warnToast', { ok: successCount, fail: failCount }))
        }
        deselectAll()
      },
    })
  }

  // 查询当前页凭据信息（只读已缓存余额快照，一次拉取、绝不触发上游调用）。
  // 封号红线：绝不批量主动拉 per-account balance。后端后台每 30 分钟温和刷新缓存，
  // 这里读的是最近已知值 + cachedAt 新鲜度，零上游、零风控风险。
  const handleQueryCurrentPageInfo = async () => {
    if (currentCredentials.length === 0) {
      toast.error(t('dashboard.queryInfo.noCredentials'))
      return
    }

    const ids = currentCredentials
      .filter(credential => !credential.disabled)
      .map(credential => credential.id)

    if (ids.length === 0) {
      toast.error(t('dashboard.queryInfo.noEnabled'))
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
        toast.success(t('dashboard.queryInfo.successToast', { hit: hitCount, total: ids.length }))
      } else {
        toast.warning(t('dashboard.queryInfo.warnToast', { hit: hitCount, total: ids.length, miss: missCount }))
      }
    } catch (error) {
      toast.error(t('dashboard.queryInfo.errorToast'))
    } finally {
      setQueryingInfo(false)
    }
  }

  // 批量验活
  const handleBatchVerify = async () => {
    // 去重:已在验活中(含"后台运行"关窗后 verifying 仍 true)则忽略重入,
    // 避免两个并发验活循环共享 cancelVerifyRef/进度 state 互相覆盖、且把 2s 防封号间隔打穿。
    if (verifying) return
    if (selectedIds.size === 0) {
      toast.error(t('dashboard.verify.noSelection'))
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
        toast.info(t('dashboard.verify.canceled'))
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
        // 深度验活：发真实 API 请求检测 suspend 状态(custom_api 走透传上游探测,后端已分流)
        await deepVerifyCredential(id)
        // await 期间用户可能点了取消:复检,取消则不再记成功/不再多发余额请求。
        if (cancelVerifyRef.current) {
          toast.info(t('dashboard.verify.canceled'))
          break
        }
        const cred = data?.credentials.find(c => c.id === id)
        const isCustomApi = cred?.authMethod === 'custom_api' || !!cred?.baseUrl
        // custom_api 号无 Kiro 余额概念,跳过 getCredentialBalance(对透传号必失败),usage 显"可达/请求数"
        let usage: string
        if (isCustomApi) {
          usage = t('dashboard.verify.usageReachable', { count: cred?.requestCount ?? 0 })
        } else {
          const balance = await getCredentialBalance(id)
          usage = `${balance.currentUsage}/${balance.usageLimit}`
        }
        successCount++

        // 更新为成功状态
        setVerifyResults(prev => {
          const newResults = new Map(prev)
          newResults.set(id, { id, status: 'success', usage })
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
      toast.success(t('dashboard.verify.completeToast', { ok: successCount, total: ids.length }))
    }
  }

  // 取消验活:只置取消标志,**不**提前翻 verifying。verifying 由验活循环真正退出时(第 652 行)
  // 统一置 false——否则 cancel 后循环还卡在 await(deepVerify/2s sleep)期间 verifying=false,
  // 重入守卫 if(verifying)return 失效,用户再点会起第二个并发循环打穿 2s 防封号间隔+进度错乱。
  const handleCancelVerify = () => {
    cancelVerifyRef.current = true
  }

  // 切换负载均衡模式
  const handleToggleLoadBalancing = () => {
    const currentMode = loadBalancingData?.mode || 'priority'
    const newMode = currentMode === 'priority' ? 'balanced' : 'priority'

    setLoadBalancingMode(newMode, {
      onSuccess: () => {
        const modeName = newMode === 'priority' ? t('dashboard.loadBalancing.priorityMode') : t('dashboard.loadBalancing.balancedMode')
        toast.success(t('dashboard.loadBalancing.switchedToast', { mode: modeName }))
      },
      onError: (error) => {
        toast.error(t('dashboard.loadBalancing.switchFailToast', { error: extractErrorMessage(error) }))
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
            <div className="text-red-500 mb-4">{t('dashboard.error.loadFailed')}</div>
            <p className="text-muted-foreground mb-4">{(error as Error).message}</p>
            <div className="space-x-2">
              <Button onClick={() => refetch()}>{t('dashboard.error.retry')}</Button>
              <Button variant="outline" onClick={handleLogout}>{t('dashboard.error.relogin')}</Button>
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
            <span className="font-semibold">{t('dashboard.header.title')}</span>
          </div>
          <div className="flex items-center gap-2">
            <Button
              variant="outline"
              size="sm"
              onClick={handleToggleLoadBalancing}
              disabled={isLoadingMode || isSettingMode}
              title={t('dashboard.loadBalancing.switchTitle')}
            >
              {isLoadingMode ? t('dashboard.loadBalancing.loading') : (loadBalancingData?.mode === 'priority' ? t('dashboard.loadBalancing.priorityMode') : t('dashboard.loadBalancing.balancedShort'))}
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
            label={t('dashboard.stat.totalCredentials')}
            value={data?.total ?? 0}
            hint={t('dashboard.stat.disabledHint', { count: disabledCredentialCount })}
            icon={Database}
            accent="neutral"
          />
          <StatCard
            label={t('dashboard.stat.availableCredentials')}
            value={data?.available ?? 0}
            hint={data && data.total > 0 ? t('dashboard.stat.percentHint', { percent: Math.round((data.available / data.total) * 100) }) : t('dashboard.stat.noCredentialsHint')}
            icon={CheckCircle2}
            accent="success"
          />
          <StatCard
            label={t('dashboard.stat.currentActive')}
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
                          : t('dashboard.stat.processingRequest')
                    }
                  />
                </span>
              ) : (
                t('dashboard.stat.noActiveCredential')
              )
            }
          />
        </div>

        {/* 凭据列表 */}
        <div className="space-y-4">
          {/* 工具栏：固定横向 flex-wrap + 最小高度，选中前后结构稳定不跳动 */}
          <div className="flex flex-wrap items-center justify-between gap-3 min-h-[2.5rem]">
            <div className="flex items-center gap-3">
              <h2 className="text-xl font-semibold">{t('dashboard.section.credentialManagement')}</h2>
              {/* 选中信息 + 交互说明(常驻,选中时也可见,不再被 Badge 替换掉) */}
              <div className="flex items-center gap-2 min-h-[2rem]">
                {selectedIds.size > 0 && (
                  <>
                    <Badge variant="secondary">{t('dashboard.selection.selectedCount', { count: selectedIds.size })}</Badge>
                    <Button onClick={deselectAll} size="sm" variant="ghost">
                      {t('dashboard.selection.deselect')}
                    </Button>
                  </>
                )}
                <span className="text-sm text-muted-foreground">
                  {t('dashboard.selection.hint')}
                </span>
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
                    title={t('dashboard.loadBalancing.switchTitle')}
                  >
                    {isLoadingMode ? t('dashboard.loadBalancing.loading') : (loadBalancingData?.mode === 'priority' ? t('dashboard.loadBalancing.priorityMode') : t('dashboard.loadBalancing.balancedShort'))}
                  </Button>
                  <Button variant="outline" size="sm" onClick={handleRefresh} title={t('dashboard.toolbar.refreshList')}>
                    <RefreshCw className="h-4 w-4" />
                  </Button>
                </>
              )}
              {selectedIds.size > 0 && (
                <>
                  <Button onClick={handleBatchVerify} size="sm" variant="outline" disabled={verifying}>
                    <CheckCircle2 className="h-4 w-4 mr-2" />
                    {t('dashboard.toolbar.batchVerify')}
                  </Button>
                  <Button onClick={handleExportSelected} size="sm" variant="outline" disabled={exportingSelected}>
                    <Download className={`h-4 w-4 mr-2 ${exportingSelected ? 'animate-spin' : ''}`} />
                    {exportingSelected ? `导出中... ${exportProgress.current}/${exportProgress.total}` : '导出选中'}
                  </Button>
                  <Button onClick={handleTestModels} size="sm" variant="outline">
                    <FlaskConical className="h-4 w-4 mr-2" />
                    {t('dashboard.toolbar.testModels')}
                  </Button>
                  <Button onClick={() => setBatchWhitelistOpen(true)} size="sm" variant="outline">
                    <Zap className="h-4 w-4 mr-2" />
                    {t('dashboard.toolbar.allowedModels')}
                  </Button>
                  <Button
                    onClick={handleBatchForceRefresh}
                    size="sm"
                    variant="outline"
                    disabled={batchRefreshing}
                  >
                    <RefreshCw className={`h-4 w-4 mr-2 ${batchRefreshing ? 'animate-spin' : ''}`} />
                    {batchRefreshing ? t('dashboard.toolbar.refreshingToken', { current: batchRefreshProgress.current, total: batchRefreshProgress.total }) : t('dashboard.toolbar.batchRefreshToken')}
                  </Button>
                  <Button onClick={handleBatchResetFailure} size="sm" variant="outline">
                    <RotateCcw className="h-4 w-4 mr-2" />
                    {t('dashboard.toolbar.resetFailure')}
                  </Button>
                  <Button
                    onClick={() => handleBatchSetDisabled(true)}
                    size="sm"
                    variant="outline"
                    disabled={selectedIds.size - selectedDisabledCount === 0}
                    title={selectedIds.size - selectedDisabledCount === 0 ? t('dashboard.toolbar.batchDisableTip') : undefined}
                  >
                    <Ban className="h-4 w-4 mr-2" />
                    {t('dashboard.toolbar.batchDisable')}
                  </Button>
                  <Button
                    onClick={() => handleBatchSetDisabled(false)}
                    size="sm"
                    variant="outline"
                    disabled={selectedDisabledCount === 0}
                    title={selectedDisabledCount === 0 ? t('dashboard.toolbar.batchEnableTip') : undefined}
                  >
                    <Power className="h-4 w-4 mr-2" />
                    {t('dashboard.toolbar.batchEnable')}
                  </Button>
                  <Button
                    onClick={handleBatchDelete}
                    size="sm"
                    variant="destructive"
                    disabled={selectedDisabledCount === 0}
                    title={selectedDisabledCount === 0 ? t('dashboard.toolbar.batchDeleteTip') : undefined}
                  >
                    <Trash2 className="h-4 w-4 mr-2" />
                    {t('dashboard.toolbar.batchDelete')}
                  </Button>
                </>
              )}
              {verifying && !verifyDialogOpen && (
                <Button onClick={() => setVerifyDialogOpen(true)} size="sm" variant="secondary">
                  <CheckCircle2 className="h-4 w-4 mr-2 animate-spin" />
                  {t('dashboard.toolbar.verifying', { current: verifyProgress.current, total: verifyProgress.total })}
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
                  {queryingInfo ? t('dashboard.toolbar.querying', { current: queryInfoProgress.current, total: queryInfoProgress.total }) : t('dashboard.toolbar.queryInfo')}
                </Button>
              )}
              {data?.credentials && data.credentials.length > 0 && (
                <Button
                  onClick={handleClearAll}
                  size="sm"
                  variant="outline"
                  className="text-destructive hover:text-destructive"
                  disabled={disabledCredentialCount === 0}
                  title={disabledCredentialCount === 0 ? t('dashboard.toolbar.clearDisabledTip') : undefined}
                >
                  <Trash2 className="h-4 w-4 mr-2" />
                  {t('dashboard.toolbar.clearDisabled')}
                </Button>
              )}
              {/* KAM 导入 / 批量导入 / 上号 已合并进「添加凭据」弹框的分页（手动/导入粘贴/上号），
                  这里只保留单一入口，简化工具栏。 */}
              <Button onClick={() => setAddDialogOpen(true)} size="sm" variant="default">
                <Plus className="h-4 w-4 mr-2" />
                {t('dashboard.toolbar.addCredential')}
              </Button>
            </div>
          </div>

          {data?.credentials.length === 0 ? (
            <Card>
              <CardContent className="py-8 text-center text-muted-foreground">
                {t('dashboard.list.empty')}
              </CardContent>
            </Card>
          ) : (
            <>
              <div
                className="grid gap-4"
                style={{
                  // 卡片尺寸档位 → 每列最小宽,auto-fill 按容器宽自动决定每行 N 个(紧凑~5、标准~4、大~3)。
                  gridTemplateColumns: `repeat(auto-fill, minmax(min(100%, ${
                    uiPrefs.cardSize === 'compact' ? 240 : uiPrefs.cardSize === 'large' ? 380 : 300
                  }px), 1fr))`,
                }}
              >
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
                    {t('dashboard.pagination.prev')}
                  </Button>
                  <span className="text-sm text-muted-foreground">
                    {t('dashboard.pagination.pageInfo', { current: currentPage, total: totalPages, count: data?.credentials.length })}
                  </span>
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() => setCurrentPage(p => Math.min(totalPages, p + 1))}
                    disabled={currentPage === totalPages}
                  >
                    {t('dashboard.pagination.next')}
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

      {/* 「允许模型」弹窗:勾选凭据后由工具栏「允许模型」按钮触发(交互对齐「测试可用模型」)。
          批量设成本白名单硬门;custom_api 号无白名单概念,应用时自动跳过。 */}
      <Dialog open={batchWhitelistOpen} onOpenChange={setBatchWhitelistOpen}>
        <DialogContent className="max-w-2xl">
          <DialogHeader>
            <DialogTitle>{t('dashboard.whitelistDialog.title', { count: selectedIds.size })}</DialogTitle>
            <DialogDescription>
              {batchAllowedModels.size === 0
                ? t('dashboard.whitelistDialog.descUnrestricted')
                : t('dashboard.whitelistDialog.descHardGate', { count: batchAllowedModels.size })}
              {t('dashboard.whitelistDialog.descSuffix')}
            </DialogDescription>
          </DialogHeader>
          <div className="flex flex-wrap gap-1.5 py-2">
            {PROBE_MODEL_CATALOG.map((m) => {
              const on = batchAllowedModels.has(m.id)
              return (
                <button
                  key={m.id}
                  type="button"
                  onClick={() => toggleBatchAllowedModel(m.id)}
                  className={`inline-flex items-center gap-1 rounded border px-2 py-1 text-[11px] font-medium transition-colors ${
                    on
                      ? 'border-primary/40 bg-primary/15 text-primary'
                      : 'border-white/10 bg-white/5 text-muted-foreground hover:border-white/25'
                  }`}
                  title={`${m.id} · ${m.mult}`}
                >
                  {m.id} <span className="opacity-60">{m.mult}</span>
                </button>
              )
            })}
          </div>
          <DialogFooter>
            {batchAllowedModels.size > 0 && (
              <Button size="sm" variant="ghost" onClick={() => setBatchAllowedModels(new Set())}>
                {t('dashboard.whitelistDialog.clearSelection')}
              </Button>
            )}
            <Button
              size="sm"
              disabled={batchWhitelistBusy}
              onClick={() => {
                handleBatchSetAllowedModels()
                setBatchWhitelistOpen(false)
              }}
            >
              {batchWhitelistBusy ? t('dashboard.whitelistDialog.applying') : t('dashboard.whitelistDialog.applyBtn', { count: selectedIds.size })}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* 统一二次确认弹框(替代浏览器原生 confirm):批量删除/清除已禁用等危险操作走它。 */}
      <Dialog open={!!confirmState} onOpenChange={(o) => { if (!o && !confirmBusy) setConfirmState(null) }}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>{confirmState?.title}</DialogTitle>
            <DialogDescription>{confirmState?.description}</DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setConfirmState(null)} disabled={confirmBusy}>
              {t('dashboard.confirm.cancel')}
            </Button>
            <Button
              variant="destructive"
              disabled={confirmBusy}
              onClick={async () => {
                if (!confirmState) return
                setConfirmBusy(true)
                try {
                  await confirmState.onConfirm()
                } finally {
                  setConfirmBusy(false)
                  setConfirmState(null)
                }
              }}
            >
              {confirmBusy ? t('dashboard.confirm.processing') : (confirmState?.confirmText ?? t('dashboard.confirm.default'))}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  )
}
