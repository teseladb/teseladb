//! A slab allocator with LRU eviction, for caches of variable-size items.
//!
//! [`SlabAllocator`] splits memory into pages of `max_object_size` bytes, each owned by one
//! [`SlabClass`] of fixed-size objects; class sizes grow geometrically by a growth factor. Items
//! are created in the smallest class that fits. Each class keeps its items in an LRU list; when a
//! class has no free object and no page can be allocated, its least recently used unlocked item is
//! evicted (through the optional erase callback) and its slot reused.
//!
//! With a limit of 0 the allocator grows without bound and registers a memory reclaimer: under
//! memory pressure it frees the least recently used page that holds no locked item, and from then
//! on stops allocating pages. It also exports allocation metrics under the `slab` group.
//!
//! The lists are [`IntrusiveList`]s: null-terminated doubly linked lists over [`ListHook`]s
//! embedded in the elements, so linking never allocates. Everything here is single-shard.

use std::ffi::c_void;

use crate::engine::core::future::{BadAlloc, ExceptionPtr, make_exception_ptr};
use crate::engine::core::memory::{Reclaimer, ReclaimerScope, ReclaimingResult};
use crate::engine::core::metrics;

unsafe extern "C" {
    fn aligned_alloc(alignment: usize, size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
}

/// Value of a page descriptor's magic field while it is valid; checked on every item lookup.
pub const SLAB_MAGIC_NUMBER: u16 = 0x51AB; // reads as "SLAB"

/// The link embedded in each element of an [`IntrusiveList`]: two pointers, 16 bytes.
#[repr(C)]
#[derive(Debug)]
pub struct ListHook {
    prev: *mut ListHook,
    next: *mut ListHook,
}

impl Default for ListHook {
    fn default() -> Self {
        Self { prev: std::ptr::null_mut(), next: std::ptr::null_mut() }
    }
}

/// An intrusive doubly linked list of `T`s linked through the hook at byte offset `OFFSET`.
pub struct IntrusiveList<T, const OFFSET: usize> {
    head: *mut ListHook,
    tail: *mut ListHook,
    _t: std::marker::PhantomData<*mut T>,
}

impl<T, const OFFSET: usize> Default for IntrusiveList<T, OFFSET> {
    fn default() -> Self {
        Self {
            head: std::ptr::null_mut(),
            tail: std::ptr::null_mut(),
            _t: std::marker::PhantomData,
        }
    }
}

impl<T, const OFFSET: usize> IntrusiveList<T, OFFSET> {
    #[inline(always)]
    fn hook(t: *mut T) -> *mut ListHook {
        (t as *mut u8).wrapping_add(OFFSET) as *mut ListHook
    }

    #[inline(always)]
    fn elem(h: *mut ListHook) -> *mut T {
        (h as *mut u8).wrapping_sub(OFFSET) as *mut T
    }

    /// Whether the list has no element.
    #[inline(always)]
    pub fn empty(&self) -> bool {
        self.head.is_null()
    }

    /// The last element (the list must not be empty).
    #[inline(always)]
    pub fn back(&self) -> *mut T {
        Self::elem(self.tail)
    }

    /// Links `t` first.
    ///
    /// # Safety
    /// `t` must be alive, unlinked from this list's hook, and outlive its membership.
    #[inline]
    pub unsafe fn push_front(&mut self, t: *mut T) {
        let h = Self::hook(t);
        // SAFETY: forwarded to the caller; linked hooks are alive.
        unsafe {
            (*h).prev = std::ptr::null_mut();
            (*h).next = self.head;
            if self.head.is_null() {
                self.tail = h;
            } else {
                (*self.head).prev = h;
            }
        }
        self.head = h;
    }

    /// Links `t` last.
    ///
    /// # Safety
    /// As for `push_front`.
    #[inline]
    pub unsafe fn push_back(&mut self, t: *mut T) {
        let h = Self::hook(t);
        // SAFETY: forwarded to the caller; linked hooks are alive.
        unsafe {
            (*h).next = std::ptr::null_mut();
            (*h).prev = self.tail;
            if self.tail.is_null() {
                self.head = h;
            } else {
                (*self.tail).next = h;
            }
        }
        self.tail = h;
    }

    /// Unlinks `t`. O(1).
    ///
    /// # Safety
    /// `t` must be linked in this list.
    #[inline]
    pub unsafe fn erase(&mut self, t: *mut T) {
        let h = Self::hook(t);
        // SAFETY: forwarded to the caller; its neighbours are linked and alive.
        unsafe {
            if (*h).prev.is_null() {
                self.head = (*h).next;
            } else {
                (*(*h).prev).next = (*h).next;
            }
            if (*h).next.is_null() {
                self.tail = (*h).prev;
            } else {
                (*(*h).next).prev = (*h).prev;
            }
            (*h).prev = std::ptr::null_mut();
            (*h).next = std::ptr::null_mut();
        }
    }

    /// Unlinks every element without touching them otherwise.
    #[inline]
    pub fn clear(&mut self) {
        self.head = std::ptr::null_mut();
        self.tail = std::ptr::null_mut();
    }
}

/// Descriptor of one slab page (one per page, 80 bytes): its links in the allocator's LRU of pages
/// and in its class's list of pages with free objects, the page memory, the addresses of its free
/// objects, and how many of its items are locked.
#[repr(C)]
pub struct SlabPageDesc {
    _lru_link: ListHook,
    _free_pages_link: ListHook,
    _slab_page: *mut c_void,
    _free_objects: Vec<usize>,
    _refcnt: u32,
    _index: u32, // index in the allocator's page vector
    _magic: u16,
    _slab_class_id: u8,
}

const LRU_LINK_OFFSET: usize = std::mem::offset_of!(SlabPageDesc, _lru_link);
const FREE_PAGES_LINK_OFFSET: usize = std::mem::offset_of!(SlabPageDesc, _free_pages_link);

impl SlabPageDesc {
    /// Describes a new slab page of `objects` objects of `object_size` bytes; the first object
    /// is handed out by the caller, the others are free.
    pub fn new(
        slab_page: *mut c_void,
        objects: usize,
        object_size: usize,
        slab_class_id: u8,
        index: u32,
    ) -> Self {
        let mut free_objects = Vec::with_capacity(objects.saturating_sub(1));
        let mut object = slab_page as usize;
        for _ in 1..objects {
            object += object_size;
            free_objects.push(object);
        }
        Self {
            _lru_link: ListHook::default(),
            _free_pages_link: ListHook::default(),
            _slab_page: slab_page,
            _free_objects: free_objects,
            _refcnt: 0,
            _index: index,
            _magic: SLAB_MAGIC_NUMBER,
            _slab_class_id: slab_class_id,
        }
    }

    /// Whether no object is free.
    #[inline]
    pub fn empty(&self) -> bool {
        self._free_objects.is_empty()
    }

    /// Number of free objects.
    #[inline]
    pub fn size(&self) -> usize {
        self._free_objects.len()
    }

    /// Lock count of the page.
    #[inline]
    pub fn refcnt(&mut self) -> &mut u32 {
        &mut self._refcnt
    }

    /// Index in the slab page vector.
    #[inline]
    pub fn index(&self) -> u32 {
        self._index
    }

    /// `SLAB_MAGIC_NUMBER` while valid.
    #[inline]
    pub fn magic(&self) -> u16 {
        self._magic
    }

    /// The class the page belongs to.
    #[inline]
    pub fn slab_class_id(&self) -> u8 {
        self._slab_class_id
    }

    /// The page memory.
    #[inline]
    pub fn slab_page(&self) -> *mut c_void {
        self._slab_page
    }

    /// Addresses of the free objects.
    #[inline]
    pub fn free_objects(&mut self) -> &mut Vec<usize> {
        &mut self._free_objects
    }

    /// Takes a free object.
    #[inline]
    pub fn allocate_object(&mut self) -> *mut c_void {
        assert!(!self._free_objects.is_empty());
        match self._free_objects.pop() {
            Some(o) => o as *mut c_void,
            None => std::ptr::null_mut(),
        }
    }

    /// Returns `object` to the page.
    #[inline]
    pub fn free_object(&mut self, object: *mut c_void) {
        self._free_objects.push(object as usize);
    }
}

/// The base every slab item embeds as its first field.
#[repr(C)]
#[derive(Debug, Default)]
pub struct SlabItemBase {
    _lru_link: ListHook,
}

/// An item stored in a slab.
///
/// The item records the index of its page (given to [`construct`](Self::construct)) and says
/// whether it may be evicted.
///
/// # Safety
/// The type must be `#[repr(C)]` with a [`SlabItemBase`] as its first field: the allocator links
/// items into the LRU through a hook at offset 0.
pub unsafe trait SlabItem: Sized {
    /// Construction arguments besides the page index.
    type Args;
    /// Builds the item that will live in page `slab_page_index`.
    fn construct(slab_page_index: u32, args: Self::Args) -> Self;
    /// The index of the page holding the item.
    fn get_slab_page_index(&self) -> u32;
    /// Whether the item can be evicted (it is not in use).
    fn is_unlocked(&self) -> bool;
}

/// Optional callback run on an item that is being evicted, to remove it from the cache that
/// references it. It must not free the item back to the slab.
pub type EraseFunc<Item> = Option<Box<dyn FnMut(&mut Item)>>;

/// The objects of one size: the pages with free objects and the LRU of live items.
pub struct SlabClass<Item: SlabItem> {
    _free_slab_pages: IntrusiveList<SlabPageDesc, FREE_PAGES_LINK_OFFSET>,
    _lru: IntrusiveList<Item, 0>,
    _size: usize, // object size in bytes
    _slab_class_id: u8,
}

impl<Item: SlabItem> SlabClass<Item> {
    /// Constructs an item in `object` and links it at the front of the LRU.
    #[inline]
    fn create_item(
        &mut self,
        object: *mut c_void,
        slab_page_index: u32,
        args: Item::Args,
    ) -> *mut Item {
        let new_item = object as *mut Item;
        // SAFETY: `object` is a free, suitably aligned slot of `_size >= size_of::<Item>()` bytes.
        unsafe {
            new_item.write(Item::construct(slab_page_index, args));
            self._lru.push_front(new_item);
        }
        new_item
    }

    /// Unlinks the least recently used item, runs `erase_func` on it, and returns its slot and page
    /// index; a null slot if the LRU is empty.
    ///
    /// # Panics
    /// Panics if the least recently used item is locked.
    #[inline]
    fn evict_lru_item(&mut self, erase_func: &mut EraseFunc<Item>) -> (*mut c_void, u32) {
        if self._lru.empty() {
            return (std::ptr::null_mut(), 0);
        }
        let victim = self._lru.back();
        // SAFETY: items in the LRU are alive.
        let index = unsafe { (*victim).get_slab_page_index() };
        // SAFETY: as above.
        assert!(unsafe { (*victim).is_unlocked() });
        // SAFETY: `victim` is linked in the LRU.
        unsafe { self._lru.erase(victim) };
        // erase_func must not release the victim back to the slab: its slot is reused below.
        if let Some(f) = erase_func {
            // SAFETY: the victim is alive until the slot is reused.
            f(unsafe { &mut *victim });
        }
        (victim as *mut c_void, index)
    }

    /// A class of `size`-byte objects.
    pub fn new(size: usize, slab_class_id: u8) -> Self {
        Self {
            _free_slab_pages: IntrusiveList::default(),
            _lru: IntrusiveList::default(),
            _size: size,
            _slab_class_id: slab_class_id,
        }
    }

    /// Object size.
    #[inline]
    pub fn size(&self) -> usize {
        self._size
    }

    /// Whether no page has a free object.
    #[inline]
    pub fn empty(&self) -> bool {
        self._free_slab_pages.empty()
    }

    /// Whether no item is in the LRU.
    #[inline]
    pub fn has_no_slab_pages(&self) -> bool {
        self._lru.empty()
    }

    /// Creates an item in a page with free objects.
    pub fn create(&mut self, args: Item::Args) -> *mut Item {
        assert!(!self._free_slab_pages.empty());
        let desc = self._free_slab_pages.back();
        // SAFETY: pages in the free list are alive.
        let (object, index, now_empty) = unsafe {
            let o = (*desc).allocate_object();
            (o, (*desc).index(), (*desc).empty())
        };
        if now_empty {
            // if empty, remove desc from the list of slab pages with free objects.
            // SAFETY: `desc` is linked in the free list.
            unsafe { self._free_slab_pages.erase(desc) };
        }
        self.create_item(object, index, args)
    }

    /// Allocates a new page and creates the item in its first object.
    pub fn create_from_new_page(
        &mut self,
        max_object_size: u64,
        slab_page_index: u32,
        insert_slab_page_desc: &mut dyn FnMut(*mut SlabPageDesc),
        args: Item::Args,
    ) -> Result<*mut Item, ExceptionPtr> {
        // allocate slab page.
        let alignment = std::mem::align_of::<Item>();
        // SAFETY: aligned_alloc with a power-of-two alignment.
        let slab_page = unsafe { aligned_alloc(alignment, max_object_size as usize) };
        if slab_page.is_null() {
            return Err(make_exception_ptr(BadAlloc));
        }
        // allocate descriptor to slab page.
        assert!(self._size.is_multiple_of(alignment));
        let objects = max_object_size as usize / self._size;
        let desc = Box::into_raw(Box::new(SlabPageDesc::new(
            slab_page,
            objects,
            self._size,
            self._slab_class_id,
            slab_page_index,
        )));
        // Only a page with objects left after the first one goes on the list of pages with
        // free objects; with one object per page, free_item() links the page when that
        // object comes back.
        // SAFETY: the descriptor is heap-allocated and lives until the page is freed.
        unsafe {
            if !(*desc).empty() {
                self._free_slab_pages.push_front(desc);
            }
        }
        insert_slab_page_desc(desc);
        // first object from the allocated slab page is returned.
        Ok(self.create_item(slab_page, slab_page_index, args))
    }

    /// Evicts the least recently used item and creates the new one in its slot.
    pub fn create_from_lru(
        &mut self,
        erase_func: &mut EraseFunc<Item>,
        args: Item::Args,
    ) -> Result<*mut Item, ExceptionPtr> {
        let ret = self.evict_lru_item(erase_func);
        if ret.0.is_null() {
            return Err(make_exception_ptr(BadAlloc));
        }
        Ok(self.create_item(ret.0, ret.1, args))
    }

    /// Returns `item` to its page.
    ///
    /// # Safety
    /// `item` is a live item of this class, linked in its LRU, and `desc` is its page's descriptor.
    pub unsafe fn free_item(&mut self, item: *mut Item, desc: *mut SlabPageDesc) {
        let object = item as *mut c_void;
        // SAFETY: guaranteed by the caller.
        unsafe {
            self._lru.erase(item);
            (*desc).free_object(object);
            if (*desc).size() == 1 {
                // push back desc into the list of slab pages with free objects.
                self._free_slab_pages.push_back(desc);
            }
        }
    }

    /// Moves `item` to the front of the LRU.
    ///
    /// # Safety
    /// `item` is a live item of this class, linked in its LRU.
    pub unsafe fn touch_item(&mut self, item: *mut Item) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            self._lru.erase(item);
            self._lru.push_front(item);
        }
    }

    /// Unlinks `item` from the LRU.
    ///
    /// # Safety
    /// `item` is a live item of this class, linked in its LRU.
    pub unsafe fn remove_item_from_lru(&mut self, item: *mut Item) {
        // SAFETY: guaranteed by the caller.
        unsafe { self._lru.erase(item) };
    }

    /// Links `item` at the front of the LRU.
    ///
    /// # Safety
    /// `item` is a live item of this class, not linked in its LRU.
    pub unsafe fn insert_item_into_lru(&mut self, item: *mut Item) {
        // SAFETY: guaranteed by the caller.
        unsafe { self._lru.push_front(item) };
    }

    /// Unlinks `desc` from the pages with free objects.
    ///
    /// # Safety
    /// `desc` is a live descriptor of this class, linked in its free list.
    pub unsafe fn remove_desc_from_free_list(&mut self, desc: *mut SlabPageDesc) {
        // SAFETY: guaranteed by the caller.
        assert!(unsafe { (*desc).slab_class_id() } == self._slab_class_id);
        // SAFETY: guaranteed by the caller.
        unsafe { self._free_slab_pages.erase(desc) };
    }
}

