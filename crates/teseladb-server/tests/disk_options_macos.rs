//! On macOS the disk always goes through the disk threads and without `O_DIRECT` (012 FR-007,
//! FR-003): asking for either stops the start with the reason.
#![cfg(target_os = "macos")]

use std::process::Command;

fn start_with(name: &str, extra: &[&str]) -> (Option<i32>, String) {
    let dir =
        std::env::temp_dir().join(format!("teseladb-disk-opts-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let out = Command::new(env!("CARGO_BIN_EXE_teseladb"))
        .args(["start", "--insecure", "--cores", "1", "--port", "0", "--peer-listen"])
        .args(["127.0.0.1:0", "--admin-listen", "127.0.0.1:0", "--dbpath"])
        .arg(dir.join("data"))
        .args(extra)
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    (out.status.code(), String::from_utf8_lossy(&out.stderr).into_owned())
}

#[test]
fn io_uring_and_o_direct_are_refused_with_the_reason() {
    let (code, err) = start_with("uring", &["--io-executor", "uring"]);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("--io-executor uring: io_uring is Linux only"), "{err}");
    let (code, err) = start_with("direct", &["--direct-io", "on"]);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("--direct-io on: the file system"), "{err}");
    // 012 FR-025: without io_uring there is no ring to poll.
    let (code, err) = start_with("sqpoll", &["--io-uring-sqpoll"]);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("--io-uring-sqpoll: the cores do not use io_uring"), "{err}");
}
