import { useMemo, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { useQuery, useQueryClient } from '@tanstack/react-query'
import { toast } from 'sonner'
import {
  AlertTriangle,
  Check,
  CopyPlus,
  ListPlus,
  Loader2,
  Pencil,
  Plus,
  RefreshCw,
  Trash2,
  Wifi,
  X,
} from 'lucide-react'

import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Checkbox } from '@/components/ui/checkbox'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import { Skeleton } from '@/components/ui/skeleton'
import { Callout } from '@/components/ui/callout'
import { ConfirmDialog } from '@/components/ui/confirm-dialog'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { ProxyTestButton } from '@/components/proxy-test-button'
import { useCredentials } from '@/hooks/use-credentials'
import {
  bulkImportSocksNodes,
  cloneCredential,
  deleteCredential,
  deleteCredentialsBatch,
  deleteSocksNode,
  getCredentialBalance,
  listSocksNodes,
  setCredentialDisabled,
  setCredentialTag,
  testSocksNode,
  upsertSocksNode,
} from '@/api/credentials'
import {
  previewProxyLines,
  splitProxyTextLines,
  type ProxyLinePreviewItem,
} from '@/lib/proxy-line-parse'
import {
  buildSocksNodeEditPayload,
  hasSocksNodeEdits,
  type SocksNodeEditForm,
} from '@/lib/socks-node-edit'
import type {
  CredentialStatusItem,
  SocksNode,
  SocksNodeBulkImportResponse,
} from '@/types/api'

/** 节点编辑表单的空初值。`password` 永远从空开始 —— 后端不外传密码，空 = 不改。 */
const EMPTY_EDIT_FORM: SocksNodeEditForm = {
  url: '',
  name: '',
  username: '',
  password: '',
  clearPassword: false,
}

/** 单次「再加 N 份」的份数上限。与后端 `MAX_CREDENTIAL_COPIES` 同值：
 *  后端超限会 clamp 而不报错，前端先拦一次是为了给出明确提示而不是静默按 16 建。 */
const MAX_COPIES = 16

/**
 * 这个凭据能不能加分身。
 *
 * 判据与后端 `multi_open_rejection_reason` → `is_api_key_credential()` 对齐：
 * 只有 API Key（`ksk_`）号可以。OAuth 号（social / idc / external_idp）靠 refreshToken
 * 刷新，而 refreshToken 每次刷新都被上游轮换 —— N 份带的是同一个 token，任一份刷新成功
 * 后其余份立刻拿 invalid_grant 被禁用，所以后端直接拒。这里先过滤是为了不让用户点一个
 * 必然失败的操作，**不是**替代后端校验。
 *
 * 大小写不敏感 + 兼容 `apikey` 拼法，与后端 `eq_ignore_ascii_case` 两种写法同口径。
 */
function isCloneable(c: CredentialStatusItem): boolean {
  const m = c.authMethod?.toLowerCase()
  return m === 'api_key' || m === 'apikey'
}

/**
 * 这一份有没有**自己的**出口 IP。
 *
 * 判据与后端 `SameKeyPeer::has_own_exit` 同口径：`proxyUrl` 为空/缺失（回退全局代理）
 * 与 `"direct"`（显式不走代理）都算**没有** —— 这里问的是「实际从哪个 IP 出去」，
 * 两者都是服务器自身那个出口。
 */
function hasOwnExit(c: CredentialStatusItem): boolean {
  const u = c.proxyUrl?.trim()
  return !!u && u.toLowerCase() !== 'direct'
}

/** 一个分身组的聚合视图。 */
interface CloneGroupView {
  /** 分组键（`g:<uuid>` 或回落的 `k:<apiKeyHash>`），仅用作 React key。 */
  group: string
  /** 组内成员，按 cloneSeq 升序；无 seq 的按 id 升序排在后面。 */
  members: CredentialStatusItem[]
  /** 主份（cloneSeq === 1，缺失时取第一个成员）——「查余额」只打它。 */
  primary: CredentialStatusItem
  /** 这一组是否靠 apiKeyHash 回落识别（= 没有 cloneGroup 的老数据）。
   *  UI 据此把序号显示成位置序号而不是 `#?`，并标注「旧数据」。 */
  legacy: boolean
  /** 组内**没有独立出口**的成员 id（升序）。
   *
   * 为什么这值得单独算一份而不是逐行看：同组成员共用**同一个上游账号**，所以
   * 「10 份有独立 IP、1 份没有」不是"少配了一个"，而是那 1 份把整组的账号关联度拉满了。
   * 线上实测就是这个形态（`#776` 无代理 + `#778–787` 各有独立 SOCKS，11 份一个账号），
   * 而它在列表里长得和别的份一样，逐行看根本发现不了。
   *
   * 同一天的实测代价：克隆某号 10 份并全部启用，**15 分钟后**父号连同 10 份分身
   * 全部被 `suspiciousActivityAuto` 禁用。 */
  bareExitIds: number[]
}

/**
 * 按分身组聚合凭据。
 *
 * 分组键有两级，**回落是必需的而不是保险**：
 *
 * 1. `cloneGroup`（权威）—— 持久化随机 UUID，不随 id 复用或 key 轮换漂移。
 * 2. `apiKeyHash`（仅展示期回落）—— 多开功能在 `cloneGroup` 字段之前就已在生产
 *    使用，那批分身**没有** cloneGroup。实测线上回收站 349 条里有 23 个组 / 65 个
 *    凭据属于这种老数据（其中一组 9 份），带 cloneGroup 的是 0 个。只认 cloneGroup
 *    的话，从回收站恢复任何一个老分身都在本页面上**看不见**，一个全是老分身的池
 *    会完全空白。
 *
 * 回落只用于**分组呈现**，不写回任何持久状态：按 key 分组在 key 轮换时会裂组
 * （设计评审 BLOCKER 8），所以它不能进权威路径；但对「本来就没有组」的老数据，
 * 按 key 分组严格优于什么都不显示。
 *
 * 单份的组（既无 cloneGroup 又只有一份同 key）会被剔除：单开号不是分身，
 * 否则一个只有单号的池会显示 N 个「1 份的组」，纯噪音。
 */
