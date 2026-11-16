#!/usr/bin/env python3
"""验证守护进程侧的 kitty 图像存储与挂接回放。

流程：
1. 在守护进程里起一个程序，发一张 2x2 的 RGBA 图（`a=t`）+ 显示（`a=p`）；
2. **不带**客户端（attach 之前）也能让守护进程收到图；
3. 挂接时快照必须含 `a=t` 与 `a=p`，且 id 一致；
4. 客户端自己喂同一份字节进本地 VT，图像也要在那儿出现（完整闭环）。

用法：

    cargo build --bin deepcore
    DEEPTERM_RUNTIME_DIR=/tmp/dt-smoke ./target/debug/deepcore &
    DEEPTERM_RUNTIME_DIR=/tmp/dt-smoke python3 scripts/check-kitty-replay.py
"""
import base64
import json
import os
import socket
import struct
import sys
import time

HEADER = 9
JSON, DATA, CLOSE, RESIZE, CREDIT = 0, 1, 2, 3, 4
RUN = os.environ.get("DEEPTERM_RUNTIME_DIR", "/tmp/dt-smoke")


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
    return channel, kind, payload or b""


def call(sock, request, timeout=10.0):
    sock.settimeout(timeout)
    sock.sendall(encode(0, JSON, json.dumps(request).encode()))
    while True:
        frame = read_frame(sock)
        if frame is None:
            raise RuntimeError("daemon closed the connection")
        channel, kind, payload = frame
        if channel == 0 and kind == JSON:
            return json.loads(payload)


def connect():
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect(os.path.join(RUN, "daemon.sock"))
    with open(os.path.join(RUN, "daemon.token")) as f:
        token = f.read().strip()
    sock.sendall(
        encode(0, JSON, json.dumps({"id": 0, "op": "hello", "token": token, "protocol": 2}).encode())
    )
    _, _, payload = read_frame(sock)
    reply = json.loads(payload)
    assert reply.get("ok"), reply
    return sock


def main():
    # 2x2 RGBA：左上红、右上绿、左下蓝、右下白。
    pixels = bytes([
        255, 0, 0, 255, 0, 255, 0, 255,
        0, 0, 255, 255, 255, 255, 255, 255,
    ])
    payload = base64.b64encode(pixels).decode()

    # 程序：用 printf 发 kitty 命令（q=2 静默，不需要等应答），然后停住。
    script = (
        "printf '\\033_Ga=t,f=32,s=2,v=2,i=9,q=2;%s\\033\\\\'\n"
        "printf '\\033_Ga=p,i=9,p=0,c=2,r=1,q=2\\033\\\\'\n"
        "printf 'IMAGE-PLACED\\n'\n"
        "exec sleep 60\n"
    ) % payload

    sock = connect()
    spawned = call(sock, {"op": "spawn", "cols": 40, "rows": 10,
                          "shell": "/bin/sh", "args": ["-c", script]})
    assert spawned["ok"], spawned
    address = spawned["address"]
    print(f"spawned {address}")

    # 关键：**不 attach**，等守护进程自己扫到图像。
    time.sleep(1.0)
    snap = call(sock, {"op": "snapshot", "address": address})
    assert "IMAGE-PLACED" in snap["data"], f"程序应先输出文字: {snap['data'][:200]!r}"
    print("text arrived before attach")

    # 原地重连（history=0）**不**该重传图像：客户端进程还活着、存储还在。
    plain = call(sock, {"op": "attach", "address": address, "history": 0})
    plain_ch = plain["channel"]
    sock.settimeout(1.0)
    plain_bytes = b""
    deadline = time.time() + 2
    while time.time() < deadline:
        try:
            frame = read_frame(sock)
        except (socket.timeout, TimeoutError):
            break
        if frame is None:
            break
        ch, kind, data = frame
        if ch == plain_ch and kind == DATA:
            plain_bytes += data
            sock.sendall(encode(plain_ch, CREDIT, struct.pack("<I", len(data))))
    assert b"a=t,f=32" not in plain_bytes, (
        f"原地重连不该重传图像（几 MB 像素白过一遍 socket）: {plain_bytes[:200]!r}"
    )
    print(f"plain reattach stays image-free ({len(plain_bytes)} bytes)")

    # 新窗口挂接（history>0）：快照必须带回图像。
    channel = call(sock, {"op": "attach", "address": address, "history": 2000})["channel"]
    sock.settimeout(2.0)
    paint = b""
    deadline = time.time() + 3
    while time.time() < deadline and b"a=p,i=9" not in paint:
        try:
            frame = read_frame(sock)
        except (socket.timeout, TimeoutError):
            break
        if frame is None:
            break
        ch, kind, data = frame
        if ch == channel and kind == DATA:
            paint += data
            sock.sendall(encode(channel, CREDIT, struct.pack("<I", len(data))))

    text = paint.decode("utf-8", "replace")
    assert "a=t,f=32,s=2,v=2,i=9" in text, f"挂接必须重传像素: {text[:400]!r}"
    assert "a=p,i=9" in text, f"挂接必须重摆位置: {text[:400]!r}"
    # 回放用 base64 重编码的同一份像素，必须与原 payload 一致。
    assert payload in text, "回放的像素数据必须与原图一致"
    print(f"attach replayed the image ({len(paint)} bytes)")

    call(sock, {"op": "close", "address": address})
    time.sleep(0.3)
    print("PASS: kitty images survive detach and are replayed on attach")
    return 0


if __name__ == "__main__":
    sys.exit(main())
