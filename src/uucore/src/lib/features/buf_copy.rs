// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

//! This module provides replacement for std::{io,fs}::copy without specialization for Linux
//! related with reflink, but with splice syscall for better throughput.
//! This module is also used as work around for the limitations of Rust's `fs::copy` which
//! does not handle copying special files (e.g pipes, character/block devices).

#[cfg(any(target_os = "linux", target_os = "android"))]
use std::os::fd::AsFd;
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn copy_fast(
    src: &mut (impl std::io::Read + AsFd),
    dest: &mut impl AsFd,
) -> std::io::Result<()> {
    if crate::pipes::splice_unbounded_auto(src, dest)?.is_err() {
        // fall back on writing "without buffering", or order of output would be wrong
        // unrelated for cp /dev/stdin since cp does not have multiple input? <https://github.com/uutils/coreutils/issues/5186>
        // RawWriter also removes io::copy's specialization e.g. copy_file_range which might use reflink
        std::io::copy(src, &mut crate::io::RawWriter(dest))?;
    }
    Ok(())
}

/// Copy opened descriptors, reporting bytes written between bounded transfers.
///
/// `progress(0)` runs before any I/O. Returning an error stops copying and leaves
/// cleanup to the caller. Each invocation owns its pipe, so concurrent calls and
/// failed writes cannot leave buffered data for another invocation. This does not
/// clone extents, preserve holes, copy metadata, or flush the destination.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn copy_with_progress(
    src: &mut (impl std::io::Read + AsFd),
    dest: &mut impl AsFd,
    mut progress: impl FnMut(u64) -> std::io::Result<()>,
) -> std::io::Result<u64> {
    use std::io::Read;
    const CHUNK: usize = 64 * 1024;

    progress(0)?;
    let mut written = 0;
    if let Ok((reader, writer)) = std::io::pipe() {
        loop {
            match crate::pipes::splice(src, &writer, CHUNK) {
                Ok(0) => return Ok(written),
                Ok(count) => {
                    // Even when this uses its read/write fallback, drain_pipe
                    // completes the whole chunk before returning.
                    let fallback = crate::pipes::drain_pipe(&reader, dest, count)?.is_err();
                    written += count as u64;
                    progress(count as u64)?;
                    if fallback {
                        break;
                    }
                }
                Err(rustix::io::Errno::INTR) => continue,
                Err(_) => break,
            }
        }
    }
    // Keep the fallback bounded too. Do not pass callback errors into io::copy:
    // an Interrupted cancellation would be retried instead of returned.
    loop {
        let count = std::io::copy(
            &mut (&mut *src).take(CHUNK as u64),
            &mut crate::io::RawWriter(&mut *dest),
        )?;
        if count == 0 {
            return Ok(written);
        }
        written += count;
        progress(count)?;
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub use std::io::copy as copy_fast;

#[cfg(test)]
#[cfg(any(target_os = "linux", target_os = "android"))] // copy_fast is io::copy on other platforms. nothing to test.
mod tests {
    use super::*;
    use std::fs::File;
    use tempfile::tempdir;

    use {
        std::fs::OpenOptions,
        std::{
            io::{Seek, SeekFrom},
            thread,
        },
    };

    use std::io::{Read, Write};

    fn new_temp_file() -> File {
        let temp_dir = tempdir().unwrap();
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(temp_dir.path().join("file.txt"))
            .unwrap()
    }

    #[test]
    fn test_copy_fast() {
        let mut dest_file = new_temp_file();

        let (mut pipe_read, mut pipe_write) = std::io::pipe().unwrap();
        let data = b"Hello, world!";
        let thread = thread::spawn(move || {
            pipe_write.write_all(data).unwrap();
        });
        copy_fast(&mut pipe_read, &mut dest_file).unwrap();
        thread.join().unwrap();

        // We would have been at the end already, so seek again to the start.
        dest_file.seek(SeekFrom::Start(0)).unwrap();

        let mut buf = Vec::new();
        dest_file.read_to_end(&mut buf).unwrap();

        assert_eq!(buf, data);
    }
}
