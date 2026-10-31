//! A [`Node`] tree compiled to the instructions of the backtracking machine ([`super::vm`]).

use std::collections::HashMap;

use super::ast::{Assertion, Case, Class, Condition, Node, RepeatKind, Verb};

/// Longest lookbehind, in characters.
const MAX_LOOKBEHIND: usize = 65535;
/// Longest branch of variable length in a lookbehind (PCRE2 10.43's default
/// `max_varlookbehind`).
const MAX_VARIABLE_LOOKBEHIND: usize = 255;

/// An instruction.
#[derive(Clone, Debug)]
pub(super) enum Inst {
    /// One character.
    Char(char),
    /// One of these characters (the case variants of a caseless character).
    CharSet(Vec<char>),
    /// Any character.
    Any,
    /// Any character but a newline.
    AnyNoNewline,
    /// A character of the class.
    Class(Box<Class>),
    /// A zero-width assertion.
    Assert(Assertion),
    /// Start of an alternation, recorded for `(*THEN)`.
    AltStart(usize),
    /// Tries the next instruction, then `next` on backtracking: an alternative of `alt`.
    Split {
        /// The next alternative.
        next: usize,
        /// The alternation.
        alt: usize,
    },
    /// Goes to the target.
    Jmp(usize),
    /// Records the position in a slot.
    Save(usize),
    /// End of a capture group: its value becomes the text from the start kept in `open`.
    Close {
        /// The group.
        group: usize,
        /// The hidden slot with its start.
        open: usize,
    },
    /// The text of the first set group among these.
    Backref {
        /// Groups.
        groups: Vec<usize>,
        /// Case-insensitive.
        caseless: Case,
    },
    /// Start of an atomic group.
    AtomicStart,
    /// End of an atomic group: its choices are dropped.
    AtomicEnd,
    /// A lookaround whose body runs from `body` to its [`Inst::LookEnd`].
    Look(Look),
    /// End of a lookaround body.
    LookEnd,
    /// Start of a counted repetition.
    CounterStart(usize),
    /// Head of a counted repetition: enters the body or leaves to `exit`.
    CounterLoop {
        /// Counter.
        slot: usize,
        /// Minimum.
        min: u32,
        /// Maximum.
        max: Option<u32>,
        /// Greedy (else lazy).
        greedy: bool,
        /// After the repetition.
        exit: usize,
    },
    /// End of an iteration: counts it and goes back to `head`, or leaves to `exit` after an
    /// empty iteration.
    CounterNext {
        /// Counter.
        slot: usize,
        /// The [`Inst::CounterLoop`].
        head: usize,
        /// Minimum.
        min: u32,
        /// After the repetition.
        exit: usize,
    },
    /// Continues if one of the groups is set, else goes to `no`.
    CondGroup {
        /// Groups.
        groups: Vec<usize>,
        /// The `no` branch.
        no: usize,
    },
    /// Continues if the lookaround holds, else goes to `no`.
    CondLook {
        /// The assertion.
        look: Look,
        /// The `no` branch.
        no: usize,
    },
    /// Continues if inside a call (to one of `groups` when given), else goes to `no`.
    CondCall {
        /// Groups.
        groups: Option<Vec<usize>>,
        /// The `no` branch.
        no: usize,
    },
    /// Continues if `holds`, else goes to `no` (`(?(VERSION...)`).
    CondFixed {
        /// Whether the condition holds.
        holds: bool,
        /// The `no` branch.
        no: usize,
    },
    /// Calls a group as a subroutine.
    Call {
        /// The group, 0 for the whole pattern.
        group: usize,
        /// Its first instruction.
        target: usize,
    },
    /// End of a capture group: returns if the latest call is to it.
    Ret(usize),
    /// `(*ACCEPT)`: closes the open groups (with the hidden slots of their starts) and ends the
    /// match, the call, or the non-atomic assertion it is in.
    Accept {
        /// Open capture groups and the hidden slots of their starts.
        open: Vec<(usize, usize)>,
        /// In a non-atomic assertion: its alternation, the slot of its position and the
        /// instruction after it.
        napla: Option<(usize, usize, usize)>,
    },
    /// A verb that acts when backtracked onto.
    Verb(VerbKind),
    /// `(*MARK:name)`.
    Mark(String),
    /// End of a script run whose start is in the slot.
    ScriptRunEnd(usize),
    /// End of a non-atomic lookahead: back to the position in the slot.
    LookRestore(usize),
    /// Start of a non-atomic lookbehind: saves the position in `slot` and tries each start.
    BehindStart {
        /// Slot for the position.
        slot: usize,
        /// Shortest length of the body.
        min: usize,
        /// Longest length of the body.
        max: usize,
    },
    /// End of a non-atomic lookbehind: the body must end at the position in the slot.
    BehindEnd(usize),
    /// `\K`.
    KeepOut,
    /// Fails.
    Fail,
    /// An extended grapheme cluster.
    Grapheme,
    /// A newline sequence.
    NewlineSeq,
    /// A match.
    Match,
}

