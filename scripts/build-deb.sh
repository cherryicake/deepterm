#!/bin/bash
set -euo pipefail

# Debian policy：数据文件 0644、目录 0755、maintainer script 0755。默认 umask
# 002（常见于开发机）会让 cat > 与 convert 产出 0664 的文件，而 dpkg-deb 不会
# 替你归一化权限，装出来的包权限就是 664。
umask 022

# DeepTerm 的 Ubuntu/Debian 安装包（.deb）构建脚本。
#
# 与 build-dmg.sh / bundle-windows.ps1 同一套思路：先 `cargo build --release`，
# 再把裸二进制按 Debian 目录规范（/usr/bin、/usr/share/applications…）铺进
# 一个 staging 目录，最后 `dpkg-deb --build`。
#
# 这里**不用** `dpkg-buildpackage`：那套要引入完整的 debian/ 源码包目录
# （changelog/watch/rules/系列源码），而本项目是纯 cargo 工程、没有
# upstream 源码 tarball，铺出来的 debian/ 目录大半是为了满足工具链而非真正
# 有意义。直接用 dpkg-deb 组装 + 精确控制 metadata，更贴合本仓库的打包习惯。
#
# 用法：
#   scripts/build-deb.sh              # 构建 deb
#   scripts/build-deb.sh --install    # 构建后直接装到本机
#   scripts/build-deb.sh --no-build   # 跳过 cargo build（复用已有产物）

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

APP_NAME="DeepTerm"
PKG_NAME="deepterm"
VERSION="0.1.0"
ARCH="$(dpkg --print-architecture)"
SECTION="utils"
PRIORITY="optional"
MAINTAINER="DeepTerm <deepterm@local>"

BUILD_ROOT="${PROJECT_DIR}/target/deb"
DEB_DIR="${PROJECT_DIR}/target/deb-out"
DEB_FILE="${DEB_DIR}/${PKG_NAME}_${VERSION}_${ARCH}.deb"
BIN_SRC="${PROJECT_DIR}/target/release/${PKG_NAME}"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

DO_BUILD=1
DO_INSTALL=0
for arg in "$@"; do
    case "$arg" in
        --install)  DO_INSTALL=1 ;;
        --no-build) DO_BUILD=0 ;;
        -h|--help)  sed -n '2,20p' "${BASH_SOURCE[0]}"; exit 0 ;;
        *) echo -e "${RED}未知参数: $arg${NC}" >&2; exit 1 ;;
    esac
done

command -v dpkg-deb > /dev/null || { echo -e "${RED}缺少 dpkg-deb${NC}" >&2; exit 1; }

if [ "$DO_BUILD" = "1" ]; then
    echo -e "${GREEN}=== 1/4 构建 release 二进制 ===${NC}"
    cd "$PROJECT_DIR"
    cargo build --release --bin "${PKG_NAME}"
fi

[ -f "$BIN_SRC" ] || {
    echo -e "${RED}找不到二进制: ${BIN_SRC}${NC}" >&2
    echo "先跑一次 scripts/build-deb.sh（不带 --no-build）" >&2
    exit 1
}

echo -e "${GREEN}=== 2/4 铺 staging 目录 ===${NC}"
rm -rf "$BUILD_ROOT"
mkdir -p "${BUILD_ROOT}"/{DEBIAN,usr/bin,usr/share/applications,usr/share/doc/"${PKG_NAME}"}

# --- 可执行文件 ---------------------------------------------------------------
install -m 0755 "$BIN_SRC" "${BUILD_ROOT}/usr/bin/${PKG_NAME}"

