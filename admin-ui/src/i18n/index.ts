// react-i18next 初始化。资源为扁平 key(点分命名空间在 key 名里,单一 default namespace)。
// 语言持久化:localStorage 键 `kiroLang`(项目无现成语言键,新建);默认跟随浏览器,回退中文。
import i18n from 'i18next'
import { initReactI18next } from 'react-i18next'
import LanguageDetector from 'i18next-browser-languagedetector'

import zh from './resources/zh.json'
import en from './resources/en.json'
import ja from './resources/ja.json'

export const SUPPORTED_LANGS = [
  { code: 'zh', label: '中文' },
  { code: 'en', label: 'English' },
  { code: 'ja', label: '日本語' },
] as const

export const LANG_STORAGE_KEY = 'kiroLang'

i18n
  .use(LanguageDetector)
  .use(initReactI18next)
  .init({
    resources: {
      zh: { translation: zh },
      en: { translation: en },
      ja: { translation: ja },
    },
    fallbackLng: 'zh',
    supportedLngs: ['zh', 'en', 'ja'],
    // 未翻译的 key 直接回退中文字典,再回退 key 名本身——迁移期间未替换的组件仍显示中文字面量,不受影响。
    nonExplicitSupportedLngs: true,
    interpolation: {
      escapeValue: false, // React 已防 XSS
      // 字典占位符用单括号 {var}(抽取产物既定风格),而 i18next 默认识别双括号 {{var}}。
      // 改插值前后缀为单括号,让 t('key', { var }) 的插值正确生效——否则所有带占位符的翻译
      // 运行时会显示字面 {var} 而非实际值(几百处会炸)。
      prefix: '{',
      suffix: '}',
    },
    detection: {
      order: ['localStorage', 'navigator'],
      lookupLocalStorage: LANG_STORAGE_KEY,
      caches: ['localStorage'],
    },
    react: { useSuspense: false }, // 资源同步内联,无需 Suspense
  })

export default i18n
