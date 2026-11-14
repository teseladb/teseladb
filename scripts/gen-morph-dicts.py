#!/usr/bin/env python3
"""Writes the dictionaries of the kuromoji and nori search analyzers (011 T009, D-109):
crates/teseladb-search/data/kuromoji.bin from IPADIC 2.7.0-20070801 and data/nori.bin from
mecab-ko-dic 2.1.1-20180720, both fetched with their SHA-256 by scripts/fetch-pinned.sh
(scripts/kuromoji.sha256, scripts/nori.sha256).

   python3 -I scripts/gen-morph-dicts.py

The output is gzip (mtime 0) of this layout, little-endian:
  magic b"TSLMOR01"
  strings:  u32 n, u32 offsets[n + 1], UTF-8 bytes (string 0 is empty)
  surfaces: u32 n, u32 unit offsets[n + 1], u16 UTF-16 units; sorted by their UTF-16 units
  u32 first entry of each surface [n + 1]
  entries:  u32 n, then n records (u16 left id, u16 right id, i16 cost, 2 zero bytes, u32 tag, u32 extra)
  matrix:   u16 right ids, u16 left ids, i16 cost[right * left ids + left]
  classes:  u8 class of each BMP code unit [65536], u8 n, u8 flags[n] (1 invoke, 2 group)
  unknown:  u8 n, then per class u32 count and its records
`tag` is a string: the part-of-speech (IPADIC: its four levels joined by `-` without `*`;
mecab-ko-dic: the left tag). `extra` is a string or 0: IPADIC, the base form when it is not
`*`; mecab-ko-dic, the type letter (M, C, I or P), U+0001, the reading when it differs from the
surface, then for each morpheme of the expression U+0001, its tag, U+0002 and its surface.

Entries follow the order the reference analyzers' builders give them (CSV files sorted by name, lines
sorted by surface with a stable sort); unknown entries are sorted by character class, after the
NGRAM entry. IPADIC is read as EUC-JP. IPADIC gains the era 令和, which the reference search server
knows as one word: the entry of 平成 as 名詞-固有名詞-一般 (left and right id 1288, cost 5904),
deduced from the tokens it gives in 18 contexts.
"""
import glob
import gzip
import io
import os
import re
import struct
import sys
import tarfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
IPADIC = ROOT + "/.tools/kuromoji/mecab-ipadic/2.7.0-20070801/mecab-ipadic-2.7.0-20070801.tar.gz"
KODIC = ROOT + "/.tools/nori/mecab-ko-dic-2.1.1-20180720.tar.gz"
JA_CLASSES = ["NGRAM", "DEFAULT", "SPACE", "SYMBOL", "NUMERIC", "ALPHA", "CYRILLIC", "GREEK",
              "HIRAGANA", "KATAKANA", "KANJI", "KANJINUMERIC"]
KO_CLASSES = ["NGRAM", "DEFAULT", "SPACE", "SYMBOL", "NUMERIC", "ALPHA", "CYRILLIC", "GREEK",
              "HIRAGANA", "KATAKANA", "KANJI", "HANGUL", "HANJA", "HANJANUMERIC"]
REIWA = "令和,1288,1288,5904,名詞,固有名詞,一般,*,*,*,令和,レイワ,レイワ"


def csv(line):
    """The fields of a CSV line, with quoted fields as the reference analyzers' CSV reader reads them."""
    out, cur, quoted, i = [], [], False, 0
    while i < len(line):
        c = line[i]
        if quoted:
            if c == '"':
                if i + 1 < len(line) and line[i + 1] == '"':
                    cur.append('"')
                    i += 1
                else:
                    quoted = False
            else:
                cur.append(c)
        elif c == '"':
            quoted = True
        elif c == ",":
            out.append("".join(cur))
            cur = []
        else:
            cur.append(c)
        i += 1
    out.append("".join(cur))
    return out


def units(s):
    return s.encode("utf-16-le")


def java_order(s):
    """The key that sorts strings as Java compares them: by UTF-16 code units."""
    b = units(s)
    return [b[i] | b[i + 1] << 8 for i in range(0, len(b), 2)]


def read_tar(path, encoding):
    files = {}
    with tarfile.open(path, "r:gz") as tar:
        for m in tar.getmembers():
            # Only the dictionary's own directory: mecab-ko-dic also ships user-dic/ samples.
            if m.isfile() and m.name.count("/") == 1 and m.name.endswith((".csv", ".def")):
                files[os.path.basename(m.name)] = tar.extractfile(m).read().decode(encoding)
    return files


def char_def(text, classes, override_nakaguro):
    cat = [classes.index("DEFAULT")] * 0x10000
    flags = [0] * len(classes)
    for line in text.split("\n"):
        line = re.sub(r"^\s", "", line)
        line = re.sub(r"\s*#.*", "", line)
        line = re.sub(r"\s+", " ", line)
        if not line:
            continue
        if line.startswith("0x"):
            cps, names = line.split(" ", 1)
            name = names.split(" ")[0]
            lo, hi = cps.split("..") if ".." in cps else (cps, cps)
            for cp in range(int(lo, 16), int(hi, 16) + 1):
                if cp < 0x10000:
                    cat[cp] = classes.index("SYMBOL" if override_nakaguro and cp == 0x30FB else name)
        else:
            v = line.split(" ")
            k = classes.index(v[0])
            flags[k] = (1 if v[1] == "1" else 0) | (2 if v[2] == "1" else 0)
    return cat, flags