impl<Item: SlabItem> Drop for SlabClass<Item> {
    fn drop(&mut self) {
        self._free_slab_pages.clear();
        self._lru.clear();
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct CollectdStats {
    allocs: u64,
    frees: u64,
}

/// A slab allocator of `Item`s in size classes growing by `growth_factor`.
///
/// It registers a reclaimer and metrics pointing back at itself, so it is built in place on
/// the heap.
pub struct SlabAllocator<Item: SlabItem + 'static> {
    _slab_class_sizes: Vec<usize>,
    _slab_classes: Vec<SlabClass<Item>>,
    _metrics: metrics::MetricGroups,
    // Removes an evicted item from the cache that uses the slab.
    _erase_func: EraseFunc<Item>,
    _slab_pages_vector: Vec<*mut SlabPageDesc>,
    _slab_page_desc_lru: IntrusiveList<SlabPageDesc, LRU_LINK_OFFSET>,
    _max_object_size: u64,
    _available_slab_pages: u64,
    _stats: CollectdStats,
    _reclaimer: Option<Box<Reclaimer>>,
    _reclaimed: bool,
}

impl<Item: SlabItem + 'static> SlabAllocator<Item> {
    /// Frees the least recently used page of the allocator, erasing every live item in it;
    /// returns whether memory was reclaimed.
    fn evict_lru_slab_page(&mut self) -> ReclaimingResult {
        if self._slab_page_desc_lru.empty() {
            // NOTE: Nothing to evict. If this happens, it implies that all
            // slab pages in the slab are being used at the same time.
            // That being said, this event is very unlikely to happen.
            return ReclaimingResult::ReclaimedNothing;
        }
        // get descriptor of the least-recently-used slab page and related info.
        let desc_ptr = self._slab_page_desc_lru.back();
        // SAFETY: descriptors in the LRU are alive until freed below.
        let desc = unsafe { &mut *desc_ptr };
        assert!(*desc.refcnt() == 0);
        let slab_class_id = desc.slab_class_id();
        let slab_class: *mut SlabClass<Item> = self.get_slab_class_by_id(slab_class_id);
        let slab_page = desc.slab_page();
        if !desc.empty() {
            // if not empty, remove desc from the list of slab pages with free objects.
            // SAFETY: the class lives in `_slab_classes`, which is not resized here; a non-empty
            // page of the class is linked in its free list.
            unsafe { (*slab_class).remove_desc_from_free_list(desc_ptr) };
            // and sort the array of free objects for binary search later on.
            desc.free_objects().sort_unstable();
        }
        // remove desc from the list of slab page descriptors.
        // SAFETY: `desc` is linked in the LRU.
        unsafe { self._slab_page_desc_lru.erase(desc_ptr) };
        // remove desc from the slab page vector.
        self._slab_pages_vector[desc.index() as usize] = std::ptr::null_mut();
        // Iterate through objects in the slab page and if the object is an allocated
        // item, the item should be removed from LRU and then erased.
        let mut object = slab_page as usize;
        // SAFETY: as above.
        let object_size = unsafe { (*slab_class).size() };
        let objects = self._max_object_size as usize / object_size;
        for _ in 0..objects {
            let is_free = !desc.empty() && desc.free_objects().binary_search(&object).is_ok();
            // if binary_search returns true, it means that object at the current
            // offset isn't an item.
            if !is_free {
                let item = object as *mut Item;
                // SAFETY: allocated slots of the page hold live items.
                assert!(unsafe { (*item).is_unlocked() });
                // SAFETY: as above.
                unsafe { (*slab_class).remove_item_from_lru(item) };
                if let Some(f) = &mut self._erase_func {
                    // SAFETY: as above.
                    f(unsafe { &mut *item });
                }
                self._stats.frees += 1;
            }
            object += object_size;
        }
        #[cfg(engine_debug)]
        println!("lru slab page eviction succeeded! desc_empty?={}", desc.empty() as i32);
        // SAFETY: the page and its descriptor are no longer referenced.
        unsafe {
            free(slab_page); // free slab page object
            drop(Box::from_raw(desc_ptr)); // free its descriptor
        }
        ReclaimingResult::ReclaimedSomething
    }

