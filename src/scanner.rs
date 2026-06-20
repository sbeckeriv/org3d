use anyhow::Result;
use indicatif::{ProgressBar, ProgressStyle};
use rusqlite::Connection;
use std::path::Path;
use walkdir::WalkDir;

use crate::db::{self, ModelRow};
use crate::formats::{stl, threemf};

pub struct ScanStats {
    pub scanned: usize,
    pub inserted: usize,
    pub errors: usize,
}

pub fn scan(conn: &Connection, root: &Path, thumb_dir: &Path) -> Result<ScanStats> {
    std::fs::create_dir_all(thumb_dir)?;

    // Collect matching files first so we know the total for the progress bar
    let files: Vec<_> = WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|x| x.to_str())
                .map(|x| matches!(x.to_lowercase().as_str(), "3mf" | "stl"))
                .unwrap_or(false)
        })
        .map(|e| e.path().to_path_buf())
        .collect();

    let pb = ProgressBar::new(files.len() as u64);
    pb.set_style(
        ProgressStyle::with_template("{spinner:.cyan} [{bar:40.cyan/blue}] {pos}/{len} {wide_msg}")
            .unwrap()
            .progress_chars("█▉▊▋▌▍▎▏  "),
    );
    pb.enable_steady_tick(std::time::Duration::from_millis(80));

    let mut stats = ScanStats {
        scanned: 0,
        inserted: 0,
        errors: 0,
    };

    // Single transaction: FTS triggers fire at commit, search stays consistent
    // and scan is ~10x faster than autocommit.
    conn.execute_batch("BEGIN")?;

    for path in &files {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        pb.set_message(format!("{name}"));

        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase());

        match ext.as_deref() {
            Some("3mf") => {
                stats.scanned += 1;
                match index_threemf(conn, path, root, thumb_dir) {
                    Ok(_) => stats.inserted += 1,
                    Err(e) => {
                        pb.suspend(|| tracing::warn!("3mf error {}: {e}", path.display()));
                        stats.errors += 1;
                    }
                }
            }
            Some("stl") => {
                stats.scanned += 1;
                match index_stl(conn, path, root) {
                    Ok(_) => stats.inserted += 1,
                    Err(e) => {
                        pb.suspend(|| tracing::warn!("stl error {}: {e}", path.display()));
                        stats.errors += 1;
                    }
                }
            }
            _ => {}
        }

        pb.inc(1);
    }

    conn.execute_batch("COMMIT")?;

    pb.finish_with_message(format!(
        "done — {} indexed, {} errors",
        stats.inserted, stats.errors
    ));

    Ok(stats)
}

fn index_threemf(conn: &Connection, path: &Path, root: &Path, thumb_dir: &Path) -> Result<()> {
    let meta = threemf::extract(path)?;
    let file_size = std::fs::metadata(path)?.len() as i64;
    let filename = path.file_name().unwrap().to_string_lossy().to_string();
    let folder = relative_folder(path, root);

    let thumbnail_path = if let Some(png) = meta.thumbnail {
        let thumb_name = format!("{:x}.png", fnv1a(path.to_string_lossy().as_bytes()));
        let thumb_path = thumb_dir.join(&thumb_name);
        std::fs::write(&thumb_path, &png)?;
        Some(format!("/thumbs/{thumb_name}"))
    } else {
        None
    };

    db::upsert(
        conn,
        &ModelRow {
            id: 0,
            path: path.to_string_lossy().to_string(),
            filename,
            folder,
            format: "3MF".to_string(),
            file_size,
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
        },
    )?;

    Ok(())
}

fn index_stl(conn: &Connection, path: &Path, root: &Path) -> Result<()> {
    let meta = stl::extract(path)?;
    let file_size = std::fs::metadata(path)?.len() as i64;
    let filename = path.file_name().unwrap().to_string_lossy().to_string();
    let folder = relative_folder(path, root);

    db::upsert(
        conn,
        &ModelRow {
            id: 0,
            path: path.to_string_lossy().to_string(),
            filename,
            folder,
            format: "STL".to_string(),
            file_size,
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
        },
    )?;

    Ok(())
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