def matrix(text):
    lines = text.split("\n")
    right, left = map(int, lines[0].split())
    costs = [0] * (right * left)
    for line in lines[1:]:
        if line:
            r, l, c = map(int, line.split())
            costs[r * left + l] = c
    return right, left, costs


class Writer:
    def __init__(self):
        self.strings = [""]
        self.index = {"": 0}

    def string(self, s):
        if s not in self.index:
            self.index[s] = len(self.strings)
            self.strings.append(s)
        return self.index[s]

    def blob(self, surfaces, entries, mat, cat, flags, unknown):
        out = io.BytesIO()
        w = out.write
        w(b"TSLMOR01")
        encoded = [s.encode("utf-8") for s in self.strings]
        w(struct.pack("<I", len(encoded)))
        off = 0
        for e in encoded:
            w(struct.pack("<I", off))
            off += len(e)
        w(struct.pack("<I", off))
        for e in encoded:
            w(e)
        w(struct.pack("<I", len(surfaces)))
        off = 0
        for s in surfaces:
            w(struct.pack("<I", off))
            off += len(units(s)) // 2
        w(struct.pack("<I", off))
        for s in surfaces:
            w(units(s))
        first = 0
        for s in surfaces:
            w(struct.pack("<I", first))
            first += len(entries[s])
        w(struct.pack("<I", first))
        w(struct.pack("<I", first))
        for s in surfaces:
            for rec in entries[s]:
                w(struct.pack("<HHhxxII", *rec))
        right, left, costs = mat
        w(struct.pack("<HH", right, left))
        w(struct.pack("<%dh" % len(costs), *costs))
        w(bytes(cat))
        w(struct.pack("<B", len(flags)))
        w(bytes(flags))
        w(struct.pack("<B", len(unknown)))
        for recs in unknown:
            w(struct.pack("<I", len(recs)))
            for rec in recs:
                w(struct.pack("<HHhxxII", *rec))
        return gzip.compress(out.getvalue(), 9, mtime=0)


def sort_lines(lines):
    lines.sort(key=lambda e: java_order(e[0]))
    return lines


def kuromoji():
    files = read_tar(IPADIC, "euc_jp")
    lines = []
    for name in sorted(n for n in files if n.endswith(".csv")):
        lines += [csv(l) for l in files[name].split("\n") if l]
    lines.append(csv(REIWA))
    sort_lines(lines)
    wr = Writer()

    def record(e):
        tag = "-".join(p for p in e[4:8] if p != "*")
        base = 0 if e[10] == "*" else wr.string(e[10])
        return (int(e[1]), int(e[2]), int(e[3]), wr.string(tag), base)

    entries = {}
    for e in lines:
        entries.setdefault(e[0], []).append(record(e))
    cat, flags = char_def(files["char.def"], JA_CLASSES, True)
    unk = [csv(l + ",*,*") for l in files["unk.def"].split("\n") if l]
    unk.sort(key=lambda e: JA_CLASSES.index(e[0]))
    unknown = [[] for _ in JA_CLASSES]
    unknown[0].append(record(csv("NGRAM,5,5,-32768,記号,一般,*,*,*,*,*,*,*")))
    for e in unk:
        unknown[JA_CLASSES.index(e[0])].append(record(e))
    surfaces = sorted(entries, key=java_order)
    return wr.blob(surfaces, entries, matrix(files["matrix.def"]), cat, flags, unknown)


def ko_tag(name):
    u = name.upper()
    return "J" if u.startswith("J") else "E" if u.startswith("E") else u


def nori():
    files = read_tar(KODIC, "utf-8")
    lines = []
    for name in sorted(n for n in files if n.endswith(".csv")):
        lines += [csv(l) for l in files[name].split("\n") if l]
    sort_lines(lines)
    wr = Writer()

    def record(e):
        kind = "M" if e[8] == "*" else e[8][0].upper()
        if kind in "MC" or e[9] == "*":
            left = ko_tag(e[4])
        else:
            left = ko_tag(e[9])
        reading = "" if e[7] == "*" or e[7] == e[0] else e[7]
        extra = kind + "\x01" + reading
        if kind != "M" and e[11] != "*":
            for token in e[11].split("+"):
                parts = token.split("/")
                if parts[0].strip():
                    extra += "\x01" + ko_tag(parts[1]) + "\x02" + parts[0]
        return (int(e[1]), int(e[2]), int(e[3]), wr.string(left), wr.string(extra))

    entries = {}
    for e in lines:
        s = e[0].strip()
        if s:
            entries.setdefault(s, []).append(record(e))
    cat, flags = char_def(files["char.def"], KO_CLASSES, False)
    unk = [csv(l + ",*,*") for l in files["unk.def"].split("\n") if l]
    unk.sort(key=lambda e: KO_CLASSES.index(e[0]))
    unknown = [[] for _ in KO_CLASSES]
    unknown[0].append(record(csv("NGRAM,1798,3559,3677,SY,*,*,*,*,*,*,*")))
    for e in unk:
        unknown[KO_CLASSES.index(e[0])].append(record(e))
    surfaces = sorted(entries, key=java_order)
    return wr.blob(surfaces, entries, matrix(files["matrix.def"]), cat, flags, unknown)


if __name__ == "__main__":
    data = ROOT + "/crates/teseladb-search/data/"
    which = sys.argv[1:] or ["kuromoji", "nori"]
    if "kuromoji" in which:
        open(data + "kuromoji.bin", "wb").write(kuromoji())
    if "nori" in which:
        open(data + "nori.bin", "wb").write(nori())
