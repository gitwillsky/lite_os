#![feature(allocator_ext)]
#![allow(dead_code, unused_imports)]
//! 实际 inotify event codec/queue 与 UserNS ucounts 的 CPU 测量；不复制生产算法。
extern crate alloc;
#[path = "../../kernel-unit/src/allocation_failure.rs"]
mod allocation_failure;
mod cost_policy;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../../kernel-unit/src/namespace_fixture.rs"]
mod namespace;
#[path = "../../../kernel/src/storage/fs/inotify/queue.rs"]
mod queue;
use alloc::{boxed::Box, vec::Vec};
use namespace::{UserResource, UserResourceCharge};
use std::{hint::black_box, time::Instant};

fn measure(name: &str, iterations: u64, gated: bool, mut operation: impl FnMut(u64)) {
    for index in 0..iterations {
        operation(index);
    }
    let mut samples = [0.0f64; cost_policy::SAMPLES];
    for sample in &mut samples {
        let start = Instant::now();
        for index in 0..iterations {
            operation(black_box(index));
        }
        *sample = start.elapsed().as_nanos() as f64 / iterations as f64;
    }
    samples.sort_by(f64::total_cmp);
    let median = samples[samples.len() / 2];
    println!("{name}: {median:.2} ns/op");
    if gated {
        assert!(
            median <= cost_policy::MAX_NANOSECONDS_PER_OPERATION,
            "{name}: existing hot-path policy exceeded"
        );
    }
}
fn main() {
    let mut queue = queue::EventQueue::try_new(16384).unwrap();
    let event = queue::Event {
        wd: 1,
        mask: 2,
        cookie: 0,
        name: Box::from(&b"watched-file"[..]),
    };
    measure(
        "inotify native stack record codec",
        cost_policy::ITERATIONS,
        true,
        |_| {
            black_box(&event)
                .copy_with(|bytes| {
                    black_box(bytes);
                    Ok::<_, ()>(())
                })
                .unwrap();
        },
    );
    for wd in 1..=8192 {
        let prepared = queue::EventQueue::prepare(queue::Event {
            wd,
            ..event.clone()
        })
        .unwrap();
        drop(queue.push(Some(prepared)));
    }
    measure(
        "inotify actual FIONREAD aggregate with 8192 records",
        cost_policy::ITERATIONS,
        true,
        |_| {
            black_box(black_box(&queue).pending_bytes());
        },
    );
    measure(
        "inotify reusable overflow publication/read without allocation",
        cost_policy::ITERATIONS,
        true,
        |_| {
            drop(queue.push(None));
            let record = queue.pop(usize::MAX).unwrap().unwrap();
            record
                .copy_with(|bytes| {
                    black_box(bytes);
                    Ok::<_, ()>(())
                })
                .unwrap();
        },
    );
    // 以上 warm workload 已逐条消费普通 records；余下记录先明确排空再测 cold node lifetime。
    while queue.pop(usize::MAX).unwrap().is_some() {}
    for resident in [0, 8192] {
        for wd in 1..=resident {
            drop(
                queue.push(Some(
                    queue::EventQueue::prepare(queue::Event {
                        wd,
                        ..event.clone()
                    })
                    .unwrap(),
                )),
            );
        }
        measure(
            &format!("inotify cold prepared-node enqueue/pop with {resident} resident records"),
            20_000,
            false,
            |index| {
                // wd 每轮不同，避免 native cookie-insensitive tail merge 改变 resident workload。
                let prepared = queue::EventQueue::prepare(queue::Event {
                    wd: 20_000 + index as i32,
                    ..event.clone()
                })
                .unwrap();
                drop(queue.push(Some(prepared)));
                black_box(queue.pop(usize::MAX).unwrap().unwrap());
            },
        );
        while queue.pop(usize::MAX).unwrap().is_some() {}
    }
    let owner = namespace::boot();
    let holds: Vec<_> = (1..=1024)
        .map(|uid| {
            UserResourceCharge::acquire(&owner, uid, UserResource::InotifyInstances).unwrap()
        })
        .collect();
    measure(
        "inotify existing UID ucounts admission/retirement with 1024 UID entries",
        20_000,
        false,
        |_| {
            black_box(
                UserResourceCharge::acquire(&owner, 500, UserResource::InotifyWatches).unwrap(),
            );
        },
    );
    measure(
        "inotify new UID ucounts node admission/retirement with 1024 UID entries",
        20_000,
        false,
        |index| {
            black_box(
                UserResourceCharge::acquire(
                    &owner,
                    10_000 + index as u32,
                    UserResource::InotifyWatches,
                )
                .unwrap(),
            );
        },
    );
    drop(holds);
}