    /// Frees the least recently used page that holds no locked item; called by the memory
    /// reclaimer. From then on no new pages are allocated.
    fn reclaim(&mut self) -> ReclaimingResult {
        // Once the reclaimer has run, no more pages are allocated: the memory used by the slab is
        // considered calibrated.
        self._reclaimed = true;
        // One page is evicted per call.
        self.evict_lru_slab_page()
    }

    /// Builds the size classes, from 96 bytes growing by `growth_factor` (aligned for `Item`) up
    /// to `max_object_size`, and sets up the reclaimer when `limit` is 0.
    ///
    /// # Panics
    /// Panics if more than 255 classes would be needed.
    fn initialize_slab_allocator(&mut self, growth_factor: f64, limit: u64) {
        let alignment = std::mem::align_of::<Item>();
        const INITIAL_SIZE: usize = 96;
        let mut size = INITIAL_SIZE; // initial object size
        let mut slab_class_id: u8 = 0;
        while self._max_object_size as usize / size > 1 {
            size = crate::engine::core::align::align_up(size, alignment);
            self._slab_class_sizes.push(size);
            self._slab_classes.push(SlabClass::new(size, slab_class_id));
            size = (size as f64 * growth_factor) as usize;
            assert!(slab_class_id < u8::MAX);
            slab_class_id += 1;
        }
        self._slab_class_sizes.push(self._max_object_size as usize);
        self._slab_classes.push(SlabClass::new(self._max_object_size as usize, slab_class_id));
        // An unlimited slab is reclaimable under memory pressure.
        if limit == 0 {
            let this = self as *mut Self as usize;
            self._reclaimer = Some(Reclaimer::new(
                Box::new(move || {
                    // SAFETY: the reclaimer is dropped with the allocator, which never moves (boxed).
                    unsafe { (*(this as *mut Self)).reclaim() }
                }),
                ReclaimerScope::Async,
            ));
        } else {
            self._slab_pages_vector.reserve(self._available_slab_pages as usize);
        }
    }

