use crate::{ext2_link_count, file_page_range, journal_layout, writeback_batch};

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
            FilePageRange::new(u64::MAX - 4095, 4096),
            Err(FilePageRangeError::Overflow)
        );
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
