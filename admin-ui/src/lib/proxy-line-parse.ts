/**
 * 代理节点行解析 —— **仅用于粘贴后的本地预览**。
 *
 * # 🔴 为什么这里有一份和后端一样的解析逻辑（不是重复代码的坏味道）
 *
 * 这是**信任边界**，不是复用不足：
 *
 * - **后端是权威。** 落库、SSRF 校验、节点数上限、去重全部只以
 *   `src/http_client.rs` 的判定为准。这一份的结论若与后端不一致，
 *   以后端为准 —— 前端解析出的东西**从不**被直接写进任何持久化状态。
 * - 前端这份存在的唯一理由是**在打后端之前就把话说清楚**：用户粘 10 行进来，
 *   要立刻看到「哪几行能进、哪几行是重复、哪几行格式不对以及为什么」，
 *   而不是点了导入再收到一句「跳过 10 行非链接文本」。
 * - 因此绝不能把它做成「先在前端过滤一遍再发给后端」：那会让前端的判定
 *   变成事实上的准入控制，而它是**不可信的**（浏览器里的代码用户可改）。
 *   导入时发的仍是用户勾选行的**原文**，由后端重新解析。
 *
 * 两边的判据描述共用同一套（见下方各函数注释与 `src/http_client.rs`
 * 的 `normalize_proxy_line` / `parse_proxy_link_strict` / `classify_proxy_line`）。
 * 改任一侧的判据都必须同步改另一侧，并让 `proxy-line-parse.test.ts`
 * 与 Rust 侧 `colon_form_*` 那批测试都过。
 */

/** 一行为什么没能解析出节点。与后端 `ProxyLineIssue::code()` 逐字对应。 */
export type ProxyLineIssue = 'not_proxy' | 'no_host_port' | 'bad_host' | 'bad_port' | 'ambiguous'

/** 一条解析成功的代理链接。 */
export interface ParsedProxyLink {
  /** 干净 URL（`scheme://host:port`，已剥 userinfo 与 `#fragment`）。 */
  url: string
  username?: string
  password?: string
  /** `#` 之后的展示名。 */
  name?: string
}

/** 单行分类结果。 */
export type ProxyLineVerdict =
  | { kind: 'parsed'; link: ParsedProxyLink }
  | { kind: 'skipped' }
  | { kind: 'invalid'; issue: ProxyLineIssue }

/** 归一化的三种结局。与后端 `NormalizedLine` 对应。 */
export type NormalizedLine =
  | { kind: 'rewritten'; text: string }
  | { kind: 'asIs' }
  | { kind: 'rejected'; issue: ProxyLineIssue }

/** 预览里的一行。 */
export interface ProxyLinePreviewItem {
  /** 原始行号（1 起，与用户粘的文本对齐）。 */
  lineno: number
  /** 该行原文（**密码已脱敏**）。无法识别的行靠它让用户看出是格式问题还是脏数据。 */
  raw: string
  status: 'ok' | 'duplicate' | 'invalid'
  /** 解析失败原因；`status==='duplicate'` 时为 undefined。 */
  issue?: ProxyLineIssue
  /** 重复的来源：已在池中 / 同一次粘贴内更靠前的行。 */
  dupOf?: 'pool' | 'paste'
  /** 解析出的 `scheme://host:port`。 */
  address?: string
  /** 解析出的用户名。**密码恒不进预览。** */
  username?: string
}

/** 整段文本的预览结果。 */
export interface ProxyLinesPreview {
  items: ProxyLinePreviewItem[]
  /** 安静跳过的行数（空行/注释/标题/说明文字）。不进 `items`。 */
  skipped: number
  okCount: number
  duplicateCount: number
  invalidCount: number
}

/** 这一段是否是合法端口（纯数字且 1..=65535）。对齐后端 `is_port_seg`。 */
function isPortSeg(s: string): boolean {
  if (!s || !/^[0-9]+$/.test(s)) return false
  const n = Number(s)
  return n >= 1 && n <= 65535
}

