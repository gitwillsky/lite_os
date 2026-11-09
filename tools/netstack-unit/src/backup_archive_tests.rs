//! 在同一 StorageService 能力边界运行生产归档逻辑，检查轮转/结果未知，非真实 FUSE/断电验收。
use crate::backup_archive::{list, prune, read, remove, save};
use filesystem_protocol::{StorageEntry, StorageService};
use native_abi::{ERR_ALREADY_BOUND, ERR_IO, Status};
use router_protocol::backup::Id;
use std::{collections::BTreeMap, sync::Mutex};

#[derive(Default)]
struct State {
    files: BTreeMap<Vec<u8>, Vec<u8>>,
    creates: usize,
    fail_create_after: bool,
    fail_sync: bool,
    fail_remove_after: bool,
}
#[derive(Default)]
struct Store(Mutex<State>);
impl StorageService for Store {
    fn list(&self) -> Result<Vec<StorageEntry>, Status> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .files
            .iter()
            .map(|(name, bytes)| StorageEntry {
                name: name.clone(),
                size: bytes.len() as u64,
                modified_seconds: -5,
            })
            .collect())
    }
    fn read(&self, name: &[u8]) -> Result<Option<Vec<u8>>, Status> {
        Ok(self.0.lock().unwrap().files.get(name).cloned())
    }
    fn create(&self, name: &[u8], bytes: &[u8]) -> Result<(), Status> {
        let mut state = self.0.lock().unwrap();
        if state.files.contains_key(name) {
            return Err(ERR_ALREADY_BOUND);
        }
        state.creates += 1;
        state.files.insert(name.to_vec(), bytes.to_vec());
        if core::mem::take(&mut state.fail_create_after) {
            Err(ERR_IO)
        } else {
            Ok(())
        }
    }
    fn write(&self, _: &[u8], _: &[u8]) -> Result<(), Status> {
        panic!("backups must never overwrite a file")
    }
    fn remove(&self, name: &[u8]) -> Result<(), Status> {
        let mut state = self.0.lock().unwrap();
        state.files.remove(name);
        if core::mem::take(&mut state.fail_remove_after) {
            Err(ERR_IO)
        } else {
            Ok(())
        }
    }
    fn sync(&self) -> Result<(), Status> {
        if core::mem::take(&mut self.0.lock().unwrap().fail_sync) {
            Err(ERR_IO)
        } else {
            Ok(())
        }
    }
}

#[test]
fn backup_unknown_creation_is_confirmed_without_new_file_or_overwriting_collision() {
    let store = Store::default();
    let id = Id::after(None, 100, 4).unwrap();
    {
        let mut state = store.0.lock().unwrap();
        state.fail_create_after = true;
        state.fail_sync = true;
    }
    let mut touched = false;
    assert_eq!(
        save(&store, id, true, b"configuration", &mut touched),
        Err(ERR_IO)
    );
    assert!(touched);
    touched = false;
    assert_eq!(
        save(&store, id, true, b"configuration", &mut touched),
        Ok(())
    );
    assert!(!touched);
    assert_eq!(store.0.lock().unwrap().creates, 1);
    assert_eq!(
        save(&store, id, true, b"different", &mut touched),
        Err(ERR_ALREADY_BOUND)
    );
    assert_eq!(read(&store, id).unwrap(), b"configuration");
}

#[test]
fn backup_rotation_retries_uncertain_delete_and_never_touches_manual_or_foreign_files() {
    let store = Store::default();
    store
        .0
        .lock()
        .unwrap()
        .files
        .insert(b"interfaces".to_vec(), b"active".to_vec());
    store
        .0
        .lock()
        .unwrap()
        .files
        .insert(b"backup-auto-malformed.json".to_vec(), b"foreign".to_vec());
    let mut ids = Vec::new();
    let mut previous = None;
    for index in 0..4 {
        let id = Id::after(previous, 100 - index, u128::from(index)).unwrap();
        save(&store, id, true, b"saved", &mut false).unwrap();
        ids.push(id);
        previous = Some(id);
    }
    let manual = Id::after(previous, 200, 9).unwrap();
    save(&store, manual, false, b"manual", &mut false).unwrap();
    store.0.lock().unwrap().fail_remove_after = true;
    assert_eq!(prune(&store, 2, &mut false), Err(ERR_IO));
    store.0.lock().unwrap().fail_sync = true;
    assert_eq!(prune(&store, 2, &mut false), Err(ERR_IO));
    // 已经看不到待删除文件时，仍不能跳过上一轮未确认的目录同步。
    store.0.lock().unwrap().fail_sync = true;
    assert_eq!(prune(&store, 2, &mut false), Err(ERR_IO));
    prune(&store, 2, &mut false).unwrap();
    let entries = list(&store).unwrap();
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry.automatic)
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        [ids[3], ids[2]]
    );
    assert_eq!(read(&store, manual).unwrap(), b"manual");
    assert_eq!(
        store
            .0
            .lock()
            .unwrap()
            .files
            .get(b"interfaces".as_slice())
            .unwrap(),
        b"active"
    );
    assert!(
        store
            .0
            .lock()
            .unwrap()
            .files
            .contains_key(b"backup-auto-malformed.json".as_slice())
    );
    remove(&store, manual).unwrap();
    assert!(read(&store, manual).is_err());
}
