#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
extern crate alloc;
use alloc::boxed::Box;
use alloc::vec::Vec;

use user_lib::{thread_create, thread_join, thread_exit, getpid, gettid, 
              nanosleep, TimeSpec, exit, TestStats, test_section, test_subsection};

static mut SHARED_COUNTER: usize = 0;
static mut SHARED_DATA: [u8; 1024] = [0; 1024];

// 简单线程入口函数
#[unsafe(no_mangle)]
extern "C" fn simple_thread_entry(arg: usize) -> ! {
    let code = (arg as i32) & 0xFF;
    thread_exit(code)
}

// 共享数据测试线程
#[unsafe(no_mangle)]
extern "C" fn shared_data_thread(arg: usize) -> ! {
    unsafe {
        for i in 0..100 {
            SHARED_COUNTER += 1;
            SHARED_DATA[i % 1024] = (arg as u8).wrapping_add(i as u8);
        }
    }
    thread_exit(0)
}

// 计算密集型线程
#[unsafe(no_mangle)]
extern "C" fn compute_thread(arg: usize) -> ! {
    let mut sum = 0usize;
    for i in 0..10000 {
        sum = sum.wrapping_add(i).wrapping_mul(arg);
    }
    thread_exit((sum % 256) as i32)
}

// 睡眠测试线程
#[unsafe(no_mangle)]
extern "C" fn sleep_thread(arg: usize) -> ! {
    let sleep_time = TimeSpec { 
        tv_sec: 0, 
        tv_nsec: (arg as u64) * 50_000_000 // 50ms * arg
    };
    nanosleep(&sleep_time, core::ptr::null_mut());
    thread_exit(arg as i32)
}

// 递归计算斐波那契数列
#[unsafe(no_mangle)]
extern "C" fn fibonacci_thread(arg: usize) -> ! {
    fn fibonacci(n: usize) -> usize {
        if n <= 1 {
            n
        } else {
            fibonacci(n - 1) + fibonacci(n - 2)
        }
    }
    
    let result = fibonacci(arg % 20); // 限制在合理范围内
    thread_exit((result % 256) as i32)
}

fn create_stack(size: usize) -> usize {
    let stack: &'static mut [u8] = Box::leak(alloc::vec![0u8; size].into_boxed_slice());
    stack.as_mut_ptr() as usize + size
}

fn test_basic_thread_operations(stats: &mut TestStats) {
    test_subsection!("基础线程操作测试");
    
    let mut code0: i32 = -1;
    let mut code1: i32 = -1;

    let sp0 = create_stack(4096);
    let sp1 = create_stack(4096);

    let tid0 = thread_create(simple_thread_entry as usize, sp0, 7);
    test_assert!(tid0 > 0, "thread_create 失败: {}", tid0);
    
    let tid1 = thread_create(simple_thread_entry as usize, sp1, 9);
    test_assert!(tid1 > 0, "thread_create 失败: {}", tid1);
    test_assert!(tid0 != tid1, "线程ID应该不同: {} vs {}", tid0, tid1);

    let r0 = thread_join(tid0 as usize, &mut code0);
    let r1 = thread_join(tid1 as usize, &mut code1);
    test_assert!(r0 == 0 && r1 == 0, "thread_join 失败: {}, {}", r0, r1);
    test_assert!(code0 == 7 && code1 == 9, "线程退出码异常: {} {}", code0, code1);

    test_pass!("基础线程操作测试通过");
    stats.pass();
}

fn test_multiple_threads(stats: &mut TestStats) {
    test_subsection!("多线程创建和等待测试");
    
    let thread_count = 5;
    let mut tids = Vec::new();
    let mut exit_codes = Vec::new();
    
    // 创建多个线程
    for i in 0..thread_count {
        let sp = create_stack(4096);
        let arg = (i + 1) * 10;
        let tid = thread_create(simple_thread_entry as usize, sp, arg);
        test_assert!(tid > 0, "创建线程 {} 失败: {}", i, tid);
        tids.push(tid);
        exit_codes.push(-1);
        test_info!("创建线程 {} TID: {} 参数: {}", i, tid, arg);
    }
    
    // 等待所有线程完成
    for (i, &tid) in tids.iter().enumerate() {
        let r = thread_join(tid as usize, &mut exit_codes[i]);
        test_assert!(r == 0, "等待线程 {} 失败: {}", i, r);
        
        let expected = ((i + 1) * 10) as i32;
        test_assert!(exit_codes[i] == expected, 
                    "线程 {} 退出码错误: {} != {}", i, exit_codes[i], expected);
        test_info!("线程 {} 正常退出，状态码: {}", i, exit_codes[i]);
    }

    test_pass!("多线程创建和等待测试通过");
    stats.pass();
}

