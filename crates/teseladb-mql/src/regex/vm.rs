//! The backtracking machine: an explicit stack of choices and undo records, so that neither
//! the subject's length nor the pattern's nesting can exhaust the thread's stack, with a limit
//! on steps and on the stack (PCRE2's match and heap limits). Calls to groups keep their frames
//! on the same stack, and the backtracking verbs act when the stack unwinds onto them.

use super::ast::{Assertion, Case, is_ascii_word, is_unicode_word, is_vspace};
use super::compile::{Bsr, Inst, Look, Newline, NotEmpty, Program, VerbKind};
use super::unicode::{Gcb, grapheme_break, is_script_run, pictographic};

/// Steps of one match before it fails (PCRE2's default match limit).
pub(super) const MAX_STEPS: u64 = 10_000_000;
/// Entries of the backtracking stack before the match fails.
pub(super) const MAX_STACK: usize = 2_000_000;

/// Why a match could not finish.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum MatchError {
    /// The machine ran out of steps or stack.
    Limit,
}

type Run<T> = Result<T, MatchError>;

/// What a verb does to the whole match when backtracked onto.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Abort {
    /// `(*COMMIT)`: no match.
    Commit,
    /// `(*PRUNE)`: no match at this start.
    Prune,
    /// `(*SKIP)`: no match at this start, and the next start is here.
    Skip(usize),
}

/// How a run ended.
enum RunEnd {
    Matched(Vec<Option<usize>>, usize),
    Failed,
    Aborted(Abort),
}

/// A call in progress: where to return and the state to restore.
#[derive(Clone)]
struct Frame {
    group: usize,
    ret: usize,
    slots: Vec<Option<usize>>,
    counters: Vec<u32>,
    starts: Vec<usize>,
}

enum Entry {
    Choice {
        pc: usize,
        pos: usize,
        alt: Option<usize>,
    },
    Slot {
        slot: usize,
        old: Option<usize>,
    },
    Counter {
        slot: usize,
        count: u32,
        start: usize,
    },
    Counters {
        counters: Vec<u32>,
        starts: Vec<usize>,
    },
    Barrier,
    AltStart(usize),
    Verb {
        pc: usize,
        pos: usize,
        depth: usize,
    },
    Mark,
    CallPushed {
        depth: usize,
    },
    /// A call that returned: its frame is the last of [`Thread::popped`].
    CallPopped,
}

/// The state shared by a match and its lookarounds.
pub(super) struct Machine<'a> {
    prog: &'a Program,
    text: &'a str,
    steps: u64,
    search_start: usize,
}

#[derive(Default)]
struct Thread {
    slots: Vec<Option<usize>>,
    counters: Vec<u32>,
    starts: Vec<usize>,
    stack: Vec<Entry>,
    calls: Vec<Frame>,
    /// `(*MARK)`s on the path: the instruction and the position.
    marks: Vec<(usize, usize)>,
    /// Inside a lookaround, which `(*THEN)` cannot reach beyond.
    in_look: bool,
    /// The records [`drop_choices`] keeps, while it drops the others.
    kept: Vec<Entry>,
    /// The frames of the calls that returned, one per [`Entry::CallPopped`] of the stack, in
    /// the same order: records leave the stack last first, as undo needs them.
    popped: Vec<Frame>,
}

/// The vectors a thread of the program keeps from match to match, so that matching again
/// allocates nothing once they have grown (the analyzers of 011 FR-028 match every token).
#[derive(Default)]
struct Spare {
    threads: Vec<Thread>,
    slots: Vec<Vec<Option<usize>>>,
    counters: Vec<Vec<u32>>,
    starts: Vec<Vec<usize>>,
}

/// The most vectors of each kind kept; a match rarely holds more at once.
const SPARE: usize = 64;

thread_local! {
    static SPARES: std::cell::RefCell<Spare> = std::cell::RefCell::new(Spare::default());
}

/// Runs `f` on the spare vectors of this thread; `None` once the thread has dropped them.
fn spare<R>(f: impl FnOnce(&mut Spare) -> R) -> Option<R> {
    SPARES.try_with(|s| f(&mut s.borrow_mut())).ok()
}

