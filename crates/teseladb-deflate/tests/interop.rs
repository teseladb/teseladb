//! Interoperability with the real `git`: we decompress its loose objects and `git` reads ours
//! (`git cat-file` and `git fsck`). Without `git` on the machine the test does nothing.

use std::path::Path;
use std::process::Command;

use teseladb_deflate::zlib;

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().ok()?;
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[test]
fn git_objects_both_ways() {
    let dir = std::env::temp_dir().join(format!("teseladb-deflate-interop-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    if git(&dir, &["init", "-q"]).is_none() {
        return;
    }
    // git's objects → us.
    let content: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(dir.join("blob.bin"), &content).unwrap();
    let oid = git(&dir, &["hash-object", "-w", "blob.bin"]).unwrap().trim().to_owned();
    let loose = std::fs::read(dir.join(".git/objects").join(&oid[..2]).join(&oid[2..])).unwrap();
    let (raw, used) = zlib::decompress(&loose).unwrap();
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
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join(&oid[2..]), zlib::compress(&raw, level)).unwrap();
        assert_eq!(git(&dir, &["cat-file", "-p", &oid]).unwrap(), body);
    }
    git(&dir, &["fsck", "--full", "--strict"]).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}
