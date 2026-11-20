//! Snapshot persistence: atomic writes, change detection, cross-process lock.
//!
//! FORK ADDITION (see FORK-NOTES.md). Upstream persists with
//! `std::fs::write`, which truncates in place, and neither the MCP server nor
//! the CLI notices when another process has rewritten the file underneath it.
//! The CLI's own doc comment already claims "atomic-rename" — this module is
//! that claim, implemented, plus the stamp both binaries need to notice a
//! foreign write, plus the lock that makes a check-then-write safe.
//!
//! Deliberately dependency-free: the crate's Cargo.toml carries only
//! `serde_json` and `ureq` (both optional), so no `libc`/`fs2` is pulled in for
//! a lock. Atomicity comes from write-temp + rename; the lock is a direct
//! `flock(2)` declaration (unix only — other targets keep the stamp check and
//! no cross-process lock), and lost updates are made IMPOSSIBLE by `StoreLock`
//! + VISIBLE by comparing stamps.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

/// Cheap identity of a file version: (mtime in ns, length).
///
/// A rewrite by another process changes at least one of them, which is all the
/// reload guard needs. Not a cryptographic guarantee — an adversary rewriting
/// within the same nanosecond and the same length would slip through, and that
/// is not the threat model here (two of our own processes, minutes apart).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Stamp {
    pub mtime_ns: i128,
    pub len: u64,
}

impl Stamp {
    pub fn of(path: &str) -> Option<Stamp> {
        let md = fs::metadata(Path::new(path)).ok()?;
        let m = md.modified().ok()?;
        let ns = m
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_nanos() as i128;
        Some(Stamp {
            mtime_ns: ns,
            len: md.len(),
        })
    }
}

/// Write `text` to `path` atomically: temp file in the SAME directory, fsync,
/// then rename over the target.
///
/// Readers therefore see either the previous file or the complete new one,
/// never a truncated prefix — which is exactly what `fs::write` (upstream)
/// exposes during a rewrite. Same-directory temp is required for `rename` to be
/// atomic; the pid suffix keeps two writers from sharing a temp path.
///
/// FORK CHANGE (br-gx5.16): the temp takes the mode of the file it replaces
/// (0600 for a new store). `File::create` uses 0666 & !umask, so every save
/// used to silently rewrite a 0600 snapshot — the whole assertion history — as
/// 0644. `rename` keeps the temp's mode, not the target's, so the mode has to
/// be set at creation time.
pub fn write_atomic(path: &str, text: &str) -> std::io::Result<()> {
    use std::io::Write;

    let p = Path::new(path);
    let dir = p.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    let name = p
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "snapshot".to_string());
    // pid keeps processes apart; the counter keeps two threads of one process
    // (or a fast re-save reusing the pid) apart.
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let mode = fs::metadata(p).ok().map(|md| mode_of(&md)).unwrap_or_default();
    let mode = if mode == 0 { DEFAULT_STORE_MODE } else { mode };

    // `create_new` so a leftover temp from a crashed writer can never be
    // reused (its mode would survive truncation); the second name covers the
    // rare AlreadyExists case instead of failing the save.
    let mut f;
    let mut tmp = dir.join(format!(".{}.tmp.{}.{}", name, std::process::id(), seq));
    match create_private(&tmp, mode) {
        Ok(fh) => f = fh,
        Err(_) => {
            tmp = dir.join(format!(".{}.tmp.{}.{}", name, std::process::id(), nanos()));
            f = create_private(&tmp, mode)?;
        }
    }
    if let Err(e) = f.write_all(text.as_bytes()).and_then(|_| f.sync_all()) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    drop(f);
    if let Err(e) = fs::rename(&tmp, p) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Mode for a store that does not exist yet: private. The snapshot is the
/// user's entire assertion history.
pub const DEFAULT_STORE_MODE: u32 = 0o600;

fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

#[cfg(unix)]
fn mode_of(md: &fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    // rwx only: setuid/setgid/sticky on a data file are not ours to carry.
    md.mode() & 0o777
}

#[cfg(not(unix))]
fn mode_of(_md: &fs::Metadata) -> u32 {
    0
}

#[cfg(unix)]
fn create_private(path: &Path, mode: u32) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    // `.mode()` only applies at creation, which is exactly the case here
    // (`create_new`), and is filtered by the umask — so a store that was
    // already 0600 stays 0600, and a wider mode can never widen further.
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
}

#[cfg(not(unix))]
fn create_private(path: &Path, _mode: u32) -> std::io::Result<fs::File> {
    fs::OpenOptions::new().write(true).create_new(true).open(path)
}

// ---------------------------------------------------------------------------
// Cross-process write lock
// ---------------------------------------------------------------------------

/// How long a writer waits for the store lock before giving up loudly.
///
/// The fork's shell scripts take the same lock with `flock -w 5`, so a value
/// well above the longest of them keeps the engine from failing where the
/// scripts would have waited. Override with `LEMMALOG_LOCK_WAIT_MS`.
pub const DEFAULT_LOCK_WAIT_MS: u64 = 20_000;

