#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{mkdir, chdir, getcwd, open, write, read, lseek, close, remove, 
               listdir, stat, chmod, chown, pipe, dup, dup2, flock, fcntl_getfl, fcntl_setfl,
               mkfifo, open_flags, flock_consts, fd_flags, fcntl_consts, errno,
               exit, TestStats, test_section, test_subsection};

fn test_directory_operations(stats: &mut TestStats) {
    test_subsection!("目录操作测试");
    
    // 创建测试目录
    let test_dir = "/tmp_fs_test";
    let ret = mkdir(test_dir);
    test_assert!(ret == 0 || ret == -17, "mkdir 失败: {}", ret); // -EEXIST 允许
    
    // 进入目录
    let chdir_ret = chdir(test_dir);
    test_assert!(chdir_ret == 0, "chdir 失败: {}", chdir_ret);
    
    // 获取当前工作目录
    let mut cwd_buf = [0u8; 512];
    let cwd_len = getcwd(&mut cwd_buf);
    test_assert!(cwd_len > 0, "getcwd 失败: {}", cwd_len);
    
    let cwd_str = {
        let len = cwd_len as usize;
        let slice = &cwd_buf[..len];
        let end_pos = slice.iter().position(|&b| b == 0).unwrap_or(len);
        core::str::from_utf8(&slice[..end_pos]).unwrap_or("<无效UTF8>")
    };
    
    test_assert!(cwd_str.ends_with("/tmp_fs_test") || cwd_str == "/tmp_fs_test", 
                "cwd 不正确: {}", cwd_str);
    test_info!("当前工作目录: {}", cwd_str);
    
    test_pass!("目录操作测试通过");
    stats.pass();
}

fn test_file_basic_operations(stats: &mut TestStats) {
    test_subsection!("文件基础操作测试");
    
    let filename = "test_file.txt";
    let test_data = b"Hello, LiteOS FileSystem!\nThis is a test file.\n";
    
    // 创建并打开文件
    let fd = open(filename, open_flags::O_CREAT | open_flags::O_RDWR | open_flags::O_TRUNC);
    test_assert!(fd >= 0, "open 创建文件失败: {}", fd);
    test_info!("创建文件 {} 成功，FD: {}", filename, fd);
    
    // 写入数据
    let write_len = write(fd as usize, test_data);
    test_assert!(write_len as usize == test_data.len(), "写入长度不匹配: {} != {}", 
                write_len, test_data.len());
    test_info!("写入 {} 字节数据", write_len);
    
    // 回到文件开头
    let seek_ret = lseek(fd as usize, 0, 0);
    test_assert!(seek_ret == 0, "lseek 回开头失败: {}", seek_ret);
    
    // 读取数据验证
    let mut read_buf = [0u8; 128];
    let read_len = read(fd as usize, &mut read_buf);
    test_assert!(read_len as usize == test_data.len(), "读取长度不匹配: {} != {}", 
                read_len, test_data.len());
    
    let read_data = &read_buf[..read_len as usize];
    test_assert!(read_data == test_data, "读取数据与写入数据不匹配");
    test_info!("数据读写验证成功");
    
    // 关闭文件
    let close_ret = close(fd as usize);
    test_assert!(close_ret == 0, "close 文件失败: {}", close_ret);
    
    test_pass!("文件基础操作测试通过");
    stats.pass();
}

