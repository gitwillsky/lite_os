#![no_std]
#![no_main]

#[macro_use]
extern crate alloc;
#[macro_use]
extern crate user_lib;

use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::max;
use user_lib::{get_args, listdir, stat};

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
    format!("{}", size)
}

fn owner_name(uid: u32) -> String {
    if uid == 0 {
        String::from("root")
    } else {
        format!("{}", uid)
    }
}

fn group_name(gid: u32) -> String {
    if gid == 0 {
        String::from("root")
    } else {
        format!("{}", gid)
    }
}

fn format_time(_ts: u64) -> String {
    String::from("Jan  1 00:00")
}

fn list_long_format(path: &str) -> i32 {
    let mut buf = [0u8; 8192];
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

    let mut entries: Vec<String> = core::str::from_utf8(&buf[..len as usize])
        .unwrap_or("")
        .split('\n')
        .filter(|s| !s.is_empty())
        .map(|s| String::from(s))
        .collect();
    entries.sort();

    struct Row {
        name: String,
        stat_ok: bool,
        st: FileStat,
    }

    let mut rows: Vec<Row> = Vec::new();
    for entry in &entries {
        let full_path = if path == "." {
            String::from(entry)
        } else {
            format!("{}/{}", path, entry)
        };
        let mut stat_buf = [0u8; core::mem::size_of::<FileStat>()];
        let r = stat(&full_path, &mut stat_buf);
        if r == 0 {
            let st = unsafe { *(stat_buf.as_ptr() as *const FileStat) };
            rows.push(Row {
                name: entry.clone(),
                stat_ok: true,
                st,
            });
        } else {
            rows.push(Row {
                name: entry.clone(),
                stat_ok: false,
                st: FileStat {
                    size: 0,
                    file_type: InodeType::File,
                    mode: 0,
                    nlink: 1,
                    uid: 0,
                    gid: 0,
                    atime: 0,
                    mtime: 0,
                    ctime: 0,
                },
            });
        }
    }

    let mut nlink_w = 1usize;
    let mut owner_w = 1usize;
    let mut group_w = 1usize;
    let mut size_w = 1usize;
    for r in &rows {
        if r.stat_ok {
            nlink_w = max(nlink_w, format!("{}", r.st.nlink).len());
            owner_w = max(owner_w, owner_name(r.st.uid).len());
            group_w = max(group_w, group_name(r.st.gid).len());
            size_w = max(size_w, format_size(r.st.size).len());
        }
    }

    for r in rows {
        if r.stat_ok {
            let perms = format_permissions(r.st.mode, r.st.file_type);
            let nlink_s = format!("{}", r.st.nlink);
            let owner_s = owner_name(r.st.uid);
            let group_s = group_name(r.st.gid);
            let size_s = format_size(r.st.size);
            let time_s = format_time(r.st.mtime);
            println!(
                "{} {:>n$} {:>o$} {:>g$} {:>s$} {} {}",
                perms,
                nlink_s,
                owner_s,
                group_s,
                size_s,
                time_s,
                r.name,
                n = nlink_w,
                o = owner_w,
                g = group_w,
                s = size_w
            );
        } else {
            println!(
                "?????????? {:>n$} {:>o$} {:>g$} {:>s$} {} {}",
                "1",
                "0",
                "0",
                "0",
                "Jan  1 00:00",
                r.name,
                n = nlink_w,
                o = owner_w,
                g = group_w,
                s = size_w
            );
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
        let mut buf = [0u8; 8192];
        let len = listdir(path, &mut buf);
        if len >= 0 {
            let mut entries: Vec<&str> = core::str::from_utf8(&buf[..len as usize])
                .unwrap_or("")
                .split('\n')
                .filter(|s| !s.is_empty())
                .collect();
            entries.sort();
            for (i, e) in entries.iter().enumerate() {
                if i > 0 {
                    print!("\n");
                }
                print!("{}", e);
            }
            print!("\n");
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
