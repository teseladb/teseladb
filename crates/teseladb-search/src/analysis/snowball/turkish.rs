//! The Turkish stemmer of `snowballStemming` (`turkish`), deduced from the stems `refsearch` 1.70.6
//! gives for the 96 325 words of the Turkish Snowball vocabulary (D-051). Two machines remove
//! endings from the end of the word: first the endings of the nominal predicate, then the noun
//! endings. Each state lists, in order, the classes that may come off next, and the first one that
//! matches comes off. An ending is spelt with vowels that agree with each other (`ları`, `leri`);
//! some take a buffer letter before them (`y`, `n`, `s`, or a high vowel before a possessive).
//! Endings that check harmony need, before the last vowel of the word, a vowel of its family. A
//! word with fewer than two vowels stays as it is, and a plural taken first by the predicate
//! machine ends the stemming. At the end a final `d` or `g` takes the high vowel that agrees with
//! the stem and a final `b`, `c` or `ğ` hardens, except in the stems `ad` and `soyad`.

/// The letter an ending may take before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Buffer {
    /// None.
    None,
    /// `y`.
    Y,
    /// `n`.
    N,
    /// `s`.
    S,
    /// A high vowel (`ı`, `i`, `u`, `ü`) after a consonant.
    U,
}

/// How an ending class is written and checked.
struct Spec {
    /// Its spellings, longest first.
    spellings: &'static [&'static str],
    /// The letter it may take before it.
    buffer: Buffer,
    /// Whether it checks vowel harmony.
    harmony: bool,
    /// Whether it needs an `n` after a high vowel that follows a consonant.
    required: bool,
    /// For a class of two endings, the length of the first one, where harmony is checked.
    head: Option<usize>,
}

/// The classes of endings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    /// `DA` (A: a or e; U: ı, i, u or ü; D: d or t).
    Locative,
    /// `DAn` (A: a or e; U: ı, i, u or ü; D: d or t).
    Ablative,
    /// `DUr` (A: a or e; U: ı, i, u or ü; D: d or t).
    Copula,
    /// `KIKEEP` (A: a or e; U: ı, i, u or ü; D: d or t).
    KiBeforeDative,
    /// `POSS` (A: a or e; U: ı, i, u or ü; D: d or t).
    Possessive,
    /// `U` (A: a or e; U: ı, i, u or ü; D: d or t).
    HighVowel,
    /// `UmD` (A: a or e; U: ı, i, u or ü; D: d or t).
    FirstPersonAfterCopula,
    /// `ki` (A: a or e; U: ı, i, u or ü; D: d or t).
    Ki,
    /// `lAr` (A: a or e; U: ı, i, u or ü; D: d or t).
    Plural,
    /// `lAr2` (A: a or e; U: ı, i, u or ü; D: d or t).
    PluralAfterGenitive,
    /// `lArD` (A: a or e; U: ı, i, u or ü; D: d or t).
    PluralAfterCopula,
    /// `lArU` (A: a or e; U: ı, i, u or ü; D: d or t).
    TheirPossessive,
    /// `mUsSUnUz` (A: a or e; U: ı, i, u or ü; D: d or t).
    EvidentialSecondPlural,
    /// `mUsUm` (A: a or e; U: ı, i, u or ü; D: d or t).
    EvidentialFirstPerson,
    /// `mUsUz` (A: a or e; U: ı, i, u or ü; D: d or t).
    EvidentialFirstPlural,
    /// `nA` (A: a or e; U: ı, i, u or ü; D: d or t).
    DativeAfterPossessive,
    /// `nDA` (A: a or e; U: ı, i, u or ü; D: d or t).
    LocativeAfterPossessive,
    /// `nDAn` (A: a or e; U: ı, i, u or ü; D: d or t).
    AblativeAfterPossessive,
    /// `nU` (A: a or e; U: ı, i, u or ü; D: d or t).
    AccusativeAfterPossessive,
    /// `nUn` (A: a or e; U: ı, i, u or ü; D: d or t).
    Genitive,
    /// `nUnK` (A: a or e; U: ı, i, u or ü; D: d or t).
    GenitiveBeforeKi,
    /// `ncA` (A: a or e; U: ı, i, u or ü; D: d or t).
    Equative,
    /// `sU` (A: a or e; U: ı, i, u or ü; D: d or t).
    ThirdPossessive,
    /// `sUn` (A: a or e; U: ı, i, u or ü; D: d or t).
    SecondPerson,
    /// `sUnUz` (A: a or e; U: ı, i, u or ü; D: d or t).
    SecondPlural,
    /// `yA` (A: a or e; U: ı, i, u or ü; D: d or t).
    Dative,
    /// `yDU` (A: a or e; U: ı, i, u or ü; D: d or t).
    Past,
    /// `yDUk` (A: a or e; U: ı, i, u or ü; D: d or t).
    PastPerson,
    /// `yU` (A: a or e; U: ı, i, u or ü; D: d or t).
    Accusative,
    /// `yUm` (A: a or e; U: ı, i, u or ü; D: d or t).
    FirstPerson,
    /// `yUz` (A: a or e; U: ı, i, u or ü; D: d or t).
    FirstPlural,
    /// `yken` (A: a or e; U: ı, i, u or ü; D: d or t).
    While,
    /// `ylA` (A: a or e; U: ı, i, u or ü; D: d or t).
    Instrumental,
    /// `ymUs` (A: a or e; U: ı, i, u or ü; D: d or t).
    Evidential,
    /// `ysA` (A: a or e; U: ı, i, u or ü; D: d or t).
    Conditional,
    /// `ysAk` (A: a or e; U: ı, i, u or ü; D: d or t).
    ConditionalPerson,
    /// `ysAn` (A: a or e; U: ı, i, u or ü; D: d or t).
    ConditionalSecond,
}

