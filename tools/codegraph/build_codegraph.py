#!/usr/bin/env python3
"""为 KiroStudio(Rust + TypeScript) 建符号 / 调用 / 引用索引 -> .codegraph/

    python3 -m pip install --user tree-sitter==0.25.2 \
        tree-sitter-rust==0.24.0 tree-sitter-typescript==0.23.2
    python3 tools/codegraph/build_codegraph.py

产物(全部 JSONL,gitignored):
    symbols.jsonl  声明: qname/kind/file/line/owner/sig
    edges.jsonl    关系: call / macro / impl / use / contains
    refs.jsonl     引用: 已知符号名在何处出现(替代 grep 找用法)
    strings.jsonl  字符串字面量(替代 grep 找配置键 / 错误文案 / i18n key)
    meta.json      文件清单 + 计数 + 解析口径

它读**源码 AST**,不读编译产物,所以无需先 cargo build / pnpm build。
"""

import json
import os
import re
import sys
from collections import defaultdict

import tree_sitter_rust
import tree_sitter_typescript as tsts
from tree_sitter import Language, Parser

sys.setrecursionlimit(20000)

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
OUT = os.environ.get("CODEGRAPH_DIR") or os.path.join(REPO, ".codegraph")
ROOTS = ["src", "admin-ui/src", "admin-ui/tests", "tools"]
SKIP = {"node_modules", "target", "dist", ".git", ".codegraph", "__pycache__"}

RUST = Parser(Language(tree_sitter_rust.language()))
TS = Parser(Language(tsts.language_typescript()))
TSX = Parser(Language(tsts.language_tsx()))

symbols = []  # 声明
edges = []  # 已解析的关系
raw_calls = []  # 待解析的调用点
refs = []  # (name, file, line, enclosing)
strings = []  # (text, file, line)
files_meta = []


def txt(node):
    return node.text.decode("utf8", "replace") if node is not None else ""


def flat(s, limit=220):
    s = re.sub(r"\s+", " ", s).strip()
    return s[:limit]


def add_sym(qname, name, kind, path, node, owner=None, lang="rust", sig="", test=False):
    symbols.append(
        {
            "qname": qname,
            "name": name,
            "kind": kind,
            "file": path,
            "line": node.start_point[0] + 1,
            "end": node.end_point[0] + 1,
            "owner": owner,
            "lang": lang,
            "sig": flat(sig),
            "test": test,
        }
    )


def attrs_before(node, depth=6):
    """收集紧邻前面的 attribute_item 文本(Rust 属性是兄弟节点,不是子节点)。"""
    out, cur = [], node.prev_sibling
    while cur is not None and depth > 0 and cur.type in ("attribute_item", "line_comment", "block_comment"):
        if cur.type == "attribute_item":
            out.append(txt(cur))
        cur, depth = cur.prev_sibling, depth - 1
    return out


def reparse_token_tree(node, path, mod, owner, fnq, in_test):
    """把宏体里的 `{...}` 块当 Rust 块重新解析,补回里面的调用边。

    tree-sitter 把宏体存成**不透明 token_tree**:`f()` 在里面是 identifier + token_tree,
    没有 call_expression 节点。实测后果:`tokio::select!` 里的调用全部不可见 ——
    `handlers.rs` 那 4 个 `emit_stream_usage`/`emit_buffered_usage` 调用点(客户端断连丢
    记录那条缺陷的现场)在索引里显示零调用者,而它们真实存在。

    为什么按**内层 `{}` 块**而不是整个宏体:`select!` 的 arm 形如
    `x = f() => {...}`,整体不是合法 Rust(包一层 fn 仍 has_error)。但每个 arm 的
    `{...}` **块体本身是合法的**,逐块 parse 即可。解析不过就往里再降一层;
    始终不过就放弃该块(不产生假边)。`json!` 这类内容天然不是 Rust,会走到放弃。
    """
    for child in node.children:
        if child.type != "token_tree":
            continue
        if child.text[:1] == b"{":
            tree = RUST.parse(b"fn __cg_macro_block() " + child.text)
            if not tree.root_node.has_error:
                fn = tree.root_node.child(0)
                body = fn.child_by_field_name("body") if fn is not None else None
                if body is not None:
                    _absorb(body, child.start_point[0], path, mod, owner, fnq, in_test)
                    continue
        reparse_token_tree(child, path, mod, owner, fnq, in_test)