/**
 * 一段文本是否**像 host 字面量**（IPv4 / 含点域名 / `[IPv6]`）。对齐后端
 * `looks_like_host_literal`。
 *
 * **要求含 `.` 或方括号**是承重的：没有它，`端口:40002:说明:文本` 这类文档行会被
 * 当成 host="端口" 的节点数据。单标签主机名（`myproxy:1080:u:p`）因此不被冒号形态接受。
 */
function looksLikeHostLiteral(s: string): boolean {
  if (s.startsWith('[') && s.endsWith(']') && s.length > 2) {
    const inner = s.slice(1, -1)
    return /^[0-9a-fA-F:.]+$/.test(inner)
  }
  return s.includes('.') && /^[0-9a-zA-Z._-]+$/.test(s)
}

/**
 * 按 `:` 切段并带上每段在原串中的起始偏移，但把 `[...]` 里的 IPv6 冒号当成整体。
 *
 * 偏移是必须的：读法 A 的密码要吃掉「第 3 个 `:` 之后的全部余下」，
 * 而普通 `split(':')` 不认方括号，会把 `[2001:db8::1]:1080:u:p` 切坏。
 */
function splitColonSegmentsIndexed(s: string): Array<{ at: number; seg: string }> {
  const out: Array<{ at: number; seg: string }> = []
  let start = 0
  let depth = 0
  for (let i = 0; i < s.length; i++) {
    const c = s[i]
    if (c === '[') depth += 1
    else if (c === ']') depth -= 1
    else if (c === ':' && depth <= 0) {
      out.push({ at: start, seg: s.slice(start, i) })
      start = i + 1
    }
  }
  out.push({ at: start, seg: s.slice(start) })
  return out
}

function splitColonSegments(s: string): string[] {
  return splitColonSegmentsIndexed(s).map((x) => x.seg)
}

/** 是否以 `scheme://` 开头。对齐后端 `starts_with_scheme`。 */
function startsWithScheme(s: string): boolean {
  const i = s.indexOf('://')
  if (i <= 0) return false
  const scheme = s.slice(0, i)
  return /^[a-zA-Z][a-zA-Z0-9+.-]*$/.test(scheme)
}

/**
 * 宽松 base64 解码 → UTF-8 字符串。同时接受标准表（`+/`）与 URL-safe 表（`-_`）。
 *
 * 不用 `atob`：它对 URL-safe 表与缺 padding 会直接抛，而后端是**手写查表**接受两者。
 * 两边判据必须一致，否则预览会说「解析成功」而后端拒掉（或反之）。
 */
function base64DecodeLoose(input: string): string | null {
  const table = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789'
  const val = (c: string): number => {
    const i = table.indexOf(c)
    if (i >= 0) return i
    if (c === '+' || c === '-') return 62
    if (c === '/' || c === '_') return 63
    return -1
  }
  const chars = [...input].filter((c) => c !== '=' && !/\s/.test(c))
  const bytes: number[] = []
  for (let i = 0; i < chars.length; i += 4) {
    const chunk = chars.slice(i, i + 4)
    const buf = [0, 0, 0, 0]
    for (let j = 0; j < chunk.length; j++) {
      const v = val(chunk[j])
      if (v < 0) return null
      buf[j] = v
    }
    const n = chunk.length
    bytes.push(((buf[0] << 2) | (buf[1] >> 4)) & 0xff)
    if (n > 2) bytes.push(((buf[1] << 4) | (buf[2] >> 2)) & 0xff)
    if (n > 3) bytes.push(((buf[2] << 6) | buf[3]) & 0xff)
  }
  try {
    // fatal:true ⇒ 非 UTF-8 序列返回 null，与后端 `String::from_utf8(..).ok()` 同口径。
    return new TextDecoder('utf-8', { fatal: true }).decode(new Uint8Array(bytes))
  } catch {
    return null
  }
}

/** 百分号解码；失败保持原样（对齐后端 `urlencoding::decode(..).unwrap_or_else(原样)`）。 */
function pctDecode(s: string): string {
  try {
    return decodeURIComponent(s)
  } catch {
    return s
  }
}

