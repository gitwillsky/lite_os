//! 下游 /64 执行意图，只借用已校验的事务正文或配置切片。

use core::net::Ipv6Addr;

use crate::{
    Invalid,
    delegation::{DelegatedPrefix, MAX_PREFIXES},
};

pub(super) const ROW: usize = 32;

#[derive(Clone, Copy, Debug)]
enum Data<'a> {
    Rows(&'a [DelegatedPrefix]),
    Wire(&'a [u8]),
}

/// 供同一接口配置事务替换的下游前缀及单调纳秒截止时间。
#[derive(Clone, Copy, Debug)]
pub struct DelegatedPrefixes<'a>(Data<'a>);

impl PartialEq for DelegatedPrefixes<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.iter().eq(other.iter())
    }
}
impl Eq for DelegatedPrefixes<'_> {}

impl<'a> DelegatedPrefixes<'a> {
    /// 验证并借用排序后的下游 /64 前缀，空集合显式撤销原委派。
    /// # Errors
    /// 过多、重复、乱序、非规范前缀或非法有效期。
    pub fn new(prefixes: &'a [DelegatedPrefix]) -> Result<Self, Invalid> {
        let result = Self(Data::Rows(prefixes));
        result.validate()?;
        Ok(result)
    }

    pub(super) fn validated(bytes: &'a [u8]) -> Self {
        Self(Data::Wire(bytes))
    }

    /// 执行前缀的数量。
    pub fn len(self) -> usize {
        match self.0 {
            Data::Rows(rows) => rows.len(),
            Data::Wire(bytes) => bytes.len() / ROW,
        }
    }

    /// 是否显式撤销全部前缀。
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// 按前缀顺序遍历，借用正文且不分配。
    pub fn iter(self) -> impl ExactSizeIterator<Item = DelegatedPrefix> + Clone + 'a {
        (0..self.len()).map(move |index| match self.0 {
            Data::Rows(rows) => rows[index],
            Data::Wire(bytes) => {
                let row = &bytes[index * ROW..(index + 1) * ROW];
                DelegatedPrefix {
                    address: Ipv6Addr::from(
                        <[u8; 16]>::try_from(&row[..16]).expect("prefix width"),
                    ),
                    length: 64,
                    preferred_until: i64::from_le_bytes(
                        row[16..24].try_into().expect("time width"),
                    ),
                    valid_until: i64::from_le_bytes(row[24..32].try_into().expect("time width")),
                }
            }
        })
    }

    pub(super) fn validate(self) -> Result<(), Invalid> {
        if self.len() > MAX_PREFIXES {
            return Err(Invalid);
        }
        let mut previous = None;
        for row in self.iter() {
            if row.length != 64
                || u128::from(row.address) as u64 != 0
                || row.address.is_unspecified()
                || row.address.is_multicast()
                || row.address.is_unicast_link_local()
                || row.preferred_until < 0
                || row.valid_until <= 0
                || row.valid_until < row.preferred_until
                || previous.is_some_and(|address| address >= row.address)
            {
                return Err(Invalid);
            }
            previous = Some(row.address);
        }
        Ok(())
    }

    pub(super) fn emit(self, bytes: &mut [u8]) {
        for (prefix, row) in self.iter().zip(bytes.as_chunks_mut::<ROW>().0) {
            row[..16].copy_from_slice(&prefix.address.octets());
            row[16..24].copy_from_slice(&prefix.preferred_until.to_le_bytes());
            row[24..32].copy_from_slice(&prefix.valid_until.to_le_bytes());
        }
    }
}
