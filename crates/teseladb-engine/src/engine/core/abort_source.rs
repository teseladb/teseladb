//! Abort sources: communicate a cancellation request to fibers on one shard.
//!
//! An [`AbortSource`] holds an intrusive list of [`Subscription`]s. Calling
//! [`AbortSource::request_abort`] (or one of its variants carrying an error) runs every
//! subscribed callback once, inline, and records the error, by default an
//! [`AbortRequestedException`]; later subscriptions are not linked and [`AbortSource::check`]
//! reports the error. Subscriptions store their callback inline, so subscribing does not
//! allocate. Both the source and its subscriptions are pinned once linked; dropping a
//! subscription unregisters it.

use std::fmt;
use std::pin::Pin;
use std::ptr;

use crate::engine::core::api::intrusive::{List, ListHook};
use crate::engine::core::future::{Exception, ExceptionPtr, make_exception_ptr};

/// The default error of an [`AbortSource`] whose abort was requested with
/// [`AbortSource::request_abort`].
#[derive(Debug, Clone, Copy, Default)]
pub struct AbortRequestedException;

impl AbortRequestedException {
    /// Returns the error message.
    pub fn what(&self) -> &'static str {
        "abort requested"
    }
}

impl fmt::Display for AbortRequestedException {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.what())
    }
}

impl std::error::Error for AbortRequestedException {}

/// The callback of a subscription, stored inline in the [`Subscription`]. Every
/// `FnMut(Option<&ExceptionPtr>)` closure is one; a named type implements it when the
/// subscription is a field whose type must be spelled out.
pub trait SubscriptionCallback {
    /// Runs the callback. `ex` is the error the abort was requested with, or `None` for a
    /// plain [`AbortSource::request_abort`] or a direct [`Subscription::on_abort`] call
    /// without an error.
    fn call(&mut self, ex: Option<&ExceptionPtr>);
}

impl<F: FnMut(Option<&ExceptionPtr>)> SubscriptionCallback for F {
    #[inline]
    fn call(&mut self, ex: Option<&ExceptionPtr>) {
        self(ex)
    }
}

/// The type-erased part of a subscription that the abort source links: the list hook, the
/// flag set once the callback has been called, and the function that calls the inline
/// callback.
#[repr(C)]
struct SubscriptionHeader {
    hook: ListHook,
    aborted: bool,
    call: unsafe fn(*mut SubscriptionHeader, Option<&ExceptionPtr>),
}

crate::intrusive_adapter!(SubscriptionAdapter = SubscriptionHeader { hook });

/// Calls the subscribed callback of `h` (at most once), unlinking it first.
///
/// # Safety
/// `h` must be the live header of a [`Subscription`].
unsafe fn header_on_abort(h: *mut SubscriptionHeader, ex: Option<&ExceptionPtr>) {
    // SAFETY: guaranteed by the caller.
    unsafe {
        (*h).hook.unlink();
        if !std::mem::replace(&mut (*h).aborted, true) {
            ((*h).call)(h, ex);
        }
    }
}

/// Represents a handle to the callback registered by a given fiber. Ending the
/// lifetime of the subscription will unregister the callback, if it hasn't
/// been invoked yet.
///
/// The hook and the callback are held inline, so subscribing allocates nothing. The hook
/// cannot be fixed up after a move, so a subscription is a pinned type: it moves freely
/// while empty (`Subscription::default()`),
/// and [`AbortSource::subscribe`] fills and links it in place through a pin, from which
/// point it stays there until it is dropped (which unlinks it).
#[repr(C)]
pub struct Subscription<F: SubscriptionCallback> {
    header: SubscriptionHeader,
    target: Option<F>,
    /// While the callback runs, a flag on the caller's frame that `Drop` sets, so that
    /// `call_target` knows whether the subscription outlived its callback; null otherwise.
    destroyed: *mut bool,
}

impl<F: SubscriptionCallback> Drop for Subscription<F> {
    #[inline]
    fn drop(&mut self) {
        if !self.destroyed.is_null() {
            // SAFETY: a non-null flag points at the frame of the running `call_target`,
            // which outlives this drop.
            unsafe { *self.destroyed = true };
        }
    }
}

impl<F: SubscriptionCallback> Default for Subscription<F> {
    /// An empty, unlinked subscription.
    #[inline]
    fn default() -> Self {
        Self {
            header: SubscriptionHeader {
                hook: ListHook::new(),
                aborted: false,
                call: Self::call_target,
            },
            target: None,
            destroyed: ptr::null_mut(),
        }
    }
}