/// What the verbs at the start of a pattern set for the whole match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Options {
    /// `(*NOTEMPTY)` or `(*NOTEMPTY_ATSTART)`.
    pub not_empty: Option<NotEmpty>,
    /// The newline convention (`(*CR)`, `(*CRLF)`...).
    pub newline: Newline,
    /// What `\R` matches.
    pub bsr: Bsr,
    /// Start-of-match optimizations, off with `(*NO_START_OPT)`.
    pub start_opt: bool,
}

/// Newline conventions: what `.`, `\N`, `^`, `$` and `\Z` take as a newline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Newline {
    /// `\n`, the default.
    Lf,
    /// `\r`.
    Cr,
    /// `\r\n`.
    Crlf,
    /// `\r`, `\n` or `\r\n`.
    AnyCrlf,
    /// Any Unicode newline sequence.
    Any,
    /// The NUL character.
    Nul,
}

/// What `\R` matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Bsr {
    /// Any Unicode newline sequence.
    Unicode,
    /// `\r`, `\n` or `\r\n`.
    AnyCrlf,
}

/// Which empty matches `(*NOTEMPTY)` and `(*NOTEMPTY_ATSTART)` refuse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NotEmpty {
    /// Any empty match.
    Anywhere,
    /// An empty match at the offset where the search starts.
    AtStart,
}

/// Verbs that act when backtracked onto.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum VerbKind {
    /// `(*COMMIT)`.
    Commit,
    /// `(*PRUNE)`.
    Prune,
    /// `(*SKIP)`.
    Skip,
    /// `(*SKIP:name)`.
    SkipTo(String),
    /// `(*THEN)`, with its innermost alternation.
    Then(Option<usize>),
}

/// A lookaround.
#[derive(Clone, Copy, Debug)]
pub(super) struct Look {
    /// Lookbehind.
    pub behind: bool,
    /// Negative.
    pub negate: bool,
    /// First instruction of the body.
    pub body: usize,
    /// Instruction after the body.
    pub next: usize,
    /// Shortest length of the body, in characters (lookbehind).
    pub min: usize,
    /// Longest length of the body, in characters (lookbehind).
    pub max: usize,
}

/// A compiled program.
#[derive(Debug)]
pub(super) struct Program {
    /// Instructions; the match starts at 0.
    pub insts: Vec<Inst>,
    /// Counters of repetitions.
    pub counters: usize,
    /// Slots: two per capture group, the whole match first, then the hidden ones.
    pub slots: usize,
    /// What the verbs at the start of the pattern set.
    pub options: Options,
}

