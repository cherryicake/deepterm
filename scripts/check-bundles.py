#!/usr/bin/env python3
"""校验 macOS 分发包里 core 与 holder 的 bundle 布局。

用法：

    python3 scripts/check-bundles.py /Applications/DeepTerm.app
    python3 scripts/check-bundles.py ~/Documents/workspace/deepterm/DeepTerm.app

验的是三件**只有打包之后才可能出错**的事：

1. holder 有自己独立的 `DeepHolder.app`，活动监视器里才不会显示成 core；
2. core 与 holder 的二进制来自同一次构建（`LC_UUID` 相同）——
   `identity::build_id()` 用它判断“新终端交给哪个 holder”，指纹不一致会
   让刚拉起的 holder 立刻 drain；
3. 两个 bundle 的 `CFBundleIdentifier` 不同，TCC 授权不会互相覆盖。

不放进 `cargo test`：它需要真实的分发包，而 `cargo test` 只产出裸二进制。
"""

import os
import plistlib
import subprocess
import sys

CORE_APP = "Contents/Resources/DeepTerm Core.app"
HOLDER_APP = "Contents/Resources/DeepHolder.app"
CORE_BIN = "Contents/MacOS/deepcore"
HOLDER_BIN = "Contents/MacOS/deepholder"

failures = []


def check(condition, message):
    if condition:
        return True
    failures.append(message)
    print(f"  FAIL {message}")
    return False


def uuid_of(path):
    """读链接器写入的 LC_UUID；签名不会改动它。"""
    try:
        output = subprocess.run(
            ["otool", "-l", path], capture_output=True, text=True, check=True
        ).stdout
    except (OSError, subprocess.CalledProcessError) as error:
        return None, f"otool 失败: {error}"
    lines = output.splitlines()
    for index, line in enumerate(lines):
        if "LC_UUID" in line:
            for follow in lines[index + 1 : index + 3]:
                fields = follow.split()
                if len(fields) == 2 and fields[0] == "uuid":
                    return fields[1], None
    return None, "没有 LC_UUID"


def identifier_of(app):
    plist = os.path.join(app, "Contents/Info.plist")
    try:
        with open(plist, "rb") as handle:
            return plistlib.load(handle).get("CFBundleIdentifier"), None
    except (OSError, plistlib.InvalidFileException) as error:
        return None, str(error)


def main():
    root = sys.argv[1] if len(sys.argv) > 1 else "/Applications/DeepTerm.app"
    core_app = os.path.join(root, CORE_APP)
    holder_app = os.path.join(root, HOLDER_APP)
    core_bin = os.path.join(core_app, CORE_BIN)
    holder_bin = os.path.join(holder_app, HOLDER_BIN)

    print(f"checking {root}")

    print("bundle layout")
    check(os.path.isfile(core_bin), f"缺少 {core_bin}")
    check(
        os.path.isfile(holder_bin),
        f"缺少 {holder_bin}（holder 会退回 core 的二进制，活动监视器里显示成同一个名字）",
    )

    if not (os.path.isfile(core_bin) and os.path.isfile(holder_bin)):
        return 1

    print("build fingerprints")
    core_uuid, core_error = uuid_of(core_bin)
    holder_uuid, holder_error = uuid_of(holder_bin)
    check(core_uuid is not None, f"读 core 的 LC_UUID 失败: {core_error}")
    check(holder_uuid is not None, f"读 holder 的 LC_UUID 失败: {holder_error}")
    check(
        core_uuid == holder_uuid,
        f"core 与 holder 的 LC_UUID 不一致（{core_uuid} vs {holder_uuid}），"
        "新 holder 会被判成旧构建并立刻 drain",
    )
    if core_uuid and core_uuid == holder_uuid:
        print(f"  core = holder = {core_uuid}")

    print("bundle identifiers")
    core_id, core_id_error = identifier_of(core_app)
    holder_id, holder_id_error = identifier_of(holder_app)
    check(core_id is not None, f"读 core 的 Info.plist 失败: {core_id_error}")
    check(holder_id is not None, f"读 holder 的 Info.plist 失败: {holder_id_error}")
    check(
        core_id != holder_id,
        f"两个 bundle 的 identifier 相同（{core_id}），签名会互相覆盖",
    )
    if core_id:
        print(f"  core = {core_id}")
    if holder_id:
        print(f"  holder = {holder_id}")

    print("codesign")
    for app, expected in ((core_app, "dev.deepterm.core"), (holder_app, "dev.deepterm.holder")):
        try:
            output = subprocess.run(
                ["codesign", "-d", "--verbose=2", app],
                capture_output=True,
                text=True,
                check=False,
            ).stderr
        except OSError as error:
            check(False, f"codesign 不可用: {error}")
            continue
        check(f"Identifier={expected}" in output, f"{app} 的签名 identifier 不是 {expected}")

    if failures:
        print(f"\n{len(failures)} 项失败")
        return 1
    print("\nALL BUNDLE CHECKS PASSED")
    return 0


if __name__ == "__main__":
    sys.exit(main())
