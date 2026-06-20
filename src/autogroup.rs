use anyhow::Result;
use rusqlite::Connection;

pub struct AutogroupStats {
    pub projects_created: usize,
    pub models_assigned: usize,
}

/// Auto-assign models to projects based on shared folder.
///
/// Rules:
/// - Only operates on models with no existing project (preserves manual assignments).
/// - Only creates projects for folders ≥2 segments deep (e.g. `/fallout/liberty-prime_files`),
///   not top-level category folders (e.g. `/fallout`).
/// - Only groups folders that contain ≥2 unassigned models.
/// - Project name is derived from the deepest path segment.
pub fn run(conn: &Connection) -> Result<AutogroupStats> {
    let mut stats = AutogroupStats {
        projects_created: 0,
        models_assigned: 0,
    };

    // Collect unassigned models grouped by folder
    let mut stmt =
        conn.prepare("SELECT id, folder FROM models WHERE project_id IS NULL ORDER BY folder")?;
    let pairs: Vec<(i64, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;

    // Group by folder
    use std::collections::HashMap;
    let mut by_folder: HashMap<String, Vec<i64>> = HashMap::new();
    for (id, folder) in pairs {
        by_folder.entry(folder).or_default().push(id);
    }

    for (folder, ids) in &by_folder {
        // Skip folders that are only 1 segment deep (top-level categories)
        let depth = folder.matches('/').count();
        if depth < 2 || ids.len() < 2 {
            continue;
        }

        let project_name = folder_to_project_name(folder);
        if project_name.is_empty() {
            continue;
        }

        let pid = crate::db::find_or_create_project(conn, &project_name)?;
        stats.projects_created += 1;

        for &id in ids {
            crate::db::assign_project(conn, id, pid)?;
            stats.models_assigned += 1;
        }
    }

    Ok(stats)
}

/// Turn a relative folder path into a readable project name.
///
/// `/fallout/liberty-prime-from-fallout-4-action-figure-model_files` →
/// `Liberty Prime From Fallout 4 Action Figure Model`
pub fn folder_to_project_name(folder: &str) -> String {
    // Take the deepest path segment
    let segment = folder.trim_matches('/').split('/').last().unwrap_or(folder);

    // Strip Thingiverse-style numeric suffixes: " - 4766824" or "_4766824"
    let segment = strip_id_suffix(segment);

    // Strip common file-set suffixes
    let segment = segment
        .trim_end_matches("_model_files")
        .trim_end_matches("_stl_files")
        .trim_end_matches("_files")
        .trim_end_matches("_stls")
        .trim_end_matches("_3d");

    // Replace separators with spaces
    let with_spaces: String = segment
        .chars()
        .map(|c| if c == '-' || c == '_' { ' ' } else { c })
        .collect();

    // Collapse runs of spaces and title-case
    let words: Vec<String> = with_spaces.split_whitespace().map(title_word).collect();

    words.join(" ")
}

/// Strip trailing Thingiverse/Printables numeric IDs.
/// Handles: " - 4766824", "_4766824", "- 4766824"
fn strip_id_suffix(s: &str) -> &str {
    // Try " - NNNNN" at the end
    let s = if let Some(pos) = s.rfind(" - ") {
        let tail = &s[pos + 3..];
        if tail.chars().all(|c| c.is_ascii_digit()) {
            &s[..pos]
        } else {
            s
        }
    } else {
        s
    };
    // Try trailing "_NNNNN" where NNNNN is 4-8 digits
    if let Some(pos) = s.rfind('_') {
        let tail = &s[pos + 1..];
        if tail.len() >= 4 && tail.len() <= 8 && tail.chars().all(|c| c.is_ascii_digit()) {
            return &s[..pos];
        }
    }
    s
}

/// Title-case a single word, preserving all-caps acronyms.
fn title_word(w: &str) -> String {
    if w.is_empty() {
        return w.to_string();
    }
    // Keep short all-caps words (like "X1C", "AMS") as-is
    if w.len() <= 4
        && w.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    {
        return w.to_string();
    }
    let mut chars = w.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_folder_to_project_name_basic() {
        assert_eq!(
            folder_to_project_name(
                "/fallout/liberty-prime-from-fallout-4-action-figure-model_files"
            ),
            "Liberty Prime From Fallout 4 Action Figure Model"
        );
    }

    #[test]
    fn test_strips_thingiverse_id() {
        assert_eq!(
            folder_to_project_name(
                "/gaslands/Mad Max Highway 9 Main Force Patrol Sign - Gaslands _ Darkfuture - 4766824"
            ),
            "Mad Max Highway 9 Main Force Patrol Sign Gaslands Darkfuture"
        );
    }

    #[test]
    fn test_strips_numeric_underscore_id() {
        assert_eq!(
            folder_to_project_name("/sci-fi/Lego Usagi Yojimbo head - 4676019"),
            "Lego Usagi Yojimbo Head"
        );
    }

    #[test]
    fn test_strips_files_suffix() {
        assert_eq!(
            folder_to_project_name("/to print/fallout-vault-31-diorama-model_files"),
            "Fallout Vault 31 Diorama Model"
        );
    }

    #[test]
    fn test_category_only_folder_not_grouped() {
        // depth 1 — should be skipped by run() logic, not tested here
        // but name generation should still work
        let name = folder_to_project_name("/fallout");
        assert_eq!(name, "Fallout");
    }

    #[test]
    fn test_auto_group_assigns_multi_file_subfolders() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
        crate::db::migrate(&conn).unwrap();

        let base = crate::db::ModelRow {
            id: 0,
            path: String::new(),
            filename: String::new(),
            folder: String::new(),
            format: "STL".to_string(),
            file_size: 100,
            title: None,
            designer: None,
            description: None,
            application: None,
            license: None,
            created_at: None,
            object_count: None,
            triangle_count: None,
            dim_x: None,
            dim_y: None,
            dim_z: None,
            thumbnail_path: None,
            project_id: None,
        };

        // Three files in a deep project folder
        for (path, folder) in [
            ("/tmp/p1.stl", "/fallout/liberty-prime_files"),
            ("/tmp/p2.stl", "/fallout/liberty-prime_files"),
            ("/tmp/p3.stl", "/fallout/liberty-prime_files"),
        ] {
            crate::db::upsert(
                &conn,
                &crate::db::ModelRow {
                    path: path.into(),
                    filename: path.into(),
                    folder: folder.into(),
                    ..base.clone()
                },
            )
            .unwrap();
        }
        // One file in a shallow category folder — should NOT be grouped
        crate::db::upsert(
            &conn,
            &crate::db::ModelRow {
                path: "/tmp/solo.stl".into(),
                filename: "solo.stl".into(),
                folder: "/fallout".into(),
                ..base.clone()
            },
        )
        .unwrap();

        let stats = run(&conn).unwrap();

        assert_eq!(
            stats.projects_created, 1,
            "should create exactly one project"
        );
        assert_eq!(stats.models_assigned, 3, "should assign three models");

        // Solo file should remain unassigned
        let solo = crate::db::get_by_id(&conn, 4).unwrap().unwrap();
        assert!(
            solo.project_id.is_none(),
            "shallow folder file should stay unassigned"
        );
    }

    #[test]
    fn test_auto_group_does_not_override_existing() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
        crate::db::migrate(&conn).unwrap();

        let base = crate::db::ModelRow {
            id: 0,
            path: String::new(),
            filename: String::new(),
            folder: "/toys/dragon_set".into(),
            format: "STL".to_string(),
            file_size: 100,
            title: None,
            designer: None,
            description: None,
            application: None,
            license: None,
            created_at: None,
            object_count: None,
            triangle_count: None,
            dim_x: None,
            dim_y: None,
            dim_z: None,
            thumbnail_path: None,
            project_id: None,
        };

        crate::db::upsert(
            &conn,
            &crate::db::ModelRow {
                path: "/tmp/a.stl".into(),
                filename: "a.stl".into(),
                ..base.clone()
            },
        )
        .unwrap();
        crate::db::upsert(
            &conn,
            &crate::db::ModelRow {
                path: "/tmp/b.stl".into(),
                filename: "b.stl".into(),
                ..base.clone()
            },
        )
        .unwrap();

        // Manually assign model 1 to a different project
        let manual_pid = crate::db::find_or_create_project(&conn, "My Custom Project").unwrap();
        crate::db::assign_project(&conn, 1, manual_pid).unwrap();

        let stats = run(&conn).unwrap();

        // Only model 2 should be auto-grouped (model 1 is already assigned)
        // But since only 1 unassigned model remains in the folder, no auto-group
        assert_eq!(
            stats.models_assigned, 0,
            "should not auto-group with only 1 unassigned file"
        );

        // Model 1 should keep its manual project
        let m1 = crate::db::get_by_id(&conn, 1).unwrap().unwrap();
        assert_eq!(m1.project_id, Some(manual_pid));
    }
}
