# codegraph(KiroStudio 版)—— 读码入口,替代 grep

Rust + TypeScript 源码的符号/调用/引用索引。**读 AST,不读编译产物**,所以不需要先
`cargo build` 或 `pnpm build`。全量重建实测 **1.6 秒**(193 文件 / 12 万行),没有增量模式,
改完代码直接重建。

> ⚠️ 这**不是** dwgx 那套 codegraph(tree-sitter → SQLite + MCP 工具
> `codegraph_explore`/`codegraph_node`)—— 那套在 Windows 工作站上,这台 Mac 没有。
> 也**不是** `~/Documents/Project/MCPClient/tools/codegraph/`(那套读 javap 反汇编、
> 只认 Java、索引格式是 `classes.jsonl`+`edges.jsonl`)。三者同名不同物,索引与 CLI 都不兼容。

## 装 + 建

```bash
python3 -m pip install --user tree-sitter==0.25.2 \
    tree-sitter-rust==0.24.0 tree-sitter-typescript==0.23.2
python3 tools/codegraph/build_codegraph.py     # -> .codegraph/(已在 .gitignore)
```

覆盖 `src/`(Rust)、`admin-ui/src`、`admin-ui/tests`、`tools/`。

## 十个查询

```bash
python3 tools/codegraph/cg.py stat                        # 索引概况 + 最大文件排行
python3 tools/codegraph/cg.py sym 'acquire_context$'      # 找声明(正则)-> file:line + 签名
python3 tools/codegraph/cg.py file token_manager.rs       # 该文件全部声明,按行号
python3 tools/codegraph/cg.py callers acquire_context     # 谁调用它
python3 tools/codegraph/cg.py calls select_next_credential # 它调用谁
python3 tools/codegraph/cg.py impls KiroEndpoint          # 谁实现了这个 trait
python3 tools/codegraph/cg.py refs CooldownReason         # 所有出现处(含非调用引用)
python3 tools/codegraph/cg.py str credentialRpmLimit      # 字符串字面量(配置键/文案/i18n key)
python3 tools/codegraph/cg.py tests p_avail               # 哪些 #[test] 直接调用它
python3 tools/codegraph/cg.py path post_messages acquire_context   # 最短调用链
```

`callers`/`calls`/`path`/`tests` 的参数支持 `name`、`Type::method`、`Type.method` 三种写法。

## 每条边都带诚实度标签(**先读这节再下结论**)

| 标签 | 含义 | 怎么用 |
|---|---|---|
| `[exact]` | 该名字全仓唯一,或 scope 与 owner 精确对上 | 可直接采信 |
| `[ambig N]` | 同名声明 N 处,列出**全部候选** | 必须自己判断是哪个 |
| `[extern]` | 本仓没有该声明(std / 第三方 crate / npm) | 不是"没人调用",是"目标在仓外" |

当前实测分布:`exact 9876 / extern 22757 / ambig 4050`。**extern 占多数是正常的** ——
`assert_eq!`、`unwrap`、`Some`、`to_string` 这类占了大头。

`[ambig]` 最典型的来源是 **`dyn Trait` 动态派发**。例如:

```
$ cg.py callers decorate_api
CliEndpoint::decorate_api  <- KiroProvider::call_api_with_retry  [ambig 2]  src/kiro/provider.rs:1453
                               候选: CliEndpoint::decorate_api | IdeEndpoint::decorate_api
```

调用点写的是 `endpoint.decorate_api(...)`,`endpoint` 是 `dyn KiroEndpoint`,
运行时**按凭据类型**决定走 Cli 还是 Ide(见 `docs/PROTOCOL.md` §3)。索引给不出这个答案,
所以它把两个候选都列出来而不是替你选。

## 它看不见什么(全部实测过,不是免责套话)

1. **`dyn Trait` / 泛型的实际目标** —— 见上,只能给候选集。
2. **反射式 / 字符串驱动的分派** —— 路由表、`serde` 派生、按名字查表的分派断在调用点。
3. **闭包与 `tokio::spawn` 内部不断链,但跨 spawn 的"因果"不是调用边** ——
   `spawn` 里的闭包体归属外层函数,查 `path` 时链条形式上连着,语义上是异步的。
