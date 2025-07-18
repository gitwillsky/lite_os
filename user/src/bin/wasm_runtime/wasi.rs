//! WASI接口实现 - 充分利用LiteOS的系统调用
//! 
//! 这个模块提供了完整的WASI 1.0接口实现，直接映射到LiteOS的POSIX兼容系统调用

use alloc::vec::Vec;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec;
use user_lib::*;

/// WASI错误码
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WasiErrno {
    Success = 0,
    E2big = 1,
    Eacces = 2,
    Eaddrinuse = 3,
    Eaddrnotavail = 4,
    Eafnosupport = 5,
    Eagain = 6,
    Ealready = 7,
    Ebadf = 8,
    Ebadmsg = 9,
    Ebusy = 10,
    Ecanceled = 11,
    Echild = 12,
    Econnaborted = 13,
    Econnrefused = 14,
    Econnreset = 15,
    Edeadlk = 16,
    Edestaddrreq = 17,
    Edom = 18,
    Edquot = 19,
    Eexist = 20,
    Efault = 21,
    Efbig = 22,
    Ehostunreach = 23,
    Eidrm = 24,
    Eilseq = 25,
    Einprogress = 26,
    Eintr = 27,
    Einval = 28,
    Eio = 29,
    Eisconn = 30,
    Eisdir = 31,
    Eloop = 32,
    Emfile = 33,
    Emlink = 34,
    Emsgsize = 35,
    Emultihop = 36,
    Enametoolong = 37,
    Enetdown = 38,
    Enetreset = 39,
    Enetunreach = 40,
    Enfile = 41,
    Enobufs = 42,
    Enodev = 43,
    Enoent = 44,
    Enoexec = 45,
    Enolck = 46,
    Enolink = 47,
    Enomem = 48,
    Enomsg = 49,
    Enoprotoopt = 50,
    Enospc = 51,
    Enosys = 52,
    Enotconn = 53,
    Enotdir = 54,
    Enotempty = 55,
    Enotrecoverable = 56,
    Enotsock = 57,
    Enotsup = 58,
    Enotty = 59,
    Enxio = 60,
    Eoverflow = 61,
    Eownerdead = 62,
    Eperm = 63,
    Epipe = 64,
    Eproto = 65,
    Eprotonosupport = 66,
    Eprototype = 67,
    Erange = 68,
    Erofs = 69,
    Espipe = 70,
    Esrch = 71,
    Estale = 72,
    Etimedout = 73,
    Etxtbsy = 74,
    Exdev = 75,
    Enotcapable = 76,
}

impl From<WasiErrno> for String {
    fn from(errno: WasiErrno) -> Self {
        alloc::format!("WASI error: {:?}", errno)
    }
}

/// WASI文件描述符类型
pub type WasiFd = u32;

/// WASI文件大小类型
pub type WasiFilesize = u64;

/// WASI偏移量类型
pub type WasiFiledelta = i64;

/// IOV结构体 - 用于scatter-gather I/O
#[repr(C)]
#[derive(Debug)]
pub struct WasiIovec {
    pub buf: u32,    // 指向缓冲区的指针(在WASM内存中的偏移)
    pub buf_len: u32, // 缓冲区长度
}

/// WASI上下文 - 管理WASM程序的执行环境
pub struct WasiContext {
    /// 命令行参数
    args: Vec<String>,
    
    /// 环境变量
    envs: Vec<String>,
    
    /// 文件描述符映射表 (WASI FD -> LiteOS FD)
    fd_map: Vec<Option<i32>>,
    
    /// 当前工作目录
    current_dir: String,
    
    /// 预打开的目录
    preopened_dirs: Vec<String>,
}

