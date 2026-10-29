//! Production capability state and Linux codec, with no policy/scheduler replacement.
#![allow(dead_code)]
#[path = "../../../kernel/src/execution/task/model/capabilities.rs"]
mod capabilities;
use capabilities::{CapabilityError, CapabilityExecIds, CapabilityState};
use syscall_abi::capability::{
    Capability, CapabilitySets, VALID_MASK, VERSION_1, VERSION_2, VERSION_3, data_size,
};
use syscall_abi::file_capability::{EFFECTIVE, FileCapabilityData, REVISION_2};

fn file_caps(permitted: u64, inheritable: u64, effective: bool) -> FileCapabilityData {
    FileCapabilityData {
        magic: REVISION_2 | if effective { EFFECTIVE } else { 0 },
        permitted,
        inheritable,
        root_uid: 0,
    }
}
fn file_exec(file: FileCapabilityData, nnp: bool) -> CapabilityExecIds {
    CapabilityExecIds {
        uids: [1000; 2],
        gids: [1000; 2],
        root: Some(0),
        id_changed: false,
        nnp,
        file: Some(file),
    }
}

#[test]
fn effective_file_missing_bounding_is_denied_without_mutating_credentials() {
    let mut state = CapabilityState::root();
    let boot = Capability::SYS_BOOT.mask();
    state.prctl(24, 22, [0; 3]).unwrap();
    state.fix_setuid([0; 3], [1000; 3], Some(0));
    let before = state.snapshot();
    assert!(matches!(
        state.exec(file_exec(file_caps(boot, 0, true), false)),
        Err(CapabilityError::Permission)
    ));
    assert_eq!(state.snapshot(), before);
    // capability-aware文件不要求forced集合完整，保持成功但无有效权限。
    state
        .exec(file_exec(file_caps(boot, 0, false), false))
        .unwrap();
    assert_eq!(state.snapshot().sets, CapabilitySets::default());
}

#[test]
fn inheritable_file_bits_can_supply_capabilities_removed_from_bounding() {
    let mut state = CapabilityState::root();
    let boot = Capability::SYS_BOOT.mask();
    state
        .set(CapabilitySets {
            inheritable: boot,
            ..state.snapshot().sets
        })
        .unwrap();
    state.prctl(24, 22, [0; 3]).unwrap();
    state.fix_setuid([0; 3], [1000; 3], Some(0));
    state
        .exec(file_exec(file_caps(boot, boot, true), false))
        .unwrap();
    assert_eq!(state.snapshot().sets.permitted, boot);
    assert_eq!(state.snapshot().sets.effective, boot);
}

#[test]
fn file_caps_cancel_ambient_even_for_empty_sets_and_no_new_privileges() {
    let mut state = CapabilityState::root();
    let bind = Capability::NET_BIND_SERVICE.mask();
    state
        .set(CapabilitySets {
            inheritable: bind,
            ..state.snapshot().sets
        })
        .unwrap();
    state.prctl(28, 4, [0; 3]).unwrap();
    state.fix_setuid([0; 3], [1000; 3], Some(0));
    state.prctl(47, 2, [10, 0, 0]).unwrap();
    let outcome = state.exec(file_exec(file_caps(0, 0, true), false)).unwrap();
    assert!(outcome.secure); // fixed commoncap的fE即使pP为空也设置secureexec。
    assert_eq!(state.snapshot().ambient, 0);
    assert_eq!(state.snapshot().sets.permitted, 0);
    state
        .exec(file_exec(file_caps(bind, 0, true), true))
        .unwrap();
    assert_eq!(state.snapshot().sets.effective, 0); // NNP不增长旧pP。
}

#[test]
fn setuid_root_with_file_caps_does_not_gain_legacy_root_but_real_root_still_does() {
    let bind = Capability::NET_BIND_SERVICE.mask();
    let mut state = CapabilityState::root();
    state.fix_setuid([0; 3], [1000; 3], Some(0));
    let mut ids = file_exec(file_caps(bind, 0, true), false);
    ids.uids = [1000, 0];
    ids.id_changed = true;
    state.exec(ids).unwrap();
    assert_eq!(state.snapshot().sets.effective, bind);
    let mut root = CapabilityState::root();
    let mut ids = file_exec(file_caps(bind, 0, true), false);
    ids.uids = [0; 2];
    root.exec(ids).unwrap();
    assert_eq!(root.snapshot().sets.permitted, VALID_MASK);
    // 已锁定NOROOT时仍可通过实际file caps授予指定权限，不授予完整root集合。
    root.prctl(28, 3, [0; 3]).unwrap();
    let mut ids = file_exec(file_caps(bind | (1 << 63), 1 << 63, true), false);
    ids.uids = [0; 2];
    root.exec(ids).unwrap();
    assert_eq!(root.snapshot().sets.effective, bind);
}

