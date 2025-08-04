#!/usr/bin/env python3
import subprocess
import os
import glob
import tempfile
import shutil
import argparse
import sys

def cleanup_apple_double_files(directory):
    """递归删除所有 ._ 开头的 AppleDouble 文件"""
    deleted_count = 0
    try:
        for root, dirs, files in os.walk(directory):
            for file in files:
                if file.startswith('._'):
                    apple_double_path = os.path.join(root, file)
                    try:
                        os.remove(apple_double_path)
                        print(f"✓ 已删除 AppleDouble 文件: {os.path.relpath(apple_double_path, directory)}")
                        deleted_count += 1
                    except Exception as e:
                        print(f"⚠ 删除 {file} 时出错: {e}")

        if deleted_count == 0:
            print("✓ 未发现 AppleDouble 文件")
        else:
            print(f"✓ 总共删除了 {deleted_count} 个 AppleDouble 文件")

    except Exception as e:
        print(f"⚠ 清理 AppleDouble 文件时出错: {e}")

def create_fat32_filesystem(filename, size_mb=128):
    """使用系统工具创建FAT32文件系统并挂载复制文件"""

    print(f"创建 {size_mb}MB 的FAT32文件系统映像: {filename}")

    # 1. 创建空的映像文件
    with open(filename, 'wb') as f:
        f.seek(size_mb * 1024 * 1024 - 1)
        f.write(b'\0')

    # 2. 格式化为FAT32
    try:
        subprocess.run(['mkfs.fat', '-F', '32', '-n', 'LITEOS', filename],
                      check=True, capture_output=True)
        print("✓ FAT32文件系统创建成功")
    except subprocess.CalledProcessError as e:
        print(f"✗ 格式化失败: {e}")
        return False
    except FileNotFoundError:
        print("✗ 未找到 mkfs.fat 命令，请安装 dosfstools")
        print("  macOS: brew install dosfstools")
        print("  Ubuntu: sudo apt install dosfstools")
        return False

    # 3. 挂载文件系统
    mount_point = None
    try:
        mount_point = tempfile.mkdtemp(prefix='liteos_fs_')

        # macOS 使用 hdiutil
        if os.uname().sysname == 'Darwin':
            # 在macOS上挂载FAT32映像，允许写入
            result = subprocess.run(['hdiutil', 'attach', '-mountpoint', mount_point,
                                   '-nobrowse', '-quiet', '-readwrite', filename],
                                  capture_output=True, text=True)
            if result.returncode != 0:
                print(f"✗ 挂载失败: {result.stderr}")
                return False
        else:
            # Linux 使用 mount
            subprocess.run(['sudo', 'mount', '-o', 'loop', filename, mount_point],
                          check=True)

        print(f"✓ 文件系统已挂载到: {mount_point}")

        # 4. 复制文件到文件系统
        copy_files_to_fs(mount_point)

        return True

    except Exception as e:
        print(f"✗ 挂载或复制失败: {e}")
        return False
    finally:
        # 5. 卸载文件系统
        if mount_point:
            try:
                if os.uname().sysname == 'Darwin':
                    subprocess.run(['hdiutil', 'detach', mount_point],
                                 capture_output=True, check=True)
                else:
                    subprocess.run(['sudo', 'umount', mount_point], check=True)
                os.rmdir(mount_point)
                print("✓ 文件系统已卸载")
            except Exception as e:
                print(f"⚠ 卸载警告: {e}")

