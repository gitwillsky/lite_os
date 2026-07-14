#!/usr/bin/env python3
import subprocess
import os
import tempfile
import argparse
import sys

from scripts.ext2_image import find_debugfs, find_mke2fs

BOOT_DIRECTORIES = ("/bin", "/dev", "/proc", "/sys")

def create_ext2_filesystem(filename, init_elf, size_mb=128):
    """创建带标准 JBD2 journal inode 的 4K ext2 revision 1 文件系统。"""

    if not os.path.isfile(init_elf):
        print(f"✗ 未找到用户程序 ELF: {init_elf}")
        return False
    print(f"创建 {size_mb}MB 的ext2(4K) 文件系统映像: {filename}")

    # 1. 创建空的映像文件
    with open(filename, 'wb') as f:
        f.seek(size_mb * 1024 * 1024 - 1)
        f.write(b'\0')

    # 2. 格式化为 ext2（块大小 4096，卷标 LITEOS）
    try:
        mke2fs = find_mke2fs()
    except RuntimeError:
        print("✗ 未找到 mke2fs（e2fsprogs）。请安装: brew install e2fsprogs 或 apt install e2fsprogs")
        return False

    try:
        subprocess.run([str(mke2fs), '-t', 'ext2', '-b', '4096', '-I', '256',
                        '-O', '^ext_attr,^resize_inode,^dir_index,filetype,sparse_super,large_file,has_journal',
                        '-J', 'size=4',
                        '-L', 'LITEOS', filename],
                       check=True, capture_output=True)
        print("✓ ext2 文件系统创建成功 (4K block, 4 MiB JBD2 journal)")
    except subprocess.CalledProcessError as e:
        print(f"✗ mke2fs 失败: {e}\n{e.stderr.decode(errors='ignore')}")
        return False

    # 3. 使用 debugfs 写入文件（macOS 无法直接挂载 ext2）
    try:
        debugfs = find_debugfs()
    except RuntimeError:
        print("✗ 未找到 debugfs（e2fsprogs）。请安装: brew install e2fsprogs 或 apt install e2fsprogs")
        return False

    return copy_files_to_ext2(filename, str(debugfs), init_elf)

def collect_binaries(init_elf):
    """底层 ext2 primitive 只安装调用方显式指定的 init ELF。"""

    print("允许写入镜像的用户程序: ['init']")
    return [(init_elf, "/bin/init")]

def copy_files_to_ext2(image_path, debugfs_bin, init_elf):
    """通过 debugfs 将文件写入 ext2 镜像。"""
    bin_entries = collect_binaries(init_elf)

    # 构建 debugfs 命令脚本
    # boot layout 只声明 kernel composition root 将消费的 mountpoint；设备节点由运行时 device filesystem 提供。
    commands = [f"mkdir {directory}" for directory in BOOT_DIRECTORIES]
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
    parser.add_argument('--init', required=True,
                       help='写入 /bin/init 的静态 ELF（默认 rootfs 由 make build 传入 BusyBox）')

    args = parser.parse_args()

    print(f"创建LiteOS文件系统(ext2): {args.file} ({args.size}MB)")
    if create_ext2_filesystem(args.file, args.init, args.size):
        print("\n🎉 文件系统创建成功!")
    else:
        print("\n❌ 文件系统创建失败!")
        sys.exit(1)

if __name__ == "__main__":
    main()
