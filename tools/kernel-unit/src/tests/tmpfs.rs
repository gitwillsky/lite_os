//! tmpfs 的挂载选项解析与目录 cookie 语义。

use super::{
    mount_options::{parse as parse_items, parse_number, parse_scaled},
    tmpfs_directory::{Directory, FIRST_ENTRY_COOKIE},
    tmpfs_options::{InvalidOptions, Options, defaults, parse},
};
use std::vec::Vec;

const RAM_PAGES: u64 = 1024;

#[test]
fn option_items_split_on_commas_and_skip_empty_items() {
    let items: Vec<_> = parse_items(b",size=1m,,ro,mode=0755,").collect();
    assert_eq!(items.len(), 3);
    assert_eq!(
        (items[0].key, items[0].value),
        (&b"size"[..], Some(&b"1m"[..]))
    );
    assert_eq!((items[1].key, items[1].value), (&b"ro"[..], None));
    assert_eq!(parse_number(b"0x10"), Some(16));
    assert_eq!(parse_number(b"010"), Some(8));
    assert_eq!(parse_number(b"99999999999999999999"), None);
    assert_eq!(parse_scaled(b"4k"), Some(4096));
    assert_eq!(parse_scaled(b"2G"), Some(2 << 30));
    assert_eq!(parse_scaled(b"1x"), None);
    assert_eq!(parse_scaled(b"16000000000000000000k"), None);
}

#[test]
fn tmpfs_defaults_follow_linux_and_options_override_them() {
    assert_eq!(
        parse(b"", RAM_PAGES, defaults(RAM_PAGES)),
        Ok(Options {
            blocks: Some(RAM_PAGES / 2),
            inodes: Some(RAM_PAGES),
            mode: 0o1777,
            uid: 0,
            gid: 0,
        })
    );
    let options = parse(
        b"size=64k,nr_inodes=10,mode=755,uid=1000,gid=100",
        RAM_PAGES,
        defaults(RAM_PAGES),
    )
    .unwrap();
    assert_eq!(options.blocks, Some(16));
    assert_eq!(options.inodes, Some(10));
    assert_eq!((options.mode, options.uid, options.gid), (0o755, 1000, 100));
    // size 向上取整到页；百分比以物理内存为基数；0 表示不限。
    assert_eq!(
        parse(b"size=1", RAM_PAGES, defaults(RAM_PAGES))
            .unwrap()
            .blocks,
        Some(1)
    );
    assert_eq!(
        parse(b"size=25%", RAM_PAGES, defaults(RAM_PAGES))
            .unwrap()
            .blocks,
        Some(RAM_PAGES / 4)
    );
    assert_eq!(
        parse(b"size=0,nr_inodes=0", RAM_PAGES, defaults(RAM_PAGES)).map(|o| (o.blocks, o.inodes)),
        Ok((None, None))
    );
}

#[test]
fn tmpfs_rejects_unknown_or_malformed_options_instead_of_ignoring_them() {
    for bad in [
        &b"huge=always"[..],
        b"size",
        b"size=abc",
        b"mode=9",
        b"uid=-1",
        b"unknown=1",
    ] {
        assert_eq!(
            parse(bad, RAM_PAGES, defaults(RAM_PAGES)),
            Err(InvalidOptions),
            "{bad:?}"
        );
    }
}

fn names(directory: &Directory<u32>, after: u64) -> Vec<(u64, Vec<u8>, u32)> {
    directory
        .entries_after(after)
        .map(|(cookie, name, &value)| (cookie, name.to_vec(), value))
        .collect()
}

#[test]
fn directory_cookies_survive_mutation_during_iteration() {
    let mut directory = Directory::new();
    for (value, name) in [b"a", b"b", b"c"].iter().enumerate() {
        directory.insert(Directory::reserve(*name, value as u32).unwrap());
    }
    let listed = names(&directory, 0);
    assert_eq!(
        listed.iter().map(|entry| entry.0).collect::<Vec<_>>(),
        [3, 4, 5]
    );
    assert_eq!(listed[0].0, FIRST_ENTRY_COOKIE);

    // 读到 "b" 之后删除 "b"、新建 "d"：从 "b" 的 cookie 继续，必须得到 "c" 与新项，不重复也不漏。
    let resume = listed[1].0;
    assert_eq!(directory.remove(b"b"), Some(1));
    directory.insert(Directory::reserve(b"d", 9).unwrap());
    let rest = names(&directory, resume);
    assert_eq!(
        rest.iter()
            .map(|entry| (entry.1.clone(), entry.2))
            .collect::<Vec<_>>(),
        [(b"c".to_vec(), 2), (b"d".to_vec(), 9)]
    );
    // 被删除名字的 cookie 不会复用给后来者。
    assert_eq!(rest[1].0, 6);

    assert_eq!(directory.get(b"a"), Some(&0));
    assert_eq!(directory.get(b"b"), None);
    assert_eq!(directory.len(), 3);
}

#[test]
fn pop_first_drains_in_cookie_order_and_keeps_indexes_consistent() {
    let mut directory = Directory::new();
    for (value, name) in [b"z", b"a", b"m"].iter().enumerate() {
        directory.insert(Directory::reserve(*name, value as u32).unwrap());
    }
    assert_eq!(directory.pop_first(), Some(0));
    assert_eq!(directory.get(b"z"), None);
    assert_eq!(directory.pop_first(), Some(1));
    assert_eq!(directory.pop_first(), Some(2));
    assert_eq!(directory.pop_first(), None);
    assert_eq!(directory.len(), 0);
}

#[test]
fn remount_options_keep_unmentioned_parameters() {
    let current = parse(
        b"size=64k,nr_inodes=10,mode=755,uid=7",
        RAM_PAGES,
        defaults(RAM_PAGES),
    )
    .unwrap();
    let changed = parse(b"size=128k", RAM_PAGES, current).unwrap();
    assert_eq!(changed.blocks, Some(32));
    assert_eq!(
        (changed.inodes, changed.mode, changed.uid),
        (Some(10), 0o755, 7)
    );
}
