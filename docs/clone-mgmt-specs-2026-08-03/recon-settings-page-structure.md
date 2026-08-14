Reconnaissance complete. Findings below.

## 1. 设置页分区结构

**不是 Tabs 组件** —— 是自建的「按钮组 nav + `SectionGate` 条件渲染」。`admin-ui/src/components/ui/` 里根本没有 `tabs.tsx`。

三处相互耦合的定义：

| 位置 | 作用 |
|---|---|
| `settings-page.tsx:125` | `type SectionId = 'basic' \| 'security' \| ... \| 'trash'` 联合类型（9 个） |
| `settings-page.tsx:127-137` | `SECTION_DEFS[]`：`{ id, labelKey, icon }`，驱动顶部按钮渲染（1751-1770 那段 `SECTION_DEFS.map`） |
| `settings-page.tsx:141-160` | `CARD_INDEX_DEFS[]`：`{ section, titleKey, kwKey }`，只驱动**搜索命中计数**（`matchedSections`, 1487-1494）。漏填不影响显示，只让搜索计数不准 |

分区名与顺序（`:128-136`）：basic / security / scheduling / storage / service / privacy / appearance / export / trash。

渲染机制：`ActiveSectionContext`（`:172`）持当前 tab，每张卡片包在 `<SectionGate section=... titleKey=... kwKey=...>` 里（`:193-216`）。未搜索时 `section === active` 才渲染；搜索态下 tab 栏隐藏、按 title/keyword 命中跨区展示。

### 新增「分身管理」最少改 4 处

1. **`settings-page.tsx:125`** — 联合类型加成员：
   old: `type SectionId = 'basic' | 'security' | 'scheduling' | 'storage' | 'service' | 'privacy' | 'appearance' | 'export' | 'trash'`
   new: 同上尾部加 ` | 'clones'`
2. **`settings-page.tsx:136` 后**（`{ id: 'trash', ... },` 之后、`]` 之前）插入 `{ id: 'clones', labelKey: 'settingspage.section.clones', icon: <某 lucide icon> },`。建议插在 `trash` **之前**（trash 语义上是末位）；icon 需在 `:5-28` 的 lucide 导入块补一个（如 `Copy` / `Users`）。
3. **`settings-page.tsx:159` 后** — `CARD_INDEX_DEFS` 补 `{ section: 'clones', titleKey: 'settingspage.card.clones', kwKey: 'settingspage.card.clones.kw' },`（不补则搜索搜不到该页）。
4. **JSX 里挂载点** — 与 `TrashCard` 同型，最省事的插入点是 `settings-page.tsx:1791-1794` 那段之后（回收站 `SectionGate` 的 `</SectionGate>` 之后）：
   ```tsx
   <SectionGate section="clones" titleKey="settingspage.card.clones" kwKey="settingspage.card.clones.kw">
     <CloneManagementCard />
   </SectionGate>
   ```
   `CloneManagementCard` 应作为**顶层独立函数组件**定义（与 `TrashCard`(`:1253`) / `ServiceManagementCard`(`:473`) / `StorageStatsCard`(`:692`) 同款），自带 `<Card>` 与自己的 react-query hooks，**不进 `FormState`/`diff`**。这一点很关键：底部保存栏（`:2447-2453` 的 `handleSave`/`dirty`）只管 `form` diff；分身管理是即时生效的动作型 UI，走自己的 mutation + `queryClient.invalidateQueries`，与保存栏无耦合。

> 若分身管理里要放**开关型配置**（shield 开关、推号开关、自动分身开关），那些应进 `FormState` + `diff` 走统一保存；而"生成/删除分身"这类动作则即时执行。两者混在同一 Card 内没问题，`TrashCard`/`StorageStatsCard` 已是这种"独立组件+自有 mutation"先例。
> 建议把 shield/推号这两个**开关**放进 `security` 或新分区，取决于下一阶段设计；此处只指出结构上两条路径都通。

## 2. 可复用组件清单

