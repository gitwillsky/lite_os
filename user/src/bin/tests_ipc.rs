#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
extern crate alloc;

use alloc::vec;

use user_lib::{close, exit, flock, flock_consts, fork, kill, mkdir, mkfifo, open, open_flags, pipe, read, remove, signals, sleep_ms, test_section, test_subsection, wait_pid, write, TestStats};

fn test_pipe_basic(stats: &mut TestStats) {
    test_subsection!("基础管道通信测试");

    let mut pipe_fds = [0i32; 2];
    let ret = pipe(&mut pipe_fds);
    test_assert!(ret == 0, "pipe 创建失败: {}", ret);

    let read_fd = pipe_fds[0] as usize;
    let write_fd = pipe_fds[1] as usize;
    test_info!("管道创建成功 - 读端: {}, 写端: {}", read_fd, write_fd);

    // 简单数据传输
    let test_data = b"Hello, Pipe!";
    let write_ret = write(write_fd, test_data);
    test_assert!(write_ret as usize == test_data.len(), "管道写入失败");

    let mut buf = [0u8; 64];
    let read_ret = read(read_fd, &mut buf);
    test_assert!(read_ret as usize == test_data.len(), "管道读取失败");
    test_assert!(&buf[..test_data.len()] == test_data, "管道数据不匹配");

    close(read_fd);
    close(write_fd);

    test_pass!("基础管道通信测试通过");
    stats.pass();
}

fn test_pipe_parent_child(stats: &mut TestStats) {
    test_subsection!("父子进程管道通信测试");

    let mut pipe_fds = [0i32; 2];
    let ret = pipe(&mut pipe_fds);
    test_assert!(ret == 0, "父子进程管道创建失败: {}", ret);

    let read_fd = pipe_fds[0] as usize;
    let write_fd = pipe_fds[1] as usize;

    let pid = fork();
    test_assert!(pid >= 0, "fork 失败: {}", pid);

    if pid == 0 {
        // 子进程：关闭读端，向写端发送数据
        close(read_fd);

        let messages = [
            b"Message 1 from child",
            b"Message 2 from child",
            b"Message 3 from child"
        ];

        for msg in &messages {
            let write_ret = write(write_fd, *msg);
            test_assert!(write_ret as usize == msg.len(), "子进程管道写入失败");
            sleep_ms(10); // 短暂延迟
        }

        close(write_fd);
        exit(0);
    } else {
        // 父进程：关闭写端，从读端接收数据
        close(write_fd);

        for i in 0..3 {
            let mut buf = [0u8; 64];
            let read_ret = read(read_fd, &mut buf);
            test_assert!(read_ret > 0, "父进程管道读取失败");

            let msg = core::str::from_utf8(&buf[..read_ret as usize]).unwrap_or("<invalid>");
            test_info!("父进程接收到消息 {}: {}", i + 1, msg);
        }

        close(read_fd);

        // 等待子进程
        let mut status = -1;
        let waited_pid = wait_pid(pid as usize, &mut status);
        test_assert!(waited_pid == pid, "等待子进程失败");
        test_assert!(status == 0, "子进程退出状态错误: {}", status);
    }

    test_pass!("父子进程管道通信测试通过");
    stats.pass();
}

