//! 只读全拼字词索引。单次查询只读排序表，按代价找轻量分词的整串候选。

pub struct Dictionary<'a> {
    rows: Vec<Word<'a>>,
}

struct Word<'a> {
    key: &'a str,
    text: &'a str,
    cost: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Candidate {
    pub text: String,
    pub consumed: usize,
}

const LIMIT: usize = 160;

impl<'a> Dictionary<'a> {
    pub fn new(table: &'a str) -> Self {
        Self {
            rows: table
                .lines()
                .filter_map(|line| {
                    let (key, entry) = line.split_once('\t')?;
                    let (text, cost) = entry.split_once('\t')?;
                    Some(Word {
                        key,
                        text,
                        cost: cost.parse().ok()?,
                    })
                })
                .collect(),
        }
    }

    fn lookup(&self, key: &str) -> &[Word<'a>] {
        let start = self.rows.partition_point(|row| row.key < key);
        let count = self.rows[start..].partition_point(|row| row.key == key);
        &self.rows[start..start + count]
    }

    pub fn candidates(&self, input: &str) -> Vec<Candidate> {
        let mut result = Vec::new();
        let mut add = |text: &str, consumed: usize| {
            if result.len() < LIMIT
                && !result
                    .iter()
                    .any(|item: &Candidate| item.text == text && item.consumed == consumed)
            {
                result.push(Candidate {
                    text: text.into(),
                    consumed,
                });
            }
        };
        let complete = self.lookup(input);
        for word in complete.iter().take(LIMIT) {
            add(word.text, input.len());
        }
        if complete.is_empty()
            && let Some(decoded) = self.decode(input)
        {
            add(&decoded, input.len());
        }
        // 连续全拼也可以逐词选。先列较长前缀，选择后保留未消费的拼音。
        for end in (1..input.len()).rev() {
            if input.as_bytes()[end - 1] == b'\'' {
                continue;
            }
            let consumed = end + usize::from(input.as_bytes()[end] == b'\'');
            for word in self.lookup(&input[..end]).iter().take(12) {
                add(word.text, consumed);
            }
        }
        result
    }

    fn decode(&self, input: &str) -> Option<String> {
        let mut best = vec![None; input.len() + 1];
        best[input.len()] = Some((0u32, "", input.len()));
        for start in (0..input.len()).rev() {
            if input.as_bytes()[start] == b'\'' {
                best[start] = best[start + 1];
                continue;
            }
            for end in start + 1..=input.len() {
                let Some((tail, _, _)) = best[end] else {
                    continue;
                };
                let Some(word) = self.lookup(&input[start..end]).first() else {
                    continue;
                };
                let cost = tail + word.cost + 90;
                if best[start].is_none_or(|(before, _, _)| cost < before) {
                    best[start] = Some((cost, word.text, end));
                }
            }
        }
        let mut at = 0;
        let mut text = String::new();
        while at < input.len() {
            let (_, word, end) = best[at]?;
            text.push_str(word);
            at = end;
        }
        (!text.is_empty()).then_some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_words_and_composed_sentences_appear_before_prefixes() {
        let dict =
            Dictionary::new("hao\t好\t100\nni\t你\t100\nnihao\t你好\t150\nshijie\t世界\t160\n");
        let items = dict.candidates("nihaoshijie");
        assert_eq!(
            items[0],
            Candidate {
                text: "你好世界".into(),
                consumed: 11
            }
        );
        assert_eq!(
            items[1],
            Candidate {
                text: "你好".into(),
                consumed: 5
            }
        );
        assert_eq!(dict.candidates("nihao")[0].text, "你好");
        let shipped = Dictionary::new(include_str!("../../../../assets/pinyin/pinyin.tsv"));
        assert_eq!(shipped.candidates("nihao")[0].text, "你好");
        assert_eq!(shipped.candidates("zhongwen")[0].text, "中文");
    }
}