def _absorb(body, base, path, mod, owner, fnq, in_test):
    """遍历重解析出来的块,并把相对行号折算成文件绝对行号。"""
    before = len(symbols), len(raw_calls), len(refs), len(strings)
    for c in body.children:
        walk_rust(c, path, mod, owner, fnq, in_test)
    for i in range(before[0], len(symbols)):
        symbols[i]["line"] += base
        symbols[i]["end"] += base
    for i in range(before[1], len(raw_calls)):
        r = list(raw_calls[i])
        r[5] += base
        raw_calls[i] = tuple(r)
    for i in range(before[2], len(refs)):
        r = list(refs[i])
        r[2] += base
        refs[i] = tuple(r)
    for i in range(before[3], len(strings)):
        s = list(strings[i])
        s[2] += base
        strings[i] = tuple(s)


def callee_line(f):
    """被调用名字自身所在行(1-based)。多行链式调用里它与 call_expression 起点不同行。"""
    if f is None:
        return None
    for field in ("field", "name", "property"):
        n = f.child_by_field_name(field)
        if n is not None:
            return n.start_point[0] + 1
    return f.start_point[0] + 1


def rust_sig(fn):
    """取 `fn name(...) -> T` 那一段,不含函数体。"""
    body = fn.child_by_field_name("body")
    end = body.start_byte if body is not None else fn.end_byte
    return fn.text[: end - fn.start_byte].decode("utf8", "replace")


def walk_rust(node, path, mod, owner, fnq, in_test):
    """owner=当前 impl/trait 的类型名; fnq=当前所在函数的 qname; in_test=是否在测试模块内。"""
    t = node.type

    if t == "mod_item":
        name = txt(node.child_by_field_name("name"))
        is_test = in_test or any("cfg(test)" in a.replace(" ", "") for a in attrs_before(node))
        sub = f"{mod}::{name}" if mod else name
        add_sym(sub, name, "mod", path, node, None, "rust", f"mod {name}", is_test)
        body = node.child_by_field_name("body")
        if body is not None:
            for c in body.children:
                walk_rust(c, path, sub, None, None, is_test)
        return

    if t in ("impl_item", "trait_item"):
        if t == "impl_item":
            ty = txt(node.child_by_field_name("type"))
            tr = node.child_by_field_name("trait")
            own = ty
            if tr is not None:
                edges.append({"from": ty, "to": txt(tr), "kind": "impl", "file": path,
                              "line": node.start_point[0] + 1, "res": "exact"})
        else:
            own = txt(node.child_by_field_name("name"))
            add_sym(own, own, "trait", path, node, None, "rust", f"trait {own}", in_test)
        body = node.child_by_field_name("body")
        if body is not None:
            for c in body.children:
                walk_rust(c, path, mod, own.split("<")[0].strip(), None, in_test)
        return

    if t == "function_item":
        name = txt(node.child_by_field_name("name"))
        q = f"{owner}::{name}" if owner else name
        at = attrs_before(node)
        is_test = in_test or any("[test]" in a or "[tokio::test]" in a for a in at)
        kind = "test" if is_test and not owner else ("method" if owner else "fn")
        add_sym(q, name, kind, path, node, owner, "rust", rust_sig(node), is_test)
        body = node.child_by_field_name("body")
        if body is not None:
            for c in body.children:
                walk_rust(c, path, mod, owner, q, is_test)
        return

    if t in ("struct_item", "enum_item", "union_item"):
        nn = node.child_by_field_name("name")
        name = txt(nn)
        kind = {"struct_item": "struct", "enum_item": "enum", "union_item": "union"}[t]
        add_sym(name, name, kind, path, node, None, "rust", f"{kind} {name}", in_test)
        for c in node.children:
            walk_rust(c, path, mod, name, None, in_test)
        return

    if t == "field_declaration" and owner:
        name = txt(node.child_by_field_name("name"))
        if name:
            add_sym(f"{owner}.{name}", name, "field", path, node, owner, "rust", txt(node), in_test)

    if t == "enum_variant" and owner:
        name = txt(node.child_by_field_name("name"))
        if name:
            add_sym(f"{owner}::{name}", name, "variant", path, node, owner, "rust", txt(node), in_test)

    if t in ("const_item", "static_item", "type_item", "macro_definition"):
        nn = node.child_by_field_name("name")
        if nn is not None:
            name = txt(nn)
            kind = {"const_item": "const", "static_item": "static",
                    "type_item": "type", "macro_definition": "macro"}[t]
            q = f"{owner}::{name}" if owner else name
            add_sym(q, name, kind, path, node, owner, "rust", flat(txt(node), 160), in_test)

    if t == "use_declaration":
        edges.append({"from": mod or path, "to": flat(txt(node.child_by_field_name("argument")), 120),
                      "kind": "use", "file": path, "line": node.start_point[0] + 1, "res": "exact"})

    if t in ("call_expression", "macro_invocation") and fnq:
        f = node.child_by_field_name("function") or node.child_by_field_name("macro")
        if f is not None:
            kind = "macro" if t == "macro_invocation" else "call"
            ft, callee, scope = f.type, None, None
            if ft == "identifier":
                callee = txt(f)
            elif ft == "field_expression":
                callee = txt(f.child_by_field_name("field"))
                v = f.child_by_field_name("value")
                scope = "self" if v is not None and v.type == "self" else txt(v)[:40]
            elif ft == "scoped_identifier":
                callee = txt(f.child_by_field_name("name"))
                p = f.child_by_field_name("path")
                scope = txt(p) if p is not None else None
            if callee and re.fullmatch(r"[A-Za-z_]\w*", callee):
                # 行号取**被调名字自己**的位置,不取 call_expression 起点:多行链式调用
                # (`self\n  .token_manager\n  .acquire_context(..)`) 的起点在 receiver 那行,
                # 与 grep 结果差几行,跳过去看不到调用点。
                raw_calls.append((fnq, callee, scope, kind, path,
                                  callee_line(f) or node.start_point[0] + 1, owner, "rust"))
        if t == "macro_invocation" and fnq:
            tt = node.child_by_field_name("token_tree") or (
                node.children[-1] if node.children and node.children[-1].type == "token_tree" else None)
            if tt is not None:
                reparse_token_tree(tt, path, mod, owner, fnq, in_test)
                return  # 宏体已由 reparse 走完,别再让默认遍历重复扫一遍 token

    if t in ("identifier", "type_identifier", "field_identifier"):
        refs.append((txt(node), path, node.start_point[0] + 1, fnq or owner or mod))
    elif t == "string_literal":
        s = flat(txt(node).strip('"'), 200)
        if 2 <= len(s) <= 200:
            strings.append((s, path, node.start_point[0] + 1))

    for c in node.children:
        walk_rust(c, path, mod, owner, fnq, in_test)