#[test]
fn linux_capability_codec_uses_one_layout_and_masks_undefined_bits() {
    let original = CapabilitySets {
        effective: 0x123456789a,
        permitted: VALID_MASK,
        inheritable: 0x567890abcd,
    };
    let bytes = original.encode();
    assert_eq!(CapabilitySets::decode(&bytes), Some(original));
    assert_eq!(
        CapabilitySets::decode(&bytes[..12]).unwrap().permitted,
        u32::MAX as u64
    );
    assert_eq!(data_size(VERSION_1), Some(12));
    assert_eq!(data_size(VERSION_2), Some(24));
    assert_eq!(data_size(VERSION_3), Some(24));
    assert_eq!(data_size(0), None);
    assert_eq!(
        CapabilitySets::decode(&[0xff; 24]).unwrap().effective,
        VALID_MASK
    );
    assert!(CapabilitySets::decode(&[0; 16]).is_none());
    assert!(Capability::from_raw(40).is_some());
    assert!(Capability::from_raw(41).is_none());
    assert!(Capability::from_raw(1usize << 32).is_none());
}
#[test]
fn capset_drop_is_irreversible_and_rejected_sets_leave_all_state_intact() {
    let mut state = CapabilityState::root();
    let boot = Capability::SYS_BOOT.mask();
    let sets = CapabilitySets {
        effective: boot,
        permitted: boot,
        inheritable: boot,
    };
    state.set(sets).unwrap();
    let before = state.snapshot();
    for sets in [
        CapabilitySets {
            effective: boot << 1,
            ..sets
        },
        CapabilitySets {
            permitted: boot << 1,
            ..sets
        },
        CapabilitySets {
            inheritable: boot << 1,
            ..sets
        },
    ] {
        assert_eq!(state.set(sets), Err(CapabilityError::Permission));
        assert_eq!(state.snapshot(), before);
    }
    assert!(state.capable(Capability::SYS_BOOT));
    assert!(!state.capable(Capability::SYS_ADMIN));
}
#[test]
fn inheritable_requires_bounding_even_when_setpcap_is_retained() {
    let mut state = CapabilityState::root();
    let boot = Capability::SYS_BOOT.mask();
    state.prctl(24, 22, [0; 3]).unwrap();
    let mut sets = state.snapshot().sets;
    sets.inheritable = boot;
    assert_eq!(state.set(sets), Err(CapabilityError::Permission));
    assert_eq!(state.prctl(23, 22, [0; 3]), Ok(0));
    sets.effective &= !Capability::SETPCAP.mask();
    state.set(sets).unwrap_err(); // failed addition still leaves original owner unchanged
    sets.inheritable = 0;
    state.set(sets).unwrap();
    assert_eq!(
        state.prctl(24, usize::MAX, [0; 3]),
        Err(CapabilityError::Permission)
    );
}
#[test]
fn uid_fixup_preserves_saved_root_then_drops_all_ids_and_ambient() {
    let mut state = CapabilityState::root();
    state.fix_setuid([0, 0, 0], [0, 1000, 0], Some(0));
    assert_eq!(state.snapshot().sets.effective, 0);
    assert_eq!(state.snapshot().sets.permitted, VALID_MASK);
    state.fix_setuid([0, 1000, 0], [0, 0, 0], Some(0));
    assert_eq!(state.snapshot().sets.effective, VALID_MASK);
    state.prctl(8, 1, [0; 3]).unwrap();
    state.fix_setuid([0, 0, 0], [1000; 3], Some(0));
    assert_eq!(state.snapshot().sets.permitted, VALID_MASK);
    assert_eq!(state.snapshot().sets.effective, 0);
    state.prctl(8, 0, [0; 3]).unwrap();
    state.fix_setuid([1000; 3], [0; 3], Some(0));
    state.fix_setuid([0; 3], [1000; 3], Some(0));
    assert_eq!(state.snapshot().sets.permitted, 0);
}
#[test]
fn securebits_locks_and_full_word_validation_are_irreversible() {
    let mut state = CapabilityState::root();
    assert_eq!(
        state.prctl(28, 1usize << 32, [0; 3]),
        Err(CapabilityError::Permission)
    );
    state.prctl(28, 3 | 48, [0; 3]).unwrap();
    assert_eq!(
        state.prctl(28, 2 | 48, [0; 3]),
        Err(CapabilityError::Permission)
    );
    assert_eq!(
        state.prctl(28, 1 | 48, [0; 3]),
        Err(CapabilityError::Permission)
    );
    assert_eq!(state.prctl(8, 0, [0; 3]), Err(CapabilityError::Permission));
    state
        .exec(CapabilityExecIds {
            uids: [0, 0],
            gids: [0, 0],
            root: Some(0),
            id_changed: false,
            nnp: false,
            file: None,
        })
        .unwrap();
    assert_eq!(state.prctl(7, 0, [0; 3]), Ok(0));
    assert_eq!(state.prctl(27, 0, [0; 3]), Ok(3 | 32));
    state.set(CapabilitySets::default()).unwrap();
    assert_eq!(
        state.prctl(28, 3 | 32, [0; 3]),
        Err(CapabilityError::Permission)
    );
    assert_eq!(state.prctl(28, 3 | 32 | (1 << 8) | (1 << 9), [0; 3]), Ok(0));
}
#[test]
fn exec_legacy_root_bounding_nnp_and_ambient_use_single_transition() {
    let mut state = CapabilityState::root();
    let keep = Capability::NET_BIND_SERVICE.mask();
    state.prctl(24, 22, [0; 3]).unwrap();
    state
        .set(CapabilitySets {
            effective: keep,
            permitted: keep,
            inheritable: keep,
        })
        .unwrap();
    state
        .exec(CapabilityExecIds {
            uids: [0, 0],
            gids: [0, 0],
            root: Some(0),
            id_changed: false,
            nnp: true,
            file: None,
        })
        .unwrap();
    assert_eq!(state.snapshot().sets.permitted, keep);
    state
        .exec(CapabilityExecIds {
            uids: [0, 0],
            gids: [0, 0],
            root: Some(0),
            id_changed: false,
            nnp: false,
            file: None,
        })
        .unwrap();
    assert_eq!(
        state.snapshot().sets.permitted,
        VALID_MASK & !Capability::SYS_BOOT.mask()
    );
    state.prctl(28, 4, [0; 3]).unwrap(); // NO_SETUID_FIXUP preserves retained sets on UID drop
    state.fix_setuid([0; 3], [1000; 3], Some(0));
    state.prctl(47, 2, [10, 0, 0]).unwrap();
    state
        .exec(CapabilityExecIds {
            uids: [1000, 1000],
            gids: [0, 0],
            root: Some(0),
            id_changed: false,
            nnp: false,
            file: None,
        })
        .unwrap();
    assert_eq!(state.snapshot().sets.effective, keep);
    assert_eq!(state.snapshot().sets.permitted, keep);
    let mut sets = state.snapshot().sets;
    sets.inheritable = 0;
    state.set(sets).unwrap();
    assert_eq!(state.snapshot().ambient, 0);
    assert_eq!(
        state.prctl(47, 2, [10, 0, 0]),
        Err(CapabilityError::Permission)
    );
}