# --- 图标 ---------------------------------------------------------------------
# 只安装 512x512 原图会让 128/64 的菜单位置去缩放大图，GNOME/KDE 上略显发虚；
# 有 ImageMagick 时顺带补一份 256 与 128，没有也不影响安装与运行。
ICON_SRC="${PROJECT_DIR}/assets/icon.png"
ICON_DIR="${BUILD_ROOT}/usr/share/icons/hicolor"
mkdir -p "${ICON_DIR}/512x512/apps"
install -m 0644 "$ICON_SRC" "${ICON_DIR}/512x512/apps/${PKG_NAME}.png"
if command -v convert > /dev/null; then
    for size in 256 128 64 48; do
        mkdir -p "${ICON_DIR}/${size}x${size}/apps"
        convert "$ICON_SRC" -resize "${size}x${size}" \
            "${ICON_DIR}/${size}x${size}/apps/${PKG_NAME}.png"
    done
else
    echo -e "${YELLOW}未找到 ImageMagick，跳过图标缩放（只装 512x512）${NC}"
fi

# --- terminfo -----------------------------------------------------------------
# 必须装：src/terminal/mappings/mod.rs 的 default_terminal_env() 用
# terminfo_exists("deepterm") 决定 TERM 报什么。缺这条条目时回退
# xterm-256color，应用仍能起，但会丢掉 deepterm.info 里声明的扩展能力
# （kitty 图形协议、多色等），渲染与滚动行为都退回基础档。
# 优先用系统 tic（ncurses-bin）。conda 里的 tic 版本与系统 terminfo 库未必
# 一致，编出来的条目可能带着别的 ncurses 才认的扩展字段。
if [ -x /usr/bin/tic ]; then
    TIC=/usr/bin/tic
else
    TIC="$(command -v tic || true)"
fi
if [ -n "$TIC" ] && [ -f "${PROJECT_DIR}/assets/terminfo/${PKG_NAME}.info" ]; then
    echo "编译 terminfo (${TIC})..."
    # -x = 扩展名（user_defined）条目也编进去；deepterm.info 用了这些能力。
    # -o 显式指定输出目录，避免 tic 落到 ~/.terminfo 里去（那样装进包也不会生效）。
    "$TIC" -x -o "${BUILD_ROOT}/usr/share/terminfo" \
        "${PROJECT_DIR}/assets/terminfo/${PKG_NAME}.info" > /dev/null
else
    echo -e "${YELLOW}缺少 tic 或 terminfo 源文件，跳过（将回退 xterm-256color）${NC}"
fi

# --- desktop entry -----------------------------------------------------------
cat > "${BUILD_ROOT}/usr/share/applications/${PKG_NAME}.desktop" <<EOF
[Desktop Entry]
Type=Application
Version=1.0
Name=${APP_NAME}
GenericName=Terminal Emulator
Comment=A modern terminal emulator with SSH support
Exec=${PKG_NAME} %U
Icon=${PKG_NAME}
Terminal=false
Categories=System;TerminalEmulator;
Keywords=terminal;shell;prompt;command;commandline;ssh;console;
StartupNotify=true
StartupWMClass=deepterm
SingleMainWindow=true
EOF

# --- 文档 ---------------------------------------------------------------------
# debian-policy 要求 copyright 放 DEP-5 许可文本。
cat > "${BUILD_ROOT}/usr/share/doc/${PKG_NAME}/copyright" <<EOF
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: ${APP_NAME}

Files: *
Copyright: 2024 DeepTerm authors
License: unknown
 Licensed under the terms the project is distributed under; see the upstream
 repository for the full text.
EOF
cat > "${BUILD_ROOT}/usr/share/doc/${PKG_NAME}/changelog.Debian" <<EOF
${PKG_NAME} (${VERSION}) unstable; urgency=medium

  * ${VERSION} 本地构建。

 -- DeepTerm <deepterm@local>  $(date -R)
EOF
gzip -9n "${BUILD_ROOT}/usr/share/doc/${PKG_NAME}/changelog.Debian"

echo -e "${GREEN}=== 3/4 生成 DEBIAN 元数据 ===${NC}"

