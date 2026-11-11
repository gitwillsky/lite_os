//! UDP/RAW 共用的有界 datagram ring；完整报文连续存放，wrap padding 由所属记录记账，不额外占槽。
use alloc::vec::Vec;
use network_protocol::SocketError;

const PACKETS: usize = 8;

#[derive(Clone, Copy)]
struct Record<M> {
    metadata: M,
    start: usize,
    length: usize,
    charge: usize,
}

/// 同一 endpoint owner 借出/归还的实际队列；存储只在 bind/send 准备，报文路径不分配。
pub(super) struct Queue<M: Copy> {
    bytes: Vec<u8>,
    records: [Option<Record<M>>; PACKETS],
    front: usize,
    length: usize,
    write: usize,
    used: usize,
}
impl<M: Copy> Queue<M> {
    pub(super) fn new() -> Self {
        Self {
            bytes: Vec::new(),
            records: [None; PACKETS],
            front: 0,
            length: 0,
            write: 0,
            used: 0,
        }
    }
    /// 在入队可能发生前准备 backing storage；OOM 保留空队列，不公布半条 datagram。
    pub(super) fn prepare(&mut self, capacity: usize) -> Result<(), SocketError> {
        assert!(capacity != 0);
        if self.bytes.is_empty() {
            self.bytes
                .try_reserve_exact(capacity)
                .map_err(|_| SocketError::NoMemory)?;
            self.bytes.resize(capacity, 0);
        }
        Ok(())
    }
    pub(super) fn is_empty(&self) -> bool {
        self.length == 0
    }
    pub(super) fn can_send(&self) -> bool {
        self.length < PACKETS
    }
    /// 复制完整报文或返回 Again；元数据与载荷同一次发布，零字节报文也占一槽。
    pub(super) fn push(&mut self, metadata: M, input: &[u8]) -> Result<(), SocketError> {
        self.push_with(metadata, input.len(), |bytes| {
            bytes.copy_from_slice(input);
            Ok(())
        })
    }
    /// 在未发布的队尾直接编码；编码失败不改变记录、收费或队首，无临时报文分配。
    pub(super) fn push_with(
        &mut self,
        metadata: M,
        length: usize,
        emit: impl FnOnce(&mut [u8]) -> Result<(), SocketError>,
    ) -> Result<(), SocketError> {
        if self.bytes.is_empty() || !self.can_send() || length > self.bytes.len() {
            return Err(SocketError::Again);
        }
        if self.is_empty() {
            self.write = 0;
            self.used = 0;
        }
        let tail = self.bytes.len() - self.write;
        let padding = if length > tail { tail } else { 0 };
        let charge = padding + length;
        if charge > self.bytes.len() - self.used {
            return Err(SocketError::Again);
        }
        let start = if padding != 0 { 0 } else { self.write };
        emit(&mut self.bytes[start..start + length])?;
        let index = (self.front + self.length) % PACKETS;
        self.records[index] = Some(Record {
            metadata,
            start,
            length,
            charge,
        });
        self.length += 1;
        self.used += charge;
        self.write = (start + length) % self.bytes.len();
        Ok(())
    }
    pub(super) fn peek(&self) -> Option<(M, &[u8])> {
        let record = self.records[self.front]?;
        Some((
            record.metadata,
            &self.bytes[record.start..record.start + record.length],
        ))
    }
    /// 借用唯一队首的进度元数据与独占载荷；不改变 FIFO、容量收费或发送窗口。
    /// 两者分属固定记录与 payload storage，调用期间不能出队或重新分配。
    /// RAW 只在同一 owner 内补全自动 IP 标识和校验和；不改变已发布载荷长度。
    pub(super) fn peek_mut(&mut self) -> Option<(&mut M, &mut [u8])> {
        let record = self.records[self.front].as_mut()?;
        Some((
            &mut record.metadata,
            &mut self.bytes[record.start..record.start + record.length],
        ))
    }
    /// 故障冷路径删除匹配报文；其余载荷原位压紧，复用原 metadata/storage，不在收发热路径执行。
    pub(super) fn discard(&mut self, mut matches: impl FnMut(M) -> bool) -> usize {
        let mask = (0..self.length).fold(0u8, |mask, offset| {
            let record =
                self.records[(self.front + offset) % PACKETS].expect("live datagram record");
            mask | u8::from(matches(record.metadata)) << offset
        });
        let dropped = mask.count_ones() as usize;
        if dropped == 0 {
            return 0;
        }
        if dropped == self.length {
            while !self.is_empty() {
                self.pop();
            }
            return dropped;
        }
        // 1. 仅部分退休时归一化 ring：旋转后各载荷起点按原 FIFO 单调增加，wrap padding 仍是间隙。
        // 2. 目的游标不超过尚未复制的源起点，copy_within 不会破坏后续载荷；只复制保留项。
        // 3. 一次发布压紧后的 records、字节收费与写游标；零长度仍独占 metadata slot。
        let base = self.records[self.front]
            .expect("nonempty datagram queue")
            .start;
        self.bytes.rotate_left(base);
        let previous = core::mem::replace(&mut self.records, [None; PACKETS]);
        let mut count = 0;
        let mut used = 0;
        for offset in 0..self.length {
            if mask & (1 << offset) != 0 {
                continue;
            }
            let record = previous[(self.front + offset) % PACKETS].expect("live datagram record");
            let start = (record.start + self.bytes.len() - base) % self.bytes.len();
            self.bytes.copy_within(start..start + record.length, used);
            self.records[count] = Some(Record {
                start: used,
                charge: record.length,
                ..record
            });
            used += record.length;
            count += 1;
        }
        self.front = 0;
        self.length = count;
        self.used = used;
        self.write = used % self.bytes.len();
        dropped
    }
    pub(super) fn pop(&mut self) {
        let record = self.records[self.front]
            .take()
            .expect("datagram queue popped without a datagram");
        self.front = (self.front + 1) % PACKETS;
        self.length -= 1;
        self.used = self
            .used
            .checked_sub(record.charge)
            .expect("datagram queue padding released twice");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 普通以太网 MTU 无法把八槽 ring 推过 128 KiB wrap；直接复用生产 owner 检查故障退休的载荷保全。
    #[test]
    fn failure_compaction_preserves_wrapped_payload_and_zero_length_records() {
        let mut queue = Queue::new();
        queue.prepare(128 * 1024).unwrap();
        let first = alloc::vec![0xa1; 60_000];
        let second = alloc::vec![0xb2; 40_000];
        let third = alloc::vec![0xc3; 60_000];
        queue.push(1, &first).unwrap();
        queue.push(2, &second).unwrap();
        queue.pop();
        queue.push(3, &third).unwrap();
        assert_eq!(queue.discard(|id| id == 2), 1);
        assert_eq!(queue.peek(), Some((3, third.as_slice())));
        queue.push(4, &first).unwrap();
        queue.push(5, &[]).unwrap();
        assert_eq!(queue.discard(|id| id == 3), 1);
        assert_eq!(queue.peek(), Some((4, first.as_slice())));
        queue.pop();
        assert_eq!(queue.peek(), Some((5, &[][..])));
        assert_eq!(queue.discard(|_| true), 1);
        queue.push(6, &third).unwrap();
        assert_eq!(queue.peek(), Some((6, third.as_slice())));
        assert_eq!(
            queue.push_with(7, 12, |bytes| {
                bytes.fill(0xcc);
                Err(SocketError::Invalid)
            }),
            Err(SocketError::Invalid)
        );
        assert_eq!(queue.peek(), Some((6, third.as_slice())));
        queue.push(8, b"after-failure").unwrap();
        queue.pop();
        assert_eq!(queue.peek(), Some((8, &b"after-failure"[..])));
    }
}
