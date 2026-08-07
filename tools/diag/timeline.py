#!/usr/bin/env python3
"""Read-only: per-minute outcome timeline + which credential ids served, to locate
the inflection point where rate_limited stopped. Writes nothing."""
import collections
import os
import sqlite3
import time

TP = "/opt/kirostudio/data/usage/traces.db"


def main():
    con = sqlite3.connect("file:%s?mode=ro" % TP, uri=True)
    now = time.time()
    cut = int((now - 3 * 3600) * 1000)
    rows = con.execute(
        "select ts_ms, outcome, credential_id, retries, error_message "
        "from traces where ts_ms>=? order by ts_ms", (cut,)
    ).fetchall()
    print("rows in last 3h: %d" % len(rows))
    if not rows:
        return

    # 5-minute buckets
    buckets = collections.OrderedDict()
    for ts, oc, cid, rt, em in rows:
        b = int(ts // (300 * 1000))
        d = buckets.setdefault(b, collections.Counter())
        d[oc] += 1
        d["_n"] += 1

    print()
    print("%-8s %6s %8s %8s %8s %8s %8s" % (
        "localHM", "n", "success", "rate_lim", "other", "model_un", "429%"))
    for b, d in buckets.items():
        t = time.localtime(b * 300)
        n = d["_n"]
        rl = d.get("rate_limited", 0)
        print("%-8s %6d %8d %8d %8d %8d %7.1f%%" % (
            time.strftime("%H:%M", t), n, d.get("success", 0), rl,
            d.get("other_error", 0), d.get("model_unavailable", 0),
            100.0 * rl / n if n else 0.0))

    # distinct credential ids per half hour: did the pool composition change?
    print()
    print("distinct credential_id per 30min bucket:")
    cb = collections.OrderedDict()
    for ts, oc, cid, rt, em in rows:
        cb.setdefault(int(ts // (1800 * 1000)), set()).add(cid)
    for b, s in cb.items():
        t = time.localtime(b * 1800)
        ids = sorted(str(x) for x in s)
        print("  %s  count=%2d  %s" % (time.strftime("%H:%M", t), len(ids),
                                       ",".join(ids)[:150]))

    # which creds ate the 429s, all-3h
    print()
    print("rate_limited by credential_id (3h):")
    rlc = collections.Counter(str(cid) for ts, oc, cid, rt, em in rows
                              if oc == "rate_limited")
    okc = collections.Counter(str(cid) for ts, oc, cid, rt, em in rows
                              if oc == "success")
    for cid in sorted(set(rlc) | set(okc), key=lambda k: -(rlc[k] + okc[k])):
        tot = rlc[cid] + okc[cid]
        print("  cred %-8s ok=%-5d 429=%-5d  429share=%5.1f%%"
              % (cid, okc[cid], rlc[cid], 100.0 * rlc[cid] / tot if tot else 0))

    # distinct error messages for rate_limited (truncated)
    print()
    print("distinct rate_limited error_message (top 8):")
    for msg, n in collections.Counter(
            (em or "")[:160] for ts, oc, cid, rt, em in rows
            if oc == "rate_limited").most_common(8):
        print("  %5d  %s" % (n, msg))

    print()
    print("distinct other_error error_message (top 8):")
    for msg, n in collections.Counter(
            (em or "")[:160] for ts, oc, cid, rt, em in rows
            if oc == "other_error").most_common(8):
        print("  %5d  %s" % (n, msg))
    con.close()

    print()
    print("=" * 60)
    print("service / binary identity")
    print("=" * 60)
    os.system("systemctl show kirostudio -p ActiveEnterTimestamp -p NRestarts "
              "-p MainPID -p ExecMainStartTimestamp 2>/dev/null")
    os.system("pid=$(systemctl show -p MainPID --value kirostudio 2>/dev/null); "
              "echo pid=$pid; "
              "[ -n \"$pid\" ] && [ \"$pid\" != 0 ] && "
              "sha256sum /proc/$pid/exe 2>/dev/null; "
              "sha256sum /opt/kirostudio/bin/kirostudio 2>/dev/null")
    os.system("ls -la --time-style=+%Y-%m-%dT%H:%M:%S /opt/kirostudio/bin/ 2>/dev/null | head -20")
    os.system("stat -c '%y %n' /opt/kirostudio/data/config.json "
              "/opt/kirostudio/data/credentials.json 2>/dev/null")


if __name__ == "__main__":
    main()
