//! 内核 CPRNG：ChaCha20 快速密钥擦除生成器。
//!
//! 1. 启动时以固件提供的 `rng-seed` 与计数器抖动派生初始密钥；
//! 2. 每次输出以当前密钥生成 ChaCha20 块流，前 32 字节立即替换为新密钥，其余作为输出——
//!    之后泄露的密钥无法反推此前的输出（前向安全）；
//! 3. `add_entropy` 把新熵与当前密钥混合后再经一次 ChaCha20 块派生新密钥。
//!
//! 熵源驱动在用户态，经 `cprng_add_entropy` 持续注入。

use crate::execution::sync::IrqMutex;

const KEY_BYTES: usize = 32;
const BLOCK_BYTES: usize = 64;
/// 单次注入的熵上限。
pub(crate) const MAX_ENTROPY_INPUT: usize = 256;

struct Generator {
    key: [u8; KEY_BYTES],
    counter: u64,
}

// OWNER: 全局唯一生成器状态；可能在任何上下文抽取，IRQ-safe。
static GENERATOR: IrqMutex<Generator> = IrqMutex::new(Generator {
    key: [0; KEY_BYTES],
    counter: 0,
});

fn quarter_round(state: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    state[a] = state[a].wrapping_add(state[b]);
    state[d] = (state[d] ^ state[a]).rotate_left(16);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_left(12);
    state[a] = state[a].wrapping_add(state[b]);
    state[d] = (state[d] ^ state[a]).rotate_left(8);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_left(7);
}

/// RFC 8439 ChaCha20 块函数（64 位计数器、64 位 nonce 变体）。
fn block(key: &[u8; KEY_BYTES], counter: u64, nonce: u64) -> [u8; BLOCK_BYTES] {
    let mut input = [0u32; 16];
    input[0] = 0x6170_7865;
    input[1] = 0x3320_646e;
    input[2] = 0x7962_2d32;
    input[3] = 0x6b20_6574;
    for (word, chunk) in input[4..12].iter_mut().zip(key.as_chunks::<4>().0) {
        *word = u32::from_le_bytes(*chunk);
    }
    input[12] = counter as u32;
    input[13] = (counter >> 32) as u32;
    input[14] = nonce as u32;
    input[15] = (nonce >> 32) as u32;
    let mut state = input;
    for _ in 0..10 {
        quarter_round(&mut state, 0, 4, 8, 12);
        quarter_round(&mut state, 1, 5, 9, 13);
        quarter_round(&mut state, 2, 6, 10, 14);
        quarter_round(&mut state, 3, 7, 11, 15);
        quarter_round(&mut state, 0, 5, 10, 15);
        quarter_round(&mut state, 1, 6, 11, 12);
        quarter_round(&mut state, 2, 7, 8, 13);
        quarter_round(&mut state, 3, 4, 9, 14);
    }
    let mut output = [0u8; BLOCK_BYTES];
    for (index, chunk) in output.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        chunk.copy_from_slice(&state[index].wrapping_add(input[index]).to_le_bytes());
    }
    output
}

impl Generator {
    fn mix(&mut self, input: &[u8]) {
        let mut key = self.key;
        for (index, byte) in input.iter().enumerate() {
            key[index % KEY_BYTES] ^= *byte;
        }
        let derived = block(&key, self.counter, crate::arch::time::counter());
        self.counter = self.counter.wrapping_add(1);
        self.key.copy_from_slice(&derived[..KEY_BYTES]);
    }

    fn fill(&mut self, output: &mut [u8]) {
        let mut offset = 0;
        while offset < output.len() {
            let stream = block(&self.key, self.counter, 0);
            self.counter = self.counter.wrapping_add(1);
            // 前 32 字节立即成为新密钥，旧密钥不再存在于任何地方。
            self.key.copy_from_slice(&stream[..KEY_BYTES]);
            let chunk = (output.len() - offset).min(BLOCK_BYTES - KEY_BYTES);
            output[offset..offset + chunk].copy_from_slice(&stream[KEY_BYTES..KEY_BYTES + chunk]);
            offset += chunk;
        }
    }
}

/// 以固件启动熵与计数器抖动初始化。
pub(crate) fn initialize() {
    let mut generator = GENERATOR.lock();
    generator.mix(crate::platform::boot_entropy());
    for _ in 0..16 {
        let jitter = (crate::arch::time::counter()).to_le_bytes();
        generator.mix(&jitter);
    }
}

/// 填充随机字节。
pub(crate) fn fill(output: &mut [u8]) {
    GENERATOR.lock().fill(output);
}

/// 混入外部熵（≤ [`MAX_ENTROPY_INPUT`] 字节）。
pub(crate) fn add_entropy(input: &[u8]) {
    GENERATOR
        .lock()
        .mix(&input[..input.len().min(MAX_ENTROPY_INPUT)]);
}
