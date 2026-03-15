#!/bin/bash
# 测试 DeepTerm 启动脚本

echo "=== DeepTerm 启动测试 ==="

# 方法 1: 直接运行二进制
echo ""
echo "方法 1: 直接运行二进制..."
./target/release/deepterm &
sleep 2
if pgrep -x "deepterm" > /dev/null; then
    echo "✓ 二进制直接运行成功"
    killall deepterm 2>/dev/null
else
    echo "✗ 二进制运行失败"
fi

# 方法 2: 从 .app bundle 运行
echo ""
echo "方法 2: 从 .app bundle 运行..."
if [ -d "DeepTerm.app" ]; then
    ./DeepTerm.app/Contents/MacOS/deepterm &
    sleep 2
    if pgrep -x "deepterm" > /dev/null; then
        echo "✓ .app bundle 运行成功"
        killall deepterm 2>/dev/null
    else
        echo "✗ .app bundle 运行失败"
    fi
else
    echo "✗ DeepTerm.app 不存在"
fi

# 方法 3: 使用 open 命令
echo ""
echo "方法 3: 使用 open 命令..."
if [ -d "DeepTerm.app" ]; then
    open DeepTerm.app &
    sleep 3
    if pgrep -x "deepterm" > /dev/null; then
        echo "✓ open 命令启动成功"
    else
        echo "✗ open 命令启动失败"
    fi
else
    echo "✗ DeepTerm.app 不存在"
fi

echo ""
echo "=== 测试完成 ==="
echo ""
echo "请检查:"
echo "1. 如果方法 1 成功但方法 2/3 失败，说明 .app bundle 配置有问题"
echo "2. 如果都成功，双击应该也能正常工作"
echo "3. 请确保你是在 Finder 中双击 .app，而不是在终端中"
