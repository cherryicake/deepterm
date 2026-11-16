#!/usr/bin/env python3
"""验证真实升级路径：旧 core + 旧 holder + 活终端，只换掉 core。

这是用户升级后实际发生的事：安装包装了新的 `deepcore`，但上一版的 core
与 holder 还在跑、还揣着他正在用的终端。新 GUI 连上来时握手会失败（旧 core
不报协议版本）。

正确行为：**只重启 core**，不碰 holder。holder 是终端进程的父进程，结束它
等于把用户的活全杀了；core 只是转发者，重启它不牵动任何程序。

这个脚本用**两个真实版本的二进制**跑完整条链：

1. 用旧版 `deepcore` 起 core（它会拉起旧版 holder 并 spawn 一个终端）；
2. 新客户端握手 → 判定版本不匹配；
3. 按 `client::restart_stale_core` 的动作结束旧 core、删 socket；
4. 用新 `deepcore` 起 core，它要能重新连上**旧 holder**、列出那个终端、
   并把它接回来（内容与交互都在）。

用法：

    DEEPCORE_OLD=/Applications/DeepTerm.app/Contents/MacOS/deepcore \\
    DEEPCORE_NEW=target/debug/deepcore \\
    python3 scripts/check-core-upgrade.py
"""
import json
import os
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time

OLD_CORE = os.environ.get("DEEPCORE_OLD", "/Applications/DeepTerm.app/Contents/MacOS/deepcore")
NEW_CORE = os.environ.get(
    "DEEPCORE_NEW",
    os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "target/debug/deepcore"),
)
H = 9


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
    s.sendall(enc(0, 0, json.dumps(r).encode()))
    while True:
        c, k, p = frame(s)
        if c == 0 and k == 0:
            return json.loads(p)


def connect(run, protocol=None, timeout=5):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(timeout)
    s.connect(os.path.join(run, "daemon.sock"))
    tok = open(os.path.join(run, "daemon.token")).read().strip()
    hello = {"id": 0, "op": "hello", "token": tok}
    if protocol is not None:
        hello["protocol"] = protocol
    s.sendall(enc(0, 0, json.dumps(hello).encode()))
    c, k, p = frame(s)
    return s, json.loads(p)


def main():
    run = tempfile.mkdtemp(prefix="dt-upgrade-")
    env = dict(os.environ)
    env["DEEPTERM_RUNTIME_DIR"] = run
    print(f"runtime dir: {run}")

    # ① 旧版 core（它会拉起旧版 holder 并持有终端）。
    old = subprocess.Popen(
        [OLD_CORE],
        env=env,
        stdout=open(os.path.join(run, "old-core.log"), "wb"),
        stderr=subprocess.STDOUT,
    )
    deadline = time.time() + 20
    while time.time() < deadline:
        try:
            s, hello = connect(run)
            if hello.get("ok"):
                break
        except Exception:
            pass
        time.sleep(0.2)
    else:
        raise SystemExit("旧 core 没能起来")

    print(f"old core hello: {hello}")
    assert "protocol" not in hello, "这一版应当是不报协议的旧版"

    spawned = call(s, {"op": "spawn", "cols": 80, "rows": 24,
                       "shell": "/bin/sh",
                       "args": ["-c", "echo OLD-TERMINAL-ALIVE; exec sleep 900"]})
    assert spawned["ok"], spawned
    addr = spawned["address"]
    ch = call(s, {"op": "attach", "address": addr})["channel"]
    s.settimeout(2)
    seen = b""
    deadline = time.time() + 5
    while time.time() < deadline and b"OLD-TERMINAL-ALIVE" not in seen:
        try:
            c, k, p = frame(s)
        except (socket.timeout, TimeoutError):
            break
        if c == ch and k == 1:
            seen += p
    assert b"OLD-TERMINAL-ALIVE" in seen, f"旧 core 的终端应能输出: {seen[:200]!r}"
    print(f"old core holds {addr} with live output ({len(seen)} bytes)")

    # 记下 holder 的 pid：整条升级路径里它**必须**保持不变。
    holder_pid = int(open(os.path.join(run, "holder.pid")).read().strip())
    print(f"holder pid {holder_pid} (must survive)")

    # ② 新客户端握手：旧 core 回 ok 但不报协议版本。
    s2, hello2 = connect(run, protocol=2)
    print(f"new client vs old core: {hello2}")
    assert hello2.get("ok") and "protocol" not in hello2
    s2.close()

    # ③ 只结束 core。与 client::restart_stale_core 同一动作。
    os.kill(old.pid, signal.SIGTERM)
    old.wait(timeout=10)
    sock = os.path.join(run, "daemon.sock")
    if os.path.exists(sock):
        os.remove(sock)
    print("stale core stopped (holder untouched)")

    # ④ 新 core 起来，重新连上旧 holder。
    new = subprocess.Popen(
        [NEW_CORE],
        env=env,
        stdout=open(os.path.join(run, "new-core.log"), "wb"),
        stderr=subprocess.STDOUT,
    )
    deadline = time.time() + 20
    s3 = None
    while time.time() < deadline:
        try:
            s3, hello3 = connect(run, protocol=2)
            if hello3.get("ok") and hello3.get("protocol") == 2:
                break
            s3.close()
            s3 = None
        except Exception:
            pass
        time.sleep(0.2)
    assert s3 is not None, "新 core 没有起来"
    print("new core is up and speaks protocol 2")

    # holder 必须是同一个进程。
    now_holder = int(open(os.path.join(run, "holder.pid")).read().strip())
    assert now_holder == holder_pid, (
        f"holder 被换掉了（{holder_pid} → {now_holder}）——那会带走用户的终端"
    )

    # 终端还在、内容还在、还能打字。
    got = call(s3, {"op": "get", "address": addr})
    assert got.get("ok"), got
    print(f"terminal survived: {got['terminal']['state']}")

    ch3 = call(s3, {"op": "attach", "address": addr, "history": 200})["channel"]
    s3.settimeout(2)
    buf = b""
    deadline = time.time() + 5
    while time.time() < deadline and b"OLD-TERMINAL-ALIVE" not in buf:
        try:
            c, k, p = frame(s3)
        except (socket.timeout, TimeoutError):
            break
        if c == ch3 and k == 1:
            buf += p
    assert b"OLD-TERMINAL-ALIVE" in buf, f"换 core 后屏幕内容丢了: {buf[:300]!r}"
    print("screen content survived")

    s3.sendall(enc(ch3, 1, b"echo STILL-WORKS\n"))
    buf = b""
    deadline = time.time() + 5
    while time.time() < deadline and b"STILL-WORKS" not in buf.replace(b"\r", b""):
        try:
            c, k, p = frame(s3)
        except (socket.timeout, TimeoutError):
            break
        if c == ch3 and k == 1:
            buf += p
    assert b"STILL-WORKS" in buf.replace(b"\r", b""), f"接回后不能交互: {buf[-300:]!r}"
    print("terminal still interactive")

    call(s3, {"op": "close", "address": addr})
    time.sleep(0.5)

    # 收尾：结束新 core 与 holder。
    new.terminate()
    new.wait(timeout=5)
    try:
        os.kill(holder_pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    holder_pid_file = os.path.join(run, "holder.pid")
    if os.path.exists(holder_pid_file):
        try:
            os.kill(int(open(holder_pid_file).read().strip()), signal.SIGTERM)
        except Exception:
            pass
    time.sleep(0.5)
    shutil.rmtree(run, ignore_errors=True)
    print("PASS: upgrading the core keeps the holder and its terminals alive")
    return 0


if __name__ == "__main__":
    sys.exit(main())