/// Compiles `node`, which has `groups` capture groups.
pub(super) fn compile(
    node: &Node,
    groups: usize,
    options: Options,
    reset_groups: &[usize],
) -> Result<Program, String> {
    let mut bodies = HashMap::new();
    bodies.insert(0, node);
    group_bodies(node, &mut bodies);
    // A back reference in a lookbehind needs a group of fixed length with a number of its own.
    let fixed_backrefs: Vec<usize> = bodies
        .iter()
        .filter(|(g, body)| !reset_groups.contains(g) && !refers(body, **g))
        .map(|(g, _)| *g)
        .collect();
    let mut c = Compiler {
        insts: Vec::new(),
        counters: 0,
        slots: 2 * (groups + 1),
        open: Vec::new(),
        open_slots: HashMap::new(),
        alt: None,
        alts: 0,
        group_pc: HashMap::new(),
        calls: Vec::new(),
        accepts: Vec::new(),
        has_calls: has_calls(node),
        bodies,
        fixed_backrefs,
    };
    c.emit(node)?;
    c.insts.push(Inst::Match);
    for at in std::mem::take(&mut c.calls) {
        let Inst::Call { group, .. } = c.insts[at] else { continue };
        let target = if group == 0 { Some(0) } else { c.group_pc.get(&group).copied() };
        let target = target.ok_or("reference to non-existent subpattern")?;
        c.insts[at] = Inst::Call { group, target };
    }
    Ok(Program { insts: c.insts, counters: c.counters, slots: c.slots, options })
}

/// The first group of each number, with its body.
fn group_bodies<'a>(node: &'a Node, out: &mut HashMap<usize, &'a Node>) {
    if let Node::Group(n, Some(i)) = node {
        out.entry(*i).or_insert(n);
    }
    children(node, &mut |n| group_bodies(n, out));
}

/// Whether `needle` is `haystack` or inside it.
fn contains(haystack: &Node, needle: &Node) -> bool {
    let mut found = std::ptr::eq(haystack, needle);
    children(haystack, &mut |n| found |= contains(n, needle));
    found
}

/// Whether `node` calls, directly or through other calls, a group whose body contains `target`.
fn calls_group_containing(node: &Node, bodies: &Bodies<'_>, target: &Node) -> bool {
    let mut seen = Vec::new();
    let mut pending = vec![node];
    while let Some(n) = pending.pop() {
        let mut called = Vec::new();
        collect_calls(n, &mut called);
        for g in called {
            if seen.contains(&g) {
                continue;
            }
            seen.push(g);
            if let Some(body) = bodies.get(&g) {
                if contains(body, target) {
                    return true;
                }
                pending.push(body);
            }
        }
    }
    false
}

/// The groups `node` calls.
fn collect_calls(node: &Node, out: &mut Vec<usize>) {
    if let Node::Call(g) = node {
        out.push(*g);
    }
    children(node, &mut |n| collect_calls(n, out));
}

/// Whether `node`, the body of `group`, holds a call or a back reference to `group` or a later
/// group, whose lengths are not known when it is measured.
fn refers(node: &Node, group: usize) -> bool {
    let mut found = match node {
        Node::Call(_) => true,
        Node::Backref { groups, .. } => groups.iter().any(|g| *g >= group),
        _ => false,
    };
    children(node, &mut |n| found |= refers(n, group));
    found
}

/// Whether the pattern calls a group.
fn has_calls(node: &Node) -> bool {
    let mut found = matches!(node, Node::Call(_));
    children(node, &mut |n| found |= has_calls(n));
    found
}

/// Calls `f` on each child of `node`.
fn children<'a>(node: &'a Node, f: &mut impl FnMut(&'a Node)) {
    match node {
        Node::Group(n, _)
        | Node::Atomic(n)
        | Node::ScriptRun(n)
        | Node::Repeat { node: n, .. }
        | Node::Look { node: n, .. } => f(n),
        Node::Concat(items) | Node::Alt(items) => items.iter().for_each(f),
        Node::Cond { cond, yes, no } => {
            if let Condition::Look { node, .. } = cond {
                f(node);
            }
            f(yes);
            f(no);
        }
        _ => {}
    }
}

