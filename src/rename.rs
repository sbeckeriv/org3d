use anyhow::Result;
use rusqlite::Connection;
use std::path::{Path, PathBuf};

use crate::db::{self, SearchParams};

pub struct RenameCandidate {
    pub id: i64,
    pub old_path: PathBuf,
    pub new_path: PathBuf,
    pub old_name: String,
    pub new_name: String,
}

/// Compute renames for all 3MF files under `root` that have a Title in the DB.
/// Files where the computed name matches the current name are excluded.
/// Files where the destination already exists are excluded (no overwrite).
pub fn plan(conn: &Connection, root: &Path) -> Result<Vec<RenameCandidate>> {
    let models = db::search(
        conn,
        &SearchParams {
            query: None,
            designer: None,
            folder: None,
            format: Some("3MF"),
            project: None,
            limit: 100_000,
            offset: 0,
        },
    )?;

    let mut candidates = Vec::new();

    for m in models {
        let Some(title) = &m.title else { continue };

        let old_path = PathBuf::from(&m.path);
        if !old_path.starts_with(root) {
            continue;
        }

        let new_name = make_filename(title, m.designer.as_deref());
        if new_name == m.filename {
            continue;
        }

        let new_path = old_path.parent().unwrap_or(root).join(&new_name);

        if new_path.exists() {
            tracing::debug!("skipping {}: destination already exists", m.filename);
            continue;
        }

        candidates.push(RenameCandidate {
            id: m.id,
            old_name: m.filename.clone(),
            new_name,
            old_path,
            new_path,
        });
    }

    Ok(candidates)
}

/// Apply renames to disk and update the database.
pub fn apply(conn: &Connection, candidates: &[RenameCandidate]) -> Result<usize> {
    let mut count = 0;
    for c in candidates {
        if !c.old_path.exists() {
            tracing::warn!("skipping rename: source missing {}", c.old_path.display());
            continue;
        }
        std::fs::rename(&c.old_path, &c.new_path)?;
        db::rename_model(
            conn,
            c.old_path.to_str().unwrap_or_default(),
            c.new_path.to_str().unwrap_or_default(),
            &c.new_name,
        )?;
        count += 1;
    }
    Ok(count)
}

fn make_filename(title: &str, designer: Option<&str>) -> String {
    let mut stem = sanitize(title);
    if let Some(d) = designer {
        let ds = sanitize(d);
        if !ds.is_empty() {
            stem.push_str("__");
            stem.push_str(&ds);
        }
    }
    // Truncate stem so full filename stays under ~200 chars
    let stem: String = stem.chars().take(180).collect();
    format!("{stem}.3mf")
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect::<String>()
        .trim_matches(|c: char| c == '.' || c == ' ' || c == '_')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_strips_illegal_chars() {
        assert_eq!(sanitize("hello/world"), "hello_world");
        assert_eq!(sanitize("foo:bar*baz"), "foo_bar_baz");
        assert_eq!(sanitize(r#"a"b<c>d"#), "a_b_c_d");
    }

    #[test]
    fn test_sanitize_trims_dots_and_spaces() {
        assert_eq!(sanitize("  leading space"), "leading space");
        assert_eq!(sanitize("trailing dot."), "trailing dot");
        assert_eq!(sanitize("...dots..."), "dots");
    }

    #[test]
    fn test_make_filename_with_designer() {
        let name = make_filename("Smokey the Dragon", Some("Eon3D"));
        assert_eq!(name, "Smokey the Dragon__Eon3D.3mf");
    }

    #[test]
    fn test_make_filename_without_designer() {
        let name = make_filename("My Model", None);
        assert_eq!(name, "My Model.3mf");
    }

    #[test]
    fn test_make_filename_truncates_long_title() {
        let long = "a".repeat(300);
        let name = make_filename(&long, None);
        assert!(name.len() <= 185, "filename should be truncated");
        assert!(name.ends_with(".3mf"));
    }
}