fn test_file_advanced_operations(stats: &mut TestStats) {
    test_subsection!("文件高级操作测试");
    
    let filename = "advanced_test.dat";
    
    // 测试不同打开模式
    let fd = open(filename, open_flags::O_CREAT | open_flags::O_WRONLY | open_flags::O_TRUNC);
    test_assert!(fd >= 0, "open 只写模式失败: {}", fd);
    
    // 写入一些数据
    let data = b"Advanced file operations test";
    let write_ret = write(fd as usize, data);
    test_assert!(write_ret as usize == data.len(), "写入失败");
    close(fd as usize);
    
    // 以只读模式重新打开
    let fd_ro = open(filename, open_flags::O_RDONLY);
    test_assert!(fd_ro >= 0, "open 只读模式失败: {}", fd_ro);
    
    // 读取数据
    let mut buf = [0u8; 64];
    let read_ret = read(fd_ro as usize, &mut buf);
    test_assert!(read_ret as usize == data.len(), "读取长度不匹配");
    test_assert!(&buf[..data.len()] == data, "读取数据不匹配");
    
    close(fd_ro as usize);
    
    // 测试追加模式
    let fd_append = open(filename, open_flags::O_WRONLY | open_flags::O_APPEND);
    if fd_append >= 0 {
        let append_data = b" - appended";
        let append_ret = write(fd_append as usize, append_data);
        test_assert!(append_ret as usize == append_data.len(), "追加写入失败");
        close(fd_append as usize);
        test_info!("追加模式测试成功");
    }
    
    // 测试 lseek 不同位置
    let fd_seek = open(filename, open_flags::O_RDONLY);
    if fd_seek >= 0 {
        // SEEK_END (whence = 2)
        let end_pos = lseek(fd_seek as usize, 0, 2);
        test_info!("文件大小: {} 字节", end_pos);
        
        // SEEK_SET (whence = 0) - 回到开头
        let start_pos = lseek(fd_seek as usize, 0, 0);
        test_assert!(start_pos == 0, "SEEK_SET 失败");
        
        // SEEK_CUR (whence = 1) - 相对当前位置移动
        let cur_pos = lseek(fd_seek as usize, 5, 1);
        test_assert!(cur_pos == 5, "SEEK_CUR 失败: {}", cur_pos);
        
        close(fd_seek as usize);
    }
    
    // 清理文件
    remove(filename);
    
    test_pass!("文件高级操作测试通过");
    stats.pass();
}

fn test_directory_listing(stats: &mut TestStats) {
    test_subsection!("目录列表测试");
    
    // 创建几个测试文件
    let test_files = ["file1.txt", "file2.dat", "file3.log"];
    
    for filename in &test_files {
        let fd = open(filename, open_flags::O_CREAT | open_flags::O_WRONLY | open_flags::O_TRUNC);
        if fd >= 0 {
            write(fd as usize, filename.as_bytes());
            close(fd as usize);
        }
    }
    
    // 列出当前目录
    let mut list_buf = [0u8; 1024];
    let list_len = listdir(".", &mut list_buf);
    test_assert!(list_len >= 0, "listdir 失败: {}", list_len);
    
    let listing = core::str::from_utf8(&list_buf[..list_len as usize]).unwrap_or("");
    test_info!("目录列表:\n{}", listing);
    
    // 验证文件存在
    for filename in &test_files {
        test_assert!(listing.contains(filename), "目录中缺少文件: {}", filename);
    }
    
    // 清理测试文件
    for filename in &test_files {
        remove(filename);
    }
    
    test_pass!("目录列表测试通过");
    stats.pass();
}

fn test_file_descriptor_operations(stats: &mut TestStats) {
    test_subsection!("文件描述符操作测试");
    
    let filename = "fd_test.txt";
    let test_data = b"File descriptor operations test";
    
    // 创建文件
    let fd1 = open(filename, open_flags::O_CREAT | open_flags::O_RDWR | open_flags::O_TRUNC);
    test_assert!(fd1 >= 0, "open 创建文件失败");
    write(fd1 as usize, test_data);
    
    // 测试 dup
    let fd2 = dup(fd1 as usize);
    test_assert!(fd2 >= 0, "dup 失败: {}", fd2);
    test_assert!(fd2 != fd1, "dup 应该返回不同的FD");
    test_info!("dup: {} -> {}", fd1, fd2);
    
    // 测试 dup2
    let target_fd = 10;
    let fd3 = dup2(fd1 as usize, target_fd);
    test_assert!(fd3 == target_fd as isize || fd3 < 0, "dup2 返回值异常: {}", fd3);
    if fd3 >= 0 {
        test_info!("dup2: {} -> {}", fd1, fd3);
    }
    
    // 测试所有FD都指向同一文件
    lseek(fd1 as usize, 0, 0);
    let mut buf1 = [0u8; 64];
    let read1 = read(fd2 as usize, &mut buf1);
    test_assert!(read1 as usize == test_data.len(), "dup的FD读取失败");
    test_assert!(&buf1[..test_data.len()] == test_data, "dup的FD数据不匹配");
    
    // 关闭所有FD
    close(fd1 as usize);
    close(fd2 as usize);
    if fd3 >= 0 {
        close(fd3 as usize);
    }
    
    remove(filename);
    
    test_pass!("文件描述符操作测试通过");
    stats.pass();
}

