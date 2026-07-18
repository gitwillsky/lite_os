#[cfg(test)]
extern crate alloc;

#[cfg(test)]
#[path = "../../../kernel/src/fs/file/indexed_slots.rs"]
mod indexed_slots;

#[cfg(test)]
#[path = "../../../kernel/src/fs/ext2/journal_layout.rs"]
mod journal_layout;

#[cfg(test)]
#[path = "../../../kernel/src/fs/page_cache/writeback_batch.rs"]
mod writeback_batch;

#[cfg(test)]
mod memory {
    pub(crate) const PAGE_SIZE: usize = 4096;
}

#[cfg(test)]
#[path = "../../../kernel/src/memory/mm/file_page_range.rs"]
mod file_page_range;

#[cfg(test)]
#[path = "../../../kernel/src/memory/mm/fault_preflight.rs"]
mod fault_preflight;

#[cfg(test)]
#[path = "../../../kernel/src/timer/deadline.rs"]
mod timer_deadline;

#[cfg(test)]
#[path = "../../../kernel/src/arch/riscv64/sv39.rs"]
mod sv39;

#[cfg(test)]
#[path = "../../../kernel/src/arch/riscv64/pte.rs"]
mod riscv_pte;

#[cfg(test)]
#[path = "../../../kernel/src/socket/unix/datagram_queue.rs"]
mod unix_datagram_queue;

#[cfg(test)]
#[path = "../../../kernel/src/syscall/user_iovec.rs"]
mod user_iovec;

#[cfg(test)]
#[path = "../../../kernel/src/socket/message_limits.rs"]
mod socket_message_limits;

#[cfg(test)]
#[path = "../../../kernel/src/fs/ext2/link_count.rs"]
mod ext2_link_count;

#[cfg(test)]
mod ext2_link_count_tests {
    use super::ext2_link_count::{
        LinkCountError, ParentLinkPlan, decrement, increment, plan_rename_parent_links,
    };

    #[test]
    fn ext2_link_transitions_enforce_the_fixed_limit_without_wrapping() {
        assert_eq!(increment(31_999), Ok(32_000));
        assert_eq!(increment(32_000), Err(LinkCountError::TooMany));
        assert_eq!(decrement(0), Err(LinkCountError::Corrupt));
        assert_eq!(decrement(1), Ok(0));
    }

    #[test]
    fn cross_parent_directory_rename_plans_the_net_parent_deltas() {
        assert_eq!(
            plan_rename_parent_links(7, 11, true, true, false),
            Ok(Some(ParentLinkPlan::CrossParent {
                old_parent: 6,
                new_parent: 12,
            }))
        );
        assert_eq!(
            plan_rename_parent_links(7, 32_000, true, true, false),
            Err(LinkCountError::TooMany)
        );
        assert_eq!(
            plan_rename_parent_links(7, 32_000, true, true, true),
            Ok(Some(ParentLinkPlan::CrossParent {
                old_parent: 6,
                new_parent: 32_000,
            }))
        );
    }

    #[test]
    fn rename_parent_plan_distinguishes_zero_delta_and_same_parent_replacement() {
        assert_eq!(
            plan_rename_parent_links(0, 32_000, true, false, false),
            Ok(None)
        );
        assert_eq!(
            plan_rename_parent_links(0, 32_000, false, true, false),
            Ok(None)
        );
        assert_eq!(
            plan_rename_parent_links(7, 7, true, false, true),
            Ok(Some(ParentLinkPlan::SameParent { parent: 6 }))
        );
    }

    #[test]
    fn repeated_ext2_link_plans_reach_but_never_cross_or_wrap_the_limit() {
        let mut count = 0;
        for expected in 1..=32_000 {
            count = increment(count).unwrap();
            assert_eq!(count, expected);
        }
        assert_eq!(increment(count), Err(LinkCountError::TooMany));
        for expected in (0..32_000).rev() {
            count = decrement(count).unwrap();
            assert_eq!(count, expected);
        }
        assert_eq!(decrement(count), Err(LinkCountError::Corrupt));
    }
}

