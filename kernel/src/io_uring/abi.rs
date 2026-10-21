//! io_uring 的 Linux v7.1 UAPI（`include/uapi/linux/io_uring.h`）：布局常量、参数校验与纯编解码。
//!
//! 本文件不接触内存、task 或 fd，因此 host 单测直接加载它。ring 的实际共享内存与同步在 `ring`，
//! syscall 语义在 `mod`；这里只回答“字节怎么排、哪些值合法”。

/// 单个 ring 的 SQ 条目上限，与 Linux 的 `IORING_MAX_ENTRIES` 相同。跨 ring 的总量由每 UID 的
/// `IO_URING_BYTES_PER_UID` 预算约束：缺失预算时这个上限会让一个用户用几十个 ring 耗尽物理内存。
/// 超过上限且没有 `IORING_SETUP_CLAMP` 返回 `EINVAL`。
pub(crate) const MAX_ENTRIES: u32 = 32768;
/// CQ 条目上限，与 Linux 一样是 SQ 上限的两倍。
pub(crate) const MAX_CQ_ENTRIES: u32 = 2 * MAX_ENTRIES;

pub(crate) const SETUP_CQSIZE: u32 = 1 << 3;
pub(crate) const SETUP_CLAMP: u32 = 1 << 4;
/// 本实现接受的全部 `io_uring_setup` flags。其余已定义位（IOPOLL、SQPOLL、SQE128 等）见 ABI 矩阵。
pub(crate) const SUPPORTED_SETUP_FLAGS: u32 = SETUP_CQSIZE | SETUP_CLAMP;

pub(crate) const FEAT_SINGLE_MMAP: u32 = 1 << 0;
pub(crate) const FEAT_NODROP: u32 = 1 << 1;
pub(crate) const FEAT_SUBMIT_STABLE: u32 = 1 << 2;
/// `off == -1` 表示使用并推进文件的当前 position。
pub(crate) const FEAT_RW_CUR_POS: u32 = 1 << 3;
/// 请求使用提交者在提交时刻的凭据，而不是 ring 创建者的；没有注册 personality。
pub(crate) const FEAT_CUR_PERSONALITY: u32 = 1 << 4;
/// 会阻塞的 pipe/socket 等请求由内核挂起并在就绪后完成，而不是向用户态返回 `EAGAIN`。
pub(crate) const FEAT_FAST_POLL: u32 = 1 << 5;
pub(crate) const FEAT_EXT_ARG: u32 = 1 << 8;
/// 本实现报告的 features；每一位都必须有对应实现，liburing 会据此选择路径。
pub(crate) const FEATURES: u32 = FEAT_SINGLE_MMAP
    | FEAT_NODROP
    | FEAT_SUBMIT_STABLE
    | FEAT_RW_CUR_POS
    | FEAT_CUR_PERSONALITY
    | FEAT_FAST_POLL
    | FEAT_EXT_ARG;

pub(crate) const OFF_SQ_RING: u64 = 0;
pub(crate) const OFF_CQ_RING: u64 = 0x800_0000;
pub(crate) const OFF_SQES: u64 = 0x1000_0000;

pub(crate) const ENTER_GETEVENTS: u32 = 1 << 0;
pub(crate) const ENTER_EXT_ARG: u32 = 1 << 3;
/// 本实现接受的 `io_uring_enter` flags。
pub(crate) const SUPPORTED_ENTER_FLAGS: u32 = ENTER_GETEVENTS | ENTER_EXT_ARG;

pub(crate) const SQ_CQ_OVERFLOW: u32 = 1 << 1;

pub(crate) const SQE_BYTES: usize = 64;
pub(crate) const CQE_BYTES: usize = 16;
pub(crate) const PARAMS_BYTES: usize = 120;
pub(crate) const GETEVENTS_ARG_BYTES: usize = 24;

