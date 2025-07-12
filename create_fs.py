#!/usr/bin/env python3
import struct
import os
import glob

def create_simple_fat32(filename, size_mb=128):
    """创建一个简单的FAT32文件系统映像"""

    # 查找用户程序二进制文件
    user_bins = glob.glob("target/riscv64gc-unknown-none-elf/release/*.bin")
    print(f"找到用户程序: {[os.path.basename(f) for f in user_bins]}")

    # 计算参数
    bytes_per_sector = 512
    sectors_per_cluster = 8
    reserved_sectors = 32
    num_fats = 2
    total_sectors = size_mb * 1024 * 1024 // bytes_per_sector

    # 计算FAT大小
    # 每个FAT条目4字节，每个扇区512字节，所以每个扇区128个FAT条目
    fat_entries_per_sector = bytes_per_sector // 4
    total_clusters = (total_sectors - reserved_sectors) // sectors_per_cluster
    sectors_per_fat = (total_clusters + fat_entries_per_sector - 1) // fat_entries_per_sector

    # 计算数据区起始位置
    data_start = reserved_sectors + num_fats * sectors_per_fat

    print(f"创建FAT32文件系统:")
    print(f"  总扇区数: {total_sectors}")
    print(f"  每FAT扇区数: {sectors_per_fat}")
    print(f"  数据区起始: {data_start}")
    print(f"  根目录簇: 2")

    # 创建文件
    with open(filename, 'wb') as f:
        # 写入引导扇区
        boot_sector = bytearray(bytes_per_sector)

        # 跳转指令
        boot_sector[0:3] = b'\xEB\x58\x90'

        # OEM名称
        boot_sector[3:11] = b'MSWIN4.1'

        # BPB (BIOS Parameter Block)
        struct.pack_into('<H', boot_sector, 11, bytes_per_sector)     # 每扇区字节数
        struct.pack_into('<B', boot_sector, 13, sectors_per_cluster)  # 每簇扇区数
        struct.pack_into('<H', boot_sector, 14, reserved_sectors)     # 保留扇区数
        struct.pack_into('<B', boot_sector, 16, num_fats)             # FAT数量
        struct.pack_into('<H', boot_sector, 17, 0)                    # 根目录条目数(FAT32为0)
        struct.pack_into('<H', boot_sector, 19, 0)                    # 总扇区数16位(FAT32为0)
        struct.pack_into('<B', boot_sector, 21, 0xF8)                 # 媒体描述符
        struct.pack_into('<H', boot_sector, 22, 0)                    # 每FAT扇区数16位(FAT32为0)
        struct.pack_into('<H', boot_sector, 24, 63)                   # 每磁道扇区数
        struct.pack_into('<H', boot_sector, 26, 255)                  # 磁头数
        struct.pack_into('<L', boot_sector, 28, 0)                    # 隐藏扇区数
        struct.pack_into('<L', boot_sector, 32, total_sectors)        # 总扇区数32位

        # FAT32特定字段
        struct.pack_into('<L', boot_sector, 36, sectors_per_fat)      # 每FAT扇区数32位
        struct.pack_into('<H', boot_sector, 40, 0)                    # 扩展标志
        struct.pack_into('<H', boot_sector, 42, 0)                    # 文件系统版本
        struct.pack_into('<L', boot_sector, 44, 2)                    # 根目录簇号
        struct.pack_into('<H', boot_sector, 48, 1)                    # 文件系统信息扇区
        struct.pack_into('<H', boot_sector, 50, 6)                    # 备份引导扇区

        # 跳过保留字段 (12字节)
        struct.pack_into('<B', boot_sector, 64, 0x80)                 # 驱动器号
        struct.pack_into('<B', boot_sector, 65, 0)                    # 保留
        struct.pack_into('<B', boot_sector, 66, 0x29)                 # 扩展引导签名
        struct.pack_into('<L', boot_sector, 67, 0x12345678)           # 卷序列号

        # 卷标和文件系统类型
        boot_sector[71:82] = b'LITE OS    '
        boot_sector[82:90] = b'FAT32   '

        # 引导代码区域填充
        for i in range(90, 510):
            boot_sector[i] = 0x00

        # 签名
        struct.pack_into('<H', boot_sector, 510, 0xAA55)

        f.write(boot_sector)

        # 写入文件系统信息扇区
        fsinfo_sector = bytearray(bytes_per_sector)
        struct.pack_into('<L', fsinfo_sector, 0, 0x41615252)    # 前导签名
        struct.pack_into('<L', fsinfo_sector, 484, 0x61417272)  # 结构签名
        struct.pack_into('<L', fsinfo_sector, 488, 0xFFFFFFFF)  # 空闲簇数
        struct.pack_into('<L', fsinfo_sector, 492, 3)           # 下一个空闲簇
        struct.pack_into('<H', fsinfo_sector, 510, 0xAA55)      # 扇区签名
        f.write(fsinfo_sector)

        # 填充到保留区结束
        for i in range(2, reserved_sectors):
            f.write(b'\x00' * bytes_per_sector)

        # 写入FAT表
        for fat_num in range(num_fats):
            # FAT表的第一个扇区
            fat_sector = bytearray(bytes_per_sector)
            # 前三个FAT条目是特殊值
            struct.pack_into('<L', fat_sector, 0, 0x0FFFFFF8)   # FAT[0]
            struct.pack_into('<L', fat_sector, 4, 0x0FFFFFFF)   # FAT[1]
            struct.pack_into('<L', fat_sector, 8, 0x0FFFFFFF)   # FAT[2] (根目录,EOF)
            # 为测试文件和用户程序设置FAT条目
            struct.pack_into('<L', fat_sector, 12, 0x0FFFFFFF)  # FAT[3] (hello.txt,EOF)
            struct.pack_into('<L', fat_sector, 16, 0x0FFFFFFF)  # FAT[4] (test.txt,EOF)

            # 为每个用户程序分配FAT条目
            for i, bin_file in enumerate(user_bins):
                cluster_num = 5 + i  # 从簇5开始
                struct.pack_into('<L', fat_sector, cluster_num * 4, 0x0FFFFFFF)  # EOF标记
            f.write(fat_sector)

            # 其余FAT扇区填充0
            for i in range(1, sectors_per_fat):
                f.write(b'\x00' * bytes_per_sector)

        # 写入数据区
        data_sectors = total_sectors - data_start

        # 根目录簇 (簇2)
        root_dir_cluster = bytearray(sectors_per_cluster * bytes_per_sector)

        # 创建几个测试文件的目录条目
        # hello.txt
        hello_entry = bytearray(32)
        hello_entry[0:8] = b'HELLO   '
        hello_entry[8:11] = b'TXT'
        hello_entry[11] = 0x20  # 文件属性
        hello_entry[26:28] = struct.pack('<H', 3)  # 起始簇号低16位
        hello_entry[20:22] = struct.pack('<H', 0)  # 起始簇号高16位
        hello_entry[28:32] = struct.pack('<L', 30) # 文件大小
        root_dir_cluster[0:32] = hello_entry

        # test.txt
        test_entry = bytearray(32)
        test_entry[0:8] = b'TEST    '
        test_entry[8:11] = b'TXT'
        test_entry[11] = 0x20  # 文件属性
        test_entry[26:28] = struct.pack('<H', 4)  # 起始簇号低16位
        test_entry[20:22] = struct.pack('<H', 0)  # 起始簇号高16位
        test_entry[28:32] = struct.pack('<L', 20) # 文件大小
        root_dir_cluster[32:64] = test_entry

        # 添加用户程序文件条目
        entry_offset = 64
        for i, bin_file in enumerate(user_bins):
            if entry_offset + 32 > len(root_dir_cluster):
                print(f"警告: 根目录空间不足，跳过 {bin_file}")
                break

            file_size = os.path.getsize(bin_file)
            filename = os.path.basename(bin_file).replace('.bin', '').upper()

            app_entry = bytearray(32)
            # 8.3格式文件名
            padded_name = (filename[:8]).ljust(8)
            app_entry[0:8] = padded_name.encode('ascii')
            app_entry[8:11] = b'BIN'
            app_entry[11] = 0x20  # 文件属性
            app_entry[26:28] = struct.pack('<H', 5 + i)  # 起始簇号低16位
            app_entry[20:22] = struct.pack('<H', 0)  # 起始簇号高16位
            app_entry[28:32] = struct.pack('<L', file_size) # 文件大小

            root_dir_cluster[entry_offset:entry_offset+32] = app_entry
            entry_offset += 32

        f.write(root_dir_cluster)

        # 写入hello.txt内容 (簇3)
        hello_content = b'Hello from FAT32 filesystem!\n'
        hello_cluster = bytearray(sectors_per_cluster * bytes_per_sector)
        hello_cluster[0:len(hello_content)] = hello_content
        f.write(hello_cluster)

        # 写入test.txt内容 (簇4)
        test_content = b'This is a test file\n'
        test_cluster = bytearray(sectors_per_cluster * bytes_per_sector)
        test_cluster[0:len(test_content)] = test_content
        f.write(test_cluster)

        # 写入用户程序二进制文件
        clusters_used = 3  # 已使用3个簇(根目录、hello.txt、test.txt)
        for i, bin_file in enumerate(user_bins):
            print(f"写入用户程序: {os.path.basename(bin_file)}")

            with open(bin_file, 'rb') as bin_f:
                bin_content = bin_f.read()

            # 计算需要的簇数
            cluster_size = sectors_per_cluster * bytes_per_sector
            clusters_needed = (len(bin_content) + cluster_size - 1) // cluster_size

            for cluster_idx in range(clusters_needed):
                cluster_data = bytearray(cluster_size)
                start_offset = cluster_idx * cluster_size
                end_offset = min(start_offset + cluster_size, len(bin_content))
                if start_offset < len(bin_content):
                    cluster_data[0:end_offset-start_offset] = bin_content[start_offset:end_offset]
                f.write(cluster_data)
                clusters_used += 1

        # 填充剩余数据区
        remaining_sectors = data_sectors - clusters_used * sectors_per_cluster
        for i in range(remaining_sectors):
            f.write(b'\x00' * bytes_per_sector)

    print(f"成功创建FAT32文件系统映像: {filename}")

if __name__ == "__main__":
    create_simple_fat32("fs.img", 128)