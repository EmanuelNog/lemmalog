//! Snapshot persistence: atomic writes and change detection.
//!
//! FORK ADDITION (see FORK-NOTES.md). Upstream persists with
//! `std::fs::write`, which truncates in place, and neither the MCP server nor
//! the CLI notices when another process has rewritten the file underneath it.
//! The CLI's own doc comment already claims "atomic-rename" — this module is
//! that claim, implemented, plus the stamp both binaries need to notice a
//! foreign write.
//!
//! Deliberately dependency-free: the crate's Cargo.toml carries only
//! `serde_json` and `ureq` (both optional), so no `libc`/`fs2` is pulled in for
//! a lock. Atomicity comes from write-temp + rename; lost updates are made
//! VISIBLE by comparing stamps, never silently applied.

use std::fs;
use std::path::Path;

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
pub fn write_atomic(path: &str, text: &str) -> std::io::Result<()> {
    use std::io::Write;

    let p = Path::new(path);
    let dir = p.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    let name = p
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "snapshot".to_string());
    let tmp = dir.join(format!(".{}.tmp.{}", name, std::process::id()));

    let mut f = fs::File::create(&tmp)?;
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