#[cfg(test)]
mod timer_deadline_tests {
    use super::timer_deadline::next;

    #[test]
    fn first_deadline_starts_one_interval_after_now() {
        assert_eq!(next(0, 100, 25), Some(125));
    }

    #[test]
    fn delayed_handler_preserves_phase_and_skips_missed_ticks() {
        assert_eq!(next(100, 100, 25), Some(125));
        assert_eq!(next(100, 149, 25), Some(150));
        assert_eq!(next(100, 150, 25), Some(175));
    }

    #[test]
    fn future_deadline_is_not_reprogrammed() {
        assert_eq!(next(200, 150, 25), Some(200));
    }

    #[test]
    fn invalid_or_exhausted_deadline_is_rejected() {
        assert_eq!(next(100, 100, 0), None);
        assert_eq!(next(0, u64::MAX, 1), None);
    }
}

#[cfg(test)]
mod sv39_tests {
    use super::sv39::indexes;

    #[test]
    fn virtual_page_number_splits_into_three_nine_bit_indexes() {
        assert_eq!(indexes(0), [0, 0, 0]);
        assert_eq!(indexes(0x7fff_ffff), [0x1ff, 0x1ff, 0x1ff]);
        assert_eq!(indexes((3 << 18) | (7 << 9) | 11), [3, 7, 11]);
    }
}

#[cfg(test)]
mod socket_message_limit_tests {
    use super::socket_message_limits::{
        MessageProtocol, receive_capacity, stream_send_capacity, validate_send_length,
    };

    #[test]
    fn stream_and_atomic_protocols_select_distinct_message_limits() {
        assert!(validate_send_length(MessageProtocol::Stream, 3 * 1024 * 1024).is_ok());
        assert!(validate_send_length(MessageProtocol::UnixDatagram, 65_535).is_ok());
        assert!(validate_send_length(MessageProtocol::UnixDatagram, 65_536).is_err());
        assert!(validate_send_length(MessageProtocol::Ipv4Udp, 65_507).is_ok());
        assert!(validate_send_length(MessageProtocol::Ipv4Udp, 65_508).is_err());
        assert!(validate_send_length(MessageProtocol::Ipv4Raw, 65_515).is_ok());
        assert!(validate_send_length(MessageProtocol::Ipv4Packet, 1_501).is_err());
        assert!(validate_send_length(MessageProtocol::Unsupported, 1).is_err());
    }

    #[test]
    fn receive_capacity_bounds_storage_without_rejecting_large_capacity() {
        assert_eq!(
            receive_capacity(MessageProtocol::Stream, 1_000_000, 65_536),
            65_536
        );
        assert_eq!(
            receive_capacity(MessageProtocol::UnixDatagram, 1_000_000, 65_536),
            65_535
        );
        assert_eq!(
            receive_capacity(MessageProtocol::Ipv4Raw, 1_000_000, 65_536),
            65_535
        );
        assert_eq!(
            receive_capacity(MessageProtocol::Ipv4Packet, 1_000_000, 65_536),
            1_500
        );
        assert_eq!(
            receive_capacity(MessageProtocol::Unsupported, 1_000_000, 65_536),
            0
        );
    }

    #[test]
    fn stream_send_capacity_preserves_atomic_one_message_shape() {
        assert_eq!(
            stream_send_capacity(MessageProtocol::Stream, 65_537, 65_536),
            Some(65_536)
        );
        assert_eq!(
            stream_send_capacity(MessageProtocol::UnixDatagram, 65_535, 65_536),
            None
        );
    }
}

#[cfg(test)]
mod user_iovec_tests {
    use alloc::vec;

    use crate::memory;

    use super::user_iovec::{
        BufferError, IOV_MAX, ImportError, TotalLengthError, UserIoCursor, UserIoVec,
        bounded_staging_capacity, checked_total_length, import_iovecs_with, project_total_length,
        validate_user_buffers,
    };

    fn encode(vectors: &[UserIoVec]) -> alloc::vec::Vec<u8> {
        let mut bytes = alloc::vec::Vec::new();
        for vector in vectors {
            bytes.extend_from_slice(&vector.base.to_ne_bytes());
            bytes.extend_from_slice(&vector.length.to_ne_bytes());
        }
        bytes
    }

