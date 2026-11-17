//! The notifier's flag arithmetic, a watch's token, and a notifier shut down while waited for:
//! the wait ends with no events, the notifier is no longer active and a new watch is refused.

#![cfg(target_os = "linux")]

use teseladb::engine::core::fsnotify::experimental::{
    Fsnotifier, FsnotifierFlags as Flags, WatchToken,
};
use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::thread::async_;
use teseladb::engine_test_case;

#[test]
fn flag_arithmetic() {
    let mut f = Flags::from_bits(Flags::MODIFY.bits());
    f |= Flags::CLOSE_WRITE;
    assert_eq!(f.bits(), Flags::MODIFY.bits() | Flags::CLOSE_WRITE.bits());
    assert_eq!((f & Flags::MODIFY).bits(), Flags::MODIFY.bits());
    f &= Flags::CLOSE_WRITE;
    assert_eq!(f.bits(), Flags::CLOSE_WRITE.bits());
}

engine_test_case!(watch_token_and_shutdown, {
    async_(|| -> Result<(), ExceptionPtr> {
        let dir = std::env::temp_dir().join(format!("cov4_rest_fsn_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut fsn = Fsnotifier::new()?;
        assert!(fsn.active());
        let w = fsn
            .create_watch(&Sstring::from(dir.display().to_string().as_str()), Flags::CREATE_CHILD)
            .get()?;
        let token: WatchToken = (&w).into();
        assert_eq!(token, w.token());
        // The shutdown wakes a pending wait with no events (its dummy watch's event is
        // dropped), and the notifier is no longer active from then on; the watch, dropped
        // after it, removes nothing.
        let waiting = fsn.wait();
        fsn.shutdown();
        assert!(waiting.get()?.is_empty());
        assert!(!fsn.active());
        drop(w);
        let e = fsn.create_watch(&Sstring::from("/"), Flags::MODIFY).get();
        let Err(e) = e else { panic!("a watch on a closed notifier") };
        assert!(e.to_string().contains("closed notifier"), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
        Ok(())
    })
});
