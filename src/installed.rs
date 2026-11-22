//! Run PKGs already installed on the PS5 (in `/user/app/<TID>/app.pkg`, or the same under a
//! connected drive) through Kraken Mount's normal cache/AMPR pipeline, without copying or moving
//! them. `prepare` keeps the package in place and only makes it readable; `sources` enumerates the
//! installed debug packages so the scan can treat each as a pkg game. Gated behind the
//! `run_installed` setting. The path check and in-place prepare are ported from TITAN CORE
//! (library.rs `installed_package_path`, installed_access.rs, GPL-3.0-or-later); see CREDITS.md.
use crate::scan::{Kind, Source};
use serde_json::{json, Value};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::{fs, io, path::{Path, PathBuf}};

/// Where in-place receipts for installed sources are kept (never the packages themselves).
pub const RECEIPTS: &str = "/data/krakenmount/installed-sources";

/// The install roots an installed PS5 debug package can live under: internal, and a connected
/// drive's `user/app` with or without the `ps5/` prefix.
const INSTALL_ROOTS: [&str; 5] = [
    "/user/app/", "/mnt/ext0/user/app/", "/mnt/ext0/ps5/user/app/", "/mnt/ext1/user/app/", "/mnt/ext1/ps5/user/app/",
];

/// True when `path` is exactly `<install root>/<valid TID>/app.pkg`.
pub fn installed_package_path(path: &Path) -> bool {
    let p = path.to_string_lossy();
    INSTALL_ROOTS.iter().any(|root| p.strip_prefix(root).is_some_and(|suffix| {
        let mut parts = suffix.split('/');
        matches!((parts.next(), parts.next(), parts.next()),
            (Some(tid), Some("app.pkg"), None) if crate::scan::valid_tid(tid))
    }))
}

/// Keeps an installed package at its original path and makes its read-only sandbox mount
/// accessible: records the original permissions in a receipt, then adds read bits to the package
/// and its directory. Never copies, renames or links the package.
pub fn prepare(path: &Path, tid: &str, size: u64) -> io::Result<PathBuf> {
    if !installed_package_path(path) {
        return Err(io::Error::other("Not a supported installed package path"));
    }
    prepare_at(path, tid, size, Path::new(RECEIPTS))
}

fn prepare_at(path: &Path, tid: &str, size: u64, receipts: &Path) -> io::Result<PathBuf> {
    let parent = path.parent().ok_or_else(|| io::Error::other("Missing package directory"))?;
    if !crate::scan::valid_tid(tid) || parent.file_name().is_none_or(|n| n != tid) || path.file_name().is_none_or(|n| n != "app.pkg") {
        return Err(io::Error::other("Installed package path/title mismatch"));
    }
    let file = fs::symlink_metadata(path)?;
    let dir = fs::symlink_metadata(parent)?;
    if !file.is_file() || file.file_type().is_symlink() || !dir.is_dir() || dir.file_type().is_symlink() || file.len() != size {
        return Err(io::Error::other("Installed source changed or is not a regular package"));
    }
    if fs::symlink_metadata(receipts).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(io::Error::other("Installed source receipt root is a symlink"));
    }
    fs::create_dir_all(receipts)?;
    let receipt = receipts.join(format!("{tid}.json"));
    let previous = match fs::symlink_metadata(&receipt) {
        Ok(m) if !m.is_file() || m.file_type().is_symlink() => return Err(io::Error::other("Invalid installed source receipt")),
        Ok(_) => Some(serde_json::from_slice::<Value>(&fs::read(&receipt)?).map_err(io::Error::other)?),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    if previous.as_ref().is_some_and(|v| v["owner"] != "KRAKEN MOUNT" || v["title_id"] != tid || v["source"] != path.to_string_lossy().as_ref()) {
        return Err(io::Error::other("Installed source receipt belongs to another source"));
    }
    let same = previous.as_ref().is_some_and(|v| v["inode"].as_u64() == Some(file.ino()) && v["device"].as_u64() == Some(file.dev()) && v["size"].as_u64() == Some(size));
    let record = if same {
        previous.unwrap()
    } else {
        json!({"owner": "KRAKEN MOUNT", "title_id": tid, "source": path.to_string_lossy(), "inode": file.ino(), "device": file.dev(), "size": size,
            "original_mode": file.permissions().mode() & 0o7777, "original_directory_mode": dir.permissions().mode() & 0o7777, "mode": "in-place"})
    };
    // Persist original permissions before changing them. Never copy, rename or link the PKG.
    if !same {
        use std::io::Write;
        let temporary = receipts.join(format!("{tid}.next"));
        let mut f = fs::OpenOptions::new().write(true).create_new(true).open(&temporary)?;
        let saved = (|| { f.write_all(record.to_string().as_bytes())?; f.sync_all()?; fs::rename(&temporary, &receipt) })();
        drop(f);
        if let Err(error) = saved {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
    }
    let mode = file.permissions().mode() & 0o7777;
    let directory_mode = dir.permissions().mode() & 0o7777;
    if directory_mode & 0o555 != 0o555 {
        fs::set_permissions(parent, fs::Permissions::from_mode(directory_mode | 0o555))?;
    }
    if mode & 0o444 != 0o444 {
        fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o444))?;
    }
    let now = fs::symlink_metadata(path)?;
    if !now.is_file() || now.file_type().is_symlink() || now.ino() != file.ino() || now.dev() != file.dev() || now.len() != size {
        return Err(io::Error::other("Installed source changed during preparation"));
    }
    Ok(path.to_owned())
}