/// How `class` is written and checked.
fn spec(class: Class) -> Spec {
    match class {
        Class::Locative => Spec {
            spellings: &["da", "de", "ta", "te"],
            buffer: Buffer::None,
            harmony: true,
            required: false,
            head: None,
        },
        Class::Ablative => Spec {
            spellings: &["dan", "den", "tan", "ten"],
            buffer: Buffer::None,
            harmony: true,
            required: false,
            head: None,
        },
        Class::Copula => Spec {
            spellings: &["dir", "dur", "dür", "dır", "tir", "tur", "tür", "tır"],
            buffer: Buffer::None,
            harmony: true,
            required: false,
            head: None,
        },
        Class::KiBeforeDative => Spec {
            spellings: &[],
            buffer: Buffer::None,
            harmony: false,
            required: false,
            head: None,
        },
        Class::Possessive => Spec {
            spellings: &["miz", "muz", "müz", "mız", "niz", "nuz", "nüz", "nız", "m", "n"],
            buffer: Buffer::U,
            harmony: false,
            required: false,
            head: None,
        },
        Class::HighVowel => Spec {
            spellings: &["i", "u", "ü", "ı"],
            buffer: Buffer::None,
            harmony: false,
            required: false,
            head: None,
        },
        Class::FirstPersonAfterCopula => Spec {
            spellings: &["im", "um", "üm", "ım"],
            buffer: Buffer::None,
            harmony: true,
            required: false,
            head: None,
        },
        Class::Ki => Spec {
            spellings: &["ki"],
            buffer: Buffer::None,
            harmony: false,
            required: false,
            head: None,
        },
        Class::Plural => Spec {
            spellings: &["lar", "ler"],
            buffer: Buffer::None,
            harmony: true,
            required: false,
            head: None,
        },
        Class::PluralAfterGenitive => Spec {
            spellings: &["lar", "ler"],
            buffer: Buffer::None,
            harmony: true,
            required: false,
            head: None,
        },
        Class::PluralAfterCopula => Spec {
            spellings: &["lar", "ler"],
            buffer: Buffer::None,
            harmony: true,
            required: false,
            head: None,
        },
        Class::TheirPossessive => Spec {
            spellings: &["ları", "leri"],
            buffer: Buffer::None,
            harmony: true,
            required: false,
            head: None,
        },
        Class::EvidentialSecondPlural => Spec {
            spellings: &["mişsiniz", "muşsunuz", "müşsünüz", "mışsınız"],
            buffer: Buffer::Y,
            harmony: true,
            required: false,
            head: Some(3),
        },
        Class::EvidentialFirstPerson => Spec {
            spellings: &["mişim", "muşum", "müşüm", "mışım"],
            buffer: Buffer::Y,
            harmony: true,
            required: false,
            head: Some(3),
        },
        Class::EvidentialFirstPlural => Spec {
            spellings: &["mişiz", "muşuz", "müşüz", "mışız"],
            buffer: Buffer::Y,
            harmony: true,
            required: false,
            head: Some(3),
        },
        Class::DativeAfterPossessive => Spec {
            spellings: &["a", "e"],
            buffer: Buffer::N,
            harmony: true,
            required: true,
            head: None,
        },
        Class::LocativeAfterPossessive => Spec {
            spellings: &["da", "de", "ta", "te"],
            buffer: Buffer::N,
            harmony: true,
            required: true,
            head: None,
        },
        Class::AblativeAfterPossessive => Spec {
            spellings: &["dan", "den", "tan", "ten"],
            buffer: Buffer::N,
            harmony: true,
            required: true,
            head: None,
        },
        Class::AccusativeAfterPossessive => Spec {
            spellings: &["i", "u", "ü", "ı"],
            buffer: Buffer::N,
            harmony: true,
            required: true,
            head: None,
        },
        Class::Genitive => Spec {
            spellings: &["in", "un", "ün", "ın"],
            buffer: Buffer::N,
            harmony: true,
            required: false,
            head: None,
        },
        Class::GenitiveBeforeKi => Spec {
            spellings: &["in", "un", "ün", "ın"],
            buffer: Buffer::N,
            harmony: true,
            required: false,
            head: None,
        },
        Class::Equative => Spec {
            spellings: &["ca", "ce"],
            buffer: Buffer::N,
            harmony: true,
            required: false,
            head: None,
        },
        Class::ThirdPossessive => Spec {
            spellings: &["i", "u", "ü", "ı"],
            buffer: Buffer::S,
            harmony: true,
            required: false,
            head: None,
        },
        Class::SecondPerson => Spec {
            spellings: &["sin", "sun", "sün", "sın"],
            buffer: Buffer::None,
            harmony: true,
            required: false,
            head: None,
        },
        Class::SecondPlural => Spec {
            spellings: &["siniz", "sunuz", "sünüz", "sınız"],
            buffer: Buffer::None,
            harmony: false,
            required: false,
            head: None,
        },
        Class::Dative => Spec {
            spellings: &["a", "e"],
            buffer: Buffer::Y,
            harmony: true,
            required: false,
            head: None,
        },
        Class::Past => Spec {
            spellings: &["di", "du", "dü", "dı", "ti", "tu", "tü", "tı"],
            buffer: Buffer::Y,
            harmony: true,
            required: false,
            head: None,
        },
        Class::PastPerson => Spec {
            spellings: &[
                "diniz", "dunuz", "dünüz", "dınız", "tiniz", "tunuz", "tünüz", "tınız", "dik",
                "dim", "din", "duk", "dum", "dun", "dük", "düm", "dün", "dık", "dım", "dın", "tik",
                "tim", "tin", "tuk", "tum", "tun", "tük", "tüm", "tün", "tık", "tım", "tın",
            ],
            buffer: Buffer::Y,
            harmony: true,
            required: false,
            head: Some(2),
        },
        Class::Accusative => Spec {
            spellings: &["i", "u", "ü", "ı"],
            buffer: Buffer::Y,
            harmony: true,
            required: false,
            head: None,
        },
        Class::FirstPerson => Spec {
            spellings: &["im", "um", "üm", "ım"],
            buffer: Buffer::Y,
            harmony: true,
            required: false,
            head: None,
        },
        Class::FirstPlural => Spec {
            spellings: &["iz", "uz", "üz", "ız"],
            buffer: Buffer::Y,
            harmony: true,
            required: false,
            head: None,
        },
        Class::While => Spec {
            spellings: &["ken"],
            buffer: Buffer::Y,
            harmony: false,
            required: false,
            head: None,
        },
        Class::Instrumental => Spec {
            spellings: &["la", "le"],
            buffer: Buffer::Y,
            harmony: true,
            required: false,
            head: None,
        },
        Class::Evidential => Spec {
            spellings: &["miş", "muş", "müş", "mış"],
            buffer: Buffer::Y,
            harmony: true,
            required: false,
            head: None,
        },
        Class::Conditional => Spec {
            spellings: &["sa", "se"],
            buffer: Buffer::Y,
            harmony: false,
            required: false,
            head: None,
        },
        Class::ConditionalPerson => Spec {
            spellings: &["sanız", "seniz", "sak", "sam", "san", "sek", "sem", "sen"],
            buffer: Buffer::Y,
            harmony: false,
            required: false,
            head: Some(2),
        },
        Class::ConditionalSecond => Spec {
            spellings: &["san", "sen"],
            buffer: Buffer::Y,
            harmony: false,
            required: false,
            head: None,
        },
    }
}