def ts_kind(name, has_jsx):
    if name.startswith("use") and len(name) > 3 and name[3].isupper():
        return "hook"
    if has_jsx and name[:1].isupper():
        return "component"
    return "fn"


def ts_sig(node, name):
    p = node.child_by_field_name("parameters")
    r = node.child_by_field_name("return_type")
    return f"{name}{txt(p)}{txt(r)}"


def walk_ts(node, path, owner, fnq, in_test):
    t = node.type

    if t in ("function_declaration", "generator_function_declaration"):
        name = txt(node.child_by_field_name("name"))
        q = f"{owner}.{name}" if owner else name
        has_jsx = b"jsx_element" in node.type.encode() or "<" in txt(node)[-400:]
        add_sym(q, name, ts_kind(name, has_jsx), path, node, owner, "ts", ts_sig(node, name), in_test)
        b = node.child_by_field_name("body")
        if b is not None:
            for c in b.children:
                walk_ts(c, path, owner, q, in_test)
        return

    if t in ("class_declaration", "abstract_class_declaration"):
        name = txt(node.child_by_field_name("name"))
        add_sym(name, name, "class", path, node, None, "ts", f"class {name}", in_test)
        h = node.child_by_field_name("body")
        if h is not None:
            for c in h.children:
                walk_ts(c, path, name, None, in_test)
        return

    if t == "method_definition" and owner:
        name = txt(node.child_by_field_name("name"))
        q = f"{owner}.{name}"
        add_sym(q, name, "method", path, node, owner, "ts", ts_sig(node, name), in_test)
        b = node.child_by_field_name("body")
        if b is not None:
            for c in b.children:
                walk_ts(c, path, owner, q, in_test)
        return

    if t == "variable_declarator":
        v = node.child_by_field_name("value")
        name = txt(node.child_by_field_name("name"))
        if v is not None and v.type in ("arrow_function", "function_expression") and name:
            q = f"{owner}.{name}" if owner else name
            src = txt(v)
            has_jsx = "</" in src or "/>" in src
            add_sym(q, name, ts_kind(name, has_jsx), path, node, owner, "ts",
                    f"{name} = {flat(src[:120])}", in_test)
            for c in v.children:
                walk_ts(c, path, owner, q, in_test)
            return

    if t in ("interface_declaration", "type_alias_declaration", "enum_declaration"):
        name = txt(node.child_by_field_name("name"))
        kind = {"interface_declaration": "interface", "type_alias_declaration": "type",
                "enum_declaration": "enum"}[t]
        add_sym(name, name, kind, path, node, None, "ts", f"{kind} {name}", in_test)
        for c in node.children:
            walk_ts(c, path, name, None, in_test)
        return

    if t in ("property_signature", "public_field_definition") and owner:
        nn = node.child_by_field_name("name")
        if nn is not None:
            name = txt(nn)
            add_sym(f"{owner}.{name}", name, "field", path, node, owner, "ts",
                    flat(txt(node), 160), in_test)

    if t == "import_statement":
        src = node.child_by_field_name("source")
        edges.append({"from": path, "to": txt(src).strip("'\""), "kind": "use",
                      "file": path, "line": node.start_point[0] + 1, "res": "exact"})

    if t in ("call_expression", "new_expression") and fnq:
        f = node.child_by_field_name("function") or node.child_by_field_name("constructor")
        if f is not None:
            callee, scope = None, None
            if f.type == "identifier":
                callee = txt(f)
            elif f.type == "member_expression":
                callee = txt(f.child_by_field_name("property"))
                o = f.child_by_field_name("object")
                scope = "self" if o is not None and o.type == "this" else txt(o)[:40]
            if callee and re.fullmatch(r"[A-Za-z_$]\w*", callee):
                raw_calls.append((fnq, callee, scope, "call", path,
                                  callee_line(f) or node.start_point[0] + 1, owner, "ts"))

    if t in ("identifier", "type_identifier", "property_identifier", "shorthand_property_identifier"):
        refs.append((txt(node), path, node.start_point[0] + 1, fnq or owner))
    elif t in ("string_fragment", "template_string"):
        s = flat(txt(node), 200)
        if 2 <= len(s) <= 200:
            strings.append((s, path, node.start_point[0] + 1))

    for c in node.children:
        walk_ts(c, path, owner, fnq, in_test)