pub(crate) const IOSQE_FIXED_FILE: u8 = 1 << 0;
pub(crate) const IOSQE_IO_DRAIN: u8 = 1 << 1;
pub(crate) const IOSQE_IO_LINK: u8 = 1 << 2;
pub(crate) const IOSQE_IO_HARDLINK: u8 = 1 << 3;
pub(crate) const IOSQE_ASYNC: u8 = 1 << 4;
pub(crate) const IOSQE_BUFFER_SELECT: u8 = 1 << 5;
pub(crate) const IOSQE_CQE_SKIP_SUCCESS: u8 = 1 << 6;
/// Linux `SQE_VALID_FLAGS`：之外的位是 `EINVAL`（向前兼容检查）。
pub(crate) const SQE_VALID_FLAGS: u8 = IOSQE_FIXED_FILE
    | IOSQE_IO_DRAIN
    | IOSQE_IO_LINK
    | IOSQE_IO_HARDLINK
    | IOSQE_ASYNC
    | IOSQE_BUFFER_SELECT
    | IOSQE_CQE_SKIP_SUCCESS;

pub(crate) const OP_NOP: u8 = 0;
pub(crate) const OP_READV: u8 = 1;
pub(crate) const OP_WRITEV: u8 = 2;
pub(crate) const OP_FSYNC: u8 = 3;
pub(crate) const OP_POLL_ADD: u8 = 6;
pub(crate) const OP_POLL_REMOVE: u8 = 7;
pub(crate) const OP_TIMEOUT: u8 = 11;
pub(crate) const OP_TIMEOUT_REMOVE: u8 = 12;
pub(crate) const OP_ACCEPT: u8 = 13;
pub(crate) const OP_ASYNC_CANCEL: u8 = 14;
pub(crate) const OP_LINK_TIMEOUT: u8 = 15;
pub(crate) const OP_CONNECT: u8 = 16;
pub(crate) const OP_OPENAT: u8 = 18;
pub(crate) const OP_CLOSE: u8 = 19;
pub(crate) const OP_STATX: u8 = 21;
pub(crate) const OP_READ: u8 = 22;
pub(crate) const OP_WRITE: u8 = 23;
pub(crate) const OP_SEND: u8 = 26;
pub(crate) const OP_RECV: u8 = 27;
/// `IORING_OP_LAST`：v7.1 共 65 个 opcode（0..=64）。
pub(crate) const OP_LAST: u8 = 65;
pub(crate) const NOP_INJECT_RESULT: u32 = 1 << 0;

pub(crate) const REGISTER_PROBE: u32 = 8;
pub(crate) const PROBE_HEADER_BYTES: usize = 16;
pub(crate) const PROBE_OP_BYTES: usize = 8;
pub(crate) const PROBE_OP_SUPPORTED: u16 = 1 << 0;

// ring 共享内存头部布局。head/tail 各占一条 64 字节 cache line，避免内核与用户态的生产者/消费者
// 互相使对方的 line 失效；偏移通过 `io_uring_params` 告知用户态，liburing 不假定固定值。
pub(crate) const SQ_HEAD: u32 = 0;
pub(crate) const SQ_TAIL: u32 = 64;
pub(crate) const CQ_HEAD: u32 = 128;
pub(crate) const CQ_TAIL: u32 = 192;
pub(crate) const SQ_RING_MASK: u32 = 256;
pub(crate) const CQ_RING_MASK: u32 = 260;
pub(crate) const SQ_RING_ENTRIES: u32 = 264;
pub(crate) const CQ_RING_ENTRIES: u32 = 268;
pub(crate) const SQ_DROPPED: u32 = 272;
pub(crate) const SQ_FLAGS: u32 = 276;
pub(crate) const CQ_FLAGS: u32 = 280;
pub(crate) const CQ_OVERFLOW: u32 = 284;
pub(crate) const CQES_OFFSET: u32 = 320;

/// `io_uring_setup` 的参数错误；统一映射 `EINVAL`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InvalidSetup;

/// 用户提交的 `struct io_uring_params` 输入字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SetupParams {
    pub(crate) cq_entries: u32,
    pub(crate) flags: u32,
}

