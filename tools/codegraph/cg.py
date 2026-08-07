#!/usr/bin/env python3
"""查询 .codegraph 索引 —— 读码入口,优先于 grep。

  cg.py sym <正则>              找声明(名字/qname 正则),给出 file:line
  cg.py file <路径子串>          该文件里所有声明(按行号)
  cg.py callers <name|Type::m>  谁调用它            ← 反向边
  cg.py calls <name|Type::m>    它调用谁            ← 正向边
  cg.py impls <Trait>           谁 impl 了这个 trait
  cg.py refs <name>             该符号名在何处出现(含非调用引用)
  cg.py str <正则>              字符串字面量搜索(配置键 / 错误文案 / i18n key)
  cg.py path <起点> <终点>       最短调用链(BFS)
  cg.py tests <name>            覆盖该符号的测试(调用它的 #[test])
  cg.py stat                    索引概况

每条边带 res 标签: [exact] 唯一解析 · [ambig N] 同名 N 处 · [extern] 本仓无此声明。
**ambig 与 extern 不是结论,是"这里我不确定"** —— 见 tools/codegraph/README.md §边界。
"""

import json
import os
import re
import sys
from collections import defaultdict, deque

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
GRAPH = os.environ.get("CODEGRAPH_DIR") or os.path.join(REPO, ".codegraph")


def load(name):
    p = os.path.join(GRAPH, name)
    if not os.path.exists(p):
        sys.exit(f"缺少 {p} —— 先跑 python3 tools/codegraph/build_codegraph.py")
    with open(p) as fh:
        return [json.loads(line) for line in fh if line.strip()]


def tag(e):
    r = e.get("res", "-")
    if r == "ambig":
        return f"[ambig {e.get('candidates', '?')}]"
    return f"[{r}]"


def loc(s):
    return f"{s['file']}:{s['line']}"


def show_sym(s):
    t = " (test)" if s.get("test") else ""
    own = f"  <{s['owner']}>" if s.get("owner") else ""
    print(f"{s['kind']:10} {s['qname']:52} {loc(s)}{own}{t}")
    if s.get("sig"):
        print(f"           {s['sig']}")


