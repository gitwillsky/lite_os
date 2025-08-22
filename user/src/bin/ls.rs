#![no_std]
#![no_main]

#[macro_use]
extern crate alloc;
#[macro_use]
extern crate user_lib;

use alloc::string::String;
use alloc::vec::Vec;
use user_lib::{exit, get_args, getcwd, listdir, stat};

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct FileStat {
    size: u64,
    file_type: InodeType,
    mode: u32,
    nlink: u32,
    uid: u32,
    gid: u32,
    atime: u64,
    mtime: u64,
    ctime: u64,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InodeType {
    File = 0,
    Directory = 1,
    SymLink = 2,
    Device = 3,
    Fifo = 4,
}

fn format_permissions(mode: u32, file_type: InodeType) -> String {
    let mut perms = String::new();

    // File type
    match file_type {
        InodeType::Directory => perms.push('d'),
        InodeType::SymLink => perms.push('l'),
        InodeType::Device => perms.push('c'),
        InodeType::Fifo => perms.push('p'),
        InodeType::File => perms.push('-'),
    }

    // Owner permissions
    perms.push(if mode & 0o400 != 0 { 'r' } else { '-' });
    perms.push(if mode & 0o200 != 0 { 'w' } else { '-' });
    perms.push(if mode & 0o100 != 0 { 'x' } else { '-' });

    // Group permissions
    perms.push(if mode & 0o040 != 0 { 'r' } else { '-' });
    perms.push(if mode & 0o020 != 0 { 'w' } else { '-' });
    perms.push(if mode & 0o010 != 0 { 'x' } else { '-' });

    // Others permissions
    perms.push(if mode & 0o004 != 0 { 'r' } else { '-' });
    perms.push(if mode & 0o002 != 0 { 'w' } else { '-' });
    perms.push(if mode & 0o001 != 0 { 'x' } else { '-' });

    perms
}

fn format_size(size: u64) -> String {
    if size < 1024 {
        format!("{}", size)
    } else if size < 1024 * 1024 {
        format!("{}K", size / 1024)
    } else if size < 1024 * 1024 * 1024 {
        format!("{}M", size / (1024 * 1024))
    } else {
        format!("{}G", size / (1024 * 1024 * 1024))
    }
}

fn list_long_format(path: &str) -> i32 {
    // Get directory listing first
    let mut buf = [0u8; 1024];
    let len = listdir(path, &mut buf);
    if len < 0 {
        match len {
            -2 => println!("ls: cannot access '{}': No such file or directory", path),
            -13 => println!("ls: cannot open directory '{}': Permission denied", path),
            -20 => println!("ls: cannot access '{}': Not a directory", path),
            _ => println!("ls: cannot access '{}': Unknown error ({})", path, len),
        }
        return 1;
    }

    let contents = core::str::from_utf8(&buf[..len as usize]).unwrap_or("Invalid UTF-8");
    let entries: Vec<&str> = contents.split('\n').filter(|s| !s.is_empty()).collect();

    for entry in entries {
        // Build full path for stat
        let full_path = if path == "." {
            String::from(entry)
        } else {
            format!("{}/{}", path, entry)
        };

        // Get file stats
        let mut stat_buf = [0u8; core::mem::size_of::<FileStat>()];
        let stat_result = stat(&full_path, &mut stat_buf);

        if stat_result == 0 {
            // Parse stat buffer
            let file_stat = unsafe { *(stat_buf.as_ptr() as *const FileStat) };

            // Format and print long format
            let perms = format_permissions(file_stat.mode, file_stat.file_type);
            let size_str = format_size(file_stat.size);

            println!(
                "{} {} {} {} {} {}",
                perms, file_stat.nlink, file_stat.uid, file_stat.gid, size_str, entry
            );
        } else {
            // If stat fails, just print the name with unknown permissions
            println!("?????????? 1 0 0 ? {}", entry);
        }
    }

    0
}

#[unsafe(no_mangle)]
fn main() -> i32 {
    let mut argc = 0;
    let mut argv_buf = [0u8; 1024];

    // 获取命令行参数
    let result = get_args(&mut argc, &mut argv_buf);
    if result < 0 {
        println!("ls: Failed to get arguments");
        return 1;
    }

    // 解析参数
    let mut path = "."; // 默认为当前目录
    let mut long_format = false;

    if argc > 1 {
        let args_str = core::str::from_utf8(&argv_buf[..result as usize]).unwrap_or("");
        let args: Vec<&str> = args_str.split('\0').filter(|s| !s.is_empty()).collect();

        let mut i = 1; // Skip program name
        while i < args.len() {
            let arg = args[i];
            if arg.starts_with("-") {
                if arg.contains("l") {
                    long_format = true;
                }
                if arg == "-l" || arg.len() > 1 {
                    // Continue to next arg
                }
            } else {
                // This is a path argument
                path = arg;
            }
            i += 1;
        }
    }

    if long_format {
        list_long_format(path)
    } else {
        // 执行普通ls操作
        let mut buf = [0u8; 1024];
        let len = listdir(path, &mut buf);
        if len >= 0 {
            let contents = core::str::from_utf8(&buf[..len as usize]).unwrap_or("Invalid UTF-8");
            print!("{}", contents);
            0
        } else {
            match len {
                -2 => println!("ls: cannot access '{}': No such file or directory", path),
                -13 => println!("ls: cannot open directory '{}': Permission denied", path),
                -20 => println!("ls: cannot access '{}': Not a directory", path),
                _ => println!("ls: cannot access '{}': Unknown error ({})", path, len),
            }
            1
        }
    }
}
