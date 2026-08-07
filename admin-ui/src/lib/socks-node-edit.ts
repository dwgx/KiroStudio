import type { SocksNode, SocksNodeUpsertRequest } from '@/types/api'

/**
 * 代理节点「编辑」表单 → `POST /socks/nodes` 请求体（纯函数，可被 `tests/` 直接跑）。
 *
 * # 为什么这段必须是独立的纯函数
 *
 * 后端 `upsert_socks_node` 对 name/username/password 三个键是**三态语义**：
 * 省略 = 不改、`""` = 清空、有值 = 设为该值。而前端最自然的写法（把表单当前值
 * 整体回填）恰好会踩两个坑，且**两个坑都不报错**：
 *
 * 1. **抹密码**：`password` 无法回填（后端对外视图 `SocksNodeView` 恒不外传密码，
 *    只给 `hasPassword`）⇒ 回填出来必然是空串 ⇒ 「改个节点名」就把密码清空，
 *    已绑该节点的分身全部因代理认证失败掉线，表现为「节点突然不通」。
 * 2. **吃掉分享链接自带的账密**：粘一条新的 `socks://base64(user:pass)@host#name`
 *    进来时，后端只在 `req.username` / `req.password` **省略**的情况下才采用链接里
 *    拆出来的账密（`service.rs` 里有专门注释）。若前端把旧 username 一并发过去，
 *    新链接的用户名会被旧值盖掉、密码却换成了新的 ⇒ 半新半旧的组合，必然认证失败。
 *    `name` 同理：显式 `req.name` 优先于链接的 `#fragment`，回填旧名字就等于
 *    永远读不到新链接里的名称。
 *
 * 结论：**只发用户真正改过的键**。这条规则靠人眼维护迟早失守，故抽成纯函数 + 测试。
 */

/** 编辑表单的当前值。`password` 与 node 上的字段不对称是刻意的 —— 它无法回填。 */
export interface SocksNodeEditForm {
  /** 地址输入框。空串视为"没填"，由调用方拦下（`url` 是后端必填）。 */
  url: string
  /** 名称输入框，初值 = `node.name`。 */
  name: string
  /** 用户名输入框，初值 = `node.username ?? ''`。 */
  username: string
  /** 新密码。**永远从空开始**（后端不外传密码），空 = 不改。 */
  password: string
  /** 「清除密码」勾选：显式把密码清空（发 `""`）。与 `password` 互斥，勾了以它为准。 */
  clearPassword: boolean
}

/** 该表单是否有任何改动（全没改时调用方可以直接收起编辑态，不打后端）。 */
export function hasSocksNodeEdits(node: SocksNode, form: SocksNodeEditForm): boolean {
  const payload = buildSocksNodeEditPayload(node, form)
  // payload 恒含 id 与 url 两个键（url 是后端必填），故 >2 才说明真有改动；
  // 只有 url 变了的情况由下面这条单独判。
  return Object.keys(payload).length > 2 || payload.url !== node.url
}

/**
 * 构造更新请求体：`{id, url}` 必带，其余键**仅在用户改过时**出现。
 *
 * 调用方须先保证 `form.url` 非空（`url` 在后端是必填，空串会被拒）。
 */
export function buildSocksNodeEditPayload(
  node: SocksNode,
  form: SocksNodeEditForm,
): SocksNodeUpsertRequest {
  // id 必带 —— 这正是「更新」而非「新建」的判据（后端 `req.id = Some(存在)` → 更新，
  // `Some(不存在)` → NotFound，刻意不静默新建）。
  const payload: SocksNodeUpsertRequest = { id: node.id, url: form.url.trim() }

  const name = form.name.trim()
  // 与原值相同就省略：显式 name 会压过分享链接的 `#fragment`，回填等于永久屏蔽链接里的名称。
  if (name !== node.name) payload.name = name

  const username = form.username.trim()
  // 同理省略：省略时后端才会采用新分享链接里拆出的用户名。
  // 改成空串是**有效意图**（清掉用户名），所以这里比的是"变没变"而不是"是不是空"。
  if (username !== (node.username ?? '')) payload.username = username

  // 密码三态：勾了「清除」→ `""`；填了新密码 → 该值；两者都没有 → **不发这个键**。
  // 绝不为了"补齐字段"发空串 —— 那就是上面第 1 条坑。
  if (form.clearPassword) {
    payload.password = ''
  } else if (form.password.length > 0) {
    payload.password = form.password
  }

  return payload
}
