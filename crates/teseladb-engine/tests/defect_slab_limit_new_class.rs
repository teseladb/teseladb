#![cfg(target_os = "linux")]
//! The memory limit of a slab allocator holds for all its classes together: once the pages
//! of the limit are taken, a class that has no page yet cannot take a new one either.
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use teseladb::engine::core::slab::{SlabAllocator, SlabItem, SlabItemBase};

/// The page size of the allocator, which is also its largest object.
const PAGE: usize = 1024 * 1024;

#[repr(C)]
struct Item {
    /// The allocator's link fields, first as the trait requires.
    _base: SlabItemBase,
    slab_page_index: u32,
}

// SAFETY: `Item` is repr(C) with its `SlabItemBase` first.
unsafe impl SlabItem for Item {
    type Args = ();

    fn construct(slab_page_index: u32, _args: ()) -> Self {
        Item { _base: SlabItemBase::default(), slab_page_index }
    }

    fn get_slab_page_index(&self) -> u32 {
        self.slab_page_index
    }

    fn is_unlocked(&self) -> bool {
        true
    }
}

#[test]
fn limit_is_not_violated_by_a_new_class() {
    let limit = 5 * PAGE;
    let mut slab =
        SlabAllocator::<Item>::new(1.25, limit as u64, PAGE as u64).expect("slab allocator");

    // Exhaust the page limit with the largest objects, one per page.
    let mut items = Vec::new();
    for _ in 0..limit / PAGE {
        let item = slab.create(PAGE, ()).expect("create");
        assert!(!item.is_null());
        items.push(item);
    }
    // The limit holds for the same class.
    assert!(slab.create(PAGE, ()).expect("create").is_null());
    // And for a class that has no page yet.
    let leaky_item = slab.create(1024, ()).expect("create");
    assert!(leaky_item.is_null(), "a new class took a page beyond the limit");

    for item in items {
        // SAFETY: each item was created by `slab` and is freed once.
        unsafe { slab.free(item) };
    }
}