/// The classes the predicate machine may remove after `state` (`None` at the start).
fn predicate(state: Option<Class>) -> &'static [Class] {
    match state {
        None => &[
            Class::PastPerson,
            Class::ConditionalPerson,
            Class::SecondPlural,
            Class::FirstPlural,
            Class::SecondPerson,
            Class::FirstPerson,
            Class::Copula,
            Class::Evidential,
            Class::Past,
            Class::Conditional,
            Class::While,
            Class::Plural,
        ],
        Some(Class::SecondPlural) | Some(Class::FirstPerson) => &[Class::Evidential, Class::Past],
        Some(Class::FirstPlural) => &[Class::ConditionalSecond, Class::Evidential, Class::Past],
        Some(Class::PluralAfterCopula) | Some(Class::SecondPerson) => &[Class::Evidential],
        Some(Class::Copula) => &[
            Class::Evidential,
            Class::EvidentialSecondPlural,
            Class::EvidentialFirstPlural,
            Class::EvidentialFirstPerson,
            Class::FirstPersonAfterCopula,
            Class::PluralAfterCopula,
        ],
        Some(Class::Plural) => &[
            Class::PastPerson,
            Class::Conditional,
            Class::Evidential,
            Class::Past,
            Class::Copula,
            Class::ConditionalPerson,
        ],
        _ => &[],
    }
}

