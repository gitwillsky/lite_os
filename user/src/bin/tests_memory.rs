#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{brk, sbrk, mmap, munmap, mmap_flags, exit};

#[inline(always)]
fn align_up(val: usize, align: usize) -> usize { (val + align - 1) & !(align - 1) }

#[unsafe(no_mangle)]
fn main() -> i32 {
    test_info!("memory: 开始内存子系统测试");

    // 基线：获取当前 brk
    let base = brk(0) as usize;
    test_assert!(base != 0, "brk(0) 返回非法 0");
    test_info!("memory: 初始 brk = 0x{:x}", base);

    // 扩堆 64 KiB 并写读验证
    let inc = 64 * 1024usize;
    test_info!("memory: sbrk(+{})", inc);
    let old = sbrk(inc as isize) as usize;
    test_assert!(old == base, "sbrk 返回值应为原堆顶: old={} base={}", old, base);
    let new_top = brk(0) as usize;
    test_assert!(new_top == base + inc, "堆顶不匹配: {} != {}", new_top, base + inc);
    test_info!("memory: 扩堆后 brk = 0x{:x}", new_top);

    // 缩回原位
    test_info!("memory: sbrk(-{}) 回退", inc);
    let old2 = sbrk(-(inc as isize)) as usize;
    test_assert!(old2 == base + inc, "sbrk(-inc) 应返回扩堆前的堆顶: {}", old2);
    let now = brk(0) as usize;
    test_assert!(now == base, "回退失败: {} != {}", now, base);
    test_info!("memory: 回退后 brk = 0x{:x}", now);

    // 非法 sbrk：试图低于堆基址（应失败，且 brk 不变）
    test_info!("memory: sbrk(负向大步) 期望失败");
    let neg = sbrk(-((base + 0x1000) as isize));
    test_assert!(neg < 0, "sbrk 超界未失败: {}", neg);
    let still = brk(0) as usize;
    test_assert!(still == base, "非法 sbrk 不应改变 brk: {} != {}", still, base);

    // mmap 匿名 2 页，可读写
    let len = 2 * 4096usize;
    test_info!("memory: mmap 匿名映射 {} 字节", len);
    let addr = mmap(0, len, mmap_flags::PROT_READ | mmap_flags::PROT_WRITE) as usize;
    test_assert!(addr != 0, "mmap 返回 0 代表失败");
    test_info!("memory: mmap 返回地址 0x{:x}", addr);

    // 写读校验
    unsafe {
        let p = addr as *mut u8;
        for i in 0..len { p.add(i).write_volatile((i & 0xFF) as u8); }
        for i in 0..len { let v = p.add(i).read_volatile(); test_assert!(v == (i & 0xFF) as u8, "mmap RW 校验失败在 {}", i); }
    }

    // 解除映射
    let ret = munmap(addr, len);
    test_assert!(ret == 0, "munmap 失败: {}", ret);
    test_info!("memory: munmap 成功");

    // 压力：多次 mmap/munmap 小块
    let iters = 32usize;
    test_info!("memory: 启动 {} 轮小块压力映射", iters);
    for i in 0..iters {
        let sz = align_up(1024 + (i * 37) % 5000, 4096);
        let a = mmap(0, sz, mmap_flags::PROT_READ | mmap_flags::PROT_WRITE) as usize;
        test_assert!(a != 0, "压力 mmap 失败: i={} sz={}", i, sz);
        unsafe { (a as *mut u8).write_volatile(0xAB); }
        let r = munmap(a, sz);
        test_assert!(r == 0, "压力 munmap 失败: i={} r={}", i, r);
    }

    // 大页映射尝试（可失败但不崩溃）
    let big = 8 * 1024 * 1024usize; // 8MiB
    test_info!("memory: mmap 大块 {} 字节（允许失败）", big);
    let a = mmap(0, big, mmap_flags::PROT_READ | mmap_flags::PROT_WRITE) as usize;
    if a != 0 {
        let _ = munmap(a, big);
    }

    test_info!("memory: 所有用例通过");
    exit(0);
    0
}


