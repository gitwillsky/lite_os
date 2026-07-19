/// 判断 illegal-instruction trap 对应的用户指令是否属于 RISC-V F/D 扩展。
///
/// `FS=Off` 时，硬件把浮点 load/store、融合乘加和算术指令报告为 illegal instruction。
/// 这里只识别会访问浮点状态的标准 16/32-bit 编码；缺失该精确解码会把真正的非法指令
/// 错当成 lazy-FP 首次使用，从而形成永不终止的 trap loop。
fn is_floating_point_instruction(bytes: &[u8]) -> bool {
    let Some(first) = bytes.get(..2) else {
        return false;
    };
    let halfword = u16::from_le_bytes([first[0], first[1]]);
    if halfword & 0b11 != 0b11 {
        let quadrant = halfword & 0b11;
        let funct3 = (halfword >> 13) & 0b111;
        return matches!(
            (quadrant, funct3),
            (0b00, 0b001 | 0b101) | (0b10, 0b001 | 0b101)
        );
    }

    let Some(instruction) = bytes.get(..4) else {
        return false;
    };
    let instruction = u32::from_le_bytes([
        instruction[0],
        instruction[1],
        instruction[2],
        instruction[3],
    ]);
    let opcode = instruction & 0x7f;
    if matches!(opcode, 0x07 | 0x27 | 0x43 | 0x47 | 0x4b | 0x4f | 0x53) {
        return true;
    }
    // fflags/frm/fcsr (CSR 0x001..0x003) are part of the FP state and are inaccessible while
    // FS=Off. funct3=0 is an environment/system instruction, not a Zicsr access.
    let csr = (instruction >> 20) & 0xfff;
    opcode == 0x73 && (instruction >> 12) & 0b111 != 0 && (1..=3).contains(&csr)
}

/// 从用户 instruction stream 精确读取并判断一次 lazy-FP trap。
///
/// @param program_counter trap 保存的用户 PC。
/// @param copy architecture-neutral copyin adapter；每次只请求一个 16-bit halfword。
/// @return 完整读取且编码属于 F/D 或 FP CSR 时返回 true；copy fault/overflow/其他编码返回 false。
pub(crate) fn is_floating_point_instruction_at(
    program_counter: usize,
    mut copy: impl FnMut(usize, &mut [u8]) -> bool,
) -> bool {
    let mut instruction = [0u8; 4];
    if !copy(program_counter, &mut instruction[..2]) {
        return false;
    }
    let prefix = u16::from_le_bytes([instruction[0], instruction[1]]);
    let length = if prefix & 0b11 == 0b11 { 4 } else { 2 };
    if length == 4 {
        let Some(suffix_address) = program_counter.checked_add(2) else {
            return false;
        };
        if !copy(suffix_address, &mut instruction[2..]) {
            return false;
        }
    }
    is_floating_point_instruction(&instruction[..length])
}

#[cfg(test)]
mod tests {
    use super::{is_floating_point_instruction, is_floating_point_instruction_at};

    #[test]
    fn recognizes_standard_fp_opcode_families() {
        for opcode in [0x07u32, 0x27, 0x43, 0x47, 0x4b, 0x4f, 0x53] {
            assert!(is_floating_point_instruction(&opcode.to_le_bytes()));
        }
    }

    #[test]
    fn recognizes_rv64_compressed_double_encodings() {
        for instruction in [0x2000u16, 0xa000, 0x2002, 0xa002] {
            assert!(is_floating_point_instruction(&instruction.to_le_bytes()));
        }
    }

    #[test]
    fn rejects_integer_and_truncated_encodings() {
        assert!(!is_floating_point_instruction(
            &0x0000_0013u32.to_le_bytes()
        ));
        assert!(!is_floating_point_instruction(
            &0x0000_0073u32.to_le_bytes()
        ));
        assert!(!is_floating_point_instruction(&[0x53]));
        assert!(!is_floating_point_instruction(&[]));
    }

    #[test]
    fn recognizes_fp_control_status_register_accesses_only() {
        for csr in 1u32..=3 {
            let csrrs = csr << 20 | 0b010 << 12 | 0x73;
            let csrrwi = csr << 20 | 0b101 << 12 | 0x73;
            assert!(is_floating_point_instruction(&csrrs.to_le_bytes()));
            assert!(is_floating_point_instruction(&csrrwi.to_le_bytes()));
        }
        let cycle = 0xc00u32 << 20 | 0b010 << 12 | 0x73;
        let ecall = 0x0000_0073u32;
        assert!(!is_floating_point_instruction(&cycle.to_le_bytes()));
        assert!(!is_floating_point_instruction(&ecall.to_le_bytes()));
    }

    #[test]
    fn compressed_fp_at_page_end_never_reads_the_unmapped_next_page() {
        let page_end = 4096usize;
        let compressed_fld = 0x2000u16.to_le_bytes();
        let mut reads = 0;
        let recognized = is_floating_point_instruction_at(page_end - 2, |address, destination| {
            reads += 1;
            if address == page_end - 2 && destination.len() == 2 {
                destination.copy_from_slice(&compressed_fld);
                true
            } else {
                false
            }
        });
        assert!(recognized);
        assert_eq!(reads, 1);
    }

    #[test]
    fn thirty_two_bit_fp_instruction_reads_two_exact_halfwords() {
        let instruction = 0x0000_0053u32.to_le_bytes();
        let mut requests = alloc::vec::Vec::new();
        let recognized = is_floating_point_instruction_at(0x1000, |address, destination| {
            requests.push((address, destination.len()));
            let offset = address - 0x1000;
            destination.copy_from_slice(&instruction[offset..offset + destination.len()]);
            true
        });
        assert!(recognized);
        assert_eq!(requests, alloc::vec![(0x1000, 2), (0x1002, 2)]);
    }
}