/// The classes the noun machine may remove after `state` (`None` at the start).
fn noun(state: Option<Class>) -> &'static [Class] {
    match state {
        None => &[
            Class::Ki,
            Class::TheirPossessive,
            Class::AblativeAfterPossessive,
            Class::Ablative,
            Class::LocativeAfterPossessive,
            Class::Locative,
            Class::Genitive,
            Class::Instrumental,
            Class::DativeAfterPossessive,
            Class::AccusativeAfterPossessive,
            Class::Accusative,
            Class::Equative,
            Class::Dative,
            Class::ThirdPossessive,
            Class::Possessive,
            Class::Plural,
        ],
        Some(Class::Ki) => &[Class::Locative, Class::Genitive, Class::LocativeAfterPossessive],
        Some(Class::Plural) => &[Class::Ki],
        Some(Class::PluralAfterGenitive) => {
            &[Class::Ki, Class::TheirPossessive, Class::ThirdPossessive, Class::Possessive]
        }
        Some(Class::Possessive) | Some(Class::HighVowel) | Some(Class::ThirdPossessive) => {
            &[Class::Plural]
        }
        Some(Class::AblativeAfterPossessive) | Some(Class::AccusativeAfterPossessive) => {
            &[Class::ThirdPossessive, Class::HighVowel, Class::Possessive, Class::Plural]
        }
        Some(Class::Ablative) | Some(Class::Accusative) => {
            &[Class::TheirPossessive, Class::Possessive, Class::Plural]
        }
        Some(Class::LocativeAfterPossessive) => &[
            Class::TheirPossessive,
            Class::ThirdPossessive,
            Class::HighVowel,
            Class::Possessive,
            Class::Plural,
        ],
        Some(Class::Locative) | Some(Class::Dative) => &[Class::Possessive, Class::Plural],
        Some(Class::Genitive) => {
            &[Class::Ki, Class::ThirdPossessive, Class::Possessive, Class::PluralAfterGenitive]
        }
        Some(Class::Instrumental) => {
            &[Class::ThirdPossessive, Class::Possessive, Class::PluralAfterGenitive]
        }
        Some(Class::DativeAfterPossessive) => &[
            Class::KiBeforeDative,
            Class::TheirPossessive,
            Class::ThirdPossessive,
            Class::HighVowel,
            Class::Possessive,
            Class::Plural,
        ],
        Some(Class::Equative) => {
            &[Class::TheirPossessive, Class::ThirdPossessive, Class::Possessive, Class::Plural]
        }
        Some(Class::KiBeforeDative) => {
            &[Class::Locative, Class::GenitiveBeforeKi, Class::LocativeAfterPossessive]
        }
        _ => &[],
    }
}

