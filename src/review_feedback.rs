//! Rooted storage for a review workspace's `feedback.json` only.
use crate::{
    Result, RuntimeError,
    common::{self, FileStatStamp},
};
use serde_json::Value;
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};
use tempfile::NamedTempFile;

const NAME: &str = "feedback.json";
pub const LIMIT: usize = 8 * 1024 * 1024;

pub fn json_content_type(value: &str) -> bool {
    value
        .split(';')
        .next()
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"))
}

struct Snapshot {
    bytes: Option<Vec<u8>>,
    stamp: Option<FileStatStamp>,
}

pub struct FeedbackStore {
    root: PathBuf,
    root_stamp: (u64, u64),
}

impl FeedbackStore {
    pub fn open(workspace: &Path) -> Result<Self> {
        let meta = fs::symlink_metadata(workspace)?;
        if meta.file_type().is_symlink() {
            return Err(RuntimeError::policy(
                "Refusing symlinked review workspace root",
            ));
        }
        if !meta.is_dir() {
            return Err(RuntimeError::not_found(format!(
                "Workspace directory not found: {}",
                workspace.display()
            )));
        }
        let canon = common::safe_canonicalize(workspace)?;
        let canon_meta = fs::symlink_metadata(&canon)?;
        let stamp = FileStatStamp::from_metadata(&canon_meta);
        let root_stamp = (stamp.dev, stamp.ino);
        Ok(Self {
            root: canon,
            root_stamp,
        })
    }

