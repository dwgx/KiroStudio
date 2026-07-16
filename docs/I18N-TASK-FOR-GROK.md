# I18N 剩余翻译任务书(给 Grok4.5)

## 背景与现状
KiroStudio 运维台(`D:/Project/KiroStudio/admin-ui`,React18+Vite+TS)正在做中/日/英三语 I18N。
**框架地基已搭好、13 个核心组件已译完**(overview/usage/dashboard/ops/credential-card/
ops-detail-dialogs/model-test/login-page/login-dialog/add-credential/kam-import/batch-import)。
现需你**把剩余组件也国际化**。

## 已有基础设施(直接用,勿重建)
- i18n 初始化:`src/i18n/index.ts`(react-i18next,单一 default namespace,扁平 key)
- 字典:`src/i18n/resources/{zh,en,ja}.json`(945 key,点分命名空间如 `opspage.metric.refreshOk`)
- **占位符用单括号** `{var}`(i18next 已配 prefix/suffix 为单括号),调用 `t('key', { var })`
- 语言切换器已接侧边栏,切换即时全树重渲

## 你的任务:剩余组件(按优先级)
真实 UI 文案多的(优先):
1. `settings-page.tsx`(最大,451 行含中文,注释约 111 行不算)——⚠️最大,可最后做或拆分
2. `idc-login-dialog.tsx` / `social-login-dialog.tsx`(上号弹窗)
3. `region-switcher.tsx` / `balance-dialog.tsx` / `batch-verify-dialog.tsx` / `diagnosis-card.tsx`
4. `app-shell.tsx`(侧边栏导航:概览/凭据管理/用量统计/运维/设置/上号 等菜单)
5. `proxy-test-button.tsx`
次要(多为注释,真实文案少,可低优先):`overview/*.tsx`、`ui/*.tsx`

## 每个组件的做法(严格遵守)
1. **抽取**:找出组件里所有硬编码 UI 中文(JSX文本/placeholder/title/aria-label/toast/按钮/label/select选项)。
   **不动**:代码注释、变量名、日志字符串、与后端返回值比较的业务字符串。
2. **建 key + 三语翻译**:给每个中文起点分 key(命名空间用组件名,如 `settingspage.xxx`/`appshell.nav.overview`),
   在 `zh/en/ja.json` 三个字典里都加上该 key 的中/英/日翻译。英日要地道、术语一致(参考已有 key 风格)。
   含占位符的用单括号 `{var}`,三语占位符名必须一致。
3. **替换**:组件顶部 `import { useTranslation } from 'react-i18next'`,每个用到中文的 React 组件函数体顶部
   加 `const { t } = useTranslation()`(遵守 hooks 规则),中文字面量替换成 `t('key')`,占位用 `t('key',{var})`。
4. **模块级常量/函数**(React 组件外,hook 用不了):改用 i18n 单例 `import i18n from '@/i18n'; i18n.t('key')`,
   但注意——模块级常量在 import 时求值一次,切语言不会重算,**必须改成 getter 函数或在渲染时调 t()**,否则切语言不更新。
5. **不改**业务逻辑/JSX结构/className。

## 验证(必做)
- 每改完一个组件(或一批),在 `admin-ui` 目录跑 `npm run build`(含 tsc 类型检查),**必须 build 绿**才算完成。
- 自查:grep 组件确认没误改注释/变量;字典三语 key 数一致(zh/en/ja 各自 key 集合相同)。
- 占位符插值:字典单括号 `{var}`,别写成 `${var}`(JS模板字面量)或 `{{var}}`(i18next默认双括号,本项目已改单括号)。

## 铁律
- **质量只能升不能降**:翻译要地道,不机翻硬凑;build 必须绿才交。
- commit 不加 AI 署名(用户全局规矩);只有用户明确要求才 commit。
- 参考已译的 13 个组件(如 `ops-page.tsx`/`credential-card.tsx`)学习 key 命名和替换模式。

## 交付
每个组件报告:①抽取+译了多少 key ②新增到字典的 key ③build 是否绿 ④跳过/拿不准的地方。