# 依赖从二进制实际链接的 so 反查，而不是照抄模板：gpui 在 X11/Wayland/Vulkan
# 三条后端上的依赖组合会随 zed 上游 rev 变动，写死清单迟早过期。
#
# 只取 **直接** NEEDED（objdump/readelf 的 DT_NEEDED），不用 ldd：ldd 给的是
# 传递闭包，会把 libc6 之类包自己的依赖也拖进来，而那些包自己会声明，重复
# 声明只是让清单更长更脆。
readelf -d "$BIN_SRC" 2>/dev/null |
    sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p' | sort -u > "${BUILD_ROOT}/.needed"

# soname -> 包名。
#
# `dpkg-query -S <完整路径>` 在本机（usr-merge，/lib 是 /usr/lib 的软链）查不到
# 任何东西，所以只能按 basename 查。但 basename 查出来的是**候选集**，直接取
# 第一个会踩四个坑：
#   1. libvulkan.so.1 同时被 libvulkan1 和若干自带 loader 的第三方应用包
#      （实测本机有 adspower-global / google-chrome-stable）提供；
#   2. libc.so.6 同时被 libc6 / libc6-i386 / libc6-dev 提供；
#   3. 多架构包（libc6、libxcb1 都同时装了 amd64 与 i386）上
#      `dpkg-query -W -f='${Architecture}'` 不加 \n 会把两条记录**首尾相连**
#      返回 `amd64i386`，等值比较必然失败——必须按行匹配；
#   4. dpkg 的 diversion（usr-merge 迁移）提示行格式是本地化的，中文环境下
#      是 `libc6 导致转移自：/lib64/...`，字段分隔符还是全角冒号，
#      `cut -d: -f1` 会把整行当成包名。
# 因此：LC_ALL=C 固定英文输出 + 正则滤掉非法包名 + 按行匹配架构/状态 +
# 最后优先挑符合 Debian 命名惯例的那一个（libvulkan.so.1 -> libvulkan1、
# libgcc_s.so.1 -> libgcc-s1，即包名去掉分隔符后以 lib+stem+数字开头）。
soname_to_pkg() {
    local soname="$1" cand pkg key best=""
    key="$(echo "${soname%%.so*}" | tr -d '_-')"          # libvulkan / libgccs / libc
    while read -r cand; do
        pkg="${cand%%:*}"
        dpkg-query -W -f='${Architecture}\n' "$pkg" 2>/dev/null |
            grep -qx "$ARCH" || continue
        dpkg-query -W -f='${db:Status-Status}\n' "$pkg" 2>/dev/null |
            grep -qx installed || continue
        case "$pkg" in *-dev|*-dbg|*-dbgsym|*-doc|*-source) continue ;; esac
        if [[ "$(echo "$pkg" | tr -d '_-')" =~ ^${key}[0-9] ]]; then
            echo "$pkg"; return 0
        fi
        [ -n "$best" ] || best="$pkg"
    done < <(LC_ALL=C dpkg-query -S "$soname" 2>/dev/null |
             cut -d: -f1 | sort -u |
             grep -E '^[a-z0-9][a-z0-9+.-]*$')
    [ -n "$best" ] && echo "$best"
    return 0
}

PKG_DEPS=()
while read -r soname; do
    [ -n "$soname" ] || continue
    pkg="$(soname_to_pkg "$soname")"
    if [ -n "$pkg" ]; then
        PKG_DEPS+=("$pkg")
    else
        echo -e "${YELLOW}警告: ${soname} 找不到归属包，跳过${NC}"
    fi
done < "${BUILD_ROOT}/.needed"

# 补上 DT_NEEDED 看不到、但运行期**一定**要 dlopen 的那几个。gpui 的渲染与
# 窗口后端不走动态链接，而是在 ash / wayland-backend 里 dlopen 库名，所以
# 只看 NEEDED 会漏掉它们，缺了就是「装得上、起不来」（报
# Failed to load create_instance / Cannot use Interface without c_ptr）：
#   - libvulkan1：blade/wgpu 的 Vulkan 后端，本机唯一的渲染路径
#   - libwayland-client0 / libwayland-egl1：Cargo.toml 里 wayland feature
#     打开的会话后端，Wayland 桌面上缺了就起不来窗口
DEP_EXTRA=(libvulkan1 libwayland-client0 libwayland-egl1)

