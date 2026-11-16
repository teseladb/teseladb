//! The main entry points of the engine, gathered in one module.
//!
//! The engine is a framework for high-concurrency server applications built on a shared-nothing
//! design: one reactor thread per core (a *shard*), each with its own memory, task queues and I/O,
//! and futures and continuations instead of blocking calls. Shards communicate only by explicit
//! messages (see [`smp`](crate::engine::core::smp) and [`sharded`](crate::engine::core::sharded)).
//!
//! This module re-exports the networking entry points (listen, connect, sockets and datagram
//! channels), the file system API, and process spawning, all implemented by the reactor and the
//! file and network subsystems. It also holds [`intrusive`], the intrusive doubly linked lists the
//! core uses for abort sources, condition variables and timers.
//!
//! Related modules: [`future`](crate::engine::core::future) (futures and promises),
//! [`memory`](crate::engine::core::memory) (the per-shard allocator),
//! [`smp`](crate::engine::core::smp) (multicore support), [`thread`](crate::engine::core::thread)
//! (stackful threads on a reactor), [`rpc`](crate::engine::rpc) (request/response protocols) and
//! [`websocket`](crate::engine::websocket) (an experimental WebSocket server).

// Types named by the signatures below.
pub use crate::engine::core::file::{File, FileOpenOptions, StatData};
pub use crate::engine::core::iostream::{InputStream, OutputStream};
pub use crate::engine::net::api::{
    ConnectedSocket, ListenOptions, ServerSocket, Socket, SocketAddress, Transport,
};
pub use crate::engine::util::process::experimental::{Process, SpawnParameters};

/// Network type aliases.
pub mod net {
    pub use crate::engine::net::api::DatagramChannel;
    /// Older name of [`DatagramChannel`], for UDP use.
    pub type UdpChannel = DatagramChannel;
}

// Networking: the engine offers two TCP/IP stacks behind one API. The POSIX stack uses the
// kernel's sockets; the native stack is implemented inside the engine, drives network cards
// directly, supports zero-copy transmit and receive, and is sharded without locks. The stack is
// chosen with the `--network-stack` command-line option.

/// Networking entry points of the current shard's network stack:
///
/// * `listen(sa)` / `listen_with(sa, opts)`: start listening on an address; return a
///   [`ServerSocket`] ready to accept connections.
/// * `connect(sa)` / `connect_from(sa, local, proto)`: connect to an address (optionally from a
///   local address and with a given transport); the future resolves with a [`ConnectedSocket`] or
///   fails.
/// * `make_socket()`: a [`Socket`] for establishing stream connections.
/// * `make_unbound_datagram_channel(family)`: a datagram channel for sending only, for the address
///   family `AF_INET` or `AF_INET6` (UDP) or `AF_UNIX` (UNIX domain sockets).
/// * `make_bound_datagram_channel(local)`: a datagram channel bound to `local`, for sending and
///   receiving.
/// * `make_udp_channel()` / `make_udp_channel_bound(local)`: older forms of the two above, for UDP.
pub use crate::engine::core::reactor::{
    connect, connect_from, listen, listen_with, make_bound_datagram_channel, make_socket,
    make_udp_channel, make_udp_channel_bound, make_unbound_datagram_channel,
};

// File I/O: the engine's file API is unbuffered (based on `O_DIRECT`). The application does its
// own caching, which performs better when done right. `File` reads, writes, discards and otherwise
// manipulates a file for random or sequential unbuffered I/O; buffered sequential I/O goes through
// the file input and output streams.