    /// Returns the smallest class whose objects hold `size` bytes. O(log n).
    fn get_slab_class_by_size(&mut self, size: usize) -> Option<&mut SlabClass<Item>> {
        let i = self._slab_class_sizes.partition_point(|&s| s < size);
        if i == self._slab_class_sizes.len() {
            return None;
        }
        Some(&mut self._slab_classes[i])
    }

    /// Returns the class with id `slab_class_id`.
    ///
    /// # Panics
    /// Panics if the id is out of range.
    fn get_slab_class_by_id(&mut self, slab_class_id: u8) -> &mut SlabClass<Item> {
        assert!((slab_class_id as usize) < self._slab_classes.len());
        &mut self._slab_classes[slab_class_id as usize]
    }

    /// Registers the allocation metrics of the `slab` group.
    fn register_metrics(&mut self) -> Result<(), ExceptionPtr> {
        let this = self as *const Self as usize;
        // SAFETY: the metrics are unregistered with the allocator, which never moves (boxed).
        let stats = move || unsafe { (*(this as *const Self))._stats };
        let mut group = metrics::MetricGroups::new();
        group.add_group(
            "slab",
            vec![
                metrics::make_counter_fn(
                    "malloc_total_operations",
                    metrics::Description::new("Total number of slab malloc operations"),
                    move || stats().allocs,
                ),
                metrics::make_counter_fn(
                    "free_total_operations",
                    metrics::Description::new("Total number of slab free operations"),
                    move || stats().frees,
                ),
                metrics::make_gauge_fn(
                    "malloc_objects",
                    metrics::Description::new("Number of slab created objects currently in memory"),
                    move || {
                        let s = stats();
                        (s.allocs - s.frees) as f64
                    },
                ),
            ],
        )?;
        self._metrics = group;
        Ok(())
    }

