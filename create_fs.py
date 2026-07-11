#!/usr/bin/env python3
import subprocess
import os
import glob
import tempfile
import shutil
import argparse
import sys

def find_tool(candidates):
    for path in candidates:
        if shutil.which(path):
            return shutil.which(path)
        if os.path.exists(path) and os.access(path, os.X_OK):
            return path
    return None

def create_ext2_filesystem(filename, size_mb=128):
    """创建 4K 块大小的 ext2 文件系统并用 debugfs 写入文件（兼容 macOS）。"""

    print(f"创建 {size_mb}MB 的ext2(4K) 文件系统映像: {filename}")

    # 1. 创建空的映像文件
    with open(filename, 'wb') as f:
        f.seek(size_mb * 1024 * 1024 - 1)
        f.write(b'\0')

    # 2. 格式化为 ext2（块大小 4096，卷标 LITEOS）
    mke2fs = find_tool([
        'mke2fs',
        '/opt/homebrew/opt/e2fsprogs/sbin/mke2fs',
        '/usr/local/opt/e2fsprogs/sbin/mke2fs',
        '/usr/sbin/mke2fs',
    ])
    if not mke2fs:
        print("✗ 未找到 mke2fs（e2fsprogs）。请安装: brew install e2fsprogs 或 apt install e2fsprogs")
        return False

    try:
        subprocess.run([mke2fs, '-t', 'ext2', '-b', '4096', '-L', 'LITEOS', filename],
                       check=True, capture_output=True)
        print("✓ ext2 文件系统创建成功 (4K 块)")
    except subprocess.CalledProcessError as e:
        print(f"✗ mke2fs 失败: {e}\n{e.stderr.decode(errors='ignore')}")
        return False

    # 3. 使用 debugfs 写入文件（macOS 无法直接挂载 ext2）
    debugfs = find_tool([
        'debugfs',
        '/opt/homebrew/opt/e2fsprogs/sbin/debugfs',
        '/usr/local/opt/e2fsprogs/sbin/debugfs',
        '/usr/sbin/debugfs',
    ])
    if not debugfs:
        print("✗ 未找到 debugfs（e2fsprogs）。请安装: brew install e2fsprogs 或 apt install e2fsprogs")
        return False

    return copy_files_to_ext2(filename, debugfs)

def collect_binaries():
    """仅收集最小用户态启动骨架允许进入镜像的 ELF。"""

    # 查找并复制用户程序ELF文件（原始ELF文件，不是.bin）
    user_elfs = []
    for elf_file in glob.glob("target/riscv64gc-unknown-none-elf/release/*"):
        basename = os.path.basename(elf_file)
        if (os.path.isfile(elf_file) and
            not elf_file.endswith('.d') and
            not elf_file.endswith('.bin') and
            not elf_file.endswith('.json') and
            not elf_file.endswith('.rlib') and
            not basename.startswith('._') and  # 过滤 macOS AppleDouble 文件
            '.' not in basename):
            user_elfs.append(elf_file)

    bin_commands = {'init'}
    bin_entries = []   # (src, '/bin/name')
    if user_elfs:
        for elf_file in user_elfs:
            basename = os.path.basename(elf_file)
            if basename in bin_commands:
                bin_entries.append((elf_file, f"/bin/{basename}"))
        print(f"允许写入镜像的用户程序: {[os.path.basename(src) for src, _ in bin_entries]}")
    else:
        print("⚠ 未找到用户程序ELF文件")

    return bin_entries

def copy_files_to_ext2(image_path, debugfs_bin):
    """通过 debugfs 将文件写入 ext2 镜像。"""
    bin_entries = collect_binaries()

    # 构建 debugfs 命令脚本
    commands = []
    commands.append("mkdir /bin")
    for src, dst in bin_entries:
        # debugfs 的 write 语法: write <native_file> <dest_file>
        commands.append(f"write {src} {dst}")
        # 内核按 root execve 语义要求至少一个 execute bit；缺少此 mode 会使 init 在启动时正确被拒绝。
        commands.append(f"set_inode_field {dst} mode 0100755")

    # 写入临时脚本并执行
    with tempfile.NamedTemporaryFile('w', delete=False) as tf:
        for line in commands:
            tf.write(line + "\n")
        script_path = tf.name

    try:
        subprocess.run([debugfs_bin, '-w', '-f', script_path, image_path], check=True)
        print("✓ 已将文件写入 ext2 镜像")
    except subprocess.CalledProcessError as e:
        print(f"✗ 写入失败: {e}")
        return False
    finally:
        try:
            os.remove(script_path)
        except Exception:
            pass

    # 简单列出根目录
    try:
        print("\n文件系统内容 (根目录):")
        result = subprocess.run([debugfs_bin, '-R', 'ls -l /', image_path],
                              capture_output=True, text=True, check=True)
        print(result.stdout)
    except Exception:
        pass

    return True

def main():
    parser = argparse.ArgumentParser(description='LiteOS 文件系统管理工具 (ext2)')
    parser.add_argument('action', choices=['create'], help='创建最小启动镜像')
    parser.add_argument('--file', '-f', default='fs.img',
                       help='文件系统映像文件名 (默认: fs.img)')
    parser.add_argument('--size', '-s', type=int, default=128,
                       help='文件系统大小(MB) (默认: 128)')

    args = parser.parse_args()

    print(f"创建LiteOS文件系统(ext2): {args.file} ({args.size}MB)")
    if create_ext2_filesystem(args.file, args.size):
        print("\n🎉 文件系统创建成功!")
    else:
        print("\n❌ 文件系统创建失败!")
        sys.exit(1)

if __name__ == "__main__":
    main()
