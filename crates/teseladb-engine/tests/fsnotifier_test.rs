//! Tests of the inotify-based file system notifier: modify, close, delete, overwrite, child
//! creation and removal, open and move events, and shutting the notifier down.

#![cfg(target_os = "linux")]

#[path = "common/tmpdir.rs"]
mod tmpdir;

use std::path::Path;

use teseladb::engine::core::file::open_file_dma;
use teseladb::engine::core::file_types::OpenFlags;
use teseladb::engine::core::fsnotify::experimental::{
    Fsnotifier, FsnotifierEvent as Event, FsnotifierFlags as Flags, FsnotifierWatch as Watch,
};
use teseladb::engine::core::fstream::{FileOutputStreamOptions, make_file_output_stream};
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::util::file::{remove_file, rename_file};
use teseladb::engine_test_case;
use tmpdir::Tmpdir;

fn find_event(events: &[Event], w: &Watch, mask: Flags, path: Option<Sstring>) -> bool {
    // The event matches when it has one of the flags of `mask` and comes from the watch `w`.
    events.iter().any(|e| {
        (e.mask.bits() & mask.bits()) != 0
            && e.id == w.token()
            && path.as_ref().is_none_or(|p| *p == e.name)
    })
}

fn native(p: &Path) -> &str {
    p.to_str().expect("utf-8 path")
}

fn filename(p: &Path) -> Sstring {
    Sstring::from(p.file_name().and_then(|f| f.to_str()).expect("file name"))
}

engine_test_case!(test_notify_modify_close_delete, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let tmp = Tmpdir::new();
        let mut fsn = Fsnotifier::new()?;

        let p = tmp.path().join("kossa.dat");
        let f = open_file_dma(native(&p), OpenFlags::CREATE | OpenFlags::RW).get()?;
        let w = fsn
            .create_watch(
                &Sstring::from(native(&p)),
                Flags::DELETE_SELF | Flags::MODIFY | Flags::CLOSE,
            )
            .get()?;

        let mut os = make_file_output_stream(f, FileOutputStreamOptions::default()).get()?;
        os.write_str("kossa").get()?;
        os.flush().get()?;

        {
            let events = fsn.wait().get()?;
            assert!(find_event(&events, &w, Flags::MODIFY, None));
        }

        os.close().get()?;

        {
            let events = fsn.wait().get()?;
            assert!(find_event(&events, &w, Flags::CLOSE_WRITE, None));
        }

        remove_file(native(&p)).get()?;

        {
            let events = fsn.wait().get()?;
            assert!(find_event(&events, &w, Flags::DELETE_SELF, None));
            assert!(find_event(&events, &w, Flags::IGNORED, None));
        }
        Ok(())
    })
});

engine_test_case!(test_notify_overwrite, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let tmp = Tmpdir::new();
        let mut fsn = Fsnotifier::new()?;

        let p = tmp.path().join("kossa.dat");

        let write_file = |p: &Path,
                          content: &str|
         -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
            let f = open_file_dma(native(p), OpenFlags::CREATE | OpenFlags::RW).get()?;
            let mut os = make_file_output_stream(f, FileOutputStreamOptions::default()).get()?;
            os.write_str(content).get()?;
            os.flush().get()?;
            os.close().get()?;
            Ok(())
        };

        write_file(&p, "kossa")?;

        let w = fsn
            .create_watch(
                &Sstring::from(native(&p)),
                Flags::DELETE_SELF | Flags::MODIFY | Flags::CLOSE,
            )
            .get()?;

        write_file(&p, "kossabello")?;

        {
            let events = fsn.wait().get()?;
            assert!(find_event(&events, &w, Flags::MODIFY, None));
        }

        write_file(&p, "kossaruffalobill")?;

        {
            let events = fsn.wait().get()?;
            assert!(find_event(&events, &w, Flags::MODIFY, None));
        }

        let p2 = tmp.path().join("tmp.apa");
        write_file(&p2, "le apa")?;

        let w2 = fsn.create_watch(&Sstring::from(native(&tmp.path())), Flags::MOVE_TO).get()?;

        rename_file(native(&p2), native(&p)).get()?;

        {
            let events = fsn.wait().get()?;
            assert!(find_event(&events, &w, Flags::DELETE_SELF, None));
            assert!(find_event(&events, &w2, Flags::MOVE_TO, Some(filename(&p))));
        }
        Ok(())
    })
});

