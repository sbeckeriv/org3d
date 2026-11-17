//! Library scanner.
//!
//! Designed for libraries on spinning disks that sleep (e.g. an Unraid array):
//!
//! * **Incremental.** The walk only reads directory metadata (size + mtime).
//!   A file is opened and parsed only if it is new or its size/mtime changed,
//!   so a rescan of an unchanged library never reads file contents. With the
//!   Unraid "Cache Dirs" plugin keeping dentries/inodes in RAM, an unchanged
//!   rescan doesn't need to wake any array disk at all.
//! * **Move-aware.** A file that appears at a new path with the same size and
//!   mtime as one that vanished is treated as a move/rename: the existing row
//!   (id, project, thumbnail) is re-pointed instead of re-created.
//! * **Soft delete.** Vanished files are only *marked missing* (hidden from
//!   the gallery, project kept) and restored if they reappear. They are purged
//!   after a grace period. Marking is skipped entirely if the walk had errors,
//!   found nothing, or would hide a large share of the library at once — the
//!   signature of an Unraid disk dropping out of `/mnt/user`.
//! * **Batched commits.** Writes are committed every [`BATCH`] files so the
//!   web UI (on its own connection) is never blocked for the whole scan.

use anyhow::{Context, Result, bail};
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

use crate::db::{self, ModelRow};
use crate::formats::{stl, threemf};

const BATCH: usize = 200;
const LOG_EVERY: usize = 500;
/// Refuse to mark more than this fraction of the library missing in one scan…
const MAX_MISSING_FRACTION: f64 = 0.25;
/// …unless the absolute number is this small.
const MISSING_GUARD_MIN: usize = 25;

/// Live counters a UI can poll while a scan runs. `total` stays 0 while the
/// folder walk is still in progress.
#[derive(Debug, Default)]
pub struct ScanProgress {
    pub total: AtomicUsize,
    pub done: AtomicUsize,
}

impl ScanProgress {
    pub fn reset(&self) {
        self.total.store(0, Ordering::Relaxed);
        self.done.store(0, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Purge rows that have been missing for this many days. 0 = never purge.
    pub purge_missing_after_days: u32,
    /// Skip the "too many files vanished at once" guard. For when you really
    /// did delete a large part of the library.
    pub allow_mass_missing: bool,
    pub progress: Option<Arc<ScanProgress>>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            purge_missing_after_days: 30,
            allow_mass_missing: false,
            progress: None,
        }
    }
}

/// Keep at most this many failed paths in the report (all are logged).
const MAX_FAILED_LISTED: usize = 50;

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct ScanStats {
    /// 3MF/STL files found on disk.
    pub scanned: usize,
    /// Files parsed and written (new or changed).
    pub inserted: usize,
    /// Files skipped because size and mtime were unchanged.
    pub unchanged: usize,
    /// Files matched to a vanished row by size + mtime and re-pointed.
    pub moved: usize,
    /// Previously-missing files that are back.
    pub restored: usize,
    /// Rows newly marked missing.
    pub missing: usize,
    /// Rows deleted after being missing past the grace period.
    pub purged: usize,
    /// Files that couldn't be parsed/indexed.
    pub errors: usize,
    /// Directory entries the walk couldn't read (permissions, I/O errors).
    pub walk_errors: usize,
    /// First few files that failed, with the reason.
    pub failed: Vec<String>,
    /// Things the user should know about, e.g. a refused mass-missing.
    pub warnings: Vec<String>,
}

impl ScanStats {
    pub fn has_problems(&self) -> bool {
        self.errors > 0 || self.walk_errors > 0 || !self.warnings.is_empty()
    }
}

struct Found {
    path: PathBuf,
    size: i64,
    mtime: Option<i64>,
    is_3mf: bool,
}

pub fn scan(conn: &Connection, root: &Path, thumb_dir: &Path) -> Result<ScanStats> {
    scan_with(conn, root, thumb_dir, &ScanOptions::default())
}