function groupClones(items: CredentialStatusItem[]): CloneGroupView[] {
  // 第一遍：建立 apiKeyHash → cloneGroup 的映射。
  //
  // 为什么需要这一遍：给一个**早于 cloneGroup 字段**入池的号追加分身时，后端只把新
  // UUID 写到新建的那几份上，父号仍然没有 cloneGroup。若直接按「有 group 用 group、
  // 没有就用 key」分组，同一个账号会裂成两组（父号一组、新分身一组），
  // 而用户看到的是同一个 key —— 面板上像是多了一个账号。
  //
  // 同 key 必然同账号（key 就是账号凭证），所以只要该 key 下**任何**一份有 group，
  // 就把整个 key 的成员都归到那个 group 上。
  //
  // ⏳ 这一遍**什么时候可以退役**（2026-08-06 起后端已回填，见下）：
  // 后端 `add_credential_with_intent` 现在会按 key 找出同账号成员，把缺 `cloneGroup`
  // 的那些一并回填成同一个组标识 —— 所以**新产生的**数据不再需要本回落。
  // 退役判据：池里（含回收站，因为恢复会把老条目放回来）不再存在
  // 「有同 key 兄弟却自己没有 cloneGroup」的凭据。届时删掉本遍与 `groupOfKey`，
  // 分组只认 `cloneGroup` 即可。
  // ⚠️ 在那之前不能删：线上仍有一批历史数据一个组标识都没有（实测回收站 349 条里
  // 23 个组 / 65 个凭据属于老数据，其中一组 9 份），而回填只在「给它加分身」时才发生 ——
  // 从没被追加过分身的老组永远等不到回填。现在删等于让那些组的分组关系当场丢失。
  const groupOfKey = new Map<string, string>()
  for (const it of items) {
    if (it.cloneGroup && it.apiKeyHash && !groupOfKey.has(it.apiKeyHash)) {
      groupOfKey.set(it.apiKeyHash, it.cloneGroup)
    }
  }

  const byGroup = new Map<string, CredentialStatusItem[]>()
  for (const it of items) {
    // 前缀区分两种键，避免一个 UUID 与一个 sha256 十六进制串理论上相等时并组。
    const adopted = it.cloneGroup ?? (it.apiKeyHash ? groupOfKey.get(it.apiKeyHash) : undefined)
    const key = adopted ? `g:${adopted}` : it.apiKeyHash ? `k:${it.apiKeyHash}` : null
    if (!key) continue
    const arr = byGroup.get(key) ?? []
    arr.push(it)
    byGroup.set(key, arr)
  }
  // 剔除「按 key 回落且只有一份」的组 —— 那是单开号，不是分身。
  // 有 cloneGroup 的即使只剩一份也保留：那是一组分身被删到只剩主份，仍该可见。
  for (const [key, members] of [...byGroup]) {
    if (key.startsWith('k:') && members.length < 2) byGroup.delete(key)
  }
  const out: CloneGroupView[] = []
  byGroup.forEach((members, group) => {
    // 有 seq 的按 seq；都没有 seq（老数据）时按 id —— 必须有稳定次序，
    // 否则「位置序号」每次轮询都可能变，用户看到序号乱跳。
    members.sort((a, b) => {
      const sa = a.cloneSeq ?? Number.MAX_SAFE_INTEGER
      const sb = b.cloneSeq ?? Number.MAX_SAFE_INTEGER
      return sa !== sb ? sa - sb : a.id - b.id
    })
    const primary = members.find((m) => m.cloneSeq === 1) ?? members[0]
    // legacy 的判据是「**有成员没有 cloneSeq**」，不是「分组键是 k: 前缀」：
    // 采纳（groupOfKey）之后，一个 `g:` 组里也可能混着一个早于本字段入池的父号，
    // 那一份仍然只能按位置编号，所以角标该亮。按前缀判会漏掉这种混合组。
    const legacy = members.some((m) => m.cloneSeq === undefined)
    const bareExitIds = members.filter((m) => !hasOwnExit(m)).map((m) => m.id)
    out.push({ group, members, primary, legacy, bareExitIds })
  })
  // 组间按主份 id 升序，保证渲染顺序稳定（Map 迭代序虽稳定但依赖插入序）。
  out.sort((a, b) => a.primary.id - b.primary.id)
  return out
}

function fmtAgo(unixSecs: number, t: (k: string, o?: Record<string, unknown>) => string): string {
  if (!unixSecs) return t('clones.node.neverTested')
  const mins = Math.max(0, Math.floor(Date.now() / 1000 - unixSecs) / 60)
  if (mins < 1) return t('clones.node.justNow')
  if (mins < 60) return t('clones.node.minsAgo', { n: Math.floor(mins) })
  return t('clones.node.hoursAgo', { n: Math.floor(mins / 60) })
}

