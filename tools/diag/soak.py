#!/usr/bin/env python3
"""号池浸泡测试：多会话并发打 /v1/messages，量分布、延迟、错误率。

与 acceptance.py 的分工：那份是「部署是否健康」的固定检查表；本脚本是**容量/分布**探针，
用独立会话 id 让选号分散到整个池子，然后报告：
- 总成功率 + 失败明细（HTTP 状态与错误类型）
- 延迟 p50 / p95
- 每个号实际服务的请求数（打前后各取一次 by-credential 做差）

用法::
    python tools/diag/soak.py --base http://192.168.11.4:18001 \
        --admin-key sk-admin-... --api-key sk-kiro-... --per-account 4 --workers 2

只读 + 低副作用：max_tokens=16、单轮消息、无工具。
"""

import argparse
import json
import statistics
import sys
import threading
import time
import urllib.error
import urllib.request
import uuid

LOCK = threading.Lock()
RESULTS = []


def http(method, url, key=None, body=None, timeout=120, read_cap=65536):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method)
    if key:
        req.add_header("Authorization", "Bearer " + key)
    if data is not None:
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read(read_cap)
    except urllib.error.HTTPError as e:
        return e.code, e.read(read_cap)
    except Exception as e:
        return 0, str(e).encode()


def by_credential(base, admin_key):
    st, body = http("GET", base + "/api/admin/usage/by-credential", key=admin_key, timeout=30)
    if st != 200:
        return {}
    d = json.loads(body)
    rows = d if isinstance(d, list) else d.get("items", d.get("byCredential", []))
    out = {}
    for r in rows:
        out[str(r.get("key") or r.get("credentialId"))] = int(r.get("requests", 0))
    return out


def one_request(base, api_key, idx, stream):
    session = str(uuid.uuid4())
    payload = {
        "model": "claude-sonnet-4.5",
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "Reply with the single word: ok"}],
        "metadata": {
            "user_id": json.dumps(
                {
                    "device_id": "soak-device",
                    "account_uuid": str(uuid.uuid4()),
                    "session_id": session,
                }
            )
        },
    }
    if stream:
        payload["stream"] = True
    t0 = time.time()
    st, body = http("POST", base + "/v1/messages", key=api_key, body=payload)
    dt = time.time() - t0
    ok = st == 200 and (b"ok" in body.lower() or b"text_delta" in body or b'"text"' in body)
    with LOCK:
        RESULTS.append((idx, stream, st, dt, ok, body[:120]))
    return ok


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", required=True)
    ap.add_argument("--admin-key", required=True)
    ap.add_argument("--api-key", required=True)
    ap.add_argument("--per-account", type=int, default=4)
    ap.add_argument("--workers", type=int, default=2)
    ap.add_argument("--pool", type=int, default=11, help="池大小（用于估算总请求数）")
    a = ap.parse_args()
    base = a.base.rstrip("/")

    before = by_credential(base, a.admin_key)
    total = a.per_account * a.pool
    print(f"开始浸泡：{total} 个请求（{a.pool} 号 × {a.per_account}），{a.workers} 并发")

    work = list(range(total))
    lock = threading.Lock()

    def worker():
        while True:
            with lock:
                if not work:
                    return
                idx = work.pop(0)
            # 一半流式一半非流式
            one_request(base, a.api_key, idx, stream=(idx % 2 == 1))

    threads = [threading.Thread(target=worker) for _ in range(a.workers)]
    t0 = time.time()
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    elapsed = time.time() - t0

    after = by_credential(base, a.admin_key)
    results = sorted(RESULTS)
    ok_n = sum(1 for r in results if r[4])
    lat = [r[3] for r in results if r[4]]
    codes = {}
    for r in results:
        if not r[4]:
            codes[r[2]] = codes.get(r[2], 0) + 1

    print(f"\n=== 结果 ===")
    print(f"成功 {ok_n}/{len(results)}（{ok_n/max(1,len(results))*100:.1f}%），耗时 {elapsed:.1f}s")
    if lat:
        lat.sort()
        p50 = lat[len(lat) // 2]
        p95 = lat[min(len(lat) - 1, int(len(lat) * 0.95))]
        print(f"延迟 p50={p50:.2f}s p95={p95:.2f}s max={lat[-1]:.2f}s")
    if codes:
        print(f"失败状态码分布：{codes}")
        for r in results:
            if not r[4]:
                print(f"  #{r[0]} stream={r[1]} status={r[2]} body={r[5]!r}")
    print("\n=== 本次各号服务请求数（前后差值）===")
    for k in sorted(set(before) | set(after), key=lambda x: int(x) if x.isdigit() else 0):
        delta = after.get(k, 0) - before.get(k, 0)
        if delta:
            print(f"  号 {k}: +{delta}")
    served = sum(max(0, after.get(k, 0) - before.get(k, 0)) for k in after)
    print(f"（合计 {served} 条计入号维度）")
    return 0 if ok_n == len(results) else 1


if __name__ == "__main__":
    sys.exit(main())