impl<F: SubscriptionCallback> Subscription<F> {
    /// The `call` function of the header: runs the stored callback.
    ///
    /// The callback may destroy the subscription (and the object holding it), so it is moved
    /// out of the subscription and called on this frame: calling it in place while its
    /// storage is freed would be undefined behaviour. The callback lives until the
    /// subscription is destroyed: if the subscription survives
    /// the call it is put back (unless the callback subscribed it again), otherwise it is
    /// dropped when the call returns.
    ///
    /// # Safety
    /// `h` must be the header of a live `Subscription<F>`, derived from the pointer to the
    /// whole subscription (`init`).
    unsafe fn call_target(h: *mut SubscriptionHeader, ex: Option<&ExceptionPtr>) {
        // SAFETY: the header is the first field of the repr(C) subscription and `h` has the
        // provenance of the whole subscription; after the call it is touched only if
        // `destroyed` is still false, i.e. the subscription was not dropped by the callback.
        unsafe {
            let me = h.cast::<Self>();
            if let Some(mut t) = (*ptr::addr_of_mut!((*me).target)).take() {
                let mut destroyed = false;
                *ptr::addr_of_mut!((*me).destroyed) = &mut destroyed;
                t.call(ex);
                if !destroyed {
                    *ptr::addr_of_mut!((*me).destroyed) = ptr::null_mut();
                    let target = &mut *ptr::addr_of_mut!((*me).target);
                    if target.is_none() {
                        *target = Some(t);
                    }
                }
            }
        }
    }

    /// The subscription in place (it is never moved out of its pin).
    #[inline]
    fn get(self: Pin<&mut Self>) -> &mut Self {
        // SAFETY: nothing is moved out; the hook stays where it was linked.
        unsafe { self.get_unchecked_mut() }
    }

    /// Stores the callback and links the subscription if no abort was requested yet.
    fn init(self: Pin<&mut Self>, as_: Pin<&mut AbortSource>, target: F) {
        let me = self.get();
        // Re-subscribing an engaged slot replaces it: the old subscription is unregistered
        // first.
        me.header.hook.unlink();
        me.target = Some(target);
        me.header.aborted = false;
        let as_ = as_.get();
        if !as_.abort_requested() {
            let whole: *mut Self = me;
            // SAFETY: `whole` is the live subscription; the header pointer is derived from
            // the pointer to the whole subscription, so call_target can reach the callback.
            let h: *mut SubscriptionHeader = unsafe { ptr::addr_of_mut!((*whole).header) };
            // SAFETY: the subscription is pinned, so the header stays in place while
            // linked; the abort source is pinned too, so its list does not move.
            unsafe { as_.subscriptions.push_back(h) };
        }
    }

    /// Calls the subscribed callback (at most once).
    ///
    /// The [`AbortSource`] calls this on all linked subscriptions when an abort is requested.
    /// It may also be called independently by the user at any time, which unlinks the
    /// subscription from the abort source's list.
    pub fn on_abort(self: Pin<&mut Self>, ex: Option<&ExceptionPtr>) {
        let whole: *mut Self = self.get();
        // SAFETY: `whole` is the live subscription (see init()).
        let h: *mut SubscriptionHeader = unsafe { ptr::addr_of_mut!((*whole).header) };
        // SAFETY: h is the live header of this subscription.
        unsafe { header_on_abort(h, ex) };
    }

    /// Returns whether the subscription is linked to an abort source (its callback has not
    /// run yet).
    #[inline]
    pub fn is_linked(&self) -> bool {
        self.header.hook.is_linked()
    }
}

// Dropping a subscription drops its hook in place, which unlinks it.

/// Facility to communicate a cancellation request to a fiber.
///
/// Callbacks can be registered with the abort source; they are called together, inline,
/// by [`AbortSource::request_abort`] and its variants.
///
/// The subscriptions point at the list head embedded in the abort source, so the
/// operations that can link one take it pinned (`Pin<&mut AbortSource>`).
pub struct AbortSource {
    subscriptions: List<SubscriptionAdapter>,
    ex: ExceptionPtr,
    /// Whether the abort carried an error of its own (not the default one).
    ex_is_custom: bool,
    /// Produces the error of an abort requested without one.
    default_exception: fn() -> ExceptionPtr,
}

impl Default for AbortSource {
    fn default() -> Self {
        Self::new()
    }
}

impl AbortSource {
    /// Creates an abort source with no subscriptions whose default error is
    /// [`AbortRequestedException`].
    #[inline]
    pub fn new() -> Self {
        Self::with_default_exception(Self::abort_requested_default)
    }

    /// Creates an abort source whose default error, used when an abort is requested without
    /// one, is produced by `f`.
    #[inline]
    pub fn with_default_exception(f: fn() -> ExceptionPtr) -> Self {
        Self {
            subscriptions: List::new(),
            ex: ExceptionPtr::null(),
            ex_is_custom: false,
            default_exception: f,
        }
    }

    fn abort_requested_default() -> ExceptionPtr {
        make_exception_ptr(AbortRequestedException)
    }

    /// The abort source in place (it is never moved out of its pin).
    #[inline]
    fn get(self: Pin<&mut Self>) -> &mut Self {
        // SAFETY: nothing is moved out; the subscription list stays in place.
        unsafe { self.get_unchecked_mut() }
    }

