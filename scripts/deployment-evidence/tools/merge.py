#!/usr/bin/env python3
"""Merge the direct-to-gateway run and each intermediary run into one per-case table (issue #44).

  merge.py DIR   reads DIR/direct.jsonl, DIR/<name>.jsonl and DIR/<name>.log for each intermediary
                 named in DIR/intermediaries.txt; writes DIR/framing-results.json and
                 DIR/framing-results.md.

The intermediary log is sliced per case by the sentinel request the runner sent after each case.
Log lines are reduced to a sanitized reason (no addresses, ports, timestamps, or request content).
"""
import json
import re
import sys

REJECT_CODES = {"malformed_input", "unsupported_input", "limit_exceeded", "missing_credential"}


def load(path):
    with open(path) as f:
        return {r["id"]: r for r in (json.loads(line) for line in f if line.strip())}


def first(r):
    return r["responses"][0] if r["responses"] else None


def describe(r):
    f = first(r)
    if f is None:
        return "no response (timeout)" if r["timeout"] else "connection closed, no response"
    s = "%s%s" % (f["status"], (" " + f["code"]) if f["code"] else "")
    if len(r["responses"]) > 1:
        s += " (+%d more response%s)" % (len(r["responses"]) - 1, "s" if len(r["responses"]) > 2 else "")
    if r["interim"]:
        s = "100 then " + s
    return s


def admitted(f):
    return f is not None and (f["status"] == 200 or (f["code"] or "").startswith("upstream_"))


def origin(f, name):
    if f is None:
        return None
    if f["body_class"].startswith("gateway-"):
        return "gateway"
    srv = (f.get("server") or "").lower()
    if name in srv or f["body_class"] == "html-or-text-from-intermediary":
        return "intermediary"
    if f["body_class"] == "empty" and not srv:
        return "gateway"  # the HTTP stack's own empty-bodied 4xx (e.g. hyper's parser rejections)
    return "unknown"


def slice_log(path):
    """Return {case-id: [sanitized reason, ...]} using the sentinel lines as delimiters."""
    out, cur = {}, []
    with open(path, errors="replace") as f:
        for line in f:
            m = re.search(r"/__sentinel/([a-z0-9-]+)", line)
            if m:
                out[m.group(1)] = cur  # "ready" (start-up probe) is simply never looked up
                cur = []
                continue
            reason = reduce_line(line.rstrip("\n"))
            if reason:
                cur.append(reason)
    return out


def reduce_line(line):
    m = re.search(r"\[(info|error|warn|crit|alert|emerg)\] \d+#\d+: \*\d+ (.*)$", line)
    if m:  # nginx error log
        if "closed keepalive connection" in m.group(2):
            return None  # connection teardown noise, not about the case
        msg = re.sub(r", (client|server|request|upstream|host): .*$", "", m.group(2))
        msg = re.sub(r"\d+\.\d+\.\d+\.\d+(:\d+)?", "<addr>", msg)
        return "nginx %s: %s" % (m.group(1), msg)
    m = re.search(r'" (\d{3}) \d+ "[^"]*" "[^"]*"$', line)  # nginx access log
    if m:
        return "nginx access status=%s" % m.group(1)
    m = re.search(r"\s(\d{3})\s\d+\s-\s-\s(\S{4})\s", line)  # haproxy httplog
    if m:
        req = re.search(r'"([^"]*)"\s*$', line)
        shown = " " + req.group(1) if req and req.group(1).startswith("<") else ""
        if m.group(2).startswith("C"):
            # Client-side abort flags (CH/CR) vary run to run with timing; keep only the stable fact.
            return "haproxy client-abort"
        return "haproxy status=%s termination=%s%s" % (m.group(1), m.group(2), shown)
    return None


def classify(direct, via, hints, name):
    f, d = first(via), first(direct)
    o = origin(f, name)
    reason = "; ".join(sorted(set(hints))) if hints else ""
    prematurely = "prematurely closed" in reason or "termination=SH" in reason or "termination=SD" in reason
    direct_rejects = d is None or (d["code"] in REJECT_CODES) or (d["status"] or 0) >= 400 and not admitted(d)
    if f is None:
        if "prematurely closed connection" in reason:
            return "closed at the intermediary (log: client prematurely closed connection; no gateway response)"
        if "client-abort" in reason:
            return (
                "closed at the intermediary on the client half-close (truncated request; whether it was "
                "forwarded first is timing-dependent, the gateway answers 400 if it was)"
            )
        if "status=4" in reason:
            return "rejected at the intermediary (log shows a 4xx; the client saw a closed connection, no response read)"
        return "connection closed, no response (see log reason)"
    if o == "intermediary":
        if f["status"] in (502, 503, 504) and (prematurely or d is None):
            return "reached gateway, closed by the gateway (intermediary answered %d)" % f["status"]
        if (f["status"] or 0) >= 400:
            return "rejected at the intermediary (%d)" % f["status"]
        return "answered by the intermediary (%s)" % f["status"]
    if o == "gateway":
        if admitted(f):
            if direct_rejects:
                return "NORMALIZED: reached gateway and admitted, but the same bytes sent directly are rejected"
            return "reached gateway, admitted (same outcome as direct)"
        return "reached gateway, rejected by the gateway (%s)" % (f["code"] or "empty-bodied %s from the HTTP stack" % f["status"])
    return "other (see response)"


def main():
    d = sys.argv[1]
    names = [n for n in open(d + "/intermediaries.txt").read().split() if n]
    direct = load(d + "/direct.jsonl")
    runs = {n: load("%s/%s.jsonl" % (d, n)) for n in names}
    logs = {n: slice_log("%s/%s.log" % (d, n)) for n in names}
    results = []
    for cid, dr in sorted(direct.items()):
        row = {"id": cid, "desc": dr["desc"], "direct": describe(dr), "direct_after_response": dr.get("after_response")}
        for n in names:
            vr = runs[n][cid]
            hints = logs[n].get(cid, [])
            row[n] = {
                "outcome": describe(vr),
                "classification": classify(dr, vr, hints, n),
                "after_response": vr.get("after_response"),
                "connection_header": (first(vr) or {}).get("connection"),
                "log_reasons": sorted(set(hints)),
            }
        row["direct_connection_header"] = (first(dr) or {}).get("connection")
        results.append(row)
    with open(d + "/framing-results.json", "w") as f:
        json.dump(results, f, indent=2, sort_keys=True)
        f.write("\n")
    with open(d + "/framing-results.md", "w") as f:
        for n in names:
            f.write("### Intermediary: %s\n\n" % n)
            f.write("| Case | Direct to gateway | Via %s | Classification | Log reason |\n| --- | --- | --- | --- | --- |\n" % n)
            for r in results:
                v = r[n]
                f.write(
                    "| `%s` %s | %s | %s | %s | %s |\n"
                    % (r["id"], r["desc"], r["direct"], v["outcome"], v["classification"], "; ".join(v["log_reasons"]) or "-")
                )
            f.write("\n")
        f.write("### Connection reuse (cases that observe it)\n\n| Case | Target | Connection header | Peer after the response |\n| --- | --- | --- | --- |\n")
        for r in results:
            if r["direct_after_response"]:
                f.write("| `%s` | gateway directly | %s | %s |\n" % (r["id"], r["direct_connection_header"], r["direct_after_response"]))
                for n in names:
                    f.write("| `%s` | via %s | %s | %s |\n" % (r["id"], n, r[n]["connection_header"] or "-", r[n]["after_response"]))
    print(open(d + "/framing-results.md").read())


if __name__ == "__main__":
    main()
