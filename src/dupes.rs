//! Find byte-identical copies of models and move unwanted ones to a trash
//! folder inside the library.
//!
//! Disk-friendly: only files that share an exact size with another file are
//! read, and each file's hash is stored, so a later run reads only new or
//! changed candidates. The scanner clears the hash whenever a file changes.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};
use serde::Serialize;

use crate::scanner::ScanProgress;

/// Folder (at the library root) that "deleted" duplicates are moved into.
/// The scanner and extractor never descend into it.
pub const TRASH_DIR: &str = ".org3d-trash";

/// Directories org3d creates inside the library and must never index.
pub fn is_internal_dir(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy();
    name == TRASH_DIR || name.ends_with(".org3d-partial")
}

/// walkdir filter: skip our own trash and partial-extract folders.
pub fn walk_filter(e: &walkdir::DirEntry) -> bool {
    !(e.depth() > 0 && e.file_type().is_dir() && is_internal_dir(e.file_name()))
}

const MAX_FAILED_LISTED: usize = 50;

#[derive(Debug, Default, Clone, Serialize)]
pub struct HashStats {
    /// Files that share their size with another file.
    pub candidates: usize,
    /// Candidates read and hashed this run.
    pub hashed: usize,
    /// Candidates whose stored hash was still valid (not read).
    pub cached: usize,
    /// Candidates skipped because they changed or vanished since the scan.
    pub stale: usize,
    pub errors: usize,
    pub failed: Vec<String>,
    /// Duplicate groups found after hashing.
    pub groups: usize,
    /// Extra copies (files beyond one per group).
    pub extra_copies: usize,
    pub wasted_bytes: i64,
}

struct Candidate {
    id: i64,
    path: String,
    size: i64,
    mtime: Option<i64>,
}

fn file_mtime(md: &std::fs::Metadata) -> Option<i64> {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
}

/// Hash every present file that shares its size with another present file
/// and doesn't have a hash yet.
pub fn hash_candidates(
    conn: &Connection,
    root: &Path,
    progress: Option<&ScanProgress>,
) -> Result<HashStats> {
    let mut stats = HashStats::default();
    let (todo, cached): (Vec<Candidate>, usize) = {
        let mut stmt = conn.prepare(
            "SELECT id, path, file_size, mtime, content_hash IS NOT NULL FROM models
             WHERE missing_since IS NULL AND file_size > 0 AND file_size IN (
                 SELECT file_size FROM models WHERE missing_since IS NULL
                 GROUP BY file_size HAVING COUNT(*) > 1)
             ORDER BY path",
        )?;
        let mut todo = Vec::new();
        let mut cached = 0;
        let rows = stmt.query_map([], |r| {
            Ok((
                Candidate {
                    id: r.get(0)?,
                    path: r.get(1)?,
                    size: r.get(2)?,
                    mtime: r.get(3)?,
                },
                r.get::<_, bool>(4)?,
            ))
        })?;
        for row in rows {
            let (c, has_hash) = row?;
            if has_hash {
                cached += 1;
            } else {
                todo.push(c);
            }
        }
        (todo, cached)
    };
    stats.candidates = todo.len() + cached;
    stats.cached = cached;
    if let Some(p) = progress {
        p.reset();
        p.hashing.store(true, Ordering::Relaxed);
        p.total.store(todo.len(), Ordering::Relaxed);
    }

    for c in todo {
        match hash_one(&c) {
            Ok(Some(hash)) => {
                // Only store if the row still describes the file we read.
                conn.execute(
                    "UPDATE models SET content_hash = ?1
                     WHERE id = ?2 AND file_size = ?3 AND mtime IS ?4",
                    params![hash, c.id, c.size, c.mtime],
                )?;
                stats.hashed += 1;
            }
            Ok(None) => stats.stale += 1,
            Err(e) => {
                stats.errors += 1;
                let shown = Path::new(&c.path)
                    .strip_prefix(root)
                    .unwrap_or(Path::new(&c.path))
                    .display()
                    .to_string();
                tracing::warn!("hash {}: {e:#}", c.path);
                if stats.failed.len() < MAX_FAILED_LISTED {
                    stats.failed.push(format!("{shown}: {}", e.root_cause()));
                }
            }
        }
        if let Some(p) = progress {
            p.done.fetch_add(1, Ordering::Relaxed);
        }
    }

    let groups = groups(conn)?;
    stats.groups = groups.len();
    stats.extra_copies = groups.iter().map(|g| g.files.len() - 1).sum();
    stats.wasted_bytes = groups.iter().map(|g| g.wasted_bytes()).sum();
    tracing::info!(
        "duplicates: {} candidates, {} hashed, {} cached, {} groups, {} extra copies",
        stats.candidates,
        stats.hashed,
        stats.cached,
        stats.groups,
        stats.extra_copies
    );
    Ok(stats)
}

