// 本仓定制 workflow：从参考仓移植能力 + 对抗式评审 + 实现。
// 用法：把「要移植什么 / 参考仓哪个文件」填进 args.target 与 args.refs，
// 一条命令完成 侦察→评审→实现→验证。
//
// 参考仓（只读，均在 /tmp）：
//   ref-fox  = Foxfishc/kiro.rs   （Claude Code 体验修复最全）
//   ref-mjy  = M-JYuan/kiro.rs    （同源，弱一些，缺项看 fox）
//   ref-grey = GreyGunG/Kiro-RS-Tool（工具类 + websearch_loop）
//   kiro2cc-proxy = TsinHzl/kiro2cc-proxy（缓存 fingerprint + 端点架构）
export const meta = {
  name: 'port-and-review',
  description: '从参考仓移植能力到 kirostudio，带对抗式评审',
  phases: [
    { title: 'Recon', detail: '参考仓该能力完整机制' },
    { title: 'Review', detail: '对抗评审：会不会破坏现有行为' },
    { title: 'Implement', detail: '实现 + 测试' },
  ],
}

const REPO = '/Users/dwgx/Documents/WorkSpace/Project/kirostudio'

const HARD = `
## 硬约束
- 工作目录 ${REPO}。参考仓只读：/tmp/ref-fox /tmp/ref-mjy /tmp/ref-grey /tmp/kiro2cc-proxy
- **禁止 git 写操作**（checkout/stash/reset/commit/add）。工作树有 30+ 其他会话的未提交改动。
- **禁止全仓 cargo fmt**（历史事故冲掉过别人整树改动）。
- **不要跑 cargo build/test**（本机编译不过、8GB）。用 rg/Read 静态分析。写完测试跑不了要明说。
- 中文注释，跟随现有风格。改动要最小。每处改动配内联测试。
- 三条铁验收：①每条论断先读参考仓原文（文件:行号）再对照我们（文件:行号）；
  ②实现后自问会不会破坏现有行为；③诚实标注能/不能编译验证。

## 🔴 Rust 写码禁区（2026-08-09 全部被 CI 实际抓出过，别再犯）
1. **`r#"..."#` 内容以 `"` 结尾会提前闭合** → 用 `r##"..."##`。
   写含 JSON 片段的测试字面量时最容易中。
2. **`///` 文档注释不能用在函数参数上**（Rust 直接报 error）→ 参数说明用 `//`。
3. **截断类函数要先为省略标记预留预算**，否则「截到 max 再拼标记」结果 > max，
   违反自己的契约。
4. **状态机的"放弃"分支要同时清缓冲 + 置 latch**，否则下一 chunk 重新命中
   触发条件 → 计数归零 → 死循环永不释放。
5. **测试 helper 喂给反序列化函数的 payload 必须是对象**，不是裸字符串
   （`{"content": "..."}` 而非 `"..."`）。
6. 测试用重复字符（`"x".repeat(N)`）可能误触发本仓已有的守卫过滤器 —— 先 grep
   有没有 stray/dedup 类守卫。

## 🔴 验证纪律
- **本机编不过是常态，不是你的错**（缺 dist / 缺 node_modules）。不要为了"能编译"
  去改无关代码或降级实现。写完如实说「未编译验证」，主控会用服务器 CI 跑。
- 不要用 `strings` 查二进制、或 python 数括号来"自我验证"—— 两者本仓都实测不可靠。`

const recon = await agent(HARD + `
## 任务：侦察 ${JSON.stringify(args.refs)} 里的 ${JSON.stringify(args.target)}
目标能力：${args.goal ?? '（未给出，自行判断该能力做什么）'}
- 参考仓里它完整机制是什么（函数+行号+作用），依赖哪些其他函数
- 我们仓库有没有对应（给 文件:行号）；若有，是等价/更强/更弱
- 移植它需要改我们哪些文件哪些函数；哪些是纯新增、哪些会改行为
≤800 词，每条带 文件:行号。`, { label: '侦察', phase: 'Recon', effort: 'high' })

const review = await agent(HARD + `
## 任务：**对抗式评审**下面这个移植方案
方案：
${recon}
你的职责是破坏它。重点：
1. 移植会不会复活 CLAUDE.md 里记的已修 bug（#1-#22）？读 CLAUDE.md 对照。
2. 会不会改变我们已验证的更强行为（按区自愈 / bucket_key 含 region / EWMA 派发）？
3. 边缘情况：空输入 / 并发 / 热重载 / 与现有测试冲突。
只报能给出具体失败路径的，≤700 词，带 文件:行号。`, { label: '对抗评审', phase: 'Review', effort: 'high' })

// 只有评审通过（没发现致命失败路径）才实现
const fatal = /恒 403|必然失败|完全不可用|数据丢失|死锁|panic 必现/i.test(review)
if (fatal) {
  log('⚠️ 对抗评审发现致命问题，暂停实现，返回给主控')
  return { recon, review, verdict: 'BLOCKED', reason: review }
}

const impl = await agent(HARD + `
## 任务：实现 ${JSON.stringify(args.target)}
侦察结论：
${recon}
对抗评审（实现时规避这些坑）：
${review}
按侦察结论实现到 ${REPO}。复用我们现有函数，不复制参考仓实现。配内联测试。≤500 词报告。`, { label: '实现', phase: 'Implement', effort: 'high' })

return { recon, review, verdict: 'OK', impl }