/// The lock file for a store: `<store>.lock`, the SAME path the fork's
/// sweep/export/import scripts use (`$SNAP.lock`). Sharing the name is the
/// point — a cron sweep and a CLI write must not interleave either.
pub fn lock_path(store: &str) -> String {
    format!("{store}.lock")
}

/// An exclusive, cross-process lock over one store.
///
/// Held across the WHOLE load-modify-save cycle, not just the write: the lost
/// update (br-gx5.15) happens because two processes both load at stamp S, both
/// find the stamp unchanged, and both then write — a check-then-write race no
/// stamp comparison can close. `flock` is released by the kernel when the
/// process exits, so a crashed writer cannot leave a stale lock behind.
pub struct StoreLock {
    file: fs::File,
    path: String,
}

impl StoreLock {
    /// Take the lock, waiting up to [`DEFAULT_LOCK_WAIT_MS`].
    ///
    /// Err(WouldBlock) means another writer held it for the whole wait: the
    /// caller must report that loudly rather than write without it.
    pub fn acquire(store: &str) -> std::io::Result<StoreLock> {
        let wait = std::env::var("LEMMALOG_LOCK_WAIT_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_LOCK_WAIT_MS);
        let deadline = Instant::now() + Duration::from_millis(wait);
        let lp = lock_path(store);
        loop {
            if let Some(l) = Self::try_acquire(store)? {
                return Ok(l);
            }
            if Instant::now() >= deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    format!(
                        "{lp} is held by another lemmalog writer (waited {wait}ms); \
                         the snapshot was left untouched"
                    ),
                ));
            }
            // Jitter: several CLIs released together must not re-poll in step.
            let jitter = (std::process::id() as u64 + nanos() as u64) % 7;
            std::thread::sleep(Duration::from_millis(2 + jitter));
        }
    }

    /// Take the lock without waiting, or `None` if someone else holds it.
    pub fn try_acquire(store: &str) -> std::io::Result<Option<StoreLock>> {
        let path = lock_path(store);
        let file = open_lock_file(&path)?;
        if lock_exclusive_nb(&file)? {
            Ok(Some(StoreLock { file, path }))
        } else {
            Ok(None)
        }
    }

    pub fn path(&self) -> &str {
        &self.path
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        // Closing the fd would release the lock anyway; unlocking first makes
        // the intent explicit and works even if the File outlives the guard.
        let _ = unlock(&self.file);
    }
}

fn open_lock_file(path: &str) -> std::io::Result<fs::File> {
    #[allow(unused_mut)]
    let mut opts = fs::OpenOptions::new();
    opts.read(true).write(true).create(true);
    // A lock file created here is 0600. An existing one keeps its mode: it
    // holds no data, and the fork's scripts chmod their own.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(DEFAULT_STORE_MODE);
    }
    opts.open(path)
}

#[cfg(unix)]
mod unix_flock {
    //! `flock(2)` without a crate. `LOCK_EX` on an open fd is advisory
    //! exclusion between ALL processes on the host that use the same file —
    //! including `flock(1)` in the fork's shell scripts, which is why the
    //! engine and the sweep scripts share one lock.
    extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }

    pub const LOCK_EX: i32 = 2;
    pub const LOCK_NB: i32 = 4;
    pub const LOCK_UN: i32 = 8;

    pub fn exclusive_nb(fd: i32) -> std::io::Result<bool> {
        // SAFETY: `flock` only inspects the (valid, owned) fd and an integer
        // flag; it has no other precondition and no Rust-side aliasing.
        let rc = unsafe { flock(fd, LOCK_EX | LOCK_NB) };
        if rc == 0 {
            return Ok(true);
        }
        let err = std::io::Error::last_os_error();
        match err.kind() {
            // EWOULDBLOCK: someone else holds it.
            std::io::ErrorKind::WouldBlock => Ok(false),
            // EINTR: a signal arrived mid-wait; report as "not taken" so the
            // caller's loop retries rather than failing the whole write.
            std::io::ErrorKind::Interrupted => Ok(false),
            _ => Err(err),
        }
    }

    pub fn unlock(fd: i32) -> std::io::Result<()> {
        // SAFETY: same as above; LOCK_UN on an owned fd is always sound.
        let rc = unsafe { flock(fd, LOCK_UN) };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

#[cfg(unix)]
fn lock_exclusive_nb(f: &fs::File) -> std::io::Result<bool> {
    use std::os::unix::io::AsRawFd;
    unix_flock::exclusive_nb(f.as_raw_fd())
}

#[cfg(unix)]
fn unlock(f: &fs::File) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    unix_flock::unlock(f.as_raw_fd())
}

// Non-unix fallback: NO cross-process lock. `flock` is POSIX; without it the
// only protection is the stamp check in the binaries, which narrows the window
// but cannot close the check-then-write race. Stated plainly rather than
// pretended: the fork runs on Linux, and a silent no-op lock would be worse
// than a documented gap.
#[cfg(not(unix))]
fn lock_exclusive_nb(_f: &fs::File) -> std::io::Result<bool> {
    Ok(true)
}

