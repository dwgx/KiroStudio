/**
 * 把 `tsx-loader.mjs` 注册进模块解析链（`--import` 的入口）。
 * 单独一个文件是因为 `register()` 必须在主线程跑，而钩子本体在 loader 线程 —— 两者不能同文件。
 */
import { register } from 'node:module'

register(new URL('./tsx-loader.mjs', import.meta.url))
