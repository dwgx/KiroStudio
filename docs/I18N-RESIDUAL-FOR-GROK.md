# I18N 模块级残留中文清扫任务(给 Grok4.5)

## 背景
KiroStudio 运维台(`D:/Project/KiroStudio/admin-ui`)已完成大部分 I18N(react-i18next,字典
`src/i18n/resources/{zh,en,ja}.json`,1387 key,单括号占位符 `{var}`)。但**真机切英文后仍有一批中文没译**,
根因:它们是**模块级常量/函数里的中文**(在 React 组件函数外定义,`t` hook 用不了,之前批次都跳过了)。

## 你的任务:清扫所有模块级残留中文
真机确认的残留(先做这些,都是模块级常量):
1. `ops-page.tsx`:
   - `METRIC_ITEMS`(约115行):15 个指标 label(刷新成功/刷新失败/failover 换号/failover 耗尽/自动禁用死号/
     风控冷却触发/region 重探成功/region 重探失败/泄漏清洗请求/整段退化请求/文本化工具调用/invoke 重组捞回/
     stray 熔断触发/stray 独占行(观测)/stray 句中泄漏(观测))
   - `CIRCUIT_META`(约447行):熔断/半开试探/冷却中/已禁用/亚健康/健康
   - `formatUptime`(约147行):天/小时/分
2. `usage-page.tsx`:`DEVICE_META`(浏览器/未知等设备标签)、`OUTCOME_LABEL`(成功/限流/鉴权失败/额度耗尽/
   账号封禁/服务错误/请求错误/网络错误/其他错误)
3. `ops-detail-dialogs.tsx`:`OUTCOME_META`(成功/限流/鉴权失败/…)、`OUTCOME_OPTIONS`(全部结果)、
   `timeAgo`(刚刚/秒前/分钟前/小时前/天前/个月前/年前)、`buildPieSegments`(其它)
4. `credential-card.tsx`:`formatLastUsed`/`formatCachedAt`(从未使用/刚刚/秒前/分钟前/小时前/天前)
5. 其它组件里任何 tooltip/badge 的模块级中文(如"当前活跃/在途/最近命中")、DetailRow 的"设备:未知"等。
**全站再 grep 一遍** `[一-鿿]`(排除注释行),把所有**UI展示用的**模块级中文都找出来。

## 做法(关键——模块级不能用 hook)
模块级常量/函数里的中文,**两种正确改法二选一**:
- **A(推荐)改成"渲染时翻译"**:常量里存 i18n key(如 `METRIC_ITEMS` 的 label 改成 labelKey),
  在**组件渲染处**用 `const { t } = useTranslation()` 的 `t(item.labelKey)` 翻译。这样切语言实时更新。
- **B 用 i18n 单例**:`import i18n from '@/i18n'`,模块级函数内用 `i18n.t('key')`。⚠️但注意:模块级常量
  在 import 时**只求值一次**,切语言不会重算——所以**纯常量必须用 A**(改成渲染时 t());只有**每次调用的函数**
  (如 timeAgo/formatLastUsed,每次渲染都调)才能用 B(i18n.t 每次调用取当前语言)。
- 判断:是"定义一次的常量数组/对象" → 用 A(存 key,渲染时 t);是"每次渲染调用的格式化函数" → 用 B(i18n.t)。

## 字典
字典大多已有对应 key(如 `opspage.metric.*`/`opspage.circuit.*`/`usagepage.outcome.*`/
`opsdetaildialogs.timeAgo.*`/`credentialcard.lastUsed.*`)——**先查字典有没有现成 key 复用**;没有的补三语
(zh/en/ja 三个文件都加,占位符单括号 {var},三语一致)。注意 ops-page 的 4 个新 stray 指标
(reclaimedInvokeCalls/strayGuardTripped/strayStandaloneRequests/strayInlineRequests)字典可能缺,需补。

## 验证(必做)
- 每改完在 `admin-ui` 跑 `npm run build`,必须绿(含 tsc)。
- 三语 key 集合一致(zh/en/ja 各自 key 数相同)。
- **重点自查:切语言实时更新**——凡是用了 i18n.t 单例的地方,确认它在组件渲染路径里被调用(而非模块 import 时)。
- 全站 grep 确认 UI 展示中文清干净(仅注释保留)。

## 铁律
- 质量只能升不能降;build 绿才算完成;不动业务逻辑/className/JSX 结构/注释。
- 不 commit(除非用户明确要求)。

## 交付
报告:①清扫了哪些组件的哪些模块级常量/函数 ②用了 A 还是 B ③补了多少 key ④build 绿否 ⑤还有无剩余。
