#!/usr/bin/env python3
"""Read-only pool diagnostics: keyhash / cloneGroup aggregation + live config + 429 rates.

Purpose: answer "is the capacity denominator real?" before touching anything.
Writes nothing. Reads credentials.json, config.json, traces.db (mode=ro).
"""
import collections
import hashlib
import json
import os
import sqlite3
import time

DATA = "/opt/kirostudio/data"


def load_creds():
    p = os.path.join(DATA, "credentials.json")
    with open(p, "r", encoding="utf-8") as fh:
        d = json.load(fh)
    if isinstance(d, list):
        return d
    if isinstance(d, dict):
        for key in ("credentials", "items", "list"):
            if isinstance(d.get(key), list):
                return d[key]
        return [d]
    return []


def short(s):
    return hashlib.sha256(s.encode()).hexdigest()[:12] if s else "no-key"


def main():
    print("=" * 72)
    print("SECTION 1 — credential pool identity")
    print("=" * 72)
    try:
        items = load_creds()
    except Exception as e:  # noqa: BLE001
        print("  !! cannot read credentials.json:", type(e).__name__, e)
        items = []

    print("  total credential entries: %d" % len(items))

    # Which JSON keys actually appear (field names differ from what docs claim).
    keyset = collections.Counter()
    for x in items:
        for k in x.keys():
            keyset[k] += 1
    print("  keys present (name:count):")
    for k, n in sorted(keyset.items()):
        print("      %-28s %d" % (k, n))

    # Candidate api-key fields, since exact name is unverified.
    apikey_fields = [k for k in keyset if "apikey" in k.lower() or "api_key" in k.lower()]
    print("  candidate api-key fields: %r" % (apikey_fields,))

    kh = collections.Counter()
    for x in items:
        raw = ""
        for f in apikey_fields:
            v = x.get(f)
            if isinstance(v, str) and v:
                raw = v
                break
        kh[short(raw)] += 1
    print("  keyhash distribution: %r" % (dict(kh),))

    cg = collections.Counter(str(x.get("cloneGroup", x.get("clone_group"))) for x in items)
    print("  cloneGroup distribution: %r" % (dict(cg),))
    cs = collections.Counter(str(x.get("cloneSeq", x.get("clone_seq"))) for x in items)
    print("  cloneSeq distribution: %r" % (dict(cs),))
    print("  copies distribution: %r"
          % (dict(collections.Counter(str(x.get("copies")) for x in items)),))
    print("  authMethod distribution: %r"
          % (dict(collections.Counter(str(x.get("authMethod")) for x in items)),))
    print("  disabled distribution: %r"
          % (dict(collections.Counter(str(x.get("disabled")) for x in items)),))
    print("  region fields: apiRegion=%r authRegion=%r region=%r"
          % (dict(collections.Counter(str(x.get("apiRegion")) for x in items)),
             dict(collections.Counter(str(x.get("authRegion")) for x in items)),
             dict(collections.Counter(str(x.get("region")) for x in items))))

    # Per-entry compact table: does keyhash correlate with cloneGroup?
    print("  per-entry (id / keyhash / cloneGroup / cloneSeq / disabled):")
    for x in items:
        raw = ""
        for f in apikey_fields:
            v = x.get(f)
            if isinstance(v, str) and v:
                raw = v
                break
        print("      %-14s %-14s %-38s %-5s %s"
              % (str(x.get("id"))[:14], short(raw),
                 str(x.get("cloneGroup", x.get("clone_group")))[:38],
                 str(x.get("cloneSeq", x.get("clone_seq"))),
                 str(x.get("disabled"))))

    print()
    print("=" * 72)
    print("SECTION 2 — live config (read now, never trust docs)")
    print("=" * 72)
    try:
        with open(os.path.join(DATA, "config.json"), "r", encoding="utf-8") as fh:
            cfg = json.load(fh)
    except Exception as e:  # noqa: BLE001
        print("  !! cannot read config.json:", type(e).__name__, e)
        cfg = {}
    watch = [
        "credentialRpmLimit", "rpmHeadroomFactor", "inboundTargetRpm",
        "inboundThrottleEnabled", "inboundRpmAuto", "rateLimitEnabled",
        "cooldownEnabled", "rpmHardGateOverloadWait", "region",
        "maxRetries", "failoverEnabled", "trustForwardedHeader",
    ]
    for k in watch:
        print("      %-28s %r" % (k, cfg.get(k, "<absent>")))
    extra = sorted(k for k in cfg
                   if ("rpm" in k.lower() or "cooldown" in k.lower()
                       or "throttle" in k.lower() or "retry" in k.lower())
                   and k not in watch)
    print("  other rpm/cooldown/throttle/retry keys:")
    for k in extra:
        print("      %-28s %r" % (k, cfg.get(k)))

    print()
    print("=" * 72)
    print("SECTION 3 — traces.db outcome / retries (read-only)")
    print("=" * 72)
    tp = os.path.join(DATA, "usage", "traces.db")
    if not os.path.exists(tp):
        print("  !! traces.db not at", tp)
        return
    con = sqlite3.connect("file:%s?mode=ro" % tp, uri=True)
    cols = [r[1] for r in con.execute("pragma table_info(traces)").fetchall()]
    print("  traces columns: %r" % (cols,))
    tscol = "ts_ms" if "ts_ms" in cols else ("ts" if "ts" in cols else None)
    if tscol is None:
        print("  !! no timestamp column found; abort section 3")
        return
    now = time.time()
    for label, secs in (("5min", 300), ("30min", 1800), ("2h", 7200)):
        cut = int((now - secs) * 1000) if tscol == "ts_ms" else int(now - secs)
        rows = con.execute(
            "select outcome, retries from traces where %s>=?" % tscol, (cut,)
        ).fetchall()
        oc = collections.Counter(r[0] for r in rows)
        rc = collections.Counter(int(r[1] or 0) for r in rows)
        tot = len(rows)
        rl = sum(v for k, v in oc.items() if k and "rate" in str(k).lower())
        print("  last %-6s n=%-6d rate_limited=%d (%.1f%%)"
              % (label, tot, rl, (100.0 * rl / tot) if tot else 0.0))
        print("      outcome: %r" % (dict(oc),))
        print("      retries: %r" % (dict(sorted(rc.items())),))
        print("      retries_sum=%d  amplification=%.2fx"
              % (sum(k * v for k, v in rc.items()),
                 (sum(k * v for k, v in rc.items()) / tot + 1.0) if tot else 0.0))
    con.close()


if __name__ == "__main__":
    main()
