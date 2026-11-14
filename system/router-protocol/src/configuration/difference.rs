//! 对比规范化后的 JSON，借用未解码的子树并逐项序列化差异。

use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    vec::Vec,
};
use core::fmt::Write;
use serde::{
    Deserialize, Serialize, Serializer,
    ser::{Error, SerializeMap, SerializeSeq},
};
use serde_json::value::RawValue;
use zeroize::Zeroizing;

use super::Configuration;

pub(super) fn encode(
    before: Option<&Configuration>,
    next: &Configuration,
) -> Result<Vec<u8>, String> {
    if before == Some(next) {
        next.validate().map_err(|error| error.to_string())?;
        return Ok(b"[]".to_vec());
    }
    let before = before
        .map(Configuration::encode)
        .transpose()?
        .map(Zeroizing::new);
    let next = Zeroizing::new(next.encode()?);
    let decode = |error: serde_json::Error| error.to_string();
    let before = before
        .as_deref()
        .map(|document| serde_json::from_slice::<&RawValue>(document))
        .transpose()
        .map_err(decode)?;
    let next = serde_json::from_slice(&next).map_err(decode)?;
    serde_json::to_vec(&Difference { before, next }).map_err(decode)
}

struct Difference<'a> {
    before: Option<&'a RawValue>,
    next: &'a RawValue,
}

impl Serialize for Difference<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(None)?;
        compare(
            &mut String::new(),
            self.before,
            Some(self.next),
            &mut sequence,
        )?;
        sequence.end()
    }
}

#[derive(Serialize)]
struct Change<'a> {
    path: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    before: Option<ReviewValue<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    after: Option<ReviewValue<'a>>,
}

fn emit<S: SerializeSeq>(
    changes: &mut S,
    path: &str,
    before: Option<&RawValue>,
    after: Option<&RawValue>,
) -> Result<(), S::Error> {
    let masks = credential_paths(path);
    changes.serialize_element(&Change {
        path,
        before: before.map(|raw| ReviewValue { raw, masks }),
        after: after.map(|raw| ReviewValue { raw, masks }),
    })
}

const CREDENTIAL_PATHS: [&[&str]; 8] = [
    &["ospf", "links", "*", "v2_authentication", "keys", "*"],
    &["ospf", "links", "*", "v3_authentication", "keys", "*"],
    &[
        "routing_domains",
        "*",
        "ospf",
        "links",
        "*",
        "v2_authentication",
        "keys",
        "*",
    ],
    &[
        "routing_domains",
        "*",
        "ospf",
        "links",
        "*",
        "v3_authentication",
        "keys",
        "*",
    ],
    &["bgp", "neighbors", "*", "md5_password"],
    &["bgp", "neighbors", "*", "tcp_ao", "keys", "*", "secret"],
    &[
        "routing_domains",
        "*",
        "bgp",
        "neighbors",
        "*",
        "md5_password",
    ],
    &[
        "routing_domains",
        "*",
        "bgp",
        "neighbors",
        "*",
        "tcp_ao",
        "keys",
        "*",
        "secret",
    ],
];
type CredentialPaths = [Option<&'static [&'static str]>; CREDENTIAL_PATHS.len()];

fn credential_paths(path: &str) -> CredentialPaths {
    let path = path
        .strip_prefix('/')
        .expect("changes use non-root JSON pointers");
    // 固定段对应配置字段，* 对应配置名称、密钥编号或有序数组下标。
    path.split('/').fold(CREDENTIAL_PATHS.map(Some), descend)
}

fn descend(paths: CredentialPaths, segment: &str) -> CredentialPaths {
    paths.map(|path| match path {
        Some([name, rest @ ..]) if *name == "*" || *name == segment => Some(rest),
        _ => None,
    })
}

struct ReviewValue<'a> {
    raw: &'a RawValue,
    masks: CredentialPaths,
}

impl Serialize for ReviewValue<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // 未触及密钥的子树直接写出。新增或删除整个协议时也只展开包含密钥的路径。
        if self.masks.iter().all(Option::is_none) {
            return self.raw.serialize(serializer);
        }
        let raw = self.raw.get();
        match raw.as_bytes()[0] {
            b'"' if self.masks.contains(&Some(&[])) => serializer.serialize_str("[已隐藏]"),
            b'{' => {
                let fields: BTreeMap<String, &RawValue> =
                    serde_json::from_str(raw).map_err(S::Error::custom)?;
                let mut object = serializer.serialize_map(Some(fields.len()))?;
                for (name, raw) in fields {
                    let masks = descend(self.masks, &name);
                    object.serialize_entry(&name, &ReviewValue { raw, masks })?;
                }
                object.end()
            }
            b'[' => {
                let mut remaining = array_body(Some(raw));
                let mut array = serializer.serialize_seq(None)?;
                let masks = descend(self.masks, "*");
                while let Some(raw) = next_element::<S::Error>(&mut remaining)? {
                    array.serialize_element(&ReviewValue { raw, masks })?;
                }
                array.end()
            }
            _ => self.raw.serialize(serializer),
        }
    }
}

