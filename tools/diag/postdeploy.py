#!/usr/bin/env python3
"""Post-deploy verification: outcomes since the new binary took over, plus proof
that the two shipped fixes are reachable/behaving. Read-only."""
import collections
import os
import re
import sqlite3
import subprocess
import time

TP = "/opt/kirostudio/data/usage/traces.db"


def svc_start_epoch():
    out = subprocess.run(
        ["systemctl", "show", "kirostudio", "-p", "ActiveEnterTimestampMonotonic",
         "--value"], capture_output=True, text=True).stdout.strip()
    # fall back to parsing the human timestamp
    out2 = subprocess.run(
        ["systemctl", "show", "kirostudio", "-p", "ActiveEnterTimestamp",
         "--value"], capture_output=True, text=True).stdout.strip()
    return out2


def main():
    start_h = svc_start_epoch()
    print("service ActiveEnterTimestamp: %s" % start_h)
    # derive epoch from `date -d`
    ep = subprocess.run(["date", "-d", start_h, "+%s"], capture_output=True,
                        text=True).stdout.strip()
    try:
        start = int(ep)
    except ValueError:
        start = int(time.time() - 900)
        print("  (could not parse; using last 15min)")
    print("post-deploy window: %s -> now (%.1f min)"
          % (time.strftime("%H:%M:%S", time.localtime(start)),
             (time.time() - start) / 60))

    con = sqlite3.connect("file:%s?mode=ro" % TP, uri=True)
    rows = con.execute(
        "select outcome, retries, credential_id, error_message from traces "
        "where ts_ms>=?", (start * 1000,)).fetchall()
    n = len(rows)
    oc = collections.Counter(r[0] for r in rows)
    print()
    print("=== outcomes since new binary took over (n=%d) ===" % n)
    for k, v in oc.most_common():
        print("  %-22s %6d  %5.1f%%" % (k, v, 100.0 * v / n if n else 0))

    rl = sum(v for k, v in oc.items() if k and "rate" in str(k).lower())
    print("  --> client-visible 429 rate: %.2f%%" % (100.0 * rl / n if n else 0))

    print()
    print("=== retries distribution ===")
    rc = collections.Counter(int(r[1] or 0) for r in rows)
    print("  %r" % dict(sorted(rc.items())))
    tot = sum(k * v for k, v in rc.items())
    print("  retries_sum=%d  amplification=%.3fx" % (tot, (tot / n + 1.0) if n else 0))

    print()
    print("=== requests with NO credential (pool-exhaustion shape) ===")
    nocred = sum(1 for r in rows if r[2] is None)
    print("  %d / %d  (%.2f%%)" % (nocred, n, 100.0 * nocred / n if n else 0))

    print()
    print("=== distinct error messages (top 6) ===")
    for msg, cnt in collections.Counter(
            (r[3] or "")[:130] for r in rows if r[3]).most_common(6):
        print("  %5d  %s" % (cnt, msg))
    con.close()

    print()
    print("=== journal since deploy: do the two fixes show up? ===")
    pats = [
        ("region Skipped(新)", "region 自动探测出现「没拿到答案」的候选"),
        ("region 全确定否定(新)", "全部候选均**确定**不可用"),
        ("族级风控计数(新)", "族内连续第"),
        ("族级禁用(新)", "整族移出调度"),
        ("旧文案(应为0)", "移出调度以免每个请求都在它身上白撞"),
        ("死号禁用", "判定为死号并自动禁用"),
        ("全池自愈", "执行自愈"),
    ]
    jr = subprocess.run(
        ["journalctl", "-u", "kirostudio", "--since", start_h, "--no-pager"],
        capture_output=True, text=True).stdout
    print("  journal lines since deploy: %d" % len(jr.splitlines()))
    for label, needle in pats:
        print("  %-22s %d" % (label, jr.count(needle)))


if __name__ == "__main__":
    main()