/// The classes one of which must come off after `ki` for it to come off.
const AFTER_KI: &[Class] = &[Class::Locative, Class::Genitive, Class::LocativeAfterPossessive];

fn is_vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'ı' | 'i' | 'o' | 'ö' | 'u' | 'ü')
}

fn is_high(c: char) -> bool {
    matches!(c, 'ı' | 'i' | 'u' | 'ü')
}

/// The vowels that agree with `v` when they come before it.
fn family(v: char) -> &'static [char] {
    match v {
        'a' => &['a', 'ı', 'o', 'u'],
        'ı' => &['a', 'ı'],
        'e' => &['e', 'i', 'ö', 'ü'],
        'i' => &['e', 'i'],
        'o' | 'u' => &['o', 'u'],
        _ => &['ö', 'ü'],
    }
}

/// The high vowel that agrees with `v`.
fn high(v: char) -> char {
    match v {
        'a' | 'ı' => 'ı',
        'e' | 'i' => 'i',
        'o' | 'u' => 'u',
        _ => 'ü',
    }
}

/// Whether the last vowel of `w` has a vowel of its family before it.
fn harmonic(w: &[char]) -> bool {
    let mut vowels = w.iter().rev().copied().filter(|&c| is_vowel(c));
    let Some(last) = vowels.next() else { return false };
    let fam = family(last);
    vowels.any(|c| fam.contains(&c))
}

/// Whether `w` ends with `suffix`.
fn ends_with(w: &[char], suffix: &str) -> bool {
    let n = suffix.chars().count();
    w.len() >= n && w[w.len() - n..].iter().copied().eq(suffix.chars())
}

/// The length of the buffer letter that comes off with an ending after `pre`, or `None` when the
/// ending may not come off: with the buffer letter, the letter before it must be a vowel (a
/// consonant for a high vowel); without it, the letter two places back must be.
fn buffer_len(pre: &[char], buffer: Buffer) -> Option<usize> {
    let has = match (buffer, pre.last()) {
        (Buffer::None, _) => return Some(0),
        (_, None) => return None,
        (Buffer::U, Some(&c)) => is_high(c),
        (Buffer::Y, Some(&c)) => c == 'y',
        (Buffer::N, Some(&c)) => c == 'n',
        (Buffer::S, Some(&c)) => c == 's',
    };
    let before = pre.len().checked_sub(2).map(|i| pre[i])?;
    let want_vowel = buffer != Buffer::U;
    (is_vowel(before) == want_vowel).then_some(usize::from(has))
}