pub fn scan_with(
    conn: &Connection,
    root: &Path,
    thumb_dir: &Path,
    opts: &ScanOptions,
) -> Result<ScanStats> {
    let meta = std::fs::metadata(root)
        .with_context(|| format!("library folder {} is not accessible", root.display()))?;
    if !meta.is_dir() {
        bail!("library folder {} is not a directory", root.display());
    }
    std::fs::create_dir_all(thumb_dir)?;

    // ── 1. Walk: metadata only, never opens files ────────────────────────────
    let mut walk_errors = 0usize;
    let mut found = Vec::new();
    for entry in WalkDir::new(root) {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                walk_errors += 1;
                tracing::warn!("walk error: {e}");
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let ext = entry
            .path()
            .extension()
            .and_then(|x| x.to_str())
            .map(str::to_ascii_lowercase);
        let is_3mf = match ext.as_deref() {
            Some("3mf") => true,
            Some("stl") => false,
            _ => continue,
        };
        let md = match entry.metadata() {
            Ok(m) => m,
            Err(e) => {
                walk_errors += 1;
                tracing::warn!("stat error {}: {e}", entry.path().display());
                continue;
            }
        };
        found.push(Found {
            path: entry.into_path(),
            size: md.len() as i64,
            mtime: md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64),
            is_3mf,
        });
    }

    let known = db::known_files(conn)?;
    let mut stats = ScanStats {
        scanned: found.len(),
        walk_errors,
        ..Default::default()
    };
    if let Some(p) = &opts.progress {
        p.total.store(found.len(), Ordering::Relaxed);
    }

    // Rows under this root whose path wasn't seen: either moved, or gone.
    let found_paths: HashSet<String> = found
        .iter()
        .map(|f| f.path.to_string_lossy().into_owned())
        .collect();
    let mut vanished: HashMap<&str, &db::KnownFile> = known
        .iter()
        .filter(|(p, _)| Path::new(p.as_str()).starts_with(root) && !found_paths.contains(*p))
        .map(|(p, k)| (p.as_str(), k))
        .collect();
    // (size, mtime) → vanished paths, for move detection.
    let mut by_identity: HashMap<(i64, i64), Vec<&str>> = HashMap::new();
    for (p, k) in &vanished {
        if let Some(m) = k.mtime {
            by_identity.entry((k.file_size, m)).or_default().push(p);
        }
    }

    let pb = progress_bar(found.len() as u64);

    // ── 2. Index new/changed files, detect moves ─────────────────────────────
    let mut in_batch = 0usize;
    conn.execute_batch("BEGIN")?;
    for (i, f) in found.iter().enumerate() {
        let key = f.path.to_string_lossy().into_owned();
        pb.inc(1);
        if let Some(p) = &opts.progress {
            p.done.store(i + 1, Ordering::Relaxed);
        }
        if (i + 1) % LOG_EVERY == 0 && pb.is_hidden() {
            tracing::info!("scan progress: {}/{}", i + 1, found.len());
        }

        if let Some(k) = known.get(&key) {
            if k.file_size == f.size && k.mtime.is_some() && k.mtime == f.mtime {
                if k.missing {
                    db::mark_present(conn, k.id)?;
                    stats.restored += 1;
                } else {
                    stats.unchanged += 1;
                }
                continue;
            }
        } else if let Some(old) = take_move_source(&mut by_identity, f) {
            let k = vanished.remove(old).expect("indexed vanished path");
            db::relocate(
                conn,
                k.id,
                &key,
                &file_name(&f.path),
                &relative_folder(&f.path, root),
            )?;
            tracing::info!("moved: {old} → {key}");
            stats.moved += 1;
            continue;
        }

        pb.set_message(file_name(&f.path));
        let res = if f.is_3mf {
            index_threemf(conn, f, root, thumb_dir)
        } else {
            index_stl(conn, f, root)
        };
        match res {
            Ok(()) => stats.inserted += 1,
            Err(e) => {
                pb.suspend(|| tracing::warn!("index error {}: {e:#}", f.path.display()));
                stats.errors += 1;
                if stats.failed.len() < MAX_FAILED_LISTED {
                    let shown = f.path.strip_prefix(root).unwrap_or(&f.path);
                    stats
                        .failed
                        .push(format!("{}: {}", shown.display(), e.root_cause()));
                }
            }
        }

        in_batch += 1;
        if in_batch >= BATCH {
            conn.execute_batch("COMMIT; BEGIN")?;
            in_batch = 0;
        }
    }
    conn.execute_batch("COMMIT")?;

    // ── 3. Mark vanished files missing (soft delete), with guards ────────────
    let newly_missing: Vec<i64> = vanished
        .values()
        .filter(|k| !k.missing)
        .map(|k| k.id)
        .collect();
    let indexed_here = known
        .keys()
        .filter(|p| Path::new(p.as_str()).starts_with(root))
        .count();
    if !newly_missing.is_empty() {
        let n = newly_missing.len();
        let refused = if walk_errors > 0 {
            Some(format!(
                "{n} indexed files were not found, but {walk_errors} folders/files couldn't be \
                 read, so nothing was hidden. A disk may be offline or permissions may be wrong."
            ))
        } else if found.is_empty() {
            Some(format!(
                "The library folder is empty but {n} files are indexed, so nothing was hidden. \
                 Is the share mounted into the container?"
            ))
        } else if !opts.allow_mass_missing
            && n > MISSING_GUARD_MIN
            && (n as f64) > indexed_here as f64 * MAX_MISSING_FRACTION
        {
            Some(format!(
                "{n} of {indexed_here} indexed files vanished at once, so nothing was hidden. \
                 If an array disk is missing, bring it back and rescan. If you really deleted \
                 them, run `org3d scan <path> --allow-mass-missing`."
            ))
        } else {
            None
        };
        match refused {
            Some(msg) => {
                tracing::warn!("{msg}");
                stats.warnings.push(msg);
            }
            None => {
                conn.execute_batch("BEGIN")?;
                for id in &newly_missing {
                    db::mark_missing(conn, *id)?;
                }
                conn.execute_batch("COMMIT")?;
                stats.missing = n;
            }
        }
    }
    if walk_errors > 0 && stats.warnings.is_empty() {
        stats.warnings.push(format!(
            "{walk_errors} folders/files couldn't be read during the walk (see the log)."
        ));
    }

    // ── 4. Purge rows missing past the grace period ──────────────────────────
    if opts.purge_missing_after_days > 0 && walk_errors == 0 && !found.is_empty() {
        for thumb in db::purge_missing(conn, opts.purge_missing_after_days)? {
            remove_thumbnail(thumb_dir, thumb.as_deref());
            stats.purged += 1;
        }
    }

    pb.finish_and_clear();
    tracing::info!(
        "scan done: {} found, {} indexed, {} unchanged, {} moved, {} restored, \
         {} marked missing, {} purged, {} errors",
        stats.scanned,
        stats.inserted,
        stats.unchanged,
        stats.moved,
        stats.restored,
        stats.missing,
        stats.purged,
        stats.errors
    );
    Ok(stats)
}

