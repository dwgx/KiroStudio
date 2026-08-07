I have what I need. Spec below.

# 「分身管理」页 — 前端完整实施规格

## 0 前置假设（后端契约，两处依赖同批次 agent）

| 依赖 | 来源 | 若未落地的降级 |
|---|---|---|
| `CredentialStatusItem.cloneGroup` / `cloneSeq` | credentials.rs 新增 `clone_group`/`clone_seq`（serde camelCase 已在 `CredentialStatusItem` 生效） | 用 `apiKeyHash` 分组 + `cloneSeq` 全 undefined，**只是"删除分身"按钮全禁用**，页面仍可用 |
| `POST /credentials/{id}/clone` | 新端点（本页唯一必须新增的写端点） | 页面退化为只读 |
| `GET/POST/DELETE /socks-nodes` | socks_node agent 的 `socks_nodes.json` | 节点区显示 EmptyState |

其余全部复用既有端点：余额 `useCachedBalances()` + `useCredentialBalance(id)`、
测速 `POST /proxy/test`、绑代理 `POST /credentials/{id}/proxy`、删分身 `POST /credentials/batch-delete`。

---

## 1 页面结构

```
┌─ Card「分身管理」  settingspage.card.clones ──────────────────────────────┐
│ CardHeader: <Copy/> 分身管理     [刷新 ↻]  共 3 组 · 12 个凭据            │
│ CardContent                                                              │
│ ┌── A 分身组区（按 cloneGroup 分组，每组一块）─────────────────────────┐ │
│ │ ▸ 组 a3f9c1e2…  主号 #438 · Kiro Pro · eu-central-1        [4 个分身] │ │
│ │   ┌ 主凭据行 ────────────────────────────────────────────────────┐   │ │
│ │   │ #438  user@x.com  <Badge 主号>  <Badge api_key>              │   │ │
│ │   │ ▓▓▓▓▓▓▓▓░░ 剩余 66.1%  1234/2000  截至 3 分钟前   ← BalanceBar│   │ │
│ │   │ [查看余额 Wallet] [一键生成分身 Copy] ← 份数 NumberStepper 1..16│  │ │
│ │   │            节点选择 <select 节点表 + 直连>                    │   │ │
│ │   └──────────────────────────────────────────────────────────────┘   │ │
│ │   ┌ 分身行 ×N（紧凑单行）───────────────────────────────────────┐   │ │
│ │   │ #439 <Badge 分身 #1> socks5://…:1080 [测速] p50 312ms  [×]  │   │ │
│ │   │ #440 <Badge 分身 #2> socks5://…:1081 [测速] —          [×]  │   │ │
│ │   └──────────────────────────────────────────────────────────────┘   │ │
│ │   [☑ 全选分身]  [删除选中分身 (2)]  [一键删除本组全部分身]           │ │
│ └──────────────────────────────────────────────────────────────────────┘ │
│ ┌── B SOCKS 节点区 ────────────────────────────────────────────────────┐ │
│ │ 节点表（可复用于生成分身时自动绑定）                                  │ │
│ │ ┌ 新增节点 ─────────────────────────────────────────────────────┐    │ │
│ │ │ [名称 Input] [socks5://host:port Input] [ProxyTestButton] [+]  │    │ │
│ │ │ [账号 Input] [密码 Input type=password]                        │    │ │
│ │ └───────────────────────────────────────────────────────────────┘    │ │
│ │ us-west-1  socks5://1.2.3.4:1080  绑定 2 号  [测速] [删除]           │ │
│ │ us-east-1  socks5://1.2.3.4:1081  未绑定     [测速] [删除]           │ │
│ └──────────────────────────────────────────────────────────────────────┘ │
└──────────────────────────────────────────────────────────────────────────┘
```

组件对应（全部 file:line 为现存文件）：

