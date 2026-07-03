import { useState, lazy, Suspense } from 'react'
import { useQueryClient } from '@tanstack/react-query'
import { storage } from '@/lib/storage'
import { Button } from '@/components/ui/button'
import { Server, Activity, BarChart3, Settings as SettingsIcon, Moon, Sun, LogOut } from 'lucide-react'

const Dashboard = lazy(() =>
  import('@/components/dashboard').then((m) => ({ default: m.Dashboard }))
)
const OverviewPage = lazy(() =>
  import('@/components/overview-page').then((m) => ({ default: m.OverviewPage }))
)
const UsagePage = lazy(() =>
  import('@/components/usage-page').then((m) => ({ default: m.UsagePage }))
)
const SettingsPage = lazy(() =>
  import('@/components/settings-page').then((m) => ({ default: m.SettingsPage }))
)

type Tab = 'overview' | 'usage' | 'credentials' | 'settings'

const TABS: { key: Tab; label: string; icon: React.ReactNode }[] = [
  { key: 'overview', label: '概览', icon: <Activity className="h-4 w-4" /> },
  { key: 'usage', label: '用量统计', icon: <BarChart3 className="h-4 w-4" /> },
  { key: 'credentials', label: '凭据管理', icon: <Server className="h-4 w-4" /> },
  { key: 'settings', label: '设置', icon: <SettingsIcon className="h-4 w-4" /> },
]

interface AppShellProps {
  onLogout: () => void
}

export function AppShell({ onLogout }: AppShellProps) {
  const [tab, setTab] = useState<Tab>('overview')
  const [darkMode, setDarkMode] = useState(() =>
    typeof window !== 'undefined' && document.documentElement.classList.contains('dark')
  )
  const queryClient = useQueryClient()

  const toggleDarkMode = () => {
    setDarkMode(!darkMode)
    document.documentElement.classList.toggle('dark')
  }

  const handleLogout = () => {
    storage.removeApiKey()
    queryClient.clear()
    onLogout()
  }

  return (
    <div className="min-h-screen bg-background">
      {/* 全局顶栏 */}
      <header className="sticky top-0 z-50 w-full border-b bg-background/95 backdrop-blur supports-[backdrop-filter]:bg-background/60">
        <div className="container mx-auto flex h-14 items-center justify-between gap-4 px-4 md:px-8">
          <div className="flex items-center gap-2 shrink-0">
            <Server className="h-5 w-5" />
            <span className="font-semibold">Kiro Admin</span>
          </div>

          {/* 标签导航 */}
          <nav className="flex items-center gap-1 overflow-x-auto">
            {TABS.map((t) => (
              <Button
                key={t.key}
                size="sm"
                variant={tab === t.key ? 'default' : 'ghost'}
                className="h-8 gap-1.5 px-3"
                onClick={() => setTab(t.key)}
              >
                {t.icon}
                <span className="text-sm">{t.label}</span>
              </Button>
            ))}
          </nav>

          <div className="flex items-center gap-1 shrink-0">
            <Button variant="ghost" size="icon" onClick={toggleDarkMode} title="切换主题">
              {darkMode ? <Sun className="h-5 w-5" /> : <Moon className="h-5 w-5" />}
            </Button>
            <Button variant="ghost" size="icon" onClick={handleLogout} title="退出登录">
              <LogOut className="h-5 w-5" />
            </Button>
          </div>
        </div>
      </header>

      {/* 主内容区 */}
      <main className="container mx-auto px-4 md:px-8 py-6">
        <Suspense
          fallback={
            <div className="flex items-center justify-center py-24">
              <div className="animate-spin rounded-full h-10 w-10 border-b-2 border-primary" />
            </div>
          }
        >
          {tab === 'overview' && <OverviewPage />}
          {tab === 'usage' && <UsagePage />}
          {tab === 'credentials' && <Dashboard onLogout={onLogout} embedded />}
          {tab === 'settings' && <SettingsPage />}
        </Suspense>
      </main>
    </div>
  )
}
