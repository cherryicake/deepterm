#!/usr/bin/env bash
#
# 把 dev 构建的裸二进制包成 macOS app bundle。
#
# # 为什么需要这个
#
# macOS 的「本地网络」授权按 **app 身份**发放。裸 Mach-O（`target/release/deepcore`）
# 没有 bundle、没有 Info.plist，LaunchServices 找不到它，系统也就不可能给它授权：
# 所有发往局域网主机的连接会在内核里被 NECP 策略直接丢弃，`connect(2)` 返回
# `EHOSTUNREACH`（errno 65）。表现是 `cargo run` 出来的 deepterm 连不上任何
# 局域网 SSH 服务器，而终端里 `ssh` 一切正常——默认网关那类目标是豁免的，
# 更容易让人误判成路由问题。
#
# `scripts/build-dmg.sh` 在**打包**时会生成这两个 bundle，但 `cargo build` 不会。
# 所以开发时先用这个脚本把产物包好，再启动 GUI：
#
#     ./scripts/dev-bundle.sh && ./target/release/deepterm
#
# 它只做三件事，全部与 build-dmg.sh 保持一致，避免 dev 与分发包行为分叉：
#
#   1. `DeepTerm Core.app` —— core 的独立身份（`dev.deepterm.core`）；
#   2. `DeepHolder.app` —— holder 的独立身份（`dev.deepterm.holder`），
#      活动监视器里才不会把它显示成 core；
#   3. 两份二进制同一构建的断言（LC_UUID 必须一致，否则新 holder 立刻 drain）。
#
# 产物落在 `target/Resources/`，正是 `gui_core_executable()` 会去找的位置。

set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${PROJECT_DIR}"

PROFILE="${1:-release}"
CORE_BIN="${PROJECT_DIR}/target/${PROFILE}/deepcore"
RESOURCES="${PROJECT_DIR}/target/Resources"
CORE_APP="${RESOURCES}/DeepTerm Core.app"
HOLDER_APP="${RESOURCES}/DeepHolder.app"
LSREGISTER="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"

GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
NC='\033[0m'

if [ "$(uname -s)" != "Darwin" ]; then
    echo "这个脚本只用于 macOS；其它平台不需要 app bundle。"
    exit 0
fi

if [ ! -f "${CORE_BIN}" ]; then
    echo -e "${RED}Error: ${CORE_BIN} 不存在${NC}"
    echo "先构建：cargo build --${PROFILE} --bins"
    exit 1
fi

CORE_VERSION="$("${CORE_BIN}" --version | awk '{print $2}')"
mkdir -p "${CORE_APP}/Contents/MacOS" "${HOLDER_APP}/Contents/MacOS"

# 每次都重新复制：二进制可能刚被重新编译过，旧副本会让 core/holder 指纹失配。
cp "${CORE_BIN}" "${CORE_APP}/Contents/MacOS/deepcore"
cp "${CORE_BIN}" "${HOLDER_APP}/Contents/MacOS/deepholder"
chmod +x "${CORE_APP}/Contents/MacOS/deepcore" "${HOLDER_APP}/Contents/MacOS/deepholder"

sed "s/@VERSION@/${CORE_VERSION}/g" \
    "${PROJECT_DIR}/distribution/macos-core-Info.plist" \
    > "${CORE_APP}/Contents/Info.plist"
sed "s/@VERSION@/${CORE_VERSION}/g" \
    "${PROJECT_DIR}/distribution/macos-holder-Info.plist" \
    > "${HOLDER_APP}/Contents/Info.plist"

# 签名必须**先绑定可执行文件、再签 bundle**，且每次都重签：改过的二进制上
# 残留的旧签名会让 LaunchServices 认不出它是哪个 app。
codesign --force --sign - --identifier dev.deepterm.core \
    "${CORE_APP}/Contents/MacOS/deepcore"
codesign --force --sign - "${CORE_APP}"
codesign --force --sign - --identifier dev.deepterm.holder \
    "${HOLDER_APP}/Contents/MacOS/deepholder"
codesign --force --sign - "${HOLDER_APP}"

# 两份二进制必须来自同一次构建。签名会改写文件尾部，所以比 LC_UUID 而不是
# 文件哈希——`identity::build_id()` 在 macOS 上用的正是它。
uuid_of() {
    otool -l "$1" | awk '/LC_UUID/{found=1; next} found && $1=="uuid"{print $2; exit}'
}
CORE_UUID=$(uuid_of "${CORE_APP}/Contents/MacOS/deepcore")
HOLDER_UUID=$(uuid_of "${HOLDER_APP}/Contents/MacOS/deepholder")
if [ -z "${CORE_UUID}" ] || [ "${CORE_UUID}" != "${HOLDER_UUID}" ]; then
    echo -e "${RED}Error: core 与 holder 的 LC_UUID 不一致（${CORE_UUID} vs ${HOLDER_UUID}）${NC}"
    exit 1
fi

# 不注册到 LaunchServices 的话，系统看到的是一个不在数据库里的可执行文件，
# 首次「本地网络」提示会用 Apple 的通用文案，且授权可能与 bundle 对不上。
if [ -x "${LSREGISTER}" ]; then
    "${LSREGISTER}" -f "${CORE_APP}" >/dev/null 2>&1 || true
    "${LSREGISTER}" -f "${HOLDER_APP}" >/dev/null 2>&1 || true
fi

echo -e "${GREEN}dev bundle 已就绪:${NC}"
echo "  ${CORE_APP}"
echo "  ${HOLDER_APP}"
echo "  构建指纹: ${CORE_UUID}"
echo
echo "启动 GUI: ${PROJECT_DIR}/target/${PROFILE}/deepterm"
echo -e "${YELLOW}首次连接局域网主机会弹「本地网络」授权，必须允许。${NC}"
