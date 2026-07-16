import { useState, lazy, Suspense } from 'react'
import { useTranslation } from 'react-i18next'
import { useQueryClient } from '@tanstack/react-query'
import { storage } from '@/lib/storage'
import {
  LayoutDashboard,
  Key,
  BarChart3,
  Settings,
  Wrench,
  LogIn,
  LogOut,
} from 'lucide-react'
import { LoginDialog } from '@/components/login-dialog'
import { LanguageSwitcher } from '@/components/language-switcher'
import { PageSkeleton } from '@/components/ui/page-skeleton'
import { usePoolNotifications } from '@/hooks/use-pool-notifications'
import { useConfigSnapshot } from '@/hooks/use-credentials'

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
const OpsPage = lazy(() =>
  import('@/components/ops-page').then((m) => ({ default: m.OpsPage }))
)

type Tab = 'overview' | 'credentials' | 'usage' | 'ops' | 'settings'

const NAV_ICONS: Record<Tab, React.ReactNode> = {
  overview: <LayoutDashboard className="h-4 w-4" />,
  credentials: <Key className="h-4 w-4" />,
  usage: <BarChart3 className="h-4 w-4" />,
  ops: <Wrench className="h-4 w-4" />,
  settings: <Settings className="h-4 w-4" />,
}

const NAV_KEYS: Tab[] = ['overview', 'credentials', 'usage', 'ops', 'settings']

const TAB_TITLE_KEYS: Record<Tab, string> = {
  overview: 'appshell.nav.overview',
  credentials: 'appshell.nav.credentials',
  usage: 'appshell.nav.usage',
  ops: 'appshell.nav.ops',
  settings: 'appshell.nav.settings',
}

interface AppShellProps {
  onLogout: () => void
}

export function AppShell({ onLogout }: AppShellProps) {
  const { t } = useTranslation()
  const [tab, setTab] = useState<Tab>('overview')
  const [loginOpen, setLoginOpen] = useState(false)
  const queryClient = useQueryClient()

  // 号池健康事件通知（右下角 toast，状态跃迁时弹一次；复用已有轮询数据，零额外上游调用）。
  usePoolNotifications()

  // 侧边栏版本号：读服务端真实版本（编译期注入），不再硬编码。react-query 缓存键与设置页
  // 共享（config-snapshot），此处零额外请求。取不到时不显示版本号，胜过显示过时的写死值。
  const { data: cfg } = useConfigSnapshot()

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
          <p className="text-xs text-[#666] mt-1">
            {t('appshell.brand.adminPanel')}{cfg?.serverVersion ? ` v${cfg.serverVersion}` : ''}
          </p>
        </div>

        {/* Main Nav */}
        <div className="px-3 flex-1">
          <p className="text-[11px] font-medium text-[#666] uppercase tracking-wider px-3 mb-2">
            {t('appshell.section.mainMenu')}
          </p>
          <nav className="flex flex-col gap-0.5">
            {NAV_KEYS.map((key) => (
              <button
                key={key}
                onClick={() => setTab(key)}
                className={`
                  relative flex items-center gap-3 px-3 py-2 rounded-md text-sm font-medium
                  transition-all duration-250 ease-out-expo
                  ${
                    tab === key
                      ? 'bg-[rgba(0,112,243,0.12)] text-[#ededed] border-l-2 border-l-[#0070f3] pl-[10px]'
                      : 'text-[#888] hover:bg-[#1a1a1a] hover:text-[#ededed] hover:translate-x-0.5 border-l-2 border-l-transparent pl-[10px]'
                  }
                `}
              >
                {NAV_ICONS[key]}
                {t(TAB_TITLE_KEYS[key])}
              </button>
            ))}
          </nav>

          {/* Divider */}
          <div className="border-t border-[#2e2e2e] my-4" />

          {/* Quick Actions */}
          <p className="text-[11px] font-medium text-[#666] uppercase tracking-wider px-3 mb-2">
            {t('appshell.section.quickActions')}
          </p>
          <div className="flex flex-col gap-0.5">
            <button
              onClick={() => setLoginOpen(true)}
              className="flex items-center gap-3 px-3 py-2 rounded-md text-sm text-[#888] hover:bg-[#1a1a1a] hover:text-[#ededed] transition-all duration-150"
            >
              <LogIn className="h-4 w-4" />
              {t('appshell.action.login')}
            </button>
          </div>
        </div>

        {/* Footer */}
        <div className="px-5 py-4 border-t border-[#2e2e2e] space-y-2">
          <LanguageSwitcher className="w-full" />
          <button
            onClick={handleLogout}
            className="flex w-full items-center justify-center gap-2 px-3 py-2 rounded-md text-sm text-[#888] hover:text-[#ededed] hover:bg-[#1a1a1a] transition-all duration-200 ease-out-expo"
            title={t('appshell.action.logout')}
          >
            <LogOut className="h-4 w-4" />
            {t('appshell.action.logout')}
          </button>
        </div>
      </aside>

      {/* Main Content */}
      <main className="ml-[240px] min-h-screen">
        {/* Page Header */}
        <div className="border-b border-[#2e2e2e] px-8 py-5">
          <h2 className="text-lg font-semibold text-gradient-brand">{t(TAB_TITLE_KEYS[tab])}</h2>
        </div>

        {/* Page Content */}
        <div className="max-w-[1200px] mx-auto px-8 py-8">
          <Suspense fallback={<PageSkeleton kind={tab} />}>
            {tab === 'overview' && <OverviewPage />}
            {tab === 'usage' && <UsagePage />}
            {tab === 'credentials' && <Dashboard onLogout={onLogout} embedded />}
            {tab === 'ops' && <OpsPage />}
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
