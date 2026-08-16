import { useState } from 'react'
import { useTranslation } from 'react-i18next'
import axios from 'axios'
import {
  ArrowLeft,
  BookOpen,
  ChevronRight,
  Compass,
  ExternalLink,
  Globe,
  Loader2,
  Network,
  Search,
  SearchX,
} from 'lucide-react'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { EmptyState } from '@/components/ui/empty-state'
import { storage } from '@/lib/storage'
import { ConnPage } from '@/components/conn-page'
// 数据文件由并行内容任务产出（契约见 /tmp/help-contract.md，接口名固定）；未落盘时运行时判空降级为加载态。
import type { Category, HelpEntry, HelpModule, HelpChainStep } from '@/data/help-knowledge'
import { HELP_ENTRIES, HELP_MODULES, HELP_CHAIN } from '@/data/help-knowledge'

// 分类中文名 + 展示顺序（固定 8 类，计数在渲染时按 HELP_ENTRIES 统计）。
const CATEGORY_ORDER: Category[] = [
  'pitfalls',
  'architecture',
  'protocol',
  'deploy',
  'faq',
  'research',
  'config',
  'security',
]
// codePath 统一渲染为 GitHub blob 链接（v1.1.0 分支，新窗口打开）。
const GITHUB_BLOB = 'https://github.com/dwgx/KiroStudio/blob/v1.1.0/'

// 联网搜索端点契约：GET /api/help/web-search?q=...，返回 [{title,url,snippet}]。
// 与其余 api 模块同款 axios 配置（baseURL + x-api-key + 15s 超时），就地声明避免扩权改 api/ 目录。
const webApi = axios.create({
  baseURL: '/api',
  timeout: 15000,
  headers: { 'Content-Type': 'application/json' },
})
webApi.interceptors.request.use((config) => {
  const apiKey = storage.getApiKey()
  if (apiKey) config.headers['x-api-key'] = apiKey
  return config
})

interface WebSearchResult {
  title: string
  url: string
  snippet: string
}

type View = 'kb' | 'map' | 'web' | 'conn'

function GitHubLink({ path }: { path: string }) {
  return (
    <a
      href={`${GITHUB_BLOB}${path}`}
      target="_blank"
      rel="noreferrer"
      className="inline-flex max-w-full items-center gap-1 text-primary hover:underline"
    >
      <code className="min-w-0 flex-1 truncate font-mono text-xs">{path}</code>
      <ExternalLink className="h-3 w-3 shrink-0" />
    </a>
  )
}

function SeverityBadge({ severity }: { severity: HelpEntry['severity'] }) {
  const { t } = useTranslation()
  const map = {
    high: { label: t('helppage.kb.severityHigh'), cls: 'border-red-500/30 bg-red-500/10 text-red-400' },
    medium: { label: t('helppage.kb.severityMedium'), cls: 'border-amber-500/30 bg-amber-500/10 text-amber-400' },
    low: { label: t('helppage.kb.severityLow'), cls: 'border-border/60 bg-secondary text-muted-foreground' },
  } as const
  const m = map[severity as keyof typeof map]
  return (
    <Badge variant="outline" className={`shrink-0 border ${m.cls}`}>
      {m.label}
    </Badge>
  )
}

/* ============ 知识库：分类过滤 + 条目列表 + 展开详情 ============ */

