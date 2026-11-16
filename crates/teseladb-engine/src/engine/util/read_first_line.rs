//! Reading one-line system files.
//!
//! Files under `/sys` and `/proc` often hold a single value on their first line (CPU
//! frequencies, block device queue settings, memory limits). [`read_first_line`] reads
//! such a file synchronously with small reads and returns the text before the first
//! newline; [`read_first_line_as`] parses it. Meant for start-up, not for the I/O path.

use std::path::Path;
use std::str::FromStr;

use crate::engine::core::future::ExceptionPtr;
use crate::engine::core::posix::FileDesc;
use crate::engine::core::sstring::Sstring;

/// The bytes read at a time.
const READ_CHUNK: usize = 8;

const O_RDONLY: i32 = 0;
#[cfg(target_os = "linux")]
const O_CLOEXEC: i32 = 0o2000000;
#[cfg(not(target_os = "linux"))]
const O_CLOEXEC: i32 = 0x01000000;

/// Reads `sys_file` and returns its content up to (not including) the first `\n`, or the
/// whole content if there is no newline.
///
/// The file is opened with `O_RDONLY | O_CLOEXEC`, read in 8-byte chunks into a small
/// stack buffer, and closed before returning.
///
/// # Errors
///
/// Returns the system error of `open`, `read` or `close`.
pub fn read_first_line(sys_file: &Path) -> Result<Sstring, ExceptionPtr> {
    let mut file = FileDesc::open(&sys_file.to_string_lossy(), O_RDONLY | O_CLOEXEC, 0)?;
    let mut buf = Sstring::new();
    let mut n: usize = 0;
    loop {
        // A stack buffer avoids an allocation per read.
        let mut tmp = [0u8; READ_CHUNK];
        // SAFETY: `tmp` is a live, writable buffer of exactly `READ_CHUNK` bytes.
        let ret = unsafe { file.read(tmp.as_mut_ptr() as *mut std::ffi::c_void, READ_CHUNK) }?;
        match ret {
            None => {
                // EAGAIN
                if n != 0 {
                    continue;
                }
                break;
            }
            Some(r) => {
                n = r;
                if n > 0 {
                    // Append only the bytes actually read.
                    buf.append_bytes(&tmp[..n]);
                }
            }
        }
        if n == 0 {
            break;
        }
    }
    let end = buf.find_byte(b'\n', 0);
    let value = buf.substr(0, end)?;
    file.close()?;
    Ok(value)
}

/// Reads the first line of `sys_file` (see [`read_first_line`]) and parses it as `Type`.
///
/// # Errors
///
/// Returns the I/O error of [`read_first_line`], or
/// [`BadLexicalCast`](crate::engine::util::conversions::BadLexicalCast) if the line does
/// not parse as `Type`.
pub fn read_first_line_as<Type: FromStr>(sys_file: &Path) -> Result<Type, ExceptionPtr> {
    let s = read_first_line(sys_file)?;
    s.as_str()
        .parse::<Type>()
        .map_err(|_| ExceptionPtr::new(crate::engine::util::conversions::BadLexicalCast))
}
