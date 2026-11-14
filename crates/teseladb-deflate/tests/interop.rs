//! Interoperability with the real `git`: we decompress its loose objects and `git` reads ours
//! (`git cat-file` and `git fsck`). Without `git` on the machine the test does nothing.

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

use teseladb_deflate::zlib;

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().ok()?;
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn hex(b: &[u8]) -> String {
    b.iter().fold(String::new(), |mut out, x| {
        let _ = write!(out, "{x:02x}");
        out
    })
}

#[test]
fn git_objects_both_ways() {
    let dir = std::env::temp_dir().join(format!("teseladb-deflate-interop-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create_dir_all succeeds");
    if git(&dir, &["init", "-q"]).is_none() {
        return;
    }
    // git's objects → us.
    let content: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(dir.join("blob.bin"), &content).expect("write succeeds");
    let oid =
        git(&dir, &["hash-object", "-w", "blob.bin"]).expect("git succeeds").trim().to_owned();
    let loose = std::fs::read(dir.join(".git/objects").join(&oid[..2]).join(&oid[2..]))
        .expect("read succeeds");
    let (raw, used) = zlib::decompress(&loose).expect("decompress succeeds");
    assert_eq!(used, loose.len());
    let header = format!("blob {}\0", content.len());
    assert_eq!(&raw[..header.len()], header.as_bytes());
    assert_eq!(&raw[header.len()..], &content[..]);
    // Our objects → git, at every level.
    for level in [0u8, 1, 6, 9] {
        let body = format!("object written by teseladb-deflate at level {level}\n").repeat(100);
        let raw = [format!("blob {}\0", body.len()).as_bytes(), body.as_bytes()].concat();
        let oid = hex(&teseladb_hash::sha1(&raw));
        let path = dir.join(".git/objects").join(&oid[..2]);
        std::fs::create_dir_all(&path).expect("create_dir_all succeeds");
        std::fs::write(path.join(&oid[2..]), zlib::compress(&raw, level)).expect("write succeeds");
        assert_eq!(git(&dir, &["cat-file", "-p", &oid]).expect("git succeeds"), body);
    }
    git(&dir, &["fsck", "--full", "--strict"]).expect("git succeeds");
    std::fs::remove_dir_all(&dir).expect("remove_dir_all succeeds");
}
