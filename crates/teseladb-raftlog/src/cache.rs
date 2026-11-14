//! Content cache with a byte budget and CLOCK eviction (data-model §8, `ContentCache`).
//!
//! Each slot holds a value (for example, a block of a file) and its owner key `K`. Pinned slots are
//! not evicted. The caller keeps the slot number next to its own record of the key; the cache
//! returns the owners of the slots it evicts so the caller can forget them.

use teseladb_types::Bytes;

/// Empty slot.
pub const NO_SLOT: u32 = u32::MAX;

#[derive(Debug, Clone)]
struct Slot<K> {
    owner: K,
    data: Bytes,
    referenced: bool,
    pinned: bool,
}

/// Content cache.
#[derive(Debug, Clone)]
pub struct ContentCache<K> {
    slots: Vec<Option<Slot<K>>>,
    free: Vec<u32>,
    hand: usize,
    used: u64,
    budget: u64,
}

impl<K> ContentCache<K> {
    /// Empty cache with a budget of `budget` bytes.
    #[must_use]
    pub fn new(budget: u64) -> ContentCache<K> {
        ContentCache { slots: Vec::new(), free: Vec::new(), hand: 0, used: 0, budget }
    }

    /// Bytes used.
    #[must_use]
    pub fn used(&self) -> u64 {
        self.used
    }

    /// Budget.
    #[must_use]
    pub fn budget(&self) -> u64 {
        self.budget
    }

    /// Inserts contents and returns its slot and the owners of the slots evicted to make room, so
    /// the caller forgets them. A pinned slot is never evicted, so the cache may exceed
    /// the budget if everything in it is pinned.
    ///
    /// # Panics
    ///
    /// When it would hold 2³² slots.
    pub fn insert(&mut self, owner: K, data: Bytes, pinned: bool) -> (u32, Vec<K>) {
        let size = data.len() as u64;
        let evicted = self.make_room(size);
        let slot = Slot { owner, data, referenced: true, pinned };
        self.used += size;
        let id = if let Some(id) = self.free.pop() {
            self.slots[id as usize] = Some(slot);
            id
        } else {
            self.slots.push(Some(slot));
            u32::try_from(self.slots.len() - 1).expect("a cache holds fewer than 2^32 slots")
        };
        (id, evicted)
    }

    fn make_room(&mut self, incoming: u64) -> Vec<K> {
        let mut evicted = Vec::new();
        // At most two rounds: the first one clears the reference bits.
        let mut budget_steps = 2 * self.slots.len();
        while self.used + incoming > self.budget && budget_steps > 0 {
            budget_steps -= 1;
            let i = self.hand;
            self.hand = (self.hand + 1) % self.slots.len();
            let Some(s) = self.slots[i].as_mut() else {
                continue;
            };
            if s.pinned {
                continue;
            }
            if s.referenced {
                s.referenced = false;
                continue;
            }
            let s = self.slots[i].take().expect("occupied slot");
            self.used -= s.data.len() as u64;
            self.free.push(u32::try_from(i).expect("a slot that was handed out as a u32"));
            evicted.push(s.owner);
        }
        evicted
    }

    /// Contents of a slot (marks the reference for CLOCK).
    pub fn get(&mut self, slot: u32) -> Option<Bytes> {
        let s = self.slots.get_mut(slot as usize)?.as_mut()?;
        s.referenced = true;
        Some(s.data.clone())
    }

    /// Pins or unpins a slot.
    pub fn set_pinned(&mut self, slot: u32, pinned: bool) {
        if let Some(Some(s)) = self.slots.get_mut(slot as usize) {
            s.pinned = pinned;
        }
    }

    /// Frees a slot.
    pub fn remove(&mut self, slot: u32) {
        if let Some(entry) = self.slots.get_mut(slot as usize)
            && let Some(s) = entry.take()
        {
            self.used -= s.data.len() as u64;
            self.free.push(slot);
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn owner(i: u8) -> (u64, u8) {
        (1, i)
    }

    #[test]
    fn evicts_unreferenced_first_and_respects_pins() {
        let mut c = ContentCache::new(300);
        assert_eq!(c.budget(), 300);
        let (a, _) = c.insert(owner(1), Bytes::from_vec(vec![0; 100]), false);
        let (b, _) = c.insert(owner(2), Bytes::from_vec(vec![0; 100]), true);
        let (_c3, _) = c.insert(owner(3), Bytes::from_vec(vec![0; 100]), false);
        assert_eq!(c.used(), 300);
        // An access to `a` gives it another round; the next insert evicts slot 3.
        assert!(c.get(a).is_some());
        let (_d, ev) = c.insert(owner(4), Bytes::from_vec(vec![0; 100]), false);
        assert_eq!(ev.len(), 1);
        assert!(ev[0] != owner(2), "the pinned one is not evicted");
        assert!(c.used() <= 300);
        // If everything is pinned, the budget is exceeded instead of evicting.
        let mut p = ContentCache::new(100);
        p.insert(owner(1), Bytes::from_vec(vec![0; 100]), true);
        let (_, ev) = p.insert(owner(2), Bytes::from_vec(vec![0; 100]), true);
        assert!(ev.is_empty());
        assert_eq!(p.used(), 200);
        // Unpin and remove.
        c.set_pinned(b, false);
        c.remove(b);
        assert!(c.get(b).is_none());
        c.remove(b);
        c.set_pinned(999, true);
        assert!(c.get(999).is_none());
        c.remove(999);
    }

    #[test]
    #[expect(clippy::many_single_char_names, reason = "a, b and c are the slots of the case")]
    fn reuses_free_slots() {
        let mut c = ContentCache::new(1000);
        let (a, _) = c.insert(owner(1), Bytes::from("x"), false);
        c.remove(a);
        let (b, _) = c.insert(owner(2), Bytes::from("y"), false);
        assert_eq!(a, b);
        assert_eq!(c.get(b).expect("get succeeds"), Bytes::from("y"));
        // Cache with a 0 budget and empty slots in between: it does not get stuck.
        let mut z = ContentCache::new(0);
        let (s, _) = z.insert(owner(1), Bytes::from("a"), false);
        z.remove(s);
        z.insert(owner(2), Bytes::from("b"), false);
        z.insert(owner(3), Bytes::from("c"), false);
        assert!(z.used() <= 2);
    }
}
