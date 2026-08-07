#!/usr/bin/env python3
"""Read-only: does the clone fleet present ONE account under MANY machine fingerprints
and MANY egress IPs? That is a textbook account-sharing signal to upstream risk control,
and would explain the repeated 403 "temporarily is suspended" better than RPM.

Writes nothing.
"""
import collections
import hashlib
import json

P = "/opt/kirostudio/data/credentials.json"


def short(s, n=12):
    return hashlib.sha256(s.encode()).hexdigest()[:n] if s else "-"


def main():
    with open(P, "r", encoding="utf-8") as fh:
        d = json.load(fh)
    items = d if isinstance(d, list) else d.get("credentials", [d])

    print("=" * 78)
    print("per-credential fingerprint (id / keyhash / machineId / proxy host / region)")
    print("=" * 78)
    by_key = collections.defaultdict(lambda: {"mids": set(), "proxies": set(),
                                             "ids": [], "regions": set()})
    for x in items:
        raw = x.get("kiroApiKey") or x.get("apiKey") or ""
        kh = short(raw)
        mid = x.get("machineId") or ""
        pu = x.get("proxyUrl") or ""
        # strip creds from proxy url for display; keep host:port identity
        phost = pu
        if "@" in pu:
            phost = pu.split("@", 1)[1]
        user = x.get("proxyUsername") or ""
        g = by_key[kh]
        g["mids"].add(mid)
        g["proxies"].add((phost, short(user, 6)))
        g["ids"].append(str(x.get("id")))
        g["regions"].add(str(x.get("apiRegion")))
        print("  id=%-6s key=%-13s mid=%-14s proxy=%-28s puser=%-8s region=%s"
              % (str(x.get("id")), kh, short(mid, 12), phost[:28],
                 short(user, 6), x.get("apiRegion")))

    print()
    print("=" * 78)
    print("AGGREGATED PER UPSTREAM ACCOUNT (one keyhash == one account)")
    print("=" * 78)
    for kh, g in by_key.items():
        print("  keyhash %-13s  clones=%-3d distinct_machineId=%-3d distinct_proxy=%-3d regions=%r"
              % (kh, len(g["ids"]), len(g["mids"]), len(g["proxies"]), g["regions"]))
        print("      ids: %s" % ",".join(g["ids"]))
        if len(g["mids"]) > 1:
            print("      !! %d distinct machineId on ONE account" % len(g["mids"]))
        if len(g["proxies"]) > 1:
            print("      !! %d distinct egress proxy on ONE account" % len(g["proxies"]))
            for ph, pus in sorted(g["proxies"]):
                print("         %-30s puser=%s" % (ph[:30], pus))


if __name__ == "__main__":
    main()
