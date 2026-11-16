#!/usr/bin/env python3
"""复现"程序不读 stdin 时界面冻结"，验证修复。

场景：一个 TUI 先输出一批、然后**停止读取 stdin**（模拟整屏重绘期间、
grep 运行期间、less 刚启动还没进读取循环），客户端继续灌滚轮报文把 PTY
写缓冲塞满，之后程序**恢复输出**。

修复前：输入写满 PTY 内核缓冲 → 守护进程那条线程卡在写里（无限重试 +
sleep）→ 输出再也读不出来 → 恢复输出也收不到，屏幕停在最后一帧。
修复后：输入留在待写缓冲里（超额丢最旧），读线程照常跑，恢复输出立刻收到。

用法：

    cargo build --bin deepcore
    DEEPTERM_RUNTIME_DIR=/tmp/dt-smoke ./target/debug/deepcore &
    DEEPTERM_RUNTIME_DIR=/tmp/dt-smoke python3 scripts/check-blocked-input.py
"""
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


# 先输出一批，然后**停止读 stdin** 一会儿（PTY 写端会从这时堵起），
# 再继续输出。若守护进程的读线程被写卡住，第二阶段输出就永远收不到。
STALLER = r"""
import sys, time
out = sys.stdout.buffer
out.write(b"\x1b[?1049h\x1b[?1000h\x1b[?1006h")
for i in range(200):
    out.write(b"before %05d " % i + b"X" * 200 + b"\r\n")
out.flush()
# 不读 stdin：PTY 写端从这里开始堵。
time.sleep(4)
out.write(b"AFTER-THE-STALL-MARKER\r\n")
for i in range(50):
    out.write(b"after %05d " % i + b"Y" * 200 + b"\r\n")
out.flush()
time.sleep(600)
"""


def main():
    sock = connect()
    spawned = call(sock, {"op": "spawn", "cols": 80, "rows": 24,
                          "shell": sys.executable, "args": ["-c", STALLER]})
    assert spawned["ok"], spawned
    address = spawned["address"]
    channel = call(sock, {"op": "attach", "address": address})["channel"]
    print(f"spawned {address}, channel {channel}")

    sock.settimeout(2.0)
    seen = 0
    deadline = time.time() + 3
    while time.time() < deadline and seen < 30_000:
        try:
            frame = read_frame(sock)
        except (socket.timeout, TimeoutError):
            break
        if frame is None:
            break
        ch, kind, payload = frame
        if ch == channel and kind == DATA:
            seen += len(payload)
            sock.sendall(encode(channel, CREDIT, struct.pack("<I", len(payload))))
    print(f"initial output: {seen} bytes")
    assert seen > 10_000, f"程序启动时应先输出一批: {seen}"

    # 程序现在已停止读 stdin。灌 **4 MB** 滚轮报文，远超 PTY 内核写缓冲。
    wheel = b"\x1b[<64;10;10M" * 300
    rounds = 200
    print(f"sending {len(wheel)*rounds} bytes of wheel reports while the program is not reading")
    for _ in range(rounds):
        sock.sendall(encode(channel, DATA, wheel))
        try:
            sock.settimeout(0.05)
            frame = read_frame(sock)
            if frame and frame[0] == channel and frame[1] == DATA:
                sock.sendall(encode(channel, CREDIT, struct.pack("<I", len(frame[2]))))
        except (socket.timeout, TimeoutError):
            pass
    print("input flooded; waiting for the program to resume output")

    # 等程序的第二阶段输出。若读线程被写卡住，这里会永远等不到。
    sock.settimeout(1.0)
    after = b""
    deadline = time.time() + 12
    while time.time() < deadline and b"AFTER-THE-STALL-MARKER" not in after:
        try:
            frame = read_frame(sock)
        except (socket.timeout, TimeoutError):
            continue
        if frame is None:
            break
        ch, kind, payload = frame
        if ch == channel and kind == DATA:
            after += payload
            sock.sendall(encode(channel, CREDIT, struct.pack("<I", len(payload))))

    assert b"AFTER-THE-STALL-MARKER" in after, (
        f"程序恢复输出后仍收不到——读线程被输入写卡死了（收到 {len(after)} 字节）"
    )
    print(f"output resumed after the stall ({len(after)} bytes)")

    assert call(sock, {"op": "close", "address": address})["ok"]
    time.sleep(0.5)
    print("PASS: blocked stdin never froze output")
    return 0


if __name__ == "__main__":
    sys.exit(main())