#[test]
fn fsuid_fixup_uses_fixed_fs_set_and_preserves_other_capability_state() {
    let fs_set = [
        Capability::CHOWN,
        Capability::MKNOD,
        Capability::DAC_OVERRIDE,
        Capability::DAC_READ_SEARCH,
        Capability::FOWNER,
        Capability::FSETID,
        Capability::MAC_OVERRIDE,
        Capability::LINUX_IMMUTABLE,
    ]
    .iter()
    .fold(0, |bits, cap| bits | cap.mask());
    let mut owner = CapabilityState::root();
    let before = owner.snapshot();
    owner.fix_fsuid(0, 123, Some(0));
    assert_eq!(owner.snapshot().sets.permitted, before.sets.permitted);
    assert_eq!(
        owner.snapshot().sets.effective,
        before.sets.effective & !fs_set
    );
    assert!(owner.capable(Capability::KILL));
    owner.fix_fsuid(123, 0, Some(0));
    assert_eq!(owner.snapshot().sets.effective, before.sets.effective);
    owner.prctl(28, 4, [0; 3]).unwrap();
    owner.fix_fsuid(0, 123, Some(0));
    assert_eq!(owner.snapshot().sets.effective, before.sets.effective);
    let mut no_mapping = CapabilityState::root();
    no_mapping.fix_fsuid(0, 123, None);
    assert_eq!(no_mapping.snapshot().sets.effective, VALID_MASK);
}