# control 的 Depends 是**逗号分隔**的 dependency list，不是空格分隔——空格会被
# dpkg 的解析器当成 relation 语法，报「在引用软件包 libc6 后有语法错误」。
# paste -sd, 既给出逗号又不会留下尾随逗号。
DEP_ALL=$(printf '%s\n' "${PKG_DEPS[@]}" "${DEP_EXTRA[@]}" |
    grep -v '^$' | sort -u | paste -sd, -)
rm -f "${BUILD_ROOT}/.needed"

INSTALLED_SIZE="$(du -ks "${BUILD_ROOT}/usr" | cut -f1)"

cat > "${BUILD_ROOT}/DEBIAN/control" <<EOF
Package: ${PKG_NAME}
Version: ${VERSION}
Section: ${SECTION}
Priority: ${PRIORITY}
Architecture: ${ARCH}
Depends: ${DEP_ALL}
Installed-Size: ${INSTALLED_SIZE}
Maintainer: ${MAINTAINER}
Homepage: https://github.com/deepterm/deepterm
Description: ${APP_NAME} terminal emulator
 A GPUI-based terminal emulator supporting local shells and SSH sessions,
 with a built-in editor, file tree and language-server support.
EOF

# --- maintainer scripts -------------------------------------------------------
# 只在「确实装了东西」时刷新缓存：卸载后再调 update-desktop-database 会以
# 非零退出，导致 postrm 报错并把包标成 config-files 之外的失败状态。
cat > "${BUILD_ROOT}/DEBIAN/postinst" <<'EOF'
#!/bin/bash
set -e
case "$1" in
    configure)
        if command -v update-desktop-database > /dev/null; then
            update-desktop-database -q /usr/share/applications || true
        fi
        if command -v gtk-update-icon-cache > /dev/null; then
            gtk-update-icon-cache -q -f -t /usr/share/icons/hicolor || true
        fi
        ldconfig || true
        ;;
esac
exit 0
EOF

cat > "${BUILD_ROOT}/DEBIAN/prerm" <<'EOF'
#!/bin/bash
set -e
case "$1" in
    remove|deconfigure)
        if command -v update-desktop-database > /dev/null; then
            update-desktop-database -q /usr/share/applications || true
        fi
        if command -v gtk-update-icon-cache > /dev/null; then
            gtk-update-icon-cache -q -f -t /usr/share/icons/hicolor || true
        fi
        ;;
esac
exit 0
EOF

chmod 0755 "${BUILD_ROOT}/DEBIAN/postinst" "${BUILD_ROOT}/DEBIAN/prerm"

# md5sums 覆盖所有普通文件；DEBIAN/ 自身排除在外（policy 要求）。
( cd "$BUILD_ROOT" && find . -type f ! -path './DEBIAN/*' -printf '%P\0' |
    sort -z | xargs -0 md5sum > DEBIAN/md5sums )

echo -e "${GREEN}=== 4/4 打出 .deb ===${NC}"
mkdir -p "$DEB_DIR"
rm -f "$DEB_FILE"
dpkg-deb --root-owner-group --build "$BUILD_ROOT" "$DEB_FILE"

echo ""
dpkg-deb --info "$DEB_FILE"
echo ""
echo -e "${GREEN}=== 完成: ${DEB_FILE} ===${NC}"
ls -lh "$DEB_FILE"

if [ "$DO_INSTALL" = "1" ]; then
    echo ""
    echo -e "${GREEN}=== 安装到本机 ===${NC}"
    # 用 apt 而非 dpkg -i：依赖不满足时 apt 才会去装，不会把包装到一半留成
    # unconfigured 状态。必须是绝对路径——apt 把相对路径当软件包名解析。
    #
    # --reinstall 不能省：开发时版本号一直是 0.1.0，apt 见「已经是最新版」
    # 就直接跳过，旧二进制还留在 /usr/bin 里，测新构建会白测一场。
    sudo apt-get install -y --reinstall "${DEB_FILE}"
fi