/**
 * 严格核心：**不做**任何冒号形态归一，只认 `[scheme://][userinfo@]host:port[#name]`。
 * 逐条对齐后端 `parse_proxy_link_strict`。
 *
 * # 判据顺序（承重）
 *
 * 1. 先剥 `#fragment` —— 否则它会污染 host（`40002#US-1` 不是合法端口）。
 * 2. userinfo 含 `:` ⇒ 当**明文** `user:pass`（不试 base64）。
 *    理由：明文密码完全可能恰好是合法 base64，先试 base64 会把明文解成乱码。
 * 3. 不含 `:` 且能 base64 解出含 `:` 的 UTF-8 ⇒ 当 base64。
 * 4. 都不满足 ⇒ 当纯用户名无密码。
 */
export function parseProxyLinkStrict(
  raw: string
): { ok: true; link: ParsedProxyLink } | { ok: false; issue: ProxyLineIssue } {
  const trimmed = raw.trim()
  if (!trimmed || trimmed.startsWith('#')) return { ok: false, issue: 'not_proxy' }

  // ① 先剥 fragment（必须最先做，见判据 1）
  let body = trimmed
  let name: string | undefined
  const hash = trimmed.indexOf('#')
  if (hash >= 0) {
    body = trimmed.slice(0, hash)
    const n = trimmed.slice(hash + 1).trim()
    name = n || undefined
  }

  let scheme: string
  let rest: string
  const sep = body.indexOf('://')
  if (sep >= 0) {
    scheme = body.slice(0, sep).toLowerCase()
    rest = body.slice(sep + 3)
  } else {
    // 无 scheme：视为 socks5（节点明细里常见裸 host:port）
    scheme = 'socks5'
    rest = body
  }
  // `socks://` 是分享链接惯例，reqwest 只认 socks5/socks5h。其余 scheme 原样保留。
  if (scheme === 'socks') scheme = 'socks5'

  // host 段不含 '@'，故 userinfo 与 host 的分隔符是**最后一个** '@'。
  let userinfo: string | undefined
  let hostportRaw = rest
  const at = rest.lastIndexOf('@')
  if (at >= 0) {
    userinfo = rest.slice(0, at)
    hostportRaw = rest.slice(at + 1)
  }

  hostportRaw = hostportRaw.trim()
  const colon = hostportRaw.lastIndexOf(':')
  if (colon < 0) return { ok: false, issue: 'no_host_port' }
  const host = hostportRaw.slice(0, colon).trim()
  const port = hostportRaw.slice(colon + 1).trim()

  // host 字符集：字母数字 + `.` `-` `_`，或 IPv6 的 `[::1]` 形式。
  // ⚠️ 光判「非空 + 无空白」不够：`端口  : 40002` 被 trim 后 host="端口"，
  // 非空且无空白 ⇒ 会通过。必须按字符集判，否则说明行会造出假节点。
  const hostOk =
    host.startsWith('[') && host.endsWith(']')
      ? host.length > 2 && /^[0-9a-fA-F:.]+$/.test(host.slice(1, -1))
      : /^[0-9a-zA-Z._-]+$/.test(host)
  if (!hostOk) return { ok: false, issue: 'bad_host' }
  if (!isPortSeg(port)) return { ok: false, issue: 'bad_port' }

  let username: string | undefined
  let password: string | undefined
  const ui = userinfo?.trim()
  if (ui) {
    const c = ui.indexOf(':')
    if (c >= 0) {
      // 判据 2：明文 user:pass
      username = pctDecode(ui.slice(0, c)) || undefined
      password = pctDecode(ui.slice(c + 1)) || undefined
    } else {
      // 判据 3：尝试 base64（补 padding；标准表与 URL-safe 表都接受）
      let padded = ui
      while (padded.length % 4 !== 0) padded += '='
      const decoded = base64DecodeLoose(padded)
      if (decoded && decoded.includes(':')) {
        const k = decoded.indexOf(':')
        username = decoded.slice(0, k) || undefined
        password = decoded.slice(k + 1) || undefined
      } else {
        // 判据 4：当纯用户名
        username = ui
      }
    }
  }

  return { ok: true, link: { url: `${scheme}://${host}:${port}`, username, password, name } }
}