/// Takes a spare vector of `list`, emptied, or a new one.
fn take<T>(list: impl FnOnce(&mut Spare) -> &mut Vec<Vec<T>>) -> Vec<T> {
    let mut v = spare(|s| list(s).pop()).flatten().unwrap_or_default();
    v.clear();
    v
}

/// Gives `v` back to `list`.
fn give<T>(v: Vec<T>, list: impl FnOnce(&mut Spare) -> &mut Vec<Vec<T>>) {
    if v.capacity() == 0 {
        return;
    }
    spare(|s| {
        let l = list(s);
        if l.len() < SPARE {
            l.push(v);
        }
    });
}

/// A spare vector of capture slots.
pub(super) fn take_slots() -> Vec<Option<usize>> {
    take(|s| &mut s.slots)
}

/// Gives a vector of capture slots back.
pub(super) fn give_slots(v: Vec<Option<usize>>) {
    give(v, |s| &mut s.slots);
}

/// A copy of `items` in a spare vector of `list`.
fn copy<T: Copy>(items: &[T], list: impl FnOnce(&mut Spare) -> &mut Vec<Vec<T>>) -> Vec<T> {
    let mut v = take(list);
    v.extend_from_slice(items);
    v
}

/// Gives back the vectors of `frame`.
fn give_frame(frame: Frame) {
    give(frame.slots, |s| &mut s.slots);
    give(frame.counters, |s| &mut s.counters);
    give(frame.starts, |s| &mut s.starts);
}

/// Gives back the vectors a record holds.
fn give_entry(e: Entry) {
    if let Entry::Counters { counters, starts } = e {
        give(counters, |s| &mut s.counters);
        give(starts, |s| &mut s.starts);
    }
}

impl Thread {
    /// A spare thread with `slots` and `counters` counters at zero.
    fn take(slots: &[Option<usize>], counters: usize, in_look: bool) -> Thread {
        let mut t = spare(|s| s.threads.pop()).flatten().unwrap_or_default();
        if t.slots.capacity() == 0 {
            t.slots = take_slots();
        }
        t.slots.clear();
        t.slots.extend_from_slice(slots);
        t.counters.clear();
        t.counters.resize(counters, 0);
        t.starts.clear();
        t.starts.resize(counters, 0);
        t.in_look = in_look;
        t
    }

    /// Gives the thread and the vectors its records and calls hold back.
    fn give(mut self) {
        for e in self.stack.drain(..) {
            give_entry(e);
        }
        for f in self.calls.drain(..).chain(self.popped.drain(..)) {
            give_frame(f);
        }
        self.marks.clear();
        spare(|s| {
            if s.threads.len() < SPARE {
                s.threads.push(self);
            }
        });
    }
}

/// What backtracking found.
enum Back {
    Resume(usize, usize),
    Exhausted,
    Aborted(Abort),
}

enum Step {
    Go(usize, usize),
    Fail,
    Done(usize),
    Aborted(Abort),
}

/// Whether an assertion holds.
enum Holds {
    Yes,
    No,
    Aborted(Abort),
}