    #[test]
    fn raw_iovec_import_batches_pages_without_reordering_entries() {
        let expected = [
            UserIoVec {
                base: 0x1000,
                length: 3,
            },
            UserIoVec {
                base: 0x2000,
                length: 5,
            },
            UserIoVec {
                base: 0x3000,
                length: 7,
            },
        ];
        let source = encode(&expected);
        let array = memory::PAGE_SIZE - 8;
        let mut requests = vec![];

        let imported = import_iovecs_with(array, expected.len(), |address, output| {
            requests.push((address, output.len()));
            let offset = address - array;
            output.copy_from_slice(&source[offset..offset + output.len()]);
            Ok(())
        })
        .unwrap();

        assert_eq!(imported, expected);
        assert_eq!(requests, [(array, 16), (array + 16, 32)]);
    }

    #[test]
    fn raw_iovec_import_handles_zero_maximum_and_page_end_shapes() {
        let empty = import_iovecs_with(0, 0, |_, _| -> Result<(), ()> {
            panic!("zero iovecs must not touch userspace")
        })
        .unwrap();
        assert!(empty.is_empty());

        let expected = vec![UserIoVec { base: 1, length: 0 }; IOV_MAX];
        let source = encode(&expected);
        let array = memory::PAGE_SIZE - 32;
        let mut requests = vec![];
        let imported = import_iovecs_with(array, IOV_MAX, |address, output| {
            requests.push((address, output.len()));
            let offset = address - array;
            output.copy_from_slice(&source[offset..offset + output.len()]);
            Ok(())
        })
        .unwrap();

        assert_eq!(imported.len(), IOV_MAX);
        assert_eq!(imported[IOV_MAX - 1], expected[IOV_MAX - 1]);
        assert_eq!(requests[0], (array, 32));
        assert!(
            requests[1..]
                .iter()
                .all(|(_, bytes)| *bytes <= memory::PAGE_SIZE)
        );
    }

    #[test]
    fn raw_iovec_import_rejects_invalid_array_arithmetic_before_copy() {
        let mut copied = false;
        assert_eq!(
            import_iovecs_with(1, IOV_MAX + 1, |_, _| {
                copied = true;
                Ok(())
            }),
            Err(ImportError::TooMany)
        );
        assert_eq!(
            import_iovecs_with(0, 1, |_, _| {
                copied = true;
                Ok(())
            }),
            Err(ImportError::NullArray)
        );
        assert_eq!(
            import_iovecs_with(usize::MAX - 15, 1, |_, _| {
                copied = true;
                Ok(())
            }),
            Err(ImportError::AddressOverflow)
        );
        assert!(!copied);
    }

    #[test]
    fn buffer_and_total_policies_remain_caller_selected() {
        let vectors = [
            UserIoVec { base: 0, length: 0 },
            UserIoVec { base: 0, length: 1 },
        ];
        assert_eq!(validate_user_buffers(&vectors), Err(BufferError::NullBase));

        let large_stream = [UserIoVec {
            base: 1,
            length: 65_536,
        }];
        assert_eq!(
            checked_total_length(&large_stream, isize::MAX as usize),
            Ok(65_536)
        );
        assert_eq!(
            checked_total_length(&large_stream, 65_535),
            Err(TotalLengthError::Limit)
        );
        let overflow = [
            UserIoVec {
                base: 1,
                length: usize::MAX,
            },
            UserIoVec { base: 1, length: 1 },
        ];
        assert_eq!(
            checked_total_length(&overflow, usize::MAX),
            Err(TotalLengthError::Overflow)
        );
    }

    #[test]
    fn bounded_staging_plans_large_streams_without_request_sized_storage() {
        const STAGING: usize = 64 * 1024;
        assert_eq!(bounded_staging_capacity(0, STAGING), 0);
        assert_eq!(bounded_staging_capacity(65_536, STAGING), 65_536);
        assert_eq!(bounded_staging_capacity(65_537, STAGING), 65_536);

        let mut remaining = 3 * 1024 * 1024 + 17;
        let mut chunks = 0;
        while remaining != 0 {
            let chunk = bounded_staging_capacity(remaining, STAGING);
            assert!((1..=STAGING).contains(&chunk));
            remaining -= chunk;
            chunks += 1;
        }
        assert_eq!(chunks, 49);
    }