struct Compiler<'a> {
    insts: Vec<Inst>,
    counters: usize,
    slots: usize,
    /// Capture groups open at this point, with the hidden slots of their starts.
    open: Vec<(usize, usize)>,
    /// The hidden slot of each group's start.
    open_slots: HashMap<usize, usize>,
    /// The innermost alternation.
    alt: Option<usize>,
    alts: usize,
    /// First instruction of each group.
    group_pc: HashMap<usize, usize>,
    /// Instructions that call a group, patched at the end.
    calls: Vec<usize>,
    /// `(*ACCEPT)` instructions not yet tied to a non-atomic assertion.
    accepts: Vec<usize>,
    has_calls: bool,
    bodies: HashMap<usize, &'a Node>,
    /// Groups whose back references may stand in a lookbehind.
    fixed_backrefs: Vec<usize>,
}

impl<'a> Compiler<'a> {
    fn pc(&self) -> usize {
        self.insts.len()
    }

    fn emit(&mut self, node: &Node) -> Result<(), String> {
        match node {
            Node::Empty => {}
            Node::Char { c, caseless } => {
                let variants = caseless.variants(*c);
                self.insts.push(if variants.len() == 1 {
                    Inst::Char(*c)
                } else {
                    Inst::CharSet(variants)
                });
            }
            Node::Dot { dotall } => {
                self.insts.push(if *dotall { Inst::Any } else { Inst::AnyNoNewline })
            }
            Node::Class(class) => self.insts.push(Inst::Class(Box::new(class.clone()))),
            Node::Assert(a) => self.insts.push(Inst::Assert(*a)),
            Node::Group(n, None) => self.emit(n)?,
            Node::Group(n, Some(i)) => {
                // The value changes when the group closes: a back reference inside it still
                // sees the previous iteration's text.
                let open = match self.open_slots.get(i) {
                    Some(&slot) => slot,
                    None => {
                        let slot = self.hidden_slot();
                        self.open_slots.insert(*i, slot);
                        slot
                    }
                };
                self.group_pc.entry(*i).or_insert(self.insts.len());
                self.insts.push(Inst::Save(open));
                self.open.push((*i, open));
                self.emit(n)?;
                self.open.pop();
                self.insts.push(Inst::Close { group: *i, open });
                if self.has_calls {
                    self.insts.push(Inst::Ret(*i));
                }
            }
            Node::Concat(items) => {
                for n in items {
                    self.emit(n)?;
                }
            }
            Node::Alt(alts) => self.alternation(alts)?,
            Node::Repeat { node, min, max, kind } => self.repeat(node, *min, *max, *kind)?,
            Node::Backref { groups, caseless } => {
                self.insts.push(Inst::Backref { groups: groups.clone(), caseless: *caseless });
            }
            Node::NamedBackref { .. } => return Err("unresolved reference".into()),
            Node::Look { behind, negate, atomic: true, node } => {
                let look = self.look(*behind, *negate, node)?;
                self.insts[look.body - 1] = Inst::Look(look);
            }
            Node::Look { behind, atomic: false, node, .. } => {
                self.non_atomic_look(*behind, node)?
            }
            Node::Call(group) => {
                self.calls.push(self.pc());
                self.insts.push(Inst::Call { group: *group, target: 0 });
            }
            Node::NamedCall(_) => return Err("unresolved reference".into()),
            Node::Verb(verb) => self.insts.push(match verb {
                Verb::Accept => {
                    self.accepts.push(self.pc());
                    Inst::Accept { open: self.open.clone(), napla: None }
                }
                Verb::Commit => Inst::Verb(VerbKind::Commit),
                Verb::Prune => Inst::Verb(VerbKind::Prune),
                Verb::Skip => Inst::Verb(VerbKind::Skip),
                Verb::SkipTo(name) => Inst::Verb(VerbKind::SkipTo(name.clone())),
                Verb::Then => Inst::Verb(VerbKind::Then(self.alt)),
                Verb::Mark(name) => Inst::Mark(name.clone()),
            }),
            Node::ScriptRun(n) => {
                let slot = self.hidden_slot();
                self.insts.push(Inst::Save(slot));
                self.emit(n)?;
                self.insts.push(Inst::ScriptRunEnd(slot));
            }
            Node::Atomic(n) => {
                self.insts.push(Inst::AtomicStart);
                self.emit(n)?;
                self.insts.push(Inst::AtomicEnd);
            }
            Node::Cond { cond, yes, no } => self.conditional(cond, yes, no)?,
            Node::KeepOut => self.insts.push(Inst::KeepOut),
            Node::Fail => self.insts.push(Inst::Fail),
            Node::Grapheme => self.insts.push(Inst::Grapheme),
            Node::NewlineSeq => self.insts.push(Inst::NewlineSeq),
        }
        Ok(())
    }

