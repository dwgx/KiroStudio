/**
 * Node `node:test` 用的 `.tsx` 加载钩子。
 *
 * # 为什么需要它
 *
 * 现有两个测试文件靠 Node 原生 TS 类型擦除直接 import `.ts`（见 proxy-line-parse.test.ts 头注释）。
 * 但 Node v24 对 `.tsx` 是 **ERR_UNKNOWN_FILE_EXTENSION** —— 原生擦除只擦类型、不认 JSX，
 * 连扩展名都不在识别列表里。而 `pickFireCandidates` 这个纯函数必须住在 `FireCanvas.tsx` 里
 * （它管的正是那个文件的 WebGL 上下文预算，拆出去就和被约束的对象分家了）。
 *
 * 于是这里用仓里**已有的** `typescript`（devDependencies，vite 构建也用它）做 transpileModule，
 * 只擦类型 + 转 JSX，不做类型检查（类型检查由 `npx tsc --noEmit` 负责）。
 * 不引入 vitest/jest/tsx/esbuild-register —— 理由同 proxy-line-parse.test.ts：为一个纯函数
 * 拉一整套 runner 代价不对等，且会动 pnpm-lock（工作区常有其他会话在改）。
 *
 * # 跑法
 *
 * ```bash
 * cd admin-ui && node --import ./tests/tsx-loader-register.mjs --test 'tests/*.test.ts'
 * ```
 *
 * 非 `.tsx` 一律透传给下一个 loader，所以带上这个钩子跑全量测试对另外两个文件完全无感。
 */
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { createRequire } from 'node:module'

// 从 admin-ui/package.json 解析 typescript，而不是从本文件 —— 钩子在 loader 线程里跑，
// 相对解析基点未必是仓内路径。
const requireFromPkg = createRequire(new URL('../package.json', import.meta.url))
const ts = requireFromPkg('typescript')

export async function load(url, context, nextLoad) {
  if (!url.endsWith('.tsx')) return nextLoad(url, context)
  const source = readFileSync(fileURLToPath(url), 'utf8')
  const { outputText } = ts.transpileModule(source, {
    fileName: fileURLToPath(url),
    compilerOptions: {
      target: ts.ScriptTarget.ES2022,
      module: ts.ModuleKind.ESNext,
      jsx: ts.JsxEmit.ReactJSX,
      // verbatim 关掉：让 TS 自行决定 import 省略，避免把 type-only import 留成运行时 import。
      isolatedModules: true,
    },
  })
  return { format: 'module', source: outputText, shortCircuit: true }
}