/// File system entry points:
///
/// * `open_file_dma(name, flags)` / `open_file_dma(name, flags, options)`
///   (`open_file_dma_with_options`): Opens or creates a file.  The "dma" in the name
///   refers to the fact that data transfers are unbuffered and uncached. The file name
///   is not guaranteed to be stable on disk, unless the containing directory is sync'ed.
/// * `check_direct_io_support(path)`: Checks if a given directory supports direct io.
///   The engine bypasses the operating system caches and issues direct I/O to the
///   underlying block devices, so applications should check that their directories lie on a
///   file system that supports it. The future resolves if direct I/O can be used, and fails with
///   a system error (`EINVAL`) if not, or with the respective system error if `path` is not a
///   directory.
/// * `open_directory(name)`: Opens a directory. The only legal operations are
///   listing it, flushing it, and closing it.
/// * `make_directory(name, permissions)`: Creates a new directory. The directory is not
///   guaranteed to be stable on disk, unless the containing directory is sync'ed.
/// * `touch_directory(name, permissions)`: Ensures a directory exists. Checks whether a
///   directory exists, and if not, creates it.  Only the last component of the directory
///   name is created. If the directory exists, the provided permissions are not applied.
/// * `recursive_touch_directory(name, permissions)`: Recursively ensures a directory
///   exists. This function fsyncs each component created, and is therefore guaranteed
///   to be stable on disk. The provided permissions are applied only on the last
///   component in the path, if it needs to be created, if intermediate directories do
///   not exist, they are created with the default directory permissions. If any directory
///   exists, the provided permissions are not applied.
/// * `sync_directory(name)`: Synchronizes a directory to disk. Makes sure the
///   modifications in a directory are synchronized in disk. This is useful, for
///   instance, after creating or removing a file inside the directory.
/// * `remove_file(name)`: Removes (unlinks) a file or an empty directory. The removal is
///   not guaranteed to be stable on disk, unless the containing directory is sync'ed.
/// * `rename_file(old_name, new_name)`: Renames (moves) a file. The rename is not
///   guaranteed to be stable on disk, unless the both containing directories are sync'ed.
/// * `file_stat(name, fs)`: Return stat information about a file. If name identifies a
///   symbolic link then stat_data is returned either for the target of the link, with
///   follow set, or for the link itself, with follow unset.
/// * `getgrnam(name)`: Wrapper around `getgrnam_r(3)`. Resolves with `None` if the group does
///   not exist in the group database, or with its details if it does; fails with a system error
///   if `getgrnam_r` reports an unexpected error.
/// * `chown(filepath, owner, group)`: Change the owner and group of a file with `chown(2)`; fails
///   with a system error when the call fails.
/// * `file_size(name)`: Return the size of a file. Note that file_size of a symlink is
///   NOT the size of the symlink - which is the length of the pathname it contains - but
///   rather the size of the file to which it points.
/// * `file_accessible(name, flags)`: Check file access. If only existence is queried, resolves
///   with true if the file exists, or false otherwise, and fails with a file system error on any
///   error other than `ENOENT`. If read, write or execute access is queried, resolves with true if
///   the file exists and is accessible with the requested flags, or false if it exists and is not,
///   and fails with a file system error on any error other than `EACCES`. Note that if any path
///   component leading to the file is not searchable, the file is considered
///   inaccessible with the requested mode and false will be returned.
/// * `file_exists(name)`: check if a file exists.
/// * `file_type(name, follow)`: Determine the type of a file (regular file, directory,
///   etc.): `Some` with the file type if the lookup succeeded; `None` if the file (or one of its
///   parent directories) does not exist; a failed future on other errors.
/// * `link_file(oldpath, newpath)`: Creates a hard link for a file.
/// * `chmod(name, permissions)`: Changes the permissions mode of a file or directory.
/// * `file_system_at(name)`: Return information about the filesystem where a file is
///   located.
/// * `fs_avail(name)`: Return space available to unprivileged users in filesystem where
///   a file is located, in bytes.
/// * `fs_free(name)`: Return free space in filesystem where a file is located, in bytes.
/// * `file_system_space(name)`: Return file-system-wide space information where a file is
///   located.
///
/// All of them return futures; paths are relative to the current directory.
pub use crate::engine::core::file::{open_directory, open_file_dma, open_file_dma_with_options};
pub use crate::engine::core::reactor::{check_direct_io_support, file_type};
pub use crate::engine::util::file::{
    chmod, chown, file_accessible, file_exists, file_size, file_stat, file_system_at,
    file_system_space, fs_avail, fs_free, getgrnam, link_file, make_directory,
    recursive_touch_directory, remove_file, rename_file, sync_directory, touch_directory,
};

