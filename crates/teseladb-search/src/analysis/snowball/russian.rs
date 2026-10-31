//! The Russian stemmer of snowballstem.org (`algorithms/russian/stemmer.html`).

use super::word::{Word, region};

fn vowel(c: char) -> bool {
    matches!(c, 'а' | 'е' | 'и' | 'о' | 'у' | 'ы' | 'э' | 'ю' | 'я')
}

const GERUND1: &[&str] = &["в", "вши", "вшись"];
const GERUND2: &[&str] = &["ив", "ивши", "ившись", "ыв", "ывши", "ывшись"];
const ADJECTIVE: &[&str] = &[
    "ее", "ие", "ые", "ое", "ими", "ыми", "ей", "ий", "ый", "ой", "ем", "им", "ым", "ом", "его",
    "ого", "ему", "ому", "их", "ых", "ую", "юю", "ая", "яя", "ою", "ею",
];
const PARTICIPLE1: &[&str] = &["ем", "нн", "вш", "ющ", "щ"];
const PARTICIPLE2: &[&str] = &["ивш", "ывш", "ующ"];
const VERB1: &[&str] = &[
    "ла", "на", "ете", "йте", "ли", "й", "л", "ем", "н", "ло", "но", "ет", "ют", "ны", "ть", "ешь",
    "нно",
];
const VERB2: &[&str] = &[
    "ила", "ыла", "ена", "ейте", "уйте", "ите", "или", "ыли", "ей", "уй", "ил", "ыл", "им", "ым",
    "ен", "ило", "ыло", "ено", "ят", "ует", "уют", "ит", "ыт", "ены", "ить", "ыть", "ишь", "ую",
    "ю",
];
const NOUN: &[&str] = &[
    "а", "ев", "ов", "ие", "ье", "е", "иями", "ями", "ами", "еи", "ии", "и", "ией", "ей", "ой",
    "ий", "й", "иям", "ям", "ием", "ем", "ам", "ом", "о", "у", "ах", "иях", "ях", "ы", "ь", "ию",
    "ью", "ю", "ия", "ья", "я",
];

/// Removes the longest suffix of `group1` (after `а` or `я`) or `group2` that lies in RV; a
/// match of `group1` without `а` or `я` before it removes nothing. Says whether one went.
fn remove_grouped(w: &mut Word, group1: &[&str], group2: &[&str], rv: usize) -> bool {
    let all: Vec<&str> = [group1, group2].concat();
    let Some(s) = w.longest_within(&all, rv) else { return false };
    let start = w.start_of(s);
    if group1.contains(&s) && !(start > rv && matches!(w.c[start - 1], 'а' | 'я')) {
        return false;
    }
    w.cut(s.chars().count());
    true
}

/// Removes the longest suffix of `list` that lies in RV; says whether one went.
fn remove(w: &mut Word, list: &[&str], rv: usize) -> bool {
    let found = w.longest_within(list, rv);
    if let Some(s) = found {
        w.cut(s.chars().count());
    }
    found.is_some()
}

/// Stems `text`.
pub fn stem(text: &str) -> String {
    let mut w = Word { c: text.chars().map(|c| if c == 'ё' { 'е' } else { c }).collect() };
    let rv = w.c.iter().position(|&c| vowel(c)).map_or(w.len(), |i| i + 1);
    let r2 = region(&w.c, region(&w.c, 0, vowel), vowel);
    if !remove_grouped(&mut w, GERUND1, GERUND2, rv) {
        remove(&mut w, &["ся", "сь"], rv);
        if remove(&mut w, ADJECTIVE, rv) {
            remove_grouped(&mut w, PARTICIPLE1, PARTICIPLE2, rv);
        } else if !remove_grouped(&mut w, VERB1, VERB2, rv) {
            remove(&mut w, NOUN, rv);
        }
    }
    remove(&mut w, &["и"], rv);
    remove(&mut w, &["ост", "ость"], r2);
    if w.ends("нн") {
        w.cut(1);
    } else if remove(&mut w, &["ейш", "ейше"], rv) {
        if w.ends("нн") {
            w.cut(1);
        }
    } else {
        remove(&mut w, &["ь"], rv);
    }
    w.text()
}
