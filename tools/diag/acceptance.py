#!/usr/bin/env python3
"""部署验收：对一台在跑的 KiroStudio 打一轮端到端检查（只读 / 低副作用）。

与 `postdeploy.py` 的区别：那份写于 systemd 直跑时代（读 `/opt/kirostudio` + `systemctl`），
对现在的 Docker/ws 部署已失效。本脚本**只走 HTTP**，两种部署都成立，Windows 侧也能跑。

用法::

    python tools/diag/acceptance.py --base http://192.168.11.4:18001 \
        --admin-key sk-admin-... [--api-key sk-kiro-...] [--live]

判定：每条独立 PASS/FAIL，末尾汇总；**退出码 = 失败条数**（0 = 全过）。
`--live` 追加真实推理检查（需要号池非空）：非流式一次 + 流式首事件一次。
"""

import argparse
import json
import sys
import urllib.error
import urllib.request

RESULTS = []


def check(name, ok, detail=""):
    RESULTS.append((name, bool(ok), detail))
    print(("  PASS  " if ok else "  FAIL  ") + name + (("  — " + detail) if detail else ""))
    return bool(ok)


def http(method, url, key=None, body=None, timeout=30, read_cap=65536):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method)
    if key:
        req.add_header("Authorization", "Bearer " + key)
    if data is not None:
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read(read_cap), dict(r.headers)
    except urllib.error.HTTPError as e:
        return e.code, e.read(read_cap), dict(e.headers or {})
    except Exception as e:  # 连接层失败：状态 0，正文放异常文本
        return 0, str(e).encode(), {}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", required=True, help="如 http://192.168.11.4:18001")
    ap.add_argument("--admin-key", required=True)
    ap.add_argument("--api-key", default="", help="客户端密钥；给了才做 /v1 鉴权检查")
    ap.add_argument("--live", action="store_true", help="追加真实推理（需号池非空）")
    ap.add_argument("--model", default="claude-sonnet-4.5")
    a = ap.parse_args()
    base = a.base.rstrip("/")
    print("=== KiroStudio 验收 @ %s ===" % base)

    # 1. 健康检查
    st, body, _ = http("GET", base + "/healthz")
    try:
        h = json.loads(body)
    except Exception:
        h = {}
    check("GET /healthz → 200", st == 200, "status=%s" % st)
    check("healthz.ok=true", h.get("ok") is True, str(h.get("ok")))
    check("healthz 带 build_sha", bool(h.get("build_sha")), str(h.get("build_sha")))
    check("healthz.sqlite_writable", h.get("sqlite_writable") is True, str(h.get("sqlite_writable")))
    pool = h.get("pool_count")

    # 2. 模型表
    if a.api_key:
        st, body, _ = http("GET", base + "/v1/models", key=a.api_key)
        try:
            models = [m["id"] for m in json.loads(body).get("data", [])]
        except Exception:
            models = []
        check("GET /v1/models → 200", st == 200, "status=%s" % st)
        check("模型表非空", len(models) > 0, "count=%d" % len(models))
        check("含 opus 系", any("opus" in m for m in models), ",".join(models[:3]))

    # 3. 鉴权闸
    st, _, _ = http("GET", base + "/v1/models")
    check("/v1/models 无 key → 401", st == 401, "status=%s" % st)
    st, _, _ = http("GET", base + "/v1/models", key="sk-wrong-key-for-test")
    check("/v1/models 错 key → 401", st == 401, "status=%s" % st)

    # 4. 管理面鉴权
    st, _, _ = http("GET", base + "/api/admin/update/check")
    check("/api/admin/* 无 key → 401", st == 401, "status=%s" % st)

    # 5. 路径逃逸（审计 e61e714 的守卫；不得 200）。400/404 都算正确拒绝：
    #    不同 HTTP 客户端对 `..` 的处理不同（curl 会先归一化，urllib 原样发），
    #    服务端要么在路由前拒（400）要么查不到（404），**关键是别把文件吐出来**。
    st, body, _ = http("GET", base + "/admin/../../etc/passwd")
    check("静态路径逃逸被拒（400/404，非 200）", st in (400, 404),
          "status=%s body=%s" % (st, body[:80]))

    # 6. P1-1 导入端点的错误路径（缺 dbPath → 400）
    st, body, _ = http("POST", base + "/api/admin/import/kiro-cli", key=a.admin_key, body={})
    check("POST /import/kiro-cli 空 body → 400", st == 400, "status=%s body=%s" % (st, body[:120]))

    # 7. OTA 检查
    st, body, _ = http("GET", base + "/api/admin/update/check", key=a.admin_key, timeout=60)
    try:
        u = json.loads(body)
    except Exception:
        u = {}
    check("GET /update/check → 200", st == 200, "status=%s" % st)
    check("OTA 带 local_version", bool(u.get("local_version")), str(u.get("local_version")))
    check("OTA 带 available_versions", isinstance(u.get("available_versions"), list),
          "n=%s" % (len(u.get("available_versions") or [])))

    # 8. 号池 / 凭据列表
    st, body, _ = http("GET", base + "/api/admin/credentials", key=a.admin_key)
    try:
        creds = json.loads(body)
        n = len(creds) if isinstance(creds, list) else len(creds.get("credentials", []))
    except Exception:
        n = -1
    check("GET /credentials → 200", st == 200, "status=%s" % st)
    check("凭据数 = healthz.pool_count", n == pool, "list=%s healthz=%s" % (n, pool))

    # 9. 真实推理（可选）
    if a.live:
        if not pool:
            check("真实推理（跳过：号池为空）", False, "pool_count=0")
        else:
            payload = {"model": a.model, "max_tokens": 32,
                       "messages": [{"role": "user", "content": "只回复两个字：收到"}]}
            st, body, _ = http("POST", base + "/v1/messages", key=a.api_key, body=payload, timeout=120)
            ok = st == 200 and b'"text"' in body
            check("非流式 /v1/messages → 200 且有文本", ok, "status=%s body=%s" % (st, body[:160]))

            # 流式：不只看首事件，要看**整条序列**收尾（审计波动过 message_stop/收尾路径）。
            payload["stream"] = True
            st, body, _ = http("POST", base + "/v1/messages", key=a.api_key, body=payload, timeout=120)
            check("流式 /v1/messages 首事件 message_start",
                  st == 200 and b"event: message_start" in body,
                  "status=%s body=%s" % (st, body[:160]))
            check("流式序列完整（content_block_delta + message_stop）",
                  st == 200 and b"event: content_block_delta" in body and b"event: message_stop" in body,
                  "status=%s has_delta=%s has_stop=%s"
                  % (st, b"content_block_delta" in body, b"message_stop" in body))

            # 工具调用：tool_use 块必须成形（name/id/input 齐全，stop_reason=tool_use）。
            tool = {"name": "get_weather", "description": "查询指定城市的当前天气",
                    "input_schema": {"type": "object",
                                     "properties": {"city": {"type": "string"}},
                                     "required": ["city"]}}
            tpayload = {"model": a.model, "max_tokens": 256, "tools": [tool],
                        "messages": [{"role": "user", "content": "北京现在天气如何？请调用工具查询。"}]}
            st, body, _ = http("POST", base + "/v1/messages", key=a.api_key, body=tpayload, timeout=180)
            try:
                tj = json.loads(body)
            except Exception:
                tj = {}
            tblocks = [b for b in tj.get("content", []) if b.get("type") == "tool_use"]
            check("工具调用返回 tool_use 块",
                  st == 200 and bool(tblocks) and tblocks[0].get("name") == "get_weather"
                  and bool(tblocks[0].get("id")),
                  "status=%s stop=%s blocks=%s" % (st, tj.get("stop_reason"), [b.get("type") for b in tj.get("content", [])]))

            # 工具结果回程：把 tool_use 原样喂回（含 tool_result），必须拿到文本收尾。
            if tblocks:
                tb = tblocks[0]
                rpayload = {"model": a.model, "max_tokens": 128, "tools": [tool],
                            "messages": [
                                {"role": "user", "content": "北京现在天气如何？请调用工具查询。"},
                                {"role": "assistant", "content": [tb]},
                                {"role": "user", "content": [
                                    {"type": "tool_result", "tool_use_id": tb.get("id"),
                                     "content": "晴，25℃，湿度 40%"}]},
                            ]}
                st, body, _ = http("POST", base + "/v1/messages", key=a.api_key, body=rpayload, timeout=180)
                ok = st == 200 and b'"text"' in body
                check("工具结果回程 → 200 且有文本", ok, "status=%s body=%s" % (st, body[:200]))
            else:
                check("工具结果回程 → 200 且有文本", False, "上一步没拿到 tool_use，跳过回程")

            # 无参工具（对手对照 #2/#8/#9 修复的回归钉）：上游对无参调用只发一条
            # `toolUseEvent`（input:""、永不补 stop）——旧代码把它当「缺终止信号」，
            # 非流式会静默吞掉（返回 stop_reason:tool_use + content:[] 的畸形成功）或 502。
            # 这里构造一个无参数工具让模型真调，断言客户端拿到**完整** tool_use（input 为 {}）。
            noarg = {"name": "get_current_time", "description": "获取服务器当前时间，不需要任何参数",
                     "input_schema": {"type": "object", "properties": {}}}
            npayload = {"model": a.model, "max_tokens": 256, "tools": [noarg],
                        "messages": [{"role": "user", "content": "现在几点了？请调用工具查询。"}]}
            st, body, _ = http("POST", base + "/v1/messages", key=a.api_key, body=npayload, timeout=180)
            try:
                nj = json.loads(body)
            except Exception:
                nj = {}
            nblocks = [b for b in nj.get("content", []) if b.get("type") == "tool_use"]
            check("无参工具返回完整 tool_use（input={}）",
                  st == 200 and bool(nblocks) and nblocks[0].get("input") == {}
                  and bool(nblocks[0].get("id")),
                  "status=%s stop=%s blocks=%s" % (st, nj.get("stop_reason"), [b.get("type") for b in nj.get("content", [])]))
            # 流式同款：事件序列必须完整且**无 error 事件**（修前该形态整轮 Incomplete + SSE error）。
            npayload["stream"] = True
            st, body, _ = http("POST", base + "/v1/messages", key=a.api_key, body=npayload, timeout=180)
            check("无参工具流式无 error 事件且正常收尾",
                  st == 200 and b"message_stop" in body and b"event: error" not in body
                  and b"tool_use" in body,
                  "status=%s has_stop=%s has_error=%s has_tool_use=%s"
                  % (st, b"message_stop" in body, b"event: error" in body, b"tool_use" in body))

            # 扩展思考：thinking 块必须成形（上游用 <thinking> 标签表达，网关负责转块）。
            # 这条覆盖审计改动最重的 thinking_tags 解析路径（引用包裹的闭标签、未闭合残留等）。
            think = {"model": a.model, "max_tokens": 2048,
                     "thinking": {"type": "enabled", "budget_tokens": 1024},
                     "messages": [{"role": "user", "content": "笼子里鸡兔共35头94脚，各几只？一步步算。"}]}
            st, body, _ = http("POST", base + "/v1/messages", key=a.api_key, body=think, timeout=240)
            try:
                thj = json.loads(body)
            except Exception:
                thj = {}
            types = [b.get("type") for b in thj.get("content", [])]
            check("扩展思考返回 thinking 块",
                  st == 200 and "thinking" in types,
                  "status=%s blocks=%s" % (st, types))
            think["stream"] = True
            st, body, _ = http("POST", base + "/v1/messages", key=a.api_key, body=think, timeout=240)
            check("扩展思考流式含 thinking_delta 且正常收尾",
                  st == 200 and b"thinking_delta" in body and b"event: message_stop" in body,
                  "status=%s has_thinking_delta=%s has_stop=%s"
                  % (st, b"thinking_delta" in body, b"message_stop" in body))

    failed = [n for n, ok, _ in RESULTS if not ok]
    print("=== 汇总：%d 通过 / %d 失败 ===" % (len(RESULTS) - len(failed), len(failed)))
    for n in failed:
        print("  FAIL: " + n)
    return len(failed)


if __name__ == "__main__":
    sys.exit(main())