| 区块 | 用什么 |
|---|---|
| 外层 Card | `admin-ui/src/components/ui/card.tsx` — settings-page 已导入 (`settings-page.tsx:29`) |
| 分组折叠 | 自写 `<details>`/`useState` 展开态，**不引新库**（本仓无 accordion 组件） |
| 主号 / 分身 Badge | `admin-ui/src/components/ui/badge.tsx` |
| 余额条 | 抽取自 `admin-ui/src/components/credential-card.tsx:455-526` → 新 `BalanceBar`（见 §2） |
| 「查看余额」按钮 | 照抄 `credential-card.tsx:836-849` 样式（sky outline + `Wallet`），但**不开 Dialog**，就地 `useCredentialBalance(id)` 覆盖显示 |
| 份数选择 | `admin-ui/src/components/ui/number-stepper.tsx:5-15`，`min=1 max=16`（对齐 `MAX_CREDENTIAL_COPIES`） |
| 代理输入 + 测速 | `admin-ui/src/components/proxy-test-button.tsx`（props `{proxyUrl, proxyUsername?, proxyPassword?, className?}`）**零改动直接用** |
| 二次确认 | `admin-ui/src/components/ui/confirm-dialog.tsx:18-38` |
| 空态 | `admin-ui/src/components/ui/empty-state.tsx` |
| 骨架屏 | `admin-ui/src/components/ui/skeleton.tsx` |

---

## 2 新建文件与 props

### 2.1 `admin-ui/src/components/settings/clone-management-card.tsx`（新建，主体）

```ts
export function CloneManagementCard(): JSX.Element   // 无 props，自带 hooks
```
顶层独立组件，与 `TrashCard`(`settings-page.tsx:1253`) 同款：自带 `<Card>`、自有 react-query、
**不进 `FormState`/`diff`**，与底部保存栏 (`settings-page.tsx:2447`) 零耦合。

内部私有子组件（同文件，不导出）：
```ts
function CloneGroupBlock(p: { group: CloneGroup; nodes: SocksNode[] }): JSX.Element
function CloneRow(p: { item: CredentialStatusItem; selected: boolean; onToggle: (v: boolean) => void }): JSX.Element
function SocksNodeSection(p: { nodes: SocksNode[]; bindCount: Record<string, number> }): JSX.Element
```

### 2.2 `admin-ui/src/components/balance-bar.tsx`（抽取，**动 credential-card.tsx**）

```ts
export function BalanceBar(p: {
  balance: BalanceResponse | CachedBalanceItem | null   // null = 无缓存
  pending?: boolean
  /** 有值 → 显示"截至 X 分钟前"；null → 显示"实时" */
  cachedAt?: number | null
  className?: string
}): JSX.Element
```

**权衡（明确给出）**：抽取要改 `credential-card.tsx`（删 `renderBalanceBar` 闭包 72 行、
改 1 处调用点、把 `formatAmount`/`formatCachedAt` 一并搬走或从 `lib/format` 导入）。
该文件本轮有其它会话的未提交改动 → 冲突风险实存。

**我的建议：抽取，但排在最后一步单独一个 patch**。理由是判据能过 —— 「移除它即失败」
成立：不抽取就会有两份百分比阈值（40/20）与配色映射，任何一份改了另一份不跟，
用户会在两个页面看到同一个号一个绿一个黄。而复制粘贴的第二份**没有任何测试能发现它漂移**。
若主线判断冲突代价更高，可退化为 `BalanceBar` 只在新页面用、`credential-card` 暂不动
（此时两份并存，须在新文件头写明"待收口"TODO 并指向 `credential-card.tsx:455`）。

### 2.3 `admin-ui/src/components/proxy-editor.tsx` — **本轮不抽取**

分身行的代理只需「展示 + 测速 + 换节点（下拉选）」，不需要 `credential-card.tsx:923-973`
那套「URL/账密自由输入 + 保存」的完整表单。为一个用不上的形态去改高冲突文件，构造不出
「移除即失败」的测试。分身换节点直接调 `setCredentialProxy(id, node.url, node.username, node.password)`。

### 2.4 `admin-ui/src/api/socks-nodes.ts`（新建）

```ts
export async function listSocksNodes(): Promise<SocksNodesResponse>
export async function addSocksNode(req: AddSocksNodeRequest): Promise<SocksNode>
export async function deleteSocksNode(id: string): Promise<SuccessResponse>
```

### 2.5 `admin-ui/src/hooks/use-clones.ts`（新建）

```ts
export function useSocksNodes(): UseQueryResult<SocksNodesResponse>
export function useAddSocksNode(): UseMutationResult<SocksNode, unknown, AddSocksNodeRequest>
export function useDeleteSocksNode(): UseMutationResult<SuccessResponse, unknown, string>
export function useCloneCredential(): UseMutationResult<CloneCredentialResponse, unknown, { id: number; req: CloneCredentialRequest }>
/** 纯派生：把 credentials 列表折成分身组，无网络请求 */
export function useCloneGroups(items: CredentialStatusItem[] | undefined): CloneGroup[]
```

---

