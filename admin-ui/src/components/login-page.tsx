import { useState, useEffect } from 'react'
import { KeyRound, Loader2 } from 'lucide-react'
import { storage } from '@/lib/storage'
import { getLoadBalancingMode, setSuppressAuthReload } from '@/api/credentials'

interface LoginPageProps {
  onLogin: (apiKey: string) => void
}

export function LoginPage({ onLogin }: LoginPageProps) {
  const [apiKey, setApiKey] = useState('')
  const [bgLoaded, setBgLoaded] = useState(false)
  const [bgUrl, setBgUrl] = useState<string | null>(null)
  // 登录校验态：verifying=校验中(转圈)、error=就地错误文案（错密钥秒失败，不再进面板死转圈）
  const [verifying, setVerifying] = useState(false)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    const savedKey = storage.getApiKey()
    if (savedKey) {
      setApiKey(savedKey)
    }
  }, [])

  useEffect(() => {
    let cancelled = false

    async function loadBg() {
      try {
        const controller = new AbortController()
        const timeout = setTimeout(() => controller.abort(), 10000)
        const res = await fetch('/admin/api/random-bg', {
          signal: controller.signal,
        })
        clearTimeout(timeout)
        if (!res.ok) throw new Error('API failed')
        const json = await res.json()
        if (json.url && !cancelled) {
          const img = new Image()
          img.onload = () => {
            if (!cancelled) { setBgUrl(img.src); setBgLoaded(true) }
          }
          img.onerror = () => {
            if (!cancelled) setBgLoaded(true)
          }
          img.src = json.url
        } else {
          if (!cancelled) setBgLoaded(true)
        }
      } catch {
        if (!cancelled) setBgLoaded(true)
      }
    }

    loadBg()
    return () => { cancelled = true }
  }, [])

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault()
    const key = apiKey.trim()
    if (!key || verifying) return
    setError(null)
    setVerifying(true)
    // 临时写入 key，让 axios 拦截器带上它去打一个**只读、不触上游**的端点校验（GET /config/load-balancing）。
    // 200 才真正进面板；401/错误则清掉 key + 就地报错——错密钥秒失败，不再乐观放行后死转圈。
    storage.setApiKey(key)
    setSuppressAuthReload(true) // 校验期间抑制拦截器自动 reload，让错误就地展示
    try {
      await getLoadBalancingMode()
      onLogin(key)
    } catch (err) {
      storage.removeApiKey()
      const status = (err as { response?: { status?: number } })?.response?.status
      setError(status === 401 || status === 403 ? '密钥无效，请重新输入' : '无法连接服务，请稍后重试')
    } finally {
      setSuppressAuthReload(false)
      setVerifying(false)
    }
  }

  return (
    <div className="fixed inset-0 flex items-center justify-center">
      {/* Background image or gradient fallback */}
      <div
        className="absolute inset-0 bg-cover bg-center"
        style={{
          backgroundImage: bgUrl
            ? `url(${bgUrl})`
            : 'linear-gradient(135deg, #1a1a2e 0%, #16213e 30%, #0f3460 60%, #0a0a0a 100%)',
          opacity: bgLoaded ? 1 : 0,
          transition: 'opacity 1.5s ease',
        }}
      />

      {/* Dark overlay */}
      <div
        className="absolute inset-0"
        style={{ backgroundColor: 'rgba(0, 0, 0, 0.55)' }}
      />

      {/* Login card */}
      <div
        className="relative z-10 w-full mx-4"
        style={{
          maxWidth: '380px',
          padding: '40px',
          backdropFilter: 'blur(20px)',
          WebkitBackdropFilter: 'blur(20px)',
          background: 'rgba(10, 10, 10, 0.75)',
          border: '1px solid rgba(255, 255, 255, 0.1)',
          borderRadius: '16px',
        }}
      >
        {/* Logo */}
        <div className="text-center mb-8">
          <h1
            className="font-bold mb-2"
            style={{
              fontSize: '28px',
              background: 'linear-gradient(135deg, #0070f3, #7928ca)',
              WebkitBackgroundClip: 'text',
              WebkitTextFillColor: 'transparent',
              backgroundClip: 'text',
            }}
          >
            KiroStudio
          </h1>
          <p style={{ fontSize: '13px', color: '#888888' }}>
            Kiro IDE Gateway
          </p>
        </div>

        {/* Form */}
        <form onSubmit={handleSubmit}>
          <div className="mb-4">
            <div className="flex items-center gap-2 mb-2">
              <KeyRound style={{ width: '14px', height: '14px', color: '#666' }} />
              <label style={{ fontSize: '12px', color: '#888', fontWeight: 500 }}>
                Admin API Key
              </label>
            </div>
            <input
              type="password"
              placeholder="输入管理密钥"
              value={apiKey}
              onChange={(e) => { setApiKey(e.target.value); if (error) setError(null) }}
              disabled={verifying}
              className="w-full outline-none"
              style={{
                padding: '10px 14px',
                fontSize: '14px',
                background: 'rgba(255, 255, 255, 0.05)',
                border: '1px solid rgba(255, 255, 255, 0.1)',
                borderRadius: '8px',
                color: '#ededed',
                transition: 'border-color 150ms ease',
              }}
              onFocus={(e) => {
                e.currentTarget.style.borderColor = '#0070f3'
              }}
              onBlur={(e) => {
                e.currentTarget.style.borderColor = 'rgba(255, 255, 255, 0.1)'
              }}
            />
          </div>

          {/* 就地错误文案：错密钥秒显示，不再进面板转圈 */}
          {error && (
            <p
              className="mb-3 text-center"
              style={{ fontSize: '12px', color: '#f5554e', fontWeight: 500 }}
              role="alert"
            >
              {error}
            </p>
          )}

          <button
            type="submit"
            disabled={!apiKey.trim() || verifying}
            className="flex w-full items-center justify-center gap-2 cursor-pointer disabled:opacity-40 disabled:cursor-not-allowed"
            style={{
              padding: '10px 0',
              fontSize: '14px',
              fontWeight: 500,
              background: '#ededed',
              color: '#0a0a0a',
              border: 'none',
              borderRadius: '8px',
              transition: 'box-shadow 150ms ease, transform 150ms ease',
            }}
            onMouseEnter={(e) => {
              e.currentTarget.style.boxShadow = '0 0 20px rgba(255,255,255,0.15)'
            }}
            onMouseLeave={(e) => {
              e.currentTarget.style.boxShadow = 'none'
            }}
          >
            {verifying && <Loader2 style={{ width: '15px', height: '15px' }} className="animate-spin" />}
            {verifying ? '校验中…' : '登录'}
          </button>
        </form>

        {/* Footer：dwgx 指向作者 GitHub 主页 */}
        <p className="text-center mt-6" style={{ fontSize: '11px', color: '#555' }}>
          Powered by{' '}
          <a
            href="https://github.com/dwgx"
            target="_blank"
            rel="noopener noreferrer"
            className="hover:underline"
            style={{
              color: '#0070f3',
              fontWeight: 600,
              textUnderlineOffset: '2px',
              transition: 'color 150ms ease',
            }}
            onMouseEnter={(e) => {
              e.currentTarget.style.color = '#3b93ff'
            }}
            onMouseLeave={(e) => {
              e.currentTarget.style.color = '#0070f3'
            }}
          >
            dwgx
          </a>
        </p>
      </div>
    </div>
  )
}