/// Tag type of [`FollowSymlink`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FollowSymlinkTag;

/// Typed boolean for file lookups: whether a symbolic link is followed to its target (`yes`) or
/// the link itself is examined (`no`).
pub type FollowSymlink = crate::engine::util::bool_class::BoolClass<FollowSymlinkTag>;

/// Interprocess communication (experimental API).
pub mod experimental {
    /// * `make_pipe()`: Create a pipe with `pipe2(2)`; returns a pair of file descriptors, the
    ///   first for reading from the pipe and the second for writing to it.
    /// * `spawn_process(pathname, params)`: Spawn a subprocess with `posix_spawn(3)`, so
    ///   `pathname` must be a relative or absolute path of the executable; the future resolves
    ///   with the process.
    /// * `spawn_process_default(pathname)`: The same with no parameters: `pathname` is used as
    ///   `argv[0]`.
    pub use crate::engine::core::reactor::experimental::{
        make_pipe, spawn_process, spawn_process_default,
    };
}

/// Intrusive doubly linked lists whose nodes unlink themselves when dropped, with a linear-time
/// `size`.
///
/// A node embeds a [`ListHook`](crate::engine::core::api::intrusive::ListHook) and must not move while it is linked (a node that must move swaps
/// hooks explicitly with [`ListHook::replace_with`](crate::engine::core::api::intrusive::ListHook::replace_with)). The list head is a sentinel hook embedded in
/// the [`List`](crate::engine::core::api::intrusive::List), so nothing is allocated.
///
/// Moving a list cannot fix up the neighbours of its sentinel, so a list must not move while it
/// holds nodes; an empty list moves freely. To keep that true when the last node unlinks itself
/// (auto-unlink, without knowing its list), the links that point at a sentinel carry a
/// tag bit: a node whose two neighbours are tagged is the only node of its list, and
/// unlinking it resets the sentinel to the null (empty, unanchored) state.
///
/// The links are `Cell`s: a node unlinks itself through its neighbours, which may be
/// the sentinel of a list that is borrowed elsewhere.
pub mod intrusive {
    use std::cell::Cell;
    use std::marker::{PhantomData, PhantomPinned};
    use std::ptr::{self, NonNull};

    /// The tag of a link that points at a list sentinel.
    const SENTINEL_TAG: usize = 1;

    /// Returns `p` tagged as a sentinel link.
    #[inline]
    fn tag(p: *mut ListHook) -> *mut ListHook {
        p.map_addr(|a| a | SENTINEL_TAG)
    }

    /// Returns whether the link `p` points at a sentinel.
    #[inline]
    fn is_tagged(p: *mut ListHook) -> bool {
        p.addr() & SENTINEL_TAG != 0
    }

    /// Returns the hook a (possibly tagged) link points at.
    #[inline]
    fn untag(p: *mut ListHook) -> *mut ListHook {
        p.map_addr(|a| a & !SENTINEL_TAG)
    }

    /// The link embedded in each node of a [`List`]; it unlinks itself when dropped.
    ///
    /// In a node, `next`/`prev` point at the neighbouring hooks, tagged when the
    /// neighbour is the sentinel. In a sentinel they point at the first and last node
    /// (untagged), or are null when the list is empty.
    #[repr(C)]
    #[derive(Debug)]
    pub struct ListHook {
        next: Cell<*mut ListHook>,
        prev: Cell<*mut ListHook>,
        _pin: PhantomPinned,
    }

    impl Default for ListHook {
        #[inline]
        fn default() -> Self {
            Self::new()
        }
    }

    impl ListHook {
        /// Returns an unlinked hook.
        #[inline]
        pub const fn new() -> Self {
            Self {
                next: Cell::new(ptr::null_mut()),
                prev: Cell::new(ptr::null_mut()),
                _pin: PhantomPinned,
            }
        }

