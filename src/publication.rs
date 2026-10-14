//! Durable undo journal for a native import spanning files and database rows.
//! Each before-image is durable before its mutation. Incomplete imports are
//! restored on error or on the next sync/clean after a process interruption.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Serialize, Deserialize)]
struct FileUndo {
    path: PathBuf,
    backup: Option<PathBuf>,
}

#[derive(Default, Serialize, Deserialize)]
struct Journal {
    files: Vec<FileUndo>,
    sql: Vec<(PathBuf, String)>,
    opencode: Vec<(String, PathBuf, Option<PathBuf>)>,
}

struct Active {
    directory: PathBuf,
    journal: Journal,
}

thread_local! {
    static ACTIVE: RefCell<Option<Active>> = const { RefCell::new(None) };
}

fn root() -> Result<PathBuf> {
    Ok(crate::index::path()?.with_file_name("pending-imports"))
}

impl Active {
    fn save(&self) -> Result<()> {
        crate::storage::atomic_write(
            &self.directory.join("journal.json"),
            &serde_json::to_vec(&self.journal)?,
        )
    }

    fn file(&mut self, path: &Path) -> Result<()> {
        if self.journal.files.iter().any(|undo| undo.path == path) {
            return Ok(());
        }
        let backup = match std::fs::symlink_metadata(path) {
            Ok(metadata) => {
                anyhow::ensure!(
                    metadata.is_file(),
                    "import target is not a regular file: {}",
                    path.display()
                );
                let backup = self
                    .directory
                    .join(format!("file-{}", self.journal.files.len()));
                std::fs::copy(path, &backup)?;
                std::fs::File::open(&backup)?.sync_all()?;
                Some(backup)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        self.journal.files.push(FileUndo {
            path: path.into(),
            backup,
        });
        self.save()
    }
}

pub struct Publication;

impl Publication {
    pub fn begin() -> Result<Self> {
        Self::begin_at(root()?)
    }

    fn begin_at(base: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&base)?;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let directory = loop {
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = base.join(format!("{}-{n}", std::process::id()));
            match std::fs::create_dir(&path) {
                Ok(()) => break path,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        }
        let active = Active {
            directory,
            journal: Journal::default(),
        };
        active.save()?;
        ACTIVE.with(|slot| -> Result<()> {
            anyhow::ensure!(slot.borrow().is_none(), "nested import publication");
            *slot.borrow_mut() = Some(active);
            Ok(())
        })?;
        Ok(Self)
    }

    pub fn commit(self) -> Result<()> {
        ACTIVE.with(|slot| -> Result<()> {
            let mut active = slot.borrow_mut();
            let journal = active.as_ref().context("no active publication")?;
            // A durable commit marker prevents recovery from undoing a fully
            // published import if removing its backup directory is interrupted.
            crate::storage::atomic_write(&journal.directory.join("committed"), b"ok")?;
            let journal = active.take().context("no active publication")?;
            std::fs::remove_dir_all(journal.directory)?;
            Ok(())
        })
    }
}

impl Drop for Publication {
    fn drop(&mut self) {
        ACTIVE.with(|slot| {
            if let Some(active) = slot.borrow_mut().take() {
                if let Err(e) = rollback(&active) {
                    crate::issues::record(
                        "publication-recovery",
                        &active.directory.to_string_lossy(),
                        &e.to_string(),
                    );
                }
            }
        });
    }
}

pub fn track_file(path: &Path) -> Result<()> {
    ACTIVE.with(|slot| {
        if let Some(active) = slot.borrow_mut().as_mut() {
            active.file(path)?;
        }
        Ok(())
    })
}

pub fn is_active() -> bool {
    ACTIVE.with(|slot| slot.borrow().is_some())
}

pub fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    track_file(path)?;
    crate::storage::atomic_write(path, bytes)
}

/// Store exact SQL before-images, including NULLs and blobs, using SQLite's
/// own quote() function rather than JSON round-tripping database types.
pub fn track_rows(db: &Path, table: &str, column: &str, id: &str) -> Result<()> {
    ACTIVE.with(|slot| -> Result<()> {
        let mut slot = slot.borrow_mut();
        let Some(active) = slot.as_mut() else {
            return Ok(());
        };
        let ident = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
        let columns = crate::proc::sqlite_json(
            db,
            &format!("PRAGMA table_info({});", ident(table)),
            Duration::from_secs(10),
            "publication schema",
        )?;
        anyhow::ensure!(!columns.is_empty(), "missing native table {table}");
        let names: Vec<_> = columns
            .iter()
            .filter_map(|c| c["name"].as_str())
            .map(ident)
            .collect();
        let prefix = format!(
            "INSERT OR REPLACE INTO {} ({}) VALUES(",
            ident(table),
            names.join(",")
        );
        let values = names
            .iter()
            .map(|name| format!("quote({name})"))
            .collect::<Vec<_>>()
            .join("||','||");
        let predicate = format!("{}={}", ident(column), crate::proc::sql_quote_id(id)?);
        let query = format!(
            "SELECT {}||{values}||');' FROM {} WHERE {predicate};",
            crate::proc::sql_quote(&prefix),
            ident(table)
        );
        let rows =
            crate::proc::sqlite_text(db, &query, Duration::from_secs(30), "publication rows")?;
        let script = format!("DELETE FROM {} WHERE {predicate};\n{rows}\n", ident(table));
        active.journal.sql.push((db.into(), script));
        active.save()
    })
}

pub fn track_opencode(id: &str, cwd: &Path) -> Result<()> {
    ACTIVE.with(|slot| -> Result<()> {
        let mut slot = slot.borrow_mut();
        let Some(active) = slot.as_mut() else {
            return Ok(());
        };
        let backup = if crate::scan::opencode_exists(id)? {
            let mut command = std::process::Command::new("opencode");
            command.args(["session", "export", id]);
            let out = crate::proc::output_with_timeout(command, Duration::from_secs(60))?;
            anyhow::ensure!(out.status.success(), "could not back up OpenCode session");
            let value: serde_json::Value = serde_json::from_slice(&out.stdout)
                .context("OpenCode backup export is incomplete")?;
            anyhow::ensure!(
                value.pointer("/info/id").and_then(|v| v.as_str()) == Some(id)
                    && value["messages"].is_array(),
                "OpenCode backup has wrong session shape"
            );
            let snapshot = crate::extract::opencode::opencode_snapshot(id)?;
            anyhow::ensure!(
                value["messages"].as_array().map(Vec::len) == Some(snapshot.identities.len()),
                "OpenCode backup omitted native messages; replacement was refused"
            );
            let path = active.directory.join("opencode-backup.json");
            crate::storage::atomic_write(&path, &out.stdout)?;
            Some(path)
        } else {
            None
        };
        active
            .journal
            .opencode
            .push((id.into(), cwd.into(), backup));
        active.save()
    })
}

fn rollback(active: &Active) -> Result<()> {
    // Restore databases before exposing the old transcript again. Each
    // database's restoration is atomic and can be repeated after interruption.
    let mut databases = std::collections::BTreeMap::<PathBuf, String>::new();
    for (db, script) in &active.journal.sql {
        databases.entry(db.clone()).or_default().push_str(script);
    }
    for (db, script) in databases {
        crate::proc::sqlite_exec_input(
            &db,
            format!("BEGIN IMMEDIATE;\n{script}\nCOMMIT;").into_bytes(),
            Duration::from_secs(30),
            "publication rollback",
        )?;
    }
    for undo in active.journal.files.iter().rev() {
        match &undo.backup {
            Some(backup) => {
                crate::storage::atomic_write(&undo.path, &std::fs::read(backup)?)?;
            }
            None => match std::fs::remove_file(&undo.path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            },
        }
    }
    for (id, cwd, backup) in &active.journal.opencode {
        if crate::scan::opencode_exists(id)? {
            let mut command = std::process::Command::new("opencode");
            command.args(["session", "delete", id]);
            anyhow::ensure!(
                crate::proc::output_with_timeout(command, Duration::from_secs(60))?
                    .status
                    .success(),
                "OpenCode rollback delete failed"
            );
        }
        if let Some(backup) = backup {
            let mut command = std::process::Command::new("opencode");
            command
                .args(["session", "import", "--directory"])
                .arg(cwd)
                .arg(backup);
            let output = crate::proc::output_with_timeout(command, Duration::from_secs(60))?;
            anyhow::ensure!(
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains("Imported session:"),
                "OpenCode rollback import failed; recovery backup was retained"
            );
        }
    }
    std::fs::remove_dir_all(&active.directory)?;
    Ok(())
}

pub fn recover() -> Result<()> {
    recover_at(&root()?)
}

pub fn has_pending() -> Result<bool> {
    let root = root()?;
    if !root.exists() {
        return Ok(false);
    }
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        if path.join("journal.json").is_file() && !path.join("committed").exists() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn recover_at(root: &Path) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(root)? {
        let directory = entry?.path();
        if !directory.is_dir() {
            continue;
        }
        if directory.join("committed").exists() {
            std::fs::remove_dir_all(directory)?;
            continue;
        }
        let bytes = match std::fs::read(directory.join("journal.json")) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        let journal = serde_json::from_slice(&bytes).context("invalid pending import journal")?;
        rollback(&Active { directory, journal })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("xses-publication-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn interrupted_publication_restores_exact_file_bytes() {
        let base = scratch("interrupted");
        let file = base.join("native.jsonl");
        let fresh = base.join("new.jsonl");
        std::fs::write(&file, [0, 255, 10, 13]).unwrap();
        let publication = Publication::begin_at(base.join("pending")).unwrap();
        write(&file, b"replacement").unwrap();
        write(&fresh, b"new import").unwrap();
        ACTIVE.with(|slot| {
            slot.borrow_mut().take();
        });
        std::mem::forget(publication);
        recover_at(&base.join("pending")).unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), [0, 255, 10, 13]);
        assert!(!fresh.exists());
        recover_at(&base.join("pending")).unwrap();
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn failed_later_artifact_restores_earlier_replacements() {
        let base = scratch("failure");
        let file = base.join("rollout");
        let blocked = base.join("blocked");
        std::fs::write(&file, b"previous working import").unwrap();
        std::fs::create_dir(&blocked).unwrap();
        let publication = Publication::begin_at(base.join("pending")).unwrap();
        write(&file, b"replacement").unwrap();
        assert!(write(&blocked, b"sidecar").is_err());
        drop(publication);
        assert_eq!(std::fs::read(file).unwrap(), b"previous working import");
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn rollback_preserves_sqlite_types_and_only_affected_rows() {
        let base = scratch("sqlite");
        let db = base.join("native.sqlite");
        crate::proc::sqlite_exec(&db, "CREATE TABLE native(id TEXT PRIMARY KEY, n INTEGER, b BLOB, t TEXT); INSERT INTO native VALUES('ours',7,X'00FF',NULL); INSERT INTO native VALUES('other',9,NULL,'leave me');", Duration::from_secs(5), "fixture").unwrap();
        let publication = Publication::begin_at(base.join("pending")).unwrap();
        track_rows(&db, "native", "id", "ours").unwrap();
        crate::proc::sqlite_exec(
            &db,
            "UPDATE native SET n=42,b=NULL,t='replacement' WHERE id='ours';",
            Duration::from_secs(5),
            "mutation",
        )
        .unwrap();
        drop(publication);
        let restored = crate::proc::sqlite_text(
            &db,
            "SELECT id,n,hex(b),typeof(t) FROM native ORDER BY id;",
            Duration::from_secs(5),
            "check",
        )
        .unwrap();
        assert_eq!(restored, "other|9||text\nours|7|00FF|null");
        std::fs::remove_dir_all(base).unwrap();
    }
}
