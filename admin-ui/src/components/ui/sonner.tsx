import { Toaster as Sonner } from 'sonner'
import { CheckCircle2, XCircle, AlertTriangle, Info } from 'lucide-react'

type ToasterProps = React.ComponentProps<typeof Sonner>

// 全站 toast 视觉重设计（呼应 .card-metal 的金属质感）：
// - 暗色金属磨砂基底：斜向渐变 + 顶部 1px 内高光 + 两层外阴影 + 背景模糊
// - 四态语义色（成功绿/错误红/警告琥珀/信息蓝）以「左侧竖条 + 图标圆底」克制呈现，不辣眼
// - 标题清晰、描述次级色，层级分明
// - 进出场沿用 sonner 自带丝滑动画，motion-reduce 下关闭过渡
// 说明：所有样式 scope 在 .toaster-metal 之下，只影响本 Toaster，不污染全局。
const TOAST_CSS = `
.toaster-metal [data-sonner-toast] {
  --metal-accent: rgba(255, 255, 255, 0.16);
  position: relative;
  background: linear-gradient(145deg, rgba(32,32,35,0.94) 0%, rgba(24,24,26,0.94) 55%, rgba(19,19,21,0.95) 100%);
  border: 1px solid rgba(255, 255, 255, 0.07);
  border-radius: 12px;
  box-shadow:
    inset 0 1px 0 rgba(255, 255, 255, 0.07),
    0 1px 3px rgba(0, 0, 0, 0.45),
    0 10px 30px -6px rgba(0, 0, 0, 0.5);
  -webkit-backdrop-filter: blur(14px) saturate(1.2);
  backdrop-filter: blur(14px) saturate(1.2);
  padding: 14px 16px 14px 18px;
  color: #ededed;
  overflow: hidden;
}

/* 左侧语义竖条：随 data-type 换色，带一点同色柔光 */
.toaster-metal [data-sonner-toast]::before {
  content: '';
  position: absolute;
  left: 0;
  top: 10px;
  bottom: 10px;
  width: 3px;
  border-radius: 0 3px 3px 0;
  background: var(--metal-accent);
  box-shadow: 0 0 10px -1px var(--metal-accent);
}
.toaster-metal [data-sonner-toast][data-type="success"] { --metal-accent: #50e3c2; }
.toaster-metal [data-sonner-toast][data-type="error"]   { --metal-accent: #f5554e; }
.toaster-metal [data-sonner-toast][data-type="warning"] { --metal-accent: #f5a623; }
.toaster-metal [data-sonner-toast][data-type="info"]    { --metal-accent: #3b93ff; }

/* 图标：圆角淡色底 + 语义色前景，做出「克制的图标底色」 */
.toaster-metal [data-sonner-toast] [data-icon] {
  width: 26px;
  height: 26px;
  margin-right: 12px;
  display: flex;
  align-items: center;
  justify-content: center;
  border-radius: 8px;
  background: color-mix(in srgb, var(--metal-accent) 15%, transparent);
  color: var(--metal-accent);
  flex-shrink: 0;
}
.toaster-metal [data-sonner-toast] [data-icon] svg {
  width: 16px;
  height: 16px;
}

/* 文案层级：标题清晰、描述次级色 */
.toaster-metal [data-sonner-toast] [data-title] {
  font-size: 13.5px;
  font-weight: 600;
  letter-spacing: -0.01em;
  color: #f2f2f2;
  line-height: 1.35;
}
.toaster-metal [data-sonner-toast] [data-description] {
  font-size: 12.5px;
  color: #9a9a9a;
  line-height: 1.45;
  margin-top: 2px;
}

/* 关闭按钮：默认低调，hover 提亮 */
.toaster-metal [data-sonner-toast] [data-close-button] {
  background: rgba(255, 255, 255, 0.05);
  border: 1px solid rgba(255, 255, 255, 0.08);
  color: #b5b5b5;
  transition: background 150ms ease, color 150ms ease;
}
.toaster-metal [data-sonner-toast] [data-close-button]:hover {
  background: rgba(255, 255, 255, 0.1);
  color: #ffffff;
}

/* 动作/取消按钮：跟随整体圆角与字重 */
.toaster-metal [data-sonner-toast] [data-button] {
  border-radius: 7px;
  font-weight: 500;
}

/* 偏好减少动效时关闭过渡（进出场位移交给 sonner，其本身也遵循该偏好） */
@media (prefers-reduced-motion: reduce) {
  .toaster-metal [data-sonner-toast],
  .toaster-metal [data-sonner-toast] [data-close-button] {
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
        className="toaster-metal"
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
