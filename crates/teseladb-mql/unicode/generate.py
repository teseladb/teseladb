#!/usr/bin/env python3
"""Writes src/regex/unicode_data.rs from the Unicode files of unicode/ucd.sha256.

    scripts/fetch-pinned.sh crates/teseladb-mql/unicode/ucd.sha256
    python3 -I crates/teseladb-mql/unicode/generate.py .tools/unicode/ucd-16.0.0 \
        > crates/teseladb-mql/src/regex/unicode_data.rs
    cargo fmt -p teseladb-mql

Tables of `(first, last, value)` code point ranges, sorted and without overlaps: general category,
script, script extensions, grapheme cluster break, extended pictographic and bidi class, and the
binary properties PCRE2 10.43 knows (`pcre2test -LP`) as flat lists of `first, last` pairs.
"""

import re
import sys

UCD = sys.argv[1]

CATEGORIES = [
    "Cc", "Cf", "Cn", "Co", "Cs", "Ll", "Lm", "Lo", "Lt", "Lu", "Mc", "Me", "Mn", "Nd", "Nl",
    "No", "Pc", "Pd", "Pe", "Pf", "Pi", "Po", "Ps", "Sc", "Sk", "Sm", "So", "Zl", "Zp", "Zs",
]
GCB = [
    "Other", "CR", "LF", "Control", "Extend", "ZWJ", "Regional_Indicator", "Prepend",
    "SpacingMark", "L", "V", "T", "LV", "LVT",
]

# PCRE2 10.43's binary properties: UCD name and short alias ("" when there is none).
BINARY = [
    ("ASCII", ""), ("ASCII_Hex_Digit", "AHex"), ("Alphabetic", "Alpha"),
    ("Bidi_Control", "Bidi_C"), ("Bidi_Mirrored", "Bidi_M"), ("Case_Ignorable", "CI"),
    ("Cased", ""), ("Changes_When_Casefolded", "CWCF"), ("Changes_When_Casemapped", "CWCM"),
    ("Changes_When_Lowercased", "CWL"), ("Changes_When_Titlecased", "CWT"),
    ("Changes_When_Uppercased", "CWU"), ("Dash", ""), ("Default_Ignorable_Code_Point", "DI"),
    ("Deprecated", "Dep"), ("Diacritic", "Dia"), ("Emoji", ""), ("Emoji_Component", "EComp"),
    ("Emoji_Modifier", "EMod"), ("Emoji_Modifier_Base", "EBase"),
    ("Emoji_Presentation", "EPres"), ("Extended_Pictographic", "ExtPict"), ("Extender", "Ext"),
    ("Grapheme_Base", "Gr_Base"), ("Grapheme_Extend", "Gr_Ext"), ("Grapheme_Link", "Gr_Link"),
    ("Hex_Digit", "Hex"), ("IDS_Binary_Operator", "IDSB"), ("IDS_Trinary_Operator", "IDST"),
    ("ID_Continue", "IDC"), ("ID_Start", "IDS"), ("Ideographic", "Ideo"),
    ("Join_Control", "Join_C"), ("Logical_Order_Exception", "LOE"), ("Lowercase", "Lower"),
    ("Math", ""), ("Noncharacter_Code_Point", "NChar"), ("Pattern_Syntax", "Pat_Syn"),
    ("Pattern_White_Space", "Pat_WS"), ("Prepended_Concatenation_Mark", "PCM"),
    ("Quotation_Mark", "QMark"), ("Radical", ""), ("Regional_Indicator", "RI"),
    ("Sentence_Terminal", "STerm"), ("Soft_Dotted", "SD"), ("Terminal_Punctuation", "Term"),
    ("Unified_Ideograph", "UIdeo"), ("Uppercase", "Upper"), ("Variation_Selector", "VS"),
    ("White_Space", "WSpace"), ("XID_Continue", "XIDC"), ("XID_Start", "XIDS"),
]
BINARY_FILES = ["PropList.txt", "DerivedCoreProperties.txt", "emoji/emoji-data.txt"]