/** 代理节点表：候选池的增删改与测活。 */
function SocksNodesPanel() {
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  const { data, isLoading } = useQuery({ queryKey: ['socks-nodes'], queryFn: listSocksNodes })
  const nodes = useMemo(() => data?.nodes ?? [], [data])

  /** 新建节点只有**一个**输入框：整条链接直接发给后端。
   *
   * 为什么不再有 name/username/password 四个字段：节点商下发的是
   * `socks://base64(user:pass)@host:port#name`，后端 `parse_proxy_link` 会拆出账密与
   * `#name`、只把干净地址写进 url。让用户自己拆等于逼他手工 base64 解码，
   * 且最容易的错法是把整个 base64 串当用户名填进去 —— 那个失败长得像「节点不通」。
   * 显式账密仍然可用（内嵌在 URL 里），所以少三个框不丢能力。 */
  const [draftUrl, setDraftUrl] = useState('')
  const [busy, setBusy] = useState(false)
  const [testingId, setTestingId] = useState<number | null>(null)
  const [pendingDelete, setPendingDelete] = useState<SocksNode | null>(null)
  /** 正在编辑的节点 id + 表单草稿（同一时刻只允许编辑一个，避免多行草稿互相覆盖）。
   *
   * 草稿是**整表单**而不只是地址：改名 / 改用户名 / 换密码此前都做不到 —— 名称只能
   * 靠分享链接的 `#fragment` 带进来，密码只能连同整条链接一起重贴。
   * 三态提交规则（哪些键该发、哪些必须省略）在 `lib/socks-node-edit`。 */
  const [editingId, setEditingId] = useState<number | null>(null)
  const [editForm, setEditForm] = useState<SocksNodeEditForm>(EMPTY_EDIT_FORM)
  const [savingEdit, setSavingEdit] = useState(false)
  /** 批量导入：整段文本 + 是否直接启用（默认关）+ 上一次结果。 */
  const [bulkText, setBulkText] = useState('')
  const [bulkEnabled, setBulkEnabled] = useState(false)
  const [bulkBusy, setBulkBusy] = useState(false)
  const [bulkResult, setBulkResult] = useState<SocksNodeBulkImportResponse | null>(null)
  /** 勾选的**覆盖**项（键 = 行号）。不在表里的行按默认值走（`status === 'ok'` 才默认勾）。
   *
   * 为什么用「覆盖 + 默认」而不是直接存一个选中集合：粘贴内容一变，行号的含义就全变了，
   * 存集合就得用 effect 去同步，而 effect 跑在渲染之后 ⇒ 有一帧勾选是错的。
   * 覆盖表在 textarea 的 onChange 里清空即可，无 effect。 */
  const [bulkSel, setBulkSel] = useState<Record<number, boolean>>({})

  /** 粘贴后**立即本地预览**（不打后端）。
   *
   * 🔴 这份解析与后端 `src/http_client.rs` 的判据是同一套，但**后端才是权威** ——
   * 见 `lib/proxy-line-parse.ts` 文件头。预览只决定「界面上怎么显示、默认勾哪几行」，
   * 导入时发的仍是用户勾选行的**原文**，由后端重新解析并落库。
   * 池内重复要传 `nodes` 的 url 进去，否则「已在池中」这一类在导入前看不出来。 */
  const bulkPreview = useMemo(
    () => previewProxyLines(bulkText, nodes.map((n) => n.url)),
    [bulkText, nodes],
  )
  /** 原文行（**未脱敏**）—— 导入时按行号从这里取。预览里的 `raw` 是脱敏过的，不能发。 */
  const bulkRawLines = useMemo(() => splitProxyTextLines(bulkText), [bulkText])
  const isBulkLineSelected = (it: ProxyLinePreviewItem) =>
    bulkSel[it.lineno] ?? it.status === 'ok'
  const bulkSelected = bulkPreview.items.filter(isBulkLineSelected)

  const invalidate = () => queryClient.invalidateQueries({ queryKey: ['socks-nodes'] })

  const add = async () => {
    const url = draftUrl.trim()
    if (!url) {
      toast.error(t('clones.node.urlRequired'))
      return
    }
    setBusy(true)
    try {
      // 只发 url：name 由后端从 `#fragment` 取，账密由 parse_proxy_link 拆。
      await upsertSocksNode({ url })
      toast.success(t('clones.node.added'))
      setDraftUrl('')
      invalidate()
    } catch (e) {
      toast.error(e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  const toggleEnabled = async (n: SocksNode, enabled: boolean) => {
    try {
      // ⚠️ 只发 id/url/enabled，**不带 password 键** —— 省略 = 不改密码。
      // 若这里为了「补齐字段」回填空串，切一次开关就会把密码清空，
      // 已绑该节点的分身全部因代理认证失败掉线。
      await upsertSocksNode({ id: n.id, url: n.url, name: n.name, username: n.username, enabled })
      invalidate()
    } catch (e) {
      toast.error(e instanceof Error ? e.message : String(e))
    }
  }

  const startEdit = (n: SocksNode) => {
    setEditingId(n.id)
    // 密码栏刻意留空：后端恒不外传密码，回填只能填出空串，而空串在三态里是"清空"。
    setEditForm({
      url: n.url,
      name: n.name,
      username: n.username ?? '',
      password: '',
      clearPassword: false,
    })
  }

  const cancelEdit = () => {
    setEditingId(null)
    setEditForm(EMPTY_EDIT_FORM)
  }

  /** 编辑表单的单字段更新（保留其余字段，不整体替换）。 */
  const patchEdit = (patch: Partial<SocksNodeEditForm>) =>
    setEditForm((prev) => ({ ...prev, ...patch }))

  /** 保存改动。请求体由 `buildSocksNodeEditPayload` 构造 —— **只发用户真改过的键**，
   *  哪些必须省略、为什么省略（抹密码 / 吃掉分享链接自带账密）见该函数文档。 */
  const saveEdit = async (n: SocksNode) => {
    if (!editForm.url.trim()) {
      toast.error(t('clones.node.urlRequired'))
      return
    }
    // 一个字段都没改就直接收起，不白打一次后端（也避免无意义的持久化写盘）。
    if (!hasSocksNodeEdits(n, editForm)) {
      cancelEdit()
      return
    }
    setSavingEdit(true)
    try {
      await upsertSocksNode(buildSocksNodeEditPayload(n, editForm))
      toast.success(t('clones.node.saved'))
      cancelEdit()
      invalidate()
    } catch (e) {
      toast.error(e instanceof Error ? e.message : String(e))
    } finally {
      setSavingEdit(false)
    }
  }

  /** 整段粘贴批量导入。四个计数都展示：只报 added 会让「一个都没进」看起来像失败，
   *  而真实原因通常是 duplicate（已存在、未覆盖）或 overCapacity（超上限）。
   *
   * 只发**勾选行的原文**：预览里的 `raw` 已脱敏（密码换成 `***`），发它等于把节点
   * 的密码写成字面量 `***` 导进池子 —— 那些节点会全部认证失败，且表现为「节点不通」。 */
  const runBulkImport = async () => {
    if (!bulkText.trim()) {
      toast.error(t('clones.node.bulkTextRequired'))
      return
    }
    const text = bulkSelected
      .map((it) => bulkRawLines[it.lineno - 1])
      .filter((l): l is string => typeof l === 'string')
      .join('\n')
    if (!text.trim()) {
      toast.error(t('clones.node.bulkNoneSelected'))
      return
    }
    setBulkBusy(true)
    try {
      const r = await bulkImportSocksNodes(text, bulkEnabled)
      setBulkResult(r)
      // 直接用服务端文案：它会如实说明去重/超上限/默认未启用，
      // 这些细节被前端的通用「导入成功」盖掉正是最容易误判的地方。
      if (r.added > 0) toast.success(r.message)
      else toast.error(r.message)
      // 清空 textarea 与勾选覆盖：清了文本再留覆盖表，下次粘贴会带着上次的行号勾选。
      if (r.added > 0) {
        setBulkText('')
        setBulkSel({})
      }
      invalidate()
    } catch (e) {
      toast.error(e instanceof Error ? e.message : String(e))
    } finally {
      setBulkBusy(false)
    }
  }

  const runTest = async (n: SocksNode) => {
    setTestingId(n.id)
    try {
      const r = await testSocksNode(n.id)
      if (r.ok) toast.success(t('clones.node.testOk', { ms: r.latencyMs, ip: r.exitIp ?? '?' }))
      else toast.error(t('clones.node.testFail', { err: r.error ?? '' }))
      invalidate()
    } catch (e) {
      toast.error(e instanceof Error ? e.message : String(e))
    } finally {
      setTestingId(null)
    }
  }

  const confirmDelete = async () => {
    if (!pendingDelete) return
    try {
      await deleteSocksNode(pendingDelete.id)
      toast.success(t('clones.node.deleted'))
      invalidate()
    } catch (e) {
      toast.error(e instanceof Error ? e.message : String(e))
    } finally {
      setPendingDelete(null)
    }
  }

  return (
    <div className="space-y-3">
      <div className="text-xs text-muted-foreground">{t('clones.node.hint')}</div>
      <div className="rounded-md bg-muted/50 p-2 text-xs text-muted-foreground">
        {t('clones.node.consumedBy')}
      </div>

      {/* 单条添加：与「凭据代理设置」同一形态 —— 一个输入框 + [测活] + [添加]。 */}
      <div className="space-y-1.5">
        <p className="text-xs text-muted-foreground">{t('clones.node.urlHint')}</p>
        <div className="flex items-center gap-2">
          <Input
            className="h-9 font-mono text-xs"
            placeholder={t('clones.node.urlPlaceholder')}
            value={draftUrl}
            onChange={(e) => setDraftUrl(e.target.value)}
            aria-label={t('clones.node.urlAria')}
          />
          <ProxyTestButton proxyUrl={draftUrl} className="h-9 shrink-0" />
          <Button onClick={add} disabled={busy} size="sm" className="h-9 shrink-0">
            {busy ? <Loader2 className="h-4 w-4 animate-spin" /> : <Plus className="h-4 w-4" />}
            <span className="ml-1">{t('clones.node.add')}</span>
          </Button>
        </div>
        {/* 分享链接的账密在后端才被拆出来，而 /proxy/test 只做百分号解码 ——
            所以未保存的分享链接草稿测活会因认证失败而显示"不通"。保存后用行内的
            测活按钮（走 /socks/nodes/{id}/test，用已拆好的账密）才是准的。 */}
        <p className="text-xs text-muted-foreground">{t('clones.node.testDraftCaveat')}</p>
      </div>

      {/* 批量导入：整段粘贴节点商文档，逐行解析。 */}
      <div className="space-y-1.5 rounded-md border p-2">
        <div className="flex flex-wrap items-center gap-2">
          <span className="text-sm font-medium">{t('clones.node.bulkTitle')}</span>
          <div className="ml-auto flex items-center gap-2">
            <span className="text-xs text-muted-foreground">{t('clones.node.bulkEnabled')}</span>
            <Switch checked={bulkEnabled} onCheckedChange={setBulkEnabled} />
            <Button
              onClick={runBulkImport}
              disabled={bulkBusy || (bulkText.trim().length > 0 && bulkSelected.length === 0)}
              size="sm"
              variant="outline"
            >
              {bulkBusy ? (
                <Loader2 className="h-4 w-4 animate-spin" />
              ) : (
                <ListPlus className="h-4 w-4" />
              )}
              <span className="ml-1">
                {bulkPreview.items.length > 0
                  ? t('clones.node.bulkImportSelected', { n: bulkSelected.length })
                  : t('clones.node.bulkImport')}
              </span>
            </Button>
          </div>
        </div>
        <p className="text-xs text-muted-foreground">{t('clones.node.bulkHint')}</p>
        <textarea
          className="flex min-h-[110px] w-full rounded-md border border-input bg-background px-3 py-2 font-mono text-xs ring-offset-background placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 disabled:cursor-not-allowed disabled:opacity-50"
          placeholder={t('clones.node.bulkPlaceholder')}
          value={bulkText}
          onChange={(e) => {
            setBulkText(e.target.value)
            // 行号的含义随文本变化，上次的勾选覆盖必须一起丢掉（见 bulkSel 注释）。
            setBulkSel({})
          }}
          disabled={bulkBusy}
          aria-label={t('clones.node.bulkAria')}
        />

        {/* 本地预览：粘贴即出，不打后端。可勾选，重复项默认不勾。 */}
        {bulkPreview.items.length > 0 && (
          <div className="space-y-1 rounded-md bg-muted/40 p-2">
            <div className="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs">
              <span className="font-medium">{t('clones.node.previewTitle')}</span>
              <span className="text-muted-foreground">
                {t('clones.node.previewSummary', {
                  ok: bulkPreview.okCount,
                  duplicate: bulkPreview.duplicateCount,
                  invalid: bulkPreview.invalidCount,
                })}
              </span>
              {bulkPreview.skipped > 0 && (
                <span className="text-muted-foreground">
                  {t('clones.node.previewSkipped', { n: bulkPreview.skipped })}
                </span>
              )}
            </div>
            {/* 后端才是权威：预览只是让用户先看清，勾选行的原文仍会重新发给后端解析。 */}
            <p className="text-[11px] text-muted-foreground">{t('clones.node.previewAuthority')}</p>
            <ul className="max-h-56 space-y-0.5 overflow-y-auto">
              {bulkPreview.items.map((it) => {
                const selected = isBulkLineSelected(it)
                return (
                  <li key={it.lineno} className="flex items-start gap-2 py-0.5 text-xs">
                    <Checkbox
                      className="mt-0.5 shrink-0"
                      checked={selected}
                      disabled={bulkBusy}
                      onCheckedChange={(v) =>
                        setBulkSel((prev) => ({ ...prev, [it.lineno]: v === true }))
                      }
                      aria-label={t('clones.node.previewRowAria', { n: it.lineno })}
                    />
                    <span className="w-8 shrink-0 text-right font-mono text-muted-foreground">
                      {it.lineno}
                    </span>
                    {it.status === 'ok' ? (
                      <Check className="mt-0.5 h-3.5 w-3.5 shrink-0 text-emerald-500" />
                    ) : it.status === 'duplicate' ? (
                      <CopyPlus className="mt-0.5 h-3.5 w-3.5 shrink-0 text-amber-500" />
                    ) : (
                      <X className="mt-0.5 h-3.5 w-3.5 shrink-0 text-destructive" />
                    )}
                    <div className="min-w-0 flex-1 space-y-0.5">
                      {it.address ? (
                        <div className="break-all font-mono">
                          {it.address}
                          {/* 只显示用户名，密码永不进 DOM。 */}
                          {it.username && (
                            <span className="ml-2 text-muted-foreground">
                              {t('clones.node.previewUser', { user: it.username })}
                            </span>
                          )}
                        </div>
                      ) : (
                        // 无法识别的行显示原文：形状本身就是诊断信息，用户能立刻看出
                        // 是自己格式写错了还是数据脏了。（密码已脱敏。）
                        <div className="break-all font-mono text-muted-foreground">{it.raw}</div>
                      )}
                      <div className="text-[11px] text-muted-foreground">
                        {it.status === 'duplicate'
                          ? t(
                              it.dupOf === 'pool'
                                ? 'clones.node.previewDupPool'
                                : 'clones.node.previewDupPaste',
                            )
                          : it.issue
                            ? t(`clones.node.issue.${it.issue}`)
                            : null}
                      </div>
                    </div>
                  </li>
                )
              })}
            </ul>
          </div>
        )}

        {bulkResult && (
          <div className="space-y-1">
            <div className="text-xs text-muted-foreground">
              {t('clones.node.bulkResult', {
                added: bulkResult.added,
                duplicate: bulkResult.duplicate,
                skipped: bulkResult.skipped,
                overCapacity: bulkResult.overCapacity,
              })}
            </div>
            {/* 后端逐行结论里**不是 ok** 的那些。只列这些是因为它们带着前端无从得知的
                原因：address_rejected（SSRF 策略拦下）与 over_capacity（节点数上限）。
                `items` 可能缺失（旧后端），故先判空。 */}
            {(bulkResult.items ?? []).filter((i) => i.status !== 'ok').length > 0 && (
              <ul className="space-y-0.5 text-xs">
                {(bulkResult.items ?? [])
                  .filter((i) => i.status !== 'ok')
                  .map((i) => (
                    <li key={i.lineno} className="flex items-start gap-2">
                      <span className="w-8 shrink-0 text-right font-mono text-muted-foreground">
                        {i.lineno}
                      </span>
                      <span className="min-w-0 flex-1 break-all font-mono text-muted-foreground">
                        {i.address ?? i.raw}
                      </span>
                      <span className="shrink-0 text-muted-foreground">
                        {i.reason ? t(`clones.node.issue.${i.reason}`) : i.status}
                      </span>
                    </li>
                  ))}
              </ul>
            )}
          </div>
        )}
      </div>

      {isLoading ? (
        <Skeleton className="h-16 w-full" />
      ) : nodes.length === 0 ? (
        <div className="rounded-md border border-dashed p-4 text-center text-sm text-muted-foreground">
          {t('clones.node.empty')}
        </div>
      ) : (
        <div className="space-y-2">
          {nodes.map((n) =>
            editingId === n.id ? (
              // 编辑态：地址行沿用新建的形态（输入框 + [测活] + [保存] + [取消]），
              // 下面再补名称 / 用户名 / 新密码三个字段 —— 此前这三项在面板上都改不了。
              <div key={n.id} className="space-y-1.5 rounded-md border p-2">
                <div className="flex items-center gap-2">
                  <Input
                    className="h-9 font-mono text-xs"
                    value={editForm.url}
                    onChange={(e) => patchEdit({ url: e.target.value })}
                    placeholder={t('clones.node.urlPlaceholder')}
                    aria-label={t('clones.node.urlAria')}
                  />
                  <ProxyTestButton proxyUrl={editForm.url} className="h-9 shrink-0" />
                  <Button
                    size="sm"
                    className="h-9 shrink-0"
                    onClick={() => saveEdit(n)}
                    disabled={savingEdit}
                  >
                    {savingEdit ? (
                      <Loader2 className="h-4 w-4 animate-spin" />
                    ) : (
                      <Check className="h-4 w-4" />
                    )}
                    <span className="ml-1">{t('clones.node.save')}</span>
                  </Button>
                  <Button
                    variant="outline"
                    size="sm"
                    className="h-9 shrink-0"
                    onClick={cancelEdit}
                    disabled={savingEdit}
                  >
                    <X className="h-4 w-4" />
                  </Button>
                </div>
                <div className="grid gap-1.5 sm:grid-cols-3">
                  <Input
                    className="h-9 text-xs"
                    value={editForm.name}
                    onChange={(e) => patchEdit({ name: e.target.value })}
                    placeholder={t('clones.node.namePlaceholder')}
                    aria-label={t('clones.node.nameAria')}
                    disabled={savingEdit}
                  />
                  <Input
                    className="h-9 font-mono text-xs"
                    value={editForm.username}
                    onChange={(e) => patchEdit({ username: e.target.value })}
                    placeholder={t('clones.node.usernamePlaceholder')}
                    aria-label={t('clones.node.usernameAria')}
                    disabled={savingEdit}
                  />
                  {/* 密码框**永远从空开始**：后端不外传密码，留空 = 不改。
                      要清空得显式勾下面那个框（空串才是"清空"，见 socks-node-edit）。 */}
                  <Input
                    type="password"
                    className="h-9 font-mono text-xs"
                    value={editForm.password}
                    onChange={(e) => patchEdit({ password: e.target.value })}
                    placeholder={t('clones.node.passwordEditPlaceholder')}
                    aria-label={t('clones.node.passwordAria')}
                    disabled={savingEdit || editForm.clearPassword}
                  />
                </div>
                {/* 「清除密码」只在该节点当前有密码时才有意义，否则不显示（少一个能点错的框）。 */}
                {n.hasPassword && (
                  <label className="flex items-center gap-2 text-xs text-muted-foreground">
                    <Checkbox
                      checked={editForm.clearPassword}
                      disabled={savingEdit}
                      onCheckedChange={(v) =>
                        // 勾上就把已键入的新密码一并清掉：两者互斥，留着会让人以为新密码生效了。
                        patchEdit({ clearPassword: v === true, password: v === true ? '' : editForm.password })
                      }
                      aria-label={t('clones.node.clearPassword')}
                    />
                    {t('clones.node.clearPassword')}
                  </label>
                )}
                <p className="text-xs text-muted-foreground">{t('clones.node.editHint')}</p>
                <p className="text-xs text-muted-foreground">{t('clones.node.editPassHint')}</p>
              </div>
            ) : (
              <div key={n.id} className="flex flex-wrap items-center gap-2 rounded-md border p-2 text-sm">
                <span className="font-medium">{n.label}</span>
                <span className="font-mono text-xs text-muted-foreground">{n.url}</span>
                {n.hasPassword && <Badge variant="outline">{t('clones.node.hasPassword')}</Badge>}
                {n.lastTest && (
                  <Badge variant={n.lastTest.ok ? 'default' : 'destructive'}>
                    {n.lastTest.ok
                      ? t('clones.node.okBadge', { ms: n.lastTest.latencyMs })
                      : t('clones.node.failBadge')}
                  </Badge>
                )}
                <span className="text-xs text-muted-foreground">
                  {fmtAgo(n.lastTest?.testedAt ?? 0, t)}
                </span>
                <div className="ml-auto flex items-center gap-2">
                  <Switch checked={n.enabled} onCheckedChange={(v) => toggleEnabled(n, v)} />
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() => startEdit(n)}
                    title={t('clones.node.edit')}
                    aria-label={t('clones.node.edit')}
                  >
                    <Pencil className="h-3.5 w-3.5" />
                  </Button>
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() => runTest(n)}
                    disabled={testingId === n.id}
                    title={t('clones.node.test')}
                    aria-label={t('clones.node.test')}
                  >
                    {testingId === n.id ? (
                      <Loader2 className="h-3.5 w-3.5 animate-spin" />
                    ) : (
                      <Wifi className="h-3.5 w-3.5" />
                    )}
                  </Button>
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() => setPendingDelete(n)}
                    title={t('clones.node.delete')}
                    aria-label={t('clones.node.delete')}
                  >
                    <Trash2 className="h-3.5 w-3.5" />
                  </Button>
                </div>
              </div>
            ),
          )}
        </div>
      )}

      <ConfirmDialog
        open={!!pendingDelete}
        onOpenChange={(v) => !v && setPendingDelete(null)}
        title={t('clones.node.deleteTitle')}
        description={t('clones.node.deleteDesc', { label: pendingDelete?.label ?? '' })}
        onConfirm={confirmDelete}
      />
    </div>
  )
}