impl WasiContext {
    /// 创建新的WASI上下文
    pub fn new(args: Vec<String>, envs: Vec<String>) -> Self {
        let mut fd_map = Vec::new();
        fd_map.resize(64, None); // 支持最多64个文件描述符
        
        // 预设标准文件描述符
        fd_map[0] = Some(0); // stdin
        fd_map[1] = Some(1); // stdout
        fd_map[2] = Some(2); // stderr
        
        // 获取当前工作目录
        let mut cwd_buf = [0u8; 256];
        let current_dir = if getcwd(&mut cwd_buf) >= 0 {
            String::from_utf8_lossy(&cwd_buf[..cwd_buf.iter().position(|&x| x == 0).unwrap_or(cwd_buf.len())])
                .to_string()
        } else {
            "/".to_string()
        };
        
        Self {
            args,
            envs,
            fd_map,
            current_dir,
            preopened_dirs: vec!["/".to_string(), ".".to_string()],
        }
    }
    
    /// 分配新的WASI文件描述符
    fn allocate_fd(&mut self, liteos_fd: i32) -> Option<WasiFd> {
        for (wasi_fd, slot) in self.fd_map.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(liteos_fd);
                return Some(wasi_fd as WasiFd);
            }
        }
        None
    }
    
    /// 获取LiteOS文件描述符
    fn get_liteos_fd(&self, wasi_fd: WasiFd) -> Option<i32> {
        self.fd_map.get(wasi_fd as usize)?.as_ref().copied()
    }
    
    /// 释放WASI文件描述符
    fn deallocate_fd(&mut self, wasi_fd: WasiFd) -> Option<i32> {
        if let Some(slot) = self.fd_map.get_mut(wasi_fd as usize) {
            slot.take()
        } else {
            None
        }
    }
}

/// WASI接口实现
impl WasiContext {
    /// args_sizes_get - 获取参数数组的大小信息
    /// 返回: (argc, argv_buf_size)
    pub fn args_sizes_get(&self) -> Result<(u32, u32), WasiErrno> {
        let argc = self.args.len() as u32;
        let argv_buf_size = self.args.iter()
            .map(|arg| arg.len() + 1) // +1 for null terminator
            .sum::<usize>() as u32;
        
        Ok((argc, argv_buf_size))
    }
    
    /// args_get - 获取参数数组
    pub fn args_get(&self, argv_ptr: u32, argv_buf: u32, memory: &mut [u8]) -> Result<(), WasiErrno> {
        println!("WASI args_get: argv_ptr=0x{:x}, argv_buf=0x{:x}, {} arguments", 
                argv_ptr, argv_buf, self.args.len());
        
        let mut buf_offset = argv_buf as usize;
        let mut ptr_offset = argv_ptr as usize;
        
        // 写入每个参数字符串到内存
        for (i, arg) in self.args.iter().enumerate() {
            // 检查内存边界
            if buf_offset + arg.len() + 1 > memory.len() || ptr_offset + 4 > memory.len() {
                println!("WASI args_get: Memory bounds exceeded for arg[{}]: {}", i, arg);
                return Err(WasiErrno::Efault);
            }
            
            // 写入指针到argv数组
            let ptr_bytes = (buf_offset as u32).to_le_bytes();
            memory[ptr_offset..ptr_offset + 4].copy_from_slice(&ptr_bytes);
            ptr_offset += 4;
            
            // 写入字符串到缓冲区
            memory[buf_offset..buf_offset + arg.len()].copy_from_slice(arg.as_bytes());
            memory[buf_offset + arg.len()] = 0; // null terminator
            buf_offset += arg.len() + 1;
            
            println!("  arg[{}]: {} -> buf_offset: 0x{:x}", i, arg, buf_offset - arg.len() - 1);
        }
        
        println!("Successfully wrote {} arguments to WASM memory", self.args.len());
        Ok(())
    }
    
    /// environ_sizes_get - 获取环境变量数组的大小信息
    pub fn environ_sizes_get(&self) -> Result<(u32, u32), WasiErrno> {
        let environc = self.envs.len() as u32;
        let environ_buf_size = self.envs.iter()
            .map(|env| env.len() + 1)
            .sum::<usize>() as u32;
        
        Ok((environc, environ_buf_size))
    }
    