impl<'a> Machine<'a> {
    pub(super) fn new(prog: &'a Program, text: &'a str) -> Machine<'a> {
        Machine { prog, text, steps: 0, search_start: 0 }
    }

    /// The first match at or after `from` whose start `may_start` accepts: whether there is
    /// one, with its capture slots in `out`, 0 and 1 being the whole match.
    pub(super) fn search(
        &mut self,
        from: usize,
        groups: usize,
        anchored: bool,
        may_start: impl Fn(usize) -> bool,
        out: &mut Vec<Option<usize>>,
    ) -> Run<bool> {
        let mut start = from;
        self.search_start = from;
        let mut slots = take_slots();
        let found = loop {
            let mut next = None;
            if may_start(start) {
                slots.clear();
                slots.resize(self.prog.slots, None);
                slots[0] = Some(start);
                match self.run(0, start, &slots, None, false) {
                    Err(e) => {
                        give_slots(slots);
                        return Err(e);
                    }
                    Ok(RunEnd::Matched(found, end)) => {
                        out.clear();
                        out.extend_from_slice(&found);
                        give_slots(found);
                        out[1] = Some(end);
                        out.truncate(2 * (groups + 1));
                        break true;
                    }
                    Ok(RunEnd::Aborted(Abort::Commit)) => break false,
                    Ok(RunEnd::Aborted(Abort::Skip(to))) if to > start => next = Some(to),
                    Ok(RunEnd::Failed | RunEnd::Aborted(_)) => {}
                }
            }
            if anchored {
                break false;
            }
            start = match (next, self.text[start..].chars().next()) {
                (Some(to), _) => to,
                (None, Some(c)) => start + c.len_utf8(),
                (None, None) => break false,
            };
        };
        give_slots(slots);
        Ok(found)
    }

    /// Runs from `pc` at `pos` with `slots`; at [`Inst::Match`] or at a [`Inst::LookEnd`]
    /// (ending at `look_end` when given) returns the slots, in a vector to give back with
    /// [`give_slots`], and the position.
    fn run(
        &mut self,
        pc: usize,
        pos: usize,
        slots: &[Option<usize>],
        look_end: Option<usize>,
        in_look: bool,
    ) -> Run<RunEnd> {
        let mut t = Thread::take(slots, self.prog.counters, in_look);
        let end = self.run_thread(&mut t, pc, pos, look_end);
        t.give();
        end
    }

    /// [`Machine::run`] on thread `t`.
    fn run_thread(
        &mut self,
        t: &mut Thread,
        pc: usize,
        pos: usize,
        look_end: Option<usize>,
    ) -> Run<RunEnd> {
        let (mut pc, mut pos) = (pc, pos);
        loop {
            self.steps += 1;
            if self.steps > MAX_STEPS || t.stack.len() > MAX_STACK {
                return Err(MatchError::Limit);
            }
            let back = match self.step(t, pc, pos, look_end)? {
                Step::Go(npc, npos) => Back::Resume(npc, npos),
                Step::Fail => self.backtrack(t),
                Step::Done(end) => {
                    return Ok(RunEnd::Matched(copy(&t.slots, |s| &mut s.slots), end));
                }
                // A verb that acts from inside an assertion only fails the call it is in.
                Step::Aborted(_) if !t.calls.is_empty() => {
                    fail_call(t);
                    self.backtrack(t)
                }
                Step::Aborted(a) => Back::Aborted(a),
            };
            match back {
                Back::Resume(npc, npos) => (pc, pos) = (npc, npos),
                Back::Exhausted => return Ok(RunEnd::Failed),
                Back::Aborted(a) => return Ok(RunEnd::Aborted(a)),
            }
        }
    }

    fn step(
        &mut self,
        t: &mut Thread,
        pc: usize,
        pos: usize,
        look_end: Option<usize>,
    ) -> Run<Step> {
        let text = self.text;
        let next_char = || text[pos..].chars().next();
        let one = |ok: bool, c: Option<char>| match c {
            Some(c) if ok => Step::Go(pc + 1, pos + c.len_utf8()),
            _ => Step::Fail,
        };
        let go = |holds: bool, no: usize| Step::Go(if holds { pc + 1 } else { no }, pos);
        Ok(match &self.prog.insts[pc] {
            Inst::Char(want) => {
                let c = next_char();
                one(c == Some(*want), c)
            }
            Inst::CharSet(set) => {
                let c = next_char();
                one(c.is_some_and(|c| set.contains(&c)), c)
            }
            Inst::Any => {
                let c = next_char();
                one(true, c)
            }
            Inst::AnyNoNewline => {
                let c = next_char();
                one(self.newline_at(pos).is_none(), c)
            }
            Inst::Class(class) => {
                let c = next_char();
                one(c.is_some_and(|c| class.matches(c)), c)
            }
            Inst::Assert(a) => {
                if self.assertion(*a, pos) {
                    Step::Go(pc + 1, pos)
                } else {
                    Step::Fail
                }
            }
            Inst::AltStart(alt) => {
                t.stack.push(Entry::AltStart(*alt));
                Step::Go(pc + 1, pos)
            }
            Inst::Split { next, alt } => {
                t.stack.push(Entry::Choice { pc: *next, pos, alt: Some(*alt) });
                Step::Go(pc + 1, pos)
            }
            Inst::Jmp(to) => Step::Go(*to, pos),
            Inst::Save(slot) => {
                set_slot(t, *slot, Some(pos));
                Step::Go(pc + 1, pos)
            }
            Inst::Close { group, open } => {
                set_slot(t, 2 * group, t.slots[*open]);
                set_slot(t, 2 * group + 1, Some(pos));
                Step::Go(pc + 1, pos)
            }
            Inst::KeepOut => {
                set_slot(t, 0, Some(pos));
                Step::Go(pc + 1, pos)
            }
            Inst::Backref { groups, caseless } => match self.backref(t, groups, *caseless, pos) {
                Some(end) => Step::Go(pc + 1, end),
                None => Step::Fail,
            },
            Inst::AtomicStart => {
                t.stack.push(Entry::Barrier);
                Step::Go(pc + 1, pos)
            }
            Inst::AtomicEnd => {
                cut(t);
                Step::Go(pc + 1, pos)
            }
            Inst::Look(look) => match self.lookaround(t, look, pos, false)? {
                Holds::Yes => Step::Go(look.next, pos),
                Holds::No => Step::Fail,
                Holds::Aborted(a) => Step::Aborted(a),
            },
            Inst::CondLook { look, no } => match self.lookaround(t, look, pos, true)? {
                Holds::Yes => Step::Go(look.next, pos),
                Holds::No | Holds::Aborted(_) => Step::Go(*no, pos),
            },
            Inst::CondGroup { groups, no } => {
                go(groups.iter().any(|g| t.slots.get(2 * g + 1).copied().flatten().is_some()), *no)
            }
            Inst::CondCall { groups, no } => go(
                t.calls
                    .last()
                    .is_some_and(|f| groups.as_ref().is_none_or(|g| g.contains(&f.group))),
                *no,
            ),
            Inst::CondFixed { holds, no } => go(*holds, *no),
            Inst::Call { group, target } => {
                let frame = Frame {
                    group: *group,
                    ret: pc + 1,
                    slots: copy(&t.slots, |s| &mut s.slots),
                    counters: copy(&t.counters, |s| &mut s.counters),
                    starts: copy(&t.starts, |s| &mut s.starts),
                };
                t.calls.push(frame);
                t.stack.push(Entry::CallPushed { depth: t.calls.len() });
                Step::Go(*target, pos)
            }
            Inst::Ret(group) => match t.calls.last() {
                Some(f) if f.group == *group => Step::Go(ret(t), pos),
                _ => Step::Go(pc + 1, pos),
            },
            Inst::Accept { napla: Some((alt, slot, end)), .. } => {
                // The non-atomic assertion holds, and is no longer backtracked into.
                cut_to(t, |e| matches!(e, Entry::AltStart(a) if a == alt));
                Step::Go(*end, t.slots[*slot].unwrap_or(pos))
            }
            Inst::Accept { .. } if !t.calls.is_empty() => {
                // The call ends here and is not backtracked into.
                let depth = t.calls.len();
                cut_to(t, |e| matches!(e, Entry::CallPushed { depth: d } if *d == depth));
                Step::Go(ret(t), pos)
            }
            Inst::Accept { .. } if self.refused_empty(t, pos) => Step::Fail,
            Inst::Accept { open, .. } => {
                for &(g, slot) in open {
                    if let Some(start) = t.slots[slot] {
                        set_slot(t, 2 * g, Some(start));
                        set_slot(t, 2 * g + 1, Some(pos));
                    }
                }
                Step::Done(pos)
            }
            Inst::Verb(_) => {
                t.stack.push(Entry::Verb { pc, pos, depth: t.calls.len() });
                Step::Go(pc + 1, pos)
            }
            Inst::Mark(_) => {
                t.marks.push((pc, pos));
                t.stack.push(Entry::Mark);
                Step::Go(pc + 1, pos)
            }
            Inst::ScriptRunEnd(slot) => {
                let from = t.slots[*slot].unwrap_or(pos);
                if is_script_run(&text[from..pos]) { Step::Go(pc + 1, pos) } else { Step::Fail }
            }
            Inst::LookRestore(slot) => Step::Go(pc + 1, t.slots[*slot].unwrap_or(pos)),
            Inst::BehindStart { slot, min, max } => {
                set_slot(t, *slot, Some(pos));
                let starts = char_starts_before(text, pos, *max);
                let kept = starts.iter().enumerate().filter(|(len, _)| len >= min).count();
                // The longest body first, as PCRE2 does.
                let mut pushed = 0;
                for (_, &s) in starts.iter().enumerate().filter(|(len, _)| len >= min) {
                    pushed += 1;
                    if pushed == kept {
                        break;
                    }
                    t.stack.push(Entry::Choice { pc: pc + 1, pos: s, alt: None });
                }
                let last = starts.iter().enumerate().rfind(|(len, _)| len >= min).map(|(_, &s)| s);
                give(starts, |s| &mut s.starts);
                match last {
                    Some(s) => Step::Go(pc + 1, s),
                    None => Step::Fail,
                }
            }
            Inst::BehindEnd(slot) => {
                if t.slots[*slot] == Some(pos) {
                    Step::Go(pc + 1, pos)
                } else {
                    Step::Fail
                }
            }
            Inst::LookEnd => match look_end {
                Some(end) if end != pos => Step::Fail,
                _ => Step::Done(pos),
            },
            Inst::Match if !t.calls.is_empty() => Step::Go(ret(t), pos),
            Inst::Match if self.refused_empty(t, pos) => Step::Fail,
            Inst::Match => Step::Done(pos),
            Inst::Fail => Step::Fail,
            Inst::CounterStart(slot) => {
                save_counter(t, *slot);
                t.counters[*slot] = 0;
                Step::Go(pc + 1, pos)
            }
            Inst::CounterLoop { slot, min, max, greedy, exit } => {
                let count = t.counters[*slot];
                if count < *min {
                    enter(t, *slot, pos);
                    Step::Go(pc + 1, pos)
                } else if *max == Some(count) {
                    Step::Go(*exit, pos)
                } else if *greedy {
                    t.stack.push(Entry::Choice { pc: *exit, pos, alt: None });
                    enter(t, *slot, pos);
                    Step::Go(pc + 1, pos)
                } else {
                    t.stack.push(Entry::Choice { pc: pc + 1, pos, alt: None });
                    Step::Go(*exit, pos)
                }
            }
            Inst::CounterNext { slot, head, min, exit } => {
                let count = t.counters[*slot] + 1;
                let empty = pos == t.starts[*slot];
                save_counter(t, *slot);
                t.counters[*slot] = count;
                if empty && count >= *min { Step::Go(*exit, pos) } else { Step::Go(*head, pos) }
            }
            Inst::Grapheme => match grapheme_end(text, pos) {
                Some(end) => Step::Go(pc + 1, end),
                None => Step::Fail,
            },
            Inst::NewlineSeq => {
                let rest = &text[pos..];
                if rest.starts_with("\r\n") {
                    Step::Go(pc + 1, pos + 2)
                } else {
                    let c = next_char();
                    let ok = match self.prog.options.bsr {
                        Bsr::Unicode => c.is_some_and(is_vspace),
                        Bsr::AnyCrlf => matches!(c, Some('\r' | '\n')),
                    };
                    one(ok, c)
                }
            }
        })
    }

    /// Whether the match would end empty at `pos` where `(*NOTEMPTY)` forbids it.
    fn refused_empty(&self, t: &Thread, pos: usize) -> bool {
        if t.in_look || t.slots[0] != Some(pos) {
            return false;
        }
        match self.prog.options.not_empty {
            Some(NotEmpty::Anywhere) => true,
            Some(NotEmpty::AtStart) => pos == self.search_start,
            None => false,
        }
    }

    /// Undoes records up to the last choice and resumes there; a verb met on the way acts.
    fn backtrack(&self, t: &mut Thread) -> Back {
        while let Some(e) = t.stack.pop() {
            match e {
                Entry::Choice { pc, pos, .. } => return Back::Resume(pc, pos),
                Entry::Verb { pc, pos, depth } => {
                    if let Some(back) = self.verb(t, pc, pos, depth) {
                        return back;
                    }
                }
                e => undo(t, e),
            }
        }
        Back::Exhausted
    }

    /// What the verb at `pc`, passed at `pos` with `depth` calls in progress, does when
    /// backtracked onto; `None` to go on backtracking.
    fn verb(&self, t: &mut Thread, pc: usize, pos: usize, depth: usize) -> Option<Back> {
        let Inst::Verb(kind) = &self.prog.insts[pc] else { return None };
        if let VerbKind::Then(Some(alt)) = kind {
            // The next alternative of the alternation, unless the call ends first.
            while let Some(e) = t.stack.pop() {
                match e {
                    Entry::Choice { pc, pos, alt: Some(a) } if a == *alt => {
                        return Some(Back::Resume(pc, pos));
                    }
                    Entry::AltStart(a) if a == *alt => return None,
                    Entry::CallPushed { depth: d } if d == depth => {
                        undo(t, Entry::CallPushed { depth: d });
                        return None;
                    }
                    e => undo(t, e),
                }
            }
            return Some(Back::Exhausted);
        }
        if depth > 0 {
            // In a called group the verb only fails the call.
            fail_call(t);
            return None;
        }
        Some(match kind {
            VerbKind::Commit => Back::Aborted(Abort::Commit),
            VerbKind::Prune => Back::Aborted(Abort::Prune),
            VerbKind::Then(_) if t.in_look => Back::Exhausted,
            VerbKind::Then(_) => Back::Aborted(Abort::Prune),
            VerbKind::Skip => Back::Aborted(Abort::Skip(pos)),
            VerbKind::SkipTo(name) => {
                let mark = t.marks.iter().rev().find(
                    |(mpc, _)| matches!(&self.prog.insts[*mpc], Inst::Mark(m) if m == name),
                )?;
                Back::Aborted(Abort::Skip(mark.1))
            }
        })
    }

    fn assertion(&self, a: Assertion, pos: usize) -> bool {
        let text = self.text;
        let before = text[..pos].chars().next_back();
        let after = text[pos..].chars().next();
        let final_newline = || self.newline_at(pos).is_some_and(|n| pos + n == text.len());
        match a {
            Assertion::Start { multiline } => {
                pos == 0 || (multiline && self.newline_before(pos) && pos < text.len())
            }
            Assertion::End { multiline } => {
                pos == text.len()
                    || (multiline && self.newline_at(pos).is_some())
                    || final_newline()
            }
            Assertion::SubjectStart => pos == 0,
            Assertion::SubjectEnd => pos == text.len(),
            Assertion::SubjectEndNewline => pos == text.len() || final_newline(),
            Assertion::MatchStart => pos == self.search_start,
            Assertion::GraphemeBoundary => grapheme_boundary(text, pos),
            Assertion::WordBoundary { negate, ucp } => {
                let word = |c: Option<char>| {
                    c.is_some_and(|c| if ucp { is_unicode_word(c) } else { is_ascii_word(c) })
                };
                (word(before) != word(after)) != negate
            }
        }
    }

    /// The length in bytes of the newline that starts at `pos`, by the newline convention.
    fn newline_at(&self, pos: usize) -> Option<usize> {
        let rest = &self.text[pos..];
        let c = rest.chars().next()?;
        let crlf = rest.starts_with("\r\n");
        match self.prog.options.newline {
            Newline::Lf => (c == '\n').then_some(1),
            Newline::Cr => (c == '\r').then_some(1),
            Newline::Crlf => crlf.then_some(2),
            Newline::AnyCrlf if crlf => Some(2),
            Newline::AnyCrlf => matches!(c, '\r' | '\n').then_some(1),
            Newline::Any if crlf => Some(2),
            Newline::Any => is_vspace(c).then_some(c.len_utf8()),
            Newline::Nul => (c == '\0').then_some(1),
        }
    }

    /// Whether a newline ends at `pos`.
    fn newline_before(&self, pos: usize) -> bool {
        let Some(c) = self.text[..pos].chars().next_back() else { return false };
        let at = pos - c.len_utf8();
        if c == '\n' && at > 0 && self.newline_at(at - 1) == Some(2) {
            return true;
        }
        self.newline_at(at) == Some(c.len_utf8())
    }

    fn backref(&self, t: &Thread, groups: &[usize], caseless: Case, pos: usize) -> Option<usize> {
        let (s, e) = groups.iter().find_map(|g| {
            let s = t.slots.get(2 * g).copied().flatten()?;
            let e = t.slots.get(2 * g + 1).copied().flatten()?;
            Some((s, e))
        })?;
        let captured = &self.text[s..e.max(s)];
        let rest = &self.text[pos..];
        if caseless == Case::Sensitive {
            return rest.starts_with(captured).then_some(pos + captured.len());
        }
        let mut end = pos;
        let mut subject = rest.chars();
        for want in captured.chars() {
            let got = subject.next()?;
            if got != want && !caseless.variants(want).contains(&got) {
                return None;
            }
            end += got.len_utf8();
        }
        Some(end)
    }

    /// Whether the lookaround holds at `pos`; a positive one keeps the captures it set. A verb
    /// that aborts inside makes a negative assertion hold and a condition fail, and acts on the
    /// whole match from a standalone positive assertion.
    fn lookaround(
        &mut self,
        t: &mut Thread,
        look: &Look,
        pos: usize,
        condition: bool,
    ) -> Run<Holds> {
        let mut found = RunEnd::Failed;
        if look.behind {
            let starts = char_starts_before(self.text, pos, look.max);
            // The longest body first, as PCRE2 does.
            for (len, &start) in starts.iter().enumerate().rev() {
                if len < look.min {
                    continue;
                }
                found = match self.run(look.body, start, &t.slots, Some(pos), true) {
                    Ok(found) => found,
                    Err(e) => {
                        give(starts, |s| &mut s.starts);
                        return Err(e);
                    }
                };
                if !matches!(found, RunEnd::Failed) {
                    break;
                }
            }
            give(starts, |s| &mut s.starts);
        } else {
            found = self.run(look.body, pos, &t.slots, None, true)?;
        }
        Ok(match (found, look.negate) {
            (RunEnd::Matched(slots, _), false) => {
                keep_captures(t, slots);
                Holds::Yes
            }
            (RunEnd::Aborted(a), false) if !condition => Holds::Aborted(a),
            // A negative condition that fails keeps what its assertion captured.
            (RunEnd::Matched(slots, _), true) if condition => {
                keep_captures(t, slots);
                Holds::No
            }
            (RunEnd::Failed | RunEnd::Aborted(_), true) => Holds::Yes,
            (RunEnd::Matched(slots, _), _) => {
                give_slots(slots);
                Holds::No
            }
            _ => Holds::No,
        })
    }
}

