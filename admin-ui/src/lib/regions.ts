// AWS 区域列表 + 中文名 + 中英文搜索关键词。
// 手写维护，不引入任何 i18n / 地区数据库依赖。
// region-select 组件与各处 regionLabel() 展示共用此数据源。

export interface AwsRegion {
  /** 区域代码，如 us-east-1 */
  code: string
  /** 中文名，如 美国东部（弗吉尼亚北部） */
  label: string
  /** 城市/地理英文名，用于英文关键词匹配（tokyo / virginia 等） */
  city: string
  /** 额外搜索别名（中英文关键词），全部小写 */
  keywords: string[]
}

// 覆盖常用商用区域；AWS 新区可由用户自由输入（region-select 允许非列表值）。
export const AWS_REGIONS: AwsRegion[] = [
  { code: 'us-east-1', label: '美国东部（弗吉尼亚北部）', city: 'N. Virginia', keywords: ['virginia', '弗吉尼亚', '美东', '美国东部'] },
  { code: 'us-east-2', label: '美国东部（俄亥俄）', city: 'Ohio', keywords: ['ohio', '俄亥俄', '美东'] },
  { code: 'us-west-1', label: '美国西部（加利福尼亚北部）', city: 'N. California', keywords: ['california', '加利福尼亚', '加州', '美西', '美国西部'] },
  { code: 'us-west-2', label: '美国西部（俄勒冈）', city: 'Oregon', keywords: ['oregon', '俄勒冈', '美西', '美国西部'] },
  { code: 'ca-central-1', label: '加拿大（中部）', city: 'Central', keywords: ['canada', '加拿大', '蒙特利尔', 'montreal'] },
  { code: 'sa-east-1', label: '南美洲（圣保罗）', city: 'São Paulo', keywords: ['sao paulo', '圣保罗', '巴西', 'brazil', '南美'] },
  { code: 'eu-west-1', label: '欧洲（爱尔兰）', city: 'Ireland', keywords: ['ireland', '爱尔兰', '都柏林', 'dublin', '欧洲'] },
  { code: 'eu-west-2', label: '欧洲（伦敦）', city: 'London', keywords: ['london', '伦敦', '英国', 'uk', '欧洲'] },
  { code: 'eu-west-3', label: '欧洲（巴黎）', city: 'Paris', keywords: ['paris', '巴黎', '法国', 'france', '欧洲'] },
  { code: 'eu-central-1', label: '欧洲（法兰克福）', city: 'Frankfurt', keywords: ['frankfurt', '法兰克福', '德国', 'germany', '欧洲'] },
  { code: 'eu-north-1', label: '欧洲（斯德哥尔摩）', city: 'Stockholm', keywords: ['stockholm', '斯德哥尔摩', '瑞典', 'sweden', '欧洲'] },
  { code: 'eu-south-1', label: '欧洲（米兰）', city: 'Milan', keywords: ['milan', '米兰', '意大利', 'italy', '欧洲'] },
  { code: 'ap-east-1', label: '亚太（香港）', city: 'Hong Kong', keywords: ['hong kong', '香港', 'hk', '亚太'] },
  { code: 'ap-northeast-1', label: '亚太（东京）', city: 'Tokyo', keywords: ['tokyo', '东京', '日本', 'japan', '亚太'] },
  { code: 'ap-northeast-2', label: '亚太（首尔）', city: 'Seoul', keywords: ['seoul', '首尔', '韩国', 'korea', '亚太'] },
  { code: 'ap-northeast-3', label: '亚太（大阪）', city: 'Osaka', keywords: ['osaka', '大阪', '日本', 'japan', '亚太'] },
  { code: 'ap-southeast-1', label: '亚太（新加坡）', city: 'Singapore', keywords: ['singapore', '新加坡', '亚太'] },
  { code: 'ap-southeast-2', label: '亚太（悉尼）', city: 'Sydney', keywords: ['sydney', '悉尼', '澳大利亚', 'australia', '亚太'] },
  { code: 'ap-southeast-3', label: '亚太（雅加达）', city: 'Jakarta', keywords: ['jakarta', '雅加达', '印尼', 'indonesia', '亚太'] },
  { code: 'ap-south-1', label: '亚太（孟买）', city: 'Mumbai', keywords: ['mumbai', '孟买', '印度', 'india', '亚太'] },
  { code: 'me-south-1', label: '中东（巴林）', city: 'Bahrain', keywords: ['bahrain', '巴林', '中东', 'middle east'] },
  { code: 'af-south-1', label: '非洲（开普敦）', city: 'Cape Town', keywords: ['cape town', '开普敦', '南非', 'africa', '非洲'] },
]