    /// environ_get - 获取环境变量数组
    pub fn environ_get(&self, environ_ptr: u32, environ_buf: u32, memory: &mut [u8]) -> Result<(), WasiErrno> {
        println!("WASI environ_get: environ_ptr=0x{:x}, environ_buf=0x{:x}, {} environment variables", 
                environ_ptr, environ_buf, self.envs.len());
        
        let mut buf_offset = environ_buf as usize;
        let mut ptr_offset = environ_ptr as usize;
        
        // 写入每个环境变量字符串到内存
        for (i, env) in self.envs.iter().enumerate() {
            // 检查内存边界
            if buf_offset + env.len() + 1 > memory.len() || ptr_offset + 4 > memory.len() {
                println!("WASI environ_get: Memory bounds exceeded for env[{}]: {}", i, env);
                return Err(WasiErrno::Efault);
            }
            
            // 写入指针到environ数组
            let ptr_bytes = (buf_offset as u32).to_le_bytes();
            memory[ptr_offset..ptr_offset + 4].copy_from_slice(&ptr_bytes);
            ptr_offset += 4;
            
            // 写入字符串到缓冲区
            memory[buf_offset..buf_offset + env.len()].copy_from_slice(env.as_bytes());
            memory[buf_offset + env.len()] = 0; // null terminator
            buf_offset += env.len() + 1;
            
            println!("  env[{}]: {} -> buf_offset: 0x{:x}", i, env, buf_offset - env.len() - 1);
        }
        
        println!("Successfully wrote {} environment variables to WASM memory", self.envs.len());
        Ok(())
    }
    
    /// fd_read - 从文件描述符读取数据
    pub fn fd_read(&self, fd: WasiFd, iovs: &[WasiIovec], memory: &mut [u8]) -> Result<u32, WasiErrno> {
        let liteos_fd = self.get_liteos_fd(fd).ok_or(WasiErrno::Ebadf)?;
        
        println!("WASI fd_read: fd={}, liteos_fd={}, iovs_len={}", fd, liteos_fd, iovs.len());
        
        let mut total_read = 0u32;
        
        for (i, iov) in iovs.iter().enumerate() {
            let buf_start = iov.buf as usize;
            let buf_len = iov.buf_len as usize;
            
            // 检查内存边界
            if buf_start + buf_len > memory.len() {
                println!("WASI fd_read: Memory bounds exceeded for iov[{}]: buf=0x{:x}, len={}", 
                        i, iov.buf, buf_len);
                return Err(WasiErrno::Efault);
            }
            
            // 直接读取到WASM内存缓冲区
            let bytes_read = read(liteos_fd as usize, &mut memory[buf_start..buf_start + buf_len]);
            
            if bytes_read < 0 {
                println!("WASI fd_read: Read error: {}", bytes_read);
                return match bytes_read {
                    -1 => Err(WasiErrno::Eio),
                    -2 => Err(WasiErrno::Eagain),
                    -9 => Err(WasiErrno::Ebadf),
                    _ => Err(WasiErrno::Eio),
                };
            }
            
            total_read += bytes_read as u32;
            println!("WASI fd_read: iov[{}] read {} bytes", i, bytes_read);
            
            if bytes_read < buf_len as isize {
                break; // 读取完毕或遇到EOF
            }
        }
        
        println!("WASI fd_read: Total read {} bytes", total_read);
        Ok(total_read)
    }
    
    /// fd_write - 向文件描述符写入数据
    pub fn fd_write(&self, fd: WasiFd, iovs: &[WasiIovec], memory: &[u8]) -> Result<u32, WasiErrno> {
        let liteos_fd = self.get_liteos_fd(fd).ok_or(WasiErrno::Ebadf)?;
        
        println!("WASI fd_write: fd={}, liteos_fd={}, iovs_len={}", fd, liteos_fd, iovs.len());
        
        let mut total_written = 0u32;
        
        for (i, iov) in iovs.iter().enumerate() {
            let buf_start = iov.buf as usize;
            let buf_len = iov.buf_len as usize;
            
            // 检查内存边界
            if buf_start + buf_len > memory.len() {
                println!("WASI fd_write: Memory bounds exceeded for iov[{}]: buf=0x{:x}, len={}", 
                        i, iov.buf, buf_len);
                return Err(WasiErrno::Efault);
            }
            
            // 从WASM内存读取数据并写入文件描述符
            let data_to_write = &memory[buf_start..buf_start + buf_len];
            let bytes_written = write(liteos_fd as usize, data_to_write);
            
            if bytes_written < 0 {
                println!("WASI fd_write: Write error: {}", bytes_written);
                return match bytes_written {
                    -1 => Err(WasiErrno::Eio),
                    -2 => Err(WasiErrno::Enospc),
                    -9 => Err(WasiErrno::Ebadf),
                    _ => Err(WasiErrno::Eio),
                };
            }
            
            total_written += bytes_written as u32;
        }
        
        Ok(total_written)
    }
    
