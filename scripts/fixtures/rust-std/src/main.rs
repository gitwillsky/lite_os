//! Rust `std` 对 LiteOS Linux/musl ABI 的首个运行时 consumer。
//!
//! 每一阶段只在该领域全部断言成功后发布 marker；最终 marker 因而证明 allocator、entropy、
//! filesystem、thread/TLS、process、Unix socket 与 IPv4 已在同一进程生命周期完成。

use std::{
    collections::HashMap,
    env, fs,
    hash::{BuildHasher, Hash, Hasher},
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddrV4, TcpStream},
    os::unix::{
        fs::symlink,
        net::{UnixListener, UnixStream},
    },
    path::Path,
    process::Command,
    sync::{Arc, Barrier, Mutex},
    thread,
    time::{Duration, Instant},
};

thread_local! {
    static THREAD_TOKEN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn allocation_and_entropy() {
    let mut bytes = vec![0u8; 1024 * 1024];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = (index % 251) as u8;
    }
    assert_eq!(bytes[817_263], (817_263 % 251) as u8);

    let mut values = HashMap::new();
    values.insert(String::from("liteos"), 61usize);
    assert_eq!(values.get("liteos"), Some(&61));

    // RandomState::new() 通过 std 的 Linux backend 取得随机 key；缺少 getrandom 时本阶段失败。
    let state = std::collections::hash_map::RandomState::new();
    let mut first = state.build_hasher();
    let mut second = state.build_hasher();
    "rust-std-smoke".hash(&mut first);
    "rust-std-smoke".hash(&mut second);
    assert_eq!(first.finish(), second.finish());
    println!("LITEOS_RUST_STD_ALLOC_61");
}

fn filesystem(root: &Path) {
    fs::create_dir(root).expect("create std fixture directory");
    let source = root.join("source");
    let renamed = root.join("renamed");
    let link = root.join("link");
    const CONTENT: &[u8] = b"rust-std-filesystem-61\n";
    fs::write(&source, CONTENT).expect("write std fixture");
    fs::rename(&source, &renamed).expect("rename std fixture");
    symlink("renamed", &link).expect("symlink std fixture");
    assert_eq!(
        fs::read_to_string(&link).expect("read symlink"),
        "rust-std-filesystem-61\n"
    );
    assert_eq!(
        fs::read_link(&link).expect("readlink std fixture"),
        Path::new("renamed")
    );
    assert_eq!(
        fs::metadata(&renamed).expect("stat std fixture").len(),
        CONTENT.len() as u64
    );
    let mut names = fs::read_dir(root)
        .expect("read std fixture directory")
        .map(|entry| entry.expect("decode directory entry").file_name())
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(names.len(), 2);
    println!("LITEOS_RUST_STD_FS_61");
}

fn threads_and_time() {
    let value = Arc::new(Mutex::new(0usize));
    let barrier = Arc::new(Barrier::new(5));
    let mut workers = Vec::new();
    for token in 1..=4u64 {
        let value = Arc::clone(&value);
        let barrier = Arc::clone(&barrier);
        workers.push(thread::spawn(move || {
            THREAD_TOKEN.with(|slot| {
                slot.set(token);
                assert_eq!(slot.get(), token);
            });
            barrier.wait();
            *value.lock().expect("lock shared std state") += token as usize;
        }));
    }
    barrier.wait();
    for worker in workers {
        worker.join().expect("join std worker");
    }
    assert_eq!(*value.lock().expect("read shared std state"), 10);
    let started = Instant::now();
    thread::sleep(Duration::from_millis(5));
    assert!(started.elapsed() >= Duration::from_millis(5));
    println!("LITEOS_RUST_STD_THREAD_61");
}

fn process() {
    assert_eq!(env::var("PATH").expect("PATH must exist").is_empty(), false);
    let output = Command::new("/bin/sh")
        .args(["-c", "printf rust-std-process-61"])
        .output()
        .expect("spawn shell through std");
    assert!(output.status.success());
    assert_eq!(output.stdout, b"rust-std-process-61");
    println!("LITEOS_RUST_STD_PROCESS_61");
}

fn unix_socket(root: &Path) {
    let path = root.join("socket");
    let listener = UnixListener::bind(&path).expect("bind std Unix listener");
    let client_path = path.clone();
    let client = thread::spawn(move || {
        let mut stream = UnixStream::connect(client_path).expect("connect std Unix stream");
        stream.write_all(b"unix-61").expect("write std Unix stream");
        let mut reply = [0u8; 8];
        stream.read_exact(&mut reply).expect("read std Unix reply");
        assert_eq!(&reply, b"reply-61");
    });
    let (mut stream, _) = listener.accept().expect("accept std Unix stream");
    let mut request = [0u8; 7];
    stream
        .read_exact(&mut request)
        .expect("read std Unix request");
    assert_eq!(&request, b"unix-61");
    stream.write_all(b"reply-61").expect("write std Unix reply");
    client.join().expect("join std Unix client");
    println!("LITEOS_RUST_STD_UNIX_61");
}

fn ipv4_host(port: u16) {
    let address = SocketAddrV4::new(Ipv4Addr::new(10, 0, 2, 2), port);
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut stream = loop {
        match TcpStream::connect(address) {
            Ok(stream) => break stream,
            Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(100)),
            Err(error) => panic!("connect std IPv4 stream: {error}"),
        }
    };
    stream
        .write_all(b"GET /user/base/udhcpc.script HTTP/1.0\r\nHost: liteos-std-gate\r\n\r\n")
        .expect("write std IPv4 request");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .expect("read std IPv4 response");
    assert!(
        response
            .windows(b"# @description BusyBox udhcpc".len())
            .any(|window| window == b"# @description BusyBox udhcpc")
    );
    println!("LITEOS_RUST_STD_IPV4_61");
}

fn main() {
    let port = env::args()
        .nth(1)
        .expect("std IPv4 gate port argument")
        .parse::<u16>()
        .expect("parse std IPv4 gate port");
    allocation_and_entropy();
    let root = Path::new("/tmp/rust-std-smoke");
    let _ = fs::remove_dir_all(root);
    filesystem(root);
    threads_and_time();
    process();
    unix_socket(root);
    ipv4_host(port);
    fs::remove_dir_all(root).expect("remove std fixture directory");
    println!("LITEOS_RUST_STD_61");
}
