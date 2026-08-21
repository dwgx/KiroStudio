/**
 * 错误提示词配置弹窗（高级设置 → 错误提示词入口）。
 *
 * 参照 ops-detail-dialogs 的 TraceDetailDialog「页面化」模式：独立组件自带全部状态，
 * 受控 `open`/`onOpenChange`；配置数据复用设置页的 config-snapshot 查询缓存
 * （同 queryKey，React Query 去重，不产生额外请求），保存走 useUpdateConfig
 * （PUT /config 字段级 merge）。
 *
 * 契约（docs/error-codes-config-design.md §六）：
 * - **全量 key 表** = 内置默认表（GET /api/admin/error-messages/defaults，只读）
 *   ∪ 配置覆盖（GET /config 快照的 `errorMessages` 字段）。每行显示**当前生效值**
 *   （配置 or 默认）：字段草稿为空 = 该字段未配置，显示默认值预览 + 「默认」badge；
 *   编辑字段 = 写入草稿（非空即覆盖，保存后进入 config.errorMessages）。
 * - 保存：只提交**有改动的 key**，每条含该 key 全量非空字段（空字段省略 = 回落
 *   内置默认）；「恢复默认」= 清空草稿 → 提交空对象 = 后端删掉该 key 回默认。
 * - 校验失败：后端 400 整表拒绝，错误逐条显示在 toast（title + detail）。
 * - 默认表加载失败：横幅报错 + 重试，已配置 key 照常渲染（不空白）。
 * - 改动本地暂存（脏标记行高亮），「关闭」有未保存改动时二次确认。
 */