/// Pick the vanished row this new file most likely came from: same size and
/// mtime, preferring one with the same filename (a folder move) when several
/// identical files vanished. Ambiguous matches are treated as new files.
fn take_move_source<'a>(
    by_identity: &mut HashMap<(i64, i64), Vec<&'a str>>,
    f: &Found,
) -> Option<&'a str> {
    let cands = by_identity.get_mut(&(f.size, f.mtime?))?;
    let name = f.path.file_name()?;
    let idx = if cands.len() == 1 {
        0
    } else {
        let same: Vec<usize> = (0..cands.len())
            .filter(|&i| Path::new(cands[i]).file_name() == Some(name))
            .collect();
        if same.len() != 1 {
            return None;
        }
        same[0]
    };
    Some(cands.swap_remove(idx))
}

fn progress_bar(len: u64) -> ProgressBar {
    // In Docker/systemd stderr is not a TTY; fall back to periodic log lines.
    if !std::io::stderr().is_terminal() {
        return ProgressBar::with_draw_target(Some(len), ProgressDrawTarget::hidden());
    }
    let pb = ProgressBar::new(len);
    pb.set_style(
        ProgressStyle::with_template("{spinner:.cyan} [{bar:40.cyan/blue}] {pos}/{len} {wide_msg}")
            .expect("valid template")
            .progress_chars("█▉▊▋▌▍▎▏  "),
    );
    pb.enable_steady_tick(std::time::Duration::from_millis(80));
    pb
}