/// Sets the captures of a lookaround that holds, and gives its slots back.
fn keep_captures(t: &mut Thread, slots: Vec<Option<usize>>) {
    for (i, &v) in slots.iter().enumerate() {
        if i > 1 && t.slots[i] != v {
            set_slot(t, i, v);
        }
    }
    give_slots(slots);
}

fn set_slot(t: &mut Thread, slot: usize, v: Option<usize>) {
    t.stack.push(Entry::Slot { slot, old: t.slots[slot] });
    t.slots[slot] = v;
}

fn save_counter(t: &mut Thread, slot: usize) {
    t.stack.push(Entry::Counter { slot, count: t.counters[slot], start: t.starts[slot] });
}

/// Enters an iteration of a repetition at `pos`.
fn enter(t: &mut Thread, slot: usize, pos: usize) {
    save_counter(t, slot);
    t.starts[slot] = pos;
}

/// Returns from the latest call: the captures and counters go back to what they were before
/// it. Returns the instruction after the call.
fn ret(t: &mut Thread) -> usize {
    let Some(frame) = t.calls.pop() else { return 0 };
    for i in 2..t.slots.len() {
        if t.slots[i] != frame.slots[i] {
            set_slot(t, i, frame.slots[i]);
        }
    }
    let counters = std::mem::replace(&mut t.counters, copy(&frame.counters, |s| &mut s.counters));
    let starts = std::mem::replace(&mut t.starts, copy(&frame.starts, |s| &mut s.starts));
    t.stack.push(Entry::Counters { counters, starts });
    let to = frame.ret;
    t.popped.push(frame);
    t.stack.push(Entry::CallPopped);
    to
}