    fn hidden_slot(&mut self) -> usize {
        self.slots += 1;
        self.slots - 1
    }

    /// A new alternation scope for `(*THEN)`, recorded at this point.
    fn new_alt(&mut self) -> usize {
        self.alts += 1;
        self.insts.push(Inst::AltStart(self.alts));
        self.alts
    }

    fn alternation(&mut self, alts: &[Node]) -> Result<(), String> {
        if alts.len() == 1 {
            return self.emit(&alts[0]);
        }
        let id = self.new_alt();
        let outer = self.alt.replace(id);
        let mut jumps = Vec::new();
        for (i, alt) in alts.iter().enumerate() {
            if i + 1 < alts.len() {
                let split = self.pc();
                self.insts.push(Inst::Split { next: 0, alt: id });
                self.emit(alt)?;
                jumps.push(self.pc());
                self.insts.push(Inst::Jmp(0));
                let next = self.pc();
                self.insts[split] = Inst::Split { next, alt: id };
            } else {
                self.emit(alt)?;
            }
        }
        self.alt = outer;
        let end = self.pc();
        for j in jumps {
            self.insts[j] = Inst::Jmp(end);
        }
        Ok(())
    }

    /// A non-atomic positive lookaround, matched inline so that it can be backtracked into.
    fn non_atomic_look(&mut self, behind: bool, node: &Node) -> Result<(), String> {
        let slot = self.hidden_slot();
        if behind {
            let (min, max) = self.behind_lengths(node)?;
            self.insts.push(Inst::BehindStart { slot, min, max });
        } else {
            self.insts.push(Inst::Save(slot));
        }
        let id = self.new_alt();
        let outer = self.alt.replace(id);
        let accepts = self.accepts.len();
        self.emit(node)?;
        self.alt = outer;
        self.insts.push(if behind { Inst::BehindEnd(slot) } else { Inst::LookRestore(slot) });
        // `(*ACCEPT)` ends the assertion, which then holds atomically.
        let end = self.pc();
        for at in self.accepts.split_off(accepts) {
            if let Inst::Accept { napla, .. } = &mut self.insts[at] {
                *napla = Some((id, slot, end));
            }
        }
        Ok(())
    }

    fn lengths_context(&self) -> LengthContext<'a, '_> {
        LengthContext { bodies: &self.bodies, fixed_backrefs: &self.fixed_backrefs }
    }

    /// The shortest and longest length of a lookbehind's body, which must be limited.
    fn behind_lengths(&self, node: &Node) -> Result<(usize, usize), String> {
        let calls_itself = self
            .bodies
            .values()
            .any(|body| contains(body, node) && calls_group_containing(node, &self.bodies, node));
        if calls_itself {
            return Err("recursive subroutine call in lookbehind assertion".into());
        }
        let branches = match node {
            Node::Alt(alts) => alts.iter().collect(),
            n => vec![n],
        };
        for branch in branches {
            let (min, max) = lengths(branch, &self.lengths_context(), &mut Vec::new())?;
            if max.is_some_and(|m| m != min && m > MAX_VARIABLE_LOOKBEHIND) {
                return Err("branch too long in variable-length lookbehind assertion".into());
            }
        }
        let (min, max) = lengths(node, &self.lengths_context(), &mut Vec::new())?;
        match max {
            Some(m) if m <= MAX_LOOKBEHIND => Ok((min, m)),
            _ => Err("length of lookbehind assertion is not limited".into()),
        }
    }

