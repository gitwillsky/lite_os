//! 存储能力边界注入 rename 前/后失败，运行生产配置提交补偿逻辑；不伪称实际 ext4 崩溃验收。

use crate::configuration_policy::{SETTINGS_FILE, decode_settings, encode_settings};
use crate::configuration_storage::{persist, restore};
use filesystem_protocol::StorageService;
use native_abi::{ERR_IO, Status};
use router_protocol::{administration::CommitId, configuration::Configuration};
use std::sync::Mutex;

struct Store {
    state: Mutex<State>,
}

struct State {
    file: Option<Vec<u8>>,
    failures: Vec<(usize, bool)>,
    operations: usize,
    syncs: usize,
}

impl Store {
    fn new(file: Option<Vec<u8>>, failures: Vec<(usize, bool)>) -> Self {
        Self {
            state: Mutex::new(State {
                file,
                failures,
                operations: 0,
                syncs: 0,
            }),
        }
    }

    fn replace(&self, next: Option<&[u8]>) -> Result<(), Status> {
        let mut state = self.state.lock().unwrap();
        let operation = state.operations;
        state.operations += 1;
        let failure = state
            .failures
            .iter()
            .find(|(at, _)| *at == operation)
            .copied();
        if failure == Some((operation, false)) {
            return Err(ERR_IO);
        }
        state.file = next.map(<[u8]>::to_vec);
        if failure.is_some() {
            Err(ERR_IO)
        } else {
            Ok(())
        }
    }
}

impl StorageService for Store {
    // 本夹具只裁决同一个配置文件的补偿；目录/独占创建不属于这些调用路径。
    fn list(&self) -> Result<Vec<filesystem_protocol::StorageEntry>, Status> {
        Err(native_abi::ERR_NOT_SUPPORTED)
    }
    fn create(&self, _: &[u8], _: &[u8]) -> Result<(), Status> {
        Err(native_abi::ERR_NOT_SUPPORTED)
    }

    fn sync(&self) -> Result<(), Status> {
        let mut state = self.state.lock().unwrap();
        let operation = state.operations;
        state.operations += 1;
        state.syncs += 1;
        if state.failures.iter().any(|(at, _)| *at == operation) {
            Err(ERR_IO)
        } else {
            Ok(())
        }
    }

    fn read(&self, _: &[u8]) -> Result<Option<Vec<u8>>, Status> {
        Ok(self.state.lock().unwrap().file.clone())
    }
    fn write(&self, _: &[u8], bytes: &[u8]) -> Result<(), Status> {
        self.replace(Some(bytes))
    }
    fn remove(&self, _: &[u8]) -> Result<(), Status> {
        self.replace(None)
    }
}

#[test]
fn configuration_file_failure_before_or_after_replace_restores_exact_previous_image() {
    for after_replace in [false, true] {
        let before = Some(b"old format bytes, not a re-encoded copy".to_vec());
        let store = Store::new(before.clone(), vec![(0, after_replace)]);
        let failure = persist(&store, SETTINGS_FILE, before.clone(), b"candidate")
            .err()
            .unwrap();
        assert_eq!(failure.status, ERR_IO);
        assert_eq!(failure.restore_error, None);
        assert_eq!(store.read(SETTINGS_FILE).unwrap(), before);
        assert_eq!(store.state.lock().unwrap().operations, 2);
        assert_eq!(
            store.state.lock().unwrap().syncs,
            usize::from(!after_replace)
        );
    }
}

#[test]
fn configuration_file_first_creation_rollback_restores_absence_even_if_delete_reply_is_uncertain() {
    let store = Store::new(None, vec![(0, true), (1, true)]);
    let failure = persist(&store, SETTINGS_FILE, None, b"candidate")
        .err()
        .unwrap();
    assert_eq!(failure.restore_error, Some(ERR_IO));
    assert_eq!(failure.before, None);
    assert_eq!(store.read(SETTINGS_FILE).unwrap(), None);
    restore(&store, SETTINGS_FILE, failure.before.as_deref()).unwrap();
    assert_eq!(store.state.lock().unwrap().operations, 3);
}

#[test]
fn configuration_file_failed_compensation_retains_before_image_and_durable_success_never_reverts() {
    let old = Configuration::default();
    let old_id = CommitId::from_random_bytes([1; 16]);
    let candidate_id = CommitId::from_random_bytes([2; 16]);
    let candidate = Configuration {
        ipv4_forwarding: !old.ipv4_forwarding,
        ..Configuration::default()
    };
    let before = Some(encode_settings(&old, &[old_id]).unwrap());
    let next = encode_settings(&candidate, &[old_id, candidate_id]).unwrap();
    let store = Store::new(before.clone(), vec![(0, true), (1, false)]);
    let failure = persist(&store, SETTINGS_FILE, before.clone(), &next)
        .err()
        .unwrap();
    assert_eq!(failure.restore_error, Some(ERR_IO));
    assert_eq!(failure.before, before);
    assert_eq!(store.read(SETTINGS_FILE).unwrap(), Some(next.clone()));
    restore(&store, SETTINGS_FILE, failure.before.as_deref()).unwrap();
    assert_eq!(store.read(SETTINGS_FILE).unwrap(), before);
    assert_eq!(
        decode_settings(&store.read(SETTINGS_FILE).unwrap().unwrap()).unwrap(),
        (old, vec![old_id])
    );
    assert!(persist(&store, SETTINGS_FILE, before, &next).is_ok());
    assert_eq!(
        decode_settings(&store.read(SETTINGS_FILE).unwrap().unwrap()).unwrap(),
        (candidate, vec![old_id, candidate_id])
    );
    assert_eq!(store.state.lock().unwrap().operations, 4);
}