impl SetupParams {
    /// 解码并校验输入字段。
    ///
    /// # Errors
    ///
    /// 含未实现的 flags、非零保留字段或 SQPOLL/ATTACH_WQ 配套字段时返回 [`InvalidSetup`]。
    pub(crate) fn decode(bytes: &[u8; PARAMS_BYTES]) -> Result<Self, InvalidSetup> {
        let word = |offset: usize| {
            u32::from_ne_bytes(bytes[offset..offset + 4].try_into().expect("four bytes"))
        };
        let flags = word(8);
        // sq_thread_cpu(12)、sq_thread_idle(16)、wq_fd(24) 只在 SQ_AFF/SQPOLL/ATTACH_WQ 下有意义，
        // 这些 flags 未实现，因此 Linux 不会读取它们；这里同样忽略。
        let reserved = [word(28), word(32), word(36)];
        if flags & !SUPPORTED_SETUP_FLAGS != 0 || reserved != [0; 3] {
            return Err(InvalidSetup);
        }
        Ok(Self {
            cq_entries: word(4),
            flags,
        })
    }
}

/// 已校验的 ring 尺寸与共享内存布局。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Geometry {
    pub(crate) sq_entries: u32,
    pub(crate) cq_entries: u32,
    /// SQ array 在 ring 区域内的偏移（位于 CQE 数组之后）。
    pub(crate) array_offset: u32,
    /// ring 区域（头部 + CQE + SQ array）字节数，未按页取整。
    pub(crate) ring_bytes: usize,
    /// SQE 数组字节数，未按页取整。
    pub(crate) sqes_bytes: usize,
}

impl Geometry {
    /// 按 Linux `io_uring_fill_params` 的顺序校验并取整。
    ///
    /// # Parameters
    ///
    /// - `entries`: `io_uring_setup` 的 `entries`。
    /// - `params`: 已解码的输入字段。
    ///
    /// # Errors
    ///
    /// `entries` 为零、超过上限且未 CLAMP、CQSIZE 缺失或小于 SQ 时返回 [`InvalidSetup`]。
    pub(crate) fn new(entries: u32, params: SetupParams) -> Result<Self, InvalidSetup> {
        if entries == 0 {
            return Err(InvalidSetup);
        }
        let entries = if entries > MAX_ENTRIES {
            if params.flags & SETUP_CLAMP == 0 {
                return Err(InvalidSetup);
            }
            MAX_ENTRIES
        } else {
            entries
        };
        let sq_entries = entries.next_power_of_two();
        let cq_entries = if params.flags & SETUP_CQSIZE != 0 {
            if params.cq_entries == 0 {
                return Err(InvalidSetup);
            }
            let requested = if params.cq_entries > MAX_CQ_ENTRIES {
                if params.flags & SETUP_CLAMP == 0 {
                    return Err(InvalidSetup);
                }
                MAX_CQ_ENTRIES
            } else {
                params.cq_entries
            };
            let rounded = requested.next_power_of_two();
            if rounded < sq_entries {
                return Err(InvalidSetup);
            }
            rounded
        } else {
            2 * sq_entries
        };
        let array_offset = CQES_OFFSET + cq_entries * CQE_BYTES as u32;
        Ok(Self {
            sq_entries,
            cq_entries,
            array_offset,
            ring_bytes: array_offset as usize + sq_entries as usize * 4,
            sqes_bytes: sq_entries as usize * SQE_BYTES,
        })
    }

