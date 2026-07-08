# KiroStudio Admin UI 组件规范

> 目的：统一 admin-ui 的基础表单控件，杜绝浏览器原生丑控件（原生 `<select>` 箭头、
> `<input type=number>` 的 spinner），保证暗色主题一致、键盘可达、观感统一。
> **本规范是硬约定**：新增表单一律用下列封装组件，不再直接写原生 `<select>` / `type="number"`。

## 一、下拉选择 = `Select`（`@/components/ui/select`）

纯手写触发按钮 + 绝对定位弹层，暗色主题、点击外部/Esc 关闭、上下键+回车键盘导航。
**不用**浏览器原生 `<select>`。

```tsx
import { Select } from '@/components/ui/select'

<Select
  value={ipFilter ?? ''}
  onChange={(v) => setIpFilter(v || null)}
  options={[
    { value: '', label: '全部 IP' },
    ...ipOptions.map((ip) => ({ value: ip, label: ip })),
  ]}
  className="w-36"
  aria-label="按 IP 筛选"
/>
```

Props：

| prop | 类型 | 说明 |
|---|---|---|
| `value` | `T`（string 泛型） | 当前值。空串可作“全部/未选”哨兵。 |
| `onChange` | `(value: T) => void` | 选中回调。 |
| `options` | `SelectOption<T>[]` | `{ value, label, hint?, disabled? }`。`hint` 显示为选项下方次要说明。 |
| `className` | `string` | 外层宽度类（如 `w-36`）。触发按钮固定 `h-10`。 |
| `placeholder` | `string` | 无选中时显示，默认“请选择”。 |
| `disabled` / `id` / `aria-label` | — | 常规。 |

适用：选项固定的枚举（IP 筛选、区域、模式切换等）。区域选择另有 `RegionSelect`（带搜索），见下。

## 二、数字输入 = `NumberStepper`（`@/components/ui/number-stepper`）

中间受控输入 + 右侧上下箭头（±step），长按加速连续步进。**不用** `<input type="number">`。

```tsx
import { NumberStepper } from '@/components/ui/number-stepper'

// 数值 state（推荐）
<NumberStepper value={priorityValue} onChange={setPriorityValue} min={0} className="w-24" aria-label="优先级" />

// 字符串 state（表单聚合对象常见）：用 Number()/String() 桥接
<NumberStepper
  value={Number(form.maxBodyBytes) || 0}
  onChange={(v) => set('maxBodyBytes', String(v))}
  min={0}
  step={1048576}
  className="w-40"
  aria-label="请求体上限（字节）"
/>
```

Props：

| prop | 类型 | 说明 |
|---|---|---|
| `value` | `number` | 当前值（字符串 state 用 `Number(x) || 0` 桥接）。 |
| `onChange` | `(value: number) => void` | 提交回调（字符串 state 回写用 `String(v)`）。 |
| `min` / `max` | `number` | 边界，超出自动 clamp。优先级/字节等非负量用 `min={0}`。 |
| `step` | `number` | 步进值。默认 1；字节级用 `1048576`（1MiB）、次/分钟用 `10`。 |
| `className` | `string` | 宽度类，默认 `w-16`。窄弹窗内用 `w-full`。 |
| `disabled` / `aria-label` | — | 常规。 |

行为：文本框允许中间态（空串/负号）不立刻回写；`blur`/`Enter` 提交并 clamp；`↑`/`↓` 步进。

## 三、区域选择 = `RegionSelect`（`@/components/ui/region-select`）

带搜索的区域专用下拉（选项多、需过滤）。区域字段专用；一般枚举用 `Select`。

## 四、开关 = `Switch`、勾选 = `Checkbox`

布尔开关一律用 `@/components/ui/switch`（如超额、启用/禁用、隐私开关）；
多选勾选用 `@/components/ui/checkbox`。不用原生 `checkbox`/`radio`。

---

## 五、现存待替换点清单（迁移追踪）

截至 2026-07-08 晚已全部替换完成（下列为已完成记录，防回潮参考）：

| 位置 | 原控件 | 现 | 状态 |
|---|---|---|---|
| `add-credential-dialog.tsx` 优先级 | `type=number` | NumberStepper | ✅ |
| `login-dialog.tsx`（web/idc/eidp 三处优先级） | `type=number` | NumberStepper | ✅ |
| `idc-login-dialog.tsx` 优先级 | `type=number` | NumberStepper | ✅ |
| `social-login-dialog.tsx` 优先级 | `type=number` | NumberStepper | ✅ |
| `settings-page.tsx` 请求体上限（字节） | `type=number` | NumberStepper（step 1MiB） | ✅ |
| `settings-page.tsx` 入口限流 | 早已是 NumberStepper | NumberStepper | ✅ |
| `credential-card.tsx` 优先级 | 早已是 NumberStepper | NumberStepper | ✅ |
| `usage-page.tsx` IP 筛选 | 原生 `<select>` | Select | ✅ |

### 刻意保留的例外

- `settings-page.tsx` 存储清理「保留天数」`keepDays`：保留原生 `<Input>`。
  原因：该字段是**三态**——空串表示“用默认保留天数”、非空为具体天数。NumberStepper
  始终把值强制成数字、无法表达“空=默认”，若转换会丢失该语义（0 天 ≠ 默认）。
  故此处不替换，属规范的合理例外。

---

## 六、约定

1. 新增任何数字输入用 `NumberStepper`，任何固定枚举下拉用 `Select`，区域用 `RegionSelect`。
2. 一律传 `aria-label`（无可视 label 时）保证可达性。
3. 表单聚合对象存字符串时，用 `Number()/String()` 桥接，不改底层 state 类型以免波及提交逻辑。
4. 宽度通过 `className` 控制；窄弹窗内数字步进器用 `w-full`。
5. 需要“空=默认”这类三态语义、NumberStepper 无法表达时，可保留原生 Input，但须在本表“例外”登记原因。
