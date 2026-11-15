//! Explicit inspection of old, unowned engine sidecars. Never recurse or follow links.
use crate::model::Item;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const MIN_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Clone)]
pub struct Candidate {
    pub path: PathBuf,
    pub bytes: u64,
    modified: SystemTime,
    companion: Option<(PathBuf, u64, SystemTime)>,
}
impl Candidate {
    pub fn total_bytes(&self) -> u64 {
        self.bytes
            .saturating_add(self.companion.as_ref().map_or(0, |(_, n, _)| *n))
    }
    pub fn has_resume_data(&self) -> bool {
        self.companion.is_some()
    }
}

/// Only clone in-memory names on the UI thread. Resolve paths on the scanning worker.
#[derive(Default)]
pub struct Protected {
    files: Vec<PathBuf>,
    folders: Vec<PathBuf>,
    videos: Vec<(PathBuf, String)>,
}
pub fn protect(items: &[Item]) -> Protected {
    let mut protected = Protected::default();
    for item in items {
        for file in std::iter::once(&item.dest).chain(item.video.iter().flat_map(|v| &v.parts)) {
            protected.files.extend([
                file.clone(),
                mdm_engine::naming::part_path(file),
                mdm_engine::state::state_path(file),
            ]);
        }
        if item.torrent.is_some() || item.gallery.is_some() {
            protected.folders.push(item.dest.clone());
        }
        if let Some(remote) = &item.remote {
            protected.folders.push(remote.folder.clone());
        }
        if item.video.is_some()
            && let (Some(parent), Some(stem)) = (item.dest.parent(), item.dest.file_stem())
        {
            protected.videos.push((
                parent.to_owned(),
                format!("{}.", stem.to_string_lossy().to_lowercase()),
            ));
        }
    }
    protected
}
fn key(path: &Path) -> PathBuf {
    let path = std::fs::canonicalize(path)
        .or_else(|_| {
            std::fs::canonicalize(path.parent().unwrap_or(Path::new(".")))
                .map(|p| path.file_name().map_or(p.clone(), |n| p.join(n)))
        })
        .unwrap_or_else(|_| std::path::absolute(path).unwrap_or_else(|_| path.to_owned()));
    #[cfg(windows)]
    let path = PathBuf::from(path.to_string_lossy().to_lowercase());
    path
}
struct Owners {
    files: HashSet<PathBuf>,
    folders: Vec<PathBuf>,
    videos: Vec<(PathBuf, String)>,
}
impl Owners {
    fn new(p: &Protected) -> Self {
        Self {
            files: p.files.iter().map(|p| key(p)).collect(),
            folders: p.folders.iter().map(|p| key(p)).collect(),
            videos: p.videos.iter().map(|(p, s)| (key(p), s.clone())).collect(),
        }
    }
    fn contains(&self, path: &Path) -> bool {
        if self.files.is_empty() && self.folders.is_empty() && self.videos.is_empty() {
            return false;
        }
        let path = key(path);
        self.files.contains(&path)
            || self.folders.iter().any(|p| path.starts_with(p))
            || self.videos.iter().any(|(dir, stem)| {
                path.parent() == Some(dir.as_path())
                    && path
                        .file_name()
                        .is_some_and(|n| n.to_string_lossy().to_lowercase().starts_with(stem))
            })
    }
}
fn linked(meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}
fn safe_directory(path: &Path) -> bool {
    let Ok(path) = std::path::absolute(path) else {
        return false;
    };
    path.ancestors()
        .all(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir() && !linked(&m)))
}
fn companion(path: &Path, suffix: &str, replacement: &str) -> Option<PathBuf> {
    path.to_str()
        .and_then(|p| p.strip_suffix(suffix))
        .map(|p| PathBuf::from(format!("{p}{replacement}")))
}
fn is_engine_leftover(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with(".mdm.part") || n.ends_with(".mdm.json"))
}
fn old_enough(meta: &std::fs::Metadata) -> bool {
    meta.modified()
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age >= MIN_AGE)
}
pub fn scan(directories: &[PathBuf], protected: &Protected) -> Vec<Candidate> {
    let owners = Owners::new(protected);
    let mut visited = HashSet::new();
    let mut found = Vec::new();
    for dir in directories {
        if !safe_directory(dir) || !visited.insert(key(dir)) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !is_engine_leftover(&path) || owners.contains(&path) {
                continue;
            }
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if !meta.is_file() || linked(&meta) || !old_enough(&meta) {
                continue;
            }
            if companion(&path, ".mdm.json", ".mdm.part")
                .is_some_and(|p| std::fs::symlink_metadata(p).is_ok())
            {
                continue;
            }
            let pair = companion(&path, ".mdm.part", ".mdm.json");
            let pair = if let Some(p) = pair {
                match std::fs::symlink_metadata(&p) {
                    Ok(m)
                        if m.is_file() && !linked(&m) && old_enough(&m) && !owners.contains(&p) =>
                    {
                        let Ok(t) = m.modified() else { continue };
                        Some((p, m.len(), t))
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    _ => continue,
                }
            } else {
                None
            };
            if let Ok(modified) = meta.modified() {
                found.push(Candidate {
                    path,
                    bytes: meta.len(),
                    modified,
                    companion: pair,
                });
            }
        }
    }
    found.sort_by(|a, b| a.path.cmp(&b.path));
    found
}
fn unchanged(path: &Path, size: u64, modified: SystemTime) -> std::io::Result<()> {
    let m = std::fs::symlink_metadata(path)?;
    if !m.is_file()
        || linked(&m)
        || !old_enough(&m)
        || m.len() != size
        || m.modified().ok() != Some(modified)
    {
        return Err(std::io::Error::other(
            crate::i18n::t("the file changed since the scan").to_owned(),
        ));
    }
    Ok(())
}
pub fn remove(candidate: &Candidate, protected: &Protected) -> std::io::Result<()> {
    remove_with(candidate, protected, recycle)
}
fn remove_with(
    candidate: &Candidate,
    protected: &Protected,
    recycle: impl FnOnce(&[PathBuf]) -> std::io::Result<()>,
) -> std::io::Result<()> {
    if !candidate.path.parent().is_some_and(safe_directory) || !is_engine_leftover(&candidate.path)
    {
        return Err(std::io::Error::other(
            crate::i18n::t("the download folder changed or is a link").to_owned(),
        ));
    }
    unchanged(&candidate.path, candidate.bytes, candidate.modified)?;
    if let Some(p) = companion(&candidate.path, ".mdm.part", ".mdm.json") {
        let present = match std::fs::symlink_metadata(&p) {
            Ok(_) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(e),
        };
        if candidate.companion.as_ref().map(|(p, _, _)| p) != present.then_some(&p) {
            return Err(std::io::Error::other(
                crate::i18n::t("the resume state changed since scanning").to_owned(),
            ));
        }
    } else if companion(&candidate.path, ".mdm.json", ".mdm.part")
        .is_some_and(|p| std::fs::symlink_metadata(p).is_ok())
    {
        return Err(std::io::Error::other(
            crate::i18n::t("the file has a matching download part").to_owned(),
        ));
    }
    // Validate the entire group before touching either file. The data part goes first,
    // so a sharing violation cannot destroy the resume record of a retained part.
    let mut files = vec![candidate.path.clone()];
    if let Some((p, size, time)) = &candidate.companion {
        unchanged(p, *size, *time)?;
        files.push(p.clone());
    }
    let owners = Owners::new(protected);
    if files.iter().any(|p| owners.contains(p)) {
        return Err(std::io::Error::other(
            crate::i18n::t("the file belongs to a current download").to_owned(),
        ));
    }
    recycle(&files)?;
    if files.iter().any(|p| std::fs::symlink_metadata(p).is_ok()) {
        return Err(std::io::Error::other(
            crate::i18n::t("some files could not be moved to the Recycle Bin").to_owned(),
        ));
    }
    Ok(())
}
fn recycle(files: &[PathBuf]) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        crate::win32::recycle_leftovers(files).map_err(std::io::Error::other)
    }
    #[cfg(unix)]
    {
        if crate::unix::to_trash(files) {
            Ok(())
        } else {
            Err(std::io::Error::other(
                crate::i18n::t("Trash is unavailable; the remaining files were kept").to_owned(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn old(path: &Path) {
        std::fs::write(path, b"data").unwrap();
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(SystemTime::now() - Duration::from_secs(9 * 86400)),
            )
            .unwrap();
    }
    fn delete_fixture(files: &[PathBuf]) -> std::io::Result<()> {
        for p in files {
            std::fs::remove_file(p)?;
        }
        Ok(())
    }
    #[test]
    fn old_orphan_is_not_automatically_removed() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("old.mdm.part");
        old(&p);
        let c = scan(&[d.path().into()], &Protected::default());
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].total_bytes(), 4);
        assert!(p.exists());
        remove_with(&c[0], &Protected::default(), delete_fixture).unwrap();
        assert!(!p.exists());
        assert!(remove_with(&c[0], &Protected::default(), delete_fixture).is_err());
        assert!(scan(&[d.path().into()], &Protected::default()).is_empty());
    }
    #[test]
    fn recent_and_unrelated_files_are_ignored() {
        let d = tempfile::tempdir().unwrap();
        for n in [
            "new.mdm.part",
            "new.mdm.json",
            "other.part",
            "example.json",
            "document.txt",
            "movie.mp4",
        ] {
            std::fs::write(d.path().join(n), b"x").unwrap();
        }
        assert!(scan(&[d.path().into()], &Protected::default()).is_empty());
    }
    #[test]
    fn pair_is_grouped_and_both_files_are_removed() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("old.mdm.part");
        let s = d.path().join("old.mdm.json");
        old(&p);
        old(&s);
        let c = scan(&[d.path().into()], &Protected::default());
        assert_eq!(c.len(), 1);
        assert!(c[0].has_resume_data());
        assert_eq!(c[0].total_bytes(), 8);
        remove_with(&c[0], &Protected::default(), delete_fixture).unwrap();
        assert!(!p.exists() && !s.exists());
    }
    #[test]
    fn a_changed_part_or_state_is_kept() {
        for name in ["old.mdm.part", "old.mdm.json"] {
            let d = tempfile::tempdir().unwrap();
            let p = d.path().join("old.mdm.part");
            let s = d.path().join("old.mdm.json");
            old(&p);
            old(&s);
            let c = scan(&[d.path().into()], &Protected::default());
            std::fs::write(d.path().join(name), b"changed").unwrap();
            assert!(remove_with(&c[0], &Protected::default(), delete_fixture).is_err());
            assert!(p.exists() && s.exists());
        }
    }
    #[test]
    fn a_new_owner_including_the_final_filename_prevents_removal() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("old.mdm.part");
        old(&p);
        let c = scan(&[d.path().into()], &Protected::default());
        let item = Item::new(
            1,
            "http://example.test/file".parse().unwrap(),
            p.clone(),
            crate::model::Category::General,
        );
        let protected = protect(&[item]);
        assert!(remove_with(&c[0], &protected, delete_fixture).is_err());
        assert!(p.exists());
    }
    #[test]
    fn exact_and_case_alias_owners_are_kept() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("tracked.mdm.part");
        old(&p);
        let protected = Protected {
            files: vec![p.clone()],
            ..Default::default()
        };
        assert!(scan(&[d.path().into()], &protected).is_empty());
        #[cfg(windows)]
        {
            let protected = Protected {
                files: vec![d.path().join("TRACKED.MDM.PART")],
                ..Default::default()
            };
            assert!(scan(&[d.path().into()], &protected).is_empty());
        }
    }
    #[test]
    fn state_created_after_scan_is_kept() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("old.mdm.part");
        old(&p);
        let c = scan(&[d.path().into()], &Protected::default());
        old(&d.path().join("old.mdm.json"));
        assert!(remove_with(&c[0], &Protected::default(), delete_fixture).is_err());
        assert!(p.exists());
    }
    #[test]
    fn no_recursion_and_missing_folders_are_safe() {
        let d = tempfile::tempdir().unwrap();
        let sub = d.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        old(&sub.join("old.mdm.part"));
        assert!(
            scan(
                &[d.path().into(), d.path().join("absent")],
                &Protected::default()
            )
            .is_empty()
        );
    }
    #[test]
    fn duplicate_directories_are_scanned_once() {
        let d = tempfile::tempdir().unwrap();
        old(&d.path().join("old.mdm.part"));
        assert_eq!(
            scan(
                &[d.path().into(), d.path().join(".")],
                &Protected::default()
            )
            .len(),
            1
        );
    }
    #[test]
    fn failed_recycling_does_not_report_success_or_erase_resume_data() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("old.mdm.part");
        let s = d.path().join("old.mdm.json");
        old(&p);
        old(&s);
        let c = scan(&[d.path().into()], &Protected::default());
        assert!(
            remove_with(&c[0], &Protected::default(), |_| Err(
                std::io::Error::other("Recycle Bin unavailable")
            ))
            .is_err()
        );
        assert!(p.exists() && s.exists());
    }
    #[test]
    fn paused_tasks_and_video_intermediates_are_protected() {
        let d = tempfile::tempdir().unwrap();
        let dest = d.path().join("movie.mp4");
        let mut item = Item::new(
            1,
            "http://example.test/file".parse().unwrap(),
            dest.clone(),
            crate::model::Category::Video,
        );
        item.status = crate::model::Status::Stopped;
        item.video = Some(crate::model::VideoSpec {
            selector: "b".into(),
            subs: Default::default(),
            parts: vec![],
        });
        for p in [
            mdm_engine::naming::part_path(&dest),
            mdm_engine::state::state_path(&dest),
            d.path().join("movie.f137.mp4.mdm.part"),
        ] {
            old(&p);
        }
        assert!(scan(&[d.path().into()], &protect(&[item])).is_empty());
    }
    #[test]
    fn a_tracked_folder_payload_is_not_treated_as_engine_garbage() {
        let d = tempfile::tempdir().unwrap();
        let mut item = Item::new(
            1,
            "http://example.test/gallery".parse().unwrap(),
            d.path().to_owned(),
            crate::model::Category::General,
        );
        item.gallery = Some(crate::model::GallerySpec { limit: None });
        old(&d.path().join("payload.mdm.part"));
        assert!(scan(&[d.path().into()], &protect(&[item])).is_empty());
    }
    #[cfg(windows)]
    #[test]
    fn junction_roots_are_never_scanned() {
        use std::os::windows::process::CommandExt;
        let d = tempfile::tempdir().unwrap();
        let target = d.path().join("target");
        std::fs::create_dir(&target).unwrap();
        old(&target.join("foreign.mdm.part"));
        let link = d.path().join("alias");
        let script = d.path().join("junction.ps1");
        std::fs::write(&script,b"param([string]$link,[string]$target)\n$ErrorActionPreference='Stop'\nNew-Item -ItemType Junction -Path $link -Target $target | Out-Null\n").unwrap();
        let result = std::process::Command::new("powershell.exe")
            // Only this self-created fixture script may run when Windows PowerShell's
            // default policy is Restricted; no user or machine policy is changed.
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(&script)
            .arg(&link)
            .arg(&target)
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(scan(&[link], &Protected::default()).is_empty());
        assert!(target.join("foreign.mdm.part").exists());
    }
    #[cfg(windows)]
    #[test]
    fn locked_part_keeps_both_files_in_the_real_recycler() {
        use std::os::windows::fs::OpenOptionsExt;
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("locked.mdm.part");
        let s = d.path().join("locked.mdm.json");
        old(&p);
        old(&s);
        let c = scan(&[d.path().into()], &Protected::default());
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(3)
            .open(&p)
            .unwrap();
        assert!(remove(&c[0], &Protected::default()).is_err());
        assert!(p.exists() && s.exists());
        drop(lock);
    }
    #[cfg(windows)]
    #[test]
    fn a_real_recycle_bin_operation_succeeds() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("mdm-leftover-recycle-smoke.mdm.part");
        old(&p);
        let c = scan(&[d.path().into()], &Protected::default());
        remove(&c[0], &Protected::default()).unwrap();
        assert!(!p.exists());
    }
}
