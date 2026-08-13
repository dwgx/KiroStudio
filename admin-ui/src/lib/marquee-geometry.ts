/**
 * 框选矩形几何 —— 画布视图与行视图共用，避免同一套「矩形相交 / 两点归一化」逻辑写两份。
 *
 * ⚠️ 坐标系契约：三个导出只做纯几何，不知道 DOM 也不知道 store。调用方负责把坐标统一到
 * **同一个坐标系**（画布用「容器局部逻辑坐标」，行视图用「相对容器的本地坐标」），
 * down 定了哪个坐标系，move/up 就用同一个，本模块不负责换算。
 *
 * 从 `credential-canvas.tsx` 抽出的行为**逐字不变**（该文件内联版本是本模块的真源）。
 */

/** 拖拽阈值（px）：小于此距离视为点击而非拖拽（否则单击会被判成一次空框选而清空选区）。 */
export const DRAG_THRESHOLD = 4

/** 任意矩形。与 `CellLayout` 结构兼容，但不引画布布局类型，以免把画布概念带进通用几何。 */
export interface Rect {
  x: number
  y: number
  w: number
  h: number
}

/** 两个矩形是否相交（框选命中测试）。 */
export function intersects(a: Rect, b: Rect): boolean {
  return a.x < b.x + b.w && a.x + a.w > b.x && a.y < b.y + b.h && a.y + a.h > b.y
}

/** 把两点归一化成左上角 + 宽高的矩形（支持任意方向拖拽）。 */
export function normRect(x0: number, y0: number, x1: number, y1: number): Rect {
  return { x: Math.min(x0, x1), y: Math.min(y0, y1), w: Math.abs(x1 - x0), h: Math.abs(y1 - y0) }
}