| 目标 | 位置 | 复用性 |
|---|---|---|
| **代理编辑 UI** | `credential-card.tsx:923-973`（含 URL Input + ProxyTestButton + 保存按钮 + 账密双列）；状态 `:135-138`；提交 `:205-223` | **需抽取**。当前内联在 `CredentialCard` 的齿轮 Dialog 里，硬依赖 `credential.id`/`credential.proxyUrl`。建议抽成 `components/proxy-editor.tsx`，props `{ credentialId, initialProxyUrl, onSaved? }`，然后 credential-card 与分身页各调一次。**直接复制会产生第二份逻辑**，违反"沿用现有容器" |
| **`ProxyTestButton`** | `components/proxy-test-button.tsx`，props: `{ proxyUrl: string; proxyUsername?: string; proxyPassword?: string; className?: string }`（`:8-17`） | **直接复用，零改动**。`proxyUrl` 空串或 `"direct"` 即测直连。内部调 `testProxy`（`api/ops.ts`），自己出 toast，不需要外部处理结果 |
| **余额展示条** | `credential-card.tsx:455-520` 的 `renderBalanceBar()` 闭包（读 `shownBalance`/`balancePending`/`cached?.cachedAt`），数据源 `useCachedBalances()`(`:256`) + `balance` prop | **需抽取**。是组件内闭包，依赖 4 个局部变量。分身页要"同账号同步显示"，建议抽 `<BalanceBar balance={...} pending={...} cachedAt={...} />` 纯展示组件 |
| **「查看余额」按钮** | `credential-card.tsx:836-849`（`onViewBalance(credential.id)` 回调，sky 色 outline + `Wallet` icon）。实现在 `dashboard.tsx:226-229`：`setSelectedCredentialId(id); setBalanceDialogOpen(true)` | 按钮本体可照抄样式；**Dialog 在 dashboard 里**，分身页需要自己接一个（或直接调 `useCredentialBalance(id)` 就地展示，不开 Dialog——分身页更适合后者） |
| **Card 容器** | `components/ui/card.tsx`：`Card / CardContent / CardHeader / CardTitle` | 直接复用，settings-page 已导入（`:29`） |
| **Dialog** | `components/ui/dialog.tsx`：`Dialog / DialogContent / DialogHeader / DialogTitle / DialogDescription / DialogFooter` | 直接复用 |
| **`ConfirmDialog`** | `components/ui/confirm-dialog.tsx`，settings-page 已导入（`:35`） | 直接复用 —— **"一键删除分身"必须走它**（批量删除不可逆） |
| **`NumberStepper`** | `components/ui/number-stepper.tsx:5-15`，props `{ value: number; onChange: (v:number)=>void; min?; max?; step?; className?; disabled?; 'aria-label'? }` | 直接复用。**分身份数（1..16，对齐 `MAX_CREDENTIAL_COPIES`）就用它** |
| 其他现成件 | `SegChoice`(`settings-page.tsx:405`)、`Field`(`:390`)、`ReadonlyRow`(`:437`)、`StatCard`、`Badge`、`Switch`、`Skeleton`、`EmptyState`、`Progress`、`ComboInput`、`RegionSelect` | 均可直接用；`Field`/`SegChoice`/`ReadonlyRow` 是 settings-page 内部函数，同文件内组件可直接调 |

⚠️ `Field`/`SectionGate` 内部消费 `SearchContext`：把 `CloneManagementCard` 写在 `settings-page.tsx` 同文件、或写成独立文件但仍包在 `SectionGate` 内即可正常工作。若独立文件里用 `Field`，需 export 它。

## 3. `admin-ui/src/api/credentials.ts` 函数清单

`baseURL: '/api/admin'`（`:33`），axios 实例 `api`。分身管理相关的：

| 函数 | 方法 + 路径（相对 `/api/admin`） | 请求体命名 |
|---|---|---|
| `getCredentials()` `:74` | GET `/credentials/status`（需确认实际路径） | — |
| `addCredential(req: AddCredentialRequest)` `:344` | POST `/credentials` | camelCase（含 `copies`） |
| `deleteCredential(id)` `:352` | DELETE `/credentials/{id}` | — |
| `deleteCredentialsBatch(ids, force=false)` `:364` | POST `/credentials/batch-delete` | `{ ids, force }` —— **一键删除分身用它**，1 次往返，`force=true` 跳过"必须先禁用"。部分失败仍返 200，须逐条看 `results[].ok` |
| `setCredentialProxy(id, proxyUrl, user?, pass?)` `:175` | POST `/credentials/{id}/proxy` | **snake_case**：`proxy_url`/`proxy_username`/`proxy_password`（后端 `SetProxyRequest` 无 camelCase 属性，与铁律一致） |
| `setCredentialName(id, name)` `:165` | POST `/credentials/{id}/name` | `{ name }` |
| `getCredentialBalance(id)` `:230` | GET `/credentials/{id}/balance` | 触发上游探测 |
| `getCachedBalances()` `:237` | GET `/credentials/balances/cached` | **只读缓存，绝不触发上游** → 分身页列表默认用这个 |
| `exportCredential(id)` `:374` | GET `/credentials/{id}/export` | 返 camelCase 原始 `KiroCredentials`（含 `kiroApiKey`）→ **"按主令牌生成分身"若前端侧取 key 就用它**；但更好的做法是后端出专用 clone 接口，避免明文 key 过前端 |
| 其余 | `setCredentialDisabled/Priority/RpmLimit/CustomApi/AllowedModels/Endpoint`、`listTrash/restore/purge/purgeTrashBatch`、`resetCredentialFailure`、`forceRefreshToken`、`deepVerifyCredential`、`probeCredentialRegions`、`switchProfileRegion`、`probeAvailableModels`、`enableOverage/disableOverage`、`getLoadBalancingMode/set`、`startSocialLogin/pollSocialLogin`、`startIdcLogin/pollIdcLogin`、`startExternalIdpLogin/submitExternalIdpLeg1/Leg2/Leg2Select`、`getConfigSnapshot/updateConfig` | | 除 `setCredentialProxy` 外**均 camelCase** |

