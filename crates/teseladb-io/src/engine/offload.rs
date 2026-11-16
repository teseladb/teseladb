//! The few calls of the peer layer that block (a node that joins waits for Group 0, a host name
//! resolves) run on a short-lived thread of their own and their result comes back to the shard
//! that asked, which awaits it without blocking.

use std::any::Any;
use std::cell::RefCell;
use std::collections::HashMap;
use std::future::poll_fn;
use std::task::{Poll, Waker};
use std::thread;

use super::post::ShardPost;

/// The results of a shard's calls that are still running elsewhere.
#[derive(Default)]
struct Waiting {
    next: u64,
    calls: HashMap<u64, (Option<Box<dyn Any>>, Option<Waker>)>,
}

thread_local! {
    static WAITING: RefCell<Waiting> = RefCell::new(Waiting::default());
}

fn deliver(id: u64, value: Box<dyn Any>) {
    WAITING.with(|w| {
        if let Some((slot, waker)) = w.borrow_mut().calls.get_mut(&id) {
            *slot = Some(value);
            if let Some(waker) = waker.take() {
                waker.wake();
            }
        }
    });
}

/// Runs `f` on a thread named `name` and gives its result back on this shard (`post` is this
/// shard's); `None` if no thread could start or the engine stopped before it ended.
pub(crate) async fn off_shard<T: Send + 'static>(
    post: &ShardPost,
    name: &str,
    f: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let id = WAITING.with(|w| {
        let mut w = w.borrow_mut();
        w.next += 1;
        let id = w.next;
        w.calls.insert(id, (None, None));
        id
    });
    let back = post.clone();
    let spawned = thread::Builder::new().name(name.to_owned()).spawn(move || {
        let value = f();
        back.post(move || deliver(id, Box::new(value)));
    });
    if spawned.is_err() {
        WAITING.with(|w| w.borrow_mut().calls.remove(&id));
        return None;
    }
    let value = poll_fn(|cx| {
        WAITING.with(|w| {
            let mut w = w.borrow_mut();
            let entry = w.calls.get_mut(&id).expect("a call waiting");
            match entry.0.take() {
                Some(v) => {
                    w.calls.remove(&id);
                    Poll::Ready(v)
                }
                None => {
                    entry.1 = Some(cx.waker().clone());
                    Poll::Pending
                }
            }
        })
    })
    .await;
    value.downcast::<T>().ok().map(|b| *b)
}