    /// path_open - 打开文件
    pub fn path_open(
        &mut self,
        dirfd: WasiFd,
        _dirflags: u32,
        path: &str,
        _oflags: u32,
        _fs_rights_base: u64,
        _fs_rights_inheriting: u64,
        _fdflags: u32,
    ) -> Result<WasiFd, WasiErrno> {
        println!("WASI path_open: dirfd={}, path={}", dirfd, path);
        
        // 构建完整路径
        let full_path = if path.starts_with('/') {
            path.to_string()
        } else {
            alloc::format!("{}/{}", self.current_dir, path)
        };
        
        // 使用LiteOS的open系统调用
        let liteos_fd = open(&full_path, 0); // O_RDONLY
        
        if liteos_fd < 0 {
            return match liteos_fd {
                -1 => Err(WasiErrno::Enoent),
                -2 => Err(WasiErrno::Eacces),
                _ => Err(WasiErrno::Eio),
            };
        }
        
        // 分配WASI文件描述符
        self.allocate_fd(liteos_fd as i32)
            .ok_or(WasiErrno::Emfile)
    }
    
    /// fd_close - 关闭文件描述符
    pub fn fd_close(&mut self, fd: WasiFd) -> Result<(), WasiErrno> {
        println!("WASI fd_close: fd={}", fd);
        
        let liteos_fd = self.deallocate_fd(fd).ok_or(WasiErrno::Ebadf)?;
        
        // 不关闭标准文件描述符
        if liteos_fd <= 2 {
            return Ok(());
        }
        
        let result = close(liteos_fd as usize);
        if result != 0 {
            Err(WasiErrno::Eio)
        } else {
            Ok(())
        }
    }
    
    /// fd_seek - 移动文件偏移量
    pub fn fd_seek(&self, fd: WasiFd, offset: WasiFiledelta, whence: u8) -> Result<WasiFilesize, WasiErrno> {
        let liteos_fd = self.get_liteos_fd(fd).ok_or(WasiErrno::Ebadf)?;
        
        println!("WASI fd_seek: fd={}, offset={}, whence={}", fd, offset, whence);
        
        let new_offset = lseek(liteos_fd as usize, offset as isize, whence as usize);
        
        if new_offset < 0 {
            Err(WasiErrno::Einval)
        } else {
            Ok(new_offset as WasiFilesize)
        }
    }
    
    /// proc_exit - 终止进程
    pub fn proc_exit(&self, exit_code: u32) -> ! {
        println!("WASI proc_exit: exit_code={}", exit_code);
        exit(exit_code as i32);
        loop {} // Unreachable but needed for ! return type
    }
    
    /// sched_yield - 让出CPU时间片
    pub fn sched_yield(&self) -> Result<(), WasiErrno> {
        yield_();
        Ok(())
    }
}

/// 将LiteOS系统调用错误码转换为WASI错误码
pub fn liteos_errno_to_wasi(liteos_errno: isize) -> WasiErrno {
    match liteos_errno {
        0 => WasiErrno::Success,
        -1 => WasiErrno::Eio,
        -2 => WasiErrno::Enoent,
        -3 => WasiErrno::Eacces,
        -4 => WasiErrno::Ebadf,
        -5 => WasiErrno::Eagain,
        _ => WasiErrno::Eio,
    }
}