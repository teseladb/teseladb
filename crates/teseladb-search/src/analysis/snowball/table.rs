//! The engine of the stemmers deduced from observation (D-051): each one is a sequence of steps,
//! and each step a table of affixes, sorted, with the condition under which the longest affix the
//! word has is removed or rewritten. As in a Snowball `among`, the longest affix decides alone:
//! when its condition fails, the step stops without trying a shorter one. A step may also look
//! only inside a region: then the longest affix is the longest one that starts in the region. An
//! affix rewritten to itself is kept: it stays at the end and the steps go on looking before it.

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

/// Whether a step runs, by what an earlier step did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum When {
    /// Always.
    Always,
    /// Only when the step at this index changed the word.
    Changed(usize),
    /// Only when the step at this index left the word as it was.
    Unchanged(usize),
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
    /// The region the affixes are looked for in (`None`: the whole word).
    pub within: Option<Cond>,
    /// Whether the step runs, by what an earlier step did.
    pub when: When,
    /// Whether an affix rewritten to itself stays out of the later steps' reach; without it the
    /// rule only counts as the step having acted (as Snowball's Catalan does).
    pub protect: bool,
}

/// A step ready to run: its rules sorted by affix and the length of the longest one.
#[derive(Clone, Debug)]
pub struct Step {
    rules: Vec<Rule>,
    longest: usize,
    prefix: bool,
    repeat: bool,
    alternative: bool,
    within: Option<Cond>,
    when: When,
    protect: bool,
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
            Step {
                rules,
                longest,
                prefix: d.prefix,
                repeat: d.repeat,
                alternative: d.alternative,
                within: d.within,
                when: d.when,
                protect: d.protect,
            }
        })
        .collect()
}

/// The stem of `c` after `steps`, with RV, R1 and R2 starting at `marks`.
pub fn run_marked(mut c: Vec<char>, steps: &[Step], marks: [usize; 3]) -> String {
    let mut removed_front = 0;
    let mut changed = false;
    // Characters at the end kept by an affix rewritten to itself; the steps look before them.
    let mut kept = 0;
    // Whether each step changed the word.
    let mut did = Vec::with_capacity(steps.len());
    for step in steps {
        let runs = match step.when {
            When::Always => true,
            When::Changed(i) => did.get(i).copied().unwrap_or(false),
            When::Unchanged(i) => !did.get(i).copied().unwrap_or(false),
        };
        if !runs {
            did.push(false);
            continue;
        }
        if step.alternative && changed {
            changed = false;
            did.push(false);
            continue;
        }
        let kept_before = kept;
        let before = c.clone();
        let mut acted = false;
        loop {
            let start = step.within.map_or(0, |r| mark(r, &marks).saturating_sub(removed_front));
            let end = if step.prefix { c.len() } else { c.len() - kept };
            let Some(rule) = longest(&c[..end], step, start) else { break };
            let n = rule.from.chars().count();
            let left = end - n;
            let allowed = match rule.cond {
                Cond::MinLen(k) => left >= k,
                _ if step.prefix => false,
                Cond::Always => left > 0 || !rule.to.is_empty(),
                cond => left > 0 && left + removed_front >= mark(cond, &marks),
            };
            if !allowed {
                break;
            }
            let to: Vec<char> = rule.to.chars().collect();
            let identity = !step.prefix && rule.to == rule.from;
            if identity && !step.protect {
                acted = true;
                break;
            }
            let keep = identity;
            if step.prefix {
                removed_front += n;
                removed_front -= to.len().min(removed_front);
                c.splice(..n, to.iter().copied());
            } else if keep {
                kept += n;
            } else {
                c.splice(left..end, to.iter().copied());
            }
            if !step.repeat || !(rule.to.is_empty() || keep) {
                break;
            }
        }
        changed = c != before || kept != kept_before || acted;
        did.push(changed);
    }
    c.into_iter().collect()
}

/// Where the region of `cond` starts (0 for the conditions that are not regions).
fn mark(cond: Cond, marks: &[usize; 3]) -> usize {
    match cond {
        Cond::Rv => marks[0],
        Cond::R1 => marks[1],
        Cond::R2 => marks[2],
        Cond::Always | Cond::MinLen(_) => 0,
    }
}

