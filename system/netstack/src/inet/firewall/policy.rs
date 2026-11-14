//! 发布的策略、命中计数及供既有连接保留的规则快照。
use super::{
    AddressSets, Classification, Inspection, NatAction, PacketMatch, SocketError, capture_name,
    evaluate,
};
use alloc::{sync::Arc, vec::Vec};
use core::net::IpAddr;
use native_runtime::native_abi::{self, Status};
use router_protocol::firewall::{
    ConnectionState, Destination, ExistingConnections, Prerouting, PreroutingAction,
    TransportMatch, counters,
};

#[derive(Clone, Copy, Default)]
struct Hits {
    packets: u64,
    bytes: u64,
}

impl Hits {
    fn record(&mut self, bytes: usize) {
        self.packets = self.packets.saturating_add(1);
        self.bytes = self.bytes.saturating_add(bytes as u64);
    }
}

/// 规则和地址集是同一不可变版本。旧连接可共享原索引，不持有旧计数。
pub(in crate::inet) struct Program {
    classification: Arc<Classification>,
    address_sets: AddressSets,
    generation: u64,
}

impl Program {
    /// 供独立收包线程预计算无连接状态的规则裁决。命中超过随帧容量时返回 None。
    pub(in crate::inet) fn prepare(
        &self,
        ingress: u32,
        addresses: (IpAddr, IpAddr),
        transport: Option<TransportMatch>,
    ) -> Option<[u64; 5]> {
        let mut hits = [0; 3];
        let mut available = hits.iter_mut();
        let mut overflow = false;
        let decision = evaluate(
            &self.classification,
            &self.address_sets,
            |order| {
                if let Some(slot) = available.next()
                    && let Some(index) = u32::try_from(order)
                        .ok()
                        .and_then(|order| order.checked_add(1))
                {
                    *slot = index;
                } else {
                    overflow = true;
                }
            },
            (ingress, addresses, (None, transport)),
        );
        if overflow {
            return None;
        }
        let outcome = match decision {
            Some((mark, None)) => u64::from(mark) + 1,
            None => 0,
            Some((_, Some(_))) => return None,
        };
        // 首字为零，和连接提示的非零域 ID 区分。规则序号加一，零表示无命中。
        Some([
            0,
            self.generation,
            u64::from(ingress) << 32 | u64::from(hits[0]),
            u64::from(hits[1]) << 32 | u64::from(hits[2]),
            outcome,
        ])
    }
}

/// 空链也有明确放行语义，可以成为被保留的策略版本。
pub(in crate::inet) type PolicySnapshot = Option<Arc<Program>>;

#[derive(Default)]
pub(in crate::inet) struct Firewall {
    program: PolicySnapshot,
    existing_connections: ExistingConnections,
    // NET guard 独占生效计数。发布和撤销时，计数随所属规则链交换。
    counters: Vec<Hits>,
    revision: u64,
}

/// 从默认域捕获一页计数；编码与传输在释放网络锁后执行。
/// # Errors
/// 域不可用、规则链换版、游标越界或分配失败。
pub(crate) fn counter_page(
    scope: &crate::namespace::Namespace,
    cursor: counters::Cursor,
) -> Result<counters::Page, Status> {
    crate::inet::stack(scope)
        .map_err(|_| native_abi::ERR_BAD_STATE)?
        .lock()
        .map_err(|_| native_abi::ERR_NO_MEMORY)?
        .firewall
        .counter_page(cursor)
}

impl Firewall {
    /// 捕获可由新配置共享的不可变规则索引，不持有网络锁进入编译。
    pub(in crate::inet) fn classification(&self) -> Option<Arc<Classification>> {
        self.program
            .as_ref()
            .map(|program| program.classification.clone())
    }

    /// 编译候选地址集合及需要替换的规则，预备失败保留生效版本。
    /// # Errors
    /// 索引容量、成员或存储分配失败。
    pub(in crate::inet) fn compile(
        chain: Prerouting<'_>,
        inherited: Option<Arc<Classification>>,
    ) -> Result<Self, SocketError> {
        let existing_connections = chain.existing_connections();
        if chain.rules().len() == 0 {
            return Ok(Self {
                existing_connections,
                ..Self::default()
            });
        }
        let classification =
            if let Some(inherited) = inherited.filter(|current| current.matches(chain)) {
                inherited
            } else {
                Arc::try_new(Classification::compile(chain)?).map_err(|_| SocketError::NoMemory)?
            };
        let address_sets = AddressSets::compile(chain, &classification.set_inspection)?;
        let mut counters = Vec::new();
        counters
            .try_reserve_exact(classification.rules.len())
            .map_err(|_| SocketError::NoMemory)?;
        counters.resize(classification.rules.len(), Hits::default());
        let mut revision = [0; 8];
        native_runtime::system::cprng_draw(&mut revision);
        let program = Arc::try_new(Program {
            classification,
            address_sets,
            generation: crate::identity::next(),
        })
        .map_err(|_| SocketError::NoMemory)?;
        Ok(Self {
            program: Some(program),
            existing_connections,
            counters,
            revision: u64::from_le_bytes(revision).max(2),
        })
    }

    fn same_rules(&self, other: &Self) -> bool {
        match (&self.program, &other.program) {
            (None, None) => true,
            (Some(left), Some(right)) => {
                Arc::ptr_eq(&left.classification, &right.classification)
                    || left.classification.same_rules(&right.classification)
            }
            _ => false,
        }
    }

    /// 判断候选规则及实际集合成员是否与生效投影相同。
    pub(in crate::inet) fn same_program(&self, other: &Self) -> bool {
        self.existing_connections == other.existing_connections
            && self.same_rules(other)
            && self
                .program
                .as_ref()
                .zip(other.program.as_ref())
                .is_none_or(|(left, right)| left.address_sets.same_members(&right.address_sets))
    }