    /// Returns the descriptor of the page that holds `item`.
    ///
    /// # Panics
    /// Panics if the page index is stale or the descriptor's magic number is wrong.
    #[inline]
    fn get_slab_page_desc(&self, item: *mut Item) -> *mut SlabPageDesc {
        // SAFETY: `item` is a live item of this allocator.
        let desc = self._slab_pages_vector[unsafe { (*item).get_slab_page_index() } as usize];
        assert!(!desc.is_null());
        // SAFETY: non-null entries are live descriptors.
        assert!(unsafe { (*desc).magic() } == SLAB_MAGIC_NUMBER);
        desc
    }

    /// Whether a new page may be allocated: always before the reclaimer has run, otherwise while
    /// the limit leaves pages (also for a class that has no page yet: the limit holds for all
    /// classes together).
    #[inline]
    fn can_allocate_page(&self) -> bool {
        (self._reclaimer.is_some() && !self._reclaimed) || self._available_slab_pages > 0
    }

    /// Creates a slab of at most `limit` bytes (0: unlimited and reclaimable) with pages of
    /// `max_object_size` bytes and class sizes growing by `growth_factor`.
    ///
    /// Boxed, because the reclaimer and the metrics point back at it.
    ///
    /// # Errors
    /// Fails if the metrics cannot be registered.
    pub fn new(
        growth_factor: f64,
        limit: u64,
        max_object_size: u64,
    ) -> Result<Box<Self>, ExceptionPtr> {
        Self::with_erase_func(growth_factor, limit, max_object_size, None)
    }