    /// 编码返回给用户态的 `struct io_uring_params`。
    pub(crate) fn encode_params(&self, flags: u32) -> [u8; PARAMS_BYTES] {
        let mut bytes = [0u8; PARAMS_BYTES];
        let mut put = |offset: usize, value: u32| {
            bytes[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
        };
        put(0, self.sq_entries);
        put(4, self.cq_entries);
        put(8, flags);
        put(20, FEATURES);
        // io_sqring_offsets：head tail ring_mask ring_entries flags dropped array
        for (index, value) in [
            SQ_HEAD,
            SQ_TAIL,
            SQ_RING_MASK,
            SQ_RING_ENTRIES,
            SQ_FLAGS,
            SQ_DROPPED,
            self.array_offset,
        ]
        .into_iter()
        .enumerate()
        {
            put(40 + index * 4, value);
        }
        // io_cqring_offsets：head tail ring_mask ring_entries overflow cqes flags
        for (index, value) in [
            CQ_HEAD,
            CQ_TAIL,
            CQ_RING_MASK,
            CQ_RING_ENTRIES,
            CQ_OVERFLOW,
            CQES_OFFSET,
            CQ_FLAGS,
        ]
        .into_iter()
        .enumerate()
        {
            put(80 + index * 4, value);
        }
        bytes
    }
}

/// 从 SQE 数组复制出的 submission entry（SUBMIT_STABLE：之后用户态可复用该槽）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Sqe {
    pub(crate) opcode: u8,
    pub(crate) flags: u8,
    pub(crate) ioprio: u16,
    pub(crate) fd: i32,
    pub(crate) off: u64,
    pub(crate) addr: u64,
    pub(crate) len: u32,
    /// 与 opcode 相关的 32 位 flags 联合（`rw_flags`/`nop_flags`/`timeout_flags`…）。
    pub(crate) op_flags: u32,
    pub(crate) user_data: u64,
    pub(crate) buf_index: u16,
    pub(crate) personality: u16,
    pub(crate) splice_fd_in: i32,
    pub(crate) addr3: u64,
    /// offset 56：Linux 读写类 opcode 的 `attr_type_mask`，其余 opcode 的保留字。
    pub(crate) pad2: u64,
}

impl Sqe {
    /// 解码 64 字节 SQE；任何字节组合都是合法输入，语义校验在 opcode prep。
    pub(crate) fn decode(bytes: &[u8; SQE_BYTES]) -> Self {
        let u16_at = |o: usize| u16::from_ne_bytes(bytes[o..o + 2].try_into().expect("two bytes"));
        let u32_at = |o: usize| u32::from_ne_bytes(bytes[o..o + 4].try_into().expect("four bytes"));
        let u64_at =
            |o: usize| u64::from_ne_bytes(bytes[o..o + 8].try_into().expect("eight bytes"));
        Self {
            opcode: bytes[0],
            flags: bytes[1],
            ioprio: u16_at(2),
            fd: u32_at(4) as i32,
            off: u64_at(8),
            addr: u64_at(16),
            len: u32_at(24),
            op_flags: u32_at(28),
            user_data: u64_at(32),
            buf_index: u16_at(40),
            personality: u16_at(42),
            splice_fd_in: u32_at(44) as i32,
            addr3: u64_at(48),
            pad2: u64_at(56),
        }
    }
}

/// 一个完成事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cqe {
    pub(crate) user_data: u64,
    pub(crate) res: i32,
    pub(crate) flags: u32,
}

impl Cqe {
    /// 编码为 16 字节 CQE。
    pub(crate) fn encode(self) -> [u8; CQE_BYTES] {
        let mut bytes = [0u8; CQE_BYTES];
        bytes[..8].copy_from_slice(&self.user_data.to_ne_bytes());
        bytes[8..12].copy_from_slice(&self.res.to_ne_bytes());
        bytes[12..16].copy_from_slice(&self.flags.to_ne_bytes());
        bytes
    }
}

/// opcode 是否已实现；probe 与 issue 共用，避免二者对支持集合各持一份。
pub(crate) const fn op_supported(opcode: u8) -> bool {
    matches!(
        opcode,
        OP_NOP
            | OP_READV
            | OP_WRITEV
            | OP_FSYNC
            | OP_POLL_ADD
            | OP_POLL_REMOVE
            | OP_TIMEOUT
            | OP_TIMEOUT_REMOVE
            | OP_ASYNC_CANCEL
            | OP_LINK_TIMEOUT
            | OP_OPENAT
            | OP_STATX
            | OP_CLOSE
            | OP_READ
            | OP_WRITE
            | OP_ACCEPT
            | OP_CONNECT
            | OP_SEND
            | OP_RECV
    )
}