#[cfg(not(unix))]
fn unlock(_f: &fs::File) -> std::io::Result<()> {
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("llstore-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn write_atomic_replaces_and_leaves_no_temp() {
        let d = tmpdir();
        let p = d.join("s.snap");
        let ps = p.to_str().unwrap();
        write_atomic(ps, "one").unwrap();
        write_atomic(ps, "two").unwrap();
        assert_eq!(fs::read_to_string(ps).unwrap(), "two");
        let leftovers: Vec<_> = fs::read_dir(&d)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "temp files left behind: {leftovers:?}");
        fs::remove_dir_all(d).ok();
    }

    #[test]
    fn stamp_changes_when_the_file_is_rewritten() {
        let d = tmpdir();
        let p = d.join("s.snap");
        let ps = p.to_str().unwrap();
        write_atomic(ps, "one").unwrap();
        let a = Stamp::of(ps).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_atomic(ps, "two-longer").unwrap();
        let b = Stamp::of(ps).unwrap();
        assert_ne!(a, b);
        assert_eq!(Stamp::of("/nonexistent/x"), None);
        fs::remove_dir_all(d).ok();
    }

    /// br-gx5.16: `File::create` gave the temp the default mode, so a save over
    /// a 0600 store rewrote it 0644 (measured: live `memory.snap` went 0600 →
    /// 0644 and stayed there). The temp must be born with the target's mode.
    #[cfg(unix)]
    #[test]
    fn write_atomic_keeps_the_store_mode() {
        use std::os::unix::fs::PermissionsExt;
        let mode_of = |p: &std::path::Path| {
            fs::metadata(p).unwrap().permissions().mode() & 0o777
        };

        let d = tmpdir();
        let p = d.join("s.snap");
        let ps = p.to_str().unwrap();

        // A brand-new store is private, not 0644.
        write_atomic(ps, "one").unwrap();
        assert_eq!(mode_of(&p), 0o600, "a fresh store must not be world-readable");

        // An existing mode survives a rewrite.
        fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).unwrap();
        write_atomic(ps, "two").unwrap();
        assert_eq!(mode_of(&p), 0o600, "save downgraded 0600");

        // A deliberate wider mode is respected, not forced back to 0644.
        fs::set_permissions(&p, fs::Permissions::from_mode(0o640)).unwrap();
        write_atomic(ps, "three").unwrap();
        assert_eq!(mode_of(&p), 0o640, "save did not carry the target's mode");
        fs::remove_dir_all(d).ok();
    }

    #[cfg(unix)]
    #[test]
    fn store_lock_is_exclusive() {
        let d = tmpdir();
        let p = d.join("s.snap");
        let ps = p.to_str().unwrap().to_string();
        let held = StoreLock::try_acquire(&ps).unwrap().expect("free lock");
        assert!(
            StoreLock::try_acquire(&ps).unwrap().is_none(),
            "two holders of the same store lock"
        );
        assert!(lock_path(&ps).ends_with("s.snap.lock"));
        drop(held);
        assert!(
            StoreLock::try_acquire(&ps).unwrap().is_some(),
            "lock not released on drop"
        );
        fs::remove_dir_all(d).ok();
    }

    /// A writer that never takes the lock must not block the lock holder's
    /// save (the stamp check, not the lock, is what catches that case) — and a
    /// crashed holder must not leave the store locked, because the kernel drops
    /// `flock` with the fd. Only the second half is directly observable here.
    #[cfg(unix)]
    #[test]
    fn lock_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmpdir();
        let p = d.join("s.snap");
        let ps = p.to_str().unwrap().to_string();
        let held = StoreLock::try_acquire(&ps).unwrap().unwrap();
        let mode = fs::metadata(held.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "lock file is {mode:03o}");
        drop(held);
        fs::remove_dir_all(d).ok();
    }

    /// The property that matters: a concurrent reader never observes a partial
    /// file. With `fs::write` this test fails; with temp+rename it must pass
    /// every iteration.
    #[test]
    fn readers_never_see_a_partial_snapshot() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let d = tmpdir();
        let p = d.join("s.snap");
        let ps = p.to_str().unwrap().to_string();
        write_atomic(&ps, "AAAA").unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let reader_path = ps.clone();
        let reader_stop = stop.clone();
        let reader = std::thread::spawn(move || {
            let mut reads = 0u64;
            while !reader_stop.load(Ordering::Relaxed) {
                let s = fs::read_to_string(&reader_path).unwrap();
                assert!(s == "AAAA" || s == "BBBBBBBB", "torn read: {s:?}");
                reads += 1;
            }
            reads
        });

        for i in 0..2000 {
            let body = if i % 2 == 0 { "AAAA" } else { "BBBBBBBB" };
            write_atomic(&ps, body).unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        let reads = reader.join().unwrap();
        assert!(reads > 0);
        fs::remove_dir_all(d).ok();
    }
}
