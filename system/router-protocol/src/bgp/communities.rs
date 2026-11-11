//! 团体编辑的唯一配置语义；所有输入集合只在解码时规范化，运行时不逐前缀排序。
use alloc::vec::Vec;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

/// 顺序为 replace（缺省使用原集合）、remove、add；同值同时删除和添加时添加生效。
/// 三个列表都是集合，不改变外层规则的 first-match 顺序；空 replace 显式清空。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, bound(deserialize = "T: Ord + Deserialize<'de>"))]
pub struct CommunityEdit<T> {
    #[serde(default, deserialize_with = "optional")]
    pub replace: Option<Vec<T>>,
    #[serde(default, deserialize_with = "ordered")]
    pub remove: Vec<T>,
    #[serde(default, deserialize_with = "ordered")]
    pub add: Vec<T>,
}
impl<T: Ord> CommunityEdit<T> {
    /// 检查手工构造的模型是否符合解码器保证的排序/去重不变量，不修改配置或分配。
    pub fn is_canonical(&self) -> bool {
        let ordered = |values: &[T]| values.windows(2).all(|pair| pair[0] < pair[1]);
        self.replace.as_ref().is_none_or(|values| ordered(values))
            && ordered(&self.remove)
            && ordered(&self.add)
    }
}
fn ordered<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Ord + Deserialize<'de>,
{
    let mut values = Vec::<T>::deserialize(deserializer)?;
    values.sort_unstable();
    values.dedup();
    Ok(values)
}
fn optional<'de, D, T>(deserializer: D) -> Result<Option<Vec<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Ord + Deserialize<'de>,
{
    let mut values = Option::<Vec<T>>::deserialize(deserializer)?;
    if let Some(values) = &mut values {
        values.sort_unstable();
        values.dedup();
    }
    Ok(values)
}
