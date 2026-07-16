import { useState, type ReactNode } from 'react'
import { useTranslation } from 'react-i18next'
import { Settings2 } from 'lucide-react'
import { Button } from '@/components/ui/button'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
} from '@/components/ui/dialog'

// 齿轮展开设置卡（dwgx 偏好：小齿轮点开卡片，不走下拉）。
// 渲染一个行尾小齿轮按钮，点击打开一个较宽敞的 Dialog 放细粒度字段。
// props：title（卡标题）+ description（可选副标题）+ children（具体 Field）。
// 说明：内部用 Dialog 而非 Popover，更宽敞、移动端更友好。
export function SettingGearCard({
  title,
  description,
  children,
}: {
  title: string
  description?: string
  children: ReactNode
}) {
  const { t } = useTranslation()
  const [open, setOpen] = useState(false)
  return (
    <>
      <Button
        type="button"
        variant="ghost"
        size="icon"
        className="h-7 w-7 shrink-0 text-muted-foreground hover:text-foreground"
        onClick={() => setOpen(true)}
        title={t('settinggearcard.tooltip', { title })}
        aria-label={t('settinggearcard.openAria', { title })}
      >
        <Settings2 className="h-4 w-4" />
      </Button>
      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent className="w-[min(96vw,540px)] max-w-none">
          <DialogHeader>
            <DialogTitle className="flex items-center gap-2">
              <Settings2 className="h-4 w-4" />
              {title}
            </DialogTitle>
            {description && <DialogDescription>{description}</DialogDescription>}
          </DialogHeader>
          {/* 字段容器：与卡片内 py-0 一致，Field 自带上下 padding + 分隔线 */}
          <div className="py-0">{children}</div>
        </DialogContent>
      </Dialog>
    </>
  )
}
