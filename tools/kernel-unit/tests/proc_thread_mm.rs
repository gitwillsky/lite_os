#![feature(allocator_ext)]
//! Production proc snapshots/codecs; VFS error/opened entry and wall-clock tick are host seams.
#![allow(dead_code)]
extern crate alloc;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../src/namespace_fixture.rs"]
mod namespace;
#[path = "../../../kernel/src/fs"]
mod fs {
    pub(crate) struct OpenedFile;
    #[path = "procfs"]
    mod proc {
        #[derive(Debug)]
        pub(crate) enum FileSystemError {
            OutOfMemory,
        }
        mod process;
        mod snapshot;
        mod text;
        use snapshot::*;
        use text::{ProcText, proc_text};
        mod system {
            pub(super) fn ticks(value: u64) -> u64 {
                value / 10_000
            }
        }
        #[cfg(test)]
        mod tests {
            use super::*;
            fn process_snapshot() -> (ProcProcessSnapshot, Vec<crate::namespace::PidRegistration>) {
                let namespace = crate::namespace::boot();
                let pid_namespace = crate::namespace::initial_pid()
                    .child(namespace.clone(), 0)
                    .unwrap();
                let mut leader_pid = crate::namespace::PreparedPid::try_new(
                    10,
                    pid_namespace.clone(),
                    &[],
                    &crate::namespace::root_authority(),
                )
                .unwrap();
                leader_pid.publish().unwrap();
                let mut worker_pid = crate::namespace::PreparedPid::try_new(
                    11,
                    pid_namespace,
                    &[],
                    &crate::namespace::root_authority(),
                )
                .unwrap();
                worker_pid.publish().unwrap();
                let pid = leader_pid.identity();
                let tid = worker_pid.identity();
                let registrations = vec![
                    leader_pid.take_registration(),
                    worker_pid.take_registration(),
                ];

                let credentials = ProcCredentialSnapshot {
                    user_namespace: namespace,
                    uids: [1, 2, 3],
                    gids: [4, 5, 6],
                    fsids: [7, 8],
                    groups: None,
                    capabilities: syscall_abi::capability::CapabilitySnapshot::default(),
                };
                let worker = ProcThreadSnapshot {
                    tid,
                    has_mm: true,
                    fd_size: 64,
                    credentials: credentials.clone(),
                    state: b'R',
                    nice: 5,
                    priority: 25,
                    runtime_us: 70_000,
                    start_time_us: 100_000,
                    last_cpu: 1,
                    io: ProcIoSnapshot::default(),
                };
                (
                    ProcProcessSnapshot {
                        kernel_thread: false,
                        pid: pid.clone(),
                        has_mm: false,
                        ppid: None,
                        process_group: pid.clone(),
                        session: pid,
                        tty_number: 0,
                        terminal_process_group: 0,
                        comm: b"a ()\nb".to_vec(),
                        credentials,
                        state: b'Z',
                        nice: 0,
                        priority: 20,
                        threads: vec![worker],
                        runtime_us: 90_000,
                        start_time_us: 10_000,
                        virtual_pages: 31,
                        resident_pages: 17,
                        swapped_pages: 4,
                        shared_pages: 3,
                        text_pages: 5,
                        data_pages: 23,
                        fd_size: 0,
                        last_cpu: 0,
                        io: ProcIoSnapshot::default(),
                    },
                    registrations,
                )
            }
            fn string(bytes: Vec<u8>) -> String {
                String::from_utf8(bytes).unwrap()
            }
            fn fields(text: &str) -> Vec<&str> {
                text[text.rfind(')').unwrap() + 2..]
                    .split_whitespace()
                    .collect()
            }
            #[test]
            fn retired_leader_status_omits_mm_and_preserves_zombie_own_files() {
                let (process, _registrations) = process_snapshot();
                let leader = string(
                    process::format_process_status(
                        &process,
                        &crate::namespace::initial_user(),
                        &process.pid.namespace(),
                    )
                    .unwrap(),
                );
                let worker = string(
                    process::format_thread_status(
                        &process,
                        &process.threads[0],
                        &crate::namespace::initial_user(),
                        &process.pid.namespace(),
                    )
                    .unwrap(),
                );
                assert!(leader.contains("State:\tZ (zombie)\n"));
                assert!(leader.contains("FDSize:\t0\n"));
                assert!(!leader.contains("Vm"));
                assert!(worker.contains("State:\tR (running)\n"));
                assert!(worker.contains("FDSize:\t64\n"));
                assert!(worker.contains("Uid:\t1\t2\t3\t7\n"));
                assert!(worker.contains("Gid:\t4\t5\t6\t8\n"));
                assert!(worker.contains("VmSize:\t124 kB\nVmRSS:\t68 kB\nVmSwap:\t16 kB\n"));
                assert!(worker.contains("Tgid:\t1\nNgid:\t0\nPid:\t2\n"));
            }
            #[test]
            fn actual_pid_ladder_projects_mount_view_parent_and_namespace_columns() {
                let (process, _registrations) = process_snapshot();
                let leaf = process.pid.namespace();
                let root = crate::namespace::initial_pid();
                let inside = string(
                    process::format_thread_status(
                        &process,
                        &process.threads[0],
                        &crate::namespace::initial_user(),
                        &leaf,
                    )
                    .unwrap(),
                );
                assert!(inside.contains("Tgid:\t1\nNgid:\t0\nPid:\t2\nPPid:\t0\n"));
                assert!(inside.contains("NStgid:\t1\nNSpid:\t2\nNSpgid:\t1\nNSsid:\t1\n"));
                let outside = string(
                    process::format_thread_status(
                        &process,
                        &process.threads[0],
                        &crate::namespace::initial_user(),
                        &root,
                    )
                    .unwrap(),
                );
                let root_pid = process.pid.number_in(&root);
                let root_tid = process.threads[0].tid.number_in(&root);
                assert!(
                    outside.contains(&format!("NStgid:\t{root_pid}\t1\nNSpid:\t{root_tid}\t2\n"))
                );
                assert!(
                    string(
                        process::format_process_stat(
                            &process,
                            crate::namespace::TimeOffsets::ZERO,
                            &leaf
                        )
                        .unwrap()
                    )
                    .starts_with("1 (")
                );
                assert!(
                    string(
                        process::format_thread_stat(
                            &process,
                            &process.threads[0],
                            crate::namespace::TimeOffsets::ZERO,
                            &leaf
                        )
                        .unwrap()
                    )
                    .starts_with("2 (")
                );
                drop(_registrations);
                // source snapshot retains historical immutable number ladder after association retirement.
                assert_eq!(process.pid.number_in(&leaf), 1);
                assert_eq!(process.threads[0].tid.number_in(&leaf), 2);
                assert!(leaf.lookup(2).is_none());
            }
            #[test]
            fn common_pages_project_only_through_tasks_that_have_mm() {
                let (process, _registrations) = process_snapshot();
                assert_eq!(
                    string(process::format_process_statm(&process).unwrap()),
                    "0 0 0 0 0 0 0\n"
                );
                assert_eq!(
                    string(process::format_thread_statm(&process, &process.threads[0]).unwrap()),
                    "31 17 3 5 0 23 0\n"
                );
                let leader = string(
                    process::format_process_stat(
                        &process,
                        crate::namespace::TimeOffsets::ZERO,
                        &process.pid.namespace(),
                    )
                    .unwrap(),
                );
                let worker = string(
                    process::format_thread_stat(
                        &process,
                        &process.threads[0],
                        crate::namespace::TimeOffsets::ZERO,
                        &process.pid.namespace(),
                    )
                    .unwrap(),
                );
                let (leader, worker) = (fields(&leader), fields(&worker));
                // Indices start at Linux field 3, so vsize/rss are 20/21.
                assert_eq!(
                    (&leader[20..22], &worker[20..22]),
                    (&["0", "0"][..], &["126976", "17"][..])
                );
                assert_eq!(
                    (leader[0], leader[16], leader[19], leader[36]),
                    ("Z", "0", "1", "0")
                );
                assert_eq!(
                    (worker[0], worker[16], worker[19], worker[36]),
                    ("R", "5", "10", "1")
                );
            }
            #[test]
            fn kernel_kind_is_distinct_from_retired_user_mm_and_encodes_native_flags() {
                let (mut snapshot, _registrations) = process_snapshot();
                let view = snapshot.pid.namespace();
                let user = string(
                    process::format_process_status(
                        &snapshot,
                        &crate::namespace::initial_user(),
                        &view,
                    )
                    .unwrap(),
                );
                assert!(user.contains("Kthread:\t0\n"));
                assert!(!snapshot.has_mm);
                snapshot.kernel_thread = true;
                snapshot.state = b'I';
                snapshot.threads[0].state = b'I';
                snapshot.threads[0].has_mm = false;
                snapshot.threads[0].fd_size = 0;
                let status = string(
                    process::format_process_status(
                        &snapshot,
                        &crate::namespace::initial_user(),
                        &view,
                    )
                    .unwrap(),
                );
                assert!(status.contains("Kthread:\t1\n") && status.contains("State:\tI (idle)\n"));
                assert!(!status.contains("VmSize:"));
                let process_text = string(
                    process::format_process_stat(
                        &snapshot,
                        crate::namespace::TimeOffsets::ZERO,
                        &view,
                    )
                    .unwrap(),
                );
                let thread_text = string(
                    process::format_thread_stat(
                        &snapshot,
                        &snapshot.threads[0],
                        crate::namespace::TimeOffsets::ZERO,
                        &view,
                    )
                    .unwrap(),
                );
                assert_eq!(fields(&process_text)[6], "2097152");
                assert_eq!(fields(&thread_text)[6], "2097152");
                snapshot.kernel_thread = false;
                assert_eq!(
                    fields(&string(
                        process::format_process_stat(
                            &snapshot,
                            crate::namespace::TimeOffsets::ZERO,
                            &view
                        )
                        .unwrap()
                    ))[6],
                    "0"
                );
            }
            #[test]
            fn exec_retargeted_leader_fields_are_encoded_without_worker_fallback() {
                let (mut process, _registrations) = process_snapshot();
                process.has_mm = true;
                process.fd_size = 64;
                process.state = b'R';
                process.nice = 5;
                process.threads[0].tid = process.pid.clone();
                process.threads[0].start_time_us = process.start_time_us;
                let process_text = string(
                    process::format_process_stat(
                        &process,
                        crate::namespace::TimeOffsets::ZERO,
                        &process.pid.namespace(),
                    )
                    .unwrap(),
                );
                let thread_text = string(
                    process::format_thread_stat(
                        &process,
                        &process.threads[0],
                        crate::namespace::TimeOffsets::ZERO,
                        &process.pid.namespace(),
                    )
                    .unwrap(),
                );
                let (process_fields, thread_fields) = (fields(&process_text), fields(&thread_text));
                assert_eq!(process_fields[19], thread_fields[19]);
                assert_eq!(process_fields[20..22], thread_fields[20..22]);
                assert!(
                    string(
                        process::format_process_status(
                            &process,
                            &crate::namespace::initial_user(),
                            &process.pid.namespace()
                        )
                        .unwrap()
                    )
                    .contains("VmRSS:\t68 kB\n")
                );
            }
        }
    }
}
