#!/usr/bin/env python3
"""HTTP framing conformance cases against one HTTP/1.1 endpoint (issue #44). Stdlib only.

  framing_cases.py HOST PORT OUT.jsonl [--sentinel]

Sends each raw case on its own TCP connection and records, per case, what came back: the status,
the gateway's fixed error code if the body is the gateway's envelope, who produced the response
(`origin`), the Connection header, whether the peer closed, and how many responses arrived. It
never records a body (only a class of body) and every payload is synthetic.

With --sentinel (used for an intermediary) it sends `GET /__sentinel/<case>` on a fresh connection
after each case. The intermediary logs that request, which lets the orchestrator slice the shared
log stream per case without sleeping.
"""
import json
import os
import socket
import sys
import time

KEY = "sk-SYNTHETIC-REVOKED-CI-NOT-A-KEY"
# Per-run throwaway local caller token (#63), passed in the environment; never printed.
LOCAL_TOKEN = os.environ.get("LOCAL_TOKEN")
BODY = json.dumps(
    {"model": "synthetic-model", "messages": [{"role": "user", "content": "synthetic evidence ping"}]}
).encode()
N = str(len(BODY)).encode()
PATH = b"/v1/chat/completions"


def head(extra=(), cl=True, ctype=True, target=PATH, version=b"HTTP/1.1", host=b"gateway.test"):
    lines = [b"POST " + target + b" " + version]
    if host is not None:
        lines.append(b"Host: " + host)
    if ctype:
        lines.append(b"Content-Type: application/json")
    lines.append(b"Authorization: Bearer " + KEY.encode())
    if LOCAL_TOKEN:
        lines.append(b"X-Gateway-Local-Token: " + LOCAL_TOKEN.encode())
    if cl:
        lines.append(b"Content-Length: " + N)
    lines.extend(extra)
    return b"\r\n".join(lines) + b"\r\n\r\n"


def chunked(data, size=16, ext=b"", nl=b"\r\n", trailer=b"", terminal=True):
    out = b""
    for i in range(0, len(data), size):
        piece = data[i : i + size]
        out += ("%x" % len(piece)).encode() + ext + nl + piece + nl
    if terminal:
        out += b"0" + nl + trailer + nl
    return out


TE = b"Transfer-Encoding: chunked"
FULL = head() + BODY