fn container<'a, T: Deserialize<'a> + Default, E: Error>(raw: Option<&'a str>) -> Result<T, E> {
    raw.map(serde_json::from_str)
        .transpose()
        .map(Option::unwrap_or_default)
        .map_err(E::custom)
}

fn array_body(raw: Option<&str>) -> &str {
    raw.map_or("", |array| &array[1..array.len() - 1])
}

fn next_element<'a, E: Error>(remaining: &mut &'a str) -> Result<Option<&'a RawValue>, E> {
    // `remaining` 来自已验证的 RawValue 数组。元素边界由 serde_json 处理，
    // 此处只越过数组元素之间的分隔符，避免为整张路由表收集引用。
    let mut items = serde_json::Deserializer::from_str(remaining).into_iter::<&RawValue>();
    let value = items.next().transpose().map_err(E::custom)?;
    let tail = remaining[items.byte_offset()..].trim_start();
    *remaining = match tail.strip_prefix(',') {
        Some(rest) => rest,
        None if tail.is_empty() => "",
        None => return Err(E::custom("missing array element separator")),
    };
    Ok(value)
}

fn changed_array<'a, 'b, E: Error>(
    mut left: &'a str,
    mut right: &'b str,
) -> Result<(usize, &'a str, &'b str), E> {
    let mut index = 0;
    loop {
        let (mut a, mut b) = (left, right);
        match (next_element::<E>(&mut a)?, next_element::<E>(&mut b)?) {
            (Some(old), Some(new)) if old.get() == new.get() => {
                (left, right) = (a, b);
                index += 1;
            }
            _ => break,
        }
    }

    // 相同末尾的元素不因前方插入或删除而成为修改。字节匹配只给出扫描起点，
    // 仍沿 JSON 元素边界对齐，避免把相同的字段尾部误认为完整元素。
    let common = left
        .bytes()
        .rev()
        .zip(right.bytes().rev())
        .take_while(|(a, b)| a == b)
        .count();
    if common == 0 {
        return Ok((index, left, right));
    }
    let (mut a, mut b) = (left, right);
    while a.len() > common {
        next_element::<E>(&mut a)?;
    }
    while b.len() > common {
        next_element::<E>(&mut b)?;
    }
    if a == b {
        Ok((
            index,
            &left[..left.len() - a.len()],
            &right[..right.len() - b.len()],
        ))
    } else {
        Ok((index, left, right))
    }
}

fn compare<S: SerializeSeq>(
    path: &mut String,
    before: Option<&RawValue>,
    after: Option<&RawValue>,
    changes: &mut S,
) -> Result<(), S::Error> {
    let left = before.map(RawValue::get);
    let right = after.map(RawValue::get);
    if left == right {
        return Ok(());
    }
    let start = path.len();
    let types = (
        left.map(|s| s.as_bytes()[0]),
        right.map(|s| s.as_bytes()[0]),
    );
    if types == (Some(b'{'), Some(b'{')) || path.is_empty() {
        let left: BTreeMap<String, &RawValue> = container::<_, S::Error>(left)?;
        let right: BTreeMap<String, &RawValue> = container::<_, S::Error>(right)?;
        for key in left
            .keys()
            .chain(right.keys().filter(|key| !left.contains_key(*key)))
        {
            path.push('/');
            for character in key.chars() {
                match character {
                    '~' => path.push_str("~0"),
                    '/' => path.push_str("~1"),
                    _ => path.push(character),
                }
            }
            compare(
                path,
                left.get(key).copied(),
                right.get(key).copied(),
                changes,
            )?;
            path.truncate(start);
        }
    } else if matches!(types, (Some(b'['), Some(b'[') | None) | (None, Some(b'['))) {
        let (first, mut left, mut right) =
            changed_array::<S::Error>(array_body(left), array_body(right))?;
        for index in first.. {
            let old = next_element::<S::Error>(&mut left)?;
            let new = next_element::<S::Error>(&mut right)?;
            if old.is_none() && new.is_none() {
                if index == 0 {
                    emit(changes, path, before, after)?;
                }
                break;
            }
            write!(path, "/{index}").expect("String formatting cannot fail");
            compare(path, old, new, changes)?;
            path.truncate(start);
        }
    } else {
        emit(changes, path, before, after)?;
    }
    Ok(())
}
