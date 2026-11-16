//! Tests of `AbortSource`: subscribers are called once with the abort exception, can
//! unsubscribe, are refused once the source has fired, and abortable sleeps wake early.
//! A subscription is a pinned slot that the source links; `is_linked()` tells whether it is.

#![cfg(target_os = "linux")]

use std::cell::{Cell, RefCell};
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use teseladb::engine::core::abort_on_expiry::AbortOnExpiry;
use teseladb::engine::core::abort_source::{
    AbortRequestedException, AbortSource, NaiveCallback, Subscription,
};
use teseladb::engine::core::future::{ExceptionPtr, RuntimeError, make_ready_future};
use teseladb::engine::core::future_util::sleep_abortable_with;
use teseladb::engine::core::manual_clock::ManualClock;
use teseladb::engine::core::reactor::yield_;
use teseladb::engine::core::sleep::SleepAborted;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::core::timed_out_error::TimedOutError;
use teseladb::engine::core::timer::SteadyClockType;
use teseladb::engine::util::defer::defer;
use teseladb::engine_test_case;

type Cb = Box<dyn FnMut(Option<&ExceptionPtr>)>;
type NaiveCb = NaiveCallback<Box<dyn FnMut()>>;

fn new_as() -> Pin<Box<AbortSource>> {
    Box::pin(AbortSource::new())
}

fn new_slot<F: teseladb::engine::core::abort_source::SubscriptionCallback>()
-> Pin<Box<Subscription<F>>> {
    Box::pin(Subscription::default())
}

fn runtime_error(msg: &str) -> ExceptionPtr {
    ExceptionPtr::new(RuntimeError(msg.to_string()))
}

engine_test_case!(test_abort_source_notifies_subscriber, {
    let signalled = Rc::new(Cell::new(false));
    let mut as_ = new_as();
    let mut st_opt = new_slot::<NaiveCb>();
    let s = signalled.clone();
    as_.as_mut()
        .subscribe_naive(st_opt.as_mut(), Box::new(move || s.set(true)) as Box<dyn FnMut()>);

    assert!(st_opt.is_linked());
    as_.as_mut().request_abort();
    assert!(signalled.get());
    assert!(!st_opt.is_linked());
    assert!(matches!(as_.check(), Err(ref e) if e.is::<AbortRequestedException>()));

    make_ready_future(())
});

engine_test_case!(test_abort_source_subscription_unregister, {
    let signalled = Rc::new(Cell::new(false));
    let mut as_ = new_as();
    let mut st_opt = new_slot::<NaiveCb>();
    let s = signalled.clone();
    as_.as_mut()
        .subscribe_naive(st_opt.as_mut(), Box::new(move || s.set(true)) as Box<dyn FnMut()>);

    assert!(st_opt.is_linked());
    st_opt.set(Subscription::default());

    as_.as_mut().request_abort();
    assert!(!signalled.get());

    make_ready_future(())
});

engine_test_case!(test_abort_source_rejects_subscription, {
    let mut as_ = new_as();
    as_.as_mut().request_abort();
    let mut st_opt = new_slot::<NaiveCb>();
    let linked = as_.as_mut().subscribe_naive(st_opt.as_mut(), Box::new(|| {}) as Box<dyn FnMut()>);
    assert!(!linked);
    assert!(!st_opt.is_linked());

    make_ready_future(())
});

engine_test_case!(test_sleep_abortable, {
    let mut as_ = new_as();
    let f = sleep_abortable_with::<SteadyClockType>(Duration::from_secs(100), as_.as_mut())
        .then_wrapped(|f| match f.get() {
            Ok(()) => panic!("should have failed"),
            Err(e) if e.is::<SleepAborted>() => {
                // expected
            }
            Err(_) => panic!("unexpected exception"),
        });
    as_.as_mut().request_abort();
    f.finally(move || drop(as_))
});

engine_test_case!(test_sleep_abortable_no_abort, {
    let mut as_ = new_as();
    // Check that the sleep completes as usual if the
    // abort source doesn't fire.
    let f = sleep_abortable_with::<ManualClock>(Duration::from_secs(100), as_.as_mut());
    ManualClock::advance(Duration::from_secs(99));
    assert!(!f.available());
    ManualClock::advance(Duration::from_secs(101));
    f.finally(move || drop(as_))
});

// Verify that negative sleep does not sleep forever. It should not sleep
// at all. (A `Duration` cannot be negative; the shortest sleep is zero.)
engine_test_case!(test_negative_sleep_abortable, {
    let mut as_ = new_as();
    sleep_abortable_with::<SteadyClockType>(Duration::ZERO, as_.as_mut()).finally(move || drop(as_))
});

engine_test_case!(test_request_abort_with_exception, {
    let mut as_ = new_as();
    let mut st_opt = new_slot::<Cb>();
    let aborted_ex: Rc<RefCell<Option<Option<ExceptionPtr>>>> = Rc::new(RefCell::new(None));
    let expected_message = "expected";

    let make_abort_source = |as_: &mut Pin<Box<AbortSource>>,
                             st_opt: &mut Pin<Box<Subscription<Cb>>>| {
        as_.set(AbortSource::new());
        let a = aborted_ex.clone();
        as_.as_mut().subscribe(
            st_opt.as_mut(),
            Box::new(move |opt_ex: Option<&ExceptionPtr>| {
                *a.borrow_mut() = Some(opt_ex.cloned());
            }) as Cb,
        );
        *aborted_ex.borrow_mut() = None;
    };

    let check_aborted = |as_: &Pin<Box<AbortSource>>| {
        let got = aborted_ex.borrow();
        assert!(got.is_some());
        let mut caught_exception = false;
        if let Some(Some(e)) = got.as_ref()
            && let Some(r) = e.downcast_ref::<RuntimeError>()
        {
            assert_eq!(r.0, expected_message);
            caught_exception = true;
        }
        assert!(caught_exception);
        assert!(matches!(as_.check(), Err(ref e) if e.is::<RuntimeError>()));
    };

    make_abort_source(&mut as_, &mut st_opt);
    let ex = runtime_error(expected_message);
    as_.as_mut().request_abort_ex(ex);
    check_aborted(&as_);

    make_abort_source(&mut as_, &mut st_opt);
    as_.as_mut().request_abort_ex(runtime_error(expected_message));
    check_aborted(&as_);

    make_abort_source(&mut as_, &mut st_opt);
    as_.as_mut().request_abort_ex_from(RuntimeError(expected_message.to_string()));
    check_aborted(&as_);

    make_ready_future(())
});

engine_test_case!(test_sleep_abortable_with_exception, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut as_ = new_as();
        let f = sleep_abortable_with::<SteadyClockType>(Duration::from_secs(10), as_.as_mut());
        let expected_message = "expected";
        as_.as_mut().request_abort_ex_from(RuntimeError(expected_message.to_string()));

        let mut caught_exception = false;
        if let Err(e) = f.get()
            && let Some(r) = e.downcast_ref::<RuntimeError>()
        {
            assert_eq!(r.0, expected_message);
            caught_exception = true;
        }
        assert!(caught_exception);
        Ok(())
    })
});

// A linked subscription is pinned, so the slots stay put and only the order of
// destruction is checked: destroying the source with live subscriptions calls nothing.
engine_test_case!(test_destroy_with_moved_subscriptions, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut as_ = Some(new_as());
        let aborted = Rc::new(Cell::new(0));
        let mut sub1 = new_slot::<NaiveCb>();
        let a1 = aborted.clone();
        as_.as_mut().expect("live").as_mut().subscribe_naive(
            sub1.as_mut(),
            Box::new(move || a1.set(a1.get() + 1)) as Box<dyn FnMut()>,
        );
        let mut sub4 = new_slot::<NaiveCb>();
        let a4 = aborted.clone();
        as_.as_mut().expect("live").as_mut().subscribe_naive(
            sub4.as_mut(),
            Box::new(move || a4.set(a4.get() + 1)) as Box<dyn FnMut()>,
        );
        // Replace the second subscription: it is dropped, the first one stays.
        sub4.set(Subscription::default());
        as_ = None;
        assert!(as_.is_none());
        assert_eq!(aborted.get(), 0);
        drop(sub1);
        Ok(())
    })
});

engine_test_case!(test_request_abort_twice, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut as_ = new_as();
        as_.as_mut().request_abort_ex_from(RuntimeError(String::new()));
        as_.as_mut().request_abort();
        assert!(matches!(as_.check(), Err(ref e) if e.is::<RuntimeError>()));
        Ok(())
    })
});

fn recording_callback(signalled_ex: &Rc<RefCell<Option<ExceptionPtr>>>) -> Cb {
    let s = signalled_ex.clone();
    Box::new(move |ex: Option<&ExceptionPtr>| {
        assert!(s.borrow().is_none());
        *s.borrow_mut() = ex.cloned();
    })
}

engine_test_case!(test_on_abort_call_after_abort, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let signalled_ex: Rc<RefCell<Option<ExceptionPtr>>> = Rc::new(RefCell::new(None));
        let mut as_ = new_as();
        let mut sub = new_slot::<Cb>();
        as_.as_mut().subscribe(sub.as_mut(), recording_callback(&signalled_ex));
        assert!(sub.is_linked());
        assert!(signalled_ex.borrow().is_none());

        // on_abort should trigger the subscribed callback
        as_.as_mut().request_abort_ex(runtime_error("signaled"));
        assert!(!sub.is_linked());
        assert!(signalled_ex.borrow().is_some());
        assert!(signalled_ex.borrow().as_ref().expect("ex").is::<RuntimeError>());

        // on_abort is single-shot
        *signalled_ex.borrow_mut() = None;
        sub.as_mut().on_abort(Some(&runtime_error("signaled")));
        assert!(signalled_ex.borrow().is_none());
        Ok(())
    })
});

engine_test_case!(test_on_abort_call_before_abort, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let signalled_ex: Rc<RefCell<Option<ExceptionPtr>>> = Rc::new(RefCell::new(None));
        let mut as_ = new_as();
        let mut sub = new_slot::<Cb>();
        as_.as_mut().subscribe(sub.as_mut(), recording_callback(&signalled_ex));
        assert!(sub.is_linked());
        assert!(signalled_ex.borrow().is_none());

        // on_abort should trigger the subscribed callback
        sub.as_mut().on_abort(Some(&runtime_error("signaled")));
        assert!(!sub.is_linked());
        assert!(signalled_ex.borrow().is_some());
        assert!(signalled_ex.borrow().as_ref().expect("ex").is::<RuntimeError>());

        // subscription is single-shot
        *signalled_ex.borrow_mut() = None;
        as_.as_mut().request_abort_ex(runtime_error("signaled"));
        assert!(signalled_ex.borrow().is_none());
        Ok(())
    })
});

engine_test_case!(test_subscribe_aborted_source, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let signalled_ex: Rc<RefCell<Option<ExceptionPtr>>> = Rc::new(RefCell::new(None));
        let mut as_ = new_as();
        as_.as_mut().request_abort();
        let mut sub = new_slot::<Cb>();
        as_.as_mut().subscribe(sub.as_mut(), recording_callback(&signalled_ex));

        // subscription is expected to evaluate to false
        // if abort_source was already aborted
        assert!(!sub.is_linked());
        assert!(signalled_ex.borrow().is_none());

        // on_abort should trigger the subscribed callback
        // if abort_source was already aborted
        sub.as_mut().on_abort(Some(&runtime_error("signaled")));
        assert!(signalled_ex.borrow().is_some());
        assert!(signalled_ex.borrow().as_ref().expect("ex").is::<RuntimeError>());

        // on_abort is single-shot
        *signalled_ex.borrow_mut() = None;
        sub.as_mut().on_abort(Some(&runtime_error("signaled")));
        assert!(signalled_ex.borrow().is_none());
        Ok(())
    })
});

engine_test_case!(test_subscription_callback_lifetime, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        // The subscription callback function needs to be destroyed
        // only when the subscription is destroyed.
        let callback_destroyed = Rc::new(Cell::new(false));
        let callback_called = Rc::new(Cell::new(0));
        let cd = callback_destroyed.clone();
        let when_destroyed = defer(move || cd.set(true));
        let mut as_ = new_as();
        let mut sub: Option<Pin<Box<Subscription<Cb>>>> = Some(new_slot::<Cb>());
        let cc = callback_called.clone();
        as_.as_mut().subscribe(
            sub.as_mut().expect("sub").as_mut(),
            Box::new(move |_ex: Option<&ExceptionPtr>| {
                let _keep = &when_destroyed;
                cc.set(cc.get() + 1);
            }) as Cb,
        );
        assert!(sub.is_some());
        assert!(sub.as_ref().expect("sub").is_linked());
        assert!(!callback_destroyed.get());
        assert_eq!(callback_called.get(), 0);

        // on_abort should trigger the subscribed callback
        as_.as_mut().request_abort_ex(runtime_error("signaled"));
        assert!(!sub.as_ref().expect("sub").is_linked());
        assert!(!callback_destroyed.get());
        assert_eq!(callback_called.get(), 1);

        // on_abort is single-shot
        sub.as_mut().expect("sub").as_mut().on_abort(Some(&runtime_error("signaled")));
        assert!(!callback_destroyed.get());
        assert_eq!(callback_called.get(), 1);

        sub = None;
        assert!(sub.is_none());
        assert!(callback_destroyed.get());
        assert_eq!(callback_called.get(), 1);
        Ok(())
    })
});

engine_test_case!(test_abort_on_expiry, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut abort = std::pin::pin!(AbortOnExpiry::<ManualClock>::new(
            ManualClock::now() + Duration::from_secs(1)
        ));
        let ex: Rc<RefCell<Option<ExceptionPtr>>> = Rc::new(RefCell::new(None));
        let called = Rc::new(Cell::new(0));
        let mut sub = new_slot::<Cb>();
        let (e2, c2) = (ex.clone(), called.clone());
        abort.as_mut().abort_source().subscribe(
            sub.as_mut(),
            Box::new(move |ex_opt: Option<&ExceptionPtr>| {
                c2.set(c2.get() + 1);
                if let Some(e) = ex_opt {
                    *e2.borrow_mut() = Some(e.clone());
                }
            }) as Cb,
        );
        assert!(!abort.as_mut().abort_source().abort_requested());
        assert!(called.get() == 0);
        ManualClock::advance(Duration::from_secs(1));
        yield_().get()?;
        assert!(abort.as_mut().abort_source().abort_requested());
        assert_eq!(called.get(), 1);
        assert!(ex.borrow().is_some());
        assert!(ex.borrow().as_ref().expect("ex").is::<TimedOutError>());
        Ok(())
    })
});
