//! The tailoring builder: ICU 57.1's `CollationBuilder` (`collationbuilder.cpp`) over the root's
//! weights, so that each tailored element lands where ICU puts it.
//!
//! Every relation inserts a node into a linked list that starts at a root primary: root nodes
//! carry the weights of the root elements the rules reached, tailored nodes the new ones, each
//! with the strength of its difference from what comes before. After all rules, the tailored
//! nodes of each gap between root weights get weights spread over that gap, and the mappings,
//! which point at nodes in the meantime, take them. The weights differ from ICU's bytes but keep
//! its order: tailored weights stay between the same root weights (`CollationRootElements`).

use teseladb_collation::{
    Alternate, COMMON_WEIGHT16, CaseFirst as CollationCaseFirst, Ce,
    MaxVariable as CollationMaxVariable, Root, Settings, Strength, Tailored, elements,
};

use crate::locales::IcuCollations;
use crate::root_elements::{
    BEFORE_WEIGHT16, FIRST_TRAILING_PRIMARY, LAST_COMMON_SECONDARY, ONLY_TERTIARY, RootElements,
    SECONDARY_BOUNDARY, TERTIARY_BOUNDARY,
};
use crate::rules::{self, Anchor, CaseFirst, Level, MaxVariable, Rule, Setting, Target};

/// Primaries from here on mark an element that points at a node (`index` in the low bits,
/// strength in the secondary, case in the tertiary); no root primary reaches it.
const TEMP: u32 = 0xF000_0000;
/// The case bits of a tertiary weight.
const CASE_MASK: u16 = 0xC000;
/// At most this many elements per mapping, as in ICU.
const MAX_EXPANSION: usize = 31;
const ZERO: Ce = Ce { primary: 0, secondary: 0, tertiary: 0 };

/// A built tailoring: its settings, its mappings (context, NFD string and elements) and the code
/// points whose root contractions and contexts it turns off.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tailoring {
    /// The options its rules set.
    pub settings: Settings,
    /// The mappings, sorted: context (empty if none), string, elements.
    pub mappings: Vec<(Vec<u32>, Vec<u32>, Vec<Ce>)>,
    /// `[suppressContractions]` ranges.
    pub suppressed: Vec<(u32, u32)>,
}

#[derive(Clone, Copy, Debug)]
struct Node {
    /// A primary for a root primary node, a 16-bit weight for the others.
    weight: u32,
    strength: Level,
    tailored: bool,
    before2: bool,
    before3: bool,
    prev: usize,
    next: usize,
}

fn temp(index: usize, strength: Level) -> Ce {
    let index = u32::try_from(index).expect("fewer than 2^28 nodes");
    Ce { primary: TEMP | index, secondary: strength as u16, tertiary: 0 }
}

fn is_temp(ce: Ce) -> bool {
    ce.primary >= TEMP
}

fn temp_index(ce: Ce) -> usize {
    (ce.primary & !TEMP) as usize
}

fn level(n: u16) -> Level {
    match n {
        1 => Level::Primary,
        2 => Level::Secondary,
        3 => Level::Tertiary,
        4 => Level::Quaternary,
        _ => Level::Identical,
    }
}

/// The strength of the difference an element makes (ICU's `ceStrength`).
fn strength_of(ce: Ce) -> Level {
    if is_temp(ce) {
        level(ce.secondary)
    } else if ce.primary != 0 {
        Level::Primary
    } else if ce.secondary != 0 {
        Level::Secondary
    } else if ce.tertiary != 0 {
        Level::Tertiary
    } else {
        Level::Identical
    }
}

fn is_jamo_l(c: u32) -> bool {
    (0x1100..=0x1112).contains(&c)
}

fn is_jamo_v(c: u32) -> bool {
    (0x1161..=0x1175).contains(&c)
}

/// A tertiary weight as an index among those with only the low six bits of each byte, and back;
/// the limit `4000` is index 4096.
fn tertiary_index(t: u32) -> u32 {
    (t >> 8) * 64 + (t & 0x3F)
}

fn tertiary_from_index(i: u32) -> u32 {
    ((i / 64) << 8) | (i % 64)
}

