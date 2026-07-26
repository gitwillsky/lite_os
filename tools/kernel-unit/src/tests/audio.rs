use crate::{
    alsa_codec,
    audio_readiness::project,
    audio_state::{PcmState, PcmStateOwner},
    drivers::{PCM_BUFFER_FRAMES, PCM_PERIOD_FRAMES},
    memfd_state::{
        F_SEAL_GROW, F_SEAL_SEAL, F_SEAL_SHRINK, F_SEAL_WRITE, MemFileState, MemFileStateError,
    },
    poll_notification::wait_event,
    virtio_sound_lifecycle::{DeviceState, polled_control_ack_requires_deferred, unique_slot_for},
    virtio_sound_wire,
};

#[test]
fn alsa_direct_poll_uses_notification_read_edge_then_rechecks_output_level() {
    const POLLIN: i16 = 0x001;
    const POLLOUT: i16 = 0x004;

    assert_eq!(wait_event(), POLLIN);
    assert_eq!(project(POLLOUT, false), 0);
    assert_eq!(project(POLLOUT, true), POLLOUT);
}

#[test]
fn pcm_state_tracks_period_positions_and_recovers_xrun() {
    let mut state = PcmStateOwner::new();
    assert_eq!(state.state, PcmState::Open);
    state.configure().unwrap();
    state
        .set_software(
            PCM_PERIOD_FRAMES as u64,
            PCM_BUFFER_FRAMES as u64,
            PCM_BUFFER_FRAMES as u64,
            1 << 31,
        )
        .unwrap();
    state.prepare().unwrap();
    for _ in 0..4 {
        state.submit_period().unwrap();
    }
    assert!(!state.writable());
    state.start().unwrap();
    state.complete_period();
    assert_eq!(state.hardware_frames, PCM_PERIOD_FRAMES as u64);
    assert!(state.writable());
    for _ in 0..4 {
        state.complete_period();
    }
    assert_eq!(state.state, PcmState::Xrun);
    state.prepare().unwrap();
    assert_eq!(state.state, PcmState::Prepared);
    assert_eq!(state.application_frames, 0);
    assert_eq!(state.hardware_frames, 0);
    state.xrun();
    state.prepare().unwrap();
    state.drop_stream().unwrap();
    state.free_hardware().unwrap();
    assert_eq!(state.state, PcmState::Open);
}

#[test]
fn pcm_disconnect_is_terminal_for_submission() {
    let mut state = PcmStateOwner::new();
    state.configure().unwrap();
    state.prepare().unwrap();
    state.disconnect();
    assert_eq!(state.state, PcmState::Disconnected);
    assert!(state.submit_period().is_err());
    assert!(!state.writable());
}

#[test]
fn virtio_sound_wire_uses_spec_queue_and_pcm_values() {
    assert_eq!(virtio_sound_wire::CONTROL_QUEUE, 0);
    assert_eq!(virtio_sound_wire::EVENT_QUEUE, 1);
    assert_eq!(virtio_sound_wire::TX_QUEUE, 2);
    assert_eq!(virtio_sound_wire::RX_QUEUE, 3);
    assert_eq!(virtio_sound_wire::R_PCM_INFO, 0x0100);
    assert_eq!(virtio_sound_wire::R_PCM_SET_PARAMS, 0x0101);
    assert_eq!(virtio_sound_wire::R_PCM_PREPARE, 0x0102);
    assert_eq!(virtio_sound_wire::R_PCM_RELEASE, 0x0103);
    assert_eq!(virtio_sound_wire::R_PCM_START, 0x0104);
    assert_eq!(virtio_sound_wire::R_PCM_STOP, 0x0105);
    assert_eq!(virtio_sound_wire::EVT_PCM_XRUN, 0x1101);
    assert_eq!(virtio_sound_wire::S_OK, 0x8000);
    assert_eq!(virtio_sound_wire::D_OUTPUT, 0);
    assert_eq!(virtio_sound_wire::PCM_FMT_FLOAT, 19);
    assert_eq!(virtio_sound_wire::PCM_RATE_48000, 7);
    assert_eq!(virtio_sound_wire::PCM_INFO_BYTES, 32);
    assert_eq!(virtio_sound_wire::CONTROL_REQUEST_BYTES, 24);
    assert_eq!(virtio_sound_wire::CONTROL_RESPONSE_BYTES, 36);
    assert_eq!(virtio_sound_wire::EVENT_BYTES, 8);
    assert_eq!(virtio_sound_wire::XFER_BYTES, 4);
    assert_eq!(virtio_sound_wire::STATUS_BYTES, 8);
}