/// Unwinds the stack to the start of the latest call, which fails.
fn fail_call(t: &mut Thread) {
    let depth = t.calls.len();
    while let Some(e) = t.stack.pop() {
        let end = matches!(e, Entry::CallPushed { depth: d } if d == depth);
        undo(t, e);
        if end {
            return;
        }
    }
}

/// Undoes a record; choices, verbs and markers are dropped.
fn undo(t: &mut Thread, e: Entry) {
    match e {
        Entry::Slot { slot, old } => t.slots[slot] = old,
        Entry::Counter { slot, count, start } => {
            t.counters[slot] = count;
            t.starts[slot] = start;
        }
        Entry::Counters { counters, starts } => {
            give(std::mem::replace(&mut t.counters, counters), |s| &mut s.counters);
            give(std::mem::replace(&mut t.starts, starts), |s| &mut s.starts);
        }
        Entry::Mark => {
            t.marks.pop();
        }
        Entry::CallPushed { .. } => {
            if let Some(f) = t.calls.pop() {
                give_frame(f);
            }
        }
        Entry::CallPopped => {
            if let Some(frame) = t.popped.pop() {
                t.calls.push(frame);
            }
        }
        Entry::Choice { .. } | Entry::Verb { .. } | Entry::Barrier | Entry::AltStart(_) => {}
    }
}

