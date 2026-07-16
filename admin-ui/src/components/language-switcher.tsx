import { useTranslation } from 'react-i18next'
import { Select } from '@/components/ui/select'
import { SUPPORTED_LANGS } from '@/i18n'

// 语言切换器:侧边栏 footer + 登录页共用。选项标签用各语言母语自称(中文/English/日本語),
// 不随当前界面语言变(通用习惯)。changeLanguage 触发全树重渲 + detector 写 localStorage(kiroLang)。
export function LanguageSwitcher({ className }: { className?: string }) {
  const { i18n } = useTranslation()
  const current = SUPPORTED_LANGS.some((l) => l.code === i18n.language)
    ? i18n.language
    : 'zh'
  return (
    <Select
      value={current}
      onChange={(lng) => { void i18n.changeLanguage(lng) }}
      options={SUPPORTED_LANGS.map((l) => ({ value: l.code, label: l.label }))}
      className={className}
      aria-label="Language"
    />
  )
}