/// Installed debug packages ready to run: scans the install roots for `<TID>/app.pkg`, peeks each
/// (only PS5 debug pkgs read; retail/system pkgs fail fast), prepares it in place, and builds a pkg
/// `Source`. Skips any title already provided by a scanned source (`known`). Errors are collected
/// per package so a bad one does not stop the rest. Runs in the scan loop (never while paused).
pub fn sources(known: &std::collections::HashSet<String>) -> (Vec<Source>, Vec<(PathBuf, String)>) {
    let mut out = Vec::new();
    let mut errors = Vec::new();
    let mut roots: Vec<PathBuf> = INSTALL_ROOTS.iter().map(|r| PathBuf::from(r.trim_end_matches('/'))).collect();
    // Only real, connected roots (internal /user/app always; external only when the drive is there).
    roots.retain(|r| r.canonicalize().ok().as_deref() == Some(r.as_path()));
    let mut seen_tids = std::collections::HashSet::new();
    for root in &roots {
        let Ok(entries) = fs::read_dir(root) else { continue };
        let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        dirs.sort();
        for dir in dirs {
            let tid = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if !crate::scan::valid_tid(&tid) || known.contains(&tid) || !seen_tids.insert(tid.clone()) {
                continue;
            }
            let pkg = dir.join("app.pkg");
            if !installed_package_path(&pkg) {
                continue;
            }
            let Ok(meta) = fs::symlink_metadata(&pkg) else { continue };
            if !meta.is_file() || meta.file_type().is_symlink() {
                continue;
            }
            // peek_pkg only reads a supported debug pkg header; a retail/system title is skipped silently.
            let (read_tid, name, version) = match crate::cache::peek_pkg(&pkg) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if read_tid != tid || !tid.starts_with("PPSA") {
                continue;
            }
            match prepare(&pkg, &tid, meta.len()) {
                Ok(path) => {
                    log!("run_installed: prepared {tid} ({name}) from {}", path.display());
                    out.push(Source {
                        drive: crate::storage::drive_of(&path),
                        path,
                        kind: Kind::Pkg,
                        title_id: tid,
                        title_name: name,
                        version,
                        size: meta.len(),
                        backport: None,
                    });
                }
                Err(e) => errors.push((pkg, e.to_string())),
            }
        }
    }
    log!("run_installed: scanned {} root(s), prepared {} installed package(s), {} error(s)", roots.len(), out.len(), errors.len());
    (out, errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn root(n: &str) -> PathBuf {
        std::env::temp_dir().join(format!("km-installed-{}-{n}", std::process::id()))
    }
    #[test]
    fn recognises_only_install_root_app_pkg() {
        assert!(installed_package_path(Path::new("/user/app/PPSA12345/app.pkg")));
        assert!(installed_package_path(Path::new("/mnt/ext0/user/app/PPSA12345/app.pkg")));
        assert!(!installed_package_path(Path::new("/user/app/PPSA12345/eboot.bin")));
        assert!(!installed_package_path(Path::new("/data/games/PPSA12345/app.pkg")));
        assert!(!installed_package_path(Path::new("/user/app/not-a-tid/app.pkg")));
    }
    #[test]
    fn prepare_keeps_package_in_place_and_records_original_permissions() {
        let r = root("prepare");
        let p = r.join("PPSA12345/app.pkg");
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, b"installed package bytes").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(p.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
        let before = fs::metadata(&p).unwrap();
        assert_eq!(prepare_at(&p, "PPSA12345", 23, &r.join("receipts")).unwrap(), p);
        let after = fs::metadata(&p).unwrap();
        assert_eq!(before.ino(), after.ino());
        assert_eq!(after.permissions().mode() & 0o444, 0o444);
        assert_eq!(fs::read(&p).unwrap(), b"installed package bytes");
        // A receipt is written recording the original permissions (so they can be restored later).
        assert!(r.join("receipts/PPSA12345.json").is_file());
        fs::remove_dir_all(r).unwrap();
    }
    #[test]
    fn prepare_refuses_wrong_title_size_and_symlink() {
        let r = root("invalid");
        let p = r.join("PPSA12345/app.pkg");
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, b"bytes").unwrap();
        assert!(prepare_at(&p, "PPSA99999", 5, &r.join("receipts")).is_err());
        assert!(prepare_at(&p, "PPSA12345", 99, &r.join("receipts")).is_err());
        fs::remove_dir_all(r).unwrap();
    }
}