react-query 封装在 `hooks/use-credentials.ts`（18 个 hook，含 `useCredentials/useCachedBalances/useCredentialBalance/useAddCredential/useDeleteCredentialsBatch`）。**新增 API 请同时加 hook，保持一致**。`testProxy` 在 `api/ops.ts`（不在 credentials.ts）。

## 4. i18n 结构

**完全扁平**，点分隔字符串键，非嵌套对象。三份文件各 **1522** 个顶层键，键集完全一致（数量相同）。JSON 内按键名字母序排列。

命名约定：`<组件名全小写去连字符>.<类别>.<名称>`
- 组件段：`settingspage.` / `credentialcard.` / `addcredentialdialog.` / `proxytestbutton.` …
- 类别段：`section.` `card.` `button.` `action.` `toast.` `hint.` `common.` `time.` `settings.` `label`/`hint` 后缀
- 特殊：`settingspage.card.X.kw` = 逗号分隔的搜索同义词（各语言各写一套）
- 插值用 `{{n}}` / `{{time}}` 花括号形式

现有量级参考：`settingspage.*` 362 键，`credentialcard.*` 152 键。

### 新增分身管理页键名草案（约 40-50 键 × 3 语言）

```
settingspage.section.clones                   分区 tab 标签
settingspage.card.clones                      卡片标题
settingspage.card.clones.kw                   搜索同义词(分身,克隆,clone,socks,代理池,…)
settingspage.clones.desc                      分区说明
settingspage.clones.primary.title             主凭据区标题
settingspage.clones.primary.empty             无主凭据空态
settingspage.clones.primary.viewBalance       「查看余额」按钮
settingspage.clones.primary.balanceLoading
settingspage.clones.primary.account           账号标识(key 尾号)
settingspage.clones.list.title                分身列表标题
settingspage.clones.list.empty                无分身空态
settingspage.clones.list.count                「共 {{n}} 个分身」
settingspage.clones.tag.label                 标签字段
settingspage.clones.tag.placeholder
settingspage.clones.tag.save
settingspage.clones.generate.title            一键生成分身
settingspage.clones.generate.copies           份数(NumberStepper aria-label)
settingspage.clones.generate.button
settingspage.clones.generate.pending
settingspage.clones.generate.hint             份数上限 16 / 继承 region 说明
settingspage.clones.delete.button             一键删除分身
settingspage.clones.delete.confirmTitle
settingspage.clones.delete.confirmDesc
settingspage.clones.socks.title               SOCKS 节点管理
settingspage.clones.socks.add
settingspage.clones.socks.urlPlaceholder
settingspage.clones.socks.userPlaceholder
settingspage.clones.socks.passPlaceholder
settingspage.clones.socks.remove
settingspage.clones.socks.assign              绑定到分身
settingspage.clones.socks.unassigned
settingspage.clones.balance.shared            「同账号共享余额」提示
settingspage.clones.balance.asOf              「截至 {{time}}」
settingspage.toast.cloneCreated
settingspage.toast.cloneCreateFailed
settingspage.toast.cloneDeleted
settingspage.toast.cloneDeletePartial         批量删部分失败({{ok}}/{{total}})
settingspage.toast.tagSaved
settingspage.toast.tagSaveFailed
settingspage.toast.socksSaved
settingspage.toast.socksSaveFailed
```
另需 shield 与推号开关键（若同页）：`settingspage.shield.enabled.label/hint`、`settingspage.pushKeys.enabled.label/hint`、`settingspage.pushKeys.autoClone.label/hint`（后者 hint 须写明**默认关闭、开启会改变上号协议**）。

## 未能确认（超出本次只读范围）

- `getCredentials()` 的确切后端路径（`:74` 行体未读，只读了签名）
- `admin-ui/src/types/api.ts` 中 `CredentialStatusItem` / `BalanceResponse` / `AddCredentialRequest` 的完整字段（分身页需要 `tag` 新字段落在这里，且**两份独立定义**——`types/api.ts` 与 `api/ops.ts`——按项目事实需同步）
- settings-page 底部保存栏与 `handleSave`/`dirty` 的完整实现（只确认了 `:2447-2453` 的按钮结构，足够判定"动作型 UI 不应进 diff"）