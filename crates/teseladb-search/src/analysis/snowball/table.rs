//! The engine of the stemmers deduced from observation (D-051): each one is a sequence of steps,
//! and each step a table of affixes, sorted, with the condition under which the longest affix the
//! word has is removed or rewritten. As in a Snowball `among`, the longest affix decides alone:
//! when its condition fails, the step stops without trying a shorter one.

use super::word::{regions, rv};

/// When a rule applies, measured on the word as it was before any step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cond {
    /// Always, if something is left before a suffix.
    Always,
    /// The suffix starts in RV.
    Rv,
    /// The suffix starts in R1.
    R1,
    /// The suffix starts in R2.
    R2,
    /// At least this many characters are left.
    MinLen(usize),
}

/// An affix, its condition and what replaces it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rule {
    /// The affix.
    pub from: &'static str,
    /// When it applies.
    pub cond: Cond,
    /// What replaces it (empty to remove it).
    pub to: &'static str,
}

/// Affixes that share a condition and a replacement, as the stemmers write them.
#[derive(Clone, Copy, Debug)]
pub struct Group {
    /// When they apply.
    pub cond: Cond,
    /// What replaces them (empty to remove them).
    pub to: &'static str,
    /// The affixes.
    pub from: &'static [&'static str],
}

/// A step as written: its groups, whether its affixes are prefixes and whether it repeats while
/// it removes something.
#[derive(Clone, Copy, Debug)]
pub struct StepDef {
    /// The groups of rules.
    pub groups: &'static [Group],
    /// Whether the affixes are prefixes.
    pub prefix: bool,
    /// Whether the step repeats after a removal.
    pub repeat: bool,
    /// Whether the step runs only when the one before it changed nothing.
    pub alternative: bool,
}

/// A step ready to run: its rules sorted by affix and the length of the longest one.
#[derive(Clone, Debug)]
pub struct Step {
    rules: Vec<Rule>,
    longest: usize,
    prefix: bool,
    repeat: bool,
    alternative: bool,
}

/// The steps of `defs`, ready to run.
pub fn build(defs: &[StepDef]) -> Vec<Step> {
    defs.iter()
        .map(|d| {
            let mut rules: Vec<Rule> = d
                .groups
                .iter()
                .flat_map(|g| g.from.iter().map(|&from| Rule { from, cond: g.cond, to: g.to }))
                .collect();
            rules.sort_by(|a, b| a.from.cmp(b.from));
            let longest = rules.iter().map(|r| r.from.chars().count()).max().unwrap_or(0);
            Step { rules, longest, prefix: d.prefix, repeat: d.repeat, alternative: d.alternative }
        })
        .collect()
}

/// The stem of `word` (already normalised) after `steps`, with regions computed with `vowel`.
pub fn run(word: &str, steps: &[Step], vowel: impl Fn(char) -> bool + Copy) -> String {
    let mut c: Vec<char> = word.chars().collect();
    let (r1, r2) = regions(&c, vowel);
    let marks = [rv(&c, vowel), r1, r2];
    let mut removed_front = 0;
    let mut changed = false;
    for step in steps {
        if step.alternative && changed {
            changed = false;
            continue;
        }
        let before = c.clone();
        while let Some(rule) = longest(&c, step) {
            let n = rule.from.chars().count();
            let left = c.len() - n;
            let allowed = match rule.cond {
                Cond::MinLen(k) => left >= k,
                _ if step.prefix => false,
                Cond::Always => left > 0,
                Cond::Rv => left > 0 && left + removed_front >= marks[0],
                Cond::R1 => left > 0 && left + removed_front >= marks[1],
                Cond::R2 => left > 0 && left + removed_front >= marks[2],
            };
            if !allowed {
                break;
            }
            let to: Vec<char> = rule.to.chars().collect();
            if step.prefix {
                removed_front += n;
                removed_front -= to.len().min(removed_front);
                c.splice(..n, to.iter().copied());
            } else {
                c.truncate(left);
                c.extend(to.iter().copied());
            }
            if !step.repeat || !rule.to.is_empty() {
                break;
            }
        }
        changed = c != before;
    }
    c.into_iter().collect()
}

/// The rule of `step` with the longest affix `c` has.
fn longest(c: &[char], step: &Step) -> Option<Rule> {
    let mut key = String::new();
    for n in (1..=step.longest.min(c.len())).rev() {
        key.clear();
        let part = if step.prefix { &c[..n] } else { &c[c.len() - n..] };
        key.extend(part.iter());
        if let Ok(i) = step.rules.binary_search_by(|r| r.from.cmp(key.as_str())) {
            return Some(step.rules[i]);
        }
    }
    None
}
