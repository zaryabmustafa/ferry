use ferry_core::entry::{long_path, TICKS_PER_SEC};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::sync_channel;
use std::thread;
use std::time::{Duration, UNIX_EPOCH};
use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{
    MoveFileExW, FILE_FLAG_NO_BUFFERING, FILE_FLAG_SEQUENTIAL_SCAN, FILE_FLAG_WRITE_THROUGH,
    FILE_SHARE_READ, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
};
use xxhash_rust::xxh3::Xxh3;

pub struct Copied {
    #[cfg_attr(not(test), allow(dead_code))] // checked by the copy tests
    pub bytes: u64,
    pub hash: u128,
}

const ALIGN: usize = 4096;
const BUF: usize = 1 << 20;

/// 1 MiB buffer aligned to 4096, as unbuffered I/O requires. Over-allocates a Vec
/// and slices at the aligned offset, so it stays safe and `Send`.
struct Buf {
    v: Vec<u8>,
    off: usize,
}

impl Buf {
    fn new() -> Self {
        let v = vec![0u8; BUF + ALIGN];
        let off = v.as_ptr().align_offset(ALIGN);
        Buf { v, off }
    }
    fn get(&mut self) -> &mut [u8] {
        &mut self.v[self.off..self.off + BUF]
    }
}

/// Shares reading only: a file someone holds open for writing fails to open (a sharing
/// violation, so it waits for the next pass), and no Windows or SMB client can write it while
/// it is copied. Processes local to a NAS are not bound by SMB share modes; the length check in
/// `copy_hashed` and the size/mtime check after it cover those.
fn open_unbuffered_read(p: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ.0)
        .custom_flags(FILE_FLAG_NO_BUFFERING.0 | FILE_FLAG_SEQUENTIAL_SCAN.0)
        .open(long_path(p))
}

/// Read `f` into `h` until EOF or `limit` bytes (a multiple of 4096). Stops after a short,
/// unaligned read: that is EOF, and another unbuffered read at an unaligned offset would fail.
/// `Interrupted` once `cancel` is set; `progress` gets the bytes read so far after each read.
fn hash_into(
    f: &mut File,
    h: &mut Xxh3,
    buf: &mut Buf,
    limit: u64,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64),
) -> io::Result<u64> {
    let mut total = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let want = (limit - total).min(BUF as u64) as usize;
        if want == 0 {
            return Ok(total);
        }
        let n = f.read(&mut buf.get()[..want])?;
        if n > 0 {
            progress(total + n as u64);
        }
        h.update(&buf.get()[..n]);
        total += n as u64;
        if n == 0 || n % ALIGN != 0 {
            return Ok(total);
        }
    }
}