/// The rule of `step` with the longest affix `c` has, among those that leave at least `start`
/// characters before them.
fn longest(c: &[char], step: &Step, start: usize) -> Option<Rule> {
    let mut key = String::new();
    for n in (1..=step.longest.min(c.len().saturating_sub(start))).rev() {
        key.clear();
        let part = if step.prefix { &c[..n] } else { &c[c.len() - n..] };
        key.extend(part.iter());
        if let Ok(i) = step.rules.binary_search_by(|r| r.from.cmp(key.as_str())) {
            return Some(step.rules[i]);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::super::word::{regions, rv};
    use super::*;

    fn vowel(c: char) -> bool {
        matches!(c, 'a' | 'e' | 'i' | 'o' | 'u')
    }

    /// `run_marked` with the standard regions of `word`.
    fn run(word: &str, steps: &[Step], vowel: impl Fn(char) -> bool + Copy) -> String {
        let c: Vec<char> = word.chars().collect();
        let (r1, r2) = regions(&c, vowel);
        let marks = [rv(&c, vowel), r1, r2];
        run_marked(c, steps, marks)
    }

    const PLURAL: &[Group] = &[
        Group { cond: Cond::R1, to: "", from: &["s", "es"] },
        Group { cond: Cond::R2, to: "", from: &["ies", "xes"] },
    ];
    const ENDINGS: &[Group] = &[
        Group { cond: Cond::Rv, to: "", from: &["a", "o"] },
        Group { cond: Cond::Always, to: "y", from: &["i"] },
    ];
    const PREFIXES: &[Group] = &[
        Group { cond: Cond::MinLen(3), to: "", from: &["un", "re"] },
        Group { cond: Cond::MinLen(2), to: "x", from: &["ex"] },
    ];

    const REGION_PREFIX: &[Group] = &[Group { cond: Cond::R1, to: "", from: &["de"] }];

    const SHORT: &[Group] = &[Group { cond: Cond::MinLen(2), to: "", from: &["ts"] }];

    const KEEP: &[Group] = &[
        Group { cond: Cond::Rv, to: "", from: &["a", "o"] },
        Group { cond: Cond::Always, to: "xo", from: &["xo"] },
    ];

    fn step(groups: &'static [Group]) -> StepDef {
        StepDef {
            groups,
            prefix: false,
            repeat: false,
            alternative: false,
            within: None,
            when: When::Always,
            protect: true,
        }
    }

    #[test]
    fn the_longest_affix_decides_alone() {
        let steps = build(&[step(PLURAL)]);
        // "ies" is the longest and fails R2, so "s" is not tried.
        assert_eq!(run("tories", &steps, vowel), "tories");
        assert_eq!(run("strawberries", &steps, vowel), "strawberr");
        assert_eq!(run("taxes", &steps, vowel), "taxes");
        assert_eq!(run("cats", &steps, vowel), "cat");
        assert_eq!(run("s", &steps, vowel), "s");
    }

    #[test]
    fn a_step_within_a_region_skips_affixes_that_start_before_it() {
        let within = StepDef { within: Some(Cond::R2), ..step(PLURAL) };
        let steps = build(&[within]);
        // R2 of "tories" is at the end, so nothing starts inside it.
        assert_eq!(run("tories", &steps, vowel), "tories");
        let r1 = build(&[StepDef { within: Some(Cond::R1), ..step(PLURAL) }]);
        // "xes" starts before R1 (3), so the longest affix inside it is "es".
        assert_eq!(run("taxes", &r1, vowel), "tax");
    }

    #[test]
    fn steps_repeat_alternate_and_rewrite() {
        let repeat = StepDef { repeat: true, ..step(ENDINGS) };
        let alternative = StepDef { alternative: true, ..step(PLURAL) };
        let steps = build(&[repeat, alternative]);
        // "a" then "o" removed in RV; the plural step is skipped because the first changed it.
        assert_eq!(run("bananaoa", &steps, vowel), "banan");
        // A rewrite ends the repetition.
        assert_eq!(run("candi", &steps, vowel), "candy");
        // Nothing changed in the first step, so the alternative runs.
        assert_eq!(run("hats", &steps, vowel), "hat");
        // The alternative is skipped only once.
        let three = build(&[step(ENDINGS), alternative, step(PLURAL)]);
        assert_eq!(run("modelsa", &three, vowel), "model");
    }

    #[test]
    fn an_affix_rewritten_to_itself_is_kept_and_the_step_goes_on_before_it() {
        let repeat = build(&[StepDef { repeat: true, ..step(KEEP) }]);
        // "o" and "a" go, "xo" stays and the "a" before it goes too.
        assert_eq!(run("candaxoao", &repeat, vowel), "candxo");
        // A later step also looks only before the kept affix.
        let two = build(&[step(KEEP), step(ENDINGS)]);
        assert_eq!(run("candaxo", &two, vowel), "candxo");
        // A rewrite after a kept affix leaves the kept affix in place.
        assert_eq!(run("candixo", &build(&[step(KEEP), step(ENDINGS)]), vowel), "candyxo");
    }

    #[test]
    fn an_affix_rewritten_to_itself_without_protection_only_counts_as_acting() {
        let open = |d: &'static [Group]| StepDef { protect: false, ..step(d) };
        // "xo" stops the step; the next step sees the whole word and removes the final "o".
        assert_eq!(run("candaxo", &build(&[open(KEEP), open(ENDINGS)]), vowel), "candax");
        // The step acted, so a step that runs only after a change runs, over the whole word.
        let after = StepDef { when: When::Changed(0), ..open(ENDINGS) };
        assert_eq!(run("candxo", &build(&[open(KEEP), after]), vowel), "candx");
        let otherwise = StepDef { when: When::Unchanged(0), ..open(SHORT) };
        assert_eq!(run("cats", &build(&[open(KEEP), otherwise]), vowel), "ca");
    }

    #[test]
    fn a_step_can_depend_on_whether_an_earlier_one_changed_the_word() {
        let after = StepDef { when: When::Changed(0), ..step(PLURAL) };
        let otherwise = StepDef { when: When::Unchanged(0), ..step(SHORT) };
        let steps = build(&[step(ENDINGS), after, otherwise]);
        // "a" goes in the first step, so the plural step runs and the short one does not.
        assert_eq!(run("catsa", &steps, vowel), "cat");
        // Nothing goes in the first step: the short step removes "ts" instead.
        assert_eq!(run("cats", &steps, vowel), "ca");
    }

    #[test]
    fn prefixes_need_their_length_and_move_the_regions() {
        let prefix = StepDef { prefix: true, ..step(PREFIXES) };
        let steps = build(&[prefix, step(PLURAL)]);
        assert_eq!(run("unlocks", &steps, vowel), "lock");
        assert_eq!(run("unit", &steps, vowel), "unit");
        assert_eq!(run("exits", &steps, vowel), "xit");
        // A prefix rule whose condition is not a length never applies.
        let steps = build(&[StepDef { prefix: true, ..step(REGION_PREFIX) }]);
        assert_eq!(run("decode", &steps, vowel), "decode");
        let marked = run_marked("unlocks".chars().collect(), &build(&[step(PLURAL)]), [0, 7, 7]);
        assert_eq!(marked, "unlocks");
    }
}