    #[test]
    fn caller_projects_scalar_and_multi_iovecs_to_max_rw_count() {
        const MAX_RW_COUNT: usize = 0x7fff_f000;
        let mut below = [UserIoVec {
            base: 1,
            length: MAX_RW_COUNT - 1,
        }];
        assert_eq!(
            project_total_length(&mut below, MAX_RW_COUNT),
            MAX_RW_COUNT - 1
        );
        assert_eq!(below[0].length, MAX_RW_COUNT - 1);

        let mut scalar = [UserIoVec {
            base: 1,
            length: MAX_RW_COUNT + 1,
        }];
        assert_eq!(
            project_total_length(&mut scalar, MAX_RW_COUNT),
            MAX_RW_COUNT
        );
        assert_eq!(scalar[0].length, MAX_RW_COUNT);

        let mut multiple = [
            UserIoVec {
                base: 1,
                length: MAX_RW_COUNT - 1,
            },
            UserIoVec { base: 2, length: 2 },
            UserIoVec { base: 3, length: 7 },
        ];
        assert_eq!(
            project_total_length(&mut multiple, MAX_RW_COUNT),
            MAX_RW_COUNT
        );
        assert_eq!(multiple[0].length, MAX_RW_COUNT - 1);
        assert_eq!(multiple[1].length, 1);
        assert_eq!(multiple[2].length, 0);
    }

    #[test]
    fn staged_cursor_commits_only_the_backend_consumed_prefix() {
        let vectors = [
            UserIoVec {
                base: 100,
                length: 3,
            },
            UserIoVec {
                base: 200,
                length: 5,
            },
        ];
        let mut cursor = UserIoCursor::new(&vectors);
        let mut bytes = [0u8; 8];
        let staged = cursor.stage_with(&mut bytes, |address, output| {
            if address >= 200 {
                return Err(());
            }
            output.fill(address as u8);
            Ok(())
        });
        assert_eq!((staged.count, staged.faulted), (3, true));
        assert_eq!(cursor.completed(), 0);

        cursor.advance(2);
        let staged = cursor.stage_with(&mut bytes, |address, output| {
            output.fill(address as u8);
            Ok(())
        });
        assert_eq!((staged.count, staged.faulted), (6, false));
        assert_eq!(bytes[0], 102);
        assert_eq!(&bytes[1..6], &[200; 5]);
        assert_eq!(cursor.completed(), 2);

        cursor.advance(staged.count);
        assert_eq!(cursor.completed(), 8);
    }
}

#[cfg(test)]
mod unix_datagram_queue_tests {
    use alloc::{sync::Arc, vec};

    use super::unix_datagram_queue::{
        DatagramQueue, MAX_DATAGRAMS, PushError, peer_identity_changed,
    };

    #[test]
    fn unix_datagram_queue_rejects_the_eleventh_message_without_consuming_it() {
        let mut queue = DatagramQueue::new();
        for value in 0..MAX_DATAGRAMS {
            assert!(queue.push(value).is_ok());
        }

        match queue.push(MAX_DATAGRAMS) {
            Err(PushError::Full(value)) => assert_eq!(value, MAX_DATAGRAMS),
            _ => panic!("the bounded queue accepted or lost its eleventh message"),
        }
        assert_eq!(queue.len(), MAX_DATAGRAMS);
    }

    #[test]
    fn unix_datagram_queue_preserves_fifo_and_wakes_only_when_leaving_full() {
        let mut queue = DatagramQueue::new();
        for value in 0..MAX_DATAGRAMS {
            assert!(queue.push(value).is_ok());
        }

        for expected in 0..MAX_DATAGRAMS {
            let (value, capacity_wake) = queue.pop().unwrap();
            assert_eq!(value, expected);
            assert_eq!(capacity_wake, expected == 0);
        }
        assert!(queue.is_empty());
        assert!(queue.pop().is_none());
    }