def copy_files_to_fs(mount_point):
    """复制文件到已挂载的文件系统"""

    # 创建标准Unix目录结构
    bin_dir = os.path.join(mount_point, 'bin')

    # 创建目录
    os.makedirs(bin_dir, exist_ok=True)

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

    # 定义哪些命令应该放在 /bin/ 目录下
    bin_commands = ['ls', 'cat', 'mkdir', 'rm', 'pwd', 'echo', 'shell', 'exit', 'init', 'wasm_runtime', 'top', 'vim', 'kill', 'test']

    if user_elfs:
        print(f"找到用户程序ELF文件: {[os.path.basename(f) for f in user_elfs]}")

        for elf_file in user_elfs:
            basename = os.path.basename(elf_file)

            # 系统命令放在 /bin/ 目录下
            if basename in bin_commands:
                dest_path = os.path.join(bin_dir, basename)
                shutil.copy2(elf_file, dest_path)
                print(f"✓ 复制ELF: {basename} -> /bin/{basename}")
            # 其他程序放在根目录
            else:
                dest_path = os.path.join(mount_point, basename)
                shutil.copy2(elf_file, dest_path)
                print(f"✓ 复制ELF: {basename} -> {basename}")
    else:
        print("⚠ 未找到用户程序ELF文件")

    # 查找并复制WASM测试程序
    wasm_files = []
    wasm_dir = "wasm_programs/wasm_output"
    if os.path.exists(wasm_dir):
        for wasm_file in glob.glob(os.path.join(wasm_dir, "*.wasm")):
            if os.path.isfile(wasm_file):
                wasm_files.append(wasm_file)

    if wasm_files:
        print(f"找到WASM程序: {[os.path.basename(f) for f in wasm_files]}")

        for wasm_file in wasm_files:
            dest_name = os.path.basename(wasm_file)
            dest_path = os.path.join(mount_point, dest_name)
            shutil.copy2(wasm_file, dest_path)
            print(f"✓ 复制WASM: {os.path.basename(wasm_file)} -> {dest_name}")
    else:
        print("⚠ 未找到WASM程序文件")
        print("  请先运行: cd wasm_programs && ./build.sh")

    # 查找并复制其他WASM文件（简单的手工生成的）
    simple_wasm_files = glob.glob("wasm_programs/*.wasm")
    if simple_wasm_files:
        print(f"找到简单WASM程序: {[os.path.basename(f) for f in simple_wasm_files]}")

        for wasm_file in simple_wasm_files:
            dest_name = os.path.basename(wasm_file)
            dest_path = os.path.join(mount_point, dest_name)
            shutil.copy2(wasm_file, dest_path)
            print(f"✓ 复制简单WASM: {os.path.basename(wasm_file)} -> {dest_name}")

    # 清理 macOS 自动生成的 AppleDouble 文件
    cleanup_apple_double_files(mount_point)

    # 显示文件系统内容
    print("\n文件系统内容:")
    try:
        for item in os.listdir(mount_point):
            if not item.startswith('.'):  # 跳过隐藏文件
                item_path = os.path.join(mount_point, item)
                size = os.path.getsize(item_path) if os.path.isfile(item_path) else 0
                print(f"  {item} ({size} bytes)")
    except Exception as e:
        print(f"⚠ 无法列出目录内容: {e}")

def list_fs_contents(filename):
    """列出文件系统内容"""
    mount_point = None
    try:
        mount_point = tempfile.mkdtemp(prefix='liteos_fs_list_')

        # 挂载文件系统
        if os.uname().sysname == 'Darwin':
            result = subprocess.run(['hdiutil', 'attach', '-mountpoint', mount_point,
                                   '-nobrowse', '-quiet', '-readwrite', filename],
                                  capture_output=True, text=True)
            if result.returncode != 0:
                print(f"✗ 挂载失败: {result.stderr}")
                return False
        else:
            subprocess.run(['sudo', 'mount', '-o', 'loop', filename, mount_point],
                          check=True)

        print(f"文件系统内容 ({filename}):")
        print("=" * 50)

        total_size = 0
        file_count = 0

        for item in sorted(os.listdir(mount_point)):
            if not item.startswith('.'):  # 跳过隐藏文件
                item_path = os.path.join(mount_point, item)
                if os.path.isfile(item_path):
                    size = os.path.getsize(item_path)
                    total_size += size
                    file_count += 1

                    # 判断文件类型
                    if item.endswith('.wasm'):
                        file_type = "WASM"
                    elif '.' not in item:
                        file_type = "ELF"
                    else:
                        file_type = "TEXT"

                    print(f"  {item:<20} {size:>8} bytes  [{file_type}]")
                else:
                    print(f"  {item:<20} {'<DIR>':>8}       [DIR]")

        print("=" * 50)
        print(f"总计: {file_count} 个文件, {total_size} 字节")

        return True

    except Exception as e:
        print(f"✗ 操作失败: {e}")
        return False
    finally:
        if mount_point:
            try:
                if os.uname().sysname == 'Darwin':
                    subprocess.run(['hdiutil', 'detach', mount_point],
                                 capture_output=True, check=True)
                else:
                    subprocess.run(['sudo', 'umount', mount_point], check=True)
                os.rmdir(mount_point)
            except Exception as e:
                print(f"⚠ 卸载警告: {e}")