/// Delete a thumbnail we generated. Only touches files inside `thumb_dir`.
fn remove_thumbnail(thumb_dir: &Path, url: Option<&str>) {
    let Some(name) = url.and_then(|u| u.strip_prefix("/thumbs/")) else {
        return;
    };
    if name.contains('/') || name.contains("..") {
        return;
    }
    let _ = std::fs::remove_file(thumb_dir.join(name));
}

fn index_threemf(conn: &Connection, f: &Found, root: &Path, thumb_dir: &Path) -> Result<()> {
    let meta = threemf::extract(&f.path)?;

    let thumbnail_path = if let Some(png) = meta.thumbnail {
        let thumb_name = format!("{:x}.png", fnv1a(f.path.to_string_lossy().as_bytes()));
        std::fs::write(thumb_dir.join(&thumb_name), &png)?;
        Some(format!("/thumbs/{thumb_name}"))
    } else {
        None
    };

    db::upsert(
        conn,
        &ModelRow {
            id: 0,
            path: f.path.to_string_lossy().to_string(),
            filename: file_name(&f.path),
            folder: relative_folder(&f.path, root),
            format: "3MF".to_string(),
            file_size: f.size,
            title: meta.title,
            designer: meta.designer,
            description: meta.description,
            application: meta.application,
            license: meta.license,
            created_at: meta.created_at,
            object_count: meta.object_count.map(|n| n as i64),
            triangle_count: meta.triangle_count.map(|n| n as i64),
            dim_x: meta.dim_x,
            dim_y: meta.dim_y,
            dim_z: meta.dim_z,
            thumbnail_path,
            project_id: None,
            mtime: f.mtime,
        },
    )?;
    Ok(())
}

fn index_stl(conn: &Connection, f: &Found, root: &Path) -> Result<()> {
    let meta = stl::extract(&f.path)?;
    db::upsert(
        conn,
        &ModelRow {
            id: 0,
            path: f.path.to_string_lossy().to_string(),
            filename: file_name(&f.path),
            folder: relative_folder(&f.path, root),
            format: "STL".to_string(),
            file_size: f.size,
            title: meta.name,
            designer: None,
            description: None,
            application: meta.header,
            license: None,
            created_at: None,
            object_count: Some(1),
            triangle_count: Some(meta.triangle_count as i64),
            dim_x: meta.dim_x,
            dim_y: meta.dim_y,
            dim_z: meta.dim_z,
            thumbnail_path: None,
            project_id: None,
            mtime: f.mtime,
        },
    )?;
    Ok(())
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

fn relative_folder(path: &Path, root: &Path) -> String {
    path.parent()
        .and_then(|p| p.strip_prefix(root).ok())
        .map(|p| {
            let s = p.to_string_lossy();
            if s.is_empty() {
                "/".to_string()
            } else {
                format!("/{s}")
            }
        })
        .unwrap_or_else(|| "/".to_string())
}

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 14695981039346656037;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    h
}
