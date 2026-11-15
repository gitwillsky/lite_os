#!/usr/bin/env python3
"""将固定的 CC-CEDICT 简体词头与 wordfreq 词频拼成可二分查询的 UTF-8 表。"""

import argparse
import gzip
import hashlib
from pathlib import Path
import re

import msgpack


CEDICT_SHA256 = "c359597166ed8e2ff04aa142599254e9ba3f4ff58083f089ba01b67ea7ee408f"
WORDFREQ_SHA256 = "e079d7aaff1cfb92874850cb3ffbc7b611af939d9d671c1cbda932d968467cdd"
HEADER = """# CC BY-SA 4.0 https://creativecommons.org/licenses/by-sa/4.0/
# Sources and changes: see pinyin-NOTICE.md distributed with this table.
# CC-CEDICT contributors / MDBG; CEDICT, Paul Andrew Denisowski (1997–1998).
# Robyn Speer / wordfreq and attributed corpora, including freely available
# SUBTLEX-CH (Qing Cai and Marc Brysbaert), OpenSubtitles, Wikipedia and Google Books.
# pinyin<TAB>simplified Chinese<TAB>frequency cost (lower is more common)
"""


def verified(path: Path, sha256: str) -> bytes:
    raw = path.read_bytes()
    if hashlib.sha256(raw).hexdigest() != sha256:
        raise ValueError(f"unexpected SHA-256: {path}")
    return raw


def generate(cedict: bytes, packed: bytes) -> str:
    header, *buckets = msgpack.unpackb(gzip.decompress(packed), raw=False)
    if header != {"format": "cB", "version": 1}:
        raise ValueError("unsupported wordfreq metadata")
    frequency = {word: cost for cost, bucket in enumerate(buckets) for word in bucket}
    entries: set[tuple[str, str]] = set()
    for line in cedict.decode("utf-8").splitlines():
        match = re.match(r"^\S+ ([\u3400-\u9fff]{1,8}) \[([^]]+)\]", line)
        if not match:
            continue
        word, spell = match.groups()
        # 保留单字覆盖与常用词；冷僻多字词仍可用单字或短词逐段上屏。
        if len(word) > 1 and frequency.get(word, 900) > 620:
            continue
        spell = spell.lower().replace("u:", "v").replace("ü", "v")
        syllables = spell.split()
        if len(syllables) != len(word) or not all(
            re.fullmatch(r"[a-z]+[1-5]", syllable) for syllable in syllables
        ):
            continue
        syllables = [syllable[:-1] for syllable in syllables]
        entries.add(("".join(syllables), word))
        if len(syllables) > 1:
            entries.add(("'".join(syllables), word))
    # 子词积累多个频率代价，优先常用的整词；不在词频表的词仍可翻页选中。
    def order(entry: tuple[str, str]) -> tuple[str, int, str]:
        key, word = entry
        return key, frequency.get(word, 900), word

    return HEADER + "".join(
        f"{key}\t{word}\t{cost}\n"
        for key, cost, word in sorted(map(order, entries))
    )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("cedict", type=Path)
    parser.add_argument("wordfreq", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    output = generate(
        verified(args.cedict, CEDICT_SHA256), verified(args.wordfreq, WORDFREQ_SHA256)
    )
    args.output.write_text(output, encoding="utf-8", newline="\n")


if __name__ == "__main__":
    main()