/// `IORING_REGISTER_PROBE` 的 `ops_len`：用户 `nr_args` 与 `IORING_OP_LAST` 取小。
pub(crate) fn probe_ops(nr_args: u32) -> usize {
    nr_args.min(u32::from(OP_LAST)) as usize
}

/// 编码 probe 响应：`last_op = IORING_OP_LAST - 1`，逐 opcode 标注是否支持。
pub(crate) fn encode_probe(ops: usize, output: &mut [u8]) {
    assert_eq!(output.len(), PROBE_HEADER_BYTES + ops * PROBE_OP_BYTES);
    output.fill(0);
    output[0] = OP_LAST - 1;
    output[1] = ops as u8;
    for index in 0..ops {
        let entry = &mut output[PROBE_HEADER_BYTES + index * PROBE_OP_BYTES..][..PROBE_OP_BYTES];
        entry[0] = index as u8;
        let flags = if op_supported(index as u8) {
            PROBE_OP_SUPPORTED
        } else {
            0
        };
        entry[2..4].copy_from_slice(&flags.to_ne_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(flags: u32, cq_entries: u32) -> SetupParams {
        SetupParams { cq_entries, flags }
    }

    #[test]
    fn sq_entries_round_up_and_cq_defaults_to_twice() {
        let geometry = Geometry::new(5, params(0, 0)).unwrap();
        assert_eq!((geometry.sq_entries, geometry.cq_entries), (8, 16));
        assert_eq!(geometry.array_offset, CQES_OFFSET + 16 * 16);
        assert_eq!(geometry.ring_bytes, geometry.array_offset as usize + 8 * 4);
        assert_eq!(geometry.sqes_bytes, 8 * 64);
    }

    #[test]
    fn entries_zero_and_over_limit_follow_clamp() {
        assert_eq!(Geometry::new(0, params(0, 0)), Err(InvalidSetup));
        assert_eq!(
            Geometry::new(MAX_ENTRIES + 1, params(0, 0)),
            Err(InvalidSetup)
        );
        let clamped = Geometry::new(MAX_ENTRIES + 1, params(SETUP_CLAMP, 0)).unwrap();
        assert_eq!(clamped.sq_entries, MAX_ENTRIES);
    }

    #[test]
    fn cqsize_requires_a_value_not_below_the_sq_and_is_rounded() {
        assert_eq!(Geometry::new(8, params(SETUP_CQSIZE, 0)), Err(InvalidSetup));
        assert_eq!(Geometry::new(8, params(SETUP_CQSIZE, 4)), Err(InvalidSetup));
        let geometry = Geometry::new(8, params(SETUP_CQSIZE, 9)).unwrap();
        assert_eq!(geometry.cq_entries, 16);
        assert_eq!(
            Geometry::new(8, params(SETUP_CQSIZE, MAX_CQ_ENTRIES + 1)),
            Err(InvalidSetup)
        );
        assert_eq!(
            Geometry::new(8, params(SETUP_CQSIZE | SETUP_CLAMP, u32::MAX))
                .unwrap()
                .cq_entries,
            MAX_CQ_ENTRIES
        );
    }

    #[test]
    fn params_reject_unimplemented_flags_and_reserved_words() {
        let mut bytes = [0u8; PARAMS_BYTES];
        assert!(SetupParams::decode(&bytes).is_ok());
        bytes[8..12].copy_from_slice(&(1u32 << 1).to_ne_bytes()); // SQPOLL
        assert_eq!(SetupParams::decode(&bytes), Err(InvalidSetup));
        bytes[8..12].copy_from_slice(&0u32.to_ne_bytes());
        bytes[32] = 1; // resv[1]
        assert_eq!(SetupParams::decode(&bytes), Err(InvalidSetup));
    }

    #[test]
    fn encoded_params_publish_geometry_features_and_offsets() {
        let geometry = Geometry::new(4, params(0, 0)).unwrap();
        let bytes = geometry.encode_params(0);
        let word = |o: usize| u32::from_ne_bytes(bytes[o..o + 4].try_into().unwrap());
        assert_eq!((word(0), word(4), word(8)), (4, 8, 0));
        assert_eq!(word(20), FEATURES);
        assert_eq!(word(40), SQ_HEAD);
        assert_eq!(word(64), geometry.array_offset); // sq_off.array
        assert_eq!(word(80), CQ_HEAD);
        assert_eq!(word(100), CQES_OFFSET); // cq_off.cqes
    }

    #[test]
    fn sqe_decode_places_every_field_at_its_uapi_offset() {
        let mut bytes = [0u8; SQE_BYTES];
        bytes[0] = 7;
        bytes[1] = IOSQE_ASYNC;
        bytes[2..4].copy_from_slice(&0x0102u16.to_ne_bytes());
        bytes[4..8].copy_from_slice(&(-3i32).to_ne_bytes());
        bytes[8..16].copy_from_slice(&0x1111u64.to_ne_bytes());
        bytes[16..24].copy_from_slice(&0x2222u64.to_ne_bytes());
        bytes[24..28].copy_from_slice(&0x3333u32.to_ne_bytes());
        bytes[28..32].copy_from_slice(&0x4444u32.to_ne_bytes());
        bytes[32..40].copy_from_slice(&0x5555u64.to_ne_bytes());
        bytes[40..42].copy_from_slice(&9u16.to_ne_bytes());
        bytes[42..44].copy_from_slice(&10u16.to_ne_bytes());
        bytes[44..48].copy_from_slice(&(-4i32).to_ne_bytes());
        bytes[48..56].copy_from_slice(&0x6666u64.to_ne_bytes());
        bytes[56..64].copy_from_slice(&0x7777u64.to_ne_bytes());
        let sqe = Sqe::decode(&bytes);
        assert_eq!(
            (sqe.opcode, sqe.flags, sqe.ioprio),
            (7, IOSQE_ASYNC, 0x0102)
        );
        assert_eq!((sqe.fd, sqe.off, sqe.addr), (-3, 0x1111, 0x2222));
        assert_eq!(
            (sqe.len, sqe.op_flags, sqe.user_data),
            (0x3333, 0x4444, 0x5555)
        );
        assert_eq!((sqe.buf_index, sqe.personality), (9, 10));
        assert_eq!(
            (sqe.splice_fd_in, sqe.addr3, sqe.pad2),
            (-4, 0x6666, 0x7777)
        );
    }

    #[test]
    fn cqe_encoding_is_user_data_res_flags() {
        let bytes = Cqe {
            user_data: 0xAABB,
            res: -22,
            flags: 3,
        }
        .encode();
        assert_eq!(u64::from_ne_bytes(bytes[..8].try_into().unwrap()), 0xAABB);
        assert_eq!(i32::from_ne_bytes(bytes[8..12].try_into().unwrap()), -22);
        assert_eq!(u32::from_ne_bytes(bytes[12..16].try_into().unwrap()), 3);
    }

    #[test]
    fn probe_reports_only_implemented_opcodes_and_clamps_the_length() {
        assert_eq!(probe_ops(1000), usize::from(OP_LAST));
        assert_eq!(probe_ops(3), 3);
        let mut output = vec![0xffu8; PROBE_HEADER_BYTES + 5 * PROBE_OP_BYTES];
        encode_probe(5, &mut output);
        assert_eq!((output[0], output[1]), (OP_LAST - 1, 5));
        let flags = |op: usize| {
            u16::from_ne_bytes(
                output[PROBE_HEADER_BYTES + op * PROBE_OP_BYTES + 2..][..2]
                    .try_into()
                    .unwrap(),
            )
        };
        for supported in [OP_NOP, OP_READV, OP_WRITEV, OP_FSYNC] {
            assert_eq!(flags(usize::from(supported)), PROBE_OP_SUPPORTED);
        }
        // IORING_OP_READ_FIXED 需要缓冲区注册，目前未实现。
        assert_eq!(flags(4), 0);
        assert_eq!(output[PROBE_HEADER_BYTES + PROBE_OP_BYTES], 1); // ops[1].op
    }
}
