//! The CPU path (AES instructions and carry-less multiplication) and the portable path give the
//! same results. On a CPU without the instructions both runs take the portable path.

use teseladb_crypto::aes::Aes;
use teseladb_crypto::modes::AesGcm;
use teseladb_crypto::{accelerated, force_portable};

fn run(keys: &[Vec<u8>], data: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for key in keys {
        let aes = Aes::new(key).expect("key");
        let mut block = [0u8; 16];
        block.copy_from_slice(&data[..16]);
        aes.encrypt_block(&mut block);
        out.push(block.to_vec());
        aes.decrypt_block(&mut block);
        out.push(block.to_vec());
        let gcm = AesGcm::new(aes);
        out.push(gcm.seal(&data[..12], &data[..37], data));
        out.push(gcm.seal(&data[..7], &[], &data[..100]));
    }
    out
}

#[test]
fn the_cpu_path_matches_the_portable_path() {
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x as u8
    };
    let keys: Vec<Vec<u8>> =
        [16, 24, 32, 16, 32].iter().map(|&n| (0..n).map(|_| next()).collect()).collect();
    let data: Vec<u8> = (0..1000).map(|_| next()).collect();
    let fast = run(&keys, &data);
    let detected = accelerated();
    force_portable(true);
    assert!(!accelerated());
    let slow = run(&keys, &data);
    force_portable(false);
    assert_eq!(accelerated(), detected);
    assert_eq!(fast, slow);
}