    fn repeat(
        &mut self,
        node: &Node,
        min: u32,
        max: Option<u32>,
        kind: RepeatKind,
    ) -> Result<(), String> {
        if kind == RepeatKind::Possessive {
            self.insts.push(Inst::AtomicStart);
        }
        let slot = self.counters;
        self.counters += 1;
        self.insts.push(Inst::CounterStart(slot));
        let head = self.pc();
        self.insts.push(Inst::Fail);
        self.emit(node)?;
        let next = self.pc();
        self.insts.push(Inst::Fail);
        let exit = self.pc();
        self.insts[head] =
            Inst::CounterLoop { slot, min, max, greedy: kind != RepeatKind::Lazy, exit };
        self.insts[next] = Inst::CounterNext { slot, head, min, exit };
        if kind == RepeatKind::Possessive {
            self.insts.push(Inst::AtomicEnd);
        }
        Ok(())
    }

    /// Emits a placeholder and a lookaround body; returns the lookaround, whose instruction goes
    /// in the placeholder at `body - 1`.
    fn look(&mut self, behind: bool, negate: bool, node: &Node) -> Result<Look, String> {
        let (min, max) = if behind { self.behind_lengths(node)? } else { (0, 0) };
        self.insts.push(Inst::Fail);
        let body = self.pc();
        // `(*THEN)` and `(*ACCEPT)` do not reach beyond the assertion.
        let outer = (self.alt.take(), std::mem::take(&mut self.open));
        self.emit(node)?;
        (self.alt, self.open) = outer;
        self.insts.push(Inst::LookEnd);
        Ok(Look { behind, negate, body, next: self.pc(), min, max })
    }

    fn conditional(&mut self, cond: &Condition, yes: &Node, no: &Node) -> Result<(), String> {
        let at = match cond {
            Condition::Group(groups) => {
                let at = self.pc();
                self.insts.push(Inst::CondGroup { groups: groups.clone(), no: 0 });
                at
            }
            Condition::Look { behind, negate, node } => {
                let look = self.look(*behind, *negate, node)?;
                let at = look.body - 1;
                self.insts[at] = Inst::CondLook { look, no: 0 };
                at
            }
            Condition::Define => {
                let at = self.pc();
                self.insts.push(Inst::CondGroup { groups: Vec::new(), no: 0 });
                at
            }
            Condition::Recursion(groups) => {
                let at = self.pc();
                self.insts.push(Inst::CondCall { groups: groups.clone(), no: 0 });
                at
            }
            Condition::Fixed(holds) => {
                let at = self.pc();
                self.insts.push(Inst::CondFixed { holds: *holds, no: 0 });
                at
            }
            Condition::Name(_) | Condition::QuotedName(_) | Condition::RecursionName(_) => {
                return Err("unresolved reference".into());
            }
        };
        self.emit(yes)?;
        let jump = self.pc();
        self.insts.push(Inst::Jmp(0));
        let no_pc = self.pc();
        self.emit(no)?;
        let end = self.pc();
        self.insts[jump] = Inst::Jmp(end);
        match &mut self.insts[at] {
            Inst::CondGroup { no, .. }
            | Inst::CondLook { no, .. }
            | Inst::CondCall { no, .. }
            | Inst::CondFixed { no, .. } => *no = no_pc,
            _ => return Err("internal: misplaced condition".into()),
        }
        Ok(())
    }
}