def match_node(key, sym):
    """调用点参数支持 `name` / `Type::method` / `Type.method` 三种写法。"""
    k = key.replace(".", "::")
    return sym == k or sym.endswith("::" + k) or sym.split("::")[-1] == k


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 1
    cmd = sys.argv[1]
    arg = sys.argv[2] if len(sys.argv) > 2 else ""

    if cmd == "stat":
        m = json.load(open(os.path.join(GRAPH, "meta.json")))
        print(f"files {m['files']}  symbols {m['symbols']}  edges {m['edges']}  "
              f"refs {m['refs']}  strings {m['strings']}")
        print("kinds     ", m["kinds"])
        print("resolution", m["resolution"])
        big = sorted(m["file_list"], key=lambda f: -f["lines"])[:12]
        print("\n最大文件:")
        for f in big:
            print(f"  {f['lines']:6}  {f['symbols']:5} syms  {f['file']}")
        return 0

    if cmd == "sym":
        pat = re.compile(arg, re.I)
        hits = [s for s in load("symbols.jsonl") if pat.search(s["qname"]) or pat.search(s["name"])]
        for s in sorted(hits, key=lambda s: (s["file"], s["line"])):
            show_sym(s)
        print(f"-- {len(hits)} 个声明", file=sys.stderr)
        return 0 if hits else 1

    if cmd == "file":
        hits = [s for s in load("symbols.jsonl") if arg in s["file"]]
        for s in sorted(hits, key=lambda s: (s["file"], s["line"])):
            show_sym(s)
        print(f"-- {len(hits)} 个声明", file=sys.stderr)
        return 0 if hits else 1

    if cmd in ("callers", "calls"):
        edges = load("edges.jsonl")
        field = "to" if cmd == "callers" else "from"
        hits = [e for e in edges if e["kind"] in ("call", "macro") and match_node(arg, e[field])]
        if not hits:
            print(f"没有匹配 {arg!r} 的边(可能是 trait 方法动态派发 / 宏生成 / 外部符号)",
                  file=sys.stderr)
            return 1
        arrow = "<-" if cmd == "callers" else "->"
        other = "from" if cmd == "callers" else "to"
        for e in sorted(hits, key=lambda e: (e[other], e["file"], e["line"])):
            print(f"{e[field]:44} {arrow} {e[other]:40} {tag(e):11} {e['file']}:{e['line']}")
            if e.get("alts") and len(e["alts"]) > 1:
                print(f"{'':44}    候选: {' | '.join(e['alts'])}")
        print(f"-- {len(hits)} 条边", file=sys.stderr)
        return 0

    if cmd == "impls":
        hits = [e for e in load("edges.jsonl") if e["kind"] == "impl" and arg in e["to"]]
        for e in sorted(hits, key=lambda e: e["from"]):
            print(f"{e['from']:44} impl {e['to']:30} {e['file']}:{e['line']}")
        print(f"-- {len(hits)} 个实现", file=sys.stderr)
        return 0 if hits else 1

    if cmd == "refs":
        hits = [r for r in load("refs.jsonl") if r["name"] == arg]
        for r in sorted(hits, key=lambda r: (r["file"], r["line"])):
            print(f"{r['file']}:{r['line']:5}  in {r.get('in') or '-'}")
        print(f"-- {len(hits)} 处引用", file=sys.stderr)
        return 0 if hits else 1

    if cmd == "str":
        pat = re.compile(arg, re.I)
        hits = [s for s in load("strings.jsonl") if pat.search(s["text"])]
        for s in hits[:400]:
            print(f"{s['file']}:{s['line']:5}  {s['text']}")
        print(f"-- {len(hits)} 处字面量" + ("(仅显示前 400)" if len(hits) > 400 else ""),
              file=sys.stderr)
        return 0 if hits else 1

    if cmd == "tests":
        syms = load("symbols.jsonl")
        tests = {s["qname"] for s in syms if s.get("test")}
        edges = load("edges.jsonl")
        direct = [e for e in edges if e["kind"] == "call" and match_node(arg, e["to"])
                  and e["from"] in tests]
        for e in sorted(direct, key=lambda e: (e["file"], e["line"])):
            print(f"{e['from']:52} {tag(e):11} {e['file']}:{e['line']}")
        print(f"-- {len(direct)} 个测试直接调用它", file=sys.stderr)
        return 0 if direct else 1

    if cmd == "path":
        if len(sys.argv) < 4:
            print("path 需要两个参数", file=sys.stderr)
            return 1
        target = sys.argv[3]
        # 只走已解析到本仓声明的边。extern 节点(tokio spawn / unwrap / assert 之类)不能过路:
        # 它们把互不相关的调用点粘成一条假链(实测 main -> spawn -> run_once 就是这么来的,
        # 两个 spawn 分属不同函数)。
        out = defaultdict(list)
        for e in load("edges.jsonl"):
            if e["kind"] in ("call", "macro") and e.get("res") != "extern":
                out[e["from"]].append(e)
        starts = [k for k in out if match_node(arg, k)]
        if not starts:
            print(f"起点 {arg!r} 不在图里", file=sys.stderr)
            return 1
        seen = set(starts)
        q = deque((s, [s]) for s in starts)
        while q:
            node, trail = q.popleft()
            if len(trail) > 1 and match_node(target, node):
                print("\n  -> ".join(trail))
                return 0
            for e in out.get(node, ()):
                if e["to"] not in seen:
                    seen.add(e["to"])
                    q.append((e["to"], trail + [e["to"]]))
        print("没找到调用链(可能经 trait 动态派发 / 闭包 / spawn 断开)", file=sys.stderr)
        return 1

    print(__doc__)
    return 1


if __name__ == "__main__":
    sys.exit(main())