    fn check_root(&self) -> Result<()> {
        let meta = fs::symlink_metadata(&self.root)?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(RuntimeError::conflict(
                "Feedback workspace changed type",
            ));
        }
        let stamp = FileStatStamp::from_metadata(&meta);
        if (stamp.dev, stamp.ino) != self.root_stamp {
            return Err(RuntimeError::conflict(
                "Feedback workspace changed identity",
            ));
        }
        Ok(())
    }

    fn snapshot(&self) -> Result<Snapshot> {
        self.check_root()?;
        let path = self.root.join(NAME);
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                self.check_root()?;
                return Ok(Snapshot {
                    bytes: None,
                    stamp: None,
                });
            }
            Err(err) => return Err(RuntimeError::from(err)),
        };

        if meta.file_type().is_symlink() {
            return Err(RuntimeError::policy("Refusing symlinked feedback.json"));
        }
        if !meta.is_file() {
            return Err(RuntimeError::invalid(
                "feedback.json must be a regular file",
            ));
        }
        let stamp = FileStatStamp::from_metadata(&meta);
        if stamp.nlink > 1 {
            return Err(RuntimeError::policy("Refusing hardlinked feedback.json"));
        }
        if stamp.len > LIMIT as u64 {
            return Err(RuntimeError::coded(
                crate::ErrorKind::InvalidInput,
                "payload_too_large",
                "Feedback exceeds limit",
            ));
        }

        let file = fs::File::open(&path)?;
        let fd_meta = file.metadata()?;
        let fd_stamp = FileStatStamp::from_metadata(&fd_meta);
        if !fd_meta.is_file() || fd_stamp.nlink > 1 || !fd_stamp.same_file_identity(&stamp) {
            return Err(RuntimeError::conflict(
                "Feedback image changed during capture",
            ));
        }

        let mut bytes = Vec::with_capacity(stamp.len.min(LIMIT as u64) as usize);
        file.take(LIMIT as u64 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > LIMIT {
            return Err(RuntimeError::coded(
                crate::ErrorKind::InvalidInput,
                "payload_too_large",
                "Feedback exceeds limit",
            ));
        }

        let post_meta = fs::symlink_metadata(&path)?;
        if post_meta.file_type().is_symlink() || !post_meta.is_file() {
            return Err(RuntimeError::conflict(
                "Feedback image changed during capture",
            ));
        }
        let post_stamp = FileStatStamp::from_metadata(&post_meta);
        if !post_stamp.same_file_identity(&stamp) {
            return Err(RuntimeError::conflict(
                "Feedback image changed during capture",
            ));
        }
        self.check_root()?;

        Ok(Snapshot {
            bytes: Some(bytes),
            stamp: Some(stamp),
        })
    }

    pub fn read(&mut self) -> Result<Vec<u8>> {
        Ok(self.snapshot()?.bytes.unwrap_or_else(|| b"{}".to_vec()))
    }

    pub fn save(&mut self, body: &[u8]) -> Result<()> {
        if body.len() > LIMIT {
            return Err(RuntimeError::coded(
                crate::ErrorKind::InvalidInput,
                "payload_too_large",
                "Feedback exceeds limit",
            ));
        }
        let value: Value = serde_json::from_slice(body)?;
        self.publish(&value, || Ok(()))
    }

    pub fn publish<F>(&mut self, value: &Value, before_commit: F) -> Result<()>
    where
        F: FnOnce() -> Result<()>,
    {
        if !value.is_object() || value.get("reviews").is_none() {
            return Err(RuntimeError::invalid(
                "Expected JSON object with 'reviews' key",
            ));
        }
        let content = (common::pretty(value)? + "\n").into_bytes();
        if content.len() > LIMIT {
            return Err(RuntimeError::coded(
                crate::ErrorKind::InvalidInput,
                "payload_too_large",
                "Feedback exceeds limit",
            ));
        }
        let snapshot = self.snapshot()?;
        let target_mode = snapshot
            .stamp
            .as_ref()
            .map(|s| s.mode & 0o777)
            .unwrap_or(if cfg!(unix) { 0o600 } else { 0o644 });

        let mut tmp = NamedTempFile::new_in(&self.root)?;
        tmp.write_all(&content)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(tmp.path(), fs::Permissions::from_mode(target_mode))?;
        }
        tmp.as_file().sync_all()?;

        before_commit()?;
        self.check_root()?;

        let path = self.root.join(NAME);
        match (fs::symlink_metadata(&path), snapshot.stamp.as_ref()) {
            (Err(err), None) if err.kind() == std::io::ErrorKind::NotFound => {}
            (Ok(live_meta), Some(expected)) => {
                if live_meta.file_type().is_symlink() || !live_meta.is_file() {
                    return Err(RuntimeError::conflict(
                        "Feedback image changed before publication",
                    ));
                }
                let live_stamp = FileStatStamp::from_metadata(&live_meta);
                if live_stamp.nlink > 1 || !live_stamp.same_file_identity(expected) {
                    return Err(RuntimeError::conflict(
                        "Feedback image changed before publication",
                    ));
                }
            }
            (Ok(_), None) | (Err(_), Some(_)) => {
                return Err(RuntimeError::conflict(
                    "Feedback image changed before publication",
                ));
            }
            (Err(err), None) => return Err(RuntimeError::from(err)),
        }

        tmp.persist(&path).map_err(|e| RuntimeError::from(e.error))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ORIGINAL: &[u8] =
        b"{\"reviews\":[{\"run_id\":\"eval-1\",\"feedback\":\"keep me\"}],\"status\":\"in_progress\"}\n";

    fn entries(root: &Path) -> Result<Vec<String>> {
        let mut names = fs::read_dir(root)?
            .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
            .collect::<std::io::Result<Vec<_>>>()?;
        names.sort();
        Ok(names)
    }

    fn make_workspace() -> Result<(tempfile::TempDir, PathBuf)> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("workspace");
        fs::create_dir(&root)?;
        let root = common::safe_canonicalize(&root)?;
        Ok((temp, root))
    }

    #[test]
    fn missing_empty_and_known_feedback_keep_the_existing_wire_layout() -> Result<()> {
        let (_temp, root) = make_workspace()?;
        let mut store = FeedbackStore::open(&root)?;
        assert_eq!(store.read()?, b"{}");
        assert!(!root.join(NAME).exists(), "GET created feedback");
        store.save(br#"{"reviews":[]}"#)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(root.join(NAME))?.permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(
            serde_json::from_slice::<Value>(&store.read()?)?,
            json!({"reviews":[]})
        );
        fs::remove_file(root.join(NAME))?;
        fs::write(root.join(NAME), b"")?;
        assert_eq!(store.read()?, b"");
        let payload = json!({
            "reviews": [
                {"run_id":"eval-1","feedback":"A\nB","timestamp":"2026-11-02T12:00:00Z"},
                {"run_id":"eval-2","feedback":"","timestamp":"2026-11-02T12:00:00Z"}
            ],
            "status":"complete",
            "operator_note":"additional metadata remains intact"
        });
        store.save(&serde_json::to_vec(&payload)?)?;
        assert_eq!(store.read()?, (common::pretty(&payload)? + "\n").as_bytes());
        assert_eq!(entries(&root)?, vec![NAME]);
        Ok(())
    }

    #[test]
    fn invalid_payload_headers_and_bounds_do_not_modify_feedback() -> Result<()> {
        let (_temp, root) = make_workspace()?;
        fs::write(root.join(NAME), ORIGINAL)?;
        let mut store = FeedbackStore::open(&root)?;
        for payload in [
            b"null".as_slice(),
            b"[]",
            b"{}",
            b"{\"status\":\"complete\"}",
            b"not-json",
        ] {
            assert!(store.save(payload).is_err());
            assert_eq!(fs::read(root.join(NAME))?, ORIGINAL);
        }
        assert!(store.save(&vec![b' '; LIMIT + 1]).is_err());
        assert_eq!(fs::read(root.join(NAME))?, ORIGINAL);
        assert!(json_content_type("application/json"));
        assert!(json_content_type("application/json; charset=utf-8"));
        assert!(json_content_type("Application/JSON; charset=utf-8"));
        assert!(!json_content_type(""));
        assert!(!json_content_type("text/plain"));
        assert!(!json_content_type("application/jsonp"));
        assert!(!json_content_type("text/plain; application/json"));
        Ok(())
    }

    #[test]
    fn oversized_existing_feedback_is_refused_without_publication() -> Result<()> {
        let (_temp, root) = make_workspace()?;
        fs::write(root.join(NAME), vec![b'x'; LIMIT])?;
        let mut store = FeedbackStore::open(&root)?;
        assert_eq!(store.read()?.len(), LIMIT);
        fs::write(root.join(NAME), vec![b'x'; LIMIT + 1])?;
        assert!(store.read().is_err());
        assert!(store.save(br#"{"reviews":[]}"#).is_err());
        assert_eq!(fs::metadata(root.join(NAME))?.len(), (LIMIT + 1) as u64);
        Ok(())
    }

    #[test]
    fn hardlinked_feedback_never_reads_or_overwrites_an_outside_file() -> Result<()> {
        let (temp, root) = make_workspace()?;
        let outside = temp.path().join("outside.json");
        fs::write(&outside, ORIGINAL)?;
        fs::hard_link(&outside, root.join(NAME))?;
        let mut store = FeedbackStore::open(&root)?;
        assert!(store.read().is_err());
        assert!(store.save(br#"{"reviews":[],"status":"complete"}"#).is_err());
        assert_eq!(fs::read(&outside)?, ORIGINAL);
        assert_eq!(fs::read(root.join(NAME))?, ORIGINAL);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_feedback_never_reads_or_overwrites_an_outside_file() -> Result<()> {
        let (temp, root) = make_workspace()?;
        let outside = temp.path().join("outside.json");
        fs::write(&outside, ORIGINAL)?;
        std::os::unix::fs::symlink(&outside, root.join(NAME))?;
        let mut store = FeedbackStore::open(&root)?;
        assert!(store.read().is_err());
        assert!(store.save(br#"{"reviews":[],"status":"complete"}"#).is_err());
        assert_eq!(fs::read(&outside)?, ORIGINAL);
        assert!(fs::symlink_metadata(root.join(NAME))?.file_type().is_symlink());
        Ok(())
    }

    #[test]
    fn precommit_byte_change_and_same_bytes_rename_refuse_publication() -> Result<()> {
        let (_temp, root) = make_workspace()?;
        let path = root.join(NAME);
        fs::write(&path, ORIGINAL)?;
        let mut store = FeedbackStore::open(&root)?;
        let changed = b"{\"reviews\":[],\"external\":\"changed\"}\n";
        let err = store
            .publish(&json!({"reviews":[],"status":"complete"}), || {
                fs::write(&path, changed)?;
                Ok(())
            })
            .expect_err("stale image published");
        assert_eq!(err.kind, crate::ErrorKind::Conflict);
        assert_eq!(fs::read(&path)?, changed);
        assert_eq!(entries(&root)?, vec![NAME]);

        let replacement = root.join("replacement.json");
        let permissions = fs::metadata(&path)?.permissions();
        let err2 = store
            .publish(&json!({"reviews":[],"status":"complete"}), || {
                fs::write(&replacement, changed)?;
                fs::set_permissions(&replacement, permissions)?;
                fs::rename(&replacement, &path)?;
                Ok(())
            })
            .expect_err("same-byte replacement accepted");
        assert_eq!(err2.kind, crate::ErrorKind::Conflict);
        assert_eq!(fs::read(&path)?, changed);
        assert_eq!(entries(&root)?, vec![NAME]);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn mode_is_preserved_and_precommit_mode_changes_refuse_publication() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let (_temp, root) = make_workspace()?;
        let path = root.join(NAME);
        fs::write(&path, ORIGINAL)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640))?;
        let mut store = FeedbackStore::open(&root)?;
        store.save(br#"{"reviews":[],"status":"in_progress"}"#)?;
        assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o640);
        let before = fs::read(&path)?;
        let err = store
            .publish(&json!({"reviews":[],"status":"complete"}), || {
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
                Ok(())
            })
            .expect_err("changed mode accepted");
        assert_eq!(err.kind, crate::ErrorKind::Conflict);
        assert_eq!(fs::read(&path)?, before);
        assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
        assert_eq!(entries(&root)?, vec![NAME]);
        Ok(())
    }
}