4. **宏生成的代码不存在** —— `#[derive]`、`tracing::info!` 展开后的符号不在索引里。
   宏调用本身记为 `kind=macro` 的边。**但宏体内部的调用已经补回来了**,见下条。
5. **宏体内的调用:按内层 `{}` 块重解析补回,不是全都能补。**
   tree-sitter 把宏体存成不透明 `token_tree`(`f()` 在里面只是 identifier + token_tree,
   没有 `call_expression`)。实测这让 `tokio::select!` 里的调用**全部消失** ——
   `handlers.rs` 那 4 个 `emit_stream_usage`/`emit_buffered_usage` 调用点在索引里显示
   "零调用者",而它们真实存在于 1813/1838/2838/2861。现在逐个内层 `{}` 块当 Rust 块
   重新 parse 补回(实测这 4 条行号与 grep 完全一致)。
   **补不回的**:`json!{"k": v}`、`select!` 的 arm 头部(`x = f() => `)这类天然不是合法
   Rust 的片段 —— 放弃而不是硬猜,所以不会造假边,但那里的调用仍然看不见。
6. **`refs` 只保留指向已声明符号的名字**,局部变量被丢掉(否则全是噪音)。
7. **同名字段/变体会污染候选** —— `ParseError.max` 这类是把字段名当成了方法名的候选,
   `[ambig]` 会标出来。
8. **行号是"名字自己所在行"** —— 多行链式调用(`self\n.token_manager\n.acquire_context()`)
   取的是 `acquire_context` 那一行,与 grep 一致。这一点踩过:早期版本取 `call_expression`
   起点,结果比 grep 少 2 行,跳过去看不到调用点。

## 已经踩过并修掉的三个假阳性(**别在新工具里重犯**)

**① `tokio::spawn` 被解析成本仓的 `refresh_loop::spawn`。** 名字全仓唯一 ⇒ 早期版本
认定"就是它",于是 `cg.py path main run_once` 吐出 `main -> spawn -> run_once` ——
一条**根本不存在**的链。修法:自由函数带路径限定时(`tokio::spawn`),路径末段对不上任何
候选的模块名即判 `extern`。

修掉之后同一条查询给出的是真链,逐跳都对得上源码:

```
main -> MultiTokenManager::respawn_refresh_task -> spawn -> run_once
        main.rs:418        token_manager.rs:2572    refresh_loop.rs:54
```

**② `path` 曾允许经 `extern` 节点过路。** `unwrap`/`assert` 这种节点会把互不相关的
调用点粘成假链。现在 BFS 只走已解析到本仓声明的边。

**③ 假阴性:宏体内的调用点全部隐形。** 这个最危险 —— 前两个是"多给了错的",这个是
**"少给了对的"**,而 `callers` 返回空会被直接读成"没人调用它"。实测拿
`emit_stream_usage` 查调用者返回 0 条,而 grep 明确有 4 处
(`handlers.rs:1813/1838/2838/2861`,全在 `tokio::select!` 里)。修法见上 §看不见什么 第 5 条。

> 三个都是拿 grep 交叉验证才暴露的。**"零结果"尤其要复核** —— 它长得像结论,
> 实际可能是工具的盲区。

> 教训与 `CLAUDE.md` 那条一致:**索引"跑通了"不等于"结论对"**。这三个假阳性/假阴性都是在拿
> grep 交叉验证时才暴露的 —— 用 codegraph 得出关键结论前,至少对一条边做一次独立核对,
> **`callers` 返回空必须 grep 复核**。

## 什么时候还是得用 grep / Read

- 要看具体实现和上下文 —— codegraph 只给 `file:line` 和签名。
- 追宏展开、`derive`、路由注册表这类静态不可解析的边。
- 注释内容(注释不进索引,但 `str` 命令能搜到 Rust 字符串字面量,含跨行 `assert!` 文案)。
- 判断 `[ambig]` 究竟走哪条分支。