/// Copy `src` to `part` without filling the file cache, hashing in the same pass.
/// An existing `part` is resumed from its length rounded down to 4096.
/// Returns `UnexpectedEof` (leaving `part`) if the bytes read differ from the
/// source's length at open, e.g. a short read or a file changing mid-copy.
pub fn copy_hashed(
    src: &Path,
    part: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64),
) -> io::Result<Copied> {
    let lpart = long_path(part);
    let mut src_f = open_unbuffered_read(src)?;
    let src_len = src_f.metadata()?.len();
    let mut hasher = Xxh3::new();
    let mut first = Buf::new();
    let mut offset = 0u64;

    match fs::metadata(&lpart) {
        Ok(m) => {
            // Clamp to the source length too: a crash after the padded final write
            // leaves zero padding past the true end, which must not be hashed.
            let keep = m.len().min(src_len) & !(ALIGN as u64 - 1);
            OpenOptions::new().write(true).open(&lpart)?.set_len(keep)?;
            let mut kept = Xxh3::new();
            offset = hash_into(&mut open_unbuffered_read(part)?, &mut kept, &mut first, u64::MAX, cancel, &mut |_| {})?;
            // The hash handed on must be the source's bytes, not the laptop's: read the same
            // range from the source. A part that no longer matches it (damaged on the laptop, or
            // a source rewritten in place with the same size and mtime) is dropped and the copy
            // starts over.
            let from_source = hash_into(&mut src_f, &mut hasher, &mut first, offset, cancel, progress)?;
            if from_source != offset || hasher.digest128() != kept.digest128() {
                OpenOptions::new().write(true).open(&lpart)?.set_len(0)?;
                (hasher, offset) = (Xxh3::new(), 0);
            }
            src_f.seek(SeekFrom::Start(offset))?;
            progress(offset);
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }

    let mut dst = OpenOptions::new()
        .write(true)
        .create(true)
        .custom_flags(FILE_FLAG_NO_BUFFERING.0 | FILE_FLAG_WRITE_THROUGH.0)
        .open(&lpart)?;
    dst.seek(SeekFrom::Start(offset))?;

    // Two buffers ping-pong between a reader thread and this (writer) thread,
    // so the next read overlaps the current write.
    let total = thread::scope(|s| -> io::Result<u64> {
        let (empty_tx, empty_rx) = sync_channel::<Buf>(2);
        let (full_tx, full_rx) = sync_channel::<io::Result<(Buf, usize)>>(2);
        empty_tx.send(first).unwrap();
        empty_tx.send(Buf::new()).unwrap();

        s.spawn(move || {
            for mut buf in empty_rx {
                if cancel.load(Ordering::Relaxed) {
                    let _ = full_tx.send(Err(io::ErrorKind::Interrupted.into()));
                    return;
                }
                match src_f.read(buf.get()) {
                    Ok(0) => return,
                    Ok(n) => {
                        if full_tx.send(Ok((buf, n))).is_err() || n % ALIGN != 0 {
                            return;
                        }
                    }
                    Err(e) => {
                        let _ = full_tx.send(Err(e));
                        return;
                    }
                }
            }
        });

        let mut total = offset;
        for msg in full_rx {
            let (mut buf, n) = msg?;
            let b = buf.get();
            hasher.update(&b[..n]);
            let padded = n.next_multiple_of(ALIGN);
            b[n..padded].fill(0);
            dst.write_all(&b[..padded])?;
            total += n as u64;
            progress(total);
            let _ = empty_tx.send(buf);
        }
        Ok(total)
    })?;

    drop(dst);
    OpenOptions::new().write(true).open(&lpart)?.set_len(total)?;
    if total != src_len {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("read {total} bytes, source was {src_len}"),
        ));
    }
    Ok(Copied { bytes: total, hash: hasher.digest128() })
}

/// Stamp `part` with `mtime` (FILETIME ticks) and atomically move it over `dst`.
pub fn finalize(part: &Path, dst: &Path, mtime: i64) -> io::Result<()> {
    const UNIX_EPOCH_TICKS: i64 = 116_444_736_000_000_000;
    let d = mtime - UNIX_EPOCH_TICKS;
    let dur = Duration::new(
        d.unsigned_abs() / TICKS_PER_SEC as u64,
        (d.unsigned_abs() % TICKS_PER_SEC as u64) as u32 * 100,
    );
    let t = if d >= 0 { UNIX_EPOCH + dur } else { UNIX_EPOCH - dur };
    let lpart = long_path(part);
    OpenOptions::new().write(true).open(&lpart)?.set_modified(t)?;

    let wide = |p: &Path| -> Vec<u16> { p.as_os_str().encode_wide().chain([0]).collect() };
    let (from, to) = (wide(&lpart), wide(&long_path(dst)));
    unsafe {
        MoveFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )?;
    }
    Ok(())
}

