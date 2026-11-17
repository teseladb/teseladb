#![cfg(all(target_os = "linux", feature = "tls"))]
//! Diffie-Hellman parameters read from a DER file use every byte of the file. Before, the
//! bytes were taken up to the first null, as for a C string, and a DER encoding has null
//! bytes (the leading zero of the prime, for one), so a valid DER file did not load.
//!
//! `tests/data/dh-params.der` holds 1024-bit PKCS #3 parameters; the same bytes load from
//! memory.

use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::net::tls::{DhParams, X509CrtFormat};
use teseladb::engine_test_case;

engine_test_case!(test_dh_params_from_der_file, {
    thread_async(|| -> Result<(), ExceptionPtr> {
        let path = format!("{}/tests/data/dh-params.der", env!("CARGO_MANIFEST_DIR"));
        let bytes = std::fs::read(&path).expect("the parameters file");
        assert!(bytes.contains(&0), "the DER file is expected to hold null bytes");
        // The bytes themselves are valid parameters.
        DhParams::from_blob(&bytes, X509CrtFormat::DER)?;
        // And so is the file.
        let r = DhParams::from_file(&Sstring::from(path.as_str()), X509CrtFormat::DER).get();
        assert!(r.is_ok(), "the DER file did not load: {:?}", r.err());
        Ok(())
    })
});