    /// 原子交换配置及前像。只改集合时，已有规则计数随同一规则定义继续累计。
    pub(in crate::inet) fn exchange(&mut self, other: &mut Self) {
        if self.same_rules(other) {
            core::mem::swap(&mut self.counters, &mut other.counters);
        }
        core::mem::swap(self, other);
    }

    /// 复制同一已发布链的一页计数。查询不会改变计数。
    /// # Errors
    /// 规则链已换版、游标越界或分页缓冲分配失败。
    pub(in crate::inet) fn counter_page(
        &self,
        cursor: counters::Cursor,
    ) -> Result<counters::Page, Status> {
        let revision = self.revision.max(1);
        if cursor.revision != 0 && cursor.revision != revision {
            return Err(native_abi::ERR_BAD_STATE);
        }
        let start = cursor.offset as usize;
        let end = start
            .saturating_add(counters::LIMIT)
            .min(self.counters.len());
        let records = self
            .counters
            .get(start..end)
            .ok_or(native_abi::ERR_OUT_OF_RANGE)?;
        let mut rows = Vec::new();
        rows.try_reserve_exact(records.len())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        if let Some(program) = &self.program {
            let classification = &program.classification;
            for (index, (&hits, &rule)) in records
                .iter()
                .zip(&classification.rules[start..end])
                .enumerate()
            {
                let order = start + index;
                let chain_index = classification
                    .chains
                    .partition_point(|chain| chain.rules.end <= order);
                let chain = &classification.chains[chain_index];
                let destination_set = match rule.destination {
                    Destination::Any | Destination::Prefix(_) => None,
                    Destination::Set(index) => {
                        Some(capture_name(&classification.set_names[index as usize])?)
                    }
                };
                let jump_chain = if let PreroutingAction::Jump { chain } = rule.action {
                    classification.chains[chain as usize]
                        .name
                        .as_deref()
                        .map(capture_name)
                        .transpose()?
                } else {
                    None
                };
                rows.push(counters::Counter {
                    rule,
                    chain: chain.name.as_deref().map(capture_name).transpose()?,
                    position: (order - chain.rules.start + 1) as u32,
                    destination_set,
                    jump_chain,
                    packets: hits.packets,
                    bytes: hits.bytes,
                });
            }
        }
        Ok(counters::Page {
            revision,
            next: (end < self.counters.len()).then_some(end as u32),
            rows,
        })
    }

    /// 保留模式同时跟踪尚无显式状态规则的转发连接。
    pub(in crate::inet) fn preserves_connections(&self) -> bool {
        self.existing_connections == ExistingConnections::Preserve
    }

    /// 捕获当前规则与实际集合成员，作为同一代连接的不可变策略。
    pub(in crate::inet) fn snapshot(&self) -> PolicySnapshot {
        self.program.clone()
    }

    /// 当前规则和集合的进程内唯一身份，空链使用零。
    pub(in crate::inet) fn generation(&self) -> u64 {
        self.program.as_ref().map_or(0, |program| program.generation)
    }

    /// 返回该地址族可达规则所需的传输层事实。
    pub(in crate::inet) fn inspection(&self, ipv6: bool) -> Inspection {
        let family = usize::from(ipv6);
        let mut inspection = self
            .program
            .as_ref()
            .map_or_else(Inspection::default, |program| {
                program.classification.inspection[family]
                    .merge(program.address_sets.inspection[family])
            });
        if self.preserves_connections() {
            // 旧规则仍可能匹配传输端口。这里与跟踪共用同一次 transport parse。
            inspection.connections = true;
            inspection.transport = true;
        }
        inspection
    }

    /// 采用同一入接口、当前规则版本的无状态裁决并结算计数。
    /// 返回 None 表示需现场执行，Some(None) 表示命中丢弃规则。
    pub(in crate::inet) fn apply_prepared(
        &mut self,
        ingress: u32,
        bytes: usize,
        &[tag, generation, first, rest, outcome]: &[u64; 5],
    ) -> Option<Option<u32>> {
        let program = self.program.as_ref()?;
        if tag != 0 || generation != program.generation || first >> 32 != u64::from(ingress) {
            return None;
        }
        for index in [first as u32, (rest >> 32) as u32, rest as u32] {
            if let Some(order) = index.checked_sub(1) {
                self.counters[order as usize].record(bytes);
            }
        }
        Some(outcome.checked_sub(1).map(|mark| mark as u32))
    }

    /// 在域内执行当前或连接保留的策略。
    pub(in crate::inet) fn evaluate(
        &mut self,
        (packet, bytes): (PacketMatch<IpAddr>, usize),
        retained: Option<&PolicySnapshot>,
    ) -> Option<(u32, Option<NatAction>)> {
        if let Some(retained) = retained {
            return retained.as_ref().map_or(Some((0, None)), |program| {
                evaluate(&program.classification, &program.address_sets, |_| (), packet)
            });
        }
        let Some(program) = &self.program else {
            return Some((0, None));
        };
        evaluate(
            &program.classification,
            &program.address_sets,
            |order| self.counters[order].record(bytes),
            packet,
        )
    }

    /// 只判断本机目的是否选中转换。普通本机报文不计转发命中。
    pub(in crate::inet) fn selects_translation(
        &self,
        ingress: u32,
        addresses: (IpAddr, IpAddr),
        classification: (Option<ConnectionState>, Option<TransportMatch>),
    ) -> bool {
        self.program.as_ref().is_some_and(|program| {
            evaluate(
                &program.classification,
                &program.address_sets,
                |_| (),
                (ingress, addresses, classification),
            )
            .is_some_and(|(_, target)| target.is_some_and(|target| target.destination.is_some()))
        })
    }
}