    fn do_request_abort(self: Pin<&mut Self>, ex: Option<ExceptionPtr>) {
        // The callbacks may use the abort source, so it is reached through a raw pointer
        // and no reference to it is live while they run.
        let me: *mut Self = self.get();
        // SAFETY: `me` is the live, pinned abort source; each reference ends with its
        // statement.
        unsafe {
            if !(*me).ex.is_null() {
                return;
            }
            let def = (*me).get_default_exception();
            (*me).ex_is_custom = ex.is_some();
            (*me).ex = ex.clone().unwrap_or(def);
            assert!(!(*me).ex.is_null());
        }
        // The subscriptions are moved to a local list, which stays on this frame until it is
        // drained; the source's own list is left empty while the callbacks run.
        let mut subs = List::<SubscriptionAdapter>::new();
        // SAFETY: `subs` does not move while it holds subscriptions.
        unsafe { subs.splice_back(&mut *ptr::addr_of_mut!((*me).subscriptions)) };
        while let Some(s) = subs.front() {
            // SAFETY: linked subscriptions are live (pinned) nodes.
            unsafe { header_on_abort(s.as_ptr(), ex.as_ref()) };
        }
    }

    /// Delays the invocation of the callback `f` until an abort is requested.
    ///
    /// The subscription is the pinned `slot` provided by the caller; `f` is stored in it and
    /// the slot controls the callback's lifetime (dropping it unregisters `f`).
    ///
    /// Returns `true` if and only if no abort had been requested yet, and therefore the
    /// subscription is linked to this abort source.
    ///
    /// Once an abort is requested or [`Subscription::on_abort`] is called, `f` is called
    /// (exactly once) and the subscription is unlinked.
    ///
    /// If an abort was already requested, the slot is left unlinked and `false` is returned;
    /// the user can still call [`Subscription::on_abort`] on it to invoke `f`.
    pub fn subscribe<F>(self: Pin<&mut Self>, mut slot: Pin<&mut Subscription<F>>, f: F) -> bool
    where
        F: SubscriptionCallback,
    {
        slot.as_mut().init(self, f);
        slot.is_linked()
    }

    /// [`AbortSource::subscribe`] for a callback that takes no argument.
    pub fn subscribe_naive<F>(
        self: Pin<&mut Self>,
        slot: Pin<&mut Subscription<NaiveCallback<F>>>,
        f: F,
    ) -> bool
    where
        F: FnMut(),
    {
        self.subscribe(slot, NaiveCallback(f))
    }

    /// Requests that the target operation be aborted. Current subscriptions
    /// are invoked inline with this call with no error (`None`),
    /// and no new ones can be registered. The recorded error is the default one.
    /// Does nothing if an abort was already requested.
    pub fn request_abort(self: Pin<&mut Self>) {
        self.do_request_abort(None);
    }

    /// Requests that the target operation be aborted with the error `ex`.
    /// Current subscriptions are invoked inline with this exception,
    /// and no new ones can be registered.
    pub fn request_abort_ex(self: Pin<&mut Self>, ex: ExceptionPtr) {
        self.do_request_abort(Some(ex));
    }

    /// Requests that the target operation be aborted with the error `e`.
    /// Current subscriptions are invoked inline with this error, converted to an [`ExceptionPtr`],
    /// and no new ones can be registered.
    pub fn request_abort_ex_from<E: Exception>(self: Pin<&mut Self>, e: E) {
        self.do_request_abort(Some(make_exception_ptr(e)));
    }

    /// Returns whether an abort has been requested.
    #[inline]
    pub fn abort_requested(&self) -> bool {
        !self.ex.is_null()
    }

    /// Checks whether cancellation has been requested.
    ///
    /// # Errors
    /// Returns the error the abort was requested with, if any.
    #[inline]
    pub fn check(&self) -> Result<(), ExceptionPtr> {
        if self.abort_requested() {
            return Err(self.ex.clone());
        }
        Ok(())
    }

    /// Returns the error with which an abort was requested (null if none was).
    #[inline]
    pub fn abort_requested_exception_ptr(&self) -> &ExceptionPtr {
        &self.ex
    }

    /// The error the abort was requested with, or `None` when no abort was requested or it
    /// was requested without one (and so carries the default error).
    #[inline]
    pub fn abort_requested_custom_exception(&self) -> Option<&ExceptionPtr> {
        self.ex_is_custom.then_some(&self.ex)
    }

    /// Returns the default error of this abort source: [`AbortRequestedException`] unless the
    /// source was built with [`AbortSource::with_default_exception`].
    #[inline]
    pub fn get_default_exception(&self) -> ExceptionPtr {
        (self.default_exception)()
    }
}

/// The callback of [`AbortSource::subscribe_naive`]: calls the wrapped closure and ignores
/// the error.
pub struct NaiveCallback<F: FnMut()>(F);

impl<F: FnMut()> SubscriptionCallback for NaiveCallback<F> {
    #[inline]
    fn call(&mut self, _: Option<&ExceptionPtr>) {
        (self.0)()
    }
}
