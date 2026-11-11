//! 同一静态地址模型的借用视图；配置切片与已校验 wire 均不逐接口复制地址数组。
use crate::{Invalid, configuration::Ipv6Address};
use core::net::Ipv6Addr;

pub(super) const ROW: usize = 20;
#[derive(Clone, Copy, Debug)]
enum Data<'a> {
    Rows(&'a [Ipv6Address]),
    Wire(&'a [u8]),
}
/// 配置 owner 的静态地址意图；只借用不可变输入，不能逃逸执行事务的资源预备阶段。
#[derive(Clone, Copy, Debug)]
pub struct Ipv6Addresses<'a>(Data<'a>);
impl PartialEq for Ipv6Addresses<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.iter().eq(other.iter())
    }
}
impl Eq for Ipv6Addresses<'_> {}
impl<'a> Ipv6Addresses<'a> {
    /// 借用已按地址排序的配置集合，包括空集合；不分配或改变输入。
    /// # Errors
    /// 地址非法、重复或未排序。
    pub fn new(rows: &'a [Ipv6Address]) -> Result<Self, Invalid> {
        let result = Self(Data::Rows(rows));
        result.validate()?;
        Ok(result)
    }
    pub(super) fn parse(bytes: &'a [u8]) -> Result<Self, Invalid> {
        if !bytes.len().is_multiple_of(ROW) {
            return Err(Invalid);
        }
        for row in bytes.as_chunks::<ROW>().0 {
            if row[17] > 1 || row[18..] != [0; 2] {
                return Err(Invalid);
            }
        }
        let result = Self(Data::Wire(bytes));
        result.validate()?;
        Ok(result)
    }
    // 只能从已验证的 InterfaceConfigurationBatch 解码，不在逐项读取中重新扫描全部地址。
    pub(super) fn validated(bytes: &'a [u8]) -> Self {
        Self(Data::Wire(bytes))
    }
    /// 意图项数；空集合仍表达显式清除，不等同于缺席。
    pub fn len(self) -> usize {
        match self.0 {
            Data::Rows(rows) => rows.len(),
            Data::Wire(bytes) => bytes.len() / ROW,
        }
    }
    /// 是否显式清除全部手动地址。
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }
    /// 按同一顺序读取共享模型；wire 已在捕获时校验，不分配或重复解析 framing。
    pub fn iter(self) -> impl ExactSizeIterator<Item = Ipv6Address> + Clone + 'a {
        (0..self.len()).map(move |index| match self.0 {
            Data::Rows(rows) => rows[index],
            Data::Wire(bytes) => {
                let row = &bytes[index * ROW..(index + 1) * ROW];
                Ipv6Address {
                    address: Ipv6Addr::from(
                        <[u8; 16]>::try_from(&row[..16]).expect("address width"),
                    ),
                    prefix_length: row[16],
                    on_link: row[17] == 1,
                }
            }
        })
    }
    pub(super) fn validate(self) -> Result<(), Invalid> {
        let mut previous = None;
        for value in self.iter() {
            if !value.valid() || previous.is_some_and(|address| address >= value.address) {
                return Err(Invalid);
            }
            previous = Some(value.address);
        }
        Ok(())
    }
    pub(super) fn emit(self, bytes: &mut [u8]) {
        for (value, row) in self.iter().zip(bytes.as_chunks_mut::<ROW>().0.iter_mut()) {
            row[..16].copy_from_slice(&value.address.octets());
            row[16] = value.prefix_length;
            row[17] = u8::from(value.on_link);
            row[18..].fill(0);
        }
    }
}