function KnowledgeView({
  entries,
  query,
  loading,
}: {
  entries: HelpEntry[]
  query: string
  loading: boolean
}) {
  const { t } = useTranslation()
  const [category, setCategory] = useState<Category | null>(null)
  const [expanded, setExpanded] = useState<string | null>(null)

  // 分类计数（按全部条目统计，搜索时仍展示全量分类数与计数，过滤由下方二次筛承担）。
  const counts = new Map<Category, number>()
  for (const e of entries) counts.set(e.category, (counts.get(e.category) ?? 0) + 1)

  const q = query.trim().toLowerCase()
  const filtered = entries.filter((e) => {
    if (category && e.category !== category) return false
    if (!q) return true
    const hay = `${t(e.title)} ${e.tags.join(' ')} ${t(e.problem)}`.toLowerCase()
    return hay.includes(q)
  })

  return (
    <div className="space-y-4">
      {/* 分类过滤 chips：全部 + 8 分类（中文名 + 计数） */}
      <div className="flex flex-wrap gap-1.5">
        <button
          type="button"
          onClick={() => setCategory(null)}
          className={`rounded-full px-3 py-1 text-xs font-medium transition-colors ${
            category === null
              ? 'bg-primary/20 text-primary'
              : 'bg-white/5 text-muted-foreground hover:bg-white/10'
          }`}
        >
          {t('helppage.kb.all')} ({entries.length})
        </button>
        {CATEGORY_ORDER.filter((c) => (counts.get(c) ?? 0) > 0).map((c) => (
          <button
            key={c}
            type="button"
            onClick={() => setCategory(category === c ? null : c)}
            className={`rounded-full px-3 py-1 text-xs font-medium transition-colors ${
              category === c
                ? 'bg-primary/20 text-primary'
                : 'bg-white/5 text-muted-foreground hover:bg-white/10'
            }`}
          >
            {t('helppage.category.' + c)} ({counts.get(c)})
          </button>
        ))}
      </div>

      {loading ? (
        <EmptyState icon={BookOpen} title={t('helppage.kb.loading')} description={t('helppage.kb.loadingDesc')} />
      ) : filtered.length === 0 ? (
        <EmptyState icon={SearchX} title={t('helppage.kb.noMatch')} description={t('helppage.kb.noMatchDesc')} />
      ) : (
        <div className="space-y-2">
          {filtered.map((e) => {
            const open = expanded === e.id
            return (
              <Card key={e.id} className="overflow-hidden">
                <button
                  type="button"
                  onClick={() => setExpanded(open ? null : e.id)}
                  className="flex w-full items-center justify-between gap-3 px-4 py-3 text-left transition-colors hover:bg-secondary/30"
                >
                  <div className="min-w-0">
                    <div className="text-sm font-medium">{t(e.title)}</div>
                    <div className="mt-0.5 truncate text-xs text-muted-foreground">{t(e.problem)}</div>
                  </div>
                  <div className="flex shrink-0 items-center gap-2">
                    <span className="text-[10px] text-muted-foreground">{t('helppage.category.' + e.category)}</span>
                    <SeverityBadge severity={e.severity} />
                    <ChevronRight
                      className={`h-4 w-4 text-muted-foreground transition-transform ${open ? 'rotate-90' : ''}`}
                    />
                  </div>
                </button>
                {open && (
                  <div className="space-y-3 border-t border-border/40 px-4 py-3">
                    <div>
                      <div className="text-xs font-medium text-muted-foreground">{t('helppage.kb.cause')}</div>
                      <p className="mt-0.5 text-sm leading-relaxed">{t(e.cause)}</p>
                    </div>
                    <div>
                      <div className="text-xs font-medium text-muted-foreground">{t('helppage.kb.solution')}</div>
                      <p className="mt-0.5 whitespace-pre-wrap text-sm leading-relaxed">{t(e.solution)}</p>
                    </div>
                    <div className="flex flex-wrap items-center gap-x-6 gap-y-1 text-xs">
                      <span className="text-muted-foreground">
                        {t('helppage.kb.source')} <span className="font-mono text-[#ededed]">{e.source}</span>
                      </span>
                      {e.codePath && (
                        <span className="inline-flex items-center gap-1 text-muted-foreground">
                          {t('helppage.kb.codePath')} <GitHubLink path={e.codePath} />
                        </span>
                      )}
                      <span className="ml-auto text-[10px] text-muted-foreground">{e.updatedAt}</span>
                    </div>
                  </div>
                )}
              </Card>
            )
          })}
        </div>
      )}
    </div>
  )
}

/* ============ 架构地图：请求链路 + 模块网格 ============ */

