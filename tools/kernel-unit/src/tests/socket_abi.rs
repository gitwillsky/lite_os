use crate::{socket_message_limits, unix_datagram_queue, user_iovec};

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
        assert!(validate_send_length(MessageProtocol::NetlinkUevent, 65_535).is_ok());
        assert!(validate_send_length(MessageProtocol::NetlinkUevent, 65_536).is_err());
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
    fn unix_datagram_take_all_detaches_the_complete_fifo() {
        let mut queue = DatagramQueue::new();
        for value in 0..MAX_DATAGRAMS {
            queue.push(value).ok().unwrap();
        }

        let detached = queue.take_all();

        assert!(queue.is_empty());
        assert_eq!(
            detached.into_iter().collect::<vec::Vec<_>>(),
            (0..MAX_DATAGRAMS).collect::<vec::Vec<_>>()
        );
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