import { useEffect, useMemo, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { toast } from 'sonner'
import { ChevronLeft, ChevronRight, Inbox, MessageSquareWarning, RotateCcw, Search, SearchX, X } from 'lucide-react'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { ComboInput } from '@/components/ui/combo-input'
import { ConfirmDialog } from '@/components/ui/confirm-dialog'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { EmptyState } from '@/components/ui/empty-state'
import { Input } from '@/components/ui/input'
import { Skeleton } from '@/components/ui/skeleton'
import { useConfigSnapshot, useErrorMessagesDefaults, useUpdateConfig } from '@/hooks/use-credentials'
import { extractErrorMessage, parseError } from '@/lib/utils'
import type { ErrorMessageOverride } from '@/types/api'

const PAGE_SIZE = 10

/**
 * type 白名单：与后端 `ERROR_TYPE_WHITELIST` 对齐（并行 agent 已移除
 * `billing_error` / `quota_exceeded_error`——billing_error 触发 Claude Code CLI 层
 * 7 次重试，quota_exceeded_error 非官方类，均不再可配）。
 */
const TYPE_OPTIONS = [
  'invalid_request_error',
  'authentication_error',
  'permission_error',
  'not_found_error',
  'request_too_large',
  'rate_limit_error',
  'api_error',
  'overloaded_error',
]

/** status 白名单（设计文档 §二.1，对齐 exhausted_status 先例；504 = 上游超时形态）。 */
const STATUS_OPTIONS = ['400', '401', '403', '404', '413', '429', '500', '502', '503', '504']

// 单行草稿：字段全字符串化便于受控输入；空串 = 该字段不覆盖（回落内置默认）。
interface DraftRow {
  status: string
  type: string
  message: string
  retryAfterSecs: string
}

function emptyRow(): DraftRow {
  return { status: '', type: '', message: '', retryAfterSecs: '' }
}

// 配置表 → 草稿（retryAfterSecs 兼容 null/缺失两种"未配置"形态）。
function toDrafts(table: Record<string, ErrorMessageOverride> | undefined): Record<string, DraftRow> {
  const out: Record<string, DraftRow> = {}
  for (const [key, v] of Object.entries(table ?? {})) {
    out[key] = {
      status: v.status != null ? String(v.status) : '',
      type: v.type ?? '',
      message: v.message ?? '',
      retryAfterSecs: v.retryAfterSecs != null ? String(v.retryAfterSecs) : '',
    }
  }
  return out
}

// 草稿 → 覆盖项：只保留非空字段（空 = 不覆盖）。与后端 serde skip_serializing_if 对齐。
function toOverride(d: DraftRow): ErrorMessageOverride {
  const out: ErrorMessageOverride = {}
  if (d.status !== '') {
    const n = Number(d.status)
    if (Number.isFinite(n)) out.status = n
  }
  if (d.type !== '') out.type = d.type
  if (d.message.trim() !== '') out.message = d.message.trim()
  if (d.retryAfterSecs !== '') {
    const n = Number(d.retryAfterSecs)
    if (Number.isFinite(n)) out.retryAfterSecs = n
  }
  return out
}

// 两个覆盖项是否等价（undefined 与缺失视为同一）。
function sameOverride(a: ErrorMessageOverride, b: ErrorMessageOverride): boolean {
  return (
    (a.status ?? undefined) === (b.status ?? undefined) &&
    (a.type ?? undefined) === (b.type ?? undefined) &&
    (a.message ?? undefined) === (b.message ?? undefined) &&
    (a.retryAfterSecs ?? undefined) === (b.retryAfterSecs ?? undefined)
  )
}

export function ErrorMessagesDialog({
  open,
  onOpenChange,
}: {
  open: boolean
  onOpenChange: (v: boolean) => void
}) {
  const { t } = useTranslation()
  const { data: config, isLoading } = useConfigSnapshot()
  const {
    data: defaults,
    isLoading: defaultsLoading,
    isError: defaultsError,
    refetch: refetchDefaults,
  } = useErrorMessagesDefaults()
  const { mutate: saveConfig, isPending: isSaving } = useUpdateConfig()

  const [drafts, setDrafts] = useState<Record<string, DraftRow>>({})
  const [searchRaw, setSearchRaw] = useState('')
  const [page, setPage] = useState(1)
  // 未保存改动确认关闭的二次确认框。
  const [confirmDiscard, setConfirmDiscard] = useState(false)

  // 配置快照（含保存后的 refetch）到达即重设草稿基线 —— 与设置页 form 同范式：
  // 保存成功后 invalidate → refetch → 草稿重挂到已保存值，脏标记自然清零。
  useEffect(() => {
    if (config) setDrafts(toDrafts(config.errorMessages))
  }, [config])

  // 关闭时清掉草稿/搜索/翻页/确认框（下次打开干净；草稿若不清理，
  // 放弃改动后重开会带着残留编辑，全表渲染下更显眼）。
  useEffect(() => {
    if (!open) {
      setDrafts({})
      setSearchRaw('')
      setPage(1)
      setConfirmDiscard(false)
    }
  }, [open])

  // 搜索词变化回到第一页（与 TraceDetailDialog 过滤归零同思路）。
  useEffect(() => {
    setPage(1)
  }, [searchRaw])

  // 脏 key 集合：草稿与配置基线不一致即脏（「恢复默认」清空字段 → 空对象 vs 非空基线 → 脏）。
  const table = config?.errorMessages
  const dirtyKeys = useMemo(() => {
    const s = new Set<string>()
    for (const [key, d] of Object.entries(drafts)) {
      if (!sameOverride(toOverride(d), table?.[key] ?? {})) s.add(key)
    }
    return s
  }, [drafts, table])

  // 全量 key 表 = 内置默认表 ∪ 配置覆盖（默认表失败时退化为只列已配置 key）。
  // 配置表可能含默认表外的 key（后端只校验命名不校验存在性），一并列出避免配置静默不可见。
  const allKeys = useMemo(() => {
    const keys = new Set<string>()
    for (const k of Object.keys(defaults ?? {})) keys.add(k)
    for (const k of Object.keys(table ?? {})) keys.add(k)
    return [...keys].sort()
  }, [defaults, table])

  // 字段生效值：草稿非空 = 配置覆盖；空 = 内置默认预览（无默认的 key 显示空串）。
  const effStatus = (key: string): string => {
    const d = drafts[key]
    if (d && d.status !== '') return d.status
    const s = defaults?.[key]?.status
    return s != null ? String(s) : ''
  }
  const effType = (key: string): string => {
    const d = drafts[key]
    if (d && d.type !== '') return d.type
    return defaults?.[key]?.type ?? ''
  }
  const effMessage = (key: string): string => {
    const d = drafts[key]
    if (d && d.message !== '') return d.message
    return defaults?.[key]?.message ?? ''
  }
  const effRetryAfter = (key: string): string => {
    const d = drafts[key]
    if (d && d.retryAfterSecs !== '') return d.retryAfterSecs
    const ra = defaults?.[key]?.retryAfterSecs
    return ra != null ? String(ra) : ''
  }

  // 搜索过滤（对全量表生效）：按 key / 生效 status / 生效文案（大小写不敏感）。
  const q = searchRaw.trim().toLowerCase()
  const visibleKeys = useMemo(() => {
    if (!q) return allKeys
    return allKeys.filter((k) => {
      const d = drafts[k] ?? emptyRow()
      const msg =
        (d.message.trim() !== '' ? d.message : defaults?.[k]?.message ?? '').toLowerCase()
      const status =
        d.status !== ''
          ? d.status
          : defaults?.[k]?.status != null
            ? String(defaults[k].status)
            : ''
      return k.toLowerCase().includes(q) || msg.includes(q) || status.includes(q)
    })
  }, [allKeys, drafts, q, defaults])

  // 分页派生：totalPages 兜底 1；page 夹在合法区间（搜索/末页改动后不会越界显示空页）。
  const totalPages = Math.max(1, Math.ceil(visibleKeys.length / PAGE_SIZE))
  const pageClamped = Math.min(page, totalPages)
  const pageKeys = visibleKeys.slice((pageClamped - 1) * PAGE_SIZE, pageClamped * PAGE_SIZE)

  const setField = (key: string, field: keyof DraftRow, value: string) =>
    setDrafts((prev) => ({ ...prev, [key]: { ...(prev[key] ?? emptyRow()), [field]: value } }))

  // 「恢复默认」= 清掉该 key 的配置项（所有字段回落内置默认），保存时以空对象提交。
  const resetKey = (key: string) =>
    setDrafts((prev) => ({ ...prev, [key]: emptyRow() }))

  // 构建 PUT /config 的 errorMessages diff（与后端 per-key merge 语义对应，service.rs）：
  // - 只提交**有改动的 key**，未提交的 key 后端保持不变；
  // - 每条为该 key 的**全量非空字段**（空字段省略 = 该字段回落内置默认）——后端是
  //   整条覆盖（merged.insert），不是字段级 diff：草稿基线含该 key 既有配置字段，
  //   改动后一起提交，否则未动过的旧字段会被覆盖回落默认；
  // - 「恢复默认」的 key 草稿全空 → 提交空对象 {} = 后端删掉该 key 回内置默认。
  const buildDiff = (): Record<string, ErrorMessageOverride> => {
    const diff: Record<string, ErrorMessageOverride> = {}
    for (const [key, d] of Object.entries(drafts)) {
      const entry = toOverride(d)
      if (!sameOverride(entry, table?.[key] ?? {})) diff[key] = entry
    }
    return diff
  }

  const handleSave = () => {
    const diff = buildDiff()
    if (Object.keys(diff).length === 0 || isSaving) return
    saveConfig(
      { errorMessages: diff },
      {
        onSuccess: () => {
          toast.success(t('settingspage.errorMessages.savedToast'), {
            description: t('settingspage.errorMessages.hotReloadHint'),
          })
        },
        onError: (err) => {
          // 后端 400 校验错误逐条显示：extractErrorMessage 取主因，detail 带逐 key 明细。
          const parsed = parseError(err)
          toast.error(extractErrorMessage(err), parsed.detail ? { description: parsed.detail } : undefined)
        },
      }
    )
  }

  // 统一关闭入口：有未保存改动先二次确认（X / Esc / 底部关闭按钮都走这里）。
  const requestClose = () => {
    if (dirtyKeys.size > 0) setConfirmDiscard(true)
    else onOpenChange(false)
  }

  return (
    <>
      <Dialog open={open} onOpenChange={(v) => !v && requestClose()}>
        <DialogContent className="flex max-h-[88vh] w-[min(96vw,900px)] max-w-none flex-col overflow-hidden">
          <DialogHeader>
            <DialogTitle className="flex items-center gap-2">
              <MessageSquareWarning className="h-4 w-4" />
              {t('settingspage.errorMessages.title')}
              <span className="text-xs font-normal text-muted-foreground tabular-nums">
                {t('settingspage.errorMessages.countPage', {
                  total: visibleKeys.length,
                  page: pageClamped,
                  totalPages,
                })}
              </span>
            </DialogTitle>
            <DialogDescription>{t('settingspage.errorMessages.description')}</DialogDescription>
          </DialogHeader>

          {/* 默认表加载失败降级：横幅报错 + 重试，已配置 key 照常渲染（不空白）。 */}
          {defaultsError && (
            <div className="flex items-center gap-2 rounded-md border border-destructive/40 bg-destructive/5 px-3 py-2 text-xs text-destructive">
              <span className="min-w-0 flex-1">{t('settingspage.errorMessages.defaultsLoadFailed')}</span>
              <Button
                variant="outline"
                size="sm"
                className="h-7 shrink-0 px-2 text-xs"
                onClick={() => refetchDefaults()}
              >
                {t('settingspage.common.retry')}
              </Button>
            </div>
          )}

          {/* 搜索栏：按 key / 生效 status / 生效文案过滤（全量表） */}
          <div className="relative min-w-[200px]">
            <Search className="pointer-events-none absolute left-2.5 top-1/2 z-10 h-3.5 w-3.5 -translate-y-1/2 text-muted-foreground" />
            <Input
              id="errmsg-search"
              value={searchRaw}
              onChange={(e) => setSearchRaw(e.target.value)}
              placeholder={t('settingspage.errorMessages.searchPlaceholder')}
              className="h-8 pl-7 pr-7 text-xs"
              aria-label={t('settingspage.errorMessages.searchPlaceholder')}
            />
            {searchRaw && (
              <button
                type="button"
                onClick={() => setSearchRaw('')}
                className="absolute right-1.5 top-1/2 z-10 -translate-y-1/2 text-muted-foreground hover:text-foreground"
                aria-label={t('settingspage.errorMessages.clearSearch')}
              >
                <X className="h-3.5 w-3.5" />
              </button>
            )}
          </div>

          {/* 列头 */}
          <div className="grid grid-cols-[150px_110px_220px_minmax(0,1fr)_90px_auto] items-center gap-2 px-1 text-[11px] text-muted-foreground">
            <span>{t('settingspage.errorMessages.colKey')}</span>
            <span>{t('settingspage.errorMessages.colStatus')}</span>
            <span>{t('settingspage.errorMessages.colType')}</span>
            <span>{t('settingspage.errorMessages.colMessage')}</span>
            <span title={t('settingspage.errorMessages.retryAfterHint')}>
              {t('settingspage.errorMessages.colRetryAfter')}
            </span>
            <span />
          </div>

          {/* 行列表（仅本页 10 条；下拉用原生 datalist，不受滚动容器裁切）。
              加载中 = 配置或默认表任一未就绪（默认表是渲染全量表的前提）。 */}
          <div className="min-h-0 flex-1 space-y-2 overflow-y-auto pr-1">
            {isLoading || (defaultsLoading && !defaults) ? (
              <div className="space-y-2 py-1">
                <div className="px-1 text-[11px] text-muted-foreground">
                  {t('settingspage.errorMessages.loadingDefaults')}
                </div>
                {Array.from({ length: PAGE_SIZE }).map((_, i) => (
                  <Skeleton key={i} className="h-16 w-full" />
                ))}
              </div>
            ) : pageKeys.length === 0 ? (
              <EmptyState
                icon={q ? SearchX : Inbox}
                title={q ? t('settingspage.errorMessages.noResults') : t('settingspage.errorMessages.empty')}
                description={
                  q
                    ? t('settingspage.errorMessages.noResultsHint')
                    : t('settingspage.errorMessages.emptyHint')
                }
              />
            ) : (
              pageKeys.map((key) => {
                const d = drafts[key] ?? emptyRow()
                const dirty = dirtyKeys.has(key)
                return (
                  <div
                    key={key}
                    className={`grid grid-cols-[150px_110px_220px_minmax(0,1fr)_90px_auto] items-start gap-2 rounded-md border px-2 py-2 ${
                      dirty ? 'border-amber-500/50 bg-amber-500/5' : 'border-border/40'
                    }`}
                  >
                    <div className="min-w-0">
                      <div className="truncate font-mono text-xs" title={key}>
                        {key}
                      </div>
                      {dirty && (
                        <Badge variant="outline" className="mt-1 border-amber-500/40 px-1 text-[10px] text-amber-500">
                          {t('settingspage.errorMessages.edited')}
                        </Badge>
                      )}
                    </div>
                    <div className="flex min-w-0 flex-col gap-0.5">
                      <ComboInput
                        value={effStatus(key)}
                        onChange={(v) => setField(key, 'status', v)}
                        options={STATUS_OPTIONS}
                        placeholder={t('settingspage.errorMessages.statusUnset')}
                        className="h-8 px-2 text-xs"
                        aria-label={`${key} ${t('settingspage.errorMessages.colStatus')}`}
                      />
                      {d.status === '' && (
                        <Badge variant="secondary" className="self-start px-1 py-0 text-[9px] font-normal">
                          {t('settingspage.errorMessages.defaultBadge')}
                        </Badge>
                      )}
                    </div>
                    <div className="flex min-w-0 flex-col gap-0.5">
                      <ComboInput
                        value={effType(key)}
                        onChange={(v) => setField(key, 'type', v)}
                        options={TYPE_OPTIONS}
                        placeholder={t('settingspage.errorMessages.typeUnset')}
                        className="h-8 px-2 font-mono text-xs"
                        aria-label={`${key} ${t('settingspage.errorMessages.colType')}`}
                      />
                      {d.type === '' && (
                        <Badge variant="secondary" className="self-start px-1 py-0 text-[9px] font-normal">
                          {t('settingspage.errorMessages.defaultBadge')}
                        </Badge>
                      )}
                    </div>
                    <div className="flex min-w-0 flex-col gap-0.5">
                      <textarea
                        id={`${key}-message`}
                        className="flex min-h-[52px] w-full rounded-md border border-input bg-background px-2 py-1.5 text-xs ring-offset-background placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
                        value={effMessage(key)}
                        onChange={(e) => setField(key, 'message', e.target.value)}
                        placeholder={t('settingspage.errorMessages.messagePh')}
                        spellCheck={false}
                        aria-label={`${key} ${t('settingspage.errorMessages.colMessage')}`}
                      />
                      {d.message === '' && (
                        <Badge variant="secondary" className="self-start px-1 py-0 text-[9px] font-normal">
                          {t('settingspage.errorMessages.defaultBadge')}
                        </Badge>
                      )}
                    </div>
                    <div className="flex min-w-0 flex-col gap-0.5">
                      <Input
                        id={`${key}-retryAfter`}
                        type="number"
                        min={0}
                        max={3600}
                        className="h-8 px-2 text-xs"
                        value={effRetryAfter(key)}
                        onChange={(e) => setField(key, 'retryAfterSecs', e.target.value)}
                        placeholder="—"
                        title={t('settingspage.errorMessages.retryAfterHint')}
                        aria-label={`${key} ${t('settingspage.errorMessages.colRetryAfter')}`}
                      />
                      {d.retryAfterSecs === '' && (
                        <Badge variant="secondary" className="self-start px-1 py-0 text-[9px] font-normal">
                          {t('settingspage.errorMessages.defaultBadge')}
                        </Badge>
                      )}
                    </div>
                    <Button
                      variant="ghost"
                      size="sm"
                      className="h-8 px-2 text-xs"
                      onClick={() => resetKey(key)}
                      title={t('settingspage.errorMessages.resetDefaultTitle')}
                    >
                      <RotateCcw className="mr-1 h-3.5 w-3.5" />
                      {t('settingspage.errorMessages.resetDefault')}
                    </Button>
                  </div>
                )
              })
            )}
          </div>

          {/* 分页条 + 未保存计数 */}
          <div className="flex items-center justify-between gap-2">
            <span className="text-xs text-muted-foreground tabular-nums">
              {dirtyKeys.size > 0
                ? t('settingspage.errorMessages.editedCount', { n: dirtyKeys.size })
                : t('settingspage.errorMessages.noChanges')}
            </span>
            <div className="flex items-center gap-2">
              <Button
                variant="outline"
                size="sm"
                className="h-8 px-2 text-xs"
                disabled={pageClamped <= 1}
                onClick={() => setPage((p) => Math.max(1, p - 1))}
              >
                <ChevronLeft className="mr-1 h-3.5 w-3.5" />
                {t('settingspage.errorMessages.prevPage')}
              </Button>
              <span className="text-xs text-muted-foreground tabular-nums">
                {pageClamped} / {totalPages}
              </span>
              <Button
                variant="outline"
                size="sm"
                className="h-8 px-2 text-xs"
                disabled={pageClamped >= totalPages}
                onClick={() => setPage((p) => Math.min(totalPages, p + 1))}
              >
                {t('settingspage.errorMessages.nextPage')}
                <ChevronRight className="ml-1 h-3.5 w-3.5" />
              </Button>
            </div>
          </div>

          <DialogFooter className="border-t pt-3">
            <span className="mr-auto text-xs text-muted-foreground">
              {t('settingspage.errorMessages.hotReloadHint')}
            </span>
            <Button variant="outline" onClick={requestClose} disabled={isSaving}>
              {t('settingspage.errorMessages.close')}
            </Button>
            <Button onClick={handleSave} disabled={dirtyKeys.size === 0 || isSaving}>
              {isSaving ? t('settingspage.errorMessages.saving') : t('settingspage.errorMessages.save')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <ConfirmDialog
        open={confirmDiscard}
        onOpenChange={(v) => !v && setConfirmDiscard(false)}
        title={t('settingspage.errorMessages.dirtyConfirmTitle')}
        description={t('settingspage.errorMessages.dirtyConfirmDesc', { n: dirtyKeys.size })}
        confirmLabel={t('settingspage.errorMessages.dirtyConfirmDiscard')}
        destructive
        onConfirm={() => {
          setConfirmDiscard(false)
          onOpenChange(false)
        }}
      />
    </>
  )
}
