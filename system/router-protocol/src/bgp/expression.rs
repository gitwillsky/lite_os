//! 经过编译验证的 AS 路径表达式值；只有源文本参与持久化，程序没有连接或可变匹配状态。
use crate::Invalid;
use alloc::{string::String, sync::Arc, vec::Vec};
use core::fmt::{self, Write};
use regex_automata::{
    Input,
    dfa::{Automaton, StartKind, dense},
    nfa::thompson,
    util::{primitives::StateID, syntax},
};
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

// 配置是控制面输入，完整 DFA 构造可能指数膨胀；显式拒绝超预算程序，不能在匹配时回溯或退化。
const PROGRAM_BYTES: usize = 1024 * 1024;
const COMPILER_BYTES: usize = 4 * PROGRAM_BYTES;
const BOUNDARY: &str = "(^|[,{}() ]|$)";

struct Expression {
    text: String,
    dfa: dense::DFA<Vec<u32>>,
}

/// 校验后的正则值；克隆共享同一不可变文本/程序，源字段私有，不能出现文本与程序不同步。
/// OWNER: Arc 仅保活冻结配置值，使并发配置审阅及本地克隆不会复制自动机；没有匹配缓存、锁或全局索引。
#[derive(Clone)]
pub struct AsPathExpression(Arc<Expression>);
impl AsPathExpression {
    /// 编译 ASCII 正则；未加锚点时搜索任意子串，`_` 按 FRR 约定展开为 AS 边界。
    /// # Parameters
    /// `text` 是持久化的唯一表达式，支持有限自动机正则，不接受回溯引用或环视。
    /// # Returns
    /// 只有完整编译成功才返回的不可变值；后续匹配不编译或分配路径字符串。
    /// # Errors
    /// 非法语法、非 ASCII、过深嵌套、程序/编译工作区超预算或模型存储预留失败。
    pub fn compile(text: String) -> Result<Self, Invalid> {
        if !text.is_ascii() {
            return Err(Invalid);
        }
        // 1. 与 FRR 的边界替换保持一致；转义/字符类仍由唯一正则解析器裁决，不自造语法解析器。
        let boundaries = text.bytes().filter(|byte| *byte == b'_').count();
        let extra = boundaries.checked_mul(BOUNDARY.len() - 1).ok_or(Invalid)?;
        let expanded_length = text.len().checked_add(extra).ok_or(Invalid)?;
        if expanded_length > PROGRAM_BYTES {
            return Err(Invalid);
        }
        let mut expanded = String::new();
        expanded
            .try_reserve_exact(expanded_length)
            .map_err(|_| Invalid)?;
        for byte in text.bytes() {
            if byte == b'_' {
                expanded.push_str(BOUNDARY);
            } else {
                expanded.push(char::from(byte));
            }
        }
        // 2. 只建单方向 ASCII DFA；资源错误在配置边界终止，已安装策略和路由保持原值。
        let dfa = dense::Builder::new()
            .configure(
                dense::Config::new()
                    .start_kind(StartKind::Unanchored)
                    .dfa_size_limit(Some(PROGRAM_BYTES))
                    .determinize_size_limit(Some(COMPILER_BYTES)),
            )
            .syntax(syntax::Config::new().unicode(false).utf8(false))
            .thompson(thompson::Config::new().nfa_size_limit(Some(PROGRAM_BYTES)))
            .build(&expanded)
            .map_err(|_| Invalid)?;
        Ok(Self(
            Arc::try_new(Expression { text, dfa }).map_err(|_| Invalid)?,
        ))
    }
    /// 原始配置文本；导出/比较不暴露或重复编码自动机。
    pub fn as_str(&self) -> &str {
        &self.0.text
    }
    /// 流式匹配由路径 owner 提供的规范文本；保留集合和联盟段边界。
    /// # Returns
    /// 表达式命中返回 true；格式化失败且尚无完整匹配时拒绝，只有完整输入才执行结束锚点。
    pub fn is_match(&self, path: impl fmt::Display) -> bool {
        let dfa = &self.0.dfa;
        let mut search = Search {
            dfa,
            state: dfa
                .start_state_forward(&Input::new(b""))
                .expect("unanchored ASCII DFA supports start of input"),
            matched: false,
        };
        // 3. 提前命中/死状态时让标准 fmt::Write 短路；不把半条路径误作输入结束。
        let finished = write!(&mut search, "{path}").is_ok();
        search.matched || finished && dfa.is_match_state(dfa.next_eoi_state(search.state))
    }
}
struct Search<'a> {
    dfa: &'a dense::DFA<Vec<u32>>,
    state: StateID,
    matched: bool,
}
impl fmt::Write for Search<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for byte in text.bytes() {
            self.state = self.dfa.next_state(self.state, byte);
            if self.dfa.is_match_state(self.state) {
                self.matched = true;
                return Err(fmt::Error);
            }
            if self.dfa.is_dead_state(self.state) {
                return Err(fmt::Error);
            }
        }
        Ok(())
    }
}
impl fmt::Debug for AsPathExpression {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_str().fmt(formatter)
    }
}
impl PartialEq for AsPathExpression {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0) || self.as_str() == other.as_str()
    }
}
impl Eq for AsPathExpression {}
impl Serialize for AsPathExpression {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}
impl<'de> Deserialize<'de> for AsPathExpression {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::compile(String::deserialize(deserializer)?)
            .map_err(|_| serde::de::Error::custom("invalid or oversized AS path expression"))
    }
}
impl JsonSchema for AsPathExpression {
    fn schema_name() -> alloc::borrow::Cow<'static, str> {
        "AsPathExpression".into()
    }
    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        String::json_schema(generator)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Chunks<'a>(&'a [&'a str]);
    impl fmt::Display for Chunks<'_> {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            for chunk in self.0 {
                formatter.write_str(chunk)?;
            }
            Ok(())
        }
    }

    #[test]
    fn boundary_and_end_of_input_survive_chunks_and_configuration_round_trip() {
        // 上游正则用例不覆盖本地边界展开、fmt 分块、结束锚点及只序列化源值的契约。
        for (pattern, path, expected) in [
            ("^$", "", true),
            ("^$", "65001", false),
            ("_65001_", "165001 650010", false),
            ("_65001_", "70000 {65001,65002}", true),
            ("_65001_", "(65001) 70000", true),
            ("^65001_", "65001 70000", true),
            ("_65001$", "70000 65001", true),
            ("_65001$", "70000 65001 65002", false),
            ("^(65001 )+70000$", "65001 65001 70000", true),
            (r"^[0-9]+ \{65001,65002\}$", "70000 {65001,65002}", true),
            ("65001|70000", "70000 80000", true),
            ("^70000$", "70000 80000", false),
        ] {
            let expression = AsPathExpression::compile(pattern.into()).unwrap();
            assert_eq!(expression.is_match(path), expected, "{pattern}: {path}");
            let chunks = path
                .as_bytes()
                .as_chunks::<1>()
                .0
                .iter()
                .map(|chunk| core::str::from_utf8(chunk).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(expression.is_match(Chunks(&chunks)), expected);
            let duplicate = expression.clone();
            assert!(Arc::ptr_eq(&expression.0, &duplicate.0));
            let bytes = serde_json::to_vec(&duplicate).unwrap();
            assert_eq!(bytes, serde_json::to_vec(pattern).unwrap());
            let restored: AsPathExpression = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(restored, expression);
            assert_eq!(restored.is_match(Chunks(&chunks)), expected);
        }
        for invalid in [
            "(",
            "[",
            r"(65001)\1",
            "(?=65001)",
            "路由",
            "a{1000000000}",
            "[01]*1[01]{20}$",
        ] {
            assert!(
                AsPathExpression::compile(invalid.into()).is_err(),
                "{invalid}"
            );
        }
    }
}