engine_test_case!(test_notify_create_delete_child, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let tmp = Tmpdir::new();
        let mut fsn = Fsnotifier::new()?;

        let p = tmp.path().join("kossa.dat");
        let w = fsn
            .create_watch(
                &Sstring::from(native(&tmp.path())),
                Flags::CREATE_CHILD | Flags::DELETE_CHILD,
            )
            .get()?;

        let mut f = open_file_dma(native(&p), OpenFlags::CREATE | OpenFlags::RW).get()?;

        {
            let events = fsn.wait().get()?;
            assert!(find_event(&events, &w, Flags::CREATE_CHILD, None));
        }

        f.close().get()?;
        remove_file(native(&p)).get()?;

        {
            let events = fsn.wait().get()?;
            assert!(find_event(&events, &w, Flags::DELETE_CHILD, None));
            assert!(!find_event(&events, &w, Flags::IGNORED, None));
        }
        Ok(())
    })
});

engine_test_case!(test_notify_open, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let tmp = Tmpdir::new();
        let mut fsn = Fsnotifier::new()?;

        let p = tmp.path().join("kossa.dat");
        let mut f = open_file_dma(native(&p), OpenFlags::CREATE | OpenFlags::RW).get()?;
        f.close().get()?;

        let w = fsn.create_watch(&Sstring::from(native(&p)), Flags::OPEN).get()?;

        let mut f2 = open_file_dma(native(&p), OpenFlags::RO).get()?;

        {
            let events = fsn.wait().get()?;
            assert!(find_event(&events, &w, Flags::OPEN, None));
        }

        f2.close().get()?;
        Ok(())
    })
});

engine_test_case!(test_notify_move, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let tmp = Tmpdir::new();
        let mut fsn = Fsnotifier::new()?;

        let p = tmp.path().join("kossa.dat");
        let mut f = open_file_dma(native(&p), OpenFlags::CREATE | OpenFlags::RW).get()?;
        f.close().get()?;

        let w = fsn.create_watch(&Sstring::from(native(&tmp.path())), Flags::MOVE).get()?;

        let p2 = tmp.path().join("kossa.mu");

        rename_file(native(&p), native(&p2)).get()?;

        {
            let events = fsn.wait().get()?;
            assert!(find_event(&events, &w, Flags::MOVE_FROM, Some(filename(&p))));
            assert!(find_event(&events, &w, Flags::MOVE_TO, Some(filename(&p2))));
        }

        let tmp2 = Tmpdir::new();
        let p3 = tmp2.path().join("ninja.mission");

        let w2 = fsn.create_watch(&Sstring::from(native(&tmp2.path())), Flags::MOVE).get()?;

        rename_file(native(&p2), native(&p3)).get()?;

        {
            let events = fsn.wait().get()?;
            assert!(find_event(&events, &w, Flags::MOVE_FROM, Some(filename(&p2))));
            assert!(find_event(&events, &w2, Flags::MOVE_TO, Some(filename(&p3))));
        }
        Ok(())
    })
});

engine_test_case!(test_shutdown_notifier, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let tmp = Tmpdir::new();
        let mut fsn = Fsnotifier::new()?;

        let p = tmp.path().join("kossa.dat");
        let mut f = open_file_dma(native(&p), OpenFlags::CREATE | OpenFlags::RW).get()?;
        f.close().get()?;

        let _w =
            fsn.create_watch(&Sstring::from(native(&tmp.path())), Flags::DELETE_CHILD).get()?;

        let fut = fsn.wait();

        fsn.shutdown();

        let events = fut.get()?;

        assert!(events.is_empty());
        Ok(())
    })
});