/// Drops the choices and verbs above the last barrier, and the barrier: an atomic group is never
/// backtracked into, and the marks inside it leave the path. Undo records stay.
fn cut(t: &mut Thread) {
    let Some(at) = t.stack.iter().rposition(|e| matches!(e, Entry::Barrier)) else { return };
    drop_choices(t, at + 1);
    t.stack.remove(at);
}

/// Drops the choices and verbs above the latest entry that `marker` accepts.
fn cut_to(t: &mut Thread, marker: impl Fn(&Entry) -> bool) {
    if let Some(at) = t.stack.iter().rposition(marker) {
        drop_choices(t, at + 1);
    }
}

/// Drops the choices, verbs and marks from `at` on; undo records stay.
fn drop_choices(t: &mut Thread, at: usize) {
    let Thread { stack, kept, marks: path, .. } = t;
    kept.clear();
    let mut marks = 0;
    for e in stack.drain(at..) {
        match e {
            Entry::Choice { .. } | Entry::Verb { .. } => {}
            Entry::Mark => marks += 1,
            e => kept.push(e),
        }
    }
    let n = path.len().saturating_sub(marks);
    path.truncate(n);
    stack.append(kept);
}

/// Byte positions of the `0..=max` characters before `pos`, nearest first.
fn char_starts_before(text: &str, pos: usize, max: usize) -> Vec<usize> {
    let mut out = take(|s| &mut s.starts);
    out.push(pos);
    let mut p = pos;
    for c in text[..pos].chars().rev().take(max) {
        p -= c.len_utf8();
        out.push(p);
    }
    out
}

/// Whether `pos` is a boundary of extended grapheme clusters: the clusters are followed from the
/// start of the text.
fn grapheme_boundary(text: &str, pos: usize) -> bool {
    let mut at = 0;
    while at < pos {
        match grapheme_end(text, at) {
            Some(end) => at = end,
            None => break,
        }
    }
    at == pos
}

/// The end of the extended grapheme cluster that starts at `pos` (UAX #29).
fn grapheme_end(text: &str, pos: usize) -> Option<usize> {
    let mut chars = text[pos..].char_indices().map(|(i, c)| (pos + i, c));
    let (_, first) = chars.next()?;
    let mut prev = grapheme_break(first);
    let mut end = pos + first.len_utf8();
    let mut regional = usize::from(prev == Gcb::RegionalIndicator);
    let mut emoji = pictographic(first);
    for (i, c) in chars {
        let cur = grapheme_break(c);
        let joined = match (prev, cur) {
            (Gcb::Cr, Gcb::Lf) => true,
            (Gcb::Cr | Gcb::Lf | Gcb::Control, _) | (_, Gcb::Cr | Gcb::Lf | Gcb::Control) => false,
            (Gcb::L, Gcb::L | Gcb::V | Gcb::Lv | Gcb::Lvt) => true,
            (Gcb::Lv | Gcb::V, Gcb::V | Gcb::T) => true,
            (Gcb::Lvt | Gcb::T, Gcb::T) => true,
            (_, Gcb::Extend | Gcb::Zwj | Gcb::SpacingMark) => true,
            (Gcb::Prepend, _) => true,
            // PCRE2 joins pictographs even without a ZWJ between them.
            _ if emoji && pictographic(c) => true,
            (Gcb::RegionalIndicator, Gcb::RegionalIndicator) => regional % 2 == 1,
            _ => false,
        };
        if !joined {
            break;
        }
        if cur != Gcb::Extend && cur != Gcb::Zwj {
            emoji = pictographic(c);
        }
        regional = if cur == Gcb::RegionalIndicator { regional + 1 } else { 0 };
        prev = cur;
        end = i + c.len_utf8();
    }
    Some(end)
}