/**
 * 把代理商常见的**非规范**写法重写成严格解析认得的规范形态。
 * 逐条对齐后端 `normalize_proxy_line`。
 *
 * | 形态 | 判据 | 例 |
 * |---|---|---|
 * | `host:port:user:pass` | 第 2 段是合法端口 | `130.180.228.34:6318:u:p` |
 * | `user:pass:host:port` | 第 4 段是合法端口（**恰好 4 段**） | `u:p:130.180.228.34:6318` |
 * | `host:port@user:pass` | `@` 左侧是合法 host:port 且右侧含 `:` | `1.2.3.4:1080@u:p` |
 *
 * # 🔴 消歧判据（按优先级，第一条命中即定，**绝不猜**）
 *
 * 1. 只有一种读法的那段是合法端口 ⇒ 采用该读法。
 * 2. 两段都是合法端口 ⇒ 看哪个 host 候选像 host 字面量；恰好一个像 ⇒ 采用它。
 * 3. 两条都判不定（如 `1.2.3.4:8080:5.6.7.8:9090`）⇒ `rejected('ambiguous')`。
 *
 * 为什么不许猜：失败模式若是**静默跳过**，用户明确知道要改格式；猜错则变成
 * **造出一个假节点**，表现为「节点不通」，要翻代理日志才能定位，难查得多。
 *
 * 5 段及以上只允许读法 1，密码取第 3 个 `:` 之后的**全部余下**
 * （`host:port:user:pa:ss` ⇒ 密码 `pa:ss`）。裸 IPv6 一律不接，必须写成 `[2001:db8::1]:1080:u:p`。
 */
export function normalizeProxyLine(raw: string): NormalizedLine {
  const trimmed = raw.trim()
  if (!trimmed || trimmed.startsWith('#')) return { kind: 'asIs' }

  // ① fragment 必须最先剥（与严格解析判据 1 同序）：否则
  // `host:port:user:pass#名字` 的 `#名字` 会黏在密码上。
  let body = trimmed
  let frag: string | undefined
  const hash = trimmed.indexOf('#')
  if (hash >= 0) {
    body = trimmed.slice(0, hash).trim()
    frag = trimmed.slice(hash + 1)
  }
  // ② scheme 也先剥：`socks5://1.2.3.4:1080:u:p` 里的 `//` 会干扰数段。
  let schemePrefix = ''
  let rest = body
  const sep = body.indexOf('://')
  if (sep >= 0) {
    schemePrefix = body.slice(0, sep + 3)
    rest = body.slice(sep + 3)
  }

  const rebuild = (user: string, pass: string, host: string, port: string): NormalizedLine => {
    let s = `${schemePrefix}${user}:${pass}@${host}:${port}`
    if (frag !== undefined) s += `#${frag}`
    return { kind: 'rewritten', text: s }
  }

  // ③ 已含 `@` ⇒ 本就是规范形态，**不走冒号形态**。
  //    这道先手是承重的：`user:p:ss@host:1080`（密码含 `:`）也有 4 段，
  //    若先数冒号就会把它重写坏 —— 而它现在由严格解析的判据 2 正确处理。
  const at = rest.lastIndexOf('@')
  if (at >= 0) {
    const left = rest.slice(0, at)
    const right = rest.slice(at + 1)
    // 唯一例外：`host:port@user:pass` 倒装。仅当标准读法解不出（`right` 不是合法
    // host:port）且左侧确实是 host:port 时才交换，故 `1.2.3.4:1080@5.6.7.8:8080`
    // （两侧都合法）保持标准读法不变。
    if (!parseProxyLinkStrict(right).ok && right.includes(':')) {
      const l = splitColonSegments(left)
      if (l.length === 2 && looksLikeHostLiteral(l[0]) && isPortSeg(l[1])) {
        const k = right.indexOf(':')
        return rebuild(right.slice(0, k), right.slice(k + 1), l[0], l[1])
      }
    }
    return { kind: 'asIs' }
  }

  const indexed = splitColonSegmentsIndexed(rest)
  const segs = indexed.map((x) => x.seg)
  // 少于 4 段（`host:port` / `host:port:user`）维持原有行为：前者严格解析接受，
  // 后者被拒。放宽它会让 `port : 40002` 这类说明行造出假节点。
  if (segs.length < 4) return { kind: 'asIs' }

  // 读法 A：host:port:user:pass（≥4 段都可，密码吃掉余下全部冒号）
  const aOk = isPortSeg(segs[1]) && looksLikeHostLiteral(segs[0])
  // 读法 B：user:pass:host:port（**仅**恰好 4 段）
  const bOk = segs.length === 4 && isPortSeg(segs[3]) && looksLikeHostLiteral(segs[2])

  if (aOk && bOk) {
    // 判据 3：两读法都成立 ⇒ 判不定。两个 host 候选都已过 looksLikeHostLiteral，
    // 所以无从区分。
    return { kind: 'rejected', issue: 'ambiguous' }
  }
  if (!aOk && !bOk) {
    // 不是冒号形态（中文说明行、时间戳、裸 IPv6）⇒ 原样交给严格解析。
    return { kind: 'asIs' }
  }
  if (aOk) {
    // 密码 = 第 4 段起的**全部余下**（`host:port:user:pa:ss` ⇒ pass=`pa:ss`）。
    return rebuild(segs[2], rest.slice(indexed[3].at), segs[0], segs[1])
  }
  return rebuild(segs[0], segs[1], segs[2], segs[3])
}