        /// Returns the address of the hook, as its neighbours store it.
        #[inline]
        fn as_ptr(&self) -> *mut ListHook {
            ptr::from_ref(self).cast_mut()
        }

        /// Returns whether the hook is linked in a list.
        #[inline]
        pub fn is_linked(&self) -> bool {
            !self.next.get().is_null()
        }

        /// Removes the node from whatever list holds it; does nothing if it is not linked. O(1).
        #[inline]
        pub fn unlink(&self) {
            if !self.is_linked() {
                return;
            }
            let (next, prev) = (self.next.get(), self.prev.get());
            // SAFETY: a linked hook's neighbours are live hooks (or the sentinel) of the
            // same list, none of which moves while the node is linked.
            unsafe {
                if is_tagged(next) && is_tagged(prev) {
                    // The only node: the list becomes empty (and movable) again.
                    let s = untag(next);
                    (*s).next.set(ptr::null_mut());
                    (*s).prev.set(ptr::null_mut());
                } else {
                    // A sentinel stores untagged node links, a node stores the tagged
                    // sentinel link: the values below are already in the right form,
                    // since at most one of next/prev is the sentinel.
                    (*untag(prev)).next.set(next);
                    (*untag(next)).prev.set(prev);
                }
            }
            self.next.set(ptr::null_mut());
            self.prev.set(ptr::null_mut());
        }

        /// Puts the unlinked hook `me` in this linked hook's place in its list, leaving this hook
        /// unlinked. Does nothing if this hook is not linked. Used to move a linked node.
        ///
        /// # Safety
        /// `me` must be an unlinked hook, of a node of the same list's node type, that stays
        /// in place while linked, derived from its node's pointer (see `link_between`).
        #[inline]
        pub unsafe fn replace_with(&self, me: *mut ListHook) {
            if !self.is_linked() {
                return;
            }
            let (next, prev) = (self.next.get(), self.prev.get());
            // SAFETY: `prev` and `next` are adjacent around this hook in a live list, so they
            // stay adjacent once this hook's place is taken; the rest is the caller's contract.
            unsafe { Self::link_between(me, prev, next) };
            self.next.set(ptr::null_mut());
            self.prev.set(ptr::null_mut());
        }

        /// Links the hook `me` between `prev` and `next`, two adjacent links of one list
        /// (each a node, or the tagged sentinel).
        ///
        /// `me` is stored in the neighbours and later turned back into its node
        /// (`Adapter::value`), so it must carry the provenance of the whole node: it comes
        /// from `Adapter::hook` on the node pointer, never from a reference to the hook.
        ///
        /// # Safety
        /// `prev` and `next` must be adjacent in a live list; `me` must be an unlinked hook
        /// that stays in place while linked, derived from its node's pointer.
        #[inline]
        unsafe fn link_between(me: *mut ListHook, prev: *mut ListHook, next: *mut ListHook) {
            // SAFETY: guaranteed by the caller; writing `me` (untagged) is right both into
            // a sentinel and into a node.
            unsafe {
                (*me).prev.set(prev);
                (*me).next.set(next);
                (*untag(prev)).next.set(me);
                (*untag(next)).prev.set(me);
            }
        }
    }

    impl Drop for ListHook {
        #[inline]
        fn drop(&mut self) {
            self.unlink();
        }
    }

    /// Maps a node type to its embedded [`ListHook`] field. Declare one with
    /// [`intrusive_adapter!`](crate::intrusive_adapter).
    ///
    /// # Safety
    /// `OFFSET` must be the byte offset of a `ListHook` field inside `Value`.
    pub unsafe trait Adapter {
        /// The node type.
        type Value;
        /// Byte offset of the hook field in `Value`.
        const OFFSET: usize;

        /// Returns the hook of the node `v`. Pure address arithmetic; `v` is not dereferenced.
        #[inline]
        fn hook(v: *mut Self::Value) -> *mut ListHook {
            // SAFETY: by the trait's contract OFFSET is the offset of a hook field inside Value, so
            // the result stays within the node `v` points to.
            unsafe { (v as *mut u8).add(Self::OFFSET) as *mut ListHook }
        }

