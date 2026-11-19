//! Tests of the slab allocator: the intrusive lists and page descriptors it is built from, size
//! classes, page limits, the LRU eviction of unlocked items through the erase callback, item
//! locking, its metrics, and the reclaim of whole pages under memory pressure. The classes used
//! here hold at least two objects per page: a class of one object per page (the largest class)
//! is not exercised.
#![cfg(target_os = "linux")]
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]
// The reclaim tests are left out of default-allocator builds (see below), and with them some
// of what they use.
#![cfg_attr(engine_default_allocator, allow(unused_imports, dead_code))]

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::rc::Rc;

use teseladb::engine::core::future::BadAlloc;
use teseladb::engine::core::make_task::make_task_ptr;
use teseladb::engine::core::memory::{
    self, Reclaimer, ReclaimerScope, ReclaimingResult, free_memory, min_free_memory,
    set_min_free_pages,
};
use teseladb::engine::core::metrics::impl_fns::get_values;
use teseladb::engine::core::reactor::engine;
use teseladb::engine::core::scheduling::default_scheduling_group;
use teseladb::engine::core::slab::{
    IntrusiveList, ListHook, SLAB_MAGIC_NUMBER, SlabAllocator, SlabClass, SlabItem, SlabItemBase,
    SlabPageDesc,
};
use teseladb::engine_thread_test_case;

/// Size of a slab page in these tests.
const SLAB_PAGE: u64 = 64 * 1024;
/// Page size of the shard allocator.
const PAGE: usize = 4096;

/// A cache entry: the slab base first, then the page index and an id.
#[repr(C)]
struct Entry {
    base: SlabItemBase,
    page: u32,
    id: u32,
    locked: bool,
}

// SAFETY: `Entry` is repr(C) with its `SlabItemBase` first.
unsafe impl SlabItem for Entry {
    type Args = u32;

    fn construct(slab_page_index: u32, id: u32) -> Self {
        Entry { base: SlabItemBase::default(), page: slab_page_index, id, locked: false }
    }

    fn get_slab_page_index(&self) -> u32 {
        self.page
    }

    fn is_unlocked(&self) -> bool {
        !self.locked
    }
}

fn id_of(e: *mut Entry) -> u32 {
    // SAFETY: the tests only read live entries.
    unsafe { (*e).id }
}

/// The erase callback of a slab.
type Eraser = Option<Box<dyn FnMut(&mut Entry)>>;

/// An erase callback that records the ids of the evicted entries.
fn recording_eraser() -> (Rc<RefCell<Vec<u32>>>, Eraser) {
    let erased = Rc::new(RefCell::new(Vec::new()));
    let sink = erased.clone();
    (erased, Some(Box::new(move |e: &mut Entry| sink.borrow_mut().push(e.id))))
}

/// The value of the metric family `name` on this shard, if it is registered.
fn metric(name: &str) -> Option<f64> {
    let values = get_values();
    let copy = values.get_local().expect("read on this shard");
    let metadata = copy.metadata.as_ref().expect("metadata");
    let i = metadata.iter().position(|f| f.mf.name.as_str() == name)?;
    copy.values[i].front().map(|v| v.d())
}

#[repr(C)]
struct Node {
    hook: ListHook,
    value: u32,
}

fn node(value: u32) -> Box<Node> {
    Box::new(Node { hook: ListHook::default(), value })
}

#[test]
fn intrusive_lists_link_at_both_ends() {
    let mut list = IntrusiveList::<Node, 0>::default();
    assert!(list.empty());
    let (mut a, mut b, mut c) = (node(1), node(2), node(3));
    let (pa, pb, pc): (*mut Node, *mut Node, *mut Node) = (&mut *a, &mut *b, &mut *c);
    // SAFETY: the nodes outlive their membership and are linked once each.
    unsafe {
        list.push_back(pb);
        assert_eq!((*list.back()).value, 2);
        list.push_front(pa);
        list.push_back(pc);
        assert_eq!((*list.back()).value, 3);
        // Erase the middle, then the tail, then the head.
        list.erase(pb);
        assert_eq!((*list.back()).value, 3);
        list.erase(pc);
        assert_eq!((*list.back()).value, 1);
        list.erase(pa);
    }
    assert!(list.empty());
    // SAFETY: as above; the list is cleared before the nodes are dropped.
    unsafe {
        list.push_front(pc);
        list.push_front(pb);
        list.erase(pb);
        assert_eq!((*list.back()).value, 3);
    }
    list.clear();
    assert!(list.empty());
    assert_eq!(a.value + b.value + c.value, 6);
}

#[test]
fn page_descriptors_hand_out_and_take_back_objects() {
    // The descriptor only does address arithmetic: the page is never touched.
    let base = 0x10_000usize as *mut c_void;
    let mut desc = SlabPageDesc::new(base, 4, 32, 7, 3);
    assert_eq!(desc.magic(), SLAB_MAGIC_NUMBER);
    assert_eq!(desc.slab_class_id(), 7);
    assert_eq!(desc.index(), 3);
    assert_eq!(desc.slab_page(), base);
    assert_eq!(*desc.refcnt(), 0);
    *desc.refcnt() += 2;
    assert_eq!(*desc.refcnt(), 2);
    // The first object belongs to the caller; the other three are free.
    assert_eq!(desc.size(), 3);
    assert!(!desc.empty());
    assert_eq!(desc.free_objects().as_slice(), &[0x10_020, 0x10_040, 0x10_060]);
    let last = desc.allocate_object();
    assert_eq!(last as usize, 0x10_060);
    desc.allocate_object();
    desc.allocate_object();
    assert!(desc.empty());
    desc.free_object(last);
    assert_eq!(desc.size(), 1);
    // A page of one object starts full.
    let mut single = SlabPageDesc::new(base, 1, 64, 0, 0);
    assert!(single.empty());
    assert!(single.free_objects().is_empty());
}

#[test]
#[should_panic]
fn a_full_page_descriptor_refuses_to_allocate() {
    let mut desc = SlabPageDesc::new(0x1000usize as *mut c_void, 1, 64, 0, 0);
    desc.allocate_object();
}

#[test]
fn an_empty_class_has_nothing_to_evict() {
    let mut class = SlabClass::<Entry>::new(128, 2);
    assert_eq!(class.size(), 128);
    assert!(class.empty(), "no page has free objects");
    assert!(class.has_no_slab_pages(), "no item is in the LRU");
    let mut no_eraser = None;
    let e = class.create_from_lru(&mut no_eraser, 1).unwrap_err();
    assert!(e.downcast_ref::<BadAlloc>().is_some());
}

engine_thread_test_case!(limited_slabs_fill_their_pages, {
    let mut slab = SlabAllocator::<Entry>::new(1.25, 4 * SLAB_PAGE, SLAB_PAGE).unwrap();
    slab.print_slab_classes();
    // Classes start at 96 bytes and are aligned for the entry.
    assert_eq!(slab.class_size(1), 96);
    assert_eq!(slab.class_size(96), 96);
    let second = slab.class_size(97);
    assert!(second > 96 && second.is_multiple_of(std::mem::align_of::<Entry>()));
    assert_eq!(slab.class_size(SLAB_PAGE as usize), SLAB_PAGE as usize);
    assert_eq!(slab.class_size(SLAB_PAGE as usize + 1), 0);
    let e = slab.create(SLAB_PAGE as usize + 1, 0).unwrap_err();
    assert!(e.downcast_ref::<BadAlloc>().is_some(), "no class fits");

    // The 96-byte class takes the four pages the limit allows, then has no room.
    let per_page = SLAB_PAGE as usize / 96;
    let mut items = Vec::new();
    loop {
        let it = slab.create(90, items.len() as u32).unwrap();
        if it.is_null() {
            break;
        }
        items.push(it);
    }
    assert_eq!(items.len(), per_page * 4);
    assert!(items.iter().enumerate().all(|(i, &it)| id_of(it) == i as u32));
    // SAFETY: the entries are live.
    let pages: std::collections::BTreeSet<u32> =
        items.iter().map(|&it| unsafe { (*it).page }).collect();
    assert_eq!(pages.into_iter().collect::<Vec<_>>(), [0, 1, 2, 3]);

    // The limit holds for all classes together (upstream): a class that has no page yet gets
    // none past it either, and without an eraser it has nothing to evict.
    assert!(slab.create(second, 1000).unwrap().is_null(), "a new class went past the limit");

    // Freed slots are reused without new pages; null is ignored.
    // SAFETY: the entries are live and freed once.
    unsafe {
        slab.free(std::ptr::null_mut());
        slab.touch(std::ptr::null_mut());
        slab.touch(items[0]);
        slab.free(items[5]);
        slab.free(items[6]);
    }
    let again = slab.create(96, 77).unwrap();
    assert_eq!(again, items[6], "the last freed slot is handed out first");
    assert_eq!(id_of(again), 77);
    // SAFETY: as above.
    unsafe {
        slab.free(again);
        for (i, &it) in items.iter().enumerate() {
            if i != 5 && i != 6 {
                slab.free(it);
            }
        }
    }
});