/**
 * 一行 → 解析成功 / 安静跳过 / 报错。逐条对齐后端 `classify_proxy_line`。
 *
 * # 🔴 判据顺序（承重，改序即回归）
 *
 * 「像不像链接」的闸门必须排在**归一化之后**。后端原先它是
 * `contains("://") && contains('@')` 且排在解析器**之前**，于是纯冒号形态
 * （一个 `://` 和 `@` 都没有）根本走不到解析器 —— 那正是「10 行 0 条成功」的真因。
 * 只改解析器而不动这道闸门 ⇒ 纯函数测试全绿而功能依然不通。
 */
export function classifyProxyLine(line: string): ProxyLineVerdict {
  const t = line.trim()
  if (!t || t.startsWith('#')) return { kind: 'skipped' }
  // 行内常带引号/反引号（文档里的代码块残留）。
  const cleaned = t.replace(/^[`"'\s]+/, '').replace(/[`"'\s]+$/, '')

  const norm = normalizeProxyLine(cleaned)
  if (norm.kind === 'rewritten') {
    const r = parseProxyLinkStrict(norm.text)
    // 归一化成功说明它确实是代理形态，此时的失败必须报出来。
    return r.ok ? { kind: 'parsed', link: r.link } : { kind: 'invalid', issue: r.issue }
  }
  if (norm.kind === 'rejected') return { kind: 'invalid', issue: norm.issue }

  const parsed = parseProxyLinkStrict(cleaned)
  const segs = splitColonSegments(cleaned)
  const looksLikeLink =
    startsWithScheme(cleaned) ||
    (cleaned.includes('@') && parsed.ok) ||
    // 冒号形态但端口/host 有一处不合法（`1.2.3.4:0:u:p`）：第一段像 host 字面量
    // 就当代理数据看，报错而不是静默跳过。
    (segs.length >= 4 && looksLikeHostLiteral(segs[0]))
  if (parsed.ok) {
    // 裸 `host:port` 单条 API 接受、批量**维持拒绝**：放宽会让
    // `port : 40002` 这类英文说明行造出 host="port" 的假节点。
    return looksLikeLink ? { kind: 'parsed', link: parsed.link } : { kind: 'skipped' }
  }
  return looksLikeLink ? { kind: 'invalid', issue: parsed.issue } : { kind: 'skipped' }
}

/**
 * 单行原文脱敏：把已识别出的密码与 base64 userinfo 换成 `***`，并截断到 200 字符。
 * 对齐后端 `mask_proxy_line`。
 *
 * 为什么仍回显原文：失败行的**形状**就是诊断信息本身（用户要判断是自己格式写错了
 * 还是数据脏了）。而密码不能显示 —— 面板会被投屏/截图，且这段文本会进 DOM。
 */
export function maskProxyLine(line: string): string {
  const t = line.trim()
  let out = t
  const v = classifyProxyLine(t)
  if (v.kind === 'parsed' && v.link.password) {
    out = out.split(v.link.password).join('***')
  }
  // base64 userinfo：密码在编码里，上面的替换抓不到 ⇒ 整段 userinfo 打掉。
  const at = out.lastIndexOf('@')
  if (at >= 0) {
    const head = out.slice(0, at)
    const tail = out.slice(at + 1)
    const s = head.lastIndexOf('://')
    const ui = s >= 0 ? head.slice(s + 3) : head
    if (ui && !ui.includes(':')) {
      out = `${head.slice(0, head.length - ui.length)}***@${tail}`
    }
  }
  const MAX = 200
  const chars = [...out]
  if (chars.length > MAX) out = chars.slice(0, MAX).join('') + '…'
  return out
}

/**
 * 整段文本 → 逐行预览。
 *
 * `poolUrls` 是**已在池中**的节点地址集合（`scheme://host:port`），用于把「已存在」
 * 与「同一次粘贴内重复」都标成 duplicate —— 对用户是同一件事（这条不会新增节点），
 * 但 `dupOf` 区分开来，因为两者的处置不同：前者要去看已有节点，后者是粘贴里有冗余。
 *
 * 安静跳过的行（标题/分隔线/说明文字）**不进 `items`**，只计入 `skipped`：
 * 一份节点商文档里那类行有几十条，全列出来会把真正要看的几行埋掉。
 *
 * 🔴 结论仅供展示。落库以后端为准（见文件头注释）。
 */
/**
 * 按 Rust `str::lines()` 的语义切行：剥 `\r`，且**末尾换行不产生空行**。
 *
 * 不能直接用 `text.split('\n')`：粘贴的文本几乎总以换行结尾，split 会多出一个空串，
 * 于是「跳过 N 行」永远比后端多 1 —— 两边数字不一致会让用户以为有一行没被处理。
 * （这条差异是被 `tests/proxy-line-parse.test.ts` 抓出来的，不是推理出来的。）
 *
 * 导出给界面用：`ProxyLinePreviewItem.raw` 是**脱敏后**的，不能拿去发请求；
 * 界面要按 `lineno` 从这里取**原文**再发给后端（`lineno` 是 1 起的下标）。
 */
export function splitProxyTextLines(text: string): string[] {
  const parts = text.split('\n').map((l) => (l.endsWith('\r') ? l.slice(0, -1) : l))
  if (parts.length > 0 && parts[parts.length - 1] === '') parts.pop()
  return parts
}

export function previewProxyLines(text: string, poolUrls: Iterable<string> = []): ProxyLinesPreview {
  const pool = new Set(poolUrls)
  const seen = new Set<string>()
  const items: ProxyLinePreviewItem[] = []
  let skipped = 0
  let okCount = 0
  let duplicateCount = 0
  let invalidCount = 0

  splitProxyTextLines(text).forEach((line, idx) => {
    const lineno = idx + 1
    const v = classifyProxyLine(line)
    if (v.kind === 'skipped') {
      skipped += 1
      return
    }
    if (v.kind === 'invalid') {
      invalidCount += 1
      items.push({ lineno, raw: maskProxyLine(line), status: 'invalid', issue: v.issue })
      return
    }
    const url = v.link.url
    const dupOf: 'pool' | 'paste' | undefined = pool.has(url)
      ? 'pool'
      : seen.has(url)
        ? 'paste'
        : undefined
    if (!dupOf) seen.add(url)
    if (dupOf) duplicateCount += 1
    else okCount += 1
    items.push({
      lineno,
      raw: maskProxyLine(line),
      status: dupOf ? 'duplicate' : 'ok',
      dupOf,
      address: url,
      username: v.link.username,
    })
  })

  return { items, skipped, okCount, duplicateCount, invalidCount }
}