/// `count` weights spread strictly between `lower` and `limit`.
fn spread(lower: u32, limit: u32, count: usize, what: &str) -> Result<Vec<u32>, String> {
    let count = u64::try_from(count).expect("fits");
    let gap = u64::from(limit).saturating_sub(u64::from(lower));
    if gap <= count {
        return Err(format!("{what} tailoring gap too small"));
    }
    let step = gap / (count + 1);
    let at = |i: u64| u32::try_from(u64::from(lower) + step * i).expect("below the limit");
    Ok((1..=count).map(at).collect())
}

/// Builds one tailoring.
pub struct Builder<'a> {
    root: &'a Root,
    elements: &'a RootElements,
    icu: &'a IcuCollations,
    variable_top: u32,
    han_first_primary: u32,
    nodes: Vec<Node>,
    root_primary_indexes: Vec<usize>,
    ces: Vec<Ce>,
    data: Tailored<'a, Root>,
    settings: Settings,
    suppressed: Vec<(u32, u32)>,
}

impl<'a> Builder<'a> {
    /// A builder over the root `root`, its elements, the ICU bundles for `[import]`, the root's
    /// variable top and the first primary of the Han group (ICU's `[last regular]`).
    pub fn new(
        root: &'a Root,
        elements: &'a RootElements,
        icu: &'a IcuCollations,
        variable_top: u32,
        han_first_primary: u32,
    ) -> Builder<'a> {
        Builder {
            root,
            elements,
            icu,
            variable_top,
            han_first_primary,
            nodes: Vec::new(),
            root_primary_indexes: Vec::new(),
            ces: Vec::new(),
            data: Tailored::new(root),
            settings: Settings::default(),
            suppressed: Vec::new(),
        }
    }

    /// Builds the tailoring of `rules`.
    pub fn build(mut self, rules: &str) -> Result<Tailoring, String> {
        self.parse(rules, 0)?;
        let finals = self.make_tailored_ces()?;
        self.data.map_own(|ce| {
            if is_temp(ce) {
                let f = finals[temp_index(ce)];
                Ce { tertiary: f.tertiary | (ce.tertiary & CASE_MASK), ..f }
            } else {
                ce
            }
        });
        let mut mappings: Vec<(Vec<u32>, Vec<u32>, Vec<Ce>)> =
            self.data.own().map(|(before, cps, ces)| (before, cps, ces.to_vec())).collect();
        mappings.sort();
        Ok(Tailoring { settings: self.settings, mappings, suppressed: self.suppressed })
    }

    fn parse(&mut self, text: &str, depth: usize) -> Result<(), String> {
        if depth > 8 {
            return Err("[import] nested too deeply".into());
        }
        for rule in rules::parse(text).map_err(|e| e.0)? {
            match rule {
                Rule::Setting(Setting::Import { locale, collation }) => {
                    let (_, imported) = self
                        .icu
                        .rules(&locale, &collation)
                        .ok_or_else(|| format!("[import {locale} {collation}]: no such rules"))?;
                    self.parse(imported, depth + 1)?;
                }
                Rule::Setting(s) => self.setting(s),
                Rule::Reset { before, target } => self.add_reset(before, target)?,
                Rule::Relation { level, prefix, text, extension } => {
                    self.add_relation(level, &prefix, &text, &extension)?;
                }
            }
        }
        Ok(())
    }

    fn setting(&mut self, s: Setting) {
        let o = &mut self.settings;
        match s {
            Setting::Strength(l) => o.strength = Some(strength(l)),
            Setting::AlternateShifted(b) => {
                o.alternate = Some(if b { Alternate::Shifted } else { Alternate::NonIgnorable });
            }
            Setting::MaxVariable(m) => o.max_variable = Some(max_variable(m)),
            Setting::CaseFirst(c) => o.case_first = Some(case_first(c)),
            Setting::CaseLevel(b) => o.case_level = Some(b),
            Setting::Normalization(b) => o.normalization = Some(b),
            Setting::NumericOrdering(b) => o.numeric_ordering = Some(b),
            Setting::BackwardsSecondary => o.backwards = true,
            Setting::Reorder(codes) => o.reorder = Some(codes),
            Setting::SuppressContractions(set) => {
                for (lo, hi) in set {
                    let (lo, hi) = (u32::from(lo), u32::from(hi));
                    self.data.suppress_contractions(lo, hi);
                    self.suppressed.push((lo, hi));
                }
            }
            Setting::Optimize(_) | Setting::Import { .. } => {}
        }
    }

    fn nfd(&self, s: &str) -> Vec<u32> {
        self.root.nfd(s.chars().map(u32::from))
    }

    fn set_last_ce(&mut self, ce: Ce) {
        let n = self.ces.len();
        self.ces[n - 1] = ce;
    }

    fn add_reset(&mut self, before: Option<Level>, target: Target) -> Result<(), String> {
        match target {
            Target::Anchor(a) => self.ces = vec![self.special_reset(a)?],
            Target::Text(s) => {
                let nfd = self.nfd(&s);
                self.ces = elements(&self.data, &nfd);
                if self.ces.len() > MAX_EXPANSION {
                    return Err("reset position maps to too many collation elements".into());
                }
            }
        }
        let Some(strength) = before else { return Ok(()) };
        let mut index = self.find_or_insert_node_for_ces(strength)?;
        let mut node = self.nodes[index];
        while node.strength > strength {
            index = node.prev;
            node = self.nodes[index];
        }
        if node.strength == strength && node.tailored {
            self.set_last_ce(temp(node.prev, strength));
            return Ok(());
        }
        if strength == Level::Primary {
            let p = node.weight;
            if p == 0 {
                return Err("reset primary-before ignorable not possible".into());
            }
            if p <= self.elements.first_primary() {
                return Err("reset primary-before first non-ignorable not supported".into());
            }
            // ICU also refuses the primary before `[first trailing]`, which no rule reaches here:
            // no string maps to it, and the anchor moves on to U+FFFD while nothing follows it.
            let p = self.elements.primary_before(p);
            index = self.find_or_insert_node_for_primary(p);
            while self.nodes[index].next != 0 {
                index = self.nodes[index].next;
            }
            self.set_last_ce(temp(index, strength));
            return Ok(());
        }
        index = self.find_common_node(index, Level::Secondary);
        if strength >= Level::Tertiary {
            index = self.find_common_node(index, Level::Tertiary);
        }
        node = self.nodes[index];
        if node.strength == strength {
            if node.weight == 0 {
                return Err("reset before an ignorable weight not possible".into());
            }
            let w = u32::from(self.weight16_before(index, strength));
            let previous_index = node.prev;
            let mut i = previous_index;
            let previous_weight = loop {
                let n = self.nodes[i];
                if n.strength < strength {
                    break u32::from(COMMON_WEIGHT16);
                }
                if n.strength == strength && !n.tailored {
                    break n.weight;
                }
                i = n.prev;
            };
            if previous_weight == w {
                index = previous_index;
            } else {
                let new = weak_node(w, strength);
                index = self.insert_node_between(previous_index, index, new);
            }
        } else {
            let w = self.weight16_before(index, strength);
            index = self.find_or_insert_weak_node(index, w, strength);
        }
        let last = *self.ces.last().expect("a reset has elements");
        self.set_last_ce(temp(index, strength_of(last)));
        Ok(())
    }

    /// The weight just before the node's at `level` (ICU's `getWeight16Before`).
    fn weight16_before(&self, mut index: usize, level: Level) -> u16 {
        let common = u32::from(COMMON_WEIGHT16);
        let mut node = self.nodes[index];
        let t = if node.strength == Level::Tertiary { node.weight } else { common };
        while node.strength > Level::Secondary {
            index = node.prev;
            node = self.nodes[index];
        }
        if node.tailored {
            return BEFORE_WEIGHT16;
        }
        let s = if node.strength == Level::Secondary { node.weight } else { common };
        while node.strength > Level::Primary {
            index = node.prev;
            node = self.nodes[index];
        }
        if node.tailored {
            return BEFORE_WEIGHT16;
        }
        let (s, t) = (s as u16, t as u16);
        if level == Level::Secondary {
            self.elements.secondary_before(node.weight, s)
        } else {
            self.elements.tertiary_before(node.weight, s, t)
        }
    }

    /// The element of an anchor (ICU's `getSpecialResetPosition`).
    fn special_reset(&mut self, anchor: Anchor) -> Result<Ce, String> {
        let common = |p| Ce { primary: p, secondary: COMMON_WEIGHT16, tertiary: COMMON_WEIGHT16 };
        let element = |(p, s, t): (u32, u16, u16)| Ce { primary: p, secondary: s, tertiary: t };
        let mut strength = Level::Primary;
        let mut boundary = false;
        let first = matches!(
            anchor,
            Anchor::FirstTertiaryIgnorable
                | Anchor::FirstSecondaryIgnorable
                | Anchor::FirstPrimaryIgnorable
                | Anchor::FirstVariable
                | Anchor::FirstRegular
                | Anchor::FirstImplicit
                | Anchor::FirstTrailing
        );
        let mut ce = match anchor {
            Anchor::FirstTertiaryIgnorable | Anchor::LastTertiaryIgnorable => return Ok(ZERO),
            Anchor::FirstSecondaryIgnorable => {
                let index = self.find_or_insert_node_for_root_ce(ZERO, Level::Tertiary);
                let next = self.nodes[index].next;
                if next != 0 {
                    let n = self.nodes[next];
                    if n.tailored && n.strength == Level::Tertiary {
                        return Ok(temp(next, Level::Tertiary));
                    }
                }
                return Ok(element(self.elements.tertiary_ces().0));
            }
            Anchor::LastSecondaryIgnorable => {
                strength = Level::Tertiary;
                element(self.elements.tertiary_ces().1)
            }
            Anchor::FirstPrimaryIgnorable => {
                let mut index = self.find_or_insert_node_for_root_ce(ZERO, Level::Secondary);
                loop {
                    let next = self.nodes[index].next;
                    if next == 0 {
                        break;
                    }
                    index = next;
                    let n = self.nodes[index];
                    if n.strength < Level::Secondary {
                        break;
                    }
                    if n.strength == Level::Secondary {
                        if !n.tailored {
                            break;
                        }
                        if n.before3 {
                            index = self.nodes[n.next].next;
                        }
                        return Ok(temp(index, Level::Secondary));
                    }
                }
                strength = Level::Secondary;
                element(self.elements.secondary_ces().0)
            }
            Anchor::LastPrimaryIgnorable => {
                strength = Level::Secondary;
                element(self.elements.secondary_ces().1)
            }
            Anchor::FirstVariable => {
                boundary = true;
                common(self.elements.first_primary())
            }
            Anchor::LastVariable => {
                element(self.elements.last_ce_with_primary_before(self.variable_top + 1))
            }
            Anchor::FirstRegular => {
                boundary = true;
                element(self.elements.first_ce_with_primary_at_least(self.variable_top + 1))
            }
            Anchor::LastRegular => {
                element(self.elements.first_ce_with_primary_at_least(self.han_first_primary))
            }
            Anchor::FirstImplicit => elements(self.root, &[0x4E00])[0],
            Anchor::LastImplicit => return Err("reset to [last implicit] not supported".into()),
            Anchor::FirstTrailing => {
                boundary = true;
                common(FIRST_TRAILING_PRIMARY)
            }
            Anchor::LastTrailing => return Err("LDML forbids tailoring to U+FFFF".into()),
        };
        let mut index = self.find_or_insert_node_for_root_ce(ce, strength);
        let mut node = self.nodes[index];
        if first {
            if !(node.before2 || node.before3) && boundary {
                if node.next != 0 {
                    index = node.next;
                    ce = temp(index, strength);
                } else {
                    ce = common(self.elements.primary_after(ce.primary));
                    index = self.find_or_insert_node_for_root_ce(ce, Level::Primary);
                }
                node = self.nodes[index];
            }
            if node.before2 || node.before3 {
                if node.before2 {
                    index = self.nodes[node.next].next;
                    node = self.nodes[index];
                }
                if node.before3 {
                    index = self.nodes[node.next].next;
                }
                ce = temp(index, strength);
            }
        } else {
            while node.next != 0 && self.nodes[node.next].strength >= strength {
                index = node.next;
                node = self.nodes[index];
            }
            if node.tailored {
                ce = temp(index, strength);
            }
        }
        Ok(ce)
    }

    fn add_relation(
        &mut self,
        strength: Level,
        prefix: &str,
        text: &str,
        extension: &str,
    ) -> Result<(), String> {
        let nfd = self.nfd(text);
        if nfd.len() >= 2 {
            if is_jamo_l(nfd[0]) || is_jamo_v(nfd[0]) {
                return Err(
                    "contractions starting with conjoining Jamo L or V not supported".into()
                );
            }
            let (last, before_last) = (nfd[nfd.len() - 1], nfd[nfd.len() - 2]);
            if is_jamo_l(last) || (is_jamo_v(last) && is_jamo_l(before_last)) {
                return Err(
                    "contractions ending with conjoining Jamo L or L+V not supported".into()
                );
            }
        }
        if strength != Level::Identical {
            let index = self.find_or_insert_node_for_ces(strength)?;
            let ce = *self.ces.last().expect("at least one element");
            if strength == Level::Primary && !is_temp(ce) && ce.primary == 0 {
                return Err("tailoring primary after ignorables not supported".into());
            }
            if strength == Level::Quaternary && ce == ZERO {
                return Err("tailoring quaternary after tertiary ignorables not supported".into());
            }
            let index = self.insert_tailored_node_after(index, strength);
            self.set_last_ce(temp(index, strength_of(ce).min(strength)));
        }
        self.set_case_bits(&nfd);
        let before_extension = self.ces.len();
        if !extension.is_empty() {
            let more = elements(&self.data, &self.nfd(extension));
            self.ces.extend(more);
            if self.ces.len() > MAX_EXPANSION {
                return Err("extension string adds too many collation elements".into());
            }
        }
        let prefix = self.nfd(prefix);
        if prefix.is_empty() {
            self.data.insert(&nfd, self.ces.clone());
        } else {
            self.data.insert_context(&prefix, &nfd, self.ces.clone());
        }
        self.ces.truncate(before_extension);
        Ok(())
    }

    /// The case bits of the tailored elements from the root elements of the string (ICU's
    /// `setCaseBits`).
    fn set_case_bits(&mut self, nfd: &[u32]) {
        let tailored = self.ces.iter().filter(|ce| strength_of(**ce) == Level::Primary).count();
        let mut cases: u64 = 0;
        if tailored > 0 {
            let mut last_case = 0;
            let mut base = 0;
            for ce in elements(self.root, nfd).into_iter().filter(|ce| ce.primary != 0) {
                base += 1;
                let c = u64::from(ce.tertiary >> 14);
                if base < tailored {
                    cases |= c << ((base - 1) * 2);
                } else if base == tailored {
                    last_case = c;
                } else if c != last_case {
                    last_case = 1;
                    break;
                }
            }
            if base >= tailored {
                cases |= last_case << ((tailored - 1) * 2);
            }
        }
        for ce in &mut self.ces {
            let s = strength_of(*ce);
            ce.tertiary &= !CASE_MASK;
            if s == Level::Primary {
                ce.tertiary |= u16::try_from(cases & 3).expect("two bits") << 14;
                cases >>= 2;
            } else if s == Level::Tertiary {
                ce.tertiary |= 0x8000;
            }
        }
    }

    fn find_or_insert_node_for_ces(&mut self, strength: Level) -> Result<usize, String> {
        let ce = loop {
            match self.ces.last() {
                None => {
                    self.ces.push(ZERO);
                    break ZERO;
                }
                Some(ce) if strength_of(*ce) <= strength => break *ce,
                Some(_) => {
                    self.ces.pop();
                }
            }
        };
        if is_temp(ce) {
            return Ok(temp_index(ce));
        }
        if (0xE5..=0xE6).contains(&(ce.primary >> 24)) {
            return Err("tailoring relative to an unassigned code point not supported".into());
        }
        Ok(self.find_or_insert_node_for_root_ce(ce, strength))
    }

    fn find_or_insert_node_for_root_ce(&mut self, ce: Ce, strength: Level) -> usize {
        let mut index = self.find_or_insert_node_for_primary(ce.primary);
        if strength >= Level::Secondary {
            index = self.find_or_insert_weak_node(index, ce.secondary, Level::Secondary);
            if strength >= Level::Tertiary {
                let t = ce.tertiary & ONLY_TERTIARY;
                index = self.find_or_insert_weak_node(index, t, Level::Tertiary);
            }
        }
        index
    }

    fn find_or_insert_node_for_primary(&mut self, p: u32) -> usize {
        let nodes = &self.nodes;
        match self.root_primary_indexes.binary_search_by_key(&p, |i| nodes[*i].weight) {
            Ok(i) => self.root_primary_indexes[i],
            Err(i) => {
                let index = self.nodes.len();
                self.nodes.push(Node { weight: p, ..weak_node(0, Level::Primary) });
                self.root_primary_indexes.insert(i, index);
                index
            }
        }
    }

    fn find_or_insert_weak_node(&mut self, mut index: usize, weight16: u16, level: Level) -> usize {
        if weight16 == COMMON_WEIGHT16 {
            return self.find_common_node(index, level);
        }
        let mut node = self.nodes[index];
        let has_before = if level == Level::Secondary { node.before2 } else { node.before3 };
        if weight16 != 0 && weight16 < COMMON_WEIGHT16 && !has_before {
            let mut common = weak_node(u32::from(COMMON_WEIGHT16), level);
            if level == Level::Secondary {
                common.before3 = node.before3;
                self.nodes[index].before3 = false;
                self.nodes[index].before2 = true;
            } else {
                self.nodes[index].before3 = true;
            }
            let next = node.next;
            let below =
                self.insert_node_between(index, next, weak_node(u32::from(weight16), level));
            self.insert_node_between(below, next, common);
            return below;
        }
        let mut next;
        loop {
            next = node.next;
            if next == 0 {
                break;
            }
            node = self.nodes[next];
            if node.strength < level {
                break;
            }
            if node.strength == level && !node.tailored {
                if node.weight == u32::from(weight16) {
                    return next;
                }
                if node.weight > u32::from(weight16) {
                    break;
                }
            }
            index = next;
        }
        self.insert_node_between(index, next, weak_node(u32::from(weight16), level))
    }

    fn insert_tailored_node_after(&mut self, mut index: usize, strength: Level) -> usize {
        if strength >= Level::Secondary {
            index = self.find_common_node(index, Level::Secondary);
            if strength >= Level::Tertiary {
                index = self.find_common_node(index, Level::Tertiary);
            }
        }
        let mut next = self.nodes[index].next;
        while next != 0 && self.nodes[next].strength > strength {
            index = next;
            next = self.nodes[next].next;
        }
        let new = Node { tailored: true, ..weak_node(0, strength) };
        self.insert_node_between(index, next, new)
    }

    fn insert_node_between(&mut self, index: usize, next: usize, node: Node) -> usize {
        let new = self.nodes.len();
        self.nodes.push(Node { prev: index, next, ..node });
        self.nodes[index].next = new;
        if next != 0 {
            self.nodes[next].prev = new;
        }
        new
    }

    fn find_common_node(&self, index: usize, strength: Level) -> usize {
        let node = self.nodes[index];
        let has_before = if strength == Level::Secondary { node.before2 } else { node.before3 };
        if node.strength >= strength || !has_before {
            return index;
        }
        // Skip the below-common node, then up to the explicit common one.
        let mut i = self.nodes[node.next].next;
        loop {
            let n = self.nodes[i];
            if !n.tailored && n.strength == strength && n.weight >= u32::from(COMMON_WEIGHT16) {
                return i;
            }
            i = n.next;
        }
    }

    fn count_tailored(&self, mut i: usize, strength: Level) -> usize {
        let mut count = 0;
        while i != 0 {
            let node = self.nodes[i];
            if node.strength < strength || (node.strength == strength && !node.tailored) {
                break;
            }
            if node.strength == strength {
                count += 1;
            }
            i = node.next;
        }
        count
    }

    /// The final element of every tailored node (ICU's `makeTailoredCEs`).
    fn make_tailored_ces(&self) -> Result<Vec<Ce>, String> {
        let mut finals = vec![ZERO; self.nodes.len()];
        for &root_index in &self.root_primary_indexes {
            let mut w = Weights::new(self.nodes[root_index].weight);
            let mut next = self.nodes[root_index].next;
            while next != 0 {
                let i = next;
                let node = self.nodes[i];
                next = node.next;
                w.step(self, node, next)?;
                if node.tailored {
                    finals[i] = w.ce();
                }
            }
        }
        Ok(finals)
    }
}

