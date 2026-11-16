#![cfg(target_os = "linux")]
//! A slab class whose objects fill a whole page holds one object per page. Allocating its
//! first object takes a new page, which then has no free object left, so it must not be
//! on the list of pages with free objects: the next allocation of the class must take
//! another new page (or evict), not pop a free object from an empty page.
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
fn page_sized_objects_take_one_page_each() {
    let pages = 3usize;
    let mut slab = SlabAllocator::<Item>::new(1.25, (pages * PAGE) as u64, PAGE as u64)
        .expect("slab allocator");
    assert_eq!(slab.class_size(PAGE), PAGE);

    // Every page holds one object: as many objects as pages fit.
    let mut items = Vec::new();
    for _ in 0..pages {
        let item = slab.create(PAGE, ()).expect("create");
        assert!(!item.is_null());
        assert!(!items.contains(&item), "an object was handed out twice");
        items.push(item);
    }
    // The limit is reached and there is no eviction callback: no more objects.
    assert!(slab.create(PAGE, ()).expect("create").is_null());

    // A freed object can be allocated again, and only once.
    let freed = items.pop().expect("an item");
    // SAFETY: `freed` was created by `slab` and is freed once.
    unsafe { slab.free(freed) };
    let again = slab.create(PAGE, ()).expect("create");
    assert_eq!(again, freed);
    assert!(slab.create(PAGE, ()).expect("create").is_null());
    items.push(again);

    for item in items {
        // SAFETY: each item was created by `slab` and is freed once.
        unsafe { slab.free(item) };
    }
}