def ranges_of(path):
    """Lines `first..last ; value # comment` of a UCD property file."""
    for line in open(f"{UCD}/{path}", encoding="utf-8"):
        line = line.split("#", 1)[0].strip()
        if not line:
            continue
        cps, value = [x.strip() for x in line.split(";")[:2]]
        first, _, last = cps.partition("..")
        yield int(first, 16), int(last or first, 16), value


def merged(items):
    """Sorted `(first, last, value)` with adjacent equal values joined."""
    out = []
    for first, last, value in sorted(items):
        if out and out[-1][1] + 1 == first and out[-1][2] == value:
            out[-1] = (out[-1][0], last, value)
        else:
            out.append((first, last, value))
    return out


def categories():
    items, pending = [], None
    for line in open(f"{UCD}/UnicodeData.txt", encoding="utf-8"):
        f = line.split(";")
        cp, name, cat = int(f[0], 16), f[1], CATEGORIES.index(f[2])
        if name.endswith(", First>"):
            pending = cp
            continue
        first = pending if name.endswith(", Last>") else cp
        pending = None
        items.append((first, cp, cat))
    return merged(items)


def binary():
    """For each property of BINARY, its merged ranges."""
    found = {name: [] for name, _ in BINARY}
    found["ASCII"].append((0, 0x7F, 0))
    for path in BINARY_FILES:
        for a, b, v in ranges_of(path):
            if v in found:
                found[v].append((a, b, 0))
    for line in open(f"{UCD}/UnicodeData.txt", encoding="utf-8"):
        f = line.split(";")
        if f[9] == "Y":
            cp = int(f[0], 16)
            found["Bidi_Mirrored"].append((cp, cp, 0))
    for name, items in found.items():
        assert items, name
    return [merged(found[name]) for name, _ in BINARY]


def bidi_classes():
    """Short bidi class names, and the ranges of each with the defaults of the @missing lines."""
    longs = {}
    for line in open(f"{UCD}/PropertyValueAliases.txt", encoding="utf-8"):
        line = line.split("#", 1)[0].strip()
        if line.startswith("bc ;"):
            parts = [x.strip() for x in line.split(";")]
            longs[parts[2]] = parts[1]
    names = sorted(longs.values())
    value = {}
    path = f"{UCD}/extracted/DerivedBidiClass.txt"
    for line in open(path, encoding="utf-8"):
        if line.startswith("# @missing:"):
            cps, cls = [x.strip() for x in line[len("# @missing:"):].split(";")]
            first, _, last = cps.partition("..")
            for cp in range(int(first, 16), int(last, 16) + 1):
                value[cp] = names.index(longs[cls])
    for a, b, v in ranges_of("extracted/DerivedBidiClass.txt"):
        for cp in range(a, b + 1):
            value[cp] = names.index(v)
    return names, merged((cp, cp, v) for cp, v in value.items())


def pairs(w, ranges):
    """A flat list of `first, last` pairs."""
    w("&[")
    w(", ".join(f"0x{a:X}, 0x{b:X}" for a, b, _ in ranges))
    w("]")


def script_names():
    """Script long names in order of their short alias, with the aliases."""
    names = {}
    for line in open(f"{UCD}/PropertyValueAliases.txt", encoding="utf-8"):
        line = line.split("#", 1)[0].strip()
        if not line.startswith("sc ;"):
            continue
        parts = [x.strip() for x in line.split(";")]
        names[parts[1]] = parts[2:]
    return names


