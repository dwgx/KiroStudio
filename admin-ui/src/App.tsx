import { useState, useEffect } from 'react'
import { storage } from '@/lib/storage'
import { LoginPage } from '@/components/login-page'
import { AppShell } from '@/components/app-shell'
import { Toaster } from '@/components/ui/sonner'

function App() {
  const [isLoggedIn, setIsLoggedIn] = useState(false)

  useEffect(() => {
    // 检查是否已经有保存的 API Key
    if (storage.getApiKey()) {
      setIsLoggedIn(true)
    }
  }, [])

  const handleLogin = () => {
    setIsLoggedIn(true)
  }

  const handleLogout = () => {
    setIsLoggedIn(false)
  }

  return (
    <>
      {isLoggedIn ? (
        <AppShell onLogout={handleLogout} />
      ) : (
        <LoginPage onLogin={handleLogin} />
      )}
      {/* 通知栈挂载点：展开平铺 / 可见 6 条 / 常驻关闭按钮等行为在 Toaster 组件内统一配置，此处仅定位到右上角 */}
      <Toaster position="top-right" />
    </>
  )
}

export default App