fn strength(l: Level) -> Strength {
    match l {
        Level::Primary => Strength::Primary,
        Level::Secondary => Strength::Secondary,
        Level::Tertiary => Strength::Tertiary,
        Level::Quaternary => Strength::Quaternary,
        Level::Identical => Strength::Identical,
    }
}

fn max_variable(m: MaxVariable) -> CollationMaxVariable {
    match m {
        MaxVariable::Space => CollationMaxVariable::Space,
        MaxVariable::Punct => CollationMaxVariable::Punct,
        MaxVariable::Symbol => CollationMaxVariable::Symbol,
        MaxVariable::Currency => CollationMaxVariable::Currency,
    }
}

fn case_first(c: CaseFirst) -> CollationCaseFirst {
    match c {
        CaseFirst::Off => CollationCaseFirst::Off,
        CaseFirst::Lower => CollationCaseFirst::Lower,
        CaseFirst::Upper => CollationCaseFirst::Upper,
    }
}

fn weak_node(weight: u32, strength: Level) -> Node {
    Node { weight, strength, tailored: false, before2: false, before3: false, prev: 0, next: 0 }
}

/// The weights while walking one root primary's list in `make_tailored_ces`.
struct Weights {
    p: u32,
    s: u32,
    t: u32,
    q: u16,
    p_tailored: bool,
    s_tailored: bool,
    t_tailored: bool,
    ps: std::vec::IntoIter<u32>,
    ss: std::vec::IntoIter<u32>,
    ts: std::vec::IntoIter<u32>,
}

