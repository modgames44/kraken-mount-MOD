//! A one-time metadata snapshot of a title's existing /user/app and /user/appmeta before the
//! first registration overwrites them (register.rs copies sce_sys, icon0.png and appmeta on top),
//! so a registration that corrupts a title's metadata (save/trophy services read these) can be
//! inspected or restored. Best-effort: the caller logs a failure and registers anyway; the game's
//! own app.pkg/data are never touched. Ported from TITAN CORE (backup.rs, GPL-3.0-or-later),
//! adapted to Kraken Mount's paths; see CREDITS.md.
use std::{fs, io::{self, Read}, path::Path};

const RESERVE: u64 = 256 << 20;
const DEST_BASE: &str = "/data/krakenmount/metadata-backups";

fn copy(src: &Path, dst: &Path, bytes: &mut u64, budget: u64) -> io::Result<()> {
    let m = match fs::symlink_metadata(src) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if m.file_type().is_symlink() {
        return Err(io::Error::other("Metadata snapshot refuses symlinks"));
    }
    if m.is_dir() {
        fs::create_dir_all(dst)?;
        for e in fs::read_dir(src)? {
            let e = e?;
            copy(&e.path(), &dst.join(e.file_name()), bytes, budget)?;
        }
    } else if m.is_file() {
        let next = bytes.checked_add(m.len()).filter(|b| *b <= budget).ok_or_else(|| {
            io::Error::new(io::ErrorKind::StorageFull, format!(
                "Insufficient metadata snapshot space for {} ({} bytes); budget is {} bytes after reserving 256 MiB",
                src.display(), m.len(), budget))
        })?;
        if let Some(p) = dst.parent() {
            fs::create_dir_all(p)?;
        }
        let mut output = fs::OpenOptions::new().write(true).create_new(true).open(dst)?;
        // io::copy streams with bounded buffering; do not allocate the complete metadata file.
        let mut input = fs::File::open(src)?.take(m.len().saturating_add(1));
        let copied = io::copy(&mut input, &mut output)?;
        if copied != m.len() || fs::metadata(src)?.len() != m.len() {
            return Err(io::Error::other(format!("Metadata changed during snapshot: {}", src.display())));
        }
        output.sync_all()?;
        fs::set_permissions(dst, m.permissions())?;
        *bytes = next;
    } else {
        return Err(io::Error::other("Metadata snapshot refuses special files"));
    }
    Ok(())
}

