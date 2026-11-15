pinyin.tsv is an adaptation distributed under [CC BY-SA 4.0](https://creativecommons.org/licenses/by-sa/4.0/).

- Pronunciations and simplified headwords come from [CC-CEDICT](https://cc-cedict.org/wiki/), created by its contributors and published by MDBG (including CEDICT by Paul Andrew Denisowski, 1997–1998). [Input snapshot](https://github.com/leonsilicon/cc-cedict/blob/f0ff1c9513db0affe634835fd31eb16d7112dc48/data/cedict_1_0_ts_utf-8_mdbg.txt).
- Ranking comes from Robyn Speer's [wordfreq](https://github.com/rspeer/wordfreq/tree/42233e6c36ce792031bcccfa17cdd0cec9af5fa7) Chinese frequency data, including Google Books Ngrams, Wikipedia, Tatoeba, ParaCrawl, OpenSubtitles and the freely available [SUBTLEX-CH](https://doi.org/10.1371/journal.pone.0010729) data by Qing Cai and Marc Brysbaert. See [wordfreq's credits and terms](https://github.com/rspeer/wordfreq/tree/42233e6c36ce792031bcccfa17cdd0cec9af5fa7#license) for the full attributions.

LiteOS extracts simplified Han headwords and toneless pinyin, drops definitions, and sorts by spelling and word frequency. The generator and SHA-256 of both inputs are in scripts/generate_pinyin_dictionary.py.