    #[test]
    fn unix_datagram_payload_size_does_not_change_slot_accounting() {
        let mut queue = DatagramQueue::new();
        queue.push(vec![]).ok().unwrap();
        queue.push(vec![0; 65_535]).ok().unwrap();

        assert_eq!(queue.len(), 2);
        assert_eq!(queue.pop().map(|(bytes, _)| bytes.len()), Some(0));
        assert_eq!(queue.pop().map(|(bytes, _)| bytes.len()), Some(65_535));
    }

    #[test]
    fn unix_datagram_repeated_fill_and_drain_keeps_the_logical_bound() {
        let mut queue = DatagramQueue::new();
        for cycle in 0..64 {
            for offset in 0..MAX_DATAGRAMS {
                queue.push(cycle * MAX_DATAGRAMS + offset).ok().unwrap();
            }
            assert!(queue.is_full());
            assert!(matches!(
                queue.push(usize::MAX),
                Err(PushError::Full(usize::MAX))
            ));
            for offset in 0..MAX_DATAGRAMS {
                let (value, capacity_wake) = queue.pop().unwrap();
                assert_eq!(value, cycle * MAX_DATAGRAMS + offset);
                assert_eq!(capacity_wake, offset == 0);
            }
            assert!(queue.is_empty());
        }
    }

    #[test]
    fn unix_datagram_peer_guard_tracks_identity_not_liveness() {
        let first = Arc::new(());
        let first_peer = Some(Arc::downgrade(&first));
        let captured = first_peer.clone();
        assert!(!peer_identity_changed(&first_peer, &captured));

        drop(first);
        assert!(!peer_identity_changed(&first_peer, &captured));

        let second = Arc::new(());
        let second_peer = Some(Arc::downgrade(&second));
        assert!(peer_identity_changed(&second_peer, &captured));
        assert!(peer_identity_changed(&None, &captured));
        assert!(peer_identity_changed(&second_peer, &None));
        assert!(!peer_identity_changed::<()>(&None, &None));
    }
}

#[cfg(test)]
mod file_page_range_tests {
    use super::file_page_range::{FilePageRange, FilePageRangeError};

    #[test]
    fn first_file_byte_maps_one_page() {
        let range = FilePageRange::new(0, 1).unwrap();

        assert_eq!(range.page(0), Some(0));
        assert_eq!(range.count(), 1);
    }

    #[test]
    fn linux_signed_file_ceiling_rejects_the_next_page() {
        let last = FilePageRange::new(0x7fff_ffff_ffff_e000, 4096).unwrap();

        assert_eq!(last.page(0), Some(0x7_ffff_ffff_fffe));
        assert_eq!(last.count(), 1);
        assert_eq!(
            FilePageRange::new(0x7fff_ffff_ffff_f000, 4096),
            Err(FilePageRangeError::Overflow)
        );
    }

    #[test]
    fn split_views_preserve_each_file_page_identity() {
        let original = FilePageRange::new(0x2000, 3 * 4096).unwrap();

        assert_eq!(original.subrange(0, 1).unwrap().page(0), Some(2));
        assert_eq!(original.subrange(1, 1).unwrap().page(0), Some(3));
        assert_eq!(original.subrange(2, 1).unwrap().page(0), Some(4));
        assert_eq!(original.subrange(3, 1), None);
    }

    #[test]
    fn only_a_page_start_below_eof_contains_file_bytes() {
        let range = FilePageRange::new(0, 2 * 4096).unwrap();

        assert_eq!(range.page(1), Some(1));
        assert_eq!(range.byte_offset(1), Some(4096));
        assert_eq!(range.has_file_bytes(1, 4097), Some(true));
        assert_eq!(range.has_file_bytes(1, 4096), Some(false));
        assert_eq!(range.has_file_bytes(2, u64::MAX), None);
    }

