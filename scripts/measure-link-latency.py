#!/usr/bin/env python3
"""把"我们的链路"与"opencode 自己的重绘开销"分离开。

做法：起一个假 TUI，它收到一条输入就整屏重绘一次（与 opencode 同量级：
36 个 `?2026h/?2026l` 块、每块整屏 SGR）。然后测：
  输入 → 首字节输出 的延迟。

这个数字纯粹是"守护进程链路 + 假 TUI 重绘"的成本。若它也很高，说明瓶颈
在我们这边；若它很低而真实 opencode 高，差额就是 opencode 自己的问题。

用法：
    DEEPTERM_RUNTIME_DIR=... python3 scripts/measure-link-latency.py
"""
import json
import os
import socket
import struct
import sys
import time

H = 9
JSON, DATA, CREDIT = 0, 1, 4
RUN = os.environ.get("DEEPTERM_RUNTIME_DIR", "/tmp/dt-smoke")

# 每收到一次输入就整屏重绘，块结构与 opencode 一致。
FAKE_TUI = r"""
import sys
out = sys.stdout.buffer
out.write(b"\x1b[?1049h\x1b[?1000h\x1b[?1006h\x1b[?25l")
out.flush()
frame = 0
while True:
    data = sys.stdin.buffer.read1(4096)
    if not data:
        break
    frame += 1
    buf = bytearray()
    for _ in range(36):
        buf += b"\x1b[?2026h\x1b[H"
        for r in range(43):
            buf += ("\x1b[%d;1H" % (r + 1)).encode()
            buf += ("\x1b[38;2;%d;%d;%dm" % (
                (frame * 7 + r * 11) % 200 + 50,
                (frame * 3 + r * 5) % 200 + 50,
                (frame * 11 + r) % 200 + 50)).encode()
            buf += (("%05d row %02d " % (frame, r)) + "abcdefghij" * 17)[:180].encode()
        buf += b"\x1b[?2026l"
    out.write(buf)
    out.flush()
"""


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

    sp = call(sock, {"op": "spawn", "cols": 181, "rows": 43,
                     "shell": sys.executable, "args": ["-c", FAKE_TUI]})
    assert sp.get("ok"), sp
    addr = sp["address"]
    print(f"spawned {addr}")
    ch = call(sock, {"op": "attach", "address": addr})["channel"]

    def drain(seconds):
        total = 0
        end = time.time() + seconds
        sock.settimeout(0.005)
        while time.time() < end:
            try:
                c, k, p = frame(sock)
            except (socket.timeout, TimeoutError):
                continue
            if c is None:
                break
            if c == ch and k == DATA:
                total += len(p)
                sock.sendall(enc(ch, CREDIT, struct.pack("<I", len(p))))
        return total

    drain(1.5)  # 首屏 + 稳定

    print("\n输入 → 首字节输出 延迟（12 次）:")
    lat = []
    for i in range(12):
        t0 = time.time()
        sock.sendall(enc(ch, DATA, b"\x1b[<64;10;10M"))
        first = None
        end = t0 + 1.0
        while time.time() < end:
            sock.settimeout(0.002)
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
        drain(0.08)

    lat.sort()
    if lat:
        n = len(lat)
        print(f"  中位 {lat[n // 2]:.1f} ms   最好 {lat[0]:.1f} ms   最差 {lat[-1]:.1f} ms")

    # 一遍完整重绘的输出量
    total = drain(0.2)
    print(f"\n单次重绘输出约 {total} 字节")

    call(sock, {"op": "close", "address": addr})
    print("\n结论：这个数字就是「我们链路 + 假 TUI 整屏重绘」的净成本。")
    print("      与真实 opencode 的差额，归 opencode 自己。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