def iter_files():
    for root in ROOTS:
        base = os.path.join(REPO, root)
        if not os.path.isdir(base):
            continue
        for dirpath, dirnames, names in os.walk(base):
            dirnames[:] = [d for d in dirnames if d not in SKIP]
            for n in sorted(names):
                if n.endswith((".rs", ".ts", ".tsx")):
                    full = os.path.join(dirpath, n)
                    yield full, os.path.relpath(full, REPO).replace(os.sep, "/")


def resolve():
    """把调用点解析成边。res 字段是本工具的诚实度标签,查询时会显示:

    exact  —— 该名字全仓只有一个声明,或 scope 与 owner 精确对上
    ambig  —— 同名声明多处,列第一个但标记歧义(N 个候选)
    extern —— 本仓没有该声明(std / 第三方 crate / npm)
    """
    by_name = defaultdict(list)
    for s in symbols:
        by_name[s["name"]].append(s)

    for caller, callee, scope, kind, path, line, owner, lang in raw_calls:
        cands = by_name.get(callee, [])
        cands = [c for c in cands if c["kind"] not in ("field", "variant", "mod")] or cands
        if not cands:
            edges.append({"from": caller, "to": callee, "kind": kind, "file": path,
                          "line": line, "res": "extern", "scope": scope})
            continue
        pick, res = None, "ambig"
        if scope == "self" and owner:
            same = [c for c in cands if c["owner"] == owner]
            if len(same) == 1:
                pick, res = same[0], "exact"
        if pick is None and scope:
            sc = scope.split("::")[-1].split(".")[-1]
            same = [c for c in cands if c["owner"] == sc]
            if len(same) == 1:
                pick, res = same[0], "exact"
        if pick is None:
            samefile = [c for c in cands if c["file"] == path]
            if len(samefile) == 1:
                pick, res = samefile[0], "exact" if len(cands) == 1 else "ambig"
        if pick is None and scope and all(c["owner"] is None for c in cands):
            # 自由函数 + 带路径限定(`tokio::spawn`):名字全仓唯一不足以认定是本仓那一个。
            # 实测 `tokio::spawn` 被解析成 `kiro::refresh_loop::spawn`,于是 path 查询吐出
            # `main -> spawn -> run_once` 这条根本不存在的链。路径末段对不上模块名即判 extern。
            seg = scope.split("::")[-1].split(".")[-1].strip()
            if not any(os.path.splitext(os.path.basename(c["file"]))[0] == seg for c in cands):
                edges.append({"from": caller, "to": f"{scope}::{callee}", "kind": kind,
                              "file": path, "line": line, "res": "extern", "scope": scope})
                continue
        if pick is None:
            pick = cands[0]
            res = "exact" if len(cands) == 1 else "ambig"
        e = {"from": caller, "to": pick["qname"], "kind": kind, "file": path,
             "line": line, "res": res, "target_file": pick["file"], "target_line": pick["line"]}
        if res == "ambig":
            # 记下**全部**候选:`dyn Trait` 派发时 pick 只是任取一个,单独显示会诱导错误结论
            # (endpoint.decorate_api 会被记成 CliEndpoint,而它同样可能是 IdeEndpoint)。
            e["candidates"] = len(cands)
            e["alts"] = sorted({c["qname"] for c in cands})[:6]
        if scope:
            e["scope"] = scope
        edges.append(e)