def main():
    gc = categories()
    aliases = script_names()
    longs = sorted(v[0] for v in aliases.values())
    index = {name: i for i, name in enumerate(longs)}
    short = {v[0]: k for k, v in aliases.items()}
    scripts = merged((a, b, index[v]) for a, b, v in ranges_of("Scripts.txt"))
    sets = []
    scx = []
    for a, b, v in ranges_of("ScriptExtensions.txt"):
        members = tuple(sorted(index[aliases[s][0]] for s in v.split()))
        if members not in sets:
            sets.append(members)
        scx.append((a, b, sets.index(members)))
    scx = merged(scx)
    gcb = merged(
        (a, b, GCB.index(v)) for a, b, v in ranges_of("auxiliary/GraphemeBreakProperty.txt")
    )
    pict = merged(
        (a, b, 0)
        for a, b, v in ranges_of("emoji/emoji-data.txt")
        if v == "Extended_Pictographic"
    )

    w = sys.stdout.write
    w("//! Unicode 16.0.0 tables for the regular expression engine, written by\n")
    w("//! `unicode/generate.py` from the files of `unicode/ucd.sha256`. Do not edit.\n\n")
    w("/// General categories, in the order of the values of [`CATEGORY_RANGES`].\n")
    w("pub(super) const CATEGORIES: [&str; %d] = [%s];\n\n" % (
        len(CATEGORIES), ", ".join(f'"{c}"' for c in CATEGORIES)))
    w("/// Assigned code points and their general category; the rest are `Cn`.\n")
    w("pub(super) static CATEGORY_RANGES: &[(u32, u32, u8)] = &[\n")
    for a, b, v in gc:
        w(f"    (0x{a:X}, 0x{b:X}, {v}),\n")
    w("];\n\n")
    w("/// Scripts: long name and short alias, in the order of the values of the script tables.\n")
    w("pub(super) const SCRIPTS: [(&str, &str); %d] = [\n" % len(longs))
    for name in longs:
        w(f'    ("{name}", "{short[name]}"),\n')
    w("];\n\n")
    w("/// Code points and their script; the rest are `Unknown`.\n")
    w("pub(super) static SCRIPT_RANGES: &[(u32, u32, u8)] = &[\n")
    for a, b, v in scripts:
        w(f"    (0x{a:X}, 0x{b:X}, {v}),\n")
    w("];\n\n")
    w("/// Sets of scripts of the script extensions.\n")
    w("pub(super) static SCRIPT_SETS: &[&[u8]] = &[\n")
    for s in sets:
        w("    &[%s],\n" % ", ".join(str(x) for x in s))
    w("];\n\n")
    w("/// Code points whose script extensions are not their script, with their set.\n")
    w("pub(super) static SCRIPT_EXTENSION_RANGES: &[(u32, u32, u8)] = &[\n")
    for a, b, v in scx:
        w(f"    (0x{a:X}, 0x{b:X}, {v}),\n")
    w("];\n\n")
    w("/// Code points and their grapheme cluster break, in the order of `Gcb` of\n")
    w("/// `unicode.rs`; the rest are `Other`.\n")
    w("pub(super) static GRAPHEME_BREAK_RANGES: &[(u32, u32, u8)] = &[\n")
    for a, b, v in gcb:
        w(f"    (0x{a:X}, 0x{b:X}, {v}),\n")
    w("];\n\n")
    w("/// Binary properties: UCD name and short alias, in the order of [`BINARY_RANGES`].\n")
    w("pub(super) const BINARY: [(&str, &str); %d] = [\n" % len(BINARY))
    for name, alias in BINARY:
        w(f'    ("{name}", "{alias}"),\n')
    w("];\n\n")
    w("/// The code points of each binary property, as flat `first, last` pairs.\n")
    w("pub(super) static BINARY_RANGES: [&[u32]; %d] = [\n" % len(BINARY))
    for ranges in binary():
        w("    ")
        pairs(w, ranges)
        w(",\n")
    w("];\n\n")
    names, bidi = bidi_classes()
    w("/// Bidi classes by short name, in the order of the values of [`BIDI_RANGES`].\n")
    w("pub(super) const BIDI_CLASSES: [&str; %d] = [%s];\n\n" % (
        len(names), ", ".join(f'"{n}"' for n in names)))
    w("/// Every code point and its bidi class.\n")
    w("pub(super) static BIDI_RANGES: &[(u32, u32, u8)] = &[\n")
    for a, b, v in bidi:
        w(f"    (0x{a:X}, 0x{b:X}, {v}),\n")
    w("];\n\n")
    w("/// Extended pictographic code points.\n")
    w("pub(super) static PICTOGRAPHIC_RANGES: &[(u32, u32, u8)] = &[\n")
    for a, b, v in pict:
        w(f"    (0x{a:X}, 0x{b:X}, {v}),\n")
    w("];\n")


if __name__ == "__main__":
    main()
