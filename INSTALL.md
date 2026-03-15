# DeepTerm 安装指南

## macOS 安装

### 方法 1: 使用 DMG 安装包（推荐）

1. 下载 `DeepTerm-0.1.0-macOS.dmg`
2. 双击 DMG 文件挂载磁盘映像
3. 将 `DeepTerm.app` 拖拽到 `Applications` 文件夹
4. 从 Applications 文件夹启动 DeepTerm

**注意**: 首次启动时，macOS 可能会显示安全警告。请前往 **系统设置 > 隐私与安全性**，然后点击 "仍要打开"。

### 方法 2: 从源码构建

需要 Rust 1.85 或更高版本。

```bash
# 克隆仓库
git clone <repository-url>
cd deepterm

# 构建发布版本
cargo build --release

# 运行
./target/release/deepterm
```

## Linux 安装

### 从源码构建

```bash
# 安装依赖 (Ubuntu/Debian)
sudo apt-get install -y libgtk-3-dev libx11-dev libwayland-dev

# 克隆并构建
git clone <repository-url>
cd deepterm
cargo build --release
```

## Windows 安装

### 从源码构建

需要 Visual Studio 2022 或 Build Tools for Visual Studio 2022。

```bash
# 克隆并构建
git clone <repository-url>
cd deepterm
cargo build --release
```

## 卸载

### macOS
将 `DeepTerm.app` 从 Applications 文件夹移到废纸篓。

### Linux/Windows
删除构建目录或直接删除可执行文件。
