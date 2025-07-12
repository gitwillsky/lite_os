#!/usr/bin/env python3
import subprocess
import os
import glob
import tempfile
import shutil

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
            # 在macOS上挂载FAT32映像
            result = subprocess.run(['hdiutil', 'attach', '-mountpoint', mount_point,
                                   '-nobrowse', '-quiet', filename],
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

    # 创建测试文件
    with open(os.path.join(mount_point, 'hello.txt'), 'w') as f:
        f.write('Hello from FAT32 filesystem!\n')

    with open(os.path.join(mount_point, 'test.txt'), 'w') as f:
        f.write('This is a test file\n')

    print("✓ 测试文件已创建")

    # 查找并复制.bin文件（保持兼容性）
    user_bins = glob.glob("target/riscv64gc-unknown-none-elf/release/*.bin")
    if user_bins:
        print(f"找到用户程序BIN文件: {[os.path.basename(f) for f in user_bins]}")

        for bin_file in user_bins:
            dest_name = os.path.basename(bin_file).upper()
            dest_path = os.path.join(mount_point, dest_name)
            shutil.copy2(bin_file, dest_path)
            print(f"✓ 复制: {os.path.basename(bin_file)} -> {dest_name}")

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

if __name__ == "__main__":
    if create_fat32_filesystem("fs.img", 128):
        print("\n🎉 文件系统创建成功!")
    else:
        print("\n❌ 文件系统创建失败!")