def cases():
    c = []

    def add(cid, desc, raw, want=1, probe_close=False, half_close=False):
        c.append(dict(id=cid, desc=desc, raw=raw, want=want, probe_close=probe_close, half_close=half_close))

    add("c01-baseline-content-length", "Valid request, Content-Length framing", FULL, probe_close=True)
    add("c02-cl-and-te-cl-first", "Content-Length then Transfer-Encoding: chunked", head([TE]) + chunked(BODY))
    add(
        "c03-cl-and-te-te-first",
        "Transfer-Encoding: chunked then Content-Length",
        head([TE], cl=False).replace(b"\r\n\r\n", b"\r\nContent-Length: " + N + b"\r\n\r\n", 1) + chunked(BODY),
    )
    add("c04-te-space-before-colon", "CL plus `Transfer-Encoding : chunked`", head([b"Transfer-Encoding : chunked"]) + chunked(BODY))
    add("c05-te-tab-value", "CL plus `Transfer-Encoding:<TAB>chunked`", head([b"Transfer-Encoding:\tchunked"]) + chunked(BODY))
    add("c06-te-unknown-coding", "CL plus `Transfer-Encoding: xchunked`", head([b"Transfer-Encoding: xchunked"]) + BODY)
    add("c07-te-chunked-identity", "CL plus `Transfer-Encoding: chunked, identity`", head([b"Transfer-Encoding: chunked, identity"]) + chunked(BODY))
    add("c08-te-uppercase", "CL plus `TRANSFER-ENCODING: CHUNKED`", head([b"TRANSFER-ENCODING: CHUNKED"]) + chunked(BODY))
    add(
        "c09-duplicate-cl-conflicting",
        "Two Content-Length fields with different values",
        head([b"Content-Length: " + str(len(BODY) + 5).encode()]) + BODY,
    )
    add("c10-duplicate-cl-identical", "Two identical Content-Length fields", head([b"Content-Length: " + N]) + BODY)
    add("c11-cl-list", "`Content-Length: N, N`", head(cl=False, extra=[b"Content-Length: " + N + b", " + N]) + BODY)
    add("c12-cl-plus-sign", "`Content-Length: +N`", head(cl=False, extra=[b"Content-Length: +" + N]) + BODY)
    add("c13-chunked-only", "Valid chunked body, no Content-Length", head([TE], cl=False) + chunked(BODY))
    add("c14-chunk-extension", "Chunked body with a chunk extension", head([TE], cl=False) + chunked(BODY, ext=b";ext=1"))
    add("c15-chunk-trailer", "Chunked body with a trailer field", head([TE], cl=False) + chunked(BODY, trailer=b"X-Trailer: t\r\n"))
    add("c16-chunk-size-not-hex", "Chunk size `zz`", head([TE], cl=False) + b"zz\r\n" + BODY + b"\r\n0\r\n\r\n")
    add("c17-chunk-size-overflow", "Chunk size over 64 bits", head([TE], cl=False) + b"FFFFFFFFFFFFFFFFF\r\n" + BODY + b"\r\n0\r\n\r\n")
    add("c18-chunk-bare-lf", "Chunk lines end in bare LF", head([TE], cl=False) + chunked(BODY, nl=b"\n"))
    add("c19-chunked-unterminated", "Chunked body without the terminal chunk, then half-close", head([TE], cl=False) + chunked(BODY, terminal=False), half_close=True)
    add("c20-short-body", "Content-Length larger than the body sent, then half-close", head(cl=False, extra=[b"Content-Length: " + str(len(BODY) + 50).encode()]) + BODY, half_close=True)
    add("c21-obsolete-line-folding", "Header continuation line (obs-fold)", head([b"X-Folded: a\r\n b"]) + BODY)
    add("c22-header-bare-cr", "Bare CR inside a header value", head([b"X-Test: a\rX-Injected: b"]) + BODY)
    add("c23-header-bare-lf", "Bare LF inside the header block", head([b"X-Test: a\nX-Injected: b"]) + BODY)
    add("c24-header-nul", "NUL byte in a header value", head([b"X-Test: a\x00b"]) + BODY)
    add("c25-space-before-colon", "Space before the colon in Content-Type", head(ctype=False, extra=[b"Content-Type : application/json"]) + BODY)
    add("c26-invalid-header-name", "Header name containing a space", head([b"Bad Name: v"]) + BODY)
    add("c27-expect-100-continue", "`Expect: 100-continue`", head([b"Expect: 100-continue"]) + BODY)
    add("c28-expect-unknown", "`Expect: unknown-expectation`", head([b"Expect: unknown-expectation"]) + BODY)
    add("c29-pipelined-pair", "Two complete requests in one write", FULL + FULL, want=2)
    add(
        "c30-request-then-trailing-get",
        "One request followed by `GET /healthz` in the same write",
        FULL + b"GET /healthz HTTP/1.1\r\nHost: gateway.test\r\n\r\n",
        want=2,
    )
    add("c31-absolute-form-target", "Absolute-form request target naming another origin", head(target=b"http://evil.invalid/v1/chat/completions") + BODY)
    add("c32-duplicate-host", "Two Host fields", head([b"Host: other.invalid"]) + BODY)
    add("c33-host-names-provider", "Host names the provider (must not affect routing)", head(host=b"api.openai.com") + BODY)
    add("c34-http-1-0", "HTTP/1.0 request", head(version=b"HTTP/1.0", host=None) + BODY)
    add(
        "c35-connection-close-behavior",
        "HTTP/1.1 request asking for keep-alive; is the connection reused or closed?",
        head([b"Connection: keep-alive"]) + BODY,
        probe_close=True,
    )
    add(
        "c36-connection-nominates-framing",
        "`Connection: Content-Length` (hop-by-hop nomination of a framing header)",
        head([b"Connection: Content-Length"]) + BODY,
    )
    return c


