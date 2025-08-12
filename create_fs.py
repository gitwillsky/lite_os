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
    """收集需要放入镜像的文件列表，返回 (bin_entries, root_entries)。"""

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
    bin_commands = ['ls', 'cat', 'mkdir', 'rm', 'pwd', 'echo', 'shell', 'exit', 'init', 'wasm_runtime', 'top', 'vim', 'kill', 'litewm']
    bin_entries = []   # (src, '/bin/name')
    root_entries = []  # (src, '/name')
    test_entries = []  # (src, '/tests/name')
    if user_elfs:
        print(f"找到用户程序ELF文件: {[os.path.basename(f) for f in user_elfs]}")
        for elf_file in user_elfs:
            basename = os.path.basename(elf_file)
            if basename.startswith('tests_'):
                test_entries.append((elf_file, f"/tests/{basename[6:]}"))
            elif basename in bin_commands:
                bin_entries.append((elf_file, f"/bin/{basename}"))
            else:
                root_entries.append((elf_file, f"/{basename}"))
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
            root_entries.append((wasm_file, f"/{dest_name}"))
    else:
        print("⚠ 未找到WASM程序文件")
        print("  请先运行: cd wasm_programs && ./build.sh")

    # 查找并复制其他WASM文件（简单的手工生成的）
    simple_wasm_files = glob.glob("wasm_programs/*.wasm")
    if simple_wasm_files:
        print(f"找到简单WASM程序: {[os.path.basename(f) for f in simple_wasm_files]}")
        for wasm_file in simple_wasm_files:
            dest_name = os.path.basename(wasm_file)
            root_entries.append((wasm_file, f"/{dest_name}"))

    # 额外：收集字体文件，放入 /fonts/
    font_sources = []
    # 项目根目录下的 .ttf/.otf
    for pattern in ("*.ttf", "*.otf"):
        for font in glob.glob(pattern):
            if os.path.isfile(font):
                font_sources.append((font, f"/fonts/{os.path.basename(font)}"))
    # fonts/ 子目录下的 .ttf/.otf
    if os.path.isdir("fonts"):
        for pattern in ("*.ttf", "*.otf"):
            for font in glob.glob(os.path.join("fonts", pattern)):
                if os.path.isfile(font):
                    font_sources.append((font, f"/fonts/{os.path.basename(font)}"))

    return bin_entries, root_entries + font_sources, test_entries

def copy_files_to_ext2(image_path, debugfs_bin):
    """通过 debugfs 将文件写入 ext2 镜像。"""
    bin_entries, root_entries, test_entries = collect_binaries()

    # 构建 debugfs 命令脚本
    commands = []
    commands.append("mkdir /bin")
    commands.append("mkdir /fonts")
    commands.append("mkdir /tests")
    for src, dst in bin_entries + root_entries + test_entries:
        # debugfs 的 write 语法: write <native_file> <dest_file>
        commands.append(f"write {src} {dst}")

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
                    subprocess.run(['hdiutil', 'detach', mount_point, '-quiet'],
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
                    subprocess.run(['hdiutil', 'detach', mount_point, '-quiet'],
                                 capture_output=True, check=True)
                else:
                    subprocess.run(['sudo', 'umount', mount_point], check=True)
                os.rmdir(mount_point)
            except Exception as e:
                print(f"⚠ 卸载警告: {e}")

def main():
    parser = argparse.ArgumentParser(description='LiteOS 文件系统管理工具 (ext2)')
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
        print(f"创建LiteOS文件系统(ext2): {args.file} ({args.size}MB)")
        if create_ext2_filesystem(args.file, args.size):
            print("\n🎉 文件系统创建成功!")
            # 使用 debugfs 已输出内容
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