    /// Like [`new`](Self::new), with the callback that removes an evicted item from its cache.
    ///
    /// # Errors
    /// Fails if the metrics cannot be registered.
    pub fn with_erase_func(
        growth_factor: f64,
        limit: u64,
        max_object_size: u64,
        erase_func: EraseFunc<Item>,
    ) -> Result<Box<Self>, ExceptionPtr> {
        let mut this = Box::new(Self {
            _slab_class_sizes: Vec::new(),
            _slab_classes: Vec::new(),
            _metrics: metrics::MetricGroups::new(),
            _erase_func: erase_func,
            _slab_pages_vector: Vec::new(),
            _slab_page_desc_lru: IntrusiveList::default(),
            _max_object_size: max_object_size,
            _available_slab_pages: limit / max_object_size,
            _stats: CollectdStats::default(),
            _reclaimer: None,
            _reclaimed: false,
        });
        this.initialize_slab_allocator(growth_factor, limit);
        this.register_metrics()?;
        Ok(this)
    }

    /// Creates an item of `size` bytes in the smallest class that fits.
    ///
    /// Uses a free object if the class has one, else a new page if allowed, else evicts the class's
    /// least recently used item when an erase callback is set.
    ///
    /// # Errors
    /// Fails with a bad-allocation error if no class fits `size` or no slot can be found, and
    /// with the allocation error if a new page cannot be allocated.
    pub fn create(&mut self, size: usize, args: Item::Args) -> Result<*mut Item, ExceptionPtr> {
        let Some(slab_class) = self.get_slab_class_by_size(size) else {
            return Err(make_exception_ptr(BadAlloc));
        };
        let slab_class: *mut SlabClass<Item> = slab_class;
        // SAFETY: `slab_class` points into the heap buffer of `_slab_classes`, which is not
        // resized while creating; the fields used below are other members of the allocator,
        // reached through disjoint field borrows, so no access overlaps the class.
        let sc = unsafe { &mut *slab_class };
        let mut item: *mut Item = std::ptr::null_mut();
        if !sc.empty() {
            item = sc.create(args);
            self._stats.allocs += 1;
        } else if self.can_allocate_page() {
            let index_to_insert = self._slab_pages_vector.len() as u32;
            let max = self._max_object_size;
            let has_reclaimer = self._reclaimer.is_some();
            let slab_page_desc_lru = &mut self._slab_page_desc_lru;
            let slab_pages_vector = &mut self._slab_pages_vector;
            item = sc.create_from_new_page(
                max,
                index_to_insert,
                &mut |desc: *mut SlabPageDesc| {
                    if has_reclaimer {
                        // Link the descriptor into the LRU of page descriptors.
                        // SAFETY: the descriptor is alive and unlinked from the LRU.
                        unsafe { slab_page_desc_lru.push_front(desc) };
                    }
                    // Record the descriptor in the page vector.
                    slab_pages_vector.push(desc);
                },
                args,
            )?;
            if self._available_slab_pages > 0 {
                self._available_slab_pages -= 1;
            }
            self._stats.allocs += 1;
        } else if self._erase_func.is_some() {
            item = sc.create_from_lru(&mut self._erase_func, args)?;
        }
        Ok(item)
    }