fn test_pipe_bidirectional(stats: &mut TestStats) {
    test_subsection!("双向管道通信测试");

    // 创建两个管道实现双向通信
    let mut pipe1 = [0i32; 2]; // 父到子
    let mut pipe2 = [0i32; 2]; // 子到父

    test_assert!(pipe(&mut pipe1) == 0, "创建管道1失败");
    test_assert!(pipe(&mut pipe2) == 0, "创建管道2失败");

    let pid = fork();
    test_assert!(pid >= 0, "双向通信fork失败: {}", pid);

    if pid == 0 {
        // 子进程
        close(pipe1[1] as usize); // 关闭管道1写端
        close(pipe2[0] as usize); // 关闭管道2读端

        // 从父进程接收消息
        let mut buf = [0u8; 64];
        let read_ret = read(pipe1[0] as usize, &mut buf);
        test_assert!(read_ret > 0, "子进程接收失败");

        let received = core::str::from_utf8(&buf[..read_ret as usize]).unwrap_or("<invalid>");
        test_info!("子进程接收: {}", received);

        // 向父进程发送回复
        let reply = b"Reply from child";
        let write_ret = write(pipe2[1] as usize, reply);
        test_assert!(write_ret as usize == reply.len(), "子进程回复失败");

        close(pipe1[0] as usize);
        close(pipe2[1] as usize);
        exit(0);
    } else {
        // 父进程
        close(pipe1[0] as usize); // 关闭管道1读端
        close(pipe2[1] as usize); // 关闭管道2写端

        // 向子进程发送消息
        let msg = b"Hello from parent";
        let write_ret = write(pipe1[1] as usize, msg);
        test_assert!(write_ret as usize == msg.len(), "父进程发送失败");
        test_info!("父进程发送: {}", core::str::from_utf8(msg).unwrap_or("<invalid>"));

        // 接收子进程回复
        let mut buf = [0u8; 64];
        let read_ret = read(pipe2[0] as usize, &mut buf);
        test_assert!(read_ret > 0, "父进程接收回复失败");

        let reply = core::str::from_utf8(&buf[..read_ret as usize]).unwrap_or("<invalid>");
        test_info!("父进程接收回复: {}", reply);

        close(pipe1[1] as usize);
        close(pipe2[0] as usize);

        // 等待子进程
        let mut status = -1;
        let waited_pid = wait_pid(pid as usize, &mut status);
        test_assert!(waited_pid == pid, "等待子进程失败");
    }

    test_pass!("双向管道通信测试通过");
    stats.pass();
}

fn test_named_pipe(stats: &mut TestStats) {
    test_subsection!("命名管道(FIFO)测试");

    let fifo_path = "/tmp/test_fifo";

    // 创建命名管道
    let mkfifo_ret = mkfifo(fifo_path, 0o666);
    if mkfifo_ret < 0 {
        test_warn!("mkfifo 失败，跳过命名管道测试: {}", mkfifo_ret);
        return;
    }

    test_info!("命名管道创建成功: {}", fifo_path);

    let pid = fork();
    test_assert!(pid >= 0, "命名管道测试fork失败: {}", pid);

    if pid == 0 {
        // 子进程：写入数据
        sleep_ms(50); // 等待父进程准备

        let fd = open(fifo_path, open_flags::O_WRONLY);
        if fd >= 0 {
            let data = b"Named pipe test data";
            let write_ret = write(fd as usize, data);
            test_assert!(write_ret as usize == data.len(), "命名管道写入失败");
            close(fd as usize);
            test_info!("子进程向命名管道写入数据成功");
        }
        exit(0);
    } else {
        // 父进程：读取数据
        let fd = open(fifo_path, open_flags::O_RDONLY);
        if fd >= 0 {
            let mut buf = [0u8; 64];
            let read_ret = read(fd as usize, &mut buf);
            test_assert!(read_ret > 0, "命名管道读取失败");

            let data = core::str::from_utf8(&buf[..read_ret as usize]).unwrap_or("<invalid>");
            test_info!("父进程从命名管道读取: {}", data);
            close(fd as usize);
        }

        // 等待子进程
        let mut status = -1;
        wait_pid(pid as usize, &mut status);

        // 清理命名管道
        remove(fifo_path);
    }

    test_pass!("命名管道测试通过");
    stats.pass();
}