function MapView({ chain, modules, loading }: { chain: HelpChainStep[]; modules: typeof HELP_MODULES; loading: boolean }) {
  const { t } = useTranslation()
  if (loading) {
    return <EmptyState icon={Compass} title={t('helppage.kb.loading')} description={t('helppage.kb.loadingDesc')} />
  }
  return (
    <div className="space-y-8">
      {/* 请求链路：横向步骤卡 + 箭头，点击卡片看关键代码 */}
      <div>
        <div className="mb-1 text-base font-semibold">{t('helppage.map.chainTitle')}</div>
        <p className="mb-3 text-xs text-muted-foreground">{t('helppage.map.chainDesc')}</p>
        <div className="flex items-stretch gap-2 overflow-x-auto pb-2">
          {chain.map((step, i) => (
            <div key={step.id} className="flex shrink-0 items-stretch gap-2">
              <a
                href={`${GITHUB_BLOB}${step.codePath}`}
                target="_blank"
                rel="noreferrer"
                className="flex w-48 flex-col rounded-lg border border-border/60 bg-white/5 p-3 transition-colors hover:border-primary/40 hover:bg-primary/5"
              >
                <div className="flex items-center gap-1.5 text-xs font-medium text-primary">
                  <Network className="h-3.5 w-3.5" />
                  {t(step.name)}
                </div>
                <p className="mt-1 flex-1 text-[11px] leading-relaxed text-muted-foreground">{t(step.desc)}</p>
                <code className="mt-2 truncate font-mono text-[10px] text-[#888]">{step.codePath}</code>
              </a>
              {i < chain.length - 1 && (
                <ChevronRight className="h-4 w-4 shrink-0 self-center text-muted-foreground" />
              )}
            </div>
          ))}
        </div>
      </div>

      {/* 模块网格：路径 / 职责 / 关键文件 */}
      <div>
        <div className="mb-3 text-base font-semibold">{t('helppage.map.modulesTitle')}</div>
        <div className="grid gap-4 md:grid-cols-2">
          {modules.map((m: HelpModule) => (
            <Card key={m.path}>
              <CardHeader className="pb-2">
                <CardTitle className="flex items-center gap-2 text-sm">
                  <Compass className="h-4 w-4 shrink-0 text-muted-foreground" />
                  <span className="truncate font-mono">{m.path}</span>
                </CardTitle>
              </CardHeader>
              <CardContent className="space-y-2 py-3">
                <div>
                  <div className="text-xs font-medium text-muted-foreground">{t(m.name)}</div>
                  <p className="mt-0.5 text-xs leading-relaxed text-[#b0b0b0]">{t(m.role)}</p>
                </div>
                <div>
                  <div className="text-[11px] font-medium text-muted-foreground">{t('helppage.map.keyFiles')}</div>
                  <div className="mt-1 flex flex-wrap gap-1">
                    {m.keyFiles.map((f: string) => (
                      <code key={f} className="rounded bg-secondary/60 px-1.5 py-0.5 font-mono text-[10px] text-[#c9d1d9]">
                        {f}
                      </code>
                    ))}
                  </div>
                </div>
              </CardContent>
            </Card>
          ))}
        </div>
      </div>
    </div>
  )
}

/* ============ 联网搜索：后端端点 GET /api/help/web-search ============ */

function WebSearchView() {
  const { t } = useTranslation()
  const [raw, setRaw] = useState('')
  const [state, setState] = useState<'idle' | 'loading' | 'ok' | 'error' | 'empty'>('idle')
  const [results, setResults] = useState<WebSearchResult[]>([])
  const [errorKey, setErrorKey] = useState<'notEnabled' | 'failed'>('failed')

  const run = async (q: string) => {
    const query = q.trim()
    if (!query) return
    setState('loading')
    try {
      const { data } = await webApi.get<WebSearchResult[]>('/help/web-search', { params: { q: query } })
      setResults(data ?? [])
      setState(Array.isArray(data) && data.length > 0 ? 'ok' : 'empty')
    } catch (e) {
      // 端点未部署/未配置时后端返回 404 → 明确提示「搜索服务未启用」，其余按通用失败处理。
      if (axios.isAxiosError(e) && e.response?.status === 404) {
        setErrorKey('notEnabled')
      } else {
        setErrorKey('failed')
      }
      setState('error')
    }
  }

  return (
    <div className="space-y-4">
      <div className="flex gap-2">
        <Input
          id="help-search"
          value={raw}
          onChange={(e) => setRaw(e.target.value)}
          onKeyDown={(e) => e.key === 'Enter' && run(raw)}
          placeholder={t('helppage.web.placeholder')}
          aria-label={t('helppage.web.placeholder')}
          className="max-w-md"
        />
        <Button size="sm" onClick={() => run(raw)} disabled={state === 'loading' || !raw.trim()}>
          {state === 'loading' ? (
            <>
              <Loader2 className="mr-1.5 h-4 w-4 animate-spin" />
              {t('helppage.web.searching')}
            </>
          ) : (
            <>
              <Globe className="mr-1.5 h-4 w-4" />
              {t('helppage.web.search')}
            </>
          )}
        </Button>
      </div>

      {state === 'ok' && (
        <>
          <p className="text-xs text-muted-foreground">
            {t('helppage.web.resultCount').replace('{n}', String(results.length))}
          </p>
          <div className="space-y-2">
            {results.map((r, i) => (
              <a
                key={`${r.url}-${i}`}
                href={r.url}
                target="_blank"
                rel="noreferrer"
                className="block rounded-lg border border-border/60 bg-white/5 p-3 transition-colors hover:border-primary/40 hover:bg-primary/5"
              >
                <div className="flex items-center gap-1.5 text-sm font-medium">
                  {r.title}
                  <ExternalLink className="h-3 w-3 shrink-0 text-muted-foreground" />
                </div>
                <p className="mt-1 line-clamp-2 text-xs leading-relaxed text-muted-foreground">{r.snippet}</p>
                <code className="mt-1 block truncate font-mono text-[10px] text-[#888]">{r.url}</code>
              </a>
            ))}
          </div>
        </>
      )}
      {state === 'empty' && (
        <EmptyState icon={SearchX} title={t('helppage.web.empty')} description={t('helppage.web.emptyDesc')} />
      )}
      {state === 'error' && (
        <EmptyState
          icon={SearchX}
          tone="destructive"
          title={errorKey === 'notEnabled' ? t('helppage.web.notEnabled') : t('helppage.web.failed')}
          description={errorKey === 'notEnabled' ? t('helppage.web.notEnabledDesc') : t('helppage.web.failedDesc')}
          action={
            <Button variant="outline" size="sm" onClick={() => run(raw)}>
              {t('helppage.web.retry')}
            </Button>
          }
        />
      )}
    </div>
  )
}