/** 分身组列表：看主凭据 / 查余额 / 改标签。 */
function CloneGroupsPanel() {
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  const { data, isLoading } = useCredentials()
  const groups = useMemo(() => groupClones(data?.credentials ?? []), [data])

  const [balances, setBalances] = useState<Record<number, string>>({})
  const [loadingBalance, setLoadingBalance] = useState<number | null>(null)
  const [tagDraft, setTagDraft] = useState<Record<number, string>>({})
  /** 每组「再加 N 份」的份数草稿（key = 主份 id）。默认 1 = 最常见的「再加一份」。 */
  const [addDraft, setAddDraft] = useState<Record<number, string>>({})
  /** 待确认的扩容操作。确认步骤是必须的：分身共享同一个上游账号配额，
   *  多加份数**不增加**总容量，用户必须先看到这句再点确认。 */
  const [pendingAdd, setPendingAdd] = useState<{ id: number; copies: number } | null>(null)
  const [addBusy, setAddBusy] = useState(false)
  /**
   * 「生成分身时是否全部默认启用」。**默认关**。
   *
   * 关是刻意的：刚建出来的分身还没绑出口、没验活，直接入池就参与调度等于把未经验证
   * 的号推上热路径；而同组分身共享同一个上游账号配额，多几份立刻参与调度只会更快撞 429。
   * 这个开关同时作用于本页两条生成路径（每组的「再加 N 份」与「选凭据生成」），
   * 一处状态两处生效，避免两个入口各有一份默认值而分叉。
   */
  const [cloneEnabled, setCloneEnabled] = useState(false)
  /** 「选凭据生成」对话框：选中的凭据 id + 份数草稿。 */
  const [pickerOpen, setPickerOpen] = useState(false)
  const [pickedId, setPickedId] = useState<number | null>(null)
  const [pickerCopies, setPickerCopies] = useState('1')
  /** 待确认删除的分身成员。删单份**不动**同组其它成员，也不动节点池
   *  （节点是候选池、凭据的 proxy_* 是绑定结果，两者独立）。 */
  const [pendingMemberDelete, setPendingMemberDelete] = useState<{
    member: CredentialStatusItem
    /** 是不是这一组的主份 —— 确认文案要额外警告。 */
    isPrimary: boolean
    /** 组内剩余份数（含本份），用于文案。 */
    groupSize: number
  } | null>(null)
  const [deleteBusy, setDeleteBusy] = useState(false)
  /**
   * 待确认删除的**整组**。
   *
   * 存 id 快照而不是存 CloneGroupView：本页每 30s 轮询一次凭据，对话框开着时 groups
   * 会被重算。存快照保证「确认时删的正是弹窗里数出来的那几份」，而不是确认瞬间的组成员
   * （否则轮询插进来一份新分身，用户看到「删 3 份」却删掉了 4 份）。
   */
  const [pendingGroupDelete, setPendingGroupDelete] = useState<{
    /** 组的主份 id —— 仅用于文案定位是哪一组。 */
    primaryId: number
    /** 本次要删的成员 id 快照（含主份）。 */
    ids: number[]
  } | null>(null)
  const [groupDeleteBusy, setGroupDeleteBusy] = useState(false)

  /** 池中可加分身的凭据（供「选凭据生成」列表用）。 */
  const cloneable = useMemo(
    () => (data?.credentials ?? []).filter(isCloneable),
    [data],
  )
  /** 不可加分身的条数 —— 列表里不列它们，但要告诉用户「少了几个不是 bug」。 */
  const notCloneableCount = (data?.credentials?.length ?? 0) - cloneable.length

  /** 查主凭据余额。**只打主份** —— 同组共享同一个上游账号配额，
   *  逐份查是 N 次 web_portal 往返（上游探测，会加重风控）且必然得到同一个数。 */
  const checkBalance = async (id: number) => {
    setLoadingBalance(id)
    try {
      const b = await getCredentialBalance(id)
      setBalances((prev) => ({
        ...prev,
        [id]: t('clones.group.balanceValue', {
          remaining: Math.round(b.remaining),
          limit: Math.round(b.effectiveLimit ?? 0),
        }),
      }))
    } catch (e) {
      toast.error(e instanceof Error ? e.message : String(e))
    } finally {
      setLoadingBalance(null)
    }
  }

  const saveTag = async (id: number) => {
    const v = tagDraft[id] ?? ''
    try {
      await setCredentialTag(id, v.trim() ? v.trim() : null)
      toast.success(t('clones.group.tagSaved'))
      setTagDraft((prev) => {
        const next = { ...prev }
        delete next[id]
        return next
      })
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    } catch (e) {
      toast.error(e instanceof Error ? e.message : String(e))
    }
  }

  /** 校验份数并进入确认步骤。上限与后端 MAX_CREDENTIAL_COPIES 一致（超限后端会 clamp，
   *  但前端先拦一次能给出更明确的提示，而不是静默按 16 建）。 */
  const askAdd = (primaryId: number) => {
    const n = Number.parseInt(addDraft[primaryId] ?? '1', 10)
    if (!Number.isFinite(n) || n < 1 || n > MAX_COPIES) {
      toast.error(t('clones.group.addCopiesInvalid', { max: MAX_COPIES }))
      return
    }
    setPendingAdd({ id: primaryId, copies: n })
  }

  /** 「选凭据生成」：从池中已有凭据里选一个 + 份数 → 走同一个确认步骤。
   *
   * 与每组的「再加 N 份」共用 `pendingAdd` / `confirmAdd`，不是第二条提交路径 ——
   * 否则配额警告、份数上限、enabled 默认值三处都得写两遍。 */
  const submitPicker = () => {
    if (pickedId === null) {
      toast.error(t('clones.picker.pickFirst'))
      return
    }
    const n = Number.parseInt(pickerCopies, 10)
    if (!Number.isFinite(n) || n < 1 || n > MAX_COPIES) {
      toast.error(t('clones.group.addCopiesInvalid', { max: MAX_COPIES }))
      return
    }
    setPickerOpen(false)
    setPendingAdd({ id: pickedId, copies: n })
  }

  /**
   * 删掉一份分身。
   *
   * 两步是必需的：后端 `DELETE /credentials/{id}` 在 `force=false` 下有「必须先禁用」
   * 这道门（误删护栏），而分身通常正处启用态。所以先 `setCredentialDisabled(id, true)`
   * 再 DELETE —— 而不是靠匹配错误文案去判断，那种判据会随后端文案改动而静默失效。
   *
   * 已是禁用态时跳过第一步（少一次往返，且避免对回收站里的号重复写状态）。
   *
   * 删除是**软删**（进回收站可恢复）。⚠️ 恢复分身必须带 `force`：分身与主份必然同 key，
   * 不带会被「凭据已存在」挡住 —— 这条已在 `restoreCredential` 里处理。
   */
  const confirmMemberDelete = async () => {
    if (!pendingMemberDelete) return
    const { member } = pendingMemberDelete
    setDeleteBusy(true)
    try {
      if (!member.disabled) await setCredentialDisabled(member.id, true)
      const r = await deleteCredential(member.id)
      toast.success(r.message)
      setPendingMemberDelete(null)
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
      // 软删进回收站 → 回收站列表同样失效，否则设置页看到的是不含本条的旧缓存。
      queryClient.invalidateQueries({ queryKey: ['trash'] })
    } catch (e) {
      toast.error(e instanceof Error ? e.message : String(e))
    } finally {
      setDeleteBusy(false)
    }
  }

  /**
   * 删掉整组分身。
   *
   * 走批量端点 + `force=true`，而不是对每份复用 `confirmMemberDelete` 的两步：
   * 后端「必须先禁用」那道门会让 N 份变成 2N 次往返（每份先 PATCH 禁用再 DELETE），
   * batch + force 是 1 次。`force` 只跳过那道误删护栏，**不**跳过回收站 ——
   * 仍是软删，可从「设置 → 回收站」恢复。
   *
   * ⚠️ 部分失败仍是 HTTP 200（resolve），所以必须看 `failed` / `results[].ok`：
   * 只报「删除成功」会在「一份删失败仍在池里接流量」时给出错误的安全感。逐条失败原因
   * 直接展示，因为按 key 回落识别的老组（`k:` 前缀）删到只剩 1 份时整组会从本页消失，
   * 那一份仍然活着 —— 此时页面看起来像全删成功，只有 toast 里的失败明细能说明真相。
   */
  const confirmGroupDelete = async () => {
    if (!pendingGroupDelete) return
    const { ids } = pendingGroupDelete
    setGroupDeleteBusy(true)
    try {
      const r = await deleteCredentialsBatch(ids, true)
      if (r.failed === 0) {
        toast.success(t('clones.group.deleteAllOk', { count: r.deleted }))
      } else {
        // 逐条明细：服务端去重后 results 可能比提交的 ids 短，所以按 results 自己的 id 列，
        // 不按输入数组下标对位。
        const detail = r.results
          .filter((x) => !x.ok)
          .map((x) =>
            t('clones.group.deleteAllFailedItem', {
              id: x.id,
              err: x.error ?? t('clones.group.deleteAllUnknownErr'),
            }),
          )
          .join('\n')
        toast.warning(t('clones.group.deleteAllPartial', { ok: r.deleted, fail: r.failed }), {
          description: detail,
          duration: 12000,
        })
      }
      setPendingGroupDelete(null)
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
      // 软删进回收站 → 回收站列表同样失效（与删单份同口径）。
      queryClient.invalidateQueries({ queryKey: ['trash'] })
    } catch (e) {
      // 整体失败（网络 / 鉴权 / 400 超批量上限）—— 与「部分条目失败」是两种情形，
      // 这里一条都没删掉，所以保持对话框开着让用户可以重试。
      toast.error(e instanceof Error ? e.message : String(e))
    } finally {
      setGroupDeleteBusy(false)
    }
  }

  /** 走后端 `/credentials/{id}/clone`：key 由服务端按 id 自己读，不经前端。 */
  const confirmAdd = async () => {
    if (!pendingAdd) return
    setAddBusy(true)
    try {
      const r = await cloneCredential(pendingAdd.id, pendingAdd.copies, cloneEnabled)
      // 直接展示服务端文案：它会如实说明分到了几个节点、还有几份直连
      // （「加了节点却仍然直连」是这条路最容易踩空的地方，不该被前端的通用提示盖掉）。
      toast.success(r.message)
      setAddDraft((p) => {
        const next = { ...p }
        delete next[pendingAdd.id]
        return next
      })
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
      setPendingAdd(null)
    } catch (e) {
      toast.error(e instanceof Error ? e.message : String(e))
    } finally {
      setAddBusy(false)
    }
  }

  if (isLoading) return <Skeleton className="h-24 w-full" />

  return (
    <div className="space-y-3">
      {/* 工具条：无论有没有分身组都渲染 —— 「选凭据生成」正是「一个组都还没有」时
          最需要的入口，藏在空状态后面等于让第一次用的人无路可走。 */}
      <div className="flex flex-wrap items-center gap-2">
        <span className="text-xs text-muted-foreground">
          {t('clones.group.summary', { groups: groups.length })}
        </span>
        <div className="ml-auto flex items-center gap-2">
          <span className="text-xs text-muted-foreground" id="clone-enable-label">
            {t('clones.group.enableNew')}
          </span>
          <Switch
            checked={cloneEnabled}
            onCheckedChange={setCloneEnabled}
            aria-labelledby="clone-enable-label"
          />
          <Button variant="outline" size="sm" onClick={() => setPickerOpen(true)}>
            <CopyPlus className="h-3.5 w-3.5" />
            <span className="ml-1">{t('clones.picker.open')}</span>
          </Button>
        </div>
      </div>
      <div className="text-xs text-muted-foreground">{t('clones.group.enableNewHint')}</div>

      {groups.length === 0 && (
        <div className="rounded-md border border-dashed p-4 text-center text-sm text-muted-foreground">
          {t('clones.group.empty')}
        </div>
      )}

      {groups.map((g) => (
        <div key={g.group} className="rounded-md border p-3">
          <div className="flex flex-wrap items-center gap-2">
            <span className="text-sm font-medium">
              {t('clones.group.title', { id: g.primary.id, count: g.members.length })}
            </span>
            {g.primary.subscriptionTitle && (
              <Badge variant="outline">{g.primary.subscriptionTitle}</Badge>
            )}
            {g.legacy && (
              <Badge variant="outline" title={t('clones.group.legacyHint')}>
                {t('clones.group.legacyBadge')}
              </Badge>
            )}
            <div className="ml-auto flex items-center gap-2">
              <span className="text-xs text-muted-foreground">{balances[g.primary.id] ?? ''}</span>
              {/* 「再加 N 份」：走按 id 的 clone 端点，无需把 key 再粘一遍。
                  只对 api_key 组显示 —— OAuth 号的分身注定被 invalid_grant 禁用，
                  后端会拒；这里先不显示按钮，避免让人点一个必然失败的操作。 */}
              {g.primary.authMethod === 'api_key' && (
                <>
                  <Input
                    className="h-8 w-16"
                    type="number"
                    min={1}
                    max={MAX_COPIES}
                    aria-label={t('clones.group.addCopiesLabel')}
                    value={addDraft[g.primary.id] ?? '1'}
                    onChange={(e) =>
                      setAddDraft((p) => ({ ...p, [g.primary.id]: e.target.value }))
                    }
                  />
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() => askAdd(g.primary.id)}
                    disabled={addBusy}
                  >
                    <CopyPlus className="h-3.5 w-3.5" />
                    <span className="ml-1">{t('clones.group.addCopies')}</span>
                  </Button>
                </>
              )}
              <Button
                variant="outline"
                size="sm"
                onClick={() => checkBalance(g.primary.id)}
                disabled={loadingBalance === g.primary.id}
              >
                {loadingBalance === g.primary.id ? (
                  <Loader2 className="h-3.5 w-3.5 animate-spin" />
                ) : (
                  <RefreshCw className="h-3.5 w-3.5" />
                )}
                <span className="ml-1">{t('clones.group.checkBalance')}</span>
              </Button>
              {/* 删整组：软删进回收站，不动节点池。id 在点击时快照，见 pendingGroupDelete。 */}
              <Button
                size="sm"
                variant="destructive"
                onClick={() =>
                  setPendingGroupDelete({
                    primaryId: g.primary.id,
                    ids: g.members.map((m) => m.id),
                  })
                }
                title={t('clones.group.deleteAll')}
                aria-label={t('clones.group.deleteAllAria', {
                  id: g.primary.id,
                  count: g.members.length,
                })}
              >
                <Trash2 className="h-3.5 w-3.5" />
                <span className="ml-1">{t('clones.group.deleteAll')}</span>
              </Button>
            </div>
          </div>

          {/* 组内有份走服务器裸 IP 时才出现。放在组级而不是只在成员行加个角标：
              角标说不了"为什么要紧"，而这件事的要紧之处恰恰是**组级**的
              （同一个上游账号 + 流量集中在一个 IP ⇒ 按账号关联风控）。 */}
          {g.bareExitIds.length > 0 && (
            <Callout variant="warning" className="mt-2 text-xs">
              {t('clones.group.bareExitWarn', {
                ids: g.bareExitIds.map((id) => `#${id}`).join('、'),
                n: g.bareExitIds.length,
                total: g.members.length,
                defaultValue:
                  '本组 {total} 份共用同一个上游账号，其中 {n} 份（{ids}）没有独立出口，' +
                  '会从服务器裸 IP 出去。同账号流量集中在一个 IP 上会被按账号关联风控' +
                  '（实测：克隆 10 份并全部启用，15 分钟后父号连同 10 份分身全部被自动禁用）。' +
                  '请在下方那一份的凭据卡片上配一个节点，或确认它就是要走服务器出口（比如刻意留一份做对照）。',
              })}
            </Callout>
          )}

          <div className="mt-2 space-y-1.5">
            {g.members.map((m, idx) => (
              <div key={m.id} className="flex flex-wrap items-center gap-2 text-xs">
                <span className="font-mono">#{m.id}</span>
                <Badge variant={m.id === g.primary.id ? 'default' : 'secondary'}>
                  {m.id === g.primary.id
                    ? t('clones.group.primaryBadge')
                    : // 老数据没有 cloneSeq，用**位置序号**（已按 id 稳定排序）而不是
                      // `#?` —— 后者会让一组 9 份全显示成「分身 #?」，完全无法区分。
                      t('clones.group.cloneBadge', { seq: m.cloneSeq ?? idx + 1 })}
                </Badge>
                {m.disabled && <Badge variant="destructive">{t('clones.group.disabled')}</Badge>}
                {hasOwnExit(m) ? (
                  <span className="font-mono text-muted-foreground">{m.proxyUrl}</span>
                ) : (
                  // 是**哪一份**没有出口必须能一眼定位到行 —— 组级告警只报 id，
                  // 而用户接下来要做的事（给它配节点）是在这一行上。
                  <Badge
                    variant="warning"
                    title={t('clones.group.bareExitHint', {
                      defaultValue:
                        'proxyUrl 为空或 direct ⇒ 走服务器裸 IP，与同组其它份共用同一个上游账号。',
                    })}
                  >
                    <AlertTriangle className="mr-1 h-3 w-3" aria-hidden="true" />
                    {t('clones.group.bareExitBadge', { defaultValue: '无独立出口' })}
                  </Badge>
                )}
                <div className="ml-auto flex items-center gap-1">
                  <Input
                    className="h-7 w-40"
                    placeholder={t('clones.group.tagPlaceholder')}
                    maxLength={64}
                    value={tagDraft[m.id] ?? m.tag ?? ''}
                    onChange={(e) => setTagDraft((p) => ({ ...p, [m.id]: e.target.value }))}
                  />
                  {tagDraft[m.id] !== undefined && (
                    <Button size="sm" variant="outline" onClick={() => saveTag(m.id)}>
                      {t('clones.group.saveTag')}
                    </Button>
                  )}
                  {/* 删这一份：软删进回收站，**不动**同组其它成员，也不动节点池。 */}
                  <Button
                    size="sm"
                    variant="outline"
                    className="h-7 px-2"
                    onClick={() =>
                      setPendingMemberDelete({
                        member: m,
                        isPrimary: m.id === g.primary.id,
                        groupSize: g.members.length,
                      })
                    }
                    title={t('clones.member.delete')}
                    aria-label={t('clones.member.deleteAria', { id: m.id })}
                  >
                    <Trash2 className="h-3.5 w-3.5" />
                  </Button>
                </div>
              </div>
            ))}
          </div>
        </div>
      ))}

      <ConfirmDialog
        open={!!pendingAdd}
        onOpenChange={(v) => !v && setPendingAdd(null)}
        title={t('clones.group.addCopiesTitle')}
        description={
          <>
            {t('clones.group.addCopiesConfirm', {
              n: pendingAdd?.copies ?? 0,
              id: pendingAdd?.id ?? 0,
            })}
            <span className="mt-2 block">
              {cloneEnabled
                ? t('clones.group.addCopiesEnabledNote')
                : t('clones.group.addCopiesDisabledNote')}
            </span>
          </>
        }
        confirmLabel={t('clones.group.addCopies')}
        loading={addBusy}
        onConfirm={confirmAdd}
      />

      {/* 删除分身二次确认。破坏性样式 + 明说是软删（进回收站可恢复）。 */}
      <ConfirmDialog
        open={!!pendingMemberDelete}
        onOpenChange={(v) => !v && setPendingMemberDelete(null)}
        title={t('clones.member.deleteTitle', { id: pendingMemberDelete?.member.id ?? 0 })}
        destructive
        description={
          <>
            {t('clones.member.deleteDesc', {
              id: pendingMemberDelete?.member.id ?? 0,
              count: pendingMemberDelete?.groupSize ?? 0,
            })}
            {pendingMemberDelete?.isPrimary && (
              <span className="mt-2 block">{t('clones.member.deletePrimaryWarn')}</span>
            )}
            {pendingMemberDelete && !pendingMemberDelete.member.disabled && (
              <span className="mt-2 block">{t('clones.member.deleteAutoDisable')}</span>
            )}
          </>
        }
        confirmLabel={t('clones.member.deleteConfirm')}
        loading={deleteBusy}
        onConfirm={confirmMemberDelete}
      />

      {/* 删整组二次确认。必须说清三件事：删几份 + 主份是谁、是软删可从回收站恢复、
          节点池不受影响（节点是候选池、凭据的 proxy_* 是绑定结果，两者独立）。 */}
      <ConfirmDialog
        open={!!pendingGroupDelete}
        onOpenChange={(v) => !v && setPendingGroupDelete(null)}
        title={t('clones.group.deleteAllTitle', {
          count: pendingGroupDelete?.ids.length ?? 0,
        })}
        destructive
        description={
          <>
            {t('clones.group.deleteAllDesc', {
              count: pendingGroupDelete?.ids.length ?? 0,
              id: pendingGroupDelete?.primaryId ?? 0,
            })}
            <span className="mt-2 block">{t('clones.group.deleteAllNodeNote')}</span>
          </>
        }
        confirmLabel={t('clones.group.deleteAllConfirm')}
        loading={groupDeleteBusy}
        onConfirm={confirmGroupDelete}
      />

      {/* 「选凭据生成」：列表形态与「凭据设置」里的凭据选择器一致
          （每行 `#id · email` + 副行订阅/认证方式，点行选中）。 */}
      <Dialog open={pickerOpen} onOpenChange={setPickerOpen}>
        <DialogContent className="max-w-lg">
          <DialogHeader>
            <DialogTitle>{t('clones.picker.title')}</DialogTitle>
            <DialogDescription>{t('clones.picker.desc')}</DialogDescription>
          </DialogHeader>

          {cloneable.length === 0 ? (
            <div className="rounded-md border border-dashed p-4 text-center text-sm text-muted-foreground">
              {t('clones.picker.noCloneable')}
            </div>
          ) : (
            <div className="max-h-[300px] overflow-y-auto">
              {cloneable.map((c) => (
                <button
                  key={c.id}
                  type="button"
                  onClick={() => setPickedId(c.id)}
                  aria-pressed={pickedId === c.id}
                  className={`flex w-full items-center justify-between gap-4 rounded-md border-b border-border/40 px-2 py-2.5 text-left last:border-0 hover:bg-muted/50 ${
                    pickedId === c.id ? 'bg-muted' : ''
                  }`}
                >
                  <span className="min-w-0">
                    <span className="block truncate text-sm">
                      #{c.id}
                      {c.name ? ` · ${c.name}` : c.email ? ` · ${c.email}` : ''}
                    </span>
                    <span className="mt-0.5 block text-[11px] text-muted-foreground">
                      {c.subscriptionTitle || c.authMethod || t('clones.picker.credential')}
                      {c.disabled ? ` · ${t('clones.group.disabled')}` : ''}
                    </span>
                  </span>
                  {pickedId === c.id && <Check className="h-4 w-4 shrink-0" />}
                </button>
              ))}
            </div>
          )}

          {notCloneableCount > 0 && (
            <p className="text-xs text-muted-foreground">
              {t('clones.picker.hiddenNote', { n: notCloneableCount })}
            </p>
          )}

          <div className="flex items-center gap-2">
            <span className="text-sm">{t('clones.picker.copiesLabel')}</span>
            <Input
              className="h-8 w-20"
              type="number"
              min={1}
              max={MAX_COPIES}
              value={pickerCopies}
              onChange={(e) => setPickerCopies(e.target.value)}
              aria-label={t('clones.group.addCopiesLabel')}
            />
            <span className="ml-auto flex items-center gap-2">
              <span className="text-xs text-muted-foreground" id="clone-enable-label-picker">
                {t('clones.group.enableNew')}
              </span>
              <Switch
                checked={cloneEnabled}
                onCheckedChange={setCloneEnabled}
                aria-labelledby="clone-enable-label-picker"
              />
            </span>
          </div>

          <DialogFooter>
            <Button variant="outline" onClick={() => setPickerOpen(false)}>
              {t('clones.picker.cancel')}
            </Button>
            <Button onClick={submitPicker} disabled={pickedId === null || cloneable.length === 0}>
              <CopyPlus className="h-4 w-4" />
              <span className="ml-1">{t('clones.picker.submit')}</span>
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  )
}

