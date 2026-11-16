#![cfg(target_os = "linux")]
//! Tests of the type-erased callables, the backtrace types and the allocation failure
//! injector: empty and boxed functions taking borrowed arguments, assignment, backtrace
//! frames, vectors, task traces and their comparisons, and failures injected at counted
//! allocation points, with custom callbacks, nested runs and trace logging.

use std::cell::Cell;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;

use teseladb::engine::core::future::{BadAlloc, RuntimeError};
use teseladb::engine::core::scheduling::SchedulingGroup;
use teseladb::engine::core::shared_ptr::make_lw_shared;
use teseladb::engine::util::alloc_failure_injector::memory::{
    local_failure_injector, on_alloc_point, with_allocation_failures,
};
use teseladb::engine::util::backtrace::internal::Backtraced;
use teseladb::engine::util::backtrace::{
    Frame, SharedBacktrace, SimpleBacktrace, SimpleBacktraceVectorType, StaticVector, TaskEntry,
    Tasktrace, TasktraceEntry, TasktraceVectorType, backtrace_with, current_backtrace,
    current_backtrace_tasklocal, decorate, throw_with_backtrace,
};
use teseladb::engine::util::log::{LogLevel, global_logger_registry};
use teseladb::engine::util::noncopyable_function::{NoncopyableFunction, RefFn};

fn panic_message(p: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        return (*s).to_owned();
    }
    p.downcast_ref::<String>().cloned().unwrap_or_default()
}

// ---------------------------------------------------------------------------------------
// Functions

#[test]
fn assigning_a_function_replaces_the_previous_one() {
    let dropped = Rc::new(Cell::new(0));
    let d = dropped.clone();
    let guard = scopeguard_like(move || d.set(d.get() + 1));
    let mut f = NoncopyableFunction::<fn() -> i32>::new(move || {
        let _keep = &guard;
        1
    });
    assert_eq!(f.call(), 1);
    f.assign(NoncopyableFunction::<fn() -> i32>::new(|| 2));
    assert_eq!(dropped.get(), 1, "the replaced closure is dropped");
    assert_eq!(f.call(), 2);
    f.assign(NoncopyableFunction::<fn() -> i32>::default());
    assert!(!f.is_some());
}

/// A value that runs its function when dropped.
struct OnDrop(Box<dyn FnMut()>);

impl Drop for OnDrop {
    fn drop(&mut self) {
        (self.0)();
    }
}

fn scopeguard_like(f: impl FnMut() + 'static) -> OnDrop {
    OnDrop(Box::new(f))
}

#[test]
fn borrowed_first_argument_functions_call_inline_and_boxed() {
    type F = NoncopyableFunction<RefFn<str, usize, usize>>;
    let mut empty = F::empty();
    assert!(!empty.is_some());
    let r = catch_unwind(AssertUnwindSafe(|| empty.call("x", 1)));
    assert_eq!(panic_message(r.unwrap_err()), "call to an empty function");

    let mut small = F::new(|s: &str, n: usize| s.len() + n);
    assert!(small.is_some());
    assert_eq!(small.call("abc", 2), 5);

    // A closure larger than the inline buffer is boxed.
    let big = [7usize; 16];
    let mut boxed = F::new(move |s: &str, n: usize| s.len() * big[n]);
    assert_eq!(boxed.call("ab", 3), 14);
    drop(boxed);
}

#[test]
fn borrowed_argument_functions_call_inline_and_boxed() {
    type F = NoncopyableFunction<dyn FnMut(&String) -> usize>;
    let mut empty = F::empty();
    assert!(!empty.is_some());
    let arg = String::from("hello");
    let r = catch_unwind(AssertUnwindSafe(|| empty.call(&arg)));
    assert_eq!(panic_message(r.unwrap_err()), "call to an empty function");

    let mut small = F::new(|s: &String| s.len());
    assert_eq!(small.call(&arg), 5);

    let big = [3usize; 16];
    let calls = Rc::new(Cell::new(0));
    let c = calls.clone();
    let mut boxed = F::new(move |s: &String| {
        c.set(c.get() + 1);
        s.len() + big[15]
    });
    assert_eq!(boxed.call(&arg), 8);
    assert_eq!(boxed.call(&String::new()), 3);
    assert_eq!(calls.get(), 2);
    drop(boxed);
    assert_eq!(Rc::strong_count(&calls), 1, "the boxed closure is freed");
}

// ---------------------------------------------------------------------------------------
// Backtraces

fn hash_of<T: Hash>(t: &T) -> u64 {
    let mut h = DefaultHasher::new();
    t.hash(&mut h);
    h.finish()
}

#[test]
fn frames_compare_by_object_and_address() {
    let d = Frame::default();
    assert_eq!(d.addr, 0);
    assert!(d == Frame::default());
    assert_eq!(d.to_string(), "0x0");
    let other = Frame { addr: 1, ..d };
    assert!(d != other);

    // An address outside every loaded object is attributed to the first one.
    let far = decorate(usize::MAX);
    assert_eq!(far.addr, usize::MAX - far.so.begin);
    assert!(far != Frame::default());
}

#[test]
fn incremental_backtraces_walk_the_same_stack() {
    let mut full = Vec::new();
    backtrace_with(|f| full.push(f), false);
    let mut inc = Vec::new();
    backtrace_with(|f| inc.push(f), true);
    assert!(!full.is_empty());
    assert!(!inc.is_empty());
    // The outermost frames (the test harness) are the same in both walks.
    assert!(full.last() == inc.last());
}

#[test]
fn static_vectors_clone_and_compare() {
    let mut v: StaticVector<u32, 4> = StaticVector::default();
    assert_eq!(v.size(), 0);
    assert_eq!(v.capacity(), 4);
    assert_eq!(v.max_size(), 4);
    v.push_back(1);
    v.push_back(2);
    let c = v.clone();
    assert!(c == v);
    assert_eq!(c.as_slice(), &[1, 2]);
    let mut d = v.clone();
    d.push_back(3);
    assert!(d != v);
    assert_eq!(d.iter().copied().sum::<u32>(), 6);
}

#[test]
fn simple_backtraces_keep_their_delimiter_and_hash() {
    let mut frames = SimpleBacktraceVectorType::new();
    frames.push_back(Frame { addr: 0x10, ..Frame::default() });
    frames.push_back(Frame { addr: 0x20, ..Frame::default() });
    let a = SimpleBacktrace::with_delimeter(frames.clone(), b'\n');
    let b = SimpleBacktrace::new(frames);
    assert_eq!(a.delimeter(), b'\n');
    assert_eq!(b.delimeter(), b' ');
    assert_eq!(a.hash(), b.hash());
    assert!(a == b, "the delimiter is not part of the identity");
    assert_eq!(hash_of(&a), hash_of(&b));
    assert_eq!(b.to_string(), "0x10 0x20");

    let e = SimpleBacktrace::empty_with_delimeter(b'|');
    assert_eq!(e.delimeter(), b'|');
    assert_eq!(e.hash(), 0);
    assert_eq!(e.to_string(), "");
    let d = SimpleBacktrace::default();
    assert_eq!(d.delimeter(), b' ');
    assert!(d == e);
    assert!(d != a);
}

#[test]
fn task_traces_compare_entries_and_print_them() {
    let t1 = TaskEntry::new("TaskA", 11);
    let t1_other_hash = TaskEntry::new("TaskA", 12);
    let t2 = TaskEntry::new("TaskB", 11);
    assert_eq!(t1.hash(), 11);
    assert!(t1 == t1_other_hash, "task entries compare by type name");
    assert!(t1 != t2);
    assert_eq!(t2.to_string(), "TaskB");

    let mut frames = SimpleBacktraceVectorType::new();
    frames.push_back(Frame { addr: 0xabc, ..Frame::default() });
    let shared: SharedBacktrace = make_lw_shared(SimpleBacktrace::new(frames));
    let eb = TasktraceEntry::SharedBacktrace(shared.clone());
    let et = TasktraceEntry::TaskEntry(t1);
    assert!(eb == TasktraceEntry::SharedBacktrace(shared.clone()));
    assert!(et == TasktraceEntry::TaskEntry(t1_other_hash));
    assert!(eb != et);
    assert!(et != eb);
    assert!(et != TasktraceEntry::TaskEntry(t2));

    let mut prev = TasktraceVectorType::new();
    prev.push_back(eb.clone());
    prev.push_back(et.clone());
    let main = SimpleBacktrace::with_delimeter(SimpleBacktraceVectorType::new(), b'#');
    let tt = Tasktrace::new(main.clone(), prev.clone(), 5, SchedulingGroup::default());
    assert_eq!(tt.hash(), main.hash().wrapping_mul(31) ^ 5);
    assert_eq!(tt.delimeter(), b'#');
    let same = Tasktrace::new(main.clone(), prev, 5, SchedulingGroup::default());
    assert!(tt == same);
    assert_eq!(hash_of(&tt), hash_of(&same));
    let other = Tasktrace::new(main, TasktraceVectorType::new(), 5, SchedulingGroup::default());
    assert!(tt != other);
    assert_eq!(tt.to_string(), "\n   --------\n0xabc\n   --------\n   TaskA");

    let d = Tasktrace::default();
    assert_eq!(d.hash(), 0);
    assert_eq!(d.to_string(), "");
}

#[test]
fn backtraces_outside_a_reactor_have_no_task_chain() {
    let bt = current_backtrace();
    assert!(!bt.to_string().is_empty());
    assert!(!bt.to_string().contains("--------"));
    let local = current_backtrace_tasklocal();
    assert!(local.hash() != 0);

    let r: Result<u32, _> = throw_with_backtrace(RuntimeError("boom".to_owned()));
    let e = r.unwrap_err();
    assert!(e.is::<RuntimeError>());
    let bt = e.downcast_ref::<Backtraced<RuntimeError>>().expect("a backtraced error");
    assert!(bt.what().starts_with("boom Backtrace: "), "{}", bt.what());
    assert_eq!(bt.exc.0, "boom");
}

// ---------------------------------------------------------------------------------------
// Allocation failure injection

#[test]
fn the_injector_fails_the_chosen_allocation_point() {
    let inj = local_failure_injector();
    let start = inj.alloc_count();
    inj.fail_after(2);
    assert!(!inj.failed());
    inj.on_alloc_point();
    inj.on_alloc_point();
    assert_eq!(inj.alloc_count(), start + 2);
    let r = catch_unwind(AssertUnwindSafe(|| inj.on_alloc_point()));
    let p = r.unwrap_err();
    assert!(p.downcast_ref::<BadAlloc>().is_some());
    assert!(inj.failed());
    // A failure cancels the countdown: the next points succeed.
    inj.on_alloc_point();
    inj.on_alloc_point();
    assert_eq!(inj.alloc_count(), start + 4);

    // The feature-gated hook does nothing in builds without the feature.
    on_alloc_point();
}

#[test]
fn a_callback_replaces_the_failure_action_for_one_run() {
    let inj = local_failure_injector();
    let hits = Rc::new(Cell::new(0));
    let h = hits.clone();
    inj.run_with_callback(
        NoncopyableFunction::<fn()>::new(move || h.set(h.get() + 1)),
        NoncopyableFunction::<fn()>::new(|| {
            let inj = local_failure_injector();
            inj.fail_after(0);
            inj.on_alloc_point();
            assert!(inj.failed());
            inj.fail_after(1);
            inj.on_alloc_point();
            inj.on_alloc_point();
        }),
    );
    assert_eq!(hits.get(), 2);

    // A callback that starts a nested run gets its own action back afterwards.
    let nested = Rc::new(Cell::new(0));
    let n = nested.clone();
    let outer = Rc::new(Cell::new(0));
    let o = outer.clone();
    inj.run_with_callback(
        NoncopyableFunction::<fn()>::new(move || {
            o.set(o.get() + 1);
            let n2 = n.clone();
            local_failure_injector().run_with_callback(
                NoncopyableFunction::<fn()>::new(move || n2.set(n2.get() + 1)),
                NoncopyableFunction::<fn()>::new(|| {
                    let inj = local_failure_injector();
                    inj.fail_after(0);
                    inj.on_alloc_point();
                }),
            );
        }),
        NoncopyableFunction::<fn()>::new(|| {
            let inj = local_failure_injector();
            inj.fail_after(0);
            inj.on_alloc_point();
            inj.fail_after(0);
            inj.on_alloc_point();
        }),
    );
    assert_eq!(outer.get(), 2);
    assert_eq!(nested.get(), 2);

    // Outside the runs the default action (a BadAlloc panic) is back.
    inj.fail_after(0);
    let r = catch_unwind(AssertUnwindSafe(|| inj.on_alloc_point()));
    assert!(r.unwrap_err().downcast_ref::<BadAlloc>().is_some());
}

#[test]
fn allocation_failures_are_injected_at_every_point() {
    let runs = Rc::new(Cell::new(0));
    let completed = Rc::new(Cell::new(0));
    let (r, c) = (runs.clone(), completed.clone());
    with_allocation_failures(NoncopyableFunction::<fn()>::new(move || {
        r.set(r.get() + 1);
        let inj = local_failure_injector();
        for _ in 0..3 {
            inj.on_alloc_point();
        }
        c.set(c.get() + 1);
    }));
    // Three runs fail, one at each point, and the fourth completes.
    assert_eq!(runs.get(), 4);
    assert_eq!(completed.get(), 1);

    // A panic other than an allocation failure is propagated.
    let r = catch_unwind(AssertUnwindSafe(|| {
        with_allocation_failures(NoncopyableFunction::<fn()>::new(|| panic!("not an alloc")));
    }));
    assert_eq!(panic_message(r.unwrap_err()), "not an alloc");
    local_failure_injector().cancel();
}

#[test]
fn failures_are_traced_when_the_logger_is_verbose() {
    let inj = local_failure_injector();
    // The first failure registers the injector's logger.
    inj.fail_after(0);
    assert!(catch_unwind(AssertUnwindSafe(|| inj.on_alloc_point())).is_err());
    let reg = global_logger_registry();
    let name = "failure_injector".into();
    let before = reg.get_logger_level(&name).expect("the injector's logger is registered");
    reg.set_logger_level(&name, LogLevel::Trace).unwrap();
    inj.fail_after(0);
    let r = catch_unwind(AssertUnwindSafe(|| inj.on_alloc_point()));
    reg.set_logger_level(&name, before).unwrap();
    assert!(r.unwrap_err().downcast_ref::<BadAlloc>().is_some());
    assert!(inj.failed());
}
