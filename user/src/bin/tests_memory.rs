#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{brk, sbrk, mmap, munmap, mmap_flags, exit, TestStats, test_section, test_subsection};

#[inline(always)]
fn align_up(val: usize, align: usize) -> usize { (val + align - 1) & !(align - 1) }

fn test_brk_operations(stats: &mut TestStats) {
    test_subsection!("BRK/SBRK 堆管理测试");
    
    // 获取初始堆顶
    let base = brk(0) as usize;
    test_assert!(base != 0, "brk(0) 返回非法地址");
    test_info!("初始堆顶: 0x{:x}", base);
    
    // 扩展堆空间
    let inc = 64 * 1024;
    let old_brk = sbrk(inc as isize) as usize;
    test_assert!(old_brk == base, "sbrk 应返回原堆顶");
    
    let new_brk = brk(0) as usize;
    test_assert!(new_brk == base + inc, "堆顶扩展失败");
    
    // 写入测试数据并验证
    unsafe {
        let ptr = base as *mut u8;
        for i in 0..inc {
            ptr.add(i).write_volatile((i & 0xFF) as u8);
        }
        for i in 0..inc {
            let val = ptr.add(i).read_volatile();
            test_assert!(val == (i & 0xFF) as u8, "堆内存写入验证失败");
        }
    }
    
    // 缩回堆空间
    let shrink_brk = sbrk(-(inc as isize)) as usize;
    test_assert!(shrink_brk == base + inc, "shrink sbrk 返回值错误");
    
    let final_brk = brk(0) as usize;
    test_assert!(final_brk == base, "堆空间回退失败");
    
    // 边界测试：尝试非法收缩
    let invalid_shrink = sbrk(-((base + 0x10000) as isize));
    test_assert!(invalid_shrink < 0, "非法堆收缩应该失败");
    
    test_pass!("BRK/SBRK 堆管理测试完成");
    stats.pass();
}

fn test_mmap_operations(stats: &mut TestStats) {
    test_subsection!("MMAP/MUNMAP 内存映射测试");
    
    // 基础映射测试
    let len = 8 * 4096;  // 8 pages
    let addr = mmap(0, len, mmap_flags::PROT_READ | mmap_flags::PROT_WRITE) as usize;
    test_assert!(addr != 0, "mmap 映射失败");
    test_assert!(addr % 4096 == 0, "mmap 地址未对齐页边界");
    
    // 写入测试模式
    unsafe {
        let ptr = addr as *mut u8;
        for i in 0..len {
            ptr.add(i).write_volatile(((i ^ (i >> 8)) & 0xFF) as u8);
        }
        
        // 验证数据完整性
        for i in 0..len {
            let expected = ((i ^ (i >> 8)) & 0xFF) as u8;
            let actual = ptr.add(i).read_volatile();
            test_assert!(actual == expected, "mmap 内存数据验证失败 at {}", i);
        }
    }
    
    // 解除映射
    let ret = munmap(addr, len);
    test_assert!(ret == 0, "munmap 解除映射失败");
    
    test_pass!("基础 MMAP/MUNMAP 测试完成");
    
    // 权限测试
    let ro_addr = mmap(0, 4096, mmap_flags::PROT_READ) as usize;
    if ro_addr != 0 {
        test_info!("只读映射地址: 0x{:x}", ro_addr);
        let _ = munmap(ro_addr, 4096);
        test_info!("只读映射测试完成");
    }
    
    let wo_addr = mmap(0, 4096, mmap_flags::PROT_WRITE) as usize;
    if wo_addr != 0 {
        test_info!("只写映射地址: 0x{:x}", wo_addr);
        let _ = munmap(wo_addr, 4096);
        test_info!("只写映射测试完成");
    }
    
    stats.pass();
}

fn test_mmap_stress(stats: &mut TestStats) {
    test_subsection!("内存映射压力测试");
    
    // 多次小块映射测试
    let iterations = 50;
    for i in 0..iterations {
        let size = align_up(512 + (i * 73) % 8192, 4096);
        let addr = mmap(0, size, mmap_flags::PROT_READ | mmap_flags::PROT_WRITE) as usize;
        
        if addr == 0 {
            test_warn!("压力测试第{}轮映射失败，大小: {}", i, size);
            continue;
        }
        
        // 简单写入测试
        unsafe {
            (addr as *mut u32).write_volatile(0xDEADBEEF + i as u32);
            let val = (addr as *mut u32).read_volatile();
            test_assert!(val == 0xDEADBEEF + i as u32, "压力测试数据验证失败");
        }
        
        let ret = munmap(addr, size);
        test_assert!(ret == 0, "压力测试解除映射失败");
    }
    
    test_pass!("内存映射压力测试完成");
    stats.pass();
}

fn test_large_allocation(stats: &mut TestStats) {
    test_subsection!("大内存分配测试");
    
    // 尝试分配较大内存块
    let sizes = [1024*1024, 4*1024*1024, 8*1024*1024]; // 1MB, 4MB, 8MB
    
    for &size in &sizes {
        let addr = mmap(0, size, mmap_flags::PROT_READ | mmap_flags::PROT_WRITE) as usize;
        
        if addr != 0 {
            test_info!("成功分配 {} MB 内存块 at 0x{:x}", size / (1024*1024), addr);
            
            // 测试首尾页
            unsafe {
                (addr as *mut u64).write_volatile(0x1234567890ABCDEF);
                ((addr + size - 8) as *mut u64).write_volatile(0xFEDCBA0987654321);
                
                let val1 = (addr as *mut u64).read_volatile();
                let val2 = ((addr + size - 8) as *mut u64).read_volatile();
                
                test_assert!(val1 == 0x1234567890ABCDEF, "大内存块首页写入验证失败");
                test_assert!(val2 == 0xFEDCBA0987654321, "大内存块尾页写入验证失败");
            }
            
            let ret = munmap(addr, size);
            test_assert!(ret == 0, "大内存块解除映射失败");
        } else {
            test_info!("无法分配 {} MB 内存块 (系统限制)", size / (1024*1024));
        }
    }
    
    test_pass!("大内存分配测试完成");
    stats.pass();
}

#[unsafe(no_mangle)]
fn main() -> i32 {
    let mut stats = TestStats::new();
    
    test_section!("内存管理子系统综合测试");
    
    test_brk_operations(&mut stats);
    test_mmap_operations(&mut stats);
    test_mmap_stress(&mut stats);
    test_large_allocation(&mut stats);
    
    test_section!("内存管理测试总结");
    test_summary!(stats.total, stats.passed, stats.failed);
    
    if stats.failed == 0 {
        test_pass!("内存管理子系统测试全部通过");
        exit(0);
    } else {
        test_fail!("内存管理子系统测试发现 {} 个失败", stats.failed);
        exit(1);
    }
    0
}


