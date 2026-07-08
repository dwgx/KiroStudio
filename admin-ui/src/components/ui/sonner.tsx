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
  --width: 400px;
  --gap: 13px;
  --border-radius: 14px;
}

.toaster-glow [data-sonner-toast] {
  --tone: #8b8f98;
  --tone-soft: rgba(139, 143, 152, 0.16);
  position: relative;
  width: 100%;
  min-width: 320px;
  max-width: 440px;
  overflow: hidden;
  background:
    linear-gradient(155deg, rgba(30, 30, 34, 0.95) 0%, rgba(21, 21, 24, 0.96) 100%);
  border: 1px solid rgba(255, 255, 255, 0.08);
  border-radius: 14px;
  box-shadow:
    0 1px 2px rgba(0, 0, 0, 0.4),
    0 14px 40px -10px rgba(0, 0, 0, 0.6),
    0 0 24px -12px var(--tone);
  -webkit-backdrop-filter: blur(16px) saturate(1.25);
  backdrop-filter: blur(16px) saturate(1.25);
  padding: 15px 42px 16px 16px;
  color: #ededed;
  align-items: flex-start;
}

/* 展开态下即便非最前一条也全不透明，杜绝后面看不清 */
.toaster-glow [data-sonner-toast][data-expanded="true"][data-front="false"],
.toaster-glow [data-sonner-toast][data-expanded="true"][data-front="false"][data-styled="true"] > * {
  opacity: 1;
}

/* 顶部语义色渐变描边：中间实、两端淡出，比左竖条更成体系 */
.toaster-glow [data-sonner-toast]::before {
  content: '';
  position: absolute;
  left: 0;
  right: 0;
  top: 0;
  height: 2px;
  background: linear-gradient(90deg, transparent, var(--tone) 22%, var(--tone) 78%, transparent);
  opacity: 0.9;
}

/* 底部倒计时进度条：从满宽收缩到 0，时长 = toast duration；hover 暂停时动画一并暂停 */
.toaster-glow [data-sonner-toast]::after {
  content: '';
  position: absolute;
  left: 0;
  bottom: 0;
  height: 2px;
  width: 100%;
  transform-origin: left center;
  background: linear-gradient(90deg, var(--tone), color-mix(in srgb, var(--tone) 55%, transparent));
  animation: toast-countdown var(--toast-duration, 4000ms) linear forwards;
}
.toaster-glow [data-sonner-toast]:hover::after {
  animation-play-state: paused;
}
/* 常驻/加载态（无自动消失）不显示倒计时条 */
.toaster-glow [data-sonner-toast][data-type="loading"]::after {
  display: none;
}

@keyframes toast-countdown {
  from { transform: scaleX(1); }
  to   { transform: scaleX(0); }
}

.toaster-glow [data-sonner-toast][data-type="success"] { --tone: #34e0b4; --tone-soft: rgba(52, 224, 180, 0.18); }
.toaster-glow [data-sonner-toast][data-type="error"]   { --tone: #ff5c54; --tone-soft: rgba(255, 92, 84, 0.18); }
.toaster-glow [data-sonner-toast][data-type="warning"] { --tone: #ffb020; --tone-soft: rgba(255, 176, 32, 0.18); }
.toaster-glow [data-sonner-toast][data-type="info"]    { --tone: #4c9dff; --tone-soft: rgba(76, 157, 255, 0.18); }

/* 图标：发光圆角章——语义色前景 + 同色柔光环 */
.toaster-glow [data-sonner-toast] [data-icon] {
  width: 28px;
  height: 28px;
  margin-right: 12px;
  margin-top: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  border-radius: 9px;
  background: var(--tone-soft);
  color: var(--tone);
  box-shadow: 0 0 0 1px color-mix(in srgb, var(--tone) 30%, transparent), 0 0 14px -4px var(--tone);
  flex-shrink: 0;
}
.toaster-glow [data-sonner-toast] [data-icon] svg {
  width: 16px;
  height: 16px;
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

/* 关闭按钮：常驻右上角，低调 hover 提亮 */
.toaster-glow [data-sonner-toast] [data-close-button] {
  --toast-close-button-start: unset;
  --toast-close-button-end: 0;
  --toast-close-button-transform: translate(35%, -35%);
  left: unset;
  right: 0;
  width: 20px;
  height: 20px;
  border-radius: 7px;
  background: rgba(255, 255, 255, 0.06);
  border: 1px solid rgba(255, 255, 255, 0.09);
  color: #b5b5b5;
  transition: background 150ms ease, color 150ms ease, transform 150ms ease;
}
.toaster-glow [data-sonner-toast] [data-close-button]:hover {
  background: rgba(255, 255, 255, 0.12);
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
  .toaster-glow [data-sonner-toast]::after {
    animation: none !important;
    display: none;
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