/// How many letters `class` removes from the end of `w` (0 when it does not match).
fn matches(w: &[char], class: Class) -> usize {
    let spec = spec(class);
    for spelling in spec.spellings {
        let n = spelling.chars().count();
        // Only an ending without a buffer letter may be the whole word.
        if !ends_with(w, spelling) || (w.len() == n && spec.buffer != Buffer::None) {
            continue;
        }
        let pre = &w[..w.len() - n];
        let extra = if spec.required {
            if pre.len() < 2 || pre.last() != Some(&'n') {
                continue;
            }
            // The required buffer follows a high vowel that follows a consonant.
            let ok = pre.len() >= 3 && is_high(pre[pre.len() - 2]) && !is_vowel(pre[pre.len() - 3]);
            if !ok {
                return 0;
            }
            1
        } else {
            let Some(extra) = buffer_len(pre, spec.buffer) else { return 0 };
            extra
        };
        // A class of two endings checks harmony at the end of the first one.
        let checked = &w[..w.len() - n + spec.head.unwrap_or(n)];
        if spec.harmony && !harmonic(checked) {
            return 0;
        }
        return n + extra;
    }
    0
}

/// Whether `ki` comes off before a dative that stays: after a locative `da` that follows a back
/// vowel, or after a genitive.
fn ki_before_dative(w: &[char]) -> bool {
    if !ends_with(w, "ki") || w.len() <= 2 {
        return false;
    }
    let rest = &w[..w.len() - 2];
    if ends_with(rest, "da") {
        let before = &rest[..rest.len() - 2];
        return before
            .iter()
            .rev()
            .find(|&&c| is_vowel(c))
            .is_some_and(|&v| matches!(v, 'a' | 'ı' | 'o' | 'u'));
    }
    matches(rest, Class::Genitive) > 0
}

/// Removes endings from the end of `w` with `machine`; returns the first and the last classes
/// removed.
fn run(
    w: &mut Vec<char>,
    machine: fn(Option<Class>) -> &'static [Class],
) -> (Option<Class>, Option<Class>) {
    let mut state = None;
    let mut first = None;
    // The letters the last class removed, and the ending kept at the end of the word.
    let mut cut: Vec<char> = Vec::new();
    let mut kept: Vec<char> = Vec::new();
    loop {
        let taken = machine(state).iter().find_map(|&class| {
            if class == Class::KiBeforeDative {
                return ki_before_dative(w).then_some((class, 2));
            }
            let n = matches(w, class);
            if n == 0 {
                return None;
            }
            let rest = &w[..w.len() - n];
            if class == Class::Ki && !AFTER_KI.iter().any(|&c| matches(rest, c) > 0) {
                return None;
            }
            Some((class, n))
        });
        let Some((class, n)) = taken else { break };
        if class == Class::KiBeforeDative {
            kept = std::mem::take(&mut cut);
        }
        cut = w.split_off(w.len() - n);
        first = first.or(Some(class));
        state = Some(class);
    }
    w.extend(kept);
    (first, state)
}

/// The hard consonant of a final `b`, `c`, `d` or `ğ`.
fn hard(c: char) -> Option<char> {
    match c {
        'b' => Some('p'),
        'c' => Some('ç'),
        'd' => Some('t'),
        'ğ' => Some('k'),
        _ => None,
    }
}

/// The stem of `word`.
pub fn stem(word: &str) -> String {
    let mut w: Vec<char> = word.chars().collect();
    if w.iter().filter(|&&c| is_vowel(c)).count() < 2 {
        return word.to_string();
    }
    let (first, last) = run(&mut w, predicate);
    if first == Some(Class::Plural)
        || matches!(last, Some(Class::PluralAfterCopula | Class::FirstPersonAfterCopula))
    {
        return w.into_iter().collect();
    }
    run(&mut w, noun);
    let reserved = w.iter().copied().eq("ad".chars()) || w.iter().copied().eq("soyad".chars());
    if !reserved {
        let last_vowel = w.iter().rev().copied().find(|&c| is_vowel(c));
        match (w.last().copied(), last_vowel) {
            (Some('d' | 'g'), Some(v)) => w.push(high(v)),
            (Some(c), _) => {
                if let Some(h) = hard(c) {
                    let end = w.len() - 1;
                    w[end] = h;
                }
            }
            _ => {}
        }
    }
    w.into_iter().collect()
}
