//! seccomp classic BPF 的不可变程序、验证与有界执行。

use alloc::vec::Vec;

use super::SeccompError;

pub(super) const MAX_INSTRUCTIONS: usize = 4096;

#[derive(Debug, Clone, Copy)]
struct Instruction {
    code: u16,
    yes: u8,
    no: u8,
    operand: u32,
}

/// 验证后的 seccomp 程序；只有 forward branch，执行步数不超过 instruction count。
#[derive(Debug)]
pub(super) struct Program {
    instructions: Vec<Instruction>,
}

impl Program {
    /// 解码并验证用户 sock_filter 数组；所有 allocation 在 thread security lock 外执行。
    ///
    /// 空/过长程序、非法 opcode、越界访问/跳转、未初始化 scratch 或末尾非 RET 返回 Invalid；
    /// backing allocation 失败返回 OutOfMemory。验证对照固定 Linux 的 classic/ seccomp checker。
    pub(super) fn prepare(bytes: &[u8]) -> Result<Self, SeccompError> {
        let length = bytes.len() / 8;
        if !bytes.len().is_multiple_of(8) || !(1..=MAX_INSTRUCTIONS).contains(&length) {
            return Err(SeccompError::Invalid);
        }
        let mut instructions = Vec::new();
        instructions
            .try_reserve_exact(length)
            .map_err(|_| SeccompError::OutOfMemory)?;
        for bytes in bytes.as_chunks::<8>().0 {
            instructions.push(Instruction {
                code: u16::from_ne_bytes(bytes[..2].try_into().unwrap()),
                yes: bytes[2],
                no: bytes[3],
                operand: u32::from_ne_bytes(bytes[4..].try_into().unwrap()),
            });
        }
        for (pc, instruction) in instructions.iter().enumerate() {
            let Instruction { code, operand, .. } = *instruction;
            let remaining = length - pc - 1;
            let valid = match code {
                0x20 => operand < 64 && operand & 3 == 0, // LD W ABS seccomp_data
                0x02 | 0x03 | 0x60 | 0x61 => operand < 16, // ST/STX/LD[X] MEM
                0x34 => operand != 0,                     // DIV K
                0x64 | 0x74 => operand < 32,              // LSH/RSH K
                0x05 => (operand as usize) < remaining,   // JA
                0x15 | 0x1d | 0x25 | 0x2d | 0x35 | 0x3d | 0x45 | 0x4d => {
                    (instruction.yes as usize) < remaining && (instruction.no as usize) < remaining
                }
                // Linux seccomp whitelist deliberately excludes packet loads and MOD.
                0x00 | 0x01 | 0x80 | 0x81 | 0x06 | 0x16 | 0x07 | 0x87 | 0x84 | 0x04 | 0x0c
                | 0x14 | 0x1c | 0x24 | 0x2c | 0x3c | 0x44 | 0x4c | 0x54 | 0x5c | 0x6c | 0x7c
                | 0xa4 | 0xac => true,
                _ => false,
            };
            if !valid {
                return Err(SeccompError::Invalid);
            }
        }
        if !matches!(instructions.last().unwrap().code, 0x06 | 0x16) {
            return Err(SeccompError::Invalid);
        }
        // OWNER: 每个 forward target 的 mask 是全部 incoming path 已初始化 scratch 的交集。
        // 无该验证，LD MEM 会读取未初始化的 kernel 栈数据。分支之后不沿虚假的 fallthrough 传播。
        let mut incoming = Vec::new();
        incoming
            .try_reserve_exact(length)
            .map_err(|_| SeccompError::OutOfMemory)?;
        incoming.resize(length, u16::MAX);
        let mut initialized = 0u16;
        for (pc, instruction) in instructions.iter().enumerate() {
            initialized &= incoming[pc];
            match instruction.code {
                0x02 | 0x03 => initialized |= 1 << instruction.operand,
                0x60 | 0x61 if initialized & (1 << instruction.operand) == 0 => {
                    return Err(SeccompError::Invalid);
                }
                0x05 => {
                    incoming[pc + 1 + instruction.operand as usize] &= initialized;
                    initialized = u16::MAX;
                }
                0x15 | 0x1d | 0x25 | 0x2d | 0x35 | 0x3d | 0x45 | 0x4d => {
                    incoming[pc + 1 + instruction.yes as usize] &= initialized;
                    incoming[pc + 1 + instruction.no as usize] &= initialized;
                    initialized = u16::MAX;
                }
                _ => {}
            }
        }
        Ok(Self { instructions })
    }

    pub(super) fn length(&self) -> usize {
        self.instructions.len()
    }

    /// 执行已验证程序；无锁、allocation、user pointer 或 backward branch。
    pub(super) fn evaluate(&self, data: &[u32; 16]) -> u32 {
        let (mut accumulator, mut index) = (0u32, 0u32);
        let mut scratch = [0u32; 16];
        let mut pc = 0;
        loop {
            let instruction = self.instructions[pc];
            let operand = if instruction.code & 8 == 0 {
                instruction.operand
            } else {
                index
            };
            match instruction.code {
                0x00 => accumulator = operand,
                0x01 => index = operand,
                0x20 => accumulator = data[instruction.operand as usize / 4],
                0x80 => accumulator = 64,
                0x81 => index = 64,
                0x60 => accumulator = scratch[instruction.operand as usize],
                0x61 => index = scratch[instruction.operand as usize],
                0x02 => scratch[instruction.operand as usize] = accumulator,
                0x03 => scratch[instruction.operand as usize] = index,
                0x04 | 0x0c => accumulator = accumulator.wrapping_add(operand),
                0x14 | 0x1c => accumulator = accumulator.wrapping_sub(operand),
                0x24 | 0x2c => accumulator = accumulator.wrapping_mul(operand),
                0x34 | 0x3c => {
                    if operand == 0 {
                        return 0; // Linux BPF runtime DIV X zero returns KILL_THREAD.
                    }
                    accumulator /= operand;
                }
                0x44 | 0x4c => accumulator |= operand,
                0x54 | 0x5c => accumulator &= operand,
                0x64 | 0x6c => accumulator = accumulator.wrapping_shl(operand),
                0x74 | 0x7c => accumulator = accumulator.wrapping_shr(operand),
                0xa4 | 0xac => accumulator ^= operand,
                0x84 => accumulator = accumulator.wrapping_neg(),
                0x07 => index = accumulator,
                0x87 => accumulator = index,
                0x05 => pc += instruction.operand as usize,
                0x15 | 0x1d | 0x25 | 0x2d | 0x35 | 0x3d | 0x45 | 0x4d => {
                    let condition = match instruction.code & 0xf0 {
                        0x10 => accumulator == operand,
                        0x20 => accumulator > operand,
                        0x30 => accumulator >= operand,
                        0x40 => accumulator & operand != 0,
                        _ => unreachable!(),
                    };
                    pc += if condition {
                        instruction.yes
                    } else {
                        instruction.no
                    } as usize;
                }
                0x06 => return instruction.operand,
                0x16 => return accumulator,
                _ => unreachable!("validated seccomp opcode"),
            }
            pc += 1;
        }
    }
}