    #[test]
    fn byte_projections_and_truncate_prefix_share_the_validated_range() {
        let range = FilePageRange::new(0x2000, 3 * 4096).unwrap();

        assert_eq!(range.byte_range(), Some((0x2000, 3 * 4096)));
        assert_eq!(range.byte_within(1, 12), Some(0x300c));
        assert_eq!(range.byte_within(1, 4096), None);
        assert_eq!(range.prefix_before(0x2000), Some(0));
        assert_eq!(range.prefix_before(0x2001), Some(1));
        assert_eq!(range.prefix_before(0x4001), Some(3));
    }

    #[test]
    fn truncate_projection_keeps_the_original_origin_after_vma_split() {
        let range = FilePageRange::new(0, 3 * 4096).unwrap();

        assert_eq!(range.stale_resident_start(100, 100, 4096), Some(101));
        assert_eq!(range.stale_resident_start(100, 101, 4096), Some(101));
        assert_eq!(range.stale_resident_start(100, 102, 4096), Some(102));
    }

    #[test]
    fn invalid_and_unrepresentable_file_ranges_are_rejected() {
        assert_eq!(FilePageRange::new(0, 0), Err(FilePageRangeError::Invalid));
        assert_eq!(
            FilePageRange::new(1, 4096),
            Err(FilePageRangeError::Invalid)
        );
        assert_eq!(
            FilePageRange::new(0, 1usize << 63),
            Err(FilePageRangeError::Overflow)
        );
        assert_eq!(
            FilePageRange::new(0, usize::MAX),
            Err(FilePageRangeError::Overflow)
        );
        assert_eq!(
            FilePageRange::new(u64::MAX & !4095, 4096),
            Err(FilePageRangeError::Overflow)
        );
    }
}

#[cfg(test)]
mod fault_preflight_tests {
    use super::fault_preflight::{
        FaultAccess, FaultPermissions, FaultPreflight, FaultResidency, FileFaultState,
        preflight_fault,
    };

    #[test]
    fn unmapped_fault_does_not_inspect_file_state() {
        let outcome = preflight_fault(
            false,
            FaultPermissions::new(true, true, true, true),
            FaultAccess::Read,
            || -> Result<FileFaultState, ()> {
                panic!("an unmapped fault must not project a file page")
            },
            || FaultResidency::Private {
                lazy: true,
                resident: false,
            },
        )
        .unwrap();

        assert_eq!(outcome, FaultPreflight::SegmentationFault);
    }

    #[test]
    fn denied_access_never_requests_a_private_frame() {
        let cases = [
            (
                FaultPermissions::new(true, false, false, false),
                FaultAccess::Read,
            ),
            (
                FaultPermissions::new(true, true, false, false),
                FaultAccess::Write,
            ),
            (
                FaultPermissions::new(true, true, true, false),
                FaultAccess::Execute,
            ),
        ];

        for (permissions, access) in cases {
            assert_eq!(
                preflight_fault(
                    true,
                    permissions,
                    access,
                    || -> Result<FileFaultState, ()> {
                        panic!("a denied fault must not project a file page")
                    },
                    || FaultResidency::Private {
                        lazy: true,
                        resident: false,
                    },
                )
                .unwrap(),
                FaultPreflight::SegmentationFault
            );
        }
    }

    #[test]
    fn private_file_eof_precedes_residency_allocation() {
        let permissions = FaultPermissions::new(true, true, false, false);
        let residency = FaultResidency::Private {
            lazy: true,
            resident: false,
        };

        assert_eq!(
            preflight_fault(
                true,
                permissions,
                FaultAccess::Read,
                || Ok::<_, ()>(FileFaultState::BeyondEof),
                || -> FaultResidency { panic!("EOF classification must precede residency lookup") },
            )
            .unwrap(),
            FaultPreflight::BusError
        );
        assert_eq!(
            preflight_fault(
                true,
                permissions,
                FaultAccess::Read,
                || Ok::<_, ()>(FileFaultState::Available),
                || residency,
            )
            .unwrap(),
            FaultPreflight::NeedsPrivateFrame
        );
    }