/**
 * 「分身管理」卡片。
 *
 * 两条创建路径，**共用同一段后端实现**（不是两条并行的校验路径）：
 *
 * - 首次多开：加号对话框的 `copies` 字段（`POST /credentials`）。
 * - 给**已导入**的号扩容：本页每组的「再加 N 份」（`POST /credentials/{id}/clone`）。
 *   必须是按 id 的独立端点：凭据列表只有 `apiKeyHash` 与掩码，**前端拿不到 key 原文**，
 *   否则用户只能回加号对话框重新粘一遍 key。服务端按 id 自读 key，key 一步不出服务端。
 *   份数逻辑（去重绕过 / 组复用 / 序号预留 / 节点分配 / OAuth 拒绝）全部复用前者。
 *
 * 节点池是这两条路共同的消费方：建分身时按份从本页**启用**的节点里各取一个写进凭据，
 * 不足时多出来的份直连（刻意不复用节点，复用等于共用出口），响应文案会如实说明。
 *
 * ⚠️ 「再加 N 份」这条路（`POST /credentials/{id}/clone`）**从不碰父号的代理**，只建新条目 ——
 * 所以父号原本没有出口时，加完分身仍然没有（后端会在响应里点名告警，本页也会标出来）。
 * 这是刻意的：`proxyUrl` 是用户的显式配置，没有出口也可能是刻意留的对照，不擅自覆盖。
 * 组标识（`cloneGroup`）是唯一的例外，它是系统内部的分组标识、没有语义选择余地，会被回填。
 */
export function CloneManagementCard() {
  const { t } = useTranslation()
  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="text-base">{t('clones.card.title')}</CardTitle>
      </CardHeader>
      <CardContent className="space-y-5">
        <div className="rounded-md bg-muted/50 p-2 text-xs text-muted-foreground">
          {t('clones.card.quotaWarning')}
        </div>
        <div>
          <div className="pb-2 text-xs font-medium uppercase tracking-wide text-muted-foreground">
            {t('clones.group.heading')}
          </div>
          <CloneGroupsPanel />
        </div>
        <div>
          <div className="pb-2 text-xs font-medium uppercase tracking-wide text-muted-foreground">
            {t('clones.node.heading')}
          </div>
          <SocksNodesPanel />
        </div>
      </CardContent>
    </Card>
  )
}