/// xxh3-128 of a file, read unbuffered; cancel and progress as in `copy_hashed`.
pub fn hash_file(p: &Path, cancel: &AtomicBool, progress: &mut dyn FnMut(u64)) -> io::Result<u128> {
    let mut h = Xxh3::new();
    hash_into(&mut open_unbuffered_read(p)?, &mut h, &mut Buf::new(), u64::MAX, cancel, progress)?;
    Ok(h.digest128())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferry_core::entry::long_path;
    use std::fs;
    use std::os::windows::fs::MetadataExt;
    use std::sync::atomic::Ordering;
    use xxhash_rust::xxh3::xxh3_128;

    fn random_bytes(len: usize, seed: u64) -> Vec<u8> {
        let mut x = seed | 1;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    fn no_cancel() -> AtomicBool {
        AtomicBool::new(false)
    }

    #[test]
    fn copies_and_hashes_odd_sizes() {
        let dir = tempfile::tempdir().unwrap();
        for len in [0usize, 1, 4095, 4096, 4097, (1 << 20) + 13, 3 * (1 << 20)] {
            let src = dir.path().join(format!("src-{len}"));
            let part = dir.path().join(format!("dst-{len}.ferry-part"));
            let data = random_bytes(len, len as u64 + 7);
            fs::write(&src, &data).unwrap();
            let c = copy_hashed(&src, &part, &no_cancel(), &mut |_| {}).unwrap();
            assert_eq!(c.bytes, len as u64, "len {len}");
            assert_eq!(c.hash, xxh3_128(&data), "len {len}");
            assert_eq!(fs::read(&part).unwrap(), data, "len {len}");
        }
    }

    #[test]
    fn resumes_from_partial() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("big");
        let part = dir.path().join("big.ferry-part");
        let data = random_bytes(3 * (1 << 20), 42);
        fs::write(&src, &data).unwrap();

        let cancel = AtomicBool::new(false);
        let r = copy_hashed(&src, &part, &cancel, &mut |_| {
            cancel.store(true, Ordering::Relaxed);
        });
        assert_eq!(r.err().unwrap().kind(), io::ErrorKind::Interrupted);
        assert!(part.exists());
        let partial = fs::metadata(&part).unwrap().len();
        assert!(partial > 0 && partial < data.len() as u64, "partial {partial}");

        let mut last = 0;
        let c = copy_hashed(&src, &part, &no_cancel(), &mut |n| last = n).unwrap();
        assert_eq!(c.bytes, data.len() as u64);
        assert_eq!(last, data.len() as u64);
        assert_eq!(c.hash, xxh3_128(&data));
        assert_eq!(fs::read(&part).unwrap(), data);
    }

    const SHARING_VIOLATION: i32 = 32;

    #[test]
    fn source_cannot_be_written_during_a_copy() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let part = dir.path().join("src.ferry-part");
        let data = random_bytes(3 * (1 << 20), 5);
        fs::write(&src, &data).unwrap();
        let mut tried = None;
        let c = copy_hashed(&src, &part, &no_cancel(), &mut |_| {
            tried.get_or_insert_with(|| fs::OpenOptions::new().write(true).open(&src).err().and_then(|e| e.raw_os_error()));
        })
        .unwrap();
        assert_eq!(tried, Some(Some(SHARING_VIOLATION)), "a writer got in mid-copy");
        assert_eq!(c.hash, xxh3_128(&data));
    }

    #[test]
    fn source_held_open_for_writing_is_not_copied() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("busy.vhdx");
        fs::write(&src, random_bytes(8192, 6)).unwrap();
        let _writer = fs::OpenOptions::new().write(true).open(&src).unwrap(); // std shares read+write
        let e = copy_hashed(&src, &dir.path().join("busy.ferry-part"), &no_cancel(), &mut |_| {}).err().unwrap();
        assert_eq!(e.raw_os_error(), Some(SHARING_VIOLATION));
    }

    #[test]
    fn damaged_part_is_dropped_not_resumed() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let part = dir.path().join("src.ferry-part");
        let data = random_bytes(3 * (1 << 20), 8);
        fs::write(&src, &data).unwrap();
        // A part whose bytes differ from the source's (laptop damage, or the source rewritten in
        // place): resuming would hash the wrong bytes as if they were the source's.
        let mut bad = data[..2 << 20].to_vec();
        bad[12_345] ^= 0xff;
        fs::write(&part, &bad).unwrap();
        let c = copy_hashed(&src, &part, &no_cancel(), &mut |_| {}).unwrap();
        assert_eq!(c.hash, xxh3_128(&data));
        assert_eq!(fs::read(&part).unwrap(), data);
    }

    #[test]
    fn resumes_from_unaligned_and_padded_parts() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let part = dir.path().join("src.ferry-part");
        let data = random_bytes((1 << 20) + 13, 11);
        fs::write(&src, &data).unwrap();

        // Unaligned partial part.
        fs::write(&part, &data[..5000]).unwrap();
        let c = copy_hashed(&src, &part, &no_cancel(), &mut |_| {}).unwrap();
        assert_eq!(c.hash, xxh3_128(&data));
        assert_eq!(fs::read(&part).unwrap(), data);

        // Full copy plus zero padding: crash between padded write and set_len.
        let mut padded = data.clone();
        padded.resize(data.len().next_multiple_of(4096), 0);
        fs::write(&part, &padded).unwrap();
        let mut first = None;
        let c = copy_hashed(&src, &part, &no_cancel(), &mut |n| {
            first.get_or_insert(n);
        })
        .unwrap();
        assert_eq!(first, Some(1 << 20), "progress reported after re-hash");
        assert_eq!(c.hash, xxh3_128(&data));
        assert_eq!(fs::read(&part).unwrap(), data);
    }

    #[test]
    fn finalize_replaces_and_sets_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let part = dir.path().join("dst.ferry-part");
        let dst = dir.path().join("dst");
        fs::write(&src, b"new content").unwrap();
        fs::write(&dst, b"old content that is longer").unwrap();
        copy_hashed(&src, &part, &no_cancel(), &mut |_| {}).unwrap();

        let mtime: i64 = 133_000_000_001_234_567;
        finalize(&part, &dst, mtime).unwrap();
        assert_eq!(fs::read(&dst).unwrap(), b"new content");
        assert!(!part.exists());
        assert_eq!(fs::metadata(&dst).unwrap().last_write_time(), mtime as u64);
    }

    #[test]
    fn long_unicode_path_copies() {
        let dir = tempfile::tempdir().unwrap();
        let mut deep = dir.path().to_path_buf();
        while deep.as_os_str().len() < 300 {
            deep.push("репо-ü-0123456789-abcdefghij");
        }
        fs::create_dir_all(long_path(&deep)).unwrap();
        let src = deep.join("source-файл.bin");
        let part = deep.join("dest-файл.bin.ferry-part");
        let dst = deep.join("dest-файл.bin");
        assert!(dst.as_os_str().len() > 300);
        let data = random_bytes(10_000, 3);
        fs::write(long_path(&src), &data).unwrap();

        let c = copy_hashed(&src, &part, &no_cancel(), &mut |_| {}).unwrap();
        assert_eq!(c.hash, xxh3_128(&data));
        finalize(&part, &dst, 133_000_000_000_000_000).unwrap();
        assert_eq!(fs::read(long_path(&dst)).unwrap(), data);
    }

    #[test]
    fn hash_file_matches_copy_hash() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let part = dir.path().join("src.ferry-part");
        let data = random_bytes((1 << 20) + 4097, 9);
        fs::write(&src, &data).unwrap();
        let c = copy_hashed(&src, &part, &no_cancel(), &mut |_| {}).unwrap();
        let hash = |p: &Path| hash_file(p, &no_cancel(), &mut |_| {}).unwrap();
        assert_eq!(hash(&src), c.hash);
        assert_eq!(hash(&part), c.hash);
        assert_eq!(hash(&src), xxh3_128(&data));
        // Cancelled from its progress: stops before reading on.
        let cancel = no_cancel();
        let mut ticks = vec![];
        let r = hash_file(&src, &cancel, &mut |n| {
            ticks.push(n);
            cancel.store(true, Ordering::Relaxed);
        });
        assert_eq!(r.unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert_eq!(ticks.len(), 1);
    }
}
