//! Exercise the production retirement owner with explicit page-table and resource-drop seams.
//! The fixture does not implement retirement ordering or file lifetime policy itself.
#![feature(allocator_ext)]
#![allow(dead_code)]
extern crate alloc;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;

mod memory {
    use crate::fallible_tree::FallibleMap;
    use alloc::{sync::Arc, vec::Vec};
    use std::sync::Mutex;

    fn fixture_index<K: Ord, V, const N: usize>(entries: [(K, V); N]) -> FallibleMap<K, V> {
        let mut index = FallibleMap::new();
        for (key, value) in entries {
            index.try_insert(key, value).unwrap();
        }
        index
    }
    #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    struct VirtualPageNumber(usize);
    impl VirtualPageNumber {
        fn from_vpn(vpn: usize) -> Self {
            Self(vpn)
        }
    }
    struct PageRange {
        start: VirtualPageNumber,
        end: VirtualPageNumber,
    }
    impl VirtualPageNumber {
        fn as_usize(self) -> usize {
            self.0
        }
    }
    #[derive(Default)]
    struct Evidence {
        events: Vec<&'static str>,
        fail_fence: bool,
        writers: usize,
    }
    struct FileResource {
        evidence: Arc<Mutex<Evidence>>,
        mm: Arc<Mutex<()>>,
    }
    impl Drop for FileResource {
        fn drop(&mut self) {
            assert!(
                self.mm.try_lock().is_ok(),
                "last file cleanup reentered held MM lock"
            );
            let mut evidence = self.evidence.lock().unwrap();
            assert!(evidence.events.contains(&"fence"));
            assert_eq!(
                evidence.writers, 0,
                "file closed with page writer still held"
            );
            evidence.events.push("file-close");
        }
    }
    struct SharedResident {
        evidence: Arc<Mutex<Evidence>>,
    }
    impl Drop for SharedResident {
        fn drop(&mut self) {
            let mut evidence = self.evidence.lock().unwrap();
            evidence.writers -= 1;
            evidence.events.push("page-writer-release");
        }
    }
    struct SharedFileArea {
        _mapping: Arc<FileResource>,
        resident: FallibleMap<VirtualPageNumber, SharedResident>,
    }
    struct MapArea {
        device: Option<()>,
        lazy_private: bool,
        shared_anonymous: Option<()>,
        shared_file: Option<SharedFileArea>,
        data_frames: FallibleMap<VirtualPageNumber, ()>,
        vpn_range: PageRange,
        _file: Option<Arc<FileResource>>,
    }
    impl MapArea {
        fn unmap_device_area(&self, table: &mut PageTable, commit: &mut TranslationCommit) {
            for vpn in self.vpn_range.start.0..self.vpn_range.end.0 {
                let _ = table.unmap(VirtualPageNumber(vpn), commit);
            }
        }
    }
    struct PageTable {
        evidence: Arc<Mutex<Evidence>>,
    }
    impl PageTable {
        fn unmap(
            &mut self,
            _: VirtualPageNumber,
            commit: &mut TranslationCommit,
        ) -> Result<(), ()> {
            self.evidence.lock().unwrap().events.push("revoke");
            commit.evidence = Some(self.evidence.clone());
            commit.pending = true;
            Ok(())
        }
    }
    struct TranslationCommit {
        evidence: Option<Arc<Mutex<Evidence>>>,
        pending: bool,
    }
    impl TranslationCommit {
        fn new() -> Self {
            Self {
                evidence: None,
                pending: false,
            }
        }
        fn synchronize(&mut self) -> Result<(), ()> {
            if let Some(evidence) = &self.evidence {
                let mut evidence = evidence.lock().unwrap();
                if evidence.fail_fence {
                    return Err(());
                }
                evidence.events.push("fence");
                self.pending = false;
            }
            Ok(())
        }
    }
    impl Drop for TranslationCommit {
        fn drop(&mut self) {
            assert!(
                !self.pending,
                "host TLB adapter cannot retire an unsynchronized commit"
            );
        }
    }
    struct MemorySet {
        areas: FallibleMap<VirtualPageNumber, MapArea>,
        page_table: PageTable,
    }
    impl MemorySet {
        fn remove_area(&mut self, start: &VirtualPageNumber) -> Option<MapArea> {
            self.areas.remove(start)
        }
    }
    mod retirement {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../kernel/src/memory/mm/retire.rs"
        ));

        type Fixture = (
            Arc<Mutex<Evidence>>,
            Arc<Mutex<()>>,
            Arc<FileResource>,
            PageTable,
        );
        fn fixture() -> Fixture {
            let evidence = Arc::new(Mutex::new(Evidence::default()));
            let mm = Arc::new(Mutex::new(()));
            let file = Arc::new(FileResource {
                evidence: evidence.clone(),
                mm: mm.clone(),
            });
            let table = PageTable {
                evidence: evidence.clone(),
            };
            (evidence, mm, file, table)
        }
        fn area(file: Arc<FileResource>, start: usize, device: bool) -> MapArea {
            MapArea {
                device: device.then_some(()),
                lazy_private: !device,
                shared_anonymous: None,
                shared_file: None,
                data_frames: fixture_index([(VirtualPageNumber(start), ())]),
                vpn_range: PageRange {
                    start: VirtualPageNumber(start),
                    end: VirtualPageNumber(start + 1),
                },
                _file: Some(file),
            }
        }
        #[test]
        fn single_retirement_releases_last_file_after_guard_and_fence() {
            let (evidence, mm, file, page_table) = fixture();
            let mut memory = MemorySet {
                areas: fixture_index([(VirtualPageNumber(1), area(file, 1, false))]),
                page_table,
            };
            let retirement = {
                let _guard = mm.lock().unwrap();
                let token = memory.prepare_area_retirement(VirtualPageNumber(1));
                assert_eq!(evidence.lock().unwrap().events, ["revoke"]);
                token
            };
            retirement.synchronize();
            assert_eq!(
                evidence.lock().unwrap().events,
                ["revoke", "fence", "file-close"]
            );
        }
        #[test]
        fn batch_keeps_shared_file_until_all_device_and_regular_revocations_complete() {
            let (evidence, mm, file, mut table) = fixture();
            let retirement = {
                let _guard = mm.lock().unwrap();
                PreparedAreaRetirement::batch(
                    vec![area(file.clone(), 1, false), area(file, 2, true)],
                    &mut table,
                )
            };
            assert_eq!(evidence.lock().unwrap().events, ["revoke", "revoke"]);
            retirement.synchronize();
            assert_eq!(
                evidence.lock().unwrap().events,
                ["revoke", "revoke", "fence", "file-close"]
            );
        }
        #[test]
        fn resident_page_writers_retire_before_last_mapping_file() {
            let (evidence, _, file, mut table) = fixture();
            evidence.lock().unwrap().writers = 1;
            let shared = SharedFileArea {
                _mapping: file,
                resident: fixture_index([(
                    VirtualPageNumber(1),
                    SharedResident {
                        evidence: evidence.clone(),
                    },
                )]),
            };
            let area = MapArea {
                device: None,
                lazy_private: false,
                shared_anonymous: None,
                shared_file: Some(shared),
                data_frames: FallibleMap::new(),
                vpn_range: PageRange {
                    start: VirtualPageNumber(1),
                    end: VirtualPageNumber(2),
                },
                _file: None,
            };
            PreparedAreaRetirement::batch(vec![area], &mut table).synchronize();
            assert_eq!(
                evidence.lock().unwrap().events,
                ["revoke", "fence", "page-writer-release", "file-close"]
            );
        }
        #[test]
        fn abandoned_token_fail_stops_and_preserves_backing() {
            let (evidence, _, file, mut table) = fixture();
            let weak = Arc::downgrade(&file);
            let token = PreparedAreaRetirement::batch(vec![area(file, 1, true)], &mut table);
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(token))).is_err()
            );
            assert_eq!(evidence.lock().unwrap().events, ["revoke"]);
            assert_eq!(weak.strong_count(), 1);
        }
        #[test]
        fn failed_fence_fail_stops_without_dropping_file_or_double_panic() {
            let (evidence, _, file, mut table) = fixture();
            evidence.lock().unwrap().fail_fence = true;
            let weak = Arc::downgrade(&file);
            let token = PreparedAreaRetirement::batch(vec![area(file, 1, false)], &mut table);
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| token.synchronize()))
                    .is_err()
            );
            assert_eq!(evidence.lock().unwrap().events, ["revoke"]);
            assert_eq!(weak.strong_count(), 1);
        }
    }
}