fn test_file_locking(stats: &mut TestStats) {
    test_subsection!("文件锁定测试");
    mkdir("/tmp");

    let lock_file = "/tmp/locktest.txt";
    let fd = open(lock_file, open_flags::O_CREAT | open_flags::O_RDWR | open_flags::O_TRUNC);

    if fd < 0 {
        test_warn!("无法创建锁定测试文件，跳过文件锁定测试");
        return;
    }

    // 写入一些数据
    let data = b"File locking test";
    write(fd as usize, data);

    // 测试排他锁
    let lock_ret = flock(fd as usize, flock_consts::LOCK_EX);
    test_info!("排他锁设置结果: {}", lock_ret);

    // 测试非阻塞锁定
    let nb_lock_ret = flock(fd as usize, flock_consts::LOCK_EX | flock_consts::LOCK_NB);
    test_info!("非阻塞排他锁结果: {}", nb_lock_ret);

    // 解锁
    let unlock_ret = flock(fd as usize, flock_consts::LOCK_UN);
    test_info!("解锁结果: {}", unlock_ret);

    // 测试共享锁
    let shared_lock_ret = flock(fd as usize, flock_consts::LOCK_SH);
    test_info!("共享锁设置结果: {}", shared_lock_ret);

    // 再次解锁
    let unlock_ret2 = flock(fd as usize, flock_consts::LOCK_UN);
    test_info!("第二次解锁结果: {}", unlock_ret2);

    close(fd as usize);
    remove(lock_file);

    test_pass!("文件锁定测试通过");
    stats.pass();
}

fn test_signal_ipc(stats: &mut TestStats) {
    test_subsection!("信号IPC基础测试");

    let pid = fork();
    test_assert!(pid >= 0, "信号IPC测试fork失败: {}", pid);

    if pid == 0 {
        // 子进程：等待信号
        test_info!("子进程等待信号...");
        sleep_ms(100); // 模拟等待
        exit(42);
    } else {
        // 父进程：发送信号
        sleep_ms(50); // 让子进程先准备

        let signal_ret = kill(pid as usize, signals::SIGUSR1);
        test_info!("父进程发送SIGUSR1结果: {}", signal_ret);

        // 等待子进程
        let mut status = -1;
        let waited_pid = wait_pid(pid as usize, &mut status);
        test_assert!(waited_pid == pid, "等待子进程失败");
        test_info!("子进程退出状态: {}", status);
    }

    test_pass!("信号IPC基础测试通过");
    stats.pass();
}

fn test_pipe_capacity(stats: &mut TestStats) {
    test_subsection!("管道容量测试");

    let mut pipe_fds = [0i32; 2];
    let ret = pipe(&mut pipe_fds);
    test_assert!(ret == 0, "管道容量测试创建失败");

    let read_fd = pipe_fds[0] as usize;
    let write_fd = pipe_fds[1] as usize;

    // 写入大量数据测试管道缓冲
    let chunk_size = 1024;
    let test_chunk = vec![0xABu8; chunk_size];
    let mut total_written = 0;

    // 尝试写入多个块
    for i in 0..10 {
        let write_ret = write(write_fd, &test_chunk);
        if write_ret > 0 {
            total_written += write_ret as usize;
            test_info!("写入块 {}: {} 字节", i, write_ret);
        } else {
            test_info!("块 {} 写入失败: {}", i, write_ret);
            break;
        }
    }

    test_info!("总共写入 {} 字节", total_written);

    // 读取所有数据
    let mut total_read = 0;
    let mut read_buf = vec![0u8; chunk_size];

    while total_read < total_written {
        let read_ret = read(read_fd, &mut read_buf);
        if read_ret > 0 {
            total_read += read_ret as usize;
        } else {
            break;
        }
    }

    test_info!("总共读取 {} 字节", total_read);
    test_assert!(total_read == total_written, "读取字节数与写入字节数不匹配");

    close(read_fd);
    close(write_fd);

    test_pass!("管道容量测试通过");
    stats.pass();
}

#[unsafe(no_mangle)]
fn main() -> i32 {
    let mut stats = TestStats::new();

    test_section!("进程间通信(IPC)子系统综合测试");

    test_pipe_basic(&mut stats);
    test_pipe_parent_child(&mut stats);
    test_pipe_bidirectional(&mut stats);
    test_named_pipe(&mut stats);
    test_file_locking(&mut stats);
    test_signal_ipc(&mut stats);
    test_pipe_capacity(&mut stats);

    test_section!("IPC测试总结");
    test_summary!(stats.total, stats.passed, stats.failed);

    if stats.failed == 0 {
        test_pass!("进程间通信子系统测试全部通过");
        exit(0);
    } else {
        test_fail!("进程间通信子系统测试发现 {} 个失败", stats.failed);
        exit(1);
    }
    0
}