fn test_shared_data_access(stats: &mut TestStats) {
    test_subsection!("共享数据访问测试");
    
    unsafe {
        SHARED_COUNTER = 0;
        for i in 0..1024 {
            SHARED_DATA[i] = 0;
        }
    }
    
    let thread_count = 3;
    let mut tids = Vec::new();
    
    // 创建多个访问共享数据的线程
    for i in 0..thread_count {
        let sp = create_stack(8192);
        let tid = thread_create(shared_data_thread as usize, sp, i + 1);
        test_assert!(tid > 0, "创建共享数据线程 {} 失败", i);
        tids.push(tid);
        test_info!("创建共享数据访问线程 {} TID: {}", i, tid);
    }
    
    // 等待所有线程完成
    for (i, &tid) in tids.iter().enumerate() {
        let mut code = -1;
        let r = thread_join(tid as usize, &mut code);
        test_assert!(r == 0, "等待共享数据线程 {} 失败", i);
        test_assert!(code == 0, "共享数据线程 {} 退出码异常: {}", i, code);
    }
    
    // 验证共享数据
    unsafe {
        let counter_value = core::ptr::addr_of!(SHARED_COUNTER).read();
        test_info!("共享计数器最终值: {}", counter_value);
        test_assert!(counter_value == thread_count * 100, 
                    "共享计数器值不正确: {} != {}", counter_value, thread_count * 100);
        
        // 检查共享数据的部分内容
        let mut data_ok = true;
        for i in 0..100 {
            if core::ptr::addr_of!(SHARED_DATA[i]).read() == 0 {
                data_ok = false;
                break;
            }
        }
        test_assert!(data_ok, "共享数据写入验证失败");
    }

    test_pass!("共享数据访问测试通过");
    stats.pass();
}

fn test_compute_intensive_threads(stats: &mut TestStats) {
    test_subsection!("计算密集型线程测试");
    
    let thread_count = 4;
    let mut tids = Vec::new();
    let args = [1, 2, 3, 5];
    
    // 创建计算密集型线程
    for i in 0..thread_count {
        let sp = create_stack(8192);
        let tid = thread_create(compute_thread as usize, sp, args[i]);
        test_assert!(tid > 0, "创建计算线程 {} 失败", i);
        tids.push(tid);
        test_info!("创建计算线程 {} TID: {} 参数: {}", i, tid, args[i]);
    }
    
    // 等待并验证结果
    for (i, &tid) in tids.iter().enumerate() {
        let mut code = -1;
        let r = thread_join(tid as usize, &mut code);
        test_assert!(r == 0, "等待计算线程 {} 失败", i);
        test_assert!(code >= 0, "计算线程 {} 退出码异常: {}", i, code);
        test_info!("计算线程 {} 完成，结果: {}", i, code);
    }

    test_pass!("计算密集型线程测试通过");
    stats.pass();
}

fn test_sleep_timing_threads(stats: &mut TestStats) {
    test_subsection!("线程睡眠时序测试");
    
    let thread_count = 3;
    let mut tids = Vec::new();
    let sleep_args = [1, 2, 3]; // 不同的睡眠时间倍数
    
    test_info!("创建不同睡眠时间的线程");
    for i in 0..thread_count {
        let sp = create_stack(4096);
        let tid = thread_create(sleep_thread as usize, sp, sleep_args[i]);
        test_assert!(tid > 0, "创建睡眠线程 {} 失败", i);
        tids.push(tid);
        test_info!("创建睡眠线程 {} TID: {} 睡眠: {}*50ms", i, tid, sleep_args[i]);
    }
    
    // 按顺序等待线程（理论上应该按睡眠时间顺序完成）
    for (i, &tid) in tids.iter().enumerate() {
        let mut code = -1;
        let r = thread_join(tid as usize, &mut code);
        test_assert!(r == 0, "等待睡眠线程 {} 失败", i);
        test_assert!(code == sleep_args[i] as i32, "睡眠线程 {} 退出码错误: {} != {}", 
                    i, code, sleep_args[i]);
        test_info!("睡眠线程 {} 唤醒完成", i);
    }

    test_pass!("线程睡眠时序测试通过");
    stats.pass();
}

