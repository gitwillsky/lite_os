use super::codec as alsa_codec;
use crate::{
    audio_readiness::project,
    audio_state::{PcmState, PcmStateOwner},
    drivers::{PCM_BUFFER_FRAMES, PCM_PERIOD_FRAMES},
    memory_seals::{F_SEAL_GROW, F_SEAL_SEAL, F_SEAL_SHRINK, F_SEAL_WRITE, SealError, Seals},
    virtio_sound_lifecycle::{DeviceState, polled_control_ack_requires_deferred, unique_slot_for},
    virtio_sound_wire,
};

#[test]
fn alsa_poll_rechecks_output_level() {
    const POLLOUT: i16 = 0x004;

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
        state.submit_frames(PCM_PERIOD_FRAMES as u64).unwrap();
    }
    assert!(!state.writable());
    state.start().unwrap();
    state.complete_period(PCM_PERIOD_FRAMES as u64);
    assert_eq!(state.hardware_frames, PCM_PERIOD_FRAMES as u64);
    assert!(state.writable());
    for _ in 0..4 {
        state.complete_period(PCM_PERIOD_FRAMES as u64);
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
fn pcm_prepare_resets_an_already_prepared_stream() {
    let mut state = PcmStateOwner::new();
    state.configure().unwrap();
    state.prepare().unwrap();
    state.submit_frames(PCM_PERIOD_FRAMES as u64).unwrap();
    state.prepare().unwrap();
    assert_eq!(state.state, PcmState::Prepared);
    assert_eq!(state.application_frames, 0);
    assert_eq!(state.hardware_frames, 0);
    state.start().unwrap();
    assert!(state.prepare().is_err());
    assert_eq!(state.state, PcmState::Running);
}

#[test]
fn pcm_disconnect_is_terminal_for_submission() {
    let mut state = PcmStateOwner::new();
    state.configure().unwrap();
    state.prepare().unwrap();
    state.disconnect();
    assert_eq!(state.state, PcmState::Disconnected);
    assert!(state.submit_frames(PCM_PERIOD_FRAMES as u64).is_err());
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
fn seals_gate_growth_shrink_and_are_append_only() {
    let mut seals = Seals::new(true);
    assert_eq!(seals.check_truncate(0, 4096), Ok(()));
    assert_eq!(
        seals.add(F_SEAL_GROW | F_SEAL_SHRINK).unwrap(),
        F_SEAL_GROW | F_SEAL_SHRINK
    );
    assert_eq!(
        seals.check_truncate(4096, 8192),
        Err(SealError::PermissionDenied)
    );
    assert_eq!(
        seals.check_truncate(4096, 2048),
        Err(SealError::PermissionDenied)
    );
    assert_eq!(seals.check_write(4096, 4096), Ok(()));
    assert_eq!(
        seals.check_write(4096, 4097),
        Err(SealError::PermissionDenied)
    );
    seals.add(F_SEAL_WRITE).unwrap();
    assert_eq!(
        seals.check_write(4096, 4096),
        Err(SealError::PermissionDenied)
    );
    seals.add(F_SEAL_SEAL).unwrap();
    assert_eq!(seals.add(F_SEAL_GROW), Err(SealError::PermissionDenied));
}

#[test]
fn memfd_without_allow_sealing_starts_sealed() {
    let mut seals = Seals::new(false);
    assert_eq!(seals.bits(), F_SEAL_SEAL);
    assert_eq!(seals.add(F_SEAL_GROW), Err(SealError::PermissionDenied));
}

#[test]
fn alsa_ioctl_numbers_and_layout_match_linux_native_64_bit_uapi() {
    assert_eq!(alsa_codec::SNDRV_PCM_IOCTL_PVERSION, 0x8004_4100);
    assert_eq!(alsa_codec::SNDRV_PCM_IOCTL_TTSTAMP, 0x4004_4103);
    assert_eq!(alsa_codec::SNDRV_PCM_IOCTL_USER_PVERSION, 0x4004_4104);
    assert_eq!(alsa_codec::SNDRV_PCM_IOCTL_INFO, 0x8120_4101);
    assert_eq!(alsa_codec::SNDRV_PCM_IOCTL_HW_REFINE, 0xc260_4110);
    assert_eq!(alsa_codec::SNDRV_PCM_IOCTL_HW_PARAMS, 0xc260_4111);
    assert_eq!(alsa_codec::SNDRV_PCM_IOCTL_HW_FREE, 0x0000_4112);
    assert_eq!(alsa_codec::SNDRV_PCM_IOCTL_SW_PARAMS, 0xc088_4113);
    assert_eq!(alsa_codec::SNDRV_PCM_IOCTL_STATUS, 0x8098_4120);
    assert_eq!(alsa_codec::SNDRV_PCM_IOCTL_STATUS_EXT, 0xc098_4124);
    assert_eq!(alsa_codec::SNDRV_PCM_IOCTL_DELAY, 0x8008_4121);
    assert_eq!(alsa_codec::SNDRV_PCM_IOCTL_SYNC_PTR, 0xc088_4123);
    assert_eq!(alsa_codec::SNDRV_PCM_IOCTL_PREPARE, 0x0000_4140);
    assert_eq!(alsa_codec::SNDRV_PCM_IOCTL_START, 0x0000_4142);
    assert_eq!(alsa_codec::SNDRV_PCM_IOCTL_DROP, 0x0000_4143);
    assert_eq!(alsa_codec::SNDRV_PCM_IOCTL_WRITEI_FRAMES, 0x4018_4150);
    assert_eq!(alsa_codec::HW_PARAMS_BYTES, 608);
    assert_eq!(alsa_codec::PCM_INFO_BYTES, 288);
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
    // 物理流只配置一次（Setup → Configured），之后只在 Prepared / Running / Stopped 间循环，永不回到 Setup。
    let mut state = DeviceState::Setup;
    assert!(state.after_prepare().is_none(), "prepare before configure");
    state = state.after_configure().unwrap();
    assert_eq!(state, DeviceState::Configured);
    assert!(state.after_configure().is_none(), "configure only once");
    state = state.after_prepare().unwrap();
    assert_eq!(state, DeviceState::Prepared);
    state = state.after_start().unwrap();
    assert_eq!(state, DeviceState::Running);
    assert!(state.after_prepare().is_none());
    state = state.after_stop().unwrap();
    assert_eq!(state, DeviceState::Stopped);
    // 混音器排空后的重新装填路径：Stopped → Prepared → Running，可以反复进行。
    for _ in 0..3 {
        state = state.after_prepare().unwrap();
        assert_eq!(state, DeviceState::Prepared);
        state = state.after_start().unwrap();
        state = state.after_stop().unwrap();
    }
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

mod hw_refine {
    use crate::audio::codec::{HW_PARAMS_BYTES, refine_hardware};

    fn u32_at(bytes: &[u8], offset: usize) -> u32 {
        u32::from_ne_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    fn set_u32(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
    }

    /// `alsa-lib` 开始协商时的样子：所有掩码全选、所有区间覆盖整个取值范围。
    fn unconstrained() -> [u8; HW_PARAMS_BYTES] {
        let mut bytes = [0u8; HW_PARAMS_BYTES];
        for mask in 0..3 {
            for byte in &mut bytes[4 + mask * 32..4 + (mask + 1) * 32] {
                *byte = 0xff;
            }
        }
        for parameter in 8..=19 {
            let offset = 260 + (parameter - 8) * 12;
            set_u32(&mut bytes, offset, 0);
            set_u32(&mut bytes, offset + 4, u32::MAX);
        }
        bytes
    }

    fn interval(bytes: &[u8], parameter: usize) -> (u32, u32, u32) {
        let offset = 260 + (parameter - 8) * 12;
        (
            u32_at(bytes, offset),
            u32_at(bytes, offset + 4),
            u32_at(bytes, offset + 8),
        )
    }

    #[test]
    fn an_unconstrained_request_narrows_to_the_single_hardware_configuration() {
        let mut bytes = unconstrained();
        refine_hardware(&mut bytes).unwrap();
        assert_eq!(
            u32_at(&bytes, 4),
            (1 << 0) | (1 << 3),
            "access: mmap-interleaved and rw-interleaved"
        );
        assert_eq!(u32_at(&bytes, 36), 1 << 14, "format: FLOAT_LE only");
        assert_eq!(u32_at(&bytes, 68), 1, "subformat: standard");
        for word in 1..8 {
            assert_eq!(u32_at(&bytes, 4 + word * 4), 0, "no other access bits");
        }
        assert_eq!(
            interval(&bytes, 10).0..=interval(&bytes, 10).1,
            2..=2,
            "channels"
        );
        assert_eq!(
            (interval(&bytes, 11).0, interval(&bytes, 11).1),
            (48_000, 48_000)
        );
        assert_eq!(
            (interval(&bytes, 13).0, interval(&bytes, 13).1),
            (256, 256),
            "period size"
        );
        assert_eq!(
            (interval(&bytes, 15).0, interval(&bytes, 15).1),
            (4, 4),
            "periods"
        );
        assert_eq!(
            (interval(&bytes, 17).0, interval(&bytes, 17).1),
            (1_024, 1_024),
            "buffer size"
        );
        // 每个被收窄的参数都出现在 cmask 里。
        let changed = u32_at(&bytes, 516);
        for parameter in [0usize, 1, 2, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18] {
            assert_ne!(
                changed & (1 << parameter),
                0,
                "parameter {parameter} must be reported changed"
            );
        }
    }

    #[test]
    fn derived_times_keep_open_endpoints_so_alsa_lib_can_round_them() {
        let mut bytes = unconstrained();
        refine_hardware(&mut bytes).unwrap();
        // 256/48000 s = 5333.33 µs 不是整数：必须是开区间 (5333, 5334)，不能被写成整数点。
        assert_eq!(interval(&bytes, 12), (5_333, 5_334, 0b011));
        assert_eq!(interval(&bytes, 16), (21_333, 21_334, 0b011));
    }

    #[test]
    fn refining_a_refined_request_changes_nothing() {
        let mut bytes = unconstrained();
        refine_hardware(&mut bytes).unwrap();
        let first = bytes;
        refine_hardware(&mut bytes).unwrap();
        assert_eq!(u32_at(&bytes, 516), 0, "idempotent: nothing left to narrow");
        assert_eq!(&bytes[..512], &first[..512]);
    }

    #[test]
    fn a_request_that_misses_the_hardware_point_is_rejected() {
        for (parameter, value) in [(11usize, 44_100u32), (10, 1), (10, 6), (13, 512), (15, 2)] {
            let mut bytes = unconstrained();
            let offset = 260 + (parameter - 8) * 12;
            set_u32(&mut bytes, offset, value);
            set_u32(&mut bytes, offset + 4, value);
            assert!(
                refine_hardware(&mut bytes).is_err(),
                "parameter {parameter} = {value} is unsupported"
            );
        }
        // 只要求 S16_LE：硬件只有 FLOAT_LE，掩码求交为空。
        let mut bytes = unconstrained();
        for byte in &mut bytes[36..68] {
            *byte = 0;
        }
        set_u32(&mut bytes, 36, 1 << 2);
        assert!(
            refine_hardware(&mut bytes).is_err(),
            "S16_LE alone has no common format"
        );
        // 只要求 S16 访问以外的方式（NONINTERLEAVED=1）也为空。
        let mut bytes = unconstrained();
        for byte in &mut bytes[4..36] {
            *byte = 0;
        }
        set_u32(&mut bytes, 4, 1 << 1);
        assert!(refine_hardware(&mut bytes).is_err());
    }

    #[test]
    fn a_range_that_contains_the_hardware_point_is_accepted_and_pinned() {
        let mut bytes = unconstrained();
        set_u32(&mut bytes, 260 + 3 * 12, 8_000);
        set_u32(&mut bytes, 260 + 3 * 12 + 4, 96_000);
        refine_hardware(&mut bytes).unwrap();
        assert_eq!(
            (interval(&bytes, 11).0, interval(&bytes, 11).1),
            (48_000, 48_000)
        );
    }

    #[test]
    fn an_empty_interval_is_rejected_even_if_its_numbers_overlap() {
        let mut bytes = unconstrained();
        set_u32(&mut bytes, 260 + 3 * 12 + 8, 1 << 3);
        assert!(refine_hardware(&mut bytes).is_err());
        let mut inverted = unconstrained();
        set_u32(&mut inverted, 260 + 3 * 12, 50_000);
        set_u32(&mut inverted, 260 + 3 * 12 + 4, 40_000);
        assert!(
            refine_hardware(&mut inverted).is_err(),
            "min > max is empty"
        );
    }
}

mod pcm_info {
    use crate::audio::codec::{PCM_INFO_BYTES, PcmInfo, encode_pcm_info};

    fn text(bytes: &[u8], offset: usize, width: usize) -> &str {
        let field = &bytes[offset..offset + width];
        let length = field
            .iter()
            .position(|&byte| byte == 0)
            .expect("NUL-terminated");
        core::str::from_utf8(&field[..length]).unwrap()
    }

    fn u32_at(bytes: &[u8], offset: usize) -> u32 {
        u32::from_ne_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    #[test]
    fn layout_matches_linux_snd_pcm_info() {
        let bytes = encode_pcm_info(&PcmInfo {
            subdevice: 3,
            subdevices_count: 8,
            subdevices_available: 5,
            card_id: b"LiteOS",
            name: b"LiteOS Playback",
        });
        assert_eq!(bytes.len(), PCM_INFO_BYTES);
        assert_eq!(u32_at(&bytes, 0), 0, "device");
        assert_eq!(u32_at(&bytes, 4), 3, "subdevice");
        assert_eq!(u32_at(&bytes, 8), 0, "stream = playback");
        assert_eq!(u32_at(&bytes, 12), 0, "card");
        assert_eq!(text(&bytes, 16, 64), "LiteOS");
        assert_eq!(text(&bytes, 80, 80), "LiteOS Playback");
        assert_eq!(text(&bytes, 160, 32), "subdevice #3");
        assert_eq!(u32_at(&bytes, 192), 0, "dev_class = generic");
        assert_eq!(u32_at(&bytes, 196), 0, "dev_subclass = generic mix");
        assert_eq!(u32_at(&bytes, 200), 8, "subdevices_count");
        assert_eq!(u32_at(&bytes, 204), 5, "subdevices_avail");
        assert!(
            bytes[208..].iter().all(|&byte| byte == 0),
            "sync and reserved stay zero"
        );
    }

    #[test]
    fn subname_numbers_have_no_leading_zero_and_handle_multiple_digits() {
        let name = |subdevice: u32| {
            let bytes = encode_pcm_info(&PcmInfo {
                subdevice,
                subdevices_count: 8,
                subdevices_available: 0,
                card_id: b"",
                name: b"",
            });
            text(&bytes, 160, 32).to_owned()
        };
        assert_eq!(name(0), "subdevice #0");
        assert_eq!(name(7), "subdevice #7");
        assert_eq!(name(10), "subdevice #10");
        assert_eq!(name(4_294_967_295), "subdevice #4294967295");
    }

    #[test]
    fn overlong_strings_truncate_but_stay_nul_terminated() {
        let long = [b'x'; 200];
        let bytes = encode_pcm_info(&PcmInfo {
            subdevice: 0,
            subdevices_count: 8,
            subdevices_available: 8,
            card_id: &long,
            name: &long,
        });
        assert_eq!(text(&bytes, 16, 64).len(), 63);
        assert_eq!(text(&bytes, 80, 80).len(), 79);
    }
}

#[test]
fn software_parameters_can_change_after_prepare_and_while_running() {
    let mut state = PcmStateOwner::new();
    assert!(state.set_software(256, 1024, 1024, 1 << 31).is_err());
    state.configure().unwrap();
    state.prepare().unwrap();
    state.set_software(512, 1 << 31, 1024, 1 << 31).unwrap();
    state.submit_frames(PCM_PERIOD_FRAMES as u64).unwrap();
    state.start().unwrap();
    state.set_software(256, 1024, 1024, 1 << 31).unwrap();
    assert_eq!(state.state, PcmState::Running);
    assert_eq!(state.application_frames, 256);
    assert_eq!(state.available_min, 256);
    state.disconnect();
    assert!(state.set_software(256, 1024, 1024, 1 << 31).is_err());
}