/* ============ 帮助中心页 ============ */

export function HelpPage({ onBack }: { onBack: () => void }) {
  const { t } = useTranslation()
  const [view, setView] = useState<View>('kb')
  const [query, setQuery] = useState('')

  // 数据文件由并行内容任务产出；尚未落盘时整体降级为「知识库加载中」空态，不阻塞页面其余部分。
  const loaded = Array.isArray(HELP_ENTRIES) && Array.isArray(HELP_MODULES) && Array.isArray(HELP_CHAIN)

  const views: { id: View; label: string; icon: React.ReactNode }[] = [
    { id: 'kb', label: t('helppage.tab.kb'), icon: <BookOpen className="mr-1.5 h-4 w-4" /> },
    { id: 'map', label: t('helppage.tab.map'), icon: <Compass className="mr-1.5 h-4 w-4" /> },
    { id: 'web', label: t('helppage.tab.web'), icon: <Globe className="mr-1.5 h-4 w-4" /> },
    { id: 'conn', label: t('helppage.tab.conn'), icon: <Network className="mr-1.5 h-4 w-4" /> },
  ]

  return (
    <div className="min-h-screen">
      {/* 顶部栏：返回 + 标题 + 主搜索框（本地过滤 HELP_ENTRIES：title/tags/problem） */}
      <div className="flex flex-wrap items-center gap-3 border-b border-[#2e2e2e] px-8 py-5">
        <Button variant="ghost" size="sm" onClick={onBack}>
          <ArrowLeft className="mr-1 h-4 w-4" />
          {t('helppage.back')}
        </Button>
        <h2 className="text-lg font-semibold text-gradient-brand">{t('helppage.title')}</h2>
        <div className="relative ml-auto w-72 max-w-full">
          <Search className="pointer-events-none absolute left-2.5 top-1/2 h-4 w-4 -translate-y-1/2 text-muted-foreground" />
          <Input
            id="help-query"
            className="pl-8"
            value={query}
            onChange={(e) => {
              setQuery(e.target.value)
              // 搜索词天然属于知识库，输入时自动切到知识库视图。
              if (view !== 'kb') setView('kb')
            }}
            placeholder={t('helppage.searchPlaceholder')}
            aria-label={t('helppage.searchPlaceholder')}
          />
        </div>
      </div>

      <div className="mx-auto max-w-[1200px] space-y-5 px-8 py-6">
        {/* 视图切换 Tab 组 */}
        <div className="flex flex-nowrap gap-1 overflow-x-auto border-b pb-3">
          {views.map((v) => (
            <Button
              key={v.id}
              variant={view === v.id ? 'default' : 'outline'}
              size="sm"
              className="shrink-0 whitespace-nowrap"
              onClick={() => setView(v.id)}
            >
              {v.icon}
              {v.label}
            </Button>
          ))}
        </div>

        {view === 'kb' && <KnowledgeView entries={loaded ? HELP_ENTRIES : []} query={query} loading={!loaded} />}
        {view === 'map' && <MapView chain={loaded ? HELP_CHAIN : []} modules={loaded ? HELP_MODULES : []} loading={!loaded} />}
        {view === 'web' && <WebSearchView />}
        {view === 'conn' && <ConnPage />}
      </div>
    </div>
  )
}