## 3 数据流

| 数据 | hook | 策略 |
|---|---|---|
| 凭据列表 | 复用 `useCredentials()`（`hooks/use-credentials.ts:26`） | 已 `refetchInterval: 30000`，**不新开查询**，共享同一 queryKey `['credentials']` 缓存 |
| 分身分组 | `useCloneGroups(data?.credentials)` + `useMemo` | 纯前端派生，零请求 |
| 缓存余额 | 复用 `useCachedBalances()`（`use-credentials.ts:67`） | 只读后端缓存、零上游、`refetchInterval 300000` / `staleTime 60000` |
| 点击查询余额 | 复用 `useCredentialBalance(id)`（`:55`，`retry:false`） | `enabled` 由本地 `viewingId` 门控；**只对主号可点**，分身共账号 → 重复探测加重风控 |
| 节点表 | `useSocksNodes()` | `staleTime: 30000`，**不设 `refetchInterval`**（节点表低频变更，轮询无意义），写操作后 `invalidateQueries(['socks-nodes'])` |

**分组算法**（`useCloneGroups`）：
```
key = item.cloneGroup ?? item.apiKeyHash ?? `id:${item.id}`
主号 = 组内 cloneSeq == null 的第一个；若全有 cloneSeq（父已删）→ 取 id 最小者，
       并置 group.orphan = true（UI 显示"主号已删除"提示，禁用生成按钮）
分身 = 组内 cloneSeq != null 且 id !== primary.id，按 cloneSeq 升序
只含 1 个成员且无 cloneGroup 的组 → 折叠为"未分身账号"，默认收起
```

**故障不清屏**：所有渲染判定写 `error && !data`，不写 `if (error)` —— 30s 轮询期间
一次网络抖动不能把已展示的分组表清空。骨架屏只在 `isLoading && !data` 时出。
`gcTime` 沿用全局配置（`main.tsx` 已放宽到 30min），本页不覆盖。

---

## 4 交互细节

**一键生成分身 — 要二次确认**。不是因为不可逆（是可逆的，落回收站），而是因为它
**对上游有真实副作用**：新分身立刻进池参与调度，向同一个账号发请求。确认框须写清后果：

> 将按主号 #438 生成 **4** 个分身，每份获得独立 machineId，并绑定节点 us-west-1 / us-east-1 …。
> 分身共用同一个上游账号额度 —— 生成后网关放行量会按凭据数放大，请同步调低
> `credentialRpmLimit`（当前 200，5 份 = 网关认为 835 RPM，而账号实测约 134 RPM）。

这段警告不是可选文案：不写，用户会重演「分身越多 429 越多」。

**一键删除分身 — 必须确认，且必须写明是软删**：

> 将删除本组 **4** 个分身（#439-442）。这是**软删除**：凭据进回收站，
> 可在「设置 → 回收站」恢复；只有在回收站「永久清除」才不可恢复。
> 主号 #438 **不会**被删除。

删除走 `deleteCredentialsBatch(ids, /* force */ true)` —— `force` 跳过「必须先禁用」门，
否则要先 N 次 PATCH 禁用（`use-credentials.ts:200-211` 的注释已说明该端点存在正是为此）。

**loading / 部分失败**：
- 生成中：按钮 `<Loader2 animate-spin>` + 禁用整组操作区；`copies>1` 时后端逐份创建，
  返回 `credentialIds` 可能**短于**请求份数 → 比对长度，短了出 warning toast
  「已生成 3/4 份，其余失败（可能触发去重或上游拒绝）」，而非 success。
- 删除中：`ConfirmDialog loading` 置 true（它会禁用两个按钮并显示"处理中…"）。
- 批量删除部分失败：`BatchDeleteResponse.failed > 0` → `toast.warning` 列出失败 id 与
  `results[].error` 首条；成功那部分照常 invalidate。
- 所有 mutation 成功后 `invalidateQueries` 三个 key：`['credentials']`、`['trash']`、
  `['cached-balances']`（删号后缓存余额里会留孤儿条目）。

---

## 5 settings-page.tsx 挂载 patch（4 处，含精确 old/new）

**① 联合类型**（`settings-page.tsx:125`）
old: `... | 'appearance' | 'export' | 'trash'`
new: `... | 'appearance' | 'export' | 'clones' | 'trash'`

**② SECTION_DEFS**（`:135` 后、`{ id: 'trash' ...}` 前插入）
```tsx
  { id: 'clones', labelKey: 'settingspage.section.clones', icon: <Copy className="h-4 w-4" /> },
```
需在 `:5-28` lucide 导入块补 `Copy`。