def parse_one(buf):
    """Return (response_dict or None, rest). Handles 1xx, Content-Length, chunked, to-EOF."""
    end = buf.find(b"\r\n\r\n")
    if end < 0:
        return None, buf
    lines = buf[:end].split(b"\r\n")
    parts = lines[0].split(b" ", 2)
    try:
        status = int(parts[1])
    except (IndexError, ValueError):
        return {"status": None, "headers": {}, "body": b"", "malformed": True}, b""
    headers = {}
    for ln in lines[1:]:
        k, _, v = ln.partition(b":")
        headers.setdefault(k.strip().lower().decode("latin-1"), v.strip().decode("latin-1"))
    rest = buf[end + 4 :]
    if 100 <= status < 200 or status in (204, 304):
        return {"status": status, "headers": headers, "body": b""}, rest
    if "content-length" in headers:
        n = int(headers["content-length"])
        if len(rest) < n:
            return None, buf
        return {"status": status, "headers": headers, "body": rest[:n]}, rest[n:]
    if "chunked" in headers.get("transfer-encoding", ""):
        body, pos = b"", 0
        while True:
            e = rest.find(b"\r\n", pos)
            if e < 0:
                return None, buf
            size = int(rest[pos:e].split(b";")[0], 16)
            if size == 0:
                t = rest.find(b"\r\n\r\n", e)
                if t < 0:
                    return None, buf
                return {"status": status, "headers": headers, "body": body}, rest[t + 4 :]
            if len(rest) < e + 2 + size + 2:
                return None, buf
            body += rest[e + 2 : e + 2 + size]
            pos = e + 2 + size + 2
    return {"status": status, "headers": headers, "body": b"", "to_eof": True}, b""


def classify(resp):
    body = resp["body"]
    code = None
    cls = "empty"
    if body:
        try:
            doc = json.loads(body)
            if isinstance(doc, dict) and isinstance(doc.get("error"), dict):
                code = doc["error"].get("code")
                cls = "gateway-error-envelope"
            elif isinstance(doc, dict) and doc.get("status") in ("ok", "ready", "live"):
                cls = "gateway-health"
            else:
                cls = "json-other"
        except ValueError:
            low = body.lower()
            cls = "html-or-text-from-intermediary" if (b"<html" in low or b"bad request" in low or b"nginx" in low or b"haproxy" in low) else "text-other"
    return code, cls


def run_case(host, port, case, deadline_s=8.0):
    s = socket.create_connection((host, port), timeout=5)
    s.settimeout(1.0)
    out = {"id": case["id"], "desc": case["desc"], "responses": [], "interim": [], "eof": False, "timeout": False}
    try:
        s.sendall(case["raw"])
        if case["half_close"]:
            s.shutdown(socket.SHUT_WR)
        buf = b""
        end_at = time.monotonic() + deadline_s
        final = 0
        while final < case["want"]:
            resp, buf = parse_one(buf)
            if resp is not None:
                if resp["status"] is not None and 100 <= resp["status"] < 200:
                    out["interim"].append(resp["status"])
                    continue
                code, cls = classify(resp)
                out["responses"].append(
                    {
                        "status": resp["status"],
                        "code": code,
                        "body_class": cls,
                        "server": resp["headers"].get("server"),
                        "connection": resp["headers"].get("connection"),
                    }
                )
                final += 1
                if resp.get("to_eof"):
                    out["eof"] = True
                    break
                continue
            if time.monotonic() > end_at:
                out["timeout"] = True
                break
            try:
                data = s.recv(65536)
            except socket.timeout:
                continue
            except OSError:
                out["eof"] = True
                break
            if not data:
                out["eof"] = True
                break
            buf += data
        if case["probe_close"] and not out["eof"] and not out["timeout"]:
            # Bounded observation of the negative: does the peer close this connection right away?
            s.settimeout(2.0)
            try:
                data = s.recv(65536)
                out["after_response"] = "closed" if not data else "extra-bytes"
            except socket.timeout:
                out["after_response"] = "left-open"
            except OSError:
                out["after_response"] = "closed"
    finally:
        s.close()
    return out


def sentinel(host, port, cid):
    s = socket.create_connection((host, port), timeout=5)
    s.settimeout(5)
    s.sendall(b"GET /__sentinel/" + cid.encode() + b" HTTP/1.1\r\nHost: evidence.test\r\nConnection: close\r\n\r\n")
    data = b""
    while True:
        chunk = s.recv(4096)
        if not chunk:
            break
        data += chunk
    s.close()
    if b" 204 " not in data.split(b"\r\n", 1)[0]:
        raise SystemExit("sentinel was not answered by the intermediary: " + data[:80].decode("latin-1"))


def main():
    host, port, outp = sys.argv[1], int(sys.argv[2]), sys.argv[3]
    use_sentinel = "--sentinel" in sys.argv
    with open(outp, "w") as f:
        for case in cases():
            res = run_case(host, port, case)
            if use_sentinel:
                sentinel(host, port, case["id"])
            f.write(json.dumps(res, sort_keys=True) + "\n")
            print("%s %s" % (case["id"], json.dumps(res["responses"][:1] and {"status": res["responses"][0]["status"], "code": res["responses"][0]["code"]})), flush=True)


if __name__ == "__main__":
    main()
