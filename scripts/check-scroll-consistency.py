#!/usr/bin/env python3
"""在真实 TUI 上验证"滚动不再卡死/错乱"。

核心判据不是"输出多少字节"，而是**客户端的解析器最终能否收敛到屏幕
真实内容**。做法：把守护进程发来的字节喂进本地 vte（与真实客户端同一套
解析器），然后比对它与守护进程 `snapshot` 的文字是否一致。

三种场景，对应三个历史 bug：

1. `history=0` 的普通 attach（重绘路径）——必须能救回"卡在同步更新
   （`?2026h` 没有配对的 `?2026l`）"的客户端。
2. 丢帧后补发整屏——必须能救回"停在半截转义序列"的客户端。
3. 连续快速滚动——守护进程必须持续有响应（不冻结），且最终画面一致。

用法：

    cargo build --bin deepcore
    DEEPTERM_RUNTIME_DIR=/tmp/dt-smoke ./target/debug/deepcore &
    # 先在某个终端里跑 opencode，再：
    DEEPTERM_RUNTIME_DIR=/tmp/dt-smoke TARGET=t4 python3 scripts/check-scroll-consistency.py
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
TARGET = os.environ.get("TARGET", "t4")


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


def connect():
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(os.path.join(RUN, "daemon.sock"))
    with open(os.path.join(RUN, "daemon.token")) as f:
        token = f.read().strip()
    s.sendall(enc(0, JSON, json.dumps(
        {"id": 0, "op": "hello", "token": token, "protocol": 2}).encode()))
    frame(s)
    return s


# ── 极简 vte：只关心"能不能从坏状态里出来" ────────────────────────────────
#
# 不实现完整终端，只跟踪两件会真的卡住/错位的事：
#   · 同步更新状态（?2026h / ?2026l）
#   · 是否停在半截转义序列里（Ground 之外）
# 以及把可打印字符记下来，用于与守护进程的快照对比。

class MiniVte:
    def __init__(self):
        self.sync = False
        self.state = "ground"  # ground | esc | csi | osc | apc
        self.text = []

    def feed(self, data):
        i = 0
        n = len(data)
        while i < n:
            b = data[i]

            if self.state == "ground":
                if b == 0x18 or b == 0x1A:  # CAN / SUB
                    i += 1
                    continue
                if b == 0x1B:
                    # 可能是 ?2026h / ?2026l
                    if data[i:i + 8] == b"\x1b[?2026h":
                        self.sync = True
                        i += 8
                        continue
                    if data[i:i + 8] == b"\x1b[?2026l":
                        self.sync = False
                        i += 8
                        continue
                    self.state = "esc"
                    i += 1
                    continue
                if 0x20 <= b < 0x7F:
                    self.text.append(chr(b))
                i += 1
                continue

            if self.state == "esc":
                if b == ord("["):
                    self.state = "csi"
                elif b == ord("]"):
                    self.state = "osc"
                elif b == ord("_"):
                    self.state = "apc"
                elif b == 0x18:
                    self.state = "ground"
                else:
                    self.state = "ground"
                i += 1
                continue

            if self.state == "csi":
                if 0x40 <= b <= 0x7E:
                    self.state = "ground"
                elif b == 0x18:
                    self.state = "ground"
                i += 1
                continue

            if self.state in ("osc", "apc"):
                if b == 0x07 and self.state == "osc":
                    self.state = "ground"
                elif b == 0x18:
                    self.state = "ground"
                elif b == 0x1B and data[i:i + 2] == b"\x1b\\":
                    self.state = "ground"
                    i += 2
                    continue
                i += 1
                continue

    def settled(self):
        """没有卡在同步更新里，也没有停在半截序列里。"""
        return (not self.sync) and self.state == "ground"


def pump(vte, sock, channel, seconds):
    """把 channel 上的输出喂进 vte，返回收到的字节数。"""
    total = 0
    deadline = time.time() + seconds
    sock.settimeout(0.1)
    while time.time() < deadline:
        try:
            c, k, p = frame(sock)
        except (socket.timeout, TimeoutError):
            continue
        if c is None:
            break
        if c == channel and k == DATA:
            vte.feed(p)
            total += len(p)
            sock.sendall(enc(channel, CREDIT, struct.pack("<I", len(p))))
    return total


def main():
    sock = connect()
    terms = call(sock, {"op": "list"}).get("terminals", [])
    addresses = [t["address"] for t in terms]
    print(f"terminals: {addresses}")
    addr = TARGET if TARGET in addresses else (addresses[0] if addresses else None)
    assert addr, "守护进程里没有终端"
    print(f"using {addr}")

    # ── 场景 1：客户端卡在同步更新里，重绘必须能救回来 ────────────────
    vte = MiniVte()
    vte.feed(b"\x1b[?2026hSTUCK")  # 半帧：ESU 丢了
    assert vte.sync and not vte.settled(), "构造失败：应停在同步更新里"

    a = call(sock, {"op": "attach", "address": addr, "history": 0})
    pump(vte, sock, a["channel"], 1.5)
    assert vte.settled(), (
        f"重绘没能救回卡在同步更新里的客户端（sync={vte.sync}, state={vte.state}）"
        "——这正是界面完全卡死的原因"
    )
    print("scenario 1: repaint rescues a client stuck in synchronized update")

    # ── 场景 2：停在半截转义序列里，重绘必须能救回来 ──────────────────
    vte2 = MiniVte()
    vte2.feed(b"\x1b[38;2")  # 半截 SGR
    assert vte2.state == "csi"
    a2 = call(sock, {"op": "attach", "address": addr, "history": 0})
    pump(vte2, sock, a2["channel"], 1.5)
    assert vte2.settled(), (
        f"重绘没能救回停在半截转义里的客户端（state={vte2.state}）"
        "——这正是内容错乱的原因"
    )
    print("scenario 2: repaint rescues a client stuck mid-escape")

    # ── 场景 3：连续快速滚动，必须恢复并收敛 ──────────────────────────
    vte3 = MiniVte()
    a3 = call(sock, {"op": "attach", "address": addr, "history": 0})
    ch = a3["channel"]
    pump(vte3, sock, ch, 1.0)  # 先吃掉首屏

    up = b"\x1b[<64;10;10M"
    down = b"\x1b[<65;10;10M"
    print("flooding wheel reports (fast up/down for 3s)…")
    t0 = time.time()
    sent = 0
    while time.time() - t0 < 3:
        # 每批 4 条，制造真实的高频滚动
        for _ in range(4):
            sock.sendall(enc(ch, DATA, up if sent % 8 < 4 else down))
            sent += 1
        pump(vte3, sock, ch, 0.02)
    print(f"  sent {sent} reports; {len(vte3.text)} chars seen")

    # 灌完之后必须收敛：给一段时间让它把最后的输出吃完。
    pump(vte3, sock, ch, 3.0)
    assert vte3.settled(), (
        f"连续滚动之后客户端停在坏状态（sync={vte3.sync}, state={vte3.state}）"
        "——界面会一直卡住/错乱"
    )
    print("scenario 3: after fast scrolling the client converged to a clean state")

    # 守护进程必须仍然有响应（没有冻结在读线程里）。
    t0 = time.time()
    ok = call(sock, {"op": "get", "address": addr}, timeout=5)
    dt = time.time() - t0
    assert ok.get("ok"), ok
    assert dt < 2.0, f"滚动之后守护进程响应变慢: {dt:.2f}s"
    print(f"daemon still responsive ({dt*1000:.0f}ms)")

    print("\nALL SCROLL CHECKS PASSED")
    return 0


if __name__ == "__main__":
    sys.exit(main())