def main():
    os.makedirs(OUT, exist_ok=True)
    known = set()
    for full, rel in iter_files():
        try:
            src = open(full, "rb").read()
        except OSError as err:
            print(f"  skip {rel}: {err}", file=sys.stderr)
            continue
        n0, e0 = len(symbols), len(edges)
        is_test_file = "/tests/" in rel or rel.endswith((".test.ts", ".test.tsx"))
        if rel.endswith(".rs"):
            walk_rust(RUST.parse(src).root_node, rel, None, None, None, is_test_file)
        else:
            parser = TSX if rel.endswith(".tsx") else TS
            walk_ts(parser.parse(src).root_node, rel, None, None, is_test_file)
        files_meta.append({"file": rel, "lines": src.count(b"\n") + 1,
                           "symbols": len(symbols) - n0, "test": is_test_file})
        known.add(rel)

    resolve()

    # refs 只保留指向已声明符号的,否则全是局部变量噪音
    names = {s["name"] for s in symbols}
    kept_refs = [r for r in refs if r[0] in names]

    def dump(fname, rows):
        with open(os.path.join(OUT, fname), "w") as fh:
            for r in rows:
                fh.write(json.dumps(r, ensure_ascii=False) + "\n")
        return len(rows)

    ns = dump("symbols.jsonl", symbols)
    seen, uniq = set(), []
    for e in edges:
        k = (e["from"], e["to"], e["kind"], e["file"], e["line"])
        if k not in seen:
            seen.add(k)
            uniq.append(e)
    ne = dump("edges.jsonl", uniq)
    nr = dump("refs.jsonl", [{"name": a, "file": b, "line": c, "in": d} for a, b, c, d in kept_refs])
    nstr = dump("strings.jsonl", [{"text": a, "file": b, "line": c} for a, b, c in strings])

    kinds = defaultdict(int)
    for s in symbols:
        kinds[s["kind"]] += 1
    res_counts = defaultdict(int)
    for e in uniq:
        res_counts[e.get("res", "-")] += 1

    meta = {
        "repo": REPO,
        "roots": ROOTS,
        "files": len(files_meta),
        "symbols": ns,
        "edges": ne,
        "refs": nr,
        "strings": nstr,
        "kinds": dict(sorted(kinds.items(), key=lambda kv: -kv[1])),
        "resolution": dict(res_counts),
        "file_list": files_meta,
    }
    with open(os.path.join(OUT, "meta.json"), "w") as fh:
        json.dump(meta, fh, ensure_ascii=False, indent=1)

    print(f"{OUT}")
    print(f"  files    {len(files_meta)}")
    print(f"  symbols  {ns}  {dict(list(meta['kinds'].items())[:10])}")
    print(f"  edges    {ne}  {dict(res_counts)}")
    print(f"  refs     {nr}   strings {nstr}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