    /// Pins `item` (its page cannot be reclaimed).
    ///
    /// # Safety
    /// `item` is a live, unlocked item of this allocator.
    pub unsafe fn lock_item(&mut self, item: *mut Item) {
        let desc = self.get_slab_page_desc(item);
        if self._reclaimer.is_some() {
            // SAFETY: `desc` is the live descriptor of the item's page.
            let refcnt = unsafe { (*desc).refcnt() };
            *refcnt += 1;
            if *refcnt == 1 {
                // remove slab page descriptor from list of slab page descriptors.
                // SAFETY: an unlocked page is linked in the LRU.
                unsafe { self._slab_page_desc_lru.erase(desc) };
            }
        }
        // remove item from the lru of its slab class.
        // SAFETY: as above.
        let id = unsafe { (*desc).slab_class_id() };
        // SAFETY: the item is live and linked in the LRU of the class of its page.
        unsafe { self.get_slab_class_by_id(id).remove_item_from_lru(item) };
    }

    /// Unpins `item`.
    ///
    /// # Safety
    /// `item` is a live item of this allocator, locked by `lock_item`.
    pub unsafe fn unlock_item(&mut self, item: *mut Item) {
        let desc = self.get_slab_page_desc(item);
        if self._reclaimer.is_some() {
            // SAFETY: `desc` is the live descriptor of the item's page.
            let refcnt = unsafe { (*desc).refcnt() };
            *refcnt -= 1;
            if *refcnt == 0 {
                // insert slab page descriptor back into list of slab page descriptors.
                // SAFETY: a locked page is not linked in the LRU.
                unsafe { self._slab_page_desc_lru.push_front(desc) };
            }
        }
        // insert item into the lru of its slab class.
        // SAFETY: as above.
        let id = unsafe { (*desc).slab_class_id() };
        // SAFETY: the item is live and was unlinked by lock_item.
        unsafe { self.get_slab_class_by_id(id).insert_item_into_lru(item) };
    }

