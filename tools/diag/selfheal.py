#!/usr/bin/env python3
"""Decisive test: does the RUNNING binary contain the SuspiciousActivityAuto self-heal fix?

Two independent probes:
  1. journalctl: count "判定为死号并自动禁用" vs "执行自愈" today. If disable>0 and heal==0,
     the running code has the pre-fix behaviour (self-heal only matched TooManyFailures).
  2. byte-scan /proc/<pid>/exe for the exact UTF-8 log literals. Per repo rule, use Python
     byte search -- `grep -a` false-positives and `strings|grep` splits UTF-8.

Read-only.
"""
import os
import re
import subprocess

NEEDLES = {
    "self_heal_log": "所有凭据均已被自动禁用，执行自愈",
    "self_heal_backoff": "全池自愈处于退避期",
    "dead_disable": "判定为死号并自动禁用",
    "pool_all_disabled": "所有凭据均已禁用",
    # P0 markers from the handoff, to see what else is/isn't live
    "kiro_cli_origin": "KIRO_CLI",
    "upstream_trace": "upstream_trace",
}


def run(cmd):
    try:
        return subprocess.run(cmd, shell=True, capture_output=True, text=True,
                              timeout=120).stdout
    except Exception as e:  # noqa: BLE001
        return "ERR %s" % e


def main():
    pid = run("systemctl show -p MainPID --value kirostudio").strip()
    print("MainPID = %r" % pid)
    exe = "/proc/%s/exe" % pid

    print()
    print("=" * 74)
    print("PROBE 1 - journalctl counts (today)")
    print("=" * 74)
    for label, needle in (("dead_disable", "判定为死号并自动禁用"),
                          ("self_heal_exec", "执行自愈"),
                          ("self_heal_backoff", "全池自愈处于退避期"),
                          ("suspicious_hit", "账户级风控连续第"),
                          ("suspicious_cooldown", "触发账户级可疑活动风控")):
        out = run("journalctl -u kirostudio --since today --no-pager 2>/dev/null "
                  "| grep -c -- %s" % _q(needle))
        print("  %-22s %s" % (label, out.strip() or "0"))

    print()
    print("  --- sample: most recent 6 dead-disable lines ---")
    out = run("journalctl -u kirostudio --since today --no-pager 2>/dev/null "
              "| grep -- %s | tail -6" % _q("判定为死号并自动禁用"))
    for ln in out.splitlines():
        print("    " + ln[:190])

    print()
    print("  --- sample: any self-heal lines at all ---")
    out = run("journalctl -u kirostudio --since today --no-pager 2>/dev/null "
              "| grep -- %s | tail -6" % _q("自愈"))
    for ln in (out.splitlines() or ["    (none)"]):
        print("    " + ln[:190])

    print()
    print("=" * 74)
    print("PROBE 2 - byte-scan running binary (%s)" % exe)
    print("=" * 74)
    try:
        with open(exe, "rb") as fh:
            blob = fh.read()
        print("  size = %d bytes" % len(blob))
        for label, needle in NEEDLES.items():
            n = blob.count(needle.encode("utf-8"))
            print("  %-20s count=%-4d %s" % (label, n, "PRESENT" if n else "ABSENT"))
    except Exception as e:  # noqa: BLE001
        print("  !! cannot read %s: %s" % (exe, e))

    print()
    print("  --- for comparison: same scan on on-disk binary ---")
    try:
        with open("/opt/kirostudio/bin/kirostudio", "rb") as fh:
            blob2 = fh.read()
        for label, needle in NEEDLES.items():
            print("  %-20s count=%-4d" % (label, blob2.count(needle.encode("utf-8"))))
    except Exception as e:  # noqa: BLE001
        print("  !! %s" % e)

    print()
    print("=" * 74)
    print("PROBE 3 - version string in running binary")
    print("=" * 74)
    try:
        with open(exe, "rb") as fh:
            blob = fh.read()
        for m in set(re.findall(rb"0\.7\.\d+", blob)):
            print("  found version literal: %s" % m.decode())
    except Exception as e:  # noqa: BLE001
        print("  !! %s" % e)


def _q(s):
    return "'" + s.replace("'", "'\\''") + "'"


if __name__ == "__main__":
    main()