/// `Ok(None)` if the file no longer matches what the scan recorded.
fn hash_one(c: &Candidate) -> Result<Option<String>> {
    let mut f = std::fs::File::open(&c.path)?;
    let md = f.metadata()?;
    if md.len() as i64 != c.size || file_mtime(&md) != c.mtime {
        return Ok(None);
    }
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(Some(hasher.finalize().to_hex().to_string()))
}

#[derive(Debug, Clone, Serialize)]
pub struct DupeFile {
    pub id: i64,
    pub path: String,
    pub filename: String,
    pub title: Option<String>,
    pub mtime: Option<i64>,
    pub project_id: Option<i64>,
    pub project_name: Option<String>,
    pub thumbnail_path: Option<String>,
    /// Suggested copy to keep (first in the list).
    pub keep: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DupeGroup {
    pub hash: String,
    pub file_size: i64,
    pub files: Vec<DupeFile>,
}

impl DupeGroup {
    pub fn wasted_bytes(&self) -> i64 {
        self.file_size * (self.files.len() as i64 - 1)
    }
}

/// Groups of present files with identical content, largest waste first.
/// Within a group the suggested keeper comes first: the copy in a project,
/// then the shortest path, then the oldest.
pub fn groups(conn: &Connection) -> Result<Vec<DupeGroup>> {
    let mut stmt = conn.prepare(
        "SELECT content_hash, file_size, id, path, filename, title, mtime,
                project_id, project_name, thumbnail_path
         FROM models
         WHERE missing_since IS NULL AND content_hash IN (
             SELECT content_hash FROM models
             WHERE missing_since IS NULL AND content_hash IS NOT NULL
             GROUP BY content_hash HAVING COUNT(*) > 1)",
    )?;
    let mut by_hash: HashMap<String, DupeGroup> = HashMap::new();
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            DupeFile {
                id: r.get(2)?,
                path: r.get(3)?,
                filename: r.get(4)?,
                title: r.get(5)?,
                mtime: r.get(6)?,
                project_id: r.get(7)?,
                project_name: r.get(8)?,
                thumbnail_path: r.get(9)?,
                keep: false,
            },
        ))
    })?;
    for row in rows {
        let (hash, size, file) = row?;
        by_hash
            .entry(hash.clone())
            .or_insert_with(|| DupeGroup {
                hash,
                file_size: size,
                files: Vec::new(),
            })
            .files
            .push(file);
    }
    let mut groups: Vec<DupeGroup> = by_hash.into_values().collect();
    for g in &mut groups {
        g.files.sort_by(|a, b| {
            (a.project_id.is_none(), a.path.len(), a.mtime, &a.path).cmp(&(
                b.project_id.is_none(),
                b.path.len(),
                b.mtime,
                &b.path,
            ))
        });
        g.files[0].keep = true;
    }
    groups.sort_by(|a, b| {
        b.wasted_bytes()
            .cmp(&a.wasted_bytes())
            .then_with(|| a.files[0].path.cmp(&b.files[0].path))
    });
    Ok(groups)
}

/// Present files that share a size with another file but haven't been
/// compared yet. Non-zero means "Find duplicates" has work to do.
pub fn unhashed_candidates(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM models
         WHERE missing_since IS NULL AND file_size > 0 AND content_hash IS NULL
           AND file_size IN (SELECT file_size FROM models WHERE missing_since IS NULL
                             GROUP BY file_size HAVING COUNT(*) > 1)",
        [],
        |r| r.get(0),
    )?)
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct TrashOutcome {
    pub moved: usize,
    pub freed_bytes: i64,
    pub failed: Vec<String>,
}