    /// Destroys `item` and returns its slot to its page and class. Does nothing for null.
    ///
    /// # Safety
    /// `item` is null or a live, unlocked item of this allocator, not used afterwards.
    pub unsafe fn free(&mut self, item: *mut Item) {
        if !item.is_null() {
            let desc = self.get_slab_page_desc(item);
            // SAFETY: `desc` is the live descriptor of the item's page.
            let id = unsafe { (*desc).slab_class_id() };
            // SAFETY: the item is live, in the LRU of the class of its page, and `desc` is that page's.
            unsafe { self.get_slab_class_by_id(id).free_item(item, desc) };
            self._stats.frees += 1;
        }
    }

    /// Marks `item` as most recently used in its class's LRU. Does nothing for null.
    ///
    /// # Safety
    /// `item` is null or a live, unlocked item of this allocator.
    pub unsafe fn touch(&mut self, item: *mut Item) {
        if !item.is_null() {
            let desc = self.get_slab_page_desc(item);
            // SAFETY: `desc` is the live descriptor of the item's page.
            let id = unsafe { (*desc).slab_class_id() };
            // SAFETY: the item is live and linked in the LRU of the class of its page.
            unsafe { self.get_slab_class_by_id(id).touch_item(item) };
        }
    }

    /// Prints every slab class and its object size to standard output.
    pub fn print_slab_classes(&self) {
        for (class_id, slab_class) in self._slab_classes.iter().enumerate() {
            let size = slab_class.size();
            println!(
                "slab[{:3}]\tsize: {:10}\tper-slab-page: {:5}",
                class_id,
                size,
                self._max_object_size as usize / size
            );
        }
    }

    /// Returns the object size of the class that `size` falls in, or 0 if none fits.
    pub fn class_size(&mut self, size: usize) -> usize {
        self.get_slab_class_by_size(size).map_or(0, |sc| sc.size())
    }
}

impl<Item: SlabItem + 'static> Drop for SlabAllocator<Item> {
    fn drop(&mut self) {
        self._slab_classes.clear();
        self._slab_page_desc_lru.clear();
        for &desc in &self._slab_pages_vector {
            if desc.is_null() {
                continue;
            }
            // SAFETY: non-null entries own their page and descriptor.
            unsafe {
                free((*desc).slab_page());
                drop(Box::from_raw(desc));
            }
        }
        self._reclaimer = None;
    }
}
