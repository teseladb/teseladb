//! Tests of `SlabAllocator`: allocation and freeing over a range of growth factors and slab
//! size limits, and eviction of the least recently used items when the limit is reached.
//! A program with its own `main` (`harness = false`).

// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::ptr;

use teseladb::engine::core::slab::{SlabAllocator, SlabItem, SlabItemBase};

const MAX_OBJECT_SIZE: usize = 1024 * 1024;

/// The links of an item in the LRU list.
#[repr(C)]
struct CacheLink {
    prev: *mut Item,
    next: *mut Item,
}

#[repr(C)]
struct Item {
    base: SlabItemBase,
    cache_link: CacheLink,
    slab_page_index: u32,
}

// SAFETY: `Item` is repr(C) with its `SlabItemBase` first.
unsafe impl SlabItem for Item {
    type Args = ();

    fn construct(slab_page_index: u32, _args: ()) -> Self {
        Item {
            base: SlabItemBase::default(),
            cache_link: CacheLink { prev: ptr::null_mut(), next: ptr::null_mut() },
            slab_page_index,
        }
    }

    fn get_slab_page_index(&self) -> u32 {
        self.slab_page_index
    }

    fn is_unlocked(&self) -> bool {
        true
    }
}

/// An intrusive doubly linked LRU list of items.
struct Cache {
    head: *mut Item,
    tail: *mut Item,
}

impl Cache {
    fn new() -> Self {
        Cache { head: ptr::null_mut(), tail: ptr::null_mut() }
    }

    /// # Safety
    /// `it` must be a live item not in any list.
    unsafe fn push_front(&mut self, it: *mut Item) {
        // SAFETY: see the function contract; `head` is null or a live member.
        unsafe {
            (*it).cache_link = CacheLink { prev: ptr::null_mut(), next: self.head };
            if self.head.is_null() {
                self.tail = it;
            } else {
                (*self.head).cache_link.prev = it;
            }
        }
        self.head = it;
    }

    /// Unlinks `it` from the list.
    ///
    /// # Safety
    /// `it` must be a member of this list.
    unsafe fn erase(&mut self, it: *mut Item) {
        // SAFETY: see the function contract; neighbours are live members.
        unsafe {
            let CacheLink { prev, next } = (*it).cache_link;
            if prev.is_null() {
                self.head = next;
            } else {
                (*prev).cache_link.next = next;
            }
            if next.is_null() {
                self.tail = prev;
            } else {
                (*next).cache_link.prev = prev;
            }
            (*it).cache_link = CacheLink { prev: ptr::null_mut(), next: ptr::null_mut() };
        }
    }

    fn clear(&mut self) {
        self.head = ptr::null_mut();
        self.tail = ptr::null_mut();
    }
}

fn free_vector(slab: &mut SlabAllocator<Item>, items: &[*mut Item]) {
    for &item in items {
        // SAFETY: the items were created by `slab` and are freed once.
        unsafe { slab.free(item) };
    }
}

fn test_allocation_1(growth_factor: f64, slab_limit_size: u32) {
    let mut slab = SlabAllocator::<Item>::new(
        growth_factor,
        u64::from(slab_limit_size),
        MAX_OBJECT_SIZE as u64,
    )
    .expect("slab_allocator");
    let size = MAX_OBJECT_SIZE;

    slab.print_slab_classes();

    let mut items: Vec<*mut Item> = Vec::new();

    assert!((slab_limit_size as usize).is_multiple_of(size));
    for _ in 0..(slab_limit_size as usize / size) {
        let item = slab.create(size, ()).expect("create");
        items.push(item);
    }
    assert!(slab.create(size, ()).expect("create").is_null());

    free_vector(&mut slab, &items);
    println!("test_allocation_1 done!");
}

fn test_allocation_2(growth_factor: f64, slab_limit_size: u32) {
    let mut slab = SlabAllocator::<Item>::new(
        growth_factor,
        u64::from(slab_limit_size),
        MAX_OBJECT_SIZE as u64,
    )
    .expect("slab_allocator");
    let size = 1024usize;

    let mut items: Vec<*mut Item> = Vec::new();

    let mut allocations = 0usize;
    loop {
        let item = slab.create(size, ()).expect("create");
        if item.is_null() {
            break;
        }
        items.push(item);
        allocations += 1;
    }

    let class_size = slab.class_size(size);
    let per_slab_page = MAX_OBJECT_SIZE / class_size;
    let available_slab_pages = slab_limit_size as usize / MAX_OBJECT_SIZE;
    assert!(allocations == per_slab_page * available_slab_pages);

    free_vector(&mut slab, &items);
    println!("test_allocation_2 done!");
}

fn test_allocation_with_lru(growth_factor: f64, slab_limit_size: u32) {
    let cache: *mut Cache = Box::into_raw(Box::new(Cache::new()));
    let evictions: *mut u32 = Box::into_raw(Box::new(0u32));

    let mut slab = SlabAllocator::<Item>::with_erase_func(
        growth_factor,
        u64::from(slab_limit_size),
        MAX_OBJECT_SIZE as u64,
        Some(Box::new(move |item_ref: &mut Item| {
            // SAFETY: the cache and the counter outlive the allocator (freed below).
            unsafe {
                (*cache).erase(item_ref);
                *evictions += 1;
            }
        })),
    )
    .expect("slab_allocator");
    let size = MAX_OBJECT_SIZE;

    let max = slab_limit_size as usize / MAX_OBJECT_SIZE;
    for _ in 0..max * 1000 {
        let item = slab.create(size, ()).expect("create");
        assert!(!item.is_null());
        // SAFETY: `item` was just created and is in no list.
        unsafe { (*cache).push_front(item) };
    }
    // SAFETY: the counter is live.
    assert!(unsafe { *evictions } as usize == max * 999);

    // SAFETY: the cache is live.
    unsafe { (*cache).clear() };
    println!("test_allocation_with_lru done!");
    drop(slab);
    // SAFETY: the allocator, the only other user, is gone.
    unsafe {
        drop(Box::from_raw(cache));
        drop(Box::from_raw(evictions));
    }
}

pub(crate) fn main() {
    test_allocation_1(1.25, 5 * 1024 * 1024);
    test_allocation_2(1.07, 5 * 1024 * 1024); // 1.07 is the growth factor used by facebook.
    test_allocation_with_lru(1.25, 5 * 1024 * 1024);
}