        /// Returns the node that embeds the hook `h`. Pure address arithmetic.
        #[inline]
        fn value(h: *mut ListHook) -> *mut Self::Value {
            // SAFETY: `h` is the hook of a node of this adapter, embedded at OFFSET of it, so the
            // result is the start of that node.
            unsafe { (h as *mut u8).sub(Self::OFFSET) as *mut Self::Value }
        }
    }

    /// Declares an [`Adapter`] for a node type and its hook field:
    /// `intrusive_adapter!(pub MyAdapter = MyNode { hook });`.
    #[macro_export]
    macro_rules! intrusive_adapter {
        ($vis:vis $name:ident = $value:ty { $field:ident }) => {
            /// Intrusive list adapter declared with `intrusive_adapter!`.
            $vis struct $name;
            // SAFETY: offset_of! gives the offset of the hook field, which the caller names as a
            // `ListHook` field of the node type (type-checked by the Adapter's use of it).
            unsafe impl $crate::engine::core::api::intrusive::Adapter for $name {
                type Value = $value;
                const OFFSET: usize = ::std::mem::offset_of!($value, $field);
            }
        };
    }

    /// An intrusive doubly linked list of nodes described by the adapter `A`.
    ///
    /// Insertions and removals are O(1) and allocation-free; `size` is O(n).
    /// The list must not move while it is non-empty (its nodes point at its sentinel);
    /// every insertion is `unsafe` with that contract.
    pub struct List<A: Adapter> {
        sentinel: ListHook,
        _marker: PhantomData<A>,
    }

    impl<A: Adapter> Default for List<A> {
        #[inline]
        fn default() -> Self {
            Self::new()
        }
    }

    impl<A: Adapter> List<A> {
        /// Returns an empty list.
        #[inline]
        pub const fn new() -> Self {
            Self { sentinel: ListHook::new(), _marker: PhantomData }
        }

        /// The tagged link to the sentinel, as nodes store it.
        #[inline]
        fn sentinel_link(&self) -> *mut ListHook {
            tag(self.sentinel.as_ptr())
        }

        /// Returns whether the list has no nodes.
        #[inline]
        pub fn is_empty(&self) -> bool {
            self.sentinel.next.get().is_null()
        }

        /// Links the node `v` at the back.
        ///
        /// # Safety
        /// `v` must be a live, unlinked node that stays in place while linked, and the
        /// list must not move while it is non-empty.
        #[inline]
        pub unsafe fn push_back(&mut self, v: *mut A::Value) {
            let s = self.sentinel_link();
            let last = if self.is_empty() { s } else { self.sentinel.prev.get() };
            // SAFETY: guaranteed by the caller; last and the sentinel are adjacent.
            unsafe { ListHook::link_between(A::hook(v), last, s) };
        }

        /// Links the node `v` at the front.
        ///
        /// # Safety
        /// As for `push_back`.
        #[inline]
        pub unsafe fn push_front(&mut self, v: *mut A::Value) {
            let s = self.sentinel_link();
            let first = if self.is_empty() { s } else { self.sentinel.next.get() };
            // SAFETY: guaranteed by the caller; the sentinel and first are adjacent.
            unsafe { ListHook::link_between(A::hook(v), s, first) };
        }

        /// Links the node `v` before the node `pos`.
        ///
        /// # Safety
        /// `pos` must be linked in this list; `v` as for `push_back`.
        #[inline]
        pub unsafe fn insert_before(&mut self, pos: *mut A::Value, v: *mut A::Value) {
            let p = A::hook(pos);
            // SAFETY: guaranteed by the caller; pos.prev and pos are adjacent.
            unsafe { ListHook::link_between(A::hook(v), (*p).prev.get(), p) };
        }

        /// Returns the first node, if any.
        #[inline]
        pub fn front(&self) -> Option<NonNull<A::Value>> {
            let f = self.sentinel.next.get();
            if f.is_null() { None } else { NonNull::new(A::value(f)) }
        }