type Bodies<'a> = HashMap<usize, &'a Node>;

/// What measuring a lookbehind needs: the groups' bodies and the groups whose back references
/// have a known length.
pub(super) struct LengthContext<'a, 'b> {
    bodies: &'b Bodies<'a>,
    fixed_backrefs: &'b [usize],
}

/// Shortest and longest length in characters of what `node` matches (`None`: no limit);
/// `calling` holds the groups being measured through calls, to stop at recursion.
pub(super) fn lengths(
    node: &Node,
    ctx: &LengthContext<'_, '_>,
    calling: &mut Vec<usize>,
) -> Result<(usize, Option<usize>), String> {
    Ok(match node {
        Node::Empty | Node::Assert(_) | Node::KeepOut | Node::Look { .. } | Node::Verb(_) => {
            (0, Some(0))
        }
        Node::Char { .. } | Node::Dot { .. } | Node::Class(_) => (1, Some(1)),
        Node::Fail => (0, Some(0)),
        Node::Group(n, _) | Node::Atomic(n) | Node::ScriptRun(n) => lengths(n, ctx, calling)?,
        Node::Call(group) => {
            let body = ctx.bodies.get(group).ok_or("reference to non-existent subpattern")?;
            if calling.contains(group) {
                return Ok((0, None));
            }
            calling.push(*group);
            let out = lengths(body, ctx, calling)?;
            calling.pop();
            out
        }
        Node::Concat(items) => {
            let mut total = (0usize, Some(0usize));
            // What follows `(*ACCEPT)` is never matched.
            let end = items.iter().position(|n| *n == Node::Verb(Verb::Accept));
            for n in &items[..end.unwrap_or(items.len())] {
                let (lo, hi) = lengths(n, ctx, calling)?;
                total.0 += lo;
                total.1 = total.1.zip(hi).map(|(a, b)| a + b);
            }
            total
        }
        Node::Alt(alts) => {
            let mut out: Option<(usize, Option<usize>)> = None;
            for n in alts {
                let (lo, hi) = lengths(n, ctx, calling)?;
                out = Some(match out {
                    None => (lo, hi),
                    Some((a, b)) => (a.min(lo), b.zip(hi).map(|(x, y)| x.max(y))),
                });
            }
            out.unwrap_or((0, Some(0)))
        }
        Node::Repeat { node, min, max, .. } => {
            let (lo, hi) = lengths(node, ctx, calling)?;
            let lo = lo * *min as usize;
            let hi = match (hi, max) {
                (Some(0), _) => Some(0),
                (Some(h), Some(m)) => Some(h * *m as usize),
                _ => None,
            };
            (lo, hi)
        }
        Node::Cond { cond: Condition::Define, .. } => (0, Some(0)),
        Node::Cond { yes, no, .. } => {
            let (a, b) = lengths(yes, ctx, calling)?;
            let (c, d) = lengths(no, ctx, calling)?;
            (a.min(c), b.zip(d).map(|(x, y)| x.max(y)))
        }
        Node::NewlineSeq => (1, Some(2)),
        // A back reference to groups of one fixed length has that length.
        Node::Backref { groups, .. } => {
            let mut fixed = None;
            for g in groups {
                if !ctx.fixed_backrefs.contains(g) {
                    return Ok((0, None));
                }
                let body = ctx.bodies.get(g).ok_or("reference to non-existent subpattern")?;
                if calling.contains(g) {
                    return Ok((0, None));
                }
                calling.push(*g);
                let (lo, hi) = lengths(body, ctx, calling)?;
                calling.pop();
                if hi != Some(lo) || fixed.is_some_and(|f| f != lo) {
                    return Ok((0, None));
                }
                fixed = Some(lo);
            }
            fixed.map_or((0, None), |n| (n, Some(n)))
        }
        Node::NamedBackref { .. } | Node::NamedCall(_) | Node::Grapheme => (0, None),
    })
}
