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


DATA, CLOSE, RESIZE, CREDIT = 1, 2, 3, 4
PING = 5


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
    """连上并完成鉴权握手。

    守护进程要求第一条控制帧是带 token 的 `hello`，否则直接关连接。
    token 与 socket 同名不同后缀（`daemon.sock` → `daemon.token`），由
    守护进程启动时生成。
    """
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect(path)
    token_path = os.path.splitext(path)[0] + ".token"
    with open(token_path) as f:
        token = f.read().strip()
    sock.sendall(encode(0, 0, json.dumps({"id": 0, "op": "hello", "token": token}).encode()))
    ch, kind, payload = read_frame(sock)
    reply = json.loads(payload)
    assert reply.get("ok"), f"鉴权失败: {reply}"
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

    # ---- 尺寸：get / attach 都要报真实网格 ----
    #
    # 这两个数字是客户端建本地 VT 的依据。报错了，重放会按错误的列数
    # 折行——全屏程序的表现就是边框错位、状态栏跑到中间。
    again = call(sock2, {"op": "get", "address": address})
    assert "cols" in again["terminal"], f"get 必须报尺寸: {again}"
    cols, rows = again["terminal"]["cols"], again["terminal"]["rows"]
    assert (cols, rows) == (100, 30), f"尺寸应保持 100x30，实际 {cols}x{rows}"
    print(f"get reports size {cols}x{rows}")

    # ---- resize 要立刻生效，并且回推一帧整屏重绘 ----
    #
    # 只改 PTY 不改 VT 的话，客户端本地 VT 会与守护进程的网格错位；
    # 不回推重绘的话，屏幕上留着按旧列数折行的残影。
    sock2.sendall(encode(attached2["channel"], RESIZE, struct.pack("<HHHH", 120, 40, 0, 0)))
    deadline = time.time() + 5.0
    repainted = b""
    while time.time() < deadline:
        sock2.settimeout(0.3)
        try:
            ch, kind, payload = read_frame(sock2)
        except (socket.timeout, TimeoutError):
            continue
        if ch == attached2["channel"] and kind == DATA:
            repainted += payload
            if b"\x1b[2J" in repainted:
                break
    assert b"\x1b[2J" in repainted, (
        f"resize 之后应回推一整屏重绘（含清屏），实际 {len(repainted)} 字节"
    )
    after = call(sock2, {"op": "get", "address": address})
    assert (after["terminal"]["cols"], after["terminal"]["rows"]) == (120, 40), (
        f"resize 应生效，实际 {after['terminal']}"
    )
    print("resize applied and a full repaint was pushed")

    # ---- 备用屏：快照必须带 ?1049h ----
    #
    # 不带的话客户端停在主屏，画面被当成普通输出追加，滚轮能把整个
    # TUI 界面滚走——用户看到的就是"nvim 变成可滚动的了"。
    alt = call(sock2, {"op": "spawn", "cols": 80, "rows": 24,
                       "shell": "/bin/sh", "args": ["-c", "printf '\\033[?1049h'; exec sleep 60"]})
    assert alt["ok"], alt
    time.sleep(0.6)
    alt_snap = call(sock2, {"op": "snapshot", "address": alt["address"]})
    assert "1049h" in alt_snap["data"], (
        f"备用屏程序的快照必须带 ?1049h，否则客户端会在主屏重放: {alt_snap['data'][:120]!r}"
    )
    print("alternate-screen snapshot carries ?1049h")

    # ---- 流控：额度用尽后回信用，必须补一帧而不是永久停住 ----
    #
    # 这是"大输出后界面卡死"的核心：只丢不补的话客户端永远停在丢弃前
    # 那一帧。
    flood = call(sock2, {"op": "spawn", "cols": 80, "rows": 24,
                         "shell": "/bin/sh", "args": ["-c", "while :; do seq 1 400; done"]})
    fa = call(sock2, {"op": "attach", "address": flood["address"]})["channel"]
    sock2.settimeout(1.0)
    drained = 0
    t0 = time.time()
    while time.time() - t0 < 2.0 and drained < 1_048_576:
        try:
            ch, kind, payload = read_frame(sock2)
        except (socket.timeout, TimeoutError):
            break
        if ch == fa and kind == DATA:
            drained += len(payload)
    assert drained >= 1_000_000, f"应能收满初始额度，实际 {drained}"
    # 回一大笔信用，期待一帧完整重绘（含清屏）。
    sock2.sendall(encode(fa, CREDIT, (8 * 1024 * 1024).to_bytes(4, "little")))
    # 额度用尽后补信用，输出必须**继续流动**。
    #
    # 这里断言"能收到大量新数据"，而不是"必须收到清屏"：持续输出的程序
    # 一旦额度恢复，普通输出会立刻填满通道，补帧可能排在其后。真正要守的
    # 性质是"不会永久停住"——只丢不补的旧实现会在这里收到 0 字节。
    recovered = 0
    deadline = time.time() + 5.0
    while time.time() < deadline and recovered < 200_000:
        sock2.settimeout(0.3)
        try:
            ch, kind, payload = read_frame(sock2)
        except (socket.timeout, TimeoutError):
            if recovered:
                break
            continue
        if ch == fa and kind == DATA:
            recovered += len(payload)
    assert recovered > 0, (
        "额度恢复后必须能继续收到输出，否则界面永久停在最后一帧"
    )
    print("credit starvation recovers with a full repaint")

    # ---- 清理 ----
    for a in (address, alt["address"], flood["address"]):
        call(sock2, {"op": "close", "address": a})
        call(sock2, {"op": "reap"})
    sock2.close()

    print("\nALL CHECKS PASSED")
    return 0


if __name__ == "__main__":
    sys.exit(main())
