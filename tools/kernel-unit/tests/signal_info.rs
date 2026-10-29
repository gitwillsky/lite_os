#![feature(allocator_ext)]
extern crate alloc;
#[path = "../src/allocation_failure.rs"]
mod allocation_failure;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
#[allow(dead_code, reason = "本用例只覆盖实际 signal queue 采用的 AVL API")]
mod fallible_tree;
#[path = "../src/namespace_fixture.rs"]
mod namespace;

#[path = "."]
pub(crate) mod execution {
    #[path = "../../../kernel/src/execution/task/model"]
    pub(crate) mod task {
        // 生产 siginfo codec 的 layout 回归，覆盖已有 union 与新 seccomp payload。

        #[path = "signal_info.rs"]
        #[allow(dead_code, reason = "其他信号来源由 runtime 回归覆盖")]
        mod signal_info;
        use signal_info::PendingSignal;
        use signal_pending::PreparedSignalRecord;
        #[path = "signal_pending.rs"]
        #[allow(
            dead_code,
            reason = "本用例验证实际 pending 投影，timer 回收由专用集成回归覆盖"
        )]
        mod signal_pending;

        #[test]
        fn signalfd_native_layouts_zero_padding_and_do_not_mix_union_fields() {
            let timer = PendingSignal::timer(4, 7, 0x1122_3344_ffff_ff81).encode_signalfd(35);
            assert_eq!(&timer[24..28], &4u32.to_ne_bytes());
            assert_eq!(&timer[32..36], &7u32.to_ne_bytes());
            assert_eq!(&timer[44..48], &0xffff_ff81u32.to_ne_bytes());
            assert_eq!(&timer[48..56], &0x1122_3344_ffff_ff81u64.to_ne_bytes());
            assert!(
                timer[12..24]
                    .iter()
                    .chain(&timer[56..])
                    .all(|byte| *byte == 0)
            );
            let child = PendingSignal::child_exited(9, 23)
                .with_identity(9, 65534, false)
                .encode_signalfd(17);
            assert_eq!(&child[12..20], &[9, 0, 0, 0, 254, 255, 0, 0]);
            assert_eq!(&child[40..44], &23i32.to_ne_bytes());
            assert!(child[44..].iter().all(|byte| *byte == 0));
            let fault = PendingSignal::synchronous_fault(2, 0xffff_ff00_1234).encode_signalfd(11);
            assert_eq!(&fault[72..80], &0xffff_ff00_1234u64.to_ne_bytes());
            assert!(
                fault[12..72]
                    .iter()
                    .chain(&fault[80..])
                    .all(|byte| *byte == 0)
            );
            let seccomp =
                PendingSignal::seccomp(6, 63, 0xc000_00b7, 0x12345678).encode_signalfd(31);
            assert_eq!(&seccomp[4..8], &6i32.to_ne_bytes());
            assert_eq!(&seccomp[84..88], &63i32.to_ne_bytes());
            assert_eq!(&seccomp[88..96], &0x12345678u64.to_ne_bytes());
            assert_eq!(&seccomp[96..100], &0xc000_00b7u32.to_ne_bytes());
            assert!(
                seccomp[12..84]
                    .iter()
                    .chain(&seccomp[100..])
                    .all(|byte| *byte == 0)
            );
        }

        #[test]
        fn injected_signalfd_poll_mce_and_rt_follow_linux_layout_classification() {
            let mut native = [0xa5; 48];
            native[8..12].copy_from_slice(&(-1i32).to_ne_bytes());
            let rt = PendingSignal::from_user_native(&native).encode_signalfd(35);
            assert_eq!(&rt[12..20], &native[16..24]);
            assert_eq!(&rt[44..48], &native[24..28]);
            assert_eq!(&rt[48..56], &native[24..32]);
            assert!(rt[20..44].iter().chain(&rt[56..]).all(|byte| *byte == 0));
            for (signal, code) in [(29, 2i32), (10, 6), (31, 6), (35, -5)] {
                native[8..12].copy_from_slice(&code.to_ne_bytes());
                let poll = PendingSignal::from_user_native(&native).encode_signalfd(signal);
                assert_eq!(&poll[28..32], &native[16..20]);
                assert_eq!(&poll[20..24], &native[24..28]);
                assert!(
                    poll[12..20]
                        .iter()
                        .chain(&poll[32..])
                        .all(|byte| *byte == 0)
                );
            }
            native[8..12].copy_from_slice(&4i32.to_ne_bytes());
            let mce = PendingSignal::from_user_native(&native).encode_signalfd(7);
            assert_eq!(&mce[72..80], &native[16..24]);
            assert_eq!(&mce[80..82], &native[24..26]);
            assert!(mce[12..72].iter().chain(&mce[82..]).all(|byte| *byte == 0));
        }

        #[test]
        fn seccomp_payload_has_standard_sigsys_fields_and_no_uninitialized_bytes() {
            let bytes = PendingSignal::seccomp(42, 0xdead_beef_ffff_ffff, 0xc000_00b7, 0x1234_5678)
                .encode(31);
            assert_eq!(i32::from_ne_bytes(bytes[..4].try_into().unwrap()), 31);
            assert_eq!(i32::from_ne_bytes(bytes[4..8].try_into().unwrap()), 42);
            assert_eq!(i32::from_ne_bytes(bytes[8..12].try_into().unwrap()), 1);
            assert_eq!(
                u64::from_ne_bytes(bytes[16..24].try_into().unwrap()),
                0x1234_5678
            );
            assert_eq!(i32::from_ne_bytes(bytes[24..28].try_into().unwrap()), -1);
            assert_eq!(
                u32::from_ne_bytes(bytes[28..32].try_into().unwrap()),
                0xc000_00b7
            );
            assert!(
                bytes[12..16]
                    .iter()
                    .chain(&bytes[32..])
                    .all(|byte| *byte == 0)
            );
            assert!(PendingSignal::seccomp(0, 0, 0, 0).is_forced_fault());
        }

        #[test]
        fn identity_fault_and_timer_layouts_are_preserved() {
            for source in [
                PendingSignal::default(),
                PendingSignal::thread_directed(12),
                PendingSignal::process_directed(12),
                PendingSignal::child_exited(12, 3),
                PendingSignal::child_killed(12, 9),
                PendingSignal::child_stopped(12, 19),
                PendingSignal::child_continued(12),
                PendingSignal::kernel(),
            ] {
                assert!(!source.is_forced_fault());
                assert!(source.encode(17)[28..].iter().all(|byte| *byte == 0));
            }
            let child = PendingSignal::child_exited(12, 3).encode(17);
            assert_eq!(&child[16..20], &12i32.to_ne_bytes());
            assert_eq!(&child[24..28], &3i32.to_ne_bytes());
            let fault = PendingSignal::synchronous_fault(2, 0x1234).encode(11);
            assert_eq!(&fault[16..24], &0x1234u64.to_ne_bytes());
            let timer = PendingSignal::timer(4, 7, u64::MAX).encode(14);
            assert_eq!(&timer[16..20], &4i32.to_ne_bytes());
            assert_eq!(&timer[20..24], &7i32.to_ne_bytes());
            assert_eq!(&timer[24..32], &u64::MAX.to_ne_bytes());
        }

        #[test]
        fn recipient_identity_is_fixed_at_generation_and_force_does_not_become_a_fault() {
            let source = PendingSignal::child_exited(125, 7).with_identity(1, 65534, false);
            let bytes = source.encode(17);
            assert_eq!(&bytes[16..20], &1i32.to_ne_bytes());
            assert_eq!(&bytes[20..24], &65534u32.to_ne_bytes());
            assert_eq!(&bytes[24..28], &7i32.to_ne_bytes());
            assert!(!source.is_forced_fault());
            let ancestor = PendingSignal::process_directed(125).with_identity(0, 42, true);
            assert!(!ancestor.is_forced_fault());
            assert_eq!(&ancestor.encode(9)[16..24], &[0, 0, 0, 0, 42, 0, 0, 0]);
        }

        #[test]
        fn coalesced_synchronous_fault_retains_first_identity_but_forces_default_delivery() {
            crate::namespace::boot();
            let mut pending = signal_pending::PendingSignals::new();
            let first = PendingSignal::process_directed(42).with_identity(42, 7, false);
            pending.queue(11, first, &mut prepare(11, 591325, u64::MAX));
            pending.queue(
                11,
                PendingSignal::synchronous_fault(2, 0xdead),
                &mut PreparedSignalRecord::default(),
            );
            pending.queue(
                11,
                PendingSignal::kernel(),
                &mut PreparedSignalRecord::default(),
            );
            let (_, delivered) = pending.take(u64::MAX).unwrap().into_parts();
            assert_eq!(delivered.encode(11), first.encode(11));
            assert!(delivered.bypasses_init(11));
            let mut pending = signal_pending::PendingSignals::new();
            pending.queue(
                15,
                PendingSignal::kernel(),
                &mut PreparedSignalRecord::default(),
            );
            let (_, delivered) = pending.take(u64::MAX).unwrap().into_parts();
            assert!(!delivered.bypasses_init(15));
            assert!(delivered.bypasses_init(9));
        }
        fn queued(value: u64) -> PendingSignal {
            let mut bytes = [0u8; 48];
            bytes[8..12].copy_from_slice(&(-1i32).to_ne_bytes());
            bytes[16..20].copy_from_slice(&42i32.to_ne_bytes());
            bytes[20..24].copy_from_slice(&7u32.to_ne_bytes());
            bytes[24..32].copy_from_slice(&value.to_ne_bytes());
            PendingSignal::from_user_native(&bytes)
        }
        fn prepare(signal: usize, uid: u32, limit: u64) -> PreparedSignalRecord {
            PreparedSignalRecord::prepare(
                signal,
                queued(0),
                crate::namespace::initial_user(),
                uid,
                limit,
            )
        }
        fn assert_full(uid: u32, limit: u64) {
            assert!(matches!(
                crate::namespace::RlimitCharge::acquire(
                    crate::namespace::initial_user(),
                    uid,
                    crate::namespace::RlimitResource::PendingSignals,
                    1,
                    limit,
                    false
                ),
                Err(crate::namespace::NamespaceError::QuotaLimit)
            ));
        }
        #[test]
        fn realtime_same_number_is_fifo_and_lowest_number_precedes_other_realtime_signals() {
            crate::namespace::boot();
            let uid = 591301;
            let mut pending = signal_pending::PendingSignals::new();
            for (signal, value) in [(35, 10), (35, 11), (34, 9), (35, 12)] {
                pending.queue(signal, queued(value), &mut prepare(signal, uid, 4));
            }
            assert_full(uid, 4);
            pending.queue(10, queued(7), &mut prepare(10, 591326, 1));
            pending.queue(10, queued(8), &mut PreparedSignalRecord::default());
            for (signal, value) in [(10, 7u64), (34, 9), (35, 10), (35, 11), (35, 12)] {
                let owned = pending.take(u64::MAX).unwrap();
                let (number, info) = owned.into_parts();
                assert_eq!(number, signal);
                assert_eq!(&info.encode(signal)[24..32], &value.to_ne_bytes());
            }
            assert_eq!(pending.bits, 0);
            let all = crate::namespace::RlimitCharge::acquire(
                crate::namespace::initial_user(),
                uid,
                crate::namespace::RlimitResource::PendingSignals,
                4,
                4,
                false,
            )
            .unwrap();
            drop(all);
        }
        #[test]
        fn consumed_record_and_ignore_retirement_hold_charge_until_actual_owned_record_drop() {
            crate::namespace::boot();
            let uid = 591302;
            let mut pending = signal_pending::PendingSignals::new();
            for value in 0..3 {
                pending.queue(35, queued(value), &mut prepare(35, uid, 3));
            }
            let selected = pending.take(u64::MAX).unwrap();
            let retired = pending.discard(1 << 34);
            assert_eq!(pending.bits, 0);
            assert_full(uid, 3);
            drop(selected);
            let one = prepare(35, uid, 3);
            assert!(!one.unavailable_for(35, queued(4)));
            assert_full(uid, 3);
            drop((one, retired));
            for _ in 0..1024 {
                pending.queue(35, queued(1), &mut prepare(35, uid, u64::MAX));
            }
            // 大链递归析构会耗尽 kernel stack；实际 retirement 逐个 take next。
            drop(pending.discard(u64::MAX));
            let all = crate::namespace::RlimitCharge::acquire(
                crate::namespace::initial_user(),
                uid,
                crate::namespace::RlimitResource::PendingSignals,
                3,
                3,
                false,
            )
            .unwrap();
            drop(all);
        }
        #[test]
        fn realtime_preparation_oom_unwinds_charge_and_kill_fallback_does_not_fabricate_extra_record()
         {
            crate::namespace::boot();
            for index in 0..3 {
                let uid = 591310 + index as u32;
                crate::allocation_failure::fail_after(index);
                let mut prepared = prepare(35, uid, 1);
                assert!(crate::allocation_failure::was_consumed());
                assert!(prepared.unavailable_for(35, queued(1)));
                assert!(!prepared.unavailable_for(35, PendingSignal::process_directed(42)));
                let mut pending = signal_pending::PendingSignals::new();
                pending.queue(35, PendingSignal::process_directed(42), &mut prepared);
                let (_, fallback) = pending.take(u64::MAX).unwrap().into_parts();
                assert_eq!(fallback.encode(35), PendingSignal::default().encode(35));
                let full = prepare(35, uid, 1);
                assert!(!full.unavailable_for(35, queued(2)));
                drop(full);
                pending.queue(35, queued(3), &mut prepare(35, uid, 1));
                let mut unavailable = prepare(35, uid, 1);
                pending.queue(35, PendingSignal::process_directed(42), &mut unavailable);
                let (_, only) = pending.take(u64::MAX).unwrap().into_parts();
                assert_eq!(only.encode(35), queued(3).encode(35));
                assert!(pending.take(u64::MAX).is_none());
            }
        }
        #[test]
        fn user_native_payload_retains_errno_alignment_and_union_but_never_expansion_or_forced_fault()
         {
            let mut bytes = [0xa5u8; 48];
            bytes[8..12].copy_from_slice(&(-1i32).to_ne_bytes());
            let info = PendingSignal::from_user_native(&bytes);
            let encoded = info.encode(35);
            assert_eq!(&encoded[4..48], &bytes[4..48]);
            assert!(encoded[48..].iter().all(|byte| *byte == 0));
            assert!(!info.is_forced_fault());
            assert!(info.known_user_layout(35));
            assert!(!info.restricted_user_source());
            let projected = info.project_user_identity(35, 65534, true).encode(35);
            assert_eq!(&projected[16..20], &[0; 4]);
            assert_eq!(&projected[20..24], &65534u32.to_ne_bytes());
            assert_eq!(&projected[24..48], &bytes[24..48]);
            bytes[8..12].copy_from_slice(&128i32.to_ne_bytes());
            let user_kernel = PendingSignal::from_user_native(&bytes);
            assert!(user_kernel.restricted_user_source());
            assert!(!user_kernel.allows_queue_fallback());
            bytes[8..12].copy_from_slice(&(-200i32).to_ne_bytes());
            assert!(!PendingSignal::from_user_native(&bytes).known_user_layout(35));
            for (signal, maximum) in [
                (4, 11i32),
                (8, 15),
                (11, 10),
                (7, 5),
                (5, 6),
                (17, 6),
                (29, 6),
                (31, 2),
                (35, 6),
            ] {
                bytes[8..12].copy_from_slice(&maximum.to_ne_bytes());
                assert!(PendingSignal::from_user_native(&bytes).known_user_layout(signal));
                bytes[8..12].copy_from_slice(&(maximum + 1).to_ne_bytes());
                assert!(!PendingSignal::from_user_native(&bytes).known_user_layout(signal));
            }
        }
        #[test]
        fn posix_preallocated_node_is_reused_and_queued_notification_pins_original_charge() {
            crate::namespace::boot();
            let uid = 591320;
            let owner = signal_pending::PreallocatedSignal::try_new(
                crate::namespace::initial_user(),
                uid,
                1,
                (82, 3),
            )
            .unwrap();
            assert_full(uid, 1);
            let mut pending = signal_pending::PendingSignals::new();
            for sequence in 1..=1024 {
                let mut prepared = owner.prepare(sequence);
                assert!(prepared.has_record());
                pending.queue(35, PendingSignal::timer(3, 0, 17), &mut prepared);
                assert!(!owner.prepare(sequence).has_record());
                assert!(
                    pending
                        .take(1 << 34)
                        .unwrap()
                        .resolve(|event, info| {
                            assert!(alloc::sync::Arc::ptr_eq(&event.owner, &owner));
                            assert_eq!(event.sequence, sequence);
                            assert_eq!(event.key, (82, 3));
                            info.set_timer_overrun(9);
                            true
                        })
                        .is_some_and(|(_, info)| info.encode(35)[20..24] == 9i32.to_ne_bytes())
                );
                assert_eq!(pending.bits, 0);
                assert_full(uid, 1);
            }
            let mut prepared = owner.prepare(1025);
            pending.queue(35, PendingSignal::timer(3, 0, 17), &mut prepared);
            drop(owner);
            assert_full(uid, 1);
            let retired = pending.discard(u64::MAX);
            assert_full(uid, 1);
            drop(retired);
            let replacement = signal_pending::PreallocatedSignal::try_new(
                crate::namespace::initial_user(),
                uid,
                1,
                (82, 3),
            )
            .unwrap();
            drop(replacement);
        }
        #[test]
        fn distinct_standard_signal_timers_use_same_fifo_without_legacy_coalescing() {
            crate::namespace::boot();
            let uid = 591321;
            let first = signal_pending::PreallocatedSignal::try_new(
                crate::namespace::initial_user(),
                uid,
                2,
                (83, 0),
            )
            .unwrap();
            let second = signal_pending::PreallocatedSignal::try_new(
                crate::namespace::initial_user(),
                uid,
                2,
                (83, 1),
            )
            .unwrap();
            let mut pending = signal_pending::PendingSignals::new();
            pending.queue(14, PendingSignal::timer(0, 0, 10), &mut first.prepare(5));
            pending.queue(14, PendingSignal::timer(1, 0, 20), &mut second.prepare(8));
            // 新 expiry 只刷新原 timer 的 sequence，不排入第二个 record。
            assert!(!first.prepare(6).has_record());
            for (id, sequence, value) in [(0, 6, 10u64), (1, 8, 20)] {
                let (signal, info) = pending
                    .take(u64::MAX)
                    .unwrap()
                    .resolve(|event, _| {
                        assert_eq!(event.key, (83, id));
                        assert_eq!(event.sequence, sequence);
                        true
                    })
                    .unwrap();
                assert_eq!(signal, 14);
                assert_eq!(&info.encode(signal)[24..32], &value.to_ne_bytes());
            }
            assert_eq!(pending.bits, 0);
            drop(first);
            drop(second);
        }
        #[test]
        fn preallocated_creation_oom_rolls_back_every_charge_and_unpublished_retarget_keeps_node() {
            crate::namespace::boot();
            let uid = 591322;
            for successful_allocations in 0..4 {
                crate::allocation_failure::fail_after(successful_allocations);
                let result = signal_pending::PreallocatedSignal::try_new(
                    crate::namespace::initial_user(),
                    uid,
                    1,
                    (84, 0),
                );
                let consumed = crate::allocation_failure::was_consumed();
                assert!(consumed && result.is_err());
                drop(result);
                let probe = signal_pending::PreallocatedSignal::try_new(
                    crate::namespace::initial_user(),
                    uid,
                    1,
                    (84, 0),
                )
                .unwrap();
                drop(probe);
            }
            let owner = signal_pending::PreallocatedSignal::try_new(
                crate::namespace::initial_user(),
                uid,
                1,
                (84, 0),
            )
            .unwrap();
            owner.retarget((84, 9));
            crate::allocation_failure::fail_next();
            let mut pending = signal_pending::PendingSignals::new();
            pending.queue(35, PendingSignal::timer(9, 0, 1), &mut owner.prepare(2));
            let event = pending.take(u64::MAX).unwrap().resolve(|event, _| {
                assert_eq!(event.key, (84, 9));
                true
            });
            assert!(event.is_some());
            assert!(!crate::allocation_failure::was_consumed());
            drop(owner);
        }
        #[test]
        fn exec_timer_flush_preserves_same_signal_user_record_and_refunds_deleted_timer() {
            crate::namespace::boot();
            let uid = 591323;
            let owner = signal_pending::PreallocatedSignal::try_new(
                crate::namespace::initial_user(),
                uid,
                2,
                (85, 0),
            )
            .unwrap();
            let mut pending = signal_pending::PendingSignals::new();
            pending.queue(35, PendingSignal::timer(0, 0, 10), &mut owner.prepare(1));
            pending.queue(35, queued(20), &mut prepare(35, uid, 2));
            drop(owner);
            assert_full(uid, 2);
            let retired = pending.discard_timer_notifications();
            assert_ne!(pending.bits, 0);
            assert_full(uid, 2);
            drop(retired);
            let probe = prepare(35, uid, 2);
            assert!(!probe.unavailable_for(35, queued(30)));
            drop(probe);
            let (_, info) = pending.take(u64::MAX).unwrap().into_parts();
            assert_eq!(&info.encode(35)[24..32], &20u64.to_ne_bytes());
            assert_eq!(pending.bits, 0);
        }
        fn prepare_source(
            signal: usize,
            info: PendingSignal,
            uid: u32,
            limit: u64,
        ) -> PreparedSignalRecord {
            PreparedSignalRecord::prepare(
                signal,
                info,
                crate::namespace::initial_user(),
                uid,
                limit,
            )
        }
        #[test]
        fn ordinary_and_realtime_share_native_quota_and_ordinary_coalescing_keeps_first_record() {
            crate::namespace::boot();
            let uid = 591330;
            let mut pending = signal_pending::PendingSignals::new();
            pending.queue(10, queued(7), &mut prepare_source(10, queued(7), uid, 1));
            assert_full(uid, 1);
            assert!(prepare(35, uid, 1).unavailable_for(35, queued(8)));
            pending.queue(10, queued(9), &mut prepare_source(10, queued(9), uid, 1));
            let (_, first) = pending.take(u64::MAX).unwrap().into_parts();
            assert_eq!(&first.encode(10)[24..32], &7u64.to_ne_bytes());
            assert_eq!(pending.bits, 0);
            let available = prepare(35, uid, 1);
            assert!(!available.unavailable_for(35, queued(8)));
            drop(available);
            // ordinary SI_USER/nonnegative source 仍收费，但不受当前或祖先 soft max 阻挡。
            let kill = PendingSignal::process_directed(42);
            pending.queue(10, kill, &mut prepare_source(10, kill, uid, 0));
            pending.queue(12, kill, &mut prepare_source(12, kill, uid, 0));
            assert_full(uid, 2);
            for signal in [10, 12] {
                assert_eq!(
                    pending
                        .take(u64::MAX)
                        .unwrap()
                        .into_parts()
                        .1
                        .encode(signal),
                    kill.encode(signal)
                );
            }
            assert_eq!(pending.bits, 0);
        }
        #[test]
        fn standard_queue_failure_is_native_bit_only_and_forced_fault_consequence_survives() {
            crate::namespace::boot();
            let mut pending = signal_pending::PendingSignals::new();
            let mut unavailable = prepare_source(10, queued(7), 591331, 0);
            assert!(!unavailable.has_record());
            assert!(!unavailable.unavailable_for(10, queued(7)));
            pending.queue(10, queued(7), &mut unavailable);
            let (_, info) = pending.take(u64::MAX).unwrap().into_parts();
            assert_eq!(info.encode(10), PendingSignal::default().encode(10));
            let fault = PendingSignal::synchronous_fault(2, 0xdead);
            crate::allocation_failure::fail_next();
            let mut failed = prepare_source(11, fault, 591331, 0);
            assert!(crate::allocation_failure::was_consumed());
            pending.queue(11, fault, &mut failed);
            let (_, info) = pending.take(u64::MAX).unwrap().into_parts();
            assert_eq!(info.encode(11), PendingSignal::default().encode(11));
            assert!(info.bypasses_init(11));
        }
        #[test]
        fn ordinary_first_payload_and_each_same_number_posix_timer_keep_native_fifo_order() {
            crate::namespace::boot();
            let uid = 591332;
            let first = signal_pending::PreallocatedSignal::try_new(
                crate::namespace::initial_user(),
                uid,
                3,
                (86, 0),
            )
            .unwrap();
            let second = signal_pending::PreallocatedSignal::try_new(
                crate::namespace::initial_user(),
                uid,
                3,
                (86, 1),
            )
            .unwrap();
            let mut pending = signal_pending::PendingSignals::new();
            pending.queue(14, queued(1), &mut prepare_source(14, queued(1), uid, 3));
            pending.queue(14, PendingSignal::timer(0, 0, 2), &mut first.prepare(1));
            pending.queue(14, PendingSignal::timer(1, 0, 3), &mut second.prepare(1));
            // 已 pending 的普通 queue 仍 coalesce，timer exception 不允许改变普通 kill 规则。
            pending.queue(14, queued(4), &mut prepare_source(14, queued(4), uid, 3));
            for value in 1..=3u64 {
                let (_, info) = pending
                    .take(u64::MAX)
                    .unwrap()
                    .resolve(|_, _| true)
                    .unwrap();
                assert_eq!(&info.encode(14)[24..32], &value.to_ne_bytes());
            }
            assert_eq!(pending.bits, 0);
            assert!(core::mem::size_of::<signal_pending::PendingSignals>() <= 1152);
            drop(first);
            drop(second);
        }
    }
}