        /// Returns the last node, if any.
        #[inline]
        pub fn back(&self) -> Option<NonNull<A::Value>> {
            let b = self.sentinel.prev.get();
            if b.is_null() { None } else { NonNull::new(A::value(b)) }
        }

        /// Unlinks the first node and returns it, if any.
        #[inline]
        pub fn pop_front(&mut self) -> Option<NonNull<A::Value>> {
            let f = self.front()?;
            // SAFETY: f is a linked node of this list.
            unsafe { (*A::hook(f.as_ptr())).unlink() };
            Some(f)
        }

        /// Returns the node after `v` in this list, `None` for the last node.
        ///
        /// # Safety
        /// `v` must be linked in this list.
        #[inline]
        pub unsafe fn next_of(&self, v: *mut A::Value) -> Option<NonNull<A::Value>> {
            // SAFETY: guaranteed by the caller.
            let n = unsafe { (*A::hook(v)).next.get() };
            if is_tagged(n) { None } else { NonNull::new(A::value(n)) }
        }

        /// Returns the node before `v` in this list, `None` for the front node.
        ///
        /// # Safety
        /// `v` must be linked in this list.
        #[inline]
        pub unsafe fn prev_of(&self, v: *mut A::Value) -> Option<NonNull<A::Value>> {
            // SAFETY: guaranteed by the caller.
            let p = unsafe { (*A::hook(v)).prev.get() };
            if is_tagged(p) { None } else { NonNull::new(A::value(p)) }
        }

        /// Moves every node of `other` to the back of this list, in O(1), leaving `other` empty.
        /// With `self` a new list, this is how a non-empty list is moved.
        ///
        /// # Safety
        /// The list must not move while it is non-empty.
        pub unsafe fn splice_back(&mut self, other: &mut Self) {
            let first = other.sentinel.next.get();
            if first.is_null() {
                return;
            }
            let last = other.sentinel.prev.get();
            let s = self.sentinel_link();
            // SAFETY: other is non-empty, so first and last are its live end nodes; they
            // are relinked between this list's last node (or sentinel) and its sentinel.
            unsafe {
                if self.is_empty() {
                    self.sentinel.next.set(first);
                    (*first).prev.set(s);
                } else {
                    let tail = self.sentinel.prev.get();
                    (*tail).next.set(first);
                    (*first).prev.set(tail);
                }
                (*last).next.set(s);
            }
            self.sentinel.prev.set(last);
            other.sentinel.next.set(ptr::null_mut());
            other.sentinel.prev.set(ptr::null_mut());
        }

        /// Returns the number of nodes. O(n): the list keeps no count.
        pub fn size(&self) -> usize {
            self.iter().count()
        }

        /// Unlinks every node. O(n).
        pub fn clear(&mut self) {
            while self.pop_front().is_some() {}
        }

        /// Iterates over the node pointers, front to back. The list must not be
        /// modified while iterating, except by unlinking the node just returned.
        #[inline]
        pub fn iter(&self) -> Iter<'_, A> {
            Iter { cur: self.sentinel.next.get(), _list: PhantomData }
        }
    }

    impl<A: Adapter> Drop for List<A> {
        fn drop(&mut self) {
            // Leaves the sentinel null, so its own hook has nothing to unlink.
            self.clear();
        }
    }

    /// Iterator over the nodes of a [`List`].
    pub struct Iter<'a, A: Adapter> {
        cur: *mut ListHook,
        _list: PhantomData<&'a List<A>>,
    }

    impl<A: Adapter> Iterator for Iter<'_, A> {
        type Item = NonNull<A::Value>;
        #[inline]
        fn next(&mut self) -> Option<Self::Item> {
            if self.cur.is_null() || is_tagged(self.cur) {
                return None;
            }
            let v = A::value(self.cur);
            // SAFETY: cur is a linked node hook.
            self.cur = unsafe { (*self.cur).next.get() };
            NonNull::new(v)
        }
    }
}
