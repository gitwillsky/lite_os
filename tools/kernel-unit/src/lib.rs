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
