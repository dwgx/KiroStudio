import { Toaster as Sonner } from 'sonner'
import { CheckCircle2, XCircle, AlertTriangle, Info } from 'lucide-react'

type ToasterProps = React.ComponentProps<typeof Sonner>

// 全站 toast 视觉与行为「重写」（dwgx：notification 需重写）：
// 从旧「金属左竖条」改为**语义色顶部描边 + 发光图标章 + 底部倒计时进度条**的现代通知卡。
// - 基底：更干净的暗色玻璃（单层斜向渐变 + 背景模糊 + 柔和外阴影），不再堆金属高光。
// - 顶部一道语义色渐变描边（成功绿/错误红/警告琥珀/信息蓝），比左竖条更醒目、更成体系。
// - 图标做成「发光圆角章」：语义色前景 + 同色柔光环，替代旧的扁平淡底。
// - 底部一条语义色**倒计时进度条**：随 toast 剩余时长收缩到 0，直观知道何时消失（hover 暂停时也随之停）。
// - 标题清晰、描述次级；完整换行不截断。竖直平铺 expand + visibleToasts，多条不遮挡。
// - 关闭按钮常驻右上、低调 hover 提亮；进出场用 sonner 自带动画，motion-reduce 关过渡+进度条。
// 说明：样式全部 scope 在 .toaster-glow 之下，只影响本 Toaster，不污染全局。
const TOAST_CSS = `
.toaster-glow[data-sonner-toaster] {
  --width: 380px;
  --gap: 10px;
  --border-radius: 10px;
}

/* 干净扁平通知卡（dwgx 重写要求）：去掉一切光晕/发光/晕染（无光污染），
   纯实色暗底 + 细边框 + 左侧一道细语义色竖条。关闭叉叉常驻可见。 */
.toaster-glow [data-sonner-toast] {
  --tone: #9aa0aa;
  position: relative;
  width: 100%;
  min-width: 300px;
  max-width: 420px;
  overflow: hidden;
  background: #1c1c1f;
  border: 1px solid rgba(255, 255, 255, 0.10);
  border-left: 3px solid var(--tone);
  border-radius: 10px;
  box-shadow: 0 8px 24px -8px rgba(0, 0, 0, 0.5);
  padding: 13px 40px 13px 14px;
  color: #ededed;
  align-items: flex-start;
}

/* 修 bug(dwgx 截图:绿 toast 上方几个空白灰卡):
   sonner 折叠(未 hover)态会把非 front 的后置 toast 内容设 opacity:0 只留卡壳,
   配我这纯实色暗底 #1c1c1f 就成了"空白灰盒"堆在上面。
   处理:折叠态只显示最前一条,后置整卡隐藏(不再露空壳);hover 展开时全部完整显示。 */
.toaster-glow [data-sonner-toast][data-expanded="false"][data-front="false"] {
  opacity: 0 !important;
  pointer-events: none;
}
.toaster-glow [data-sonner-toast][data-expanded="true"],
.toaster-glow [data-sonner-toast][data-expanded="true"] > * {
  opacity: 1 !important;
}

.toaster-glow [data-sonner-toast][data-type="success"] { --tone: #2ecc9b; }
.toaster-glow [data-sonner-toast][data-type="error"]   { --tone: #ff5c54; }
.toaster-glow [data-sonner-toast][data-type="warning"] { --tone: #f0a92e; }
.toaster-glow [data-sonner-toast][data-type="info"]    { --tone: #4c9dff; }

/* 图标：扁平语义色，无光晕、无圆底章、无弹入动画 */
.toaster-glow [data-sonner-toast] [data-icon] {
  width: 18px;
  height: 18px;
  margin-right: 10px;
  margin-top: 1px;
  display: flex;
  align-items: center;
  justify-content: center;
  color: var(--tone);
  background: none;
  box-shadow: none;
  flex-shrink: 0;
}
.toaster-glow [data-sonner-toast] [data-icon] svg {
  width: 18px;
  height: 18px;
  filter: none;
}

.toaster-glow [data-sonner-toast] [data-content] {
  flex: 1 1 auto;
  min-width: 0;
}

.toaster-glow [data-sonner-toast] [data-title] {
  font-size: 13.5px;
  font-weight: 600;
  letter-spacing: -0.01em;
  color: #f4f4f5;
  line-height: 1.4;
  white-space: normal;
  word-break: break-word;
  overflow-wrap: anywhere;
}
.toaster-glow [data-sonner-toast] [data-description] {
  font-size: 12.5px;
  color: #a0a0a6;
  line-height: 1.5;
  margin-top: 3px;
  white-space: normal;
  word-break: break-word;
  overflow-wrap: anywhere;
}

/* 关闭叉叉：常驻右上角、清晰可见（实底 + 明显边框 + 亮图标），不再半透明藏起来 */
.toaster-glow [data-sonner-toast] [data-close-button] {
  --toast-close-button-start: unset;
  --toast-close-button-end: 8px;
  --toast-close-button-transform: none;
  left: unset !important;
  right: 8px !important;
  top: 10px !important;
  transform: none !important;
  width: 22px;
  height: 22px;
  border-radius: 6px;
  background: rgba(255, 255, 255, 0.10);
  border: 1px solid rgba(255, 255, 255, 0.18);
  color: #e4e4e7;
  opacity: 1;
  transition: background 150ms ease, color 150ms ease;
}
.toaster-glow [data-sonner-toast] [data-close-button] svg {
  width: 14px;
  height: 14px;
  stroke-width: 2.4;
}
.toaster-glow [data-sonner-toast] [data-close-button]:hover {
  background: rgba(255, 255, 255, 0.2);
  color: #ffffff;
}

.toaster-glow [data-sonner-toast] [data-button] {
  border-radius: 8px;
  font-weight: 500;
}

@media (prefers-reduced-motion: reduce) {
  .toaster-glow [data-sonner-toast],
  .toaster-glow [data-sonner-toast] [data-close-button] {
    transition: none !important;
  }
}
`

const Toaster = ({ ...props }: ToasterProps) => {
  return (
    <>
      <style>{TOAST_CSS}</style>
      <Sonner
        theme="dark"
        className="toaster-glow"
        // 竖直平铺展开：多条通知条条完整可见，不被折叠成堆叠隐藏后面内容
        expand
        // 同时可见数量提到 6：批量操作时后续通知也能露出，不至于全排队看不到
        visibleToasts={6}
        // 每条右上角常驻关闭按钮，用户可手动清掉
        closeButton
        // 条目之间留出明显间隔
        gap={14}
        // 合理默认时长（毫秒）；sonner 悬停自动暂停计时，保留该行为
        duration={4000}
        // 统一图标（lucide），配合上面的 [data-icon] 圆底呈现语义色
        icons={{
          success: <CheckCircle2 strokeWidth={2.25} />,
          error: <XCircle strokeWidth={2.25} />,
          warning: <AlertTriangle strokeWidth={2.25} />,
          info: <Info strokeWidth={2.25} />,
        }}
        toastOptions={{
          // 结构类保持简洁，观感主要由上面的 scoped CSS 承担
          classNames: {
            actionButton: 'group-[.toast]:bg-primary group-[.toast]:text-primary-foreground',
            cancelButton: 'group-[.toast]:bg-muted group-[.toast]:text-muted-foreground',
          },
        }}
        {...props}
      />
    </>
  )
}

export { Toaster }
