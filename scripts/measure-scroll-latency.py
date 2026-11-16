#!/usr/bin/env python3
"""量“滚动到底有多流畅”：滚轮 → 屏幕更新 的端到端延迟。

只看 CPU 是抓不到的：卡顿源于"输入进去了但画面很久才动"。所以要测：
1. 每条滚轮报文之后，**守护进程**多久开始吐字节；
2. 连续滚动期间，守护进程的输出是否持续流动（还是停住不动）。

用法：
    DEEPTERM_RUNTIME_DIR=... TARGET=t1 python3 scripts/measure-scroll-latency.py
"""
import json
import os
import socket
import struct
import sys
import time

H = 9
JSON, DATA, CLOSE, RESIZE, CREDIT = 0, 1, 2, 3, 4
RUN = os.environ.get("DEEPTERM_RUNTIME_DIR", "/tmp/dt-smoke")


def enc(c, k, p=b""):
    return struct.pack("<IBI", c, k, len(p)) + p


def rx(s, n):
    b = b""
    while len(b) < n:
        d = s.recv(n - len(b))
        if not d:
            return None
        b += d
    return b


def frame(s):
    h = rx(s, H)
    if h is None:
        return None
    c, k, n = struct.unpack("<IBI", h)
    return c, k, (rx(s, n) if n else b"")


def call(s, r, timeout=10):
    s.settimeout(timeout)
    s.sendall(enc(0, JSON, json.dumps(r).encode()))
    while True:
        c, k, p = frame(s)
        if c == 0 and k == JSON:
            return json.loads(p)


def main():
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect(os.path.join(RUN, "daemon.sock"))
    with open(os.path.join(RUN, "daemon.token")) as f:
        token = f.read().strip()
    sock.sendall(enc(0, JSON, json.dumps(
        {"id": 0, "op": "hello", "token": token, "protocol": 2}).encode()))
    frame(sock)

    terms = call(sock, {"op": "list"}).get("terminals", [])
    target = os.environ.get("TARGET")
    addr = target if target else terms[0]["address"]
    size = call(sock, {"op": "get", "address": addr})["terminal"]
    print(f"attaching {addr} ({size['cols']}x{size['rows']})")
    ch = call(sock, {"op": "attach", "address": addr})["channel"]

    def drain(seconds):
        """吃掉这段时间的输出，返回 (bytes, frames)。"""
        total = frames = 0
        end = time.time() + seconds
        sock.settimeout(0.01)
        while time.time() < end:
            try:
                c, k, p = frame(sock)
            except (socket.timeout, TimeoutError):
                continue
            if c is None:
                break
            if c == ch and k == DATA:
                total += len(p)
                frames += 1
                sock.sendall(enc(ch, CREDIT, struct.pack("<I", len(p))))
        return total, frames

    drain(1.0)  # 首屏

    up = b"\x1b[<64;10;10M"
    down = b"\x1b[<65;10;10M"

    # ── 单条滚轮：首字节延迟 ────────────────────────────────────────────
    print("\n单条滚轮 → 首字节延迟（10 次取中位数）:")
    lat = []
    for i in range(10):
        t0 = time.time()
        sock.sendall(enc(ch, DATA, up if i % 2 == 0 else down))
        first = None
        end = t0 + 0.5
        while time.time() < end:
            sock.settimeout(0.005)
            try:
                c, k, p = frame(sock)
            except (socket.timeout, TimeoutError):
                continue
            if c == ch and k == DATA:
                first = (time.time() - t0) * 1000
                sock.sendall(enc(ch, CREDIT, struct.pack("<I", len(p))))
                break
        if first is not None:
            lat.append(first)
        drain(0.05)
    lat.sort()
    if lat:
        print(f"  中位 {lat[len(lat)//2]:.1f} ms，最差 {lat[-1]:.1f} ms")

    # ── 连续滚动：输出是否持续流动 ──────────────────────────────────────
    print("\n连续滚动 5 秒（每秒 30 条，上下交替），每 500ms 采样输出量:")
    t0 = time.time()
    sent = 0
    stalls = 0
    while time.time() - t0 < 5:
        window_start = time.time()
        while time.time() - window_start < 0.5:
            sock.sendall(enc(ch, DATA, up if sent % 16 < 8 else down))
            sent += 1
            drain(0.033)
        n, f = drain(0.05)
        if n == 0:
            stalls += 1

    print(f"  共发 {sent} 条，{stalls} 个 500ms 窗口完全没有输出")

    # ── 关键判据：滚完之后必须立刻恢复响应 ──────────────────────────────
    t0 = time.time()
    ok = call(sock, {"op": "get", "address": addr}, timeout=5)
    dt = (time.time() - t0) * 1000
    print(f"\n滚完之后 get 响应 {dt:.0f} ms（ok={ok.get('ok')}）")
    assert dt < 1000, f"守护进程在滚动后响应过慢: {dt:.0f} ms"
    assert stalls <= 1, f"连续滚动期间有 {stalls} 个窗口完全无输出（界面会像卡死）"
    print("\nPASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
