import { useState, lazy, Suspense } from 'react'
import { useQueryClient } from '@tanstack/react-query'
import { storage } from '@/lib/storage'
import {
  LayoutDashboard,
  Key,
  BarChart3,
  Settings,
  LogIn,
  LogOut,
} from 'lucide-react'
import { LoginDialog } from '@/components/login-dialog'
import { PageSkeleton } from '@/components/ui/page-skeleton'
import { usePoolNotifications } from '@/hooks/use-pool-notifications'

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

type Tab = 'overview' | 'credentials' | 'usage' | 'settings'

const NAV_ITEMS: { key: Tab; label: string; icon: React.ReactNode }[] = [
  { key: 'overview', label: '概览', icon: <LayoutDashboard className="h-4 w-4" /> },
  { key: 'credentials', label: '凭据管理', icon: <Key className="h-4 w-4" /> },
  { key: 'usage', label: '用量统计', icon: <BarChart3 className="h-4 w-4" /> },
  { key: 'settings', label: '设置', icon: <Settings className="h-4 w-4" /> },
]

const TAB_TITLES: Record<Tab, string> = {
  overview: '概览',
  credentials: '凭据管理',
  usage: '用量统计',
  settings: '设置',
}

interface AppShellProps {
  onLogout: () => void
}

export function AppShell({ onLogout }: AppShellProps) {
  const [tab, setTab] = useState<Tab>('overview')
  const [loginOpen, setLoginOpen] = useState(false)
  const queryClient = useQueryClient()

  // 号池健康事件通知（右下角 toast，状态跃迁时弹一次；复用已有轮询数据，零额外上游调用）。
  usePoolNotifications()

  const handleLogout = () => {
    storage.removeApiKey()
    queryClient.clear()
    onLogout()
  }

  return (
    <div className="min-h-screen bg-[#0a0a0a] text-[#ededed]" translate="no">
      {/* Sidebar */}
      <aside className="fixed top-0 left-0 bottom-0 w-[240px] border-r border-[#2e2e2e] flex flex-col z-40">
        {/* Logo */}
        <div className="px-5 pt-6 pb-4">
          <h1 className="text-xl font-bold text-gradient-brand">
            KiroStudio
          </h1>
          <p className="text-xs text-[#666] mt-1">Admin Panel v0.6.2</p>
        </div>

        {/* Main Nav */}
        <div className="px-3 flex-1">
          <p className="text-[11px] font-medium text-[#666] uppercase tracking-wider px-3 mb-2">
            主菜单
          </p>
          <nav className="flex flex-col gap-0.5">
            {NAV_ITEMS.map((item) => (
              <button
                key={item.key}
                onClick={() => setTab(item.key)}
                className={`
                  relative flex items-center gap-3 px-3 py-2 rounded-md text-sm font-medium
                  transition-all duration-250 ease-out-expo
                  ${
                    tab === item.key
                      ? 'bg-[rgba(0,112,243,0.12)] text-[#ededed] border-l-2 border-l-[#0070f3] pl-[10px]'
                      : 'text-[#888] hover:bg-[#1a1a1a] hover:text-[#ededed] hover:translate-x-0.5 border-l-2 border-l-transparent pl-[10px]'
                  }
                `}
              >
                {item.icon}
                {item.label}
              </button>
            ))}
          </nav>

          {/* Divider */}
          <div className="border-t border-[#2e2e2e] my-4" />

          {/* Quick Actions */}
          <p className="text-[11px] font-medium text-[#666] uppercase tracking-wider px-3 mb-2">
            快捷操作
          </p>
          <div className="flex flex-col gap-0.5">
            <button
              onClick={() => setLoginOpen(true)}
              className="flex items-center gap-3 px-3 py-2 rounded-md text-sm text-[#888] hover:bg-[#1a1a1a] hover:text-[#ededed] transition-all duration-150"
            >
              <LogIn className="h-4 w-4" />
              上号
            </button>
          </div>
        </div>

        {/* Footer */}
        <div className="px-5 py-4 border-t border-[#2e2e2e]">
          <button
            onClick={handleLogout}
            className="flex w-full items-center justify-center gap-2 px-3 py-2 rounded-md text-sm text-[#888] hover:text-[#ededed] hover:bg-[#1a1a1a] transition-all duration-200 ease-out-expo"
            title="退出登录"
          >
            <LogOut className="h-4 w-4" />
            退出登录
          </button>
        </div>
      </aside>

      {/* Main Content */}
      <main className="ml-[240px] min-h-screen">
        {/* Page Header */}
        <div className="border-b border-[#2e2e2e] px-8 py-5">
          <h2 className="text-lg font-semibold text-gradient-brand">{TAB_TITLES[tab]}</h2>
        </div>

        {/* Page Content */}
        <div className="max-w-[1200px] mx-auto px-8 py-8">
          <Suspense fallback={<PageSkeleton kind={tab} />}>
            {tab === 'overview' && <OverviewPage />}
            {tab === 'usage' && <UsagePage />}
            {tab === 'credentials' && <Dashboard onLogout={onLogout} embedded />}
            {tab === 'settings' && <SettingsPage />}
          </Suspense>
        </div>
      </main>

      {/* Dialogs */}
      <LoginDialog
        open={loginOpen}
        onOpenChange={setLoginOpen}
        onSuccess={() => queryClient.invalidateQueries({ queryKey: ['credentials'] })}
      />
    </div>
  )
}