def add_files_to_fs(filename, files):
    """向现有文件系统添加文件"""
    mount_point = None
    try:
        mount_point = tempfile.mkdtemp(prefix='liteos_fs_add_')

        # 挂载文件系统
        if os.uname().sysname == 'Darwin':
            result = subprocess.run(['hdiutil', 'attach', '-mountpoint', mount_point,
                                   '-nobrowse', '-quiet', '-readwrite', filename],
                                  capture_output=True, text=True)
            if result.returncode != 0:
                print(f"✗ 挂载失败: {result.stderr}")
                return False
        else:
            subprocess.run(['sudo', 'mount', '-o', 'loop', filename, mount_point],
                          check=True)

        print(f"向文件系统添加文件:")

        for src_file in files:
            if os.path.exists(src_file):
                dest_name = os.path.basename(src_file)
                dest_path = os.path.join(mount_point, dest_name)
                shutil.copy2(src_file, dest_path)
                size = os.path.getsize(src_file)
                print(f"✓ 添加: {src_file} -> {dest_name} ({size} bytes)")
            else:
                print(f"✗ 文件不存在: {src_file}")

        # 清理添加文件后可能产生的 AppleDouble 文件
        cleanup_apple_double_files(mount_point)

        return True

    except Exception as e:
        print(f"✗ 操作失败: {e}")
        return False
    finally:
        if mount_point:
            try:
                if os.uname().sysname == 'Darwin':
                    subprocess.run(['hdiutil', 'detach', mount_point],
                                 capture_output=True, check=True)
                else:
                    subprocess.run(['sudo', 'umount', mount_point], check=True)
                os.rmdir(mount_point)
            except Exception as e:
                print(f"⚠ 卸载警告: {e}")

def main():
    parser = argparse.ArgumentParser(description='LiteOS 文件系统管理工具')
    parser.add_argument('action', choices=['create', 'list', 'add'],
                       help='操作类型: create(创建), list(列出内容), add(添加文件)')
    parser.add_argument('--file', '-f', default='fs.img',
                       help='文件系统映像文件名 (默认: fs.img)')
    parser.add_argument('--size', '-s', type=int, default=128,
                       help='文件系统大小(MB) (默认: 128)')
    parser.add_argument('--add-files', nargs='+',
                       help='要添加的文件列表')

    args = parser.parse_args()

    if args.action == 'create':
        print(f"创建LiteOS文件系统: {args.file} ({args.size}MB)")
        if create_fat32_filesystem(args.file, args.size):
            print("\n🎉 文件系统创建成功!")
            list_fs_contents(args.file)
        else:
            print("\n❌ 文件系统创建失败!")
            sys.exit(1)

    elif args.action == 'list':
        if not os.path.exists(args.file):
            print(f"✗ 文件系统映像不存在: {args.file}")
            sys.exit(1)
        list_fs_contents(args.file)

    elif args.action == 'add':
        if not os.path.exists(args.file):
            print(f"✗ 文件系统映像不存在: {args.file}")
            sys.exit(1)
        if not args.add_files:
            print("✗ 请使用 --add-files 指定要添加的文件")
            sys.exit(1)

        if add_files_to_fs(args.file, args.add_files):
            print("\n✓ 文件添加完成!")
            list_fs_contents(args.file)
        else:
            print("\n✗ 文件添加失败!")
            sys.exit(1)

if __name__ == "__main__":
    main()