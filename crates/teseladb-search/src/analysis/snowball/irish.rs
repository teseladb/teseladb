//! The Irish stemmer of `snowballStemming` (`irish`), deduced from the stems the reference search server 1.70.6 gives
//! for the 186 280 words of the Irish Snowball vocabulary (D-051): the initial mutations come off
//! first (eclipsis and lenition, `h-`, `n-` and `t-`), then the endings of five steps. RV starts
//! after the first vowel; R1 and R2 are the standard regions.

use std::sync::OnceLock;

use super::table::{Cond, Group, Step, StepDef, When, build, run_marked};
use super::word::regions;

const MUTATIONS: &[Group] = &[
    Group { cond: Cond::MinLen(0), to: "", from: &["h-", "n-", "t-"] },
    Group { cond: Cond::MinLen(0), to: "b", from: &["bh", "mb"] },
    Group { cond: Cond::MinLen(0), to: "c", from: &["ch", "gc"] },
    Group { cond: Cond::MinLen(0), to: "d", from: &["dh", "nd"] },
    Group { cond: Cond::MinLen(0), to: "f", from: &["bhf", "fh"] },
    Group { cond: Cond::MinLen(0), to: "g", from: &["gh", "ng"] },
    Group { cond: Cond::MinLen(0), to: "m", from: &["mh"] },
    Group { cond: Cond::MinLen(0), to: "p", from: &["bp", "ph"] },
    Group { cond: Cond::MinLen(0), to: "s", from: &["sh", "ts"] },
    Group { cond: Cond::MinLen(0), to: "t", from: &["dt", "th"] },
];

const STEP_1: &[Group] = &[
    Group { cond: Cond::Rv, to: "", from: &["aímid"] },
    Group { cond: Cond::R1, to: "", from: &["aibh", "ibh"] },
];

const STEP_2: &[Group] = &[
    Group { cond: Cond::Always, to: "af", from: &["afaíoch"] },
    Group { cond: Cond::Always, to: "arc", from: &["arcachtaí"] },
    Group { cond: Cond::Always, to: "gin", from: &["gineas"] },
    Group { cond: Cond::Always, to: "paite", from: &["pataigh"] },
    Group { cond: Cond::Always, to: "óid", from: &["óideacha", "óidigh"] },
    Group { cond: Cond::Rv, to: "", from: &["áileamh"] },
    Group {
        cond: Cond::R1,
        to: "",
        from: &["abh", "adhaire", "aimh", "amh", "eabh", "eamh", "imh"],
    },
    Group { cond: Cond::R2, to: "", from: &["aire", "airí", "hrafaíoch", "ire", "irí"] },
];

const STEP_3: &[Group] = &[
    Group { cond: Cond::Always, to: "arc", from: &["arcacht"] },
    Group { cond: Cond::Always, to: "paite", from: &["patach"] },
    Group { cond: Cond::Always, to: "óid", from: &["óideach"] },
    Group { cond: Cond::Rv, to: "", from: &["eadhach", "áileach", "úáileach"] },
    Group {
        cond: Cond::R1,
        to: "",
        from: &["adhach", "aíocht", "aíochta", "áilíocht", "íocht", "íochta"],
    },
    Group {
        cond: Cond::R2,
        to: "",
        from: &["ach", "acht", "achtúil", "each", "eacht", "eachta", "eachtúil"],
    },
];

const STEP_4: &[Group] = &[
    Group { cond: Cond::Rv, to: "", from: &["ailíocht", "gastar"] },
    Group { cond: Cond::R1, to: "", from: &["tar", "íocht"] },
    Group { cond: Cond::R1, to: "ch", from: &["cháil"] },
    Group { cond: Cond::R2, to: "", from: &["achta", "aíocht"] },
    Group { cond: Cond::MinLen(2), to: "il", from: &["ileamh"] },
];

const STEP_5: &[Group] = &[
    Group { cond: Cond::Rv, to: "", from: &["aimid", "faidh", "fidh", "imid", "ímid"] },
    Group { cond: Cond::R1, to: "", from: &["adh", "ain", "aímid", "eadh", "tear", "áil"] },
];

const STEPS: [StepDef; 6] = [
    StepDef {
        groups: MUTATIONS,
        prefix: true,
        repeat: false,
        alternative: false,
        within: None,
        when: When::Always,
        protect: true,
    },
    StepDef {
        groups: STEP_1,
        prefix: false,
        repeat: false,
        alternative: false,
        within: None,
        when: When::Always,
        protect: true,
    },
    StepDef {
        groups: STEP_2,
        prefix: false,
        repeat: false,
        alternative: false,
        within: None,
        when: When::Always,
        protect: true,
    },
    StepDef {
        groups: STEP_3,
        prefix: false,
        repeat: false,
        alternative: false,
        within: None,
        when: When::Always,
        protect: true,
    },
    StepDef {
        groups: STEP_4,
        prefix: false,
        repeat: false,
        alternative: false,
        within: None,
        when: When::Always,
        protect: true,
    },
    StepDef {
        groups: STEP_5,
        prefix: false,
        repeat: false,
        alternative: false,
        within: None,
        when: When::Always,
        protect: true,
    },
];

static BUILT: OnceLock<Vec<Step>> = OnceLock::new();

/// Whether `c` is a vowel of the regions.
fn vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u' | 'á' | 'é' | 'í' | 'ó' | 'ú')
}

/// The stem of `word`.
pub fn stem(word: &str) -> String {
    let c: Vec<char> = word.chars().collect();
    let rv = c.iter().position(|&ch| vowel(ch)).map_or(c.len(), |i| i + 1);
    let (r1, r2) = regions(&c, vowel);
    run_marked(c, BUILT.get_or_init(|| build(&STEPS)), [rv, r1, r2])
}