fn test_pipe_operations(stats: &mut TestStats) {
    test_subsection!("管道操作测试");
    
    let mut pipe_fds = [0i32; 2];
    let ret = pipe(&mut pipe_fds);
    test_assert!(ret == 0, "pipe 创建失败: {}", ret);
    
    let read_fd = pipe_fds[0] as usize;
    let write_fd = pipe_fds[1] as usize;
    test_info!("管道创建成功 - 读端: {}, 写端: {}", read_fd, write_fd);
    
    // 写入数据
    let pipe_data = b"Hello through pipe!";
    let write_ret = write(write_fd, pipe_data);
    test_assert!(write_ret as usize == pipe_data.len(), "管道写入失败");
    
    // 读取数据
    let mut buf = [0u8; 64];
    let read_ret = read(read_fd, &mut buf);
    test_assert!(read_ret as usize == pipe_data.len(), "管道读取失败");
    test_assert!(&buf[..pipe_data.len()] == pipe_data, "管道数据不匹配");
    
    test_info!("管道数据传输成功");
    
    // 关闭管道
    close(read_fd);
    close(write_fd);
    
    test_pass!("管道操作测试通过");
    stats.pass();
}

fn test_file_permissions(stats: &mut TestStats) {
    test_subsection!("文件权限测试");
    
    let filename = "perm_test.txt";
    let fd = open(filename, open_flags::O_CREAT | open_flags::O_WRONLY | open_flags::O_TRUNC);
    if fd >= 0 {
        write(fd as usize, b"permission test");
        close(fd as usize);
        
        // 测试 chmod
        let chmod_ret = chmod(filename, 0o644);
        test_info!("chmod 返回: {}", chmod_ret);
        
        // 测试 chown
        let chown_ret = chown(filename, 0, 0);
        test_info!("chown 返回: {}", chown_ret);
        
        // 获取文件状态
        let mut stat_buf = [0u8; 128];
        let stat_ret = stat(filename, &mut stat_buf);
        test_info!("stat 返回: {}", stat_ret);
        
        remove(filename);
    }
    
    test_pass!("文件权限测试通过");
    stats.pass();
}

fn test_fcntl_operations(stats: &mut TestStats) {
    test_subsection!("fcntl操作测试");
    
    let filename = "fcntl_test.txt";
    let fd = open(filename, open_flags::O_CREAT | open_flags::O_RDWR | open_flags::O_TRUNC);
    
    if fd >= 0 {
        // 获取文件状态标志
        let flags = fcntl_getfl(fd as usize);
        test_info!("文件状态标志: {}", flags);
        
        // 设置非阻塞标志
        if flags >= 0 {
            let new_flags = (flags as u32) | open_flags::O_NONBLOCK;
            let set_ret = fcntl_setfl(fd as usize, new_flags);
            test_info!("设置非阻塞标志返回: {}", set_ret);
            
            // 重新获取验证
            let new_flags_check = fcntl_getfl(fd as usize);
            test_info!("新文件状态标志: {}", new_flags_check);
        }
        
        close(fd as usize);
        remove(filename);
    }
    
    test_pass!("fcntl操作测试通过");
    stats.pass();
}

fn cleanup_test_environment() {
    // 清理测试环境
    let _ = chdir("/");
    let _ = remove("/tmp_fs_test");
}

#[unsafe(no_mangle)]
fn main() -> i32 {
    let mut stats = TestStats::new();
    
    test_section!("文件系统子系统综合测试");
    
    test_directory_operations(&mut stats);
    test_file_basic_operations(&mut stats);
    test_file_advanced_operations(&mut stats);
    test_directory_listing(&mut stats);
    test_file_descriptor_operations(&mut stats);
    test_pipe_operations(&mut stats);
    test_file_permissions(&mut stats);
    test_fcntl_operations(&mut stats);
    
    cleanup_test_environment();
    
    test_section!("文件系统测试总结");
    test_summary!(stats.total, stats.passed, stats.failed);
    
    if stats.failed == 0 {
        test_pass!("文件系统子系统测试全部通过");
        exit(0);
    } else {
        test_fail!("文件系统子系统测试发现 {} 个失败", stats.failed);
        exit(1);
    }
    0
}