#[test]
fn virtio_sound_wire_codec_is_little_endian_and_bounded() {
    let mut bytes = [0u8; 12];
    assert_eq!(
        virtio_sound_wire::write_u32(&mut bytes, 4, 0x1234_5678),
        Some(())
    );
    assert_eq!(&bytes[4..8], &[0x78, 0x56, 0x34, 0x12]);
    assert_eq!(virtio_sound_wire::read_u32(&bytes, 4), Some(0x1234_5678));
    assert_eq!(
        virtio_sound_wire::read_u64(&bytes, 4),
        Some(0x0000_0000_1234_5678)
    );
    assert_eq!(virtio_sound_wire::write_u32(&mut bytes, 10, 1), None);
    assert_eq!(virtio_sound_wire::read_u64(&bytes, 8), None);
}

#[test]
fn memfd_shared_storage_and_seals_are_one_state_machine() {
    let mut state = MemFileState::new(true);
    state.truncate(4096).unwrap();
    assert_eq!(state.len(), 4096);
    state.write(64, b"shared-pcm").unwrap();
    let mut output = [0u8; 10];
    assert_eq!(state.read(64, &mut output), output.len());
    assert_eq!(&output, b"shared-pcm");

    assert_eq!(
        state.add_seals(F_SEAL_GROW | F_SEAL_SHRINK).unwrap(),
        F_SEAL_GROW | F_SEAL_SHRINK
    );
    assert_eq!(
        state.truncate(8192),
        Err(MemFileStateError::PermissionDenied)
    );
    assert_eq!(
        state.truncate(2048),
        Err(MemFileStateError::PermissionDenied)
    );
    assert_eq!(state.write(usize::MAX, &[]), Ok(0));
    state.write(128, b"still-writable").unwrap();
    assert_eq!(
        state.add_seals(F_SEAL_WRITE),
        Err(MemFileStateError::InvalidOperation)
    );
    state.add_seals(F_SEAL_SEAL).unwrap();
    assert_eq!(
        state.add_seals(F_SEAL_GROW),
        Err(MemFileStateError::PermissionDenied)
    );
}

#[test]
fn memfd_without_allow_sealing_starts_sealed() {
    let mut state = MemFileState::new(false);
    assert_eq!(state.seals(), F_SEAL_SEAL);
    assert_eq!(
        state.add_seals(F_SEAL_GROW),
        Err(MemFileStateError::PermissionDenied)
    );
}

#[test]
fn alsa_ioctl_numbers_and_layout_match_linux_native_64_bit_uapi() {
    assert_eq!(alsa_codec::SNDRV_PCM_IOCTL_PVERSION, 0x8004_4100);
    assert_eq!(alsa_codec::SNDRV_PCM_IOCTL_HW_PARAMS, 0xc260_4111);
    assert_eq!(alsa_codec::SNDRV_PCM_IOCTL_HW_FREE, 0x0000_4112);
    assert_eq!(alsa_codec::SNDRV_PCM_IOCTL_SW_PARAMS, 0xc088_4113);
    assert_eq!(alsa_codec::SNDRV_PCM_IOCTL_STATUS, 0x8098_4120);
    assert_eq!(alsa_codec::SNDRV_PCM_IOCTL_DELAY, 0x8008_4121);
    assert_eq!(alsa_codec::SNDRV_PCM_IOCTL_SYNC_PTR, 0xc088_4123);
    assert_eq!(alsa_codec::SNDRV_PCM_IOCTL_PREPARE, 0x0000_4140);
    assert_eq!(alsa_codec::SNDRV_PCM_IOCTL_START, 0x0000_4142);
    assert_eq!(alsa_codec::SNDRV_PCM_IOCTL_DROP, 0x0000_4143);
    assert_eq!(alsa_codec::SNDRV_PCM_IOCTL_WRITEI_FRAMES, 0x4018_4150);
    assert_eq!(alsa_codec::HW_PARAMS_BYTES, 608);
    assert_eq!(alsa_codec::SW_PARAMS_BYTES, 136);
    assert_eq!(alsa_codec::STATUS_BYTES, 152);
    assert_eq!(alsa_codec::SYNC_PTR_BYTES, 136);
    assert_eq!(alsa_codec::XFER_BYTES, 24);
    assert_eq!(alsa_codec::PCM_PROTOCOL_VERSION, 0x0002_0012);
}