    #[test]
    fn residency_owner_selects_the_fault_path() {
        let permissions = FaultPermissions::new(true, true, true, false);
        let cases = [
            (FaultResidency::Device, FaultPreflight::Device),
            (
                FaultResidency::SharedAnonymous,
                FaultPreflight::SharedAnonymous,
            ),
            (FaultResidency::SharedFile, FaultPreflight::SharedFile),
            (
                FaultResidency::Private {
                    lazy: true,
                    resident: true,
                },
                FaultPreflight::Private,
            ),
            (
                FaultResidency::Private {
                    lazy: false,
                    resident: false,
                },
                FaultPreflight::Private,
            ),
        ];

        for (residency, expected) in cases {
            assert_eq!(
                preflight_fault(
                    true,
                    permissions,
                    FaultAccess::Read,
                    || Ok::<_, ()>(FileFaultState::NotFile),
                    || residency,
                )
                .unwrap(),
                expected
            );
        }
    }
}

#[cfg(test)]
mod writeback_tests {
    use alloc::vec::Vec;

    use super::{journal_layout::JournalLayout, writeback_batch::commit_with_backoff};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Error {
        Capacity,
        Io,
    }

    #[test]
    fn fitting_page_batch_commits_and_publishes_once() {
        let entries: Vec<_> = (0..32).collect();
        let mut commits = 0;
        let mut published = Vec::new();

        commit_with_backoff(
            &entries,
            |chunk| {
                commits += 1;
                assert_eq!(chunk, entries);
                Ok::<_, Error>(())
            },
            |chunk| published.extend_from_slice(chunk),
            |_| false,
        )
        .unwrap();

        assert_eq!(commits, 1);
        assert_eq!(published, entries);
    }

    #[test]
    fn capacity_backoff_preserves_order_and_commits_each_item_once() {
        let entries: Vec<_> = (0..32).collect();
        let mut attempts = 0;
        let mut commits = 0;
        let mut written = Vec::new();
        let mut published = Vec::new();

        commit_with_backoff(
            &entries,
            |chunk| {
                attempts += 1;
                if chunk.len() > 5 {
                    return Err(Error::Capacity);
                }
                commits += 1;
                written.extend_from_slice(chunk);
                Ok(())
            },
            |chunk| published.extend_from_slice(chunk),
            |error| *error == Error::Capacity,
        )
        .unwrap();

        assert_eq!(attempts, 11);
        assert_eq!(commits, 8);
        assert_eq!(written, entries);
        assert_eq!(published, entries);
    }

    #[test]
    fn later_failure_publishes_only_the_committed_prefix() {
        let entries: Vec<_> = (0..8).collect();
        let mut published = Vec::new();
        let result = commit_with_backoff(
            &entries,
            |chunk| {
                if chunk.len() > 4 {
                    Err(Error::Capacity)
                } else if chunk[0] >= 4 {
                    Err(Error::Io)
                } else {
                    Ok(())
                }
            },
            |chunk| published.extend_from_slice(chunk),
            |error| *error == Error::Capacity,
        );

        assert_eq!(result, Err(Error::Io));
        assert_eq!(published, [0, 1, 2, 3]);
    }

    #[test]
    fn single_item_capacity_exhaustion_propagates() {
        let result = commit_with_backoff(
            &[7],
            |_| Err(Error::Capacity),
            |_| panic!("failed item must not publish"),
            |error| *error == Error::Capacity,
        );

        assert_eq!(result, Err(Error::Capacity));
    }

    #[test]
    fn journal_layout_matches_descriptor_equation_at_boundaries() {
        for block_size in [1024, 2048, 4096] {
            let tags = 1 + (block_size - 36) / 8;
            for journal_blocks in 0..=(2 * tags + 8) {
                let expected = (1..journal_blocks)
                    .filter(|writes| {
                        let writes: usize = *writes;
                        1 + writes + writes.div_ceil(tags) < journal_blocks
                    })
                    .max();
                let layout = JournalLayout::new(journal_blocks, block_size);
                assert_eq!(layout.map(JournalLayout::write_capacity), expected);
                if let Some(layout) = layout {
                    assert_eq!(layout.tags_per_descriptor(), tags);
                }
            }
        }
    }
}
