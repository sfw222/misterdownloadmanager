//! Explicit, conservative inspection of download-engine leftovers.
//! Never walk recursively or silently delete files: a `.mdm.part` may be valuable resume data.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::model::Item;

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
        self.bytes + self.companion.as_ref().map_or(0, |(_, size, _)| *size)
    }

    pub fn has_resume_data(&self) -> bool {
        self.companion.is_some()
    }
}

/// Examine only directly selected directories. Unknown or active downloads are not inferred
/// from an extension alone: an existing state file keeps its matching part file protected.
pub fn scan(directories: &[PathBuf], items: &[Item]) -> Vec<Candidate> {
    let protected: HashSet<PathBuf> = items
        .iter()
        .flat_map(|i| {
            [
                i.dest.clone(),
                mdm_engine::naming::part_path(&i.dest),
                mdm_engine::state::state_path(&i.dest),
            ]
        })
        .collect();
    let mut found = Vec::new();
    let mut visited = HashSet::new();
    for dir in directories {
        if !visited.insert(dir.clone()) || !dir.is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if protected.contains(&path) || !is_engine_leftover(&path) {
                continue;
            }
            let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
            if !meta.file_type().is_file() || !old_enough(&meta) {
                continue;
            }
            // Show a matching part and resume state as ONE review item. Never present
            // the state file separately while its part exists.
            if let Some(stem) = path.to_str().and_then(|p| p.strip_suffix(".mdm.json"))
                && PathBuf::from(format!("{stem}.mdm.part")).exists()
            {
                continue;
            }
            let companion = path.to_str().and_then(|p| p.strip_suffix(".mdm.part"))
                .map(|stem| PathBuf::from(format!("{stem}.mdm.json")))
                .filter(|p| p.exists());
            let companion = if let Some(p) = companion {
                let Ok(m) = std::fs::symlink_metadata(&p) else { continue };
                if protected.contains(&p) || !m.file_type().is_file() || !old_enough(&m) {
                    continue;
                }
                let Ok(t) = m.modified() else { continue };
                Some((p, m.len(), t))
            } else {
                None
            };
            if let Ok(modified) = meta.modified() {
                found.push(Candidate { path, bytes: meta.len(), modified, companion });
            }
        }
    }
    found.sort_by(|a, b| a.path.cmp(&b.path));
    found
}

fn is_engine_leftover(path: &Path) -> bool {
    path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
        n.ends_with(".mdm.part") || n.ends_with(".mdm.json")
    })
}

fn old_enough(meta: &std::fs::Metadata) -> bool {
    meta.modified().ok().and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age >= MIN_AGE)
}

/// Recheck before deleting: a file changed since inspection is never removed.
pub fn remove(candidate: &Candidate) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(&candidate.path)?;
    if !meta.file_type().is_file() || !is_engine_leftover(&candidate.path)
        || !old_enough(&meta) || meta.len() != candidate.bytes
        || meta.modified().ok() != Some(candidate.modified)
    {
        return Err(std::io::Error::other("the file changed since the scan"));
    }
    let stem = candidate.path.to_string_lossy();
    if let Some(stem) = stem.strip_suffix(".mdm.part") {
        let partner = PathBuf::from(format!("{stem}.mdm.json"));
        if candidate.companion.as_ref().map(|(p, _, _)| p) != partner.exists().then_some(&partner) {
            return Err(std::io::Error::other("the resume state changed since scanning"));
        }
    } else if let Some(stem) = stem.strip_suffix(".mdm.json")
        && PathBuf::from(format!("{stem}.mdm.part")).exists()
    {
        return Err(std::io::Error::other("the file has a matching download part"));
    }
    if let Some((p, size, modified)) = &candidate.companion {
        let m = std::fs::symlink_metadata(p)?;
        if !m.file_type().is_file() || !old_enough(&m) || m.len() != *size
            || m.modified().ok() != Some(*modified)
        {
            return Err(std::io::Error::other("resume state changed since scanning"));
        }
        // Delete the sidecar first, then the part; an interrupted cleanup still leaves
        // the data file discoverable on the next scan.
        std::fs::remove_file(p)?;
    }
    std::fs::remove_file(&candidate.path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scanner_does_not_touch_files_with_resume_state_or_unrelated_files() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.mdm.part"), b"a").unwrap();
        std::fs::write(d.path().join("a.mdm.json"), b"{}").unwrap();
        std::fs::write(d.path().join("b.part"), b"b").unwrap();
        // A paired file remains visible for review, but not silently deleted.
        assert_eq!(scan(&[d.path().into()], &[]).len(), 0); // newly written: too young
    }

    #[test]
    fn recent_download_parts_are_never_selected() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("new.mdm.part"), b"data").unwrap();
        assert!(scan(&[d.path().into()], &[]).is_empty());
    }

    #[test]
    fn old_orphan_can_be_removed_but_not_after_it_changes() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("old.mdm.part");
        let f = std::fs::File::create(&path).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(
            SystemTime::now() - Duration::from_secs(9 * 24 * 60 * 60),
        )).unwrap();
        let mut found = scan(&[d.path().into()], &[]);
        assert_eq!(found.len(), 1);
        std::fs::write(&path, b"new").unwrap();
        assert!(remove(&found[0]).is_err());
        let f = std::fs::File::options().write(true).open(&path).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(
            SystemTime::now() - Duration::from_secs(9 * 24 * 60 * 60),
        )).unwrap();
        found = scan(&[d.path().into()], &[]);
        assert_eq!(found.len(), 1);
        remove(&found[0]).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn old_resume_pair_is_one_candidate_and_both_files_are_removed() {
        let d = tempfile::tempdir().unwrap();
        let part = d.path().join("old.mdm.part");
        let state = d.path().join("old.mdm.json");
        let old = SystemTime::now() - Duration::from_secs(9 * 24 * 60 * 60);
        for path in [&part, &state] {
            let f = std::fs::File::create(path).unwrap();
            f.set_times(std::fs::FileTimes::new().set_modified(old)).unwrap();
        }
        let found = scan(&[d.path().into()], &[]);
        assert_eq!(found.len(), 1);
        assert!(found[0].has_resume_data());
        remove(&found[0]).unwrap();
        assert!(!part.exists() && !state.exists());
    }
}
