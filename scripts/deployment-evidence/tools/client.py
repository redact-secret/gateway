#!/usr/bin/env python3
"""Evidence client (issue #44). Stdlib only; runs inside the pinned python image.

Every subcommand prints ONE JSON line and never prints a request or response body.

  client.py post URL      POST one valid synthetic Chat Completions request with a synthetic,
                          revoked-looking key; print {"status", "code", "relayed"}.
                          `code` is the gateway's fixed error code; `relayed` is true when the
                          fake provider's synthetic answer came back through the gateway.
  client.py connect H P   TCP connect with a 5 s deadline; print {"target", "result"} where result
                          is "connected" or "error:<errno name>" (no data is sent).
  client.py resolve NAME  getaddrinfo; print the answer set or the error class.
"""
import errno
import json
import os
import socket
import sys
import urllib.error
import urllib.request

# Synthetic and unmistakably fake (the same shape the shipped probes use).
KEY = "sk-SYNTHETIC-REVOKED-CI-NOT-A-KEY"
# The gateway enforces a local caller token (#63): a per-run throwaway value passed in the
# environment by the orchestrating script. Never printed, and not a provider credential.
LOCAL_TOKEN = os.environ.get("LOCAL_TOKEN")
BODY = json.dumps(
    {"model": "synthetic-model", "messages": [{"role": "user", "content": "synthetic evidence ping"}]}
).encode()


def post(url):
    req = urllib.request.Request(
        url,
        data=BODY,
        method="POST",
        headers={"Content-Type": "application/json", "Authorization": "Bearer " + KEY},
    )
    if LOCAL_TOKEN:
        req.add_header("X-Gateway-Local-Token", LOCAL_TOKEN)
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            status, raw = r.status, r.read(65536)
    except urllib.error.HTTPError as e:
        status, raw = e.code, e.read(65536)
    except OSError as e:
        print(json.dumps({"status": None, "code": "client_error:" + type(e).__name__, "relayed": False}))
        return
    code = None
    relayed = False
    try:
        doc = json.loads(raw)
        code = doc.get("error", {}).get("code") if isinstance(doc.get("error"), dict) else None
        relayed = doc.get("id") == "synthetic-stand-in"
    except ValueError:
        pass
    print(json.dumps({"status": status, "code": code, "relayed": relayed}))


def connect(host, port):
    target = "%s:%s" % (host, port)
    try:
        with socket.create_connection((host, int(port)), timeout=5):
            print(json.dumps({"target": target, "result": "connected"}))
    except socket.timeout:
        print(json.dumps({"target": target, "result": "error:timeout"}))
    except OSError as e:
        name = errno.errorcode.get(e.errno, type(e).__name__) if e.errno else type(e).__name__
        print(json.dumps({"target": target, "result": "error:" + name}))


def resolve(name):
    try:
        infos = socket.getaddrinfo(name, 443, proto=socket.IPPROTO_TCP)
        print(json.dumps({"name": name, "result": sorted({i[4][0] for i in infos})}))
    except socket.gaierror as e:
        print(json.dumps({"name": name, "result": "error:gaierror:%s" % e.errno}))


if __name__ == "__main__":
    a = sys.argv[1:]
    if len(a) == 2 and a[0] == "post":
        post(a[1])
    elif len(a) == 3 and a[0] == "connect":
        connect(a[1], a[2])
    elif len(a) == 2 and a[0] == "resolve":
        resolve(a[1])
    else:
        sys.exit("usage: client.py post URL | connect HOST PORT | resolve NAME")