fn test_fibonacci_computation(stats: &mut TestStats) {
    test_subsection!("递归计算线程测试");
    
    let thread_count = 3;
    let mut tids = Vec::new();
    let fib_args = [10, 12, 15]; // 不同的斐波那契数列参数
    
    for i in 0..thread_count {
        let sp = create_stack(16384); // 更大的栈用于递归
        let tid = thread_create(fibonacci_thread as usize, sp, fib_args[i]);
        test_assert!(tid > 0, "创建斐波那契线程 {} 失败", i);
        tids.push(tid);
        test_info!("创建斐波那契线程 {} TID: {} 计算fib({})", i, tid, fib_args[i]);
    }
    
    // 等待计算结果
    for (i, &tid) in tids.iter().enumerate() {
        let mut code = -1;
        let r = thread_join(tid as usize, &mut code);
        test_assert!(r == 0, "等待斐波那契线程 {} 失败", i);
        test_assert!(code >= 0, "斐波那契线程 {} 退出码异常: {}", i, code);
        test_info!("斐波那契线程 {} 计算完成，fib({}) mod 256 = {}", 
                  i, fib_args[i], code);
    }

    test_pass!("递归计算线程测试通过");
    stats.pass();
}

fn test_thread_info_syscalls(stats: &mut TestStats) {
    test_subsection!("线程信息系统调用测试");
    
    let main_pid = getpid();
    let main_tid = gettid();
    test_assert!(main_pid > 0, "getpid 返回无效值: {}", main_pid);
    test_assert!(main_tid > 0, "gettid 返回无效值: {}", main_tid);
    test_info!("主线程 - PID: {}, TID: {}", main_pid, main_tid);
    
    // 在子线程中验证进程和线程ID
    let sp = create_stack(4096);
    let tid = thread_create(simple_thread_entry as usize, sp, 42);
    test_assert!(tid > 0, "创建测试线程失败");
    
    let mut code = -1;
    let r = thread_join(tid as usize, &mut code);
    test_assert!(r == 0, "等待测试线程失败");
    test_assert!(code == 42, "测试线程退出码错误: {}", code);
    
    test_info!("线程 ID 获取正常，创建线程 TID: {}", tid);

    test_pass!("线程信息系统调用测试通过");
    stats.pass();
}

fn test_error_conditions(stats: &mut TestStats) {
    test_subsection!("错误条件处理测试");
    
    // 测试非法参数
    let invalid_tid = thread_create(0, 0, 0); // 无效入口点和栈指针
    test_info!("无效参数 thread_create 返回: {}", invalid_tid);
    
    // 测试等待不存在的线程
    let mut dummy_code = -1;
    let invalid_join = thread_join(99999, &mut dummy_code);
    test_info!("等待不存在线程返回: {}", invalid_join);
    
    // 测试重复等待同一线程
    let sp = create_stack(4096);
    let tid = thread_create(simple_thread_entry as usize, sp, 123);
    if tid > 0 {
        let mut code1 = -1;
        let r1 = thread_join(tid as usize, &mut code1);
        test_assert!(r1 == 0, "首次等待应该成功");
        test_assert!(code1 == 123, "退出码错误");
        
        let mut code2 = -1;
        let r2 = thread_join(tid as usize, &mut code2);
        test_info!("重复等待同一线程返回: {}", r2);
        // 重复等待应该失败或返回错误
    }

    test_pass!("错误条件处理测试通过");
    stats.pass();
}

fn test_large_stack_usage(stats: &mut TestStats) {
    test_subsection!("大栈空间使用测试");
    
    // 测试不同大小的栈
    let stack_sizes = [4096, 8192, 16384, 32768];
    
    for (i, &size) in stack_sizes.iter().enumerate() {
        let sp = create_stack(size);
        let tid = thread_create(compute_thread as usize, sp, i + 1);
        
        if tid > 0 {
            test_info!("使用 {} 字节栈创建线程成功，TID: {}", size, tid);
            let mut code = -1;
            let r = thread_join(tid as usize, &mut code);
            test_assert!(r == 0, "等待大栈线程 {} 失败", i);
            test_info!("大栈线程 {} 完成，栈大小: {} 字节", i, size);
        } else {
            test_warn!("无法使用 {} 字节栈创建线程", size);
        }
    }

    test_pass!("大栈空间使用测试通过");
    stats.pass();
}

#[unsafe(no_mangle)]
fn main() -> i32 {
    let mut stats = TestStats::new();
    
    test_section!("线程管理子系统综合测试");
    
    test_basic_thread_operations(&mut stats);
    test_multiple_threads(&mut stats);
    test_shared_data_access(&mut stats);
    test_compute_intensive_threads(&mut stats);
    test_sleep_timing_threads(&mut stats);
    test_fibonacci_computation(&mut stats);
    test_thread_info_syscalls(&mut stats);
    test_error_conditions(&mut stats);
    test_large_stack_usage(&mut stats);
    
    test_section!("线程管理测试总结");
    test_summary!(stats.total, stats.passed, stats.failed);
    
    if stats.failed == 0 {
        test_pass!("线程管理子系统测试全部通过");
        exit(0);
    } else {
        test_fail!("线程管理子系统测试发现 {} 个失败", stats.failed);
        exit(1);
    }
    0
}


