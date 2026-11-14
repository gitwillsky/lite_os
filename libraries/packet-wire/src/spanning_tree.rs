//! 802.3 LLC 承载的 IEEE 802.1D 配置、拓扑变化通知和快速生成树 BPDU。
use crate::PacketError;

/// 最近网桥组播地址，BPDU 在接收端口就地处理。
pub const DESTINATION: [u8; 6] = [0x01, 0x80, 0xc2, 0, 0, 0];
const LLC: [u8; 3] = [0x42, 0x42, 0x03];

/// 可排序的网桥标识，由优先级和 MAC 地址组成。
pub type BridgeId = [u8; 8];

/// 在链路上交换的优先级向量，字段顺序与比较顺序相同。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Priority {
    pub root: BridgeId,
    pub cost: u32,
    pub bridge: BridgeId,
    pub port: u16,
}

/// BPDU 中的时钟值，单位为 1/256 秒。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Times {
    pub age: u16,
    pub max_age: u16,
    pub hello: u16,
    pub forward_delay: u16,
}

impl Times {
    /// 802.1D 的根桥默认时钟。
    pub const DEFAULT: Self = Self {
        age: 0,
        max_age: 20 * 256,
        hello: 2 * 256,
        forward_delay: 15 * 256,
    };

    fn valid(self) -> bool {
        self.age < self.max_age
            && (6 * 256..=40 * 256).contains(&self.max_age)
            && (256..=10 * 256).contains(&self.hello)
            && (4 * 256..=30 * 256).contains(&self.forward_delay)
    }
}

/// STP 或 RSTP 配置消息。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Configuration {
    pub priority: Priority,
    pub times: Times,
    pub flags: u8,
    pub rapid: bool,
}

pub const TOPOLOGY_CHANGE: u8 = 1;
pub const PROPOSAL: u8 = 1 << 1;
pub const ROLE_MASK: u8 = 0b11 << 2;
pub const ALTERNATE: u8 = 0b01 << 2;
pub const ROOT: u8 = 0b10 << 2;
pub const DESIGNATED: u8 = 0b11 << 2;
pub const LEARNING: u8 = 1 << 4;
pub const FORWARDING: u8 = 1 << 5;
pub const AGREEMENT: u8 = 1 << 6;
pub const TOPOLOGY_ACK: u8 = 1 << 7;

/// 802.1D BPDU 类型。较新版本的扩展字段按标准忽略。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bpdu {
    Configuration(Configuration),
    TopologyChange,
}

impl Bpdu {
    /// 返回 802.3 Length 字段声明的载荷长度，不含以太网填充。
    pub const fn encoded_len(self) -> usize {
        match self {
            Self::Configuration(configuration) => 38 + configuration.rapid as usize,
            Self::TopologyChange => 7,
        }
    }

    /// 从 802.3 Length 界定的 LLC 载荷中解析 BPDU。
    /// # Errors
    /// 载荷截断、LLC 或时钟字段无效、版本或消息类型不受支持。
    pub fn parse(bytes: &[u8]) -> Result<Self, PacketError> {
        let header = bytes.get(..7).ok_or(PacketError::Truncated)?;
        if header[..3] != LLC || header[3..5] != [0, 0] {
            return Err(PacketError::Malformed);
        }
        let rapid = match (header[5], header[6]) {
            (_, 0x80) => return Ok(Self::TopologyChange),
            (_, 0) => false,
            (2.., 2) => true,
            _ => return Err(PacketError::Unsupported),
        };
        if bytes.len() < 38 + usize::from(rapid) {
            return Err(PacketError::Truncated);
        }
        let flags = if rapid {
            bytes[7]
        } else {
            bytes[7] & (TOPOLOGY_CHANGE | TOPOLOGY_ACK)
        };
        let word = |at| u16::from_be_bytes(bytes[at..at + 2].try_into().expect("validated BPDU"));
        let priority = Priority {
            root: bytes[8..16].try_into().expect("validated BPDU"),
            cost: u32::from_be_bytes(bytes[16..20].try_into().expect("validated BPDU")),
            bridge: bytes[20..28].try_into().expect("validated BPDU"),
            port: word(28),
        };
        let times = Times {
            age: word(30),
            max_age: word(32),
            hello: word(34),
            forward_delay: word(36),
        };
        if !times.valid() || priority.port & 0xfff == 0 {
            return Err(PacketError::Malformed);
        }
        Ok(Self::Configuration(Configuration {
            priority,
            times,
            flags,
            rapid,
        }))
    }

    /// 将 BPDU 的 LLC 载荷写入调用方提供的 Ethernet 窗口。
    /// # Errors
    /// 容量不足或配置字段无效时返回错误，保持输出窗口内容。
    pub fn emit(self, output: &mut [u8]) -> Result<(), PacketError> {
        let output = output
            .get_mut(..self.encoded_len())
            .ok_or(PacketError::Capacity)?;
        if let Self::Configuration(configuration) = self
            && (!configuration.times.valid()
                || configuration.priority.port & 0xfff == 0
                || !configuration.rapid
                    && configuration.flags & !(TOPOLOGY_CHANGE | TOPOLOGY_ACK) != 0)
        {
            return Err(PacketError::Malformed);
        }
        output[..3].copy_from_slice(&LLC);
        output[3..5].fill(0);
        match self {
            Self::TopologyChange => output[5..7].copy_from_slice(&[0, 0x80]),
            Self::Configuration(configuration) => {
                let version = if configuration.rapid { 2 } else { 0 };
                output[5..8].copy_from_slice(&[version, version, configuration.flags]);
                let priority = configuration.priority;
                output[8..16].copy_from_slice(&priority.root);
                output[16..20].copy_from_slice(&priority.cost.to_be_bytes());
                output[20..28].copy_from_slice(&priority.bridge);
                output[28..30].copy_from_slice(&priority.port.to_be_bytes());
                let times = configuration.times;
                for (field, value) in output[30..38].as_chunks_mut::<2>().0.iter_mut().zip([
                    times.age,
                    times.max_age,
                    times.hello,
                    times.forward_delay,
                ]) {
                    *field = value.to_be_bytes();
                }
                if configuration.rapid {
                    output[38] = 0;
                }
            }
        }
        Ok(())
    }
}
