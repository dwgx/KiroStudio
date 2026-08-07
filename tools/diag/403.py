#!/usr/bin/env python3
"""Read-only forensics: what actually disabled the pool during the 18:25-19:10 avalanche.

Separates three distinct client-visible-429 sources:
  A) upstream 429 ThrottlingException      (real rate limiting)
  B) upstream 403 User ID ... suspended    (account-level risk control)
  C) pool exhausted / all creds disabled   (downstream consequence, credential_id NULL)
Writes nothing.
"""
import collections
import re
import sqlite3
import time

TP = "/opt/kirostudio/data/usage/traces.db"
UID_RE = re.compile(r"User ID \((\d+)\)")


def classify(outcome, em):
    e = em or ""
    if "所有凭据均已禁用" in e or "pool_permanently_exhausted" in e:
        return "C_pool_exhausted"
    if "ThrottlingException" in e or "Too many requests" in e:
        return "A_upstream_429"
    if "temporarily is suspended" in e or "可疑活动" in e:
        return "B_403_suspend"
    if "model_unsupported_by_pool" in e:
        return "D_model_unsupported"
    if not e:
        return "ok_or_blank"
    return "E_other"


def main():
    con = sqlite3.connect("file:%s?mode=ro" % TP, uri=True)
    now = time.time()
    rows = con.execute(
        "select ts_ms, outcome, credential_id, retries, error_message from traces "
        "where ts_ms>=? order by ts_ms", (int((now - 4 * 3600) * 1000),)
    ).fetchall()
    print("rows in last 4h: %d" % len(rows))

    print()
    print("--- class x outcome cross-tab (4h) ---")
    ct = collections.Counter()
    for ts, oc, cid, rt, em in rows:
        ct[(classify(oc, em), oc)] += 1
    for (cl, oc), n in sorted(ct.items(), key=lambda kv: -kv[1]):
        print("  %-20s %-20s %6d" % (cl, oc, n))

    print()
    print("--- 15-min buckets: A/B/C split ---")
    print("%-8s %6s %8s %8s %8s %8s" % ("localHM", "n", "A_429up", "B_403", "C_pool", "success"))
    b15 = collections.OrderedDict()
    for ts, oc, cid, rt, em in rows:
        d = b15.setdefault(int(ts // (900 * 1000)), collections.Counter())
        d["_n"] += 1
        d[classify(oc, em)] += 1
        if oc == "success":
            d["_ok"] += 1
    for b, d in b15.items():
        print("%-8s %6d %8d %8d %8d %8d" % (
            time.strftime("%H:%M", time.localtime(b * 900)), d["_n"],
            d["A_upstream_429"], d["B_403_suspend"], d["C_pool_exhausted"], d["_ok"]))

    print()
    print("--- B) 403 suspend: User ID -> cred ids, first/last seen ---")
    per_uid = collections.defaultdict(lambda: {"n": 0, "creds": collections.Counter(),
                                              "first": None, "last": None})
    for ts, oc, cid, rt, em in rows:
        if classify(oc, em) != "B_403_suspend":
            continue
        m = UID_RE.search(em or "")
        uid = m.group(1) if m else "unparsed"
        d = per_uid[uid]
        d["n"] += 1
        d["creds"][str(cid)] += 1
        d["first"] = d["first"] or ts
        d["last"] = ts
    for uid, d in sorted(per_uid.items(), key=lambda kv: -kv[1]["n"]):
        print("  UID %-14s n=%-4d %s -> %s  creds=%r"
              % (uid, d["n"],
                 time.strftime("%H:%M:%S", time.localtime(d["first"] / 1000)),
                 time.strftime("%H:%M:%S", time.localtime(d["last"] / 1000)),
                 dict(d["creds"])))

    print()
    print("--- A) upstream 429: which creds, when ---")
    for ts, oc, cid, rt, em in rows:
        if classify(oc, em) == "A_upstream_429":
            print("  %s cred=%-6s retries=%s"
                  % (time.strftime("%H:%M:%S", time.localtime(ts / 1000)), cid, rt))

    print()
    print("--- C) pool-exhausted messages: the (avail/total) fractions over time ---")
    frac = re.compile(r"（(\d+)/(\d+)）")
    seq = []
    for ts, oc, cid, rt, em in rows:
        if classify(oc, em) != "C_pool_exhausted":
            continue
        m = frac.search(em or "")
        seq.append((ts, m.group(0) if m else "?",
                    "PERM" if "pool_permanently_exhausted" in (em or "") else ""))
    agg = collections.OrderedDict()
    for ts, f, p in seq:
        k = (int(ts // (900 * 1000)), f, p)
        agg[k] = agg.get(k, 0) + 1
    for (b, f, p), n in agg.items():
        print("  %s  %-10s %-4s n=%d"
              % (time.strftime("%H:%M", time.localtime(b * 900)), f, p, n))
    con.close()


if __name__ == "__main__":
    main()