impl Weights {
    fn new(p: u32) -> Weights {
        let s = if p == 0 { 0 } else { u32::from(COMMON_WEIGHT16) };
        Weights {
            p,
            s,
            t: s,
            q: 0,
            p_tailored: false,
            s_tailored: false,
            t_tailored: false,
            ps: Vec::new().into_iter(),
            ss: Vec::new().into_iter(),
            ts: Vec::new().into_iter(),
        }
    }

    fn ce(&self) -> Ce {
        let tertiary = u16::try_from(self.t).expect("16 bits") | (self.q << 6);
        Ce { primary: self.p, secondary: u16::try_from(self.s).expect("16 bits"), tertiary }
    }

    fn step(&mut self, b: &Builder<'_>, node: Node, next: usize) -> Result<(), String> {
        let common = u32::from(COMMON_WEIGHT16);
        match node.strength {
            Level::Quaternary => {
                if self.q == 3 {
                    return Err("quaternary tailoring gap too small".into());
                }
                self.q += 1;
                return Ok(());
            }
            Level::Tertiary if node.tailored => {
                if !self.t_tailored {
                    let count = b.count_tailored(next, Level::Tertiary) + 1;
                    let limit = if self.t == 0 {
                        self.t = u32::from(TERTIARY_BOUNDARY) - 0x100;
                        u32::from(b.elements.tertiary_ces().0.2)
                    } else if !self.p_tailored && !self.s_tailored {
                        b.elements.tertiary_after(self.p, self.s as u16, self.t as u16)
                    } else if self.t == u32::from(BEFORE_WEIGHT16) {
                        common
                    } else {
                        u32::from(TERTIARY_BOUNDARY)
                    };
                    let w =
                        spread(tertiary_index(self.t), tertiary_index(limit), count, "tertiary")?;
                    self.ts =
                        w.into_iter().map(tertiary_from_index).collect::<Vec<_>>().into_iter();
                    self.t_tailored = true;
                }
                self.t = self.ts.next().expect("counted");
            }
            Level::Tertiary => {
                self.t = node.weight;
                self.t_tailored = false;
            }
            Level::Secondary => {
                if node.tailored {
                    if !self.s_tailored {
                        let count = b.count_tailored(next, Level::Secondary) + 1;
                        let limit = if self.s == 0 {
                            self.s = u32::from(SECONDARY_BOUNDARY) - 0x100;
                            u32::from(b.elements.secondary_ces().0.1)
                        } else if !self.p_tailored {
                            b.elements.secondary_after(self.p, self.s as u16)
                        } else if self.s == u32::from(BEFORE_WEIGHT16) {
                            common
                        } else {
                            u32::from(SECONDARY_BOUNDARY)
                        };
                        if self.s == common {
                            self.s = u32::from(LAST_COMMON_SECONDARY);
                        }
                        self.ss = spread(self.s, limit, count, "secondary")?.into_iter();
                        self.s_tailored = true;
                    }
                    self.s = self.ss.next().expect("counted");
                } else {
                    self.s = node.weight;
                    self.s_tailored = false;
                }
                self.after_stronger();
            }
            _ => {
                if !self.p_tailored {
                    let count = b.count_tailored(next, Level::Primary) + 1;
                    let limit = b.elements.primary_after(self.p);
                    self.ps = spread(self.p, limit, count, "primary")?.into_iter();
                    self.p_tailored = true;
                }
                self.p = self.ps.next().expect("counted");
                self.s = common;
                self.s_tailored = false;
                self.after_stronger();
            }
        }
        self.q = 0;
        Ok(())
    }

    /// After a primary or secondary step the tertiary restarts at common (zero if ignorable).
    fn after_stronger(&mut self) {
        self.t = if self.s == 0 { 0 } else { u32::from(COMMON_WEIGHT16) };
        self.t_tailored = false;
    }
}