/// Move the given duplicate copies into `<root>/.org3d-trash/<relative path>`
/// and drop them from the index. Refuses, per file, anything that would leave
/// a group with no copy on disk, isn't a known duplicate, or changed since it
/// was hashed. The kept copy inherits a project and thumbnail if it had none.
pub fn trash(
    conn: &Connection,
    root: &Path,
    thumb_dir: &Path,
    ids: &[i64],
) -> Result<TrashOutcome> {
    let mut out = TrashOutcome::default();
    let groups = groups(conn)?;
    let mut requested: Vec<i64> = ids.to_vec();
    requested.sort_unstable();
    requested.dedup();

    for g in &groups {
        let (doomed, kept): (Vec<&DupeFile>, Vec<&DupeFile>) = g
            .files
            .iter()
            .partition(|f| requested.binary_search(&f.id).is_ok());
        if doomed.is_empty() {
            continue;
        }
        let show = |f: &DupeFile| rel(root, &f.path);
        // A copy we keep must really be there, unchanged, before we touch
        // any other copy.
        let Some(keeper) = kept
            .iter()
            .find(|k| matches_disk(&k.path, g.file_size, k.mtime))
        else {
            let why = if kept.is_empty() {
                "every copy was selected; keep at least one"
            } else {
                "the copy to keep is missing or changed on disk; rescan first"
            };
            for f in &doomed {
                out.failed.push(format!("{}: {why}", show(f)));
            }
            continue;
        };
        for f in doomed {
            match trash_one(conn, root, thumb_dir, g, f, keeper) {
                Ok(()) => {
                    out.moved += 1;
                    out.freed_bytes += g.file_size;
                }
                Err(e) => {
                    tracing::warn!("trash {}: {e:#}", f.path);
                    out.failed.push(format!("{}: {}", show(f), friendly(&e)));
                }
            }
        }
    }

    let known: std::collections::HashSet<i64> = groups
        .iter()
        .flat_map(|g| g.files.iter().map(|f| f.id))
        .collect();
    for id in requested.iter().filter(|id| !known.contains(id)) {
        out.failed.push(format!(
            "model {id}: not a known duplicate (refresh the page)"
        ));
    }
    if out.moved > 0 {
        tracing::info!(
            "moved {} duplicate copies to {TRASH_DIR}, {} bytes",
            out.moved,
            out.freed_bytes
        );
    }
    Ok(out)
}

fn trash_one(
    conn: &Connection,
    root: &Path,
    thumb_dir: &Path,
    g: &DupeGroup,
    f: &DupeFile,
    keeper: &DupeFile,
) -> Result<()> {
    let src = Path::new(&f.path);
    let relative = src
        .strip_prefix(root)
        .context("file is outside the library folder")?;
    if !matches_disk(&f.path, g.file_size, f.mtime) {
        bail!("missing or changed on disk since it was compared; rescan first");
    }
    let dest = unique_dest(&root.join(TRASH_DIR).join(relative));
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(src, &dest)?;

    // Carry curation over to the copy we keep.
    let tx = conn.unchecked_transaction()?;
    if keeper.project_id.is_none() && f.project_id.is_some() {
        tx.execute(
            "UPDATE models SET project_id = ?1, project_name = ?2
             WHERE id = ?3 AND project_id IS NULL",
            params![f.project_id, f.project_name, keeper.id],
        )?;
    }
    let thumb_moved = keeper.thumbnail_path.is_none()
        && f.thumbnail_path.is_some()
        && tx.execute(
            "UPDATE models SET thumbnail_path = ?1 WHERE id = ?2 AND thumbnail_path IS NULL",
            params![f.thumbnail_path, keeper.id],
        )? > 0;
    tx.execute("DELETE FROM models WHERE id = ?1", [f.id])?;
    tx.commit()?;
    if !thumb_moved {
        crate::scanner::remove_thumbnail(thumb_dir, f.thumbnail_path.as_deref());
    }
    tracing::info!("trashed duplicate {} -> {}", f.path, dest.display());
    Ok(())
}

fn matches_disk(path: &str, size: i64, mtime: Option<i64>) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|md| md.is_file() && md.len() as i64 == size && file_mtime(&md) == mtime)
}

/// `dest`, or `name (2).ext`, `name (3).ext`… if something is already there.
fn unique_dest(dest: &Path) -> PathBuf {
    if !dest.exists() {
        return dest.to_path_buf();
    }
    let stem = dest
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = dest
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (2..)
        .map(|n| dest.with_file_name(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .expect("unbounded range")
}

fn rel(root: &Path, p: &str) -> String {
    Path::new(p)
        .strip_prefix(root)
        .unwrap_or(Path::new(p))
        .display()
        .to_string()
}

fn friendly(e: &anyhow::Error) -> String {
    let io = e.downcast_ref::<std::io::Error>();
    match io.map(|e| e.raw_os_error()) {
        Some(Some(30)) => "the library is mounted read-only; change the Library path to rw in the container template".into(),
        Some(Some(13)) => "permission denied; the library may be read-only or owned by another user".into(),
        Some(Some(18)) => "the trash folder is on a different filesystem from this file".into(),
        _ => e.root_cause().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_dirs() {
        assert!(is_internal_dir(".org3d-trash".as_ref()));
        assert!(is_internal_dir(".pack.org3d-partial".as_ref()));
        assert!(!is_internal_dir("org3d-trash".as_ref()));
        assert!(!is_internal_dir("prints".as_ref()));
    }

    #[test]
    fn unique_dest_adds_suffix() {
        let dir = std::env::temp_dir().join(format!("org3d-ud-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.stl");
        assert_eq!(unique_dest(&p), p);
        std::fs::write(&p, b"x").unwrap();
        assert_eq!(unique_dest(&p), dir.join("a (2).stl"));
        std::fs::write(dir.join("a (2).stl"), b"x").unwrap();
        assert_eq!(unique_dest(&p), dir.join("a (3).stl"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