#[test]
fn alsa_codec_requires_exact_masks_and_intervals() {
    let mut bytes = [0u8; 608];
    alsa_codec::write_u32(&mut bytes, 4, 1 << 3).unwrap();
    assert_eq!(alsa_codec::exact_mask(&bytes, 4), Some(3));
    alsa_codec::write_u32(&mut bytes, 8, 1).unwrap();
    assert_eq!(alsa_codec::exact_mask(&bytes, 4), None);

    let interval = 260 + (13 - 8) * 12;
    alsa_codec::write_u32(&mut bytes, interval, 256).unwrap();
    alsa_codec::write_u32(&mut bytes, interval + 4, 256).unwrap();
    alsa_codec::write_u32(&mut bytes, interval + 8, 0b0100).unwrap();
    assert_eq!(alsa_codec::exact_interval(&bytes, 13), Some(256));
    alsa_codec::write_u32(&mut bytes, interval + 4, 512).unwrap();
    assert_eq!(alsa_codec::exact_interval(&bytes, 13), None);

    alsa_codec::write_u64(&mut bytes, 440, 1024).unwrap();
    assert_eq!(alsa_codec::read_u64(&bytes, 440), Some(1024));
}

#[test]
fn alsa_short_transfer_publishes_progress_before_later_fault() {
    assert_eq!(alsa_codec::stop_or_error(0, 14), Err(14));
    assert_eq!(alsa_codec::stop_or_error(256, 14), Ok(()));
    assert_eq!(alsa_codec::stop_or_error(256, 11), Ok(()));
    assert_eq!(alsa_codec::stop_or_error(256, 4), Ok(()));
}

#[test]
fn virtio_sound_lifecycle_is_single_track_and_reset_is_idempotent() {
    let mut state = DeviceState::Setup;
    state = state.after_configure().unwrap();
    assert_eq!(state, DeviceState::Configured);
    assert_eq!(state.after_release(), Some(DeviceState::Setup));
    state = state.after_prepare().unwrap();
    assert_eq!(state, DeviceState::Prepared);
    state = state.after_start().unwrap();
    assert_eq!(state, DeviceState::Running);
    assert!(state.after_prepare().is_none());
    state = state.after_stop().unwrap();
    assert_eq!(state, DeviceState::Stopped);
    state = state.after_release().unwrap();
    assert_eq!(state, DeviceState::Setup);
    state = state.after_configure().unwrap();
    state = state.after_release().unwrap();
    assert_eq!(state, DeviceState::Setup);
    assert!(state.fail());
    assert_eq!(state, DeviceState::Failed);
    assert!(!state.fail());
    assert!(state.after_start().is_none());
}

#[test]
fn virtio_sound_completion_claim_rejects_unknown_and_duplicate_heads() {
    let outstanding = [Some(7), None, Some(11), None];
    assert_eq!(
        unique_slot_for(11, outstanding.len(), |index| outstanding[index]),
        Some(2)
    );
    assert_eq!(
        unique_slot_for(12, outstanding.len(), |index| outstanding[index]),
        None
    );
    let duplicate = [Some(7), None, Some(7), None];
    assert_eq!(
        unique_slot_for(7, duplicate.len(), |index| duplicate[index]),
        None
    );
}

#[test]
fn virtio_sound_polled_control_ack_preserves_coalesced_tx_work() {
    assert!(!polled_control_ack_requires_deferred(0));
    assert!(polled_control_ack_requires_deferred(1));
    assert!(polled_control_ack_requires_deferred(3));
}
