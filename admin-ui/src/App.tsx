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
      {/* 通知栈挂载点：自研 Toaster（src/lib/toaster.tsx，弃用 sonner）。
          竖直平铺 / 硬上限 5 条超出丢最旧 / 常驻关闭叉叉 / 倒计时进度条 / hover 暂停。
          定位右下角（dwgx 要求）：号池健康事件通知 + 手动操作反馈都从这里弹。 */}
      <Toaster position="bottom-right" />
    </>
  )
}

export default App