engine_thread_test_case!(full_classes_evict_their_least_recently_used_item, {
    let (erased, eraser) = recording_eraser();
    let mut slab =
        SlabAllocator::<Entry>::with_erase_func(1.25, SLAB_PAGE, SLAB_PAGE, eraser).unwrap();
    let per_page = SLAB_PAGE as usize / 96;
    let items: Vec<*mut Entry> =
        (0..per_page).map(|i| slab.create(96, i as u32).unwrap()).collect();
    assert!(items.iter().all(|it| !it.is_null()));
    // Item 0 becomes the most recently used and item 1 is pinned: item 2 is the victim.
    // SAFETY: the entries are live.
    unsafe {
        slab.touch(items[0]);
        slab.lock_item(items[1]);
    }
    let reused = slab.create(96, 500).unwrap();
    assert_eq!(*erased.borrow(), [2]);
    assert_eq!(reused, items[2], "the victim's slot is reused");
    assert_eq!(id_of(reused), 500);
    // Once unpinned, item 1 is the most recently used: item 3 goes next.
    // SAFETY: as above.
    unsafe { slab.unlock_item(items[1]) };
    let reused = slab.create(96, 501).unwrap();
    assert_eq!(*erased.borrow(), [2, 3]);
    assert_eq!(reused, items[3]);
    // A freed slot is preferred over eviction.
    // SAFETY: as above.
    unsafe { slab.free(items[10]) };
    assert_eq!(slab.create(96, 502).unwrap(), items[10]);
    assert_eq!(erased.borrow().len(), 2);
});

engine_thread_test_case!(pinned_classes_get_no_page_past_the_limit, {
    // A growth factor of 100 gives the classes 96, 9600 and the page size.
    let (erased, eraser) = recording_eraser();
    let mut slab =
        SlabAllocator::<Entry>::with_erase_func(100.0, SLAB_PAGE, SLAB_PAGE, eraser).unwrap();
    assert_eq!(slab.class_size(97), 9600);
    assert_eq!(slab.class_size(9601), SLAB_PAGE as usize);
    let per_page = SLAB_PAGE as usize / 9600;
    let items: Vec<*mut Entry> =
        (0..per_page).map(|i| slab.create(9600, i as u32).unwrap()).collect();
    // With every item pinned the class's LRU is empty. The limit's only page is taken, and
    // it holds for every class (upstream), so no new page is allowed and there is nothing to
    // evict: the allocation fails.
    for &it in &items {
        // SAFETY: the entries are live and unlocked.
        unsafe { slab.lock_item(it) };
    }
    let e = slab.create(9600, 99).unwrap_err();
    assert!(e.downcast_ref::<BadAlloc>().is_some(), "{e}");
    assert!(erased.borrow().is_empty(), "a pinned item was evicted");
    // Unpinned, the least recently used item is evicted for the new one.
    for &it in &items {
        // SAFETY: the entries were locked above.
        unsafe { slab.unlock_item(it) };
    }
    let extra = slab.create(9600, 99).unwrap();
    assert!(!extra.is_null());
    // SAFETY: the entry is live.
    assert_eq!(unsafe { (*extra).page }, 0);
    assert_eq!(erased.borrow().len(), 1);
});

engine_thread_test_case!(slab_metrics_count_operations_and_refuse_a_second_slab, {
    let mut slab = SlabAllocator::<Entry>::new(1.25, 2 * SLAB_PAGE, SLAB_PAGE).unwrap();
    let a = slab.create(100, 1).unwrap();
    let b = slab.create(100, 2).unwrap();
    // SAFETY: `a` is live.
    unsafe { slab.free(a) };
    assert_eq!(metric("slab_malloc_total_operations"), Some(2.0));
    assert_eq!(metric("slab_free_total_operations"), Some(1.0));
    assert_eq!(metric("slab_malloc_objects"), Some(1.0));
    // The metrics are registered once per shard: a second slab cannot register them.
    let second = SlabAllocator::<Entry>::new(1.25, 2 * SLAB_PAGE, SLAB_PAGE);
    assert!(second.is_err());
    // SAFETY: `b` is live.
    unsafe { slab.free(b) };
    drop(slab);
    assert_eq!(metric("slab_malloc_total_operations"), None, "unregistered with the slab");
});

// The engine allocator reclaims under memory pressure; the system allocator of a
// `--cfg engine_default_allocator` build has no reclaim (nor has the original's default-allocator build).
#[cfg(not(engine_default_allocator))]
thread_local! {
    /// Asynchronous reclaim rounds handed to the test's reclaim hook.
    static PENDING: RefCell<Vec<Box<dyn FnOnce()>>> = const { RefCell::new(Vec::new()) };
}

/// Runs `body` with a low-memory threshold above the shard's free memory and a reclaim hook that
/// queues the asynchronous rounds in `PENDING`; restores the reactor's behaviour afterwards.
#[cfg(not(engine_default_allocator))]
fn under_memory_pressure(body: impl FnOnce()) {
    let old_min = min_free_memory() / PAGE;
    let pressure = free_memory() / PAGE + 256;
    set_min_free_pages(pressure).unwrap();
    memory::set_reclaim_hook(Box::new(|f: Box<dyn FnOnce()>| {
        PENDING.with(|p| p.borrow_mut().push(f))
    }));
    body();
    set_min_free_pages(old_min).unwrap();
    // The hook the reactor installs: a high-priority task per round.
    memory::set_reclaim_hook(Box::new(|f: Box<dyn FnOnce()>| {
        // SAFETY: called on this reactor's thread; the reactor outlives the hook's use here.
        unsafe { (*engine()).add_high_priority_task(make_task_ptr(default_scheduling_group(), f)) };
    }));
    let rounds = PENDING.with(|p| std::mem::take(&mut *p.borrow_mut()));
    for round in rounds {
        round();
    }
}

/// Starts the reclaim the threshold calls for and runs the asynchronous rounds it scheduled.
#[cfg(not(engine_default_allocator))]
fn reclaim_now() {
    set_min_free_pages(min_free_memory() / PAGE).unwrap();
    let rounds = PENDING.with(|p| std::mem::take(&mut *p.borrow_mut()));
    assert!(!rounds.is_empty(), "low memory schedules an asynchronous reclaim");
    for round in rounds {
        round();
    }
}

#[cfg(not(engine_default_allocator))]
engine_thread_test_case!(reclaimers_run_by_scope_until_they_stop_progressing, {
    let sync_calls = Rc::new(Cell::new(0u32));
    let async_calls = Rc::new(Cell::new(0u32));
    let (s, a) = (sync_calls.clone(), async_calls.clone());
    // Each reports progress twice, then nothing; neither frees memory.
    let sync = Reclaimer::new(
        Box::new(move || {
            s.set(s.get() + 1);
            if s.get() <= 2 {
                ReclaimingResult::ReclaimedSomething
            } else {
                ReclaimingResult::ReclaimedNothing
            }
        }),
        ReclaimerScope::Sync,
    );
    let asynchronous = Reclaimer::new(
        Box::new(move || {
            a.set(a.get() + 1);
            if a.get() <= 2 {
                ReclaimingResult::ReclaimedSomething
            } else {
                ReclaimingResult::ReclaimedNothing
            }
        }),
        ReclaimerScope::Async,
    );
    assert_eq!(sync.scope(), ReclaimerScope::Sync);
    assert_eq!(asynchronous.scope(), ReclaimerScope::Async);
    let reclaims = memory::stats().reclaims();
    under_memory_pressure(|| {
        set_min_free_pages(min_free_memory() / PAGE).unwrap();
        // The synchronous pass runs only the synchronous reclaimer: twice with progress and
        // a third time without.
        assert_eq!(sync_calls.get(), 3);
        assert_eq!(async_calls.get(), 0);
        reclaim_now();
        // The asynchronous pass runs both.
        assert!(async_calls.get() >= 3);
        assert!(sync_calls.get() >= 4);
    });
    assert!(memory::stats().reclaims() > reclaims);
    drop(sync);
    drop(asynchronous);
});

#[cfg(not(engine_default_allocator))]
engine_thread_test_case!(unlimited_slabs_give_pages_back_under_pressure, {
    let (erased, eraser) = recording_eraser();
    // A limit of 0: unbounded, with a reclaimer.
    let mut slab = SlabAllocator::<Entry>::with_erase_func(1.25, 0, SLAB_PAGE, eraser).unwrap();
    let per_page = SLAB_PAGE as usize / 96;
    let items: Vec<*mut Entry> =
        (0..3 * per_page).map(|i| slab.create(96, i as u32).unwrap()).collect();
    assert!(items.iter().all(|it| !it.is_null()));
    // Free some objects of the first page so that it is evicted with free slots in it.
    // SAFETY: the entries are live.
    unsafe {
        slab.free(items[1]);
        slab.free(items[2]);
    }
    // Pin one entry of the last page: that page cannot be reclaimed.
    let pinned = items[3 * per_page - 1];
    // SAFETY: as above.
    unsafe {
        (*pinned).locked = true;
        slab.lock_item(pinned);
    }
    under_memory_pressure(reclaim_now);
    // The two unpinned pages were evicted with their live entries.
    let evicted = erased.borrow().clone();
    assert_eq!(evicted.len(), 2 * per_page - 2);
    assert!(!evicted.contains(&1) && !evicted.contains(&2));
    assert!(!evicted.contains(&id_of(pinned)));
    // After a reclaim no new page is allocated: a full class evicts instead.
    let free_slots_before = erased.borrow().len();
    let mut created = 0;
    while created < per_page {
        let it = slab.create(96, 10_000 + created as u32).unwrap();
        assert!(!it.is_null());
        created += 1;
    }
    assert!(erased.borrow().len() > free_slots_before, "the full page evicted from its LRU");
    // SAFETY: the pinned entry is still live.
    unsafe {
        (*pinned).locked = false;
        slab.unlock_item(pinned);
    }
});