**③ CARD_INDEX_DEFS**（`:159` 一带追加）
```tsx
  { section: 'clones', titleKey: 'settingspage.card.clones', kwKey: 'settingspage.card.clones.kw' },
```

**④ JSX 挂载**（`:1791-1794` 回收站 `SectionGate` 之后）
```tsx
<SectionGate section="clones" titleKey="settingspage.card.clones" kwKey="settingspage.card.clones.kw">
  <CloneManagementCard />
</SectionGate>
```

---

## 6 前端类型 patch

### 6.1 `admin-ui/src/types/api.ts`

**patch A** — `CredentialStatusItem` 尾部（`:67` 的 `cooldownReason?: string` 之后、`}` 之前）：
```ts
  /**
   * 分身组标识：同一上游账号的全部凭据共享（后端 clone_group，账号 key 的 SHA256 前 16 位）。
   * undefined = 旧数据/非分身，前端回退用 apiKeyHash 分组。
   */
  cloneGroup?: string
  /** 组内分身序号（1 起）。undefined = 主号/普通号 —— 「删除分身」只认有此字段的。 */
  cloneSeq?: number
```

**patch B** — 文件尾追加（camelCase，与除 `SetProxyRequest` 外的多数类型一致）：
```ts
// ============ 分身管理 ============

/** 一个可复用的 SOCKS/HTTP 代理节点（后端 socks_nodes.json）。密码不下发。 */
export interface SocksNode {
  id: string
  name: string
  url: string
  username?: string
  /** 是否已配置密码（后端只下发布尔，绝不回传明文）。 */
  hasPassword: boolean
}

export interface SocksNodesResponse {
  total: number
  nodes: SocksNode[]
}

/** 新增节点请求（camelCase）。 */
export interface AddSocksNodeRequest {
  name: string
  url: string
  username?: string
  password?: string
}

/** 生成分身请求（camelCase）。 */
export interface CloneCredentialRequest {
  /** 份数 1..16，对齐后端 MAX_CREDENTIAL_COPIES。 */
  copies: number
  /**
   * 依次绑定的节点 id；不足则循环取用，空数组 = 全部直连。
   * 分身必须继承父号 api_region/region/auth_region/subscription_title（后端负责），
   * 不继承会打错 region host → 403 bearer invalid。
   */
  nodeIds?: string[]
}

/** 生成分身响应。credentialIds 可能短于 copies（部分失败仍返 200）。 */
export interface CloneCredentialResponse {
  success: boolean
  message: string
  credentialIds: number[]
  cloneGroup: string
}

/** 前端派生的分身组（无对应后端类型）。 */
export interface CloneGroup {
  key: string
  primary: CredentialStatusItem
  clones: CredentialStatusItem[]
  /** 主号已被删除，组内只剩分身 —— 禁用「生成」并提示。 */
  orphan: boolean
}
```

### 6.2 `admin-ui/src/api/credentials.ts`

导入块（`:28` 的 `CredentialRegionsResponse,` 后）加 `CloneCredentialRequest,`
`CloneCredentialResponse,`，文件尾追加：
```ts
// 按主号一键生成分身：每份独立 machineId，继承父号 region/订阅信息（后端负责继承，
// 不继承会导致分身打错 region host → 403 bearer invalid → 实测 0% 成功）。
export async function cloneCredential(
  id: number,
  req: CloneCredentialRequest
): Promise<CloneCredentialResponse> {
  const { data } = await api.post<CloneCredentialResponse>(`/credentials/${id}/clone`, req)
  return data
}
```

> 命名核对：新增三个请求体（`AddSocksNodeRequest` / `CloneCredentialRequest`）**一律 camelCase**，
> 后端对应结构体须带 `#[serde(rename_all = "camelCase")]`。
> 唯一发 snake_case 的是既有的 `setCredentialProxy`（`credentials.ts:181-185`，对齐无 camelCase
> 属性的 `SetProxyRequest`）——分身换节点复用它，**保持 snake_case 不要"顺手统一"**。

---

## 7 i18n 键（3 语，扁平键，按 `settingspage.*` 既有前缀）

插入位置：`zh.json:1103` / `en.json:1103` / `ja.json:1103` 一带（`settingspage.card.trash` 前，键按字母序）。

