//! Tests of process spawning: exit status of successful, failing and missing programs,
//! reading a child's output, writing its input, and killing it.

#![cfg(target_os = "linux")]

use std::cell::Cell;
use std::path::Path;
use std::rc::Rc;
use std::sync::LazyLock;
use std::time::Instant;

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{Future, SystemError, make_ready_future};
use teseladb::engine::core::iostream::{ConsumptionResult, ContinueConsuming, StopConsuming};
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::util::log::Logger;
use teseladb::engine::util::process::experimental::{
    Process, SpawnParameters, WaitStatus, spawn_process, spawn_process_with,
};
use teseladb::engine_test_case;

#[allow(dead_code)]
static TESTLOG: LazyLock<std::pin::Pin<Box<Logger>>> =
    LazyLock::new(|| Logger::new("testlog").expect("logger registered once"));

const EXIT_SUCCESS: i32 = 0;
const EXIT_FAILURE: i32 = 1;
const ENOENT: i32 = 2;
const SIGTERM: i32 = 15;

engine_test_case!(test_spawn_success, {
    spawn_process(Path::new("/bin/true")).then(|mut process: Process| process.wait()).then(
        |wstatus: WaitStatus| {
            let WaitStatus::Exited(exit_status) = wstatus else { panic!("exit_status != nullptr") };
            assert_eq!(exit_status.exit_code, EXIT_SUCCESS);
        },
    )
});

engine_test_case!(test_spawn_failure, {
    spawn_process(Path::new("/bin/false")).then(|mut process: Process| process.wait()).then(
        |wstatus: WaitStatus| {
            let WaitStatus::Exited(exit_status) = wstatus else { panic!("exit_status != nullptr") };
            assert_eq!(exit_status.exit_code, EXIT_FAILURE);
        },
    )
});

engine_test_case!(test_spawn_program_does_not_exist, {
    spawn_process(Path::new("non/existent/path")).then_wrapped(|fut: Future<Process>| {
        assert!(fut.failed());
        let e = fut.get_exception();
        let se = e.downcast_ref::<SystemError>().expect("a SystemError");
        assert!(se.code() == ENOENT);
    })
});

/// Consumes the child's output and records whether it matched; the flag is a shared cell,
/// because the consumer handed to `consume` is `'static`.
struct Consumer {
    expected: &'static [u8],
    matched: Rc<Cell<bool>>,
}

impl Consumer {
    fn call(&mut self, buf: TemporaryBuffer<u8>) -> Future<ConsumptionResult<u8>> {
        let b = buf.as_slice();
        if !self.expected.starts_with(b) {
            self.matched.set(false);
            return make_ready_future(StopConsuming::new(TemporaryBuffer::default()).into());
        }
        self.expected = &self.expected[b.len()..];
        if self.expected.is_empty() {
            self.matched.set(true);
            return make_ready_future(StopConsuming::new(TemporaryBuffer::default()).into());
        }
        make_ready_future(ContinueConsuming.into())
    }
}

engine_test_case!(test_spawn_echo, {
    let echo_cmd = "/bin/echo";
    let params = SpawnParameters {
        argv: vec![echo_cmd.into(), "-n".into(), "hello".into(), "world".into()],
        env: vec![],
    };
    make_coroutine(async move {
        let mut p = spawn_process_with(Path::new(echo_cmd), params).await?;
        let mut cout = p.cout();
        let matched = Rc::new(Cell::new(false));
        let r = {
            let mut c = Consumer { expected: b"hello world", matched: matched.clone() };
            cout.consume(move |buf| c.call(buf)).await
        };
        // Wait for the process whatever the outcome.
        let w = p.wait().discard_result().await;
        r?;
        assert!(matched.get());
        w?;
        Ok(())
    })
});

engine_test_case!(test_spawn_input, {
    static TEXT: LazyLock<Sstring> = LazyLock::new(|| Sstring::from("hello world\n"));
    make_coroutine(async move {
        let mut p = spawn_process(Path::new("/bin/cat")).await?;
        let mut cin = p.cin();
        let mut cout = p.cout();
        let body = async {
            let w = async {
                cin.write_str(TEXT.as_str()).await?;
                cin.close().await
            }
            .await;
            if let Err(e) = w {
                if let Some(e) = e.downcast_ref::<SystemError>() {
                    panic!("failed to write to stdin: {}", e);
                }
                return Err(e);
            }
            let echo = match cout.read_exactly(TEXT.size()).await {
                Ok(b) => b,
                Err(e) => {
                    if let Some(e) = e.downcast_ref::<SystemError>() {
                        panic!("failed to read from cout: {}", e);
                    }
                    return Err(e);
                }
            };
            assert_eq!(Sstring::from_bytes(echo.as_slice()), *TEXT);
            Ok(())
        }
        .await;
        // Wait for the process whatever the outcome.
        let wstatus = p.wait().await?;
        let WaitStatus::Exited(exit_status) = wstatus else { panic!("exit_status != nullptr") };
        assert_eq!(exit_status.exit_code, EXIT_SUCCESS);
        body
    })
});

engine_test_case!(test_spawn_kill, {
    let sleep_cmd = "/bin/sleep";
    // sleep for 10s, but terminate it right away.
    let params = SpawnParameters { argv: vec![sleep_cmd.into(), "10".into()], env: vec![] };
    spawn_process_with(Path::new(sleep_cmd), params).then(|mut p: Process| {
        let start = Instant::now();
        if let Err(e) = p.terminate() {
            return teseladb::engine::core::future::make_exception_future(e);
        }
        p.wait().finally(move || drop(p)).then(move |wait_status: WaitStatus| {
            let WaitStatus::Signaled(wait_signaled) = wait_status else {
                panic!("wait_signaled != nullptr")
            };
            assert_eq!(wait_signaled.terminating_signal, SIGTERM);
            let ms = start.elapsed().as_millis();
            // sleep should be terminated in 10ms.
            // pidfd_open(2) may fail and thus p.wait() falls back to
            // waitpid(2) with backoff (at least 20ms).
            // the minimal backoff is added to 10ms, so the test can pass on
            // older kernels as well.
            assert!(ms <= 10 + 20);
        })
    })
});