const REGION_MAP = new Map<string, AwsRegion>(AWS_REGIONS.map((r) => [r.code, r]))

/** code -> 中文名；未知 code 原样返回，便于兼容 AWS 新区。 */
export function regionLabel(code: string | null | undefined): string {
  if (!code) return '未设置'
  return REGION_MAP.get(code)?.label ?? code
}

/** code -> "中文名 · code"，用于既要中文又要保留原始代码的展示位。 */
export function regionLabelWithCode(code: string | null | undefined): string {
  if (!code) return '未设置'
  const r = REGION_MAP.get(code)
  return r ? `${r.label}（${r.code}）` : code
}

/** 关键词过滤：匹配 code / 中文名 / 城市英文名 / 别名，全部大小写不敏感。 */
export function filterRegions(query: string): AwsRegion[] {
  const q = query.trim().toLowerCase()
  if (!q) return AWS_REGIONS
  return AWS_REGIONS.filter((r) => {
    if (r.code.toLowerCase().includes(q)) return true
    if (r.label.toLowerCase().includes(q)) return true
    if (r.city.toLowerCase().includes(q)) return true
    return r.keywords.some((k) => k.includes(q))
  })
}

// ============ 最近使用区域（智能复用，全局共享） ============
// 三处 region 选择器（设置页 / IdC 上号 / 微软 SSO / 凭据卡片自定义切换）共享同一份历史，
// 让「复用过去填过的 region」在任何入口都即时可选。存 localStorage，跨会话保留。

const RECENT_REGIONS_KEY = 'kirostudio.recentRegions'
const RECENT_REGIONS_MAX = 5

// region code 的宽松形状校验：如 us-east-1 / eu-central-1 / ap-northeast-3 / us-gov-east-1。
// 只做形状过滤（防脏值/注入进历史），不校验 AWS 是否真存在——AWS 新区也应能记录复用。
const REGION_CODE_SHAPE = /^[a-z]{2}(-[a-z]+)+-\d+$/

/** 形状校验：输入是否长得像一个 AWS region code（区分「真 region」与「搜索关键词」）。 */
export function isRegionCodeShape(code: string): boolean {
  return REGION_CODE_SHAPE.test(code)
}

/** 读取最近使用区域 code 列表（最新在前）。脏数据/坏 JSON 一律返回空数组。 */
export function getRecentRegions(): string[] {
  try {
    const raw = localStorage.getItem(RECENT_REGIONS_KEY)
    if (!raw) return []
    const arr = JSON.parse(raw)
    if (!Array.isArray(arr)) return []
    // 二次防御：只保留形状合法且去重的字符串。
    const seen = new Set<string>()
    const out: string[] = []
    for (const v of arr) {
      if (typeof v !== 'string') continue
      const c = v.trim().toLowerCase()
      if (!isRegionCodeShape(c) || seen.has(c)) continue
      seen.add(c)
      out.push(c)
      if (out.length >= RECENT_REGIONS_MAX) break
    }
    return out
  } catch {
    return []
  }
}

/**
 * 记录一个刚被采用的 region 到历史（去重 + 最新置顶 + 上限 N 条）。
 * 只接受形状合法的 code（防脏值污染历史）。localStorage 写失败静默忽略（隐私模式等）。
 */
export function pushRecentRegion(code: string | null | undefined): void {
  if (!code) return
  const c = code.trim().toLowerCase()
  if (!isRegionCodeShape(c)) return
  try {
    const prev = getRecentRegions().filter((r) => r !== c)
    const next = [c, ...prev].slice(0, RECENT_REGIONS_MAX)
    localStorage.setItem(RECENT_REGIONS_KEY, JSON.stringify(next))
  } catch {
    // 忽略：隐私模式 / 存储配额满时不影响功能。
  }
}

/** 清空「最近使用」历史（用户主动清理）。localStorage 写失败静默忽略（隐私模式等）。 */
export function clearRecentRegions(): void {
  try {
    localStorage.removeItem(RECENT_REGIONS_KEY)
  } catch {
    // 忽略：隐私模式 / 存储不可用时不影响功能。
  }
}
