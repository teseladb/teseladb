//! Script reordering (`[reorder …]`, ICU's `CollationData::makeReorderRanges`): the root's groups
//! (one per script marker, plus the five special ones and the unassigned code points) are
//! contiguous ranges of primaries, and a reordering lays them out again in a new order without
//! touching the primaries below the first group (U+FFFE) or above the last one (trailing).
//!
//! The new order: the special groups (`space`, `punct`, `symbol`, `currency`, `digit`) that the
//! list does not name, then the list in its order, then every other group in its usual order; a
//! code after `others` (or `Zzzz`) goes to the end instead. Codes are matched without case; a
//! code with no group is skipped; naming a group twice is an error, as is `default` with others.

/// End of the last group: the trailing primaries stay where they are.
const GROUPS_END: u32 = 0xEF00_0000;

/// A reordering of primaries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reordering {
    /// Each group's range and where it starts now, sorted by range.
    moves: Vec<(u32, u32, u32)>,
}

const SPECIALS: [&str; 5] = ["space", "punct", "symbol", "currency", "digit"];

impl Reordering {
    /// The reordering `codes` asks for over the root's `groups` (first primary and codes, sorted);
    /// `None` when nothing moves (no codes, or `default`).
    pub fn new(
        codes: &[String],
        groups: &[(u32, Vec<String>)],
    ) -> Result<Option<Reordering>, String> {
        let codes: Vec<String> = codes.iter().map(|c| c.to_ascii_lowercase()).collect();
        if codes.is_empty() || codes == ["default"] || codes == ["others"] || codes == ["zzzz"] {
            return Ok(None);
        }
        let group_of = |code: &str| {
            groups.iter().position(|(_, names)| names.iter().any(|n| n.eq_ignore_ascii_case(code)))
        };
        let mut placed = vec![false; groups.len()];
        let mut place = |g: usize, to: &mut Vec<usize>| -> Result<(), String> {
            if placed[g] {
                return Err(format!("[reorder] names the group of {} twice", groups[g].1[0]));
            }
            placed[g] = true;
            to.push(g);
            Ok(())
        };
        let mut low = Vec::new();
        for special in SPECIALS {
            if !codes.iter().any(|c| c == special)
                && let Some(g) = group_of(special)
            {
                place(g, &mut low)?;
            }
        }
        let mut high = Vec::new();
        let mut rest = codes.iter();
        for code in rest.by_ref() {
            if code == "others" || code == "zzzz" {
                break;
            }
            if code == "default" {
                return Err("[reorder default] must be alone".into());
            }
            if let Some(g) = group_of(code) {
                place(g, &mut low)?;
            }
        }
        for code in rest {
            if matches!(code.as_str(), "others" | "zzzz" | "default") {
                return Err(format!("[reorder] cannot put {code} after others"));
            }
            if let Some(g) = group_of(code) {
                place(g, &mut high)?;
            }
        }
        let middle: Vec<usize> = (0..groups.len()).filter(|g| !placed[*g]).collect();
        let range = |g: usize| (groups[g].0, groups.get(g + 1).map_or(GROUPS_END, |n| n.0));
        let mut at = groups[0].0;
        let mut moves = Vec::with_capacity(groups.len());
        for g in low.into_iter().chain(middle).chain(high) {
            let (start, end) = range(g);
            moves.push((start, end, at));
            at += end - start;
        }
        moves.sort_unstable();
        Ok(Some(Reordering { moves }))
    }

    /// Where the primary `p` goes.
    pub fn map(&self, p: u32) -> u32 {
        let i = self.moves.partition_point(|(start, _, _)| *start <= p);
        match i.checked_sub(1).map(|i| self.moves[i]) {
            Some((start, end, to)) if p < end => to + (p - start),
            _ => p,
        }
    }
}