/// Snapshots `app` (its sce_sys and icon0.png) and `meta` into `dest`, atomically (a temp dir
/// renamed into place with a `complete` marker). Returns Ok once a complete snapshot exists;
/// a prior complete snapshot is kept untouched, an incomplete one is preserved for inspection.
fn snapshot(app: &Path, meta: &Path, dest: &Path, budget: u64) -> io::Result<()> {
    if dest.join("complete").is_file() {
        return Ok(());
    }
    if dest.exists() {
        return Err(io::Error::other("Incomplete metadata backup; preserved for inspection"));
    }
    let parent = dest.parent().ok_or_else(|| io::Error::other("Invalid backup directory"))?;
    fs::create_dir_all(parent)?;
    let mut chosen = None;
    for i in 0..1000 {
        let temp = parent.join(format!("pending-{}-{}-{i}", dest.file_name().unwrap().to_string_lossy(), std::process::id()));
        match fs::create_dir(&temp) {
            Ok(()) => { chosen = Some(temp); break; }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    let temp = chosen.ok_or_else(|| io::Error::other("No metadata snapshot directory available"))?;
    let result = (|| {
        let mut bytes = 0;
        copy(&app.join("sce_sys"), &temp.join("app/sce_sys"), &mut bytes, budget)?;
        copy(&app.join("icon0.png"), &temp.join("app/icon0.png"), &mut bytes, budget)?;
        copy(meta, &temp.join("appmeta"), &mut bytes, budget)?;
        fs::write(temp.join("complete"), b"Metadata only. The game's app.pkg and data are not copied.\n")?;
        fs::File::options().write(true).open(temp.join("complete"))?.sync_all()?;
        fs::rename(&temp, dest)
    })();
    if let Err(error) = result {
        // Clean only this new attempt, never original files, completed backups or older pending ones.
        if let Err(cleanup) = fs::remove_dir_all(&temp) {
            return Err(io::Error::new(error.kind(), format!("{error}; snapshot cleanup failed at {}: {cleanup}", temp.display())));
        }
        return Err(error);
    }
    Ok(())
}

/// Snapshot a title's installed metadata before registration overwrites it. `data_free` is the
/// free bytes on /data; a 256 MiB reserve is kept. No-op when the title has no existing metadata.
pub fn before_register(tid: &str, data_free: u64) -> io::Result<()> {
    let app = Path::new("/user/app").join(tid);
    let meta = Path::new("/user/appmeta").join(tid);
    if !app.exists() && !meta.exists() {
        return Ok(());
    }
    snapshot(&app, &meta, &Path::new(DEST_BASE).join(tid), data_free.saturating_sub(RESERVE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, path::PathBuf};
    fn root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("km-backup-{name}-{}", std::process::id()))
    }
    #[test]
    fn snapshot_excludes_package_and_preserves_original_metadata() {
        let r = root("preserve");
        let app = r.join("app");
        let meta = r.join("meta");
        let dest = r.join("backup");
        fs::create_dir_all(app.join("sce_sys")).unwrap();
        fs::create_dir_all(&meta).unwrap();
        fs::write(app.join("app.pkg"), b"owner-package").unwrap();
        fs::write(app.join("sce_sys/param.json"), b"original").unwrap();
        fs::write(meta.join("icon0.png"), b"icon").unwrap();
        snapshot(&app, &meta, &dest, 1024).unwrap();
        assert!(!dest.join("app/app.pkg").exists());
        assert_eq!(fs::read(app.join("app.pkg")).unwrap(), b"owner-package");
        // A second snapshot keeps the first (does not re-copy changed metadata).
        fs::write(app.join("sce_sys/param.json"), b"new").unwrap();
        snapshot(&app, &meta, &dest, 0).unwrap();
        assert_eq!(fs::read(dest.join("app/sce_sys/param.json")).unwrap(), b"original");
        fs::remove_dir_all(r).unwrap();
    }
    #[test]
    fn budget_too_small_fails_cleanly_and_leaves_no_partial() {
        let r = root("budget");
        let app = r.join("app");
        let meta = r.join("meta");
        let dest = r.join("backups/title");
        fs::create_dir_all(app.join("sce_sys")).unwrap();
        fs::create_dir_all(&meta).unwrap();
        fs::write(app.join("sce_sys/param.json"), b"original").unwrap();
        let e = snapshot(&app, &meta, &dest, 2).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::StorageFull);
        assert!(!dest.exists());
        assert_eq!(fs::read(app.join("sce_sys/param.json")).unwrap(), b"original");
        fs::remove_dir_all(r).unwrap();
    }
    #[test]
    fn symlinks_are_refused_without_publishing_a_partial_snapshot() {
        let r = root("symlink");
        let app = r.join("app");
        let meta = r.join("meta");
        let dest = r.join("backups/title");
        fs::create_dir_all(app.join("sce_sys")).unwrap();
        fs::create_dir_all(&meta).unwrap();
        fs::write(r.join("outside"), b"leave intact").unwrap();
        std::os::unix::fs::symlink(r.join("outside"), app.join("sce_sys/link")).unwrap();
        assert!(snapshot(&app, &meta, &dest, 1024).is_err());
        assert!(!dest.exists());
        assert_eq!(fs::read(r.join("outside")).unwrap(), b"leave intact");
        fs::remove_dir_all(r).unwrap();
    }
    #[test]
    fn large_metadata_is_snapshotted_whole() {
        let r = root("large");
        let app = r.join("app");
        let meta = r.join("meta");
        let dest = r.join("backup");
        fs::create_dir_all(&app).unwrap();
        fs::create_dir_all(&meta).unwrap();
        let len = (8 << 20) + 17;
        let mut f = fs::File::create(meta.join("big.dat")).unwrap();
        f.set_len(len).unwrap();
        f.write_all(b"head").unwrap();
        drop(f);
        snapshot(&app, &meta, &dest, len).unwrap();
        assert_eq!(fs::metadata(dest.join("appmeta/big.dat")).unwrap().len(), len);
        let mut b = [0u8; 4];
        fs::File::open(dest.join("appmeta/big.dat")).unwrap().read_exact(&mut b).unwrap();
        assert_eq!(&b, b"head");
        fs::remove_dir_all(r).unwrap();
    }
}
