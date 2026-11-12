#!/usr/bin/env python3
"""端到端验证守护进程的持久化：关掉客户端后程序仍在跑、重连能恢复屏幕。

用法：

    cargo build --bin deepterm
    DEEPTERM_RUNTIME_DIR=/tmp/dt-smoke ./target/debug/deepterm __daemon &
    DEEPTERM_RUNTIME_DIR=/tmp/dt-smoke python3 scripts/check-daemon.py

这个脚本验的是产品承诺本身——"关掉 deepterm，终端不死"，而不是某个函数
的返回值。改协议或改重绘逻辑时先跑它，比翻单元测试快。

不放进 `cargo test`：它要起真实 PTY 与真实 shell，在受限的 CI 沙箱里会
因为 `posix_openpt` 被拒而失败（和仓库里那几个已知的 PTY 用例同源）。
"""

import json
import os
import socket
import struct
import sys
import time

HEADER = 9
JSON, DATA, CLOSE, RESIZE, CREDIT, PING = 0, 1, 2, 3, 4, 5


def encode(channel, kind, payload=b""):
    return struct.pack("<IBI", channel, kind, len(payload)) + payload


def recv_exact(sock, n):
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            return None
        buf += chunk
    return buf


def read_frame(sock):
    header = recv_exact(sock, HEADER)
    if header is None:
        return None
    channel, kind, length = struct.unpack("<IBI", header)
    payload = recv_exact(sock, length) if length else b""
    if payload is None:
        return None
    return channel, kind, payload


def call(sock, request, timeout=5.0):
    sock.settimeout(timeout)
    sock.sendall(encode(0, JSON, json.dumps(request).encode()))
    while True:
        frame = read_frame(sock)
        if frame is None:
            raise RuntimeError("daemon closed the connection")
        channel, kind, payload = frame
        if channel == 0 and kind == JSON:
            return json.loads(payload)


def connect(path):
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect(path)
    return sock


def pump_until(sock, channel, needle, timeout=5.0):
    """读到输出里出现 needle 为止，返回累积的字节。"""
    sock.settimeout(timeout)
    seen = b""
    deadline = time.time() + timeout
    while time.time() < deadline and needle not in seen.replace(b"\r", b""):
        try:
            frame = read_frame(sock)
        except socket.timeout:
            break
        if frame is None:
            break
        ch, kind, payload = frame
        if ch == channel and kind == DATA:
            seen += payload
    return seen


def main():
    runtime_dir = os.environ.get("DEEPTERM_RUNTIME_DIR", "/tmp/dt-smoke")
    path = os.path.join(runtime_dir, "daemon.sock")
    print(f"connecting to {path}")

    # ---- 第一次连接：起一个终端 ----
    sock = connect(path)
    ping = call(sock, {"op": "ping"})
    assert ping["ok"], ping
    print(f"daemon version {ping['version']}")

    spawned = call(sock, {"op": "spawn", "cols": 100, "rows": 30})
    assert spawned["ok"], spawned
    address = spawned["address"]
    print(f"spawned {address}")

    attached = call(sock, {"op": "attach", "address": address})
    assert attached["ok"], attached
    channel = attached["channel"]

    marker = "DEEPTERM-PERSIST-MARKER"
    sock.sendall(encode(channel, DATA, f"echo {marker}\n".encode()))
    seen = pump_until(sock, channel, marker.encode())
    assert marker.encode() in seen.replace(b"\r", b""), f"shell 没有回应: {seen[-400:]!r}"
    print(f"shell responded ({len(seen)} bytes)")

    # ---- 模拟关闭窗口：直接断开 socket ----
    sock.close()
    print("client disconnected (simulating window close)")
    time.sleep(1.0)

    # ---- 第二次连接：终端还活着，屏幕还在 ----
    sock2 = connect(path)
    got = call(sock2, {"op": "get", "address": address})
    assert got["ok"], got
    assert got["terminal"]["state"] == "live", f"终端应该还活着: {got}"
    print("terminal survived the disconnect")

    snap = call(sock2, {"op": "snapshot", "address": address})
    assert snap["ok"], snap
    assert marker in snap["data"], f"重开后屏幕应含标记，实际:\n{snap['data']!r}"
    print("screen recovered on reattach")

    # ---- 接回后还能继续交互 ----
    attached2 = call(sock2, {"op": "attach", "address": address})
    sock2.sendall(encode(attached2["channel"], DATA, b"echo STILL-ALIVE\n"))
    seen2 = pump_until(sock2, attached2["channel"], b"STILL-ALIVE")
    assert b"STILL-ALIVE" in seen2, f"接回后应可交互: {seen2[-300:]!r}"
    print("reattached terminal still interactive")
    sock2.close()

    print("\nALL CHECKS PASSED")
    return 0


if __name__ == "__main__":
    sys.exit(main())
