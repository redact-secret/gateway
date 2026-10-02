#!/usr/bin/env python3
"""Synthetic fake provider for the deployment-chain evidence (issue #44).

Stdlib only. Serves TLS on port 443 (all IPv4 addresses of its network namespace, and IPv6 with
--ipv6) with a throwaway certificate, answers every request with a fixed synthetic JSON body, and
keeps exact counters. It NEVER logs or stores request bytes: only counts and peer addresses.

  fake_provider.py serve --cert C --key K [--ipv6]   run (prints `listening` once ready)
  fake_provider.py sync                               print counters as JSON once every accepted
                                                      connection has been fully handled

`sync` is the deterministic barrier: it drains every listener's accept queue (a connection the
kernel completed before the caller observed its outcome is in that queue) and then waits until no
handler is in flight, so a count of zero means no connection was ever made.
"""
import json
import selectors
import socket
import ssl
import sys
import threading

CONTROL = ("127.0.0.1", 9000)
BODY = b'{"id":"synthetic-fake-provider","object":"chat.completion","choices":[]}'
lock = threading.Condition()
stats = {"accepted": 0, "tls_failed": 0, "requests": 0, "peers": {}}
inflight = 0


def handle(conn, ctx):
    global inflight
    try:
        conn.settimeout(10)
        try:
            tls = ctx.wrap_socket(conn, server_side=True)
        except (ssl.SSLError, OSError):
            with lock:
                stats["tls_failed"] += 1
            return
        data = b""
        while b"\r\n\r\n" not in data:
            chunk = tls.recv(4096)
            if not chunk:
                return
            data += chunk
        tls.sendall(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n"
            b"Content-Length: " + str(len(BODY)).encode() + b"\r\n\r\n" + BODY
        )
        with lock:
            stats["requests"] += 1
        try:
            tls.close()
        except OSError:
            pass
    except OSError:
        pass
    finally:
        try:
            conn.close()
        except OSError:
            pass
        with lock:
            inflight -= 1
            lock.notify_all()


def drain(listener, ctx):
    global inflight
    while True:
        try:
            conn, peer = listener.accept()
        except (BlockingIOError, InterruptedError):
            return
        with lock:
            stats["accepted"] += 1
            stats["peers"][peer[0]] = stats["peers"].get(peer[0], 0) + 1
            inflight += 1
        threading.Thread(target=handle, args=(conn, ctx), daemon=True).start()


def serve(cert, key, ipv6):
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.minimum_version = ssl.TLSVersion.TLSv1_2
    ctx.load_cert_chain(cert, key)
    listeners = []
    families = [(socket.AF_INET, "0.0.0.0")] + ([(socket.AF_INET6, "::")] if ipv6 else [])
    for fam, addr in families:
        s = socket.socket(fam, socket.SOCK_STREAM)
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        if fam == socket.AF_INET6:
            s.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
        s.bind((addr, 443))
        s.listen(128)
        s.setblocking(False)
        listeners.append(s)
    control = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    control.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    control.bind(CONTROL)
    control.listen(8)
    sel = selectors.DefaultSelector()
    for s in listeners:
        sel.register(s, selectors.EVENT_READ, "l")
    sel.register(control, selectors.EVENT_READ, "c")
    print("listening", flush=True)
    while True:
        for key_, _ in sel.select():
            if key_.data == "l":
                drain(key_.fileobj, ctx)
            else:
                c, _ = control.accept()
                for s in listeners:
                    drain(s, ctx)
                with lock:
                    lock.wait_for(lambda: inflight == 0, timeout=30)
                    out = json.dumps(stats, sort_keys=True)
                c.sendall(out.encode() + b"\n")
                c.close()


def sync():
    with socket.create_connection(CONTROL, timeout=60) as c:
        buf = b""
        while not buf.endswith(b"\n"):
            chunk = c.recv(4096)
            if not chunk:
                break
            buf += chunk
    sys.stdout.write(buf.decode())


if __name__ == "__main__":
    args = sys.argv[1:]
    if args[:1] == ["sync"]:
        sync()
    elif args[:1] == ["serve"]:
        ipv6 = "--ipv6" in args
        rest = [x for x in args[1:] if x != "--ipv6"]
        opts = dict(zip(rest[0::2], rest[1::2]))
        serve(opts["--cert"], opts["--key"], ipv6)
    else:
        sys.exit("usage: fake_provider.py serve --cert C --key K [--ipv6] | sync")