| key | zh | en | ja |
|---|---|---|---|
| `settingspage.section.clones` | 分身管理 | Clones | 分身管理 |
| `settingspage.card.clones` | 分身管理 | Clone Management | 分身管理 |
| `settingspage.card.clones.kw` | 分身,多开,socks,代理,节点,主号,clone | clone,copies,socks,proxy,node,primary,分身 | 分身,多重起動,socks,プロキシ,ノード,主アカウント,clone |
| `settingspage.clones.desc` | 同一账号多开成多个凭据，各绑不同 SOCKS 出口。分身共用账号额度。 | Run one account as several credentials, each via its own SOCKS exit. Clones share the account quota. | 1つのアカウントを複数の資格情報として稼働させ、各々に SOCKS 出口を割り当てます。枠は共有です。 |
| `settingspage.clones.summary` | 共 {{groups}} 组 · {{total}} 个凭据 | {{groups}} groups · {{total}} credentials | {{groups}} グループ · {{total}} 件 |
| `settingspage.clones.primary` | 主号 | Primary | 主アカウント |
| `settingspage.clones.cloneBadge` | 分身 #{{seq}} | Clone #{{seq}} | 分身 #{{seq}} |
| `settingspage.clones.cloneCount` | {{n}} 个分身 | {{n}} clones | 分身 {{n}} 件 |
| `settingspage.clones.orphan` | 主号已删除，无法生成新分身 | Primary deleted; cannot generate clones | 主アカウント削除済み。分身を生成できません |
| `settingspage.clones.ungrouped` | 未分身账号 | Accounts without clones | 分身なしのアカウント |
| `settingspage.clones.viewBalance` | 查看余额 | Check balance | 残量を確認 |
| `settingspage.clones.copies` | 份数 | Copies | 数量 |
| `settingspage.clones.bindNode` | 绑定节点 | Bind node | ノードを割当 |
| `settingspage.clones.direct` | 直连（不走代理） | Direct (no proxy) | 直接接続（プロキシなし） |
| `settingspage.clones.generate` | 一键生成分身 | Generate clones | 分身を一括生成 |
| `settingspage.clones.generating` | 生成中… | Generating… | 生成中… |
| `settingspage.clones.selectAll` | 全选分身 | Select all clones | 分身をすべて選択 |
| `settingspage.clones.deleteSelected` | 删除选中分身（{{n}}） | Delete selected ({{n}}) | 選択した分身を削除（{{n}}） |
| `settingspage.clones.deleteAll` | 一键删除本组全部分身 | Delete all clones in group | このグループの分身をすべて削除 |
| `settingspage.clones.confirm.generateTitle` | 生成 {{n}} 个分身？ | Generate {{n}} clones? | 分身を {{n}} 件生成しますか？ |
| `settingspage.clones.confirm.generateDesc` | 将按主号 #{{id}} 生成 {{n}} 个分身，每份获得独立 machineId。分身共用同一账号额度：生成后网关放行量会按凭据数放大，请同步调低「每凭据 RPM 上限」，否则更早撞上游 429。 | {{n}} clones will be created from #{{id}}, each with its own machineId. Clones share one account quota: the gateway will admit N× more traffic, so lower the per-credential RPM limit accordingly or you will hit upstream 429 sooner. | 主アカウント #{{id}} から分身を {{n}} 件作成し、各々に独立した machineId を割り当てます。枠は共有のため、ゲートウェイの通過量が件数分に増えます。資格情報ごとの RPM 上限を下げないと上流 429 が早まります。 |
| `settingspage.clones.confirm.deleteTitle` | 删除 {{n}} 个分身？ | Delete {{n}} clones? | 分身を {{n}} 件削除しますか？ |
| `settingspage.clones.confirm.deleteDesc` | 将删除分身 {{ids}}。这是**软删除**：凭据进回收站，可在「设置 → 回收站」恢复；只有在回收站「永久清除」才不可恢复。主号 #{{primaryId}} 不会被删除。 | Clones {{ids}} will be deleted. This is a **soft delete**: they go to Trash and can be restored under Settings → Trash. Only "purge" in Trash is irreversible. Primary #{{primaryId}} is kept. | 分身 {{ids}} を削除します。これは**論理削除**でゴミ箱に移動し、「設定 → ゴミ箱」から復元できます。ゴミ箱の「完全削除」のみ復元不可です。主アカウント #{{primaryId}} は残ります。 |
| `settingspage.clones.toast.generated` | 已生成 {{n}} 个分身 | Generated {{n}} clones | 分身 {{n}} 件を生成しました |
| `settingspage.clones.toast.generatedPartial` | 已生成 {{ok}}/{{want}} 份，其余失败（可能触发去重或上游拒绝） | Generated {{ok}}/{{want}}; the rest failed (dedup or upstream rejection) | {{ok}}/{{want}} 件を生成。残りは失敗（重複判定または上流拒否） |
| `settingspage.clones.toast.deleted` | 已删除 {{n}} 个分身（在回收站可恢复） | Deleted {{n}} clones (restorable from Trash) | 分身 {{n}} 件を削除（ゴミ箱から復元可能） |
| `settingspage.clones.toast.deletedPartial` | 已删除 {{ok}} 个，{{failed}} 个失败：{{error}} | Deleted {{ok}}, {{failed}} failed: {{error}} | {{ok}} 件削除、{{failed}} 件失敗：{{error}} |
| `settingspage.clones.nodes.title` | SOCKS 节点 | SOCKS nodes | SOCKS ノード |
| `settingspage.clones.nodes.desc` | 节点表是候选池；生成分身时按顺序绑定。密码加密落盘，不回传明文。 | The node table is a candidate pool, bound in order when generating clones. Passwords are encrypted at rest and never returned. | ノード表は候補プールで、分身生成時に順に割り当てます。パスワードは暗号化保存され、平文は返しません。 |
| `settingspage.clones.nodes.namePlaceholder` | 节点名称（如 us-west-1） | Node name (e.g. us-west-1) | ノード名（例：us-west-1） |
| `settingspage.clones.nodes.urlPlaceholder` | socks5://host:port | socks5://host:port | socks5://host:port |
| `settingspage.clones.nodes.userPlaceholder` | 代理账号（可选） | Proxy username (optional) | プロキシユーザー名（任意） |
| `settingspage.clones.nodes.passPlaceholder` | 代理密码（可选） | Proxy password (optional) | プロキシパスワード（任意） |
| `settingspage.clones.nodes.add` | 添加节点 | Add node | ノードを追加 |
| `settingspage.clones.nodes.bound` | 已绑 {{n}} 个凭据 | Bound to {{n}} | {{n}} 件に割当済み |
| `settingspage.clones.nodes.unbound` | 未绑定 | Unbound | 未割当 |
| `settingspage.clones.nodes.delete` | 删除节点 | Delete node | ノードを削除 |
| `settingspage.clones.nodes.confirmDeleteTitle` | 删除节点 {{name}}？ | Delete node {{name}}? | ノード {{name}} を削除しますか？ |
| `settingspage.clones.nodes.confirmDeleteDesc` | 仅从候选池移除。已绑该节点的 {{n}} 个凭据**保持原代理不变**，需手动改。 | Removes it from the candidate pool only. The {{n}} credentials already bound keep their current proxy and must be changed manually. | 候補プールから削除するだけです。既に割当済みの {{n}} 件は現在のプロキシを維持するため、手動で変更してください。 |
| `settingspage.clones.nodes.empty` | 还没有节点，先在上面添加一个 | No nodes yet. Add one above. | ノードがありません。上で追加してください。 |
| `settingspage.clones.empty` | 池里还没有可分身的账号 | No accounts available to clone | 分身できるアカウントがありません |
| `settingspage.clones.loadFailed` | 加载失败，稍后自动重试 | Load failed; retrying shortly | 読み込み失敗。まもなく再試行します |

注：`confirm.*Desc` 里的 `**…**` 只是强调意图，`ConfirmDialog.description` 是 `React.ReactNode`
（`confirm-dialog.tsx:32`），实现时用 `<strong>` 包，不要指望 markdown 渲染。

---

## 8 未能确认

- `POST /credentials/{id}/clone` 的实际路径/字段名由后端 agent 定，我按 camelCase 假设。若后端选择
  复用 `AddCredentialRequest.copies`（已有 `copies` + `add_credential_allowing_duplicate`），
  则前端改调 `addCredential({ ...父号字段, copies })` —— 但前端**拿不到父号的 accessToken/refreshToken**
  （`CredentialStatusItem` 只有 hash 与 masked），所以复用这条路走不通，新端点是必需的。
- `ConfirmDialog` 的「取消/处理中…」是硬编码中文（`confirm-dialog.tsx:52,59`），i18n 未覆盖。
  本页不修（超出 scope），但英日用户会看到这两处中文 —— 值得单独一个 patch。