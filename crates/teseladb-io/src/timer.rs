//! Hierarchical timer wheel with 1 ms resolution: 6 levels of 64 slots (a slot of level `l` spans
//! 64^l ms, up to 64^6 ms ≈ 2.2 years ahead). Insert and cancel are O(1); advancing jumps straight
//! to the next deadline and entries move down a level when their slot is entered.

const LEVELS: usize = 6;
const SLOTS: usize = 64;
const BITS: u32 = 6;

/// Handle of an inserted timer (to cancel it).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TimerId {
    index: u32,
    generation: u32,
}

#[derive(Debug)]
struct Entry<T> {
    deadline: u64,
    generation: u32,
    value: Option<T>,
}

type Ref = (u32, u32);

/// Timer wheel holding values `T`, in milliseconds of a monotonic clock.
#[derive(Debug)]
pub struct TimerWheel<T> {
    /// Every deadline ≤ `cur` has fired (except what waits in `due`).
    cur: u64,
    /// `slots[level][slot]`.
    slots: Vec<Vec<Vec<Ref>>>,
    /// Inserted with a deadline already past: they fire on the next `advance`.
    due: Vec<Ref>,
    /// Beyond the wheel's range: placed once `cur` gets close enough.
    overflow: Vec<Ref>,
    entries: Vec<Entry<T>>,
    free: Vec<u32>,
    live: usize,
}

/// Level of a deadline `d > cur`, or `None` if it is out of range.
fn level_of(d: u64, cur: u64) -> Option<usize> {
    let diff = d ^ cur;
    let level = ((63 - diff.leading_zeros()) / BITS) as usize;
    (level < LEVELS).then_some(level)
}

fn slot_of(d: u64, level: usize) -> usize {
    ((d >> (BITS * level as u32)) as usize) & (SLOTS - 1)
}

impl<T> TimerWheel<T> {
    /// Empty wheel at instant `now_ms`.
    pub fn new(now_ms: u64) -> TimerWheel<T> {
        TimerWheel {
            cur: now_ms,
            slots: (0..LEVELS).map(|_| (0..SLOTS).map(|_| Vec::new()).collect()).collect(),
            due: Vec::new(),
            overflow: Vec::new(),
            entries: Vec::new(),
            free: Vec::new(),
            live: 0,
        }
    }

    /// Pending timers.
    pub fn len(&self) -> usize {
        self.live
    }

    /// No timers.
    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// Instant the wheel has advanced to.
    pub fn now(&self) -> u64 {
        self.cur
    }

    /// Schedules `value` for `deadline_ms`; if it already passed, it fires on the next `advance`.
    pub fn insert(&mut self, deadline_ms: u64, value: T) -> TimerId {
        let index = match self.free.pop() {
            Some(i) => {
                let e = &mut self.entries[i as usize];
                e.generation = e.generation.wrapping_add(1);
                e.deadline = deadline_ms;
                e.value = Some(value);
                i
            }
            None => {
                self.entries.push(Entry {
                    deadline: deadline_ms,
                    generation: 0,
                    value: Some(value),
                });
                (self.entries.len() - 1) as u32
            }
        };
        let r = (index, self.entries[index as usize].generation);
        self.place(r);
        self.live += 1;
        TimerId { index, generation: r.1 }
    }

    /// Cancels a pending timer and returns its value.
    pub fn cancel(&mut self, id: TimerId) -> Option<T> {
        let e = self.entries.get_mut(id.index as usize)?;
        if e.generation != id.generation {
            return None;
        }
        let v = e.value.take()?;
        // The reference in its slot is left without a value and dropped when reached; the entry
        // is reused with another generation.
        self.free.push(id.index);
        self.live -= 1;
        Some(v)
    }

    /// Deadline of the next timer (exact), or `None` if there is none.
    pub fn next_deadline(&self) -> Option<u64> {
        if self.live == 0 {
            return None;
        }
        if self.due.iter().any(|&r| self.alive(r)) {
            return Some(self.cur);
        }
        let mut best = self.min_deadline(&self.overflow);
        for level in 0..LEVELS {
            let start = slot_of(self.cur, level);
            // The level's deadlines sit in slots after the current one: the first occupied slot
            // holds the earliest ones.
            for k in 1..SLOTS {
                if let Some(m) = self.min_deadline(&self.slots[level][(start + k) % SLOTS]) {
                    best = Some(best.map_or(m, |b: u64| b.min(m)));
                    break;
                }
            }
        }
        best
    }

    /// Advances to `now_ms` and appends to `out` the expired values (deadline ≤ `now_ms`) in
    /// deadline order (the order among equal deadlines is unspecified).
    pub fn advance(&mut self, now_ms: u64, out: &mut Vec<T>) {
        while self.live > 0 {
            // Entries can land in `due` while cascading (a deadline equal to the new `cur`).
            let due = core::mem::take(&mut self.due);
            self.fire(due, out);
            match self.next_deadline() {
                Some(d) if d <= now_ms => {
                    self.jump_to(d);
                    let s = slot_of(d, 0);
                    let refs = core::mem::take(&mut self.slots[0][s]);
                    self.fire(refs, out);
                }
                _ => break,
            }
        }
        if now_ms > self.cur {
            self.jump_to(now_ms);
        }
    }

    fn alive(&self, (i, g): Ref) -> bool {
        let e = &self.entries[i as usize];
        e.generation == g && e.value.is_some()
    }

    fn min_deadline(&self, refs: &[Ref]) -> Option<u64> {
        refs.iter()
            .filter(|&&r| self.alive(r))
            .map(|&(i, _)| self.entries[i as usize].deadline)
            .min()
    }

    fn fire(&mut self, refs: Vec<Ref>, out: &mut Vec<T>) {
        for r in refs {
            if self.alive(r) {
                out.push(self.entries[r.0 as usize].value.take().expect("live entry"));
                self.free.push(r.0);
                self.live -= 1;
            }
        }
    }

    /// Moves `cur` to `target` (no live deadlines in between except at `target`) and cascades the
    /// slots being entered.
    fn jump_to(&mut self, target: u64) {
        debug_assert!(target >= self.cur);
        let old = self.cur;
        self.cur = target;
        for level in (1..LEVELS).rev() {
            if slot_of(old, level) == slot_of(target, level)
                && (old ^ target) >> (BITS * (level as u32 + 1)) == 0
            {
                continue;
            }
            let refs = core::mem::take(&mut self.slots[level][slot_of(target, level)]);
            for r in refs {
                if self.alive(r) {
                    self.place(r);
                }
            }
        }
        if !self.overflow.is_empty() {
            let refs = core::mem::take(&mut self.overflow);
            for r in refs {
                if self.alive(r) {
                    self.place(r);
                }
            }
        }
    }

    fn place(&mut self, r: Ref) {
        let d = self.entries[r.0 as usize].deadline;
        if d <= self.cur {
            self.due.push(r);
            return;
        }
        match level_of(d, self.cur) {
            Some(level) => self.slots[level][slot_of(d, level)].push(r),
            None => self.overflow.push(r),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn levels() {
        assert_eq!(level_of(1, 0), Some(0));
        assert_eq!(level_of(63, 0), Some(0));
        assert_eq!(level_of(64, 0), Some(1));
        assert_eq!(level_of(64u64.pow(6) - 1, 0), Some(5));
        assert_eq!(level_of(64u64.pow(6), 0), None);
    }
}
