use anyhow::Result;
use rusqlite::{Connection, params};

pub fn open(path: &str) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
    migrate(&conn)?;
    ensure_fts_sync(&conn)?;
    Ok(conn)
}

/// Rebuild the FTS index if it appears out of sync with the models table.
/// This is fast (<10ms for thousands of rows) and handles DBs created before
/// triggers were in place.
fn ensure_fts_sync(conn: &Connection) -> Result<()> {
    let model_count: i64 = conn.query_row("SELECT COUNT(*) FROM models", [], |r| r.get(0))?;
    let fts_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM models_fts", [], |r| r.get(0))
        .unwrap_or(0);
    if model_count > 0 && fts_count == 0 {
        tracing::info!("FTS index empty, rebuilding from {} models", model_count);
        conn.execute_batch("INSERT INTO models_fts(models_fts) VALUES('rebuild')")?;
    }
    Ok(())
}

pub(crate) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch("
        CREATE TABLE IF NOT EXISTS projects (
            id         INTEGER PRIMARY KEY,
            name       TEXT NOT NULL UNIQUE,
            notes      TEXT,
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS models (
            id          INTEGER PRIMARY KEY,
            path        TEXT NOT NULL UNIQUE,
            filename    TEXT NOT NULL,
            folder      TEXT NOT NULL,
            format      TEXT NOT NULL,
            file_size   INTEGER NOT NULL,

            title       TEXT,
            designer    TEXT,
            description TEXT,
            application TEXT,
            license     TEXT,
            created_at  TEXT,

            object_count  INTEGER,
            triangle_count INTEGER,
            dim_x         REAL,
            dim_y         REAL,
            dim_z         REAL,

            thumbnail_path TEXT,
            project_id     INTEGER REFERENCES projects(id),
            project_name   TEXT,

            indexed_at  TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE VIRTUAL TABLE IF NOT EXISTS models_fts USING fts5(
            title, designer, description, filename, folder, project_name,
            content='models', content_rowid='id'
        );

        CREATE TRIGGER IF NOT EXISTS models_ai AFTER INSERT ON models BEGIN
            INSERT INTO models_fts(rowid, title, designer, description, filename, folder, project_name)
            VALUES (new.id, new.title, new.designer, new.description, new.filename, new.folder, new.project_name);
        END;

        CREATE TRIGGER IF NOT EXISTS models_ad AFTER DELETE ON models BEGIN
            INSERT INTO models_fts(models_fts, rowid, title, designer, description, filename, folder, project_name)
            VALUES ('delete', old.id, old.title, old.designer, old.description, old.filename, old.folder, old.project_name);
        END;

        CREATE TRIGGER IF NOT EXISTS models_au AFTER UPDATE ON models BEGIN
            INSERT INTO models_fts(models_fts, rowid, title, designer, description, filename, folder, project_name)
            VALUES ('delete', old.id, old.title, old.designer, old.description, old.filename, old.folder, old.project_name);
            INSERT INTO models_fts(rowid, title, designer, description, filename, folder, project_name)
            VALUES (new.id, new.title, new.designer, new.description, new.filename, new.folder, new.project_name);
        END;
    ")?;
    // Columns added after initial release — ignore error if already present
    let _ = conn.execute("ALTER TABLE models ADD COLUMN project_id INTEGER", []);
    let _ = conn.execute("ALTER TABLE models ADD COLUMN project_name TEXT", []);

    // If the FTS table predates project_name, drop and rebuild it cleanly.
    let fts_sql: String = conn.query_row(
        "SELECT COALESCE(sql, '') FROM sqlite_master WHERE name = 'models_fts'",
        [], |r| r.get(0),
    ).unwrap_or_default();
    if !fts_sql.contains("project_name") {
        conn.execute_batch("
            DROP TABLE IF EXISTS models_fts;
            DROP TRIGGER IF EXISTS models_ai;
            DROP TRIGGER IF EXISTS models_ad;
            DROP TRIGGER IF EXISTS models_au;
        ")?;
        // Backfill before triggers are recreated so the UPDATE doesn't double-write FTS.
        conn.execute_batch("
            UPDATE models
            SET project_name = (SELECT name FROM projects WHERE id = models.project_id)
            WHERE project_id IS NOT NULL;
        ")?;
        conn.execute_batch("
            CREATE VIRTUAL TABLE models_fts USING fts5(
                title, designer, description, filename, folder, project_name,
                content='models', content_rowid='id'
            );
            CREATE TRIGGER models_ai AFTER INSERT ON models BEGIN
                INSERT INTO models_fts(rowid, title, designer, description, filename, folder, project_name)
                VALUES (new.id, new.title, new.designer, new.description, new.filename, new.folder, new.project_name);
            END;
            CREATE TRIGGER models_ad AFTER DELETE ON models BEGIN
                INSERT INTO models_fts(models_fts, rowid, title, designer, description, filename, folder, project_name)
                VALUES ('delete', old.id, old.title, old.designer, old.description, old.filename, old.folder, old.project_name);
            END;
            CREATE TRIGGER models_au AFTER UPDATE ON models BEGIN
                INSERT INTO models_fts(models_fts, rowid, title, designer, description, filename, folder, project_name)
                VALUES ('delete', old.id, old.title, old.designer, old.description, old.filename, old.folder, old.project_name);
                INSERT INTO models_fts(rowid, title, designer, description, filename, folder, project_name)
                VALUES (new.id, new.title, new.designer, new.description, new.filename, new.folder, new.project_name);
            END;
            INSERT INTO models_fts(models_fts) VALUES('rebuild');
        ")?;
    }

    Ok(())
}

#[derive(Clone)]
pub struct ModelRow {
    pub id: i64,
    pub path: String,
    pub filename: String,
    pub folder: String,
    pub format: String,
    pub file_size: i64,
    pub title: Option<String>,
    pub designer: Option<String>,
    pub description: Option<String>,
    pub application: Option<String>,
    pub license: Option<String>,
    pub created_at: Option<String>,
    pub object_count: Option<i64>,
    pub triangle_count: Option<i64>,
    pub dim_x: Option<f64>,
    pub dim_y: Option<f64>,
    pub dim_z: Option<f64>,
    pub thumbnail_path: Option<String>,
    pub project_id: Option<i64>,
}

#[derive(serde::Serialize)]
pub struct Project {
    pub id: i64,
    pub name: String,
    pub notes: Option<String>,
    pub model_count: i64,
}

impl ModelRow {
    pub fn display_name(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.filename)
    }

    pub fn dims_str(&self) -> Option<String> {
        match (self.dim_x, self.dim_y, self.dim_z) {
            (Some(x), Some(y), Some(z)) => Some(format!("{:.1}×{:.1}×{:.1} mm", x, y, z)),
            _ => None,
        }
    }
}

pub fn upsert(conn: &Connection, m: &ModelRow) -> Result<i64> {
    conn.execute(
        "INSERT INTO models (path, filename, folder, format, file_size, title, designer,
         description, application, license, created_at, object_count, triangle_count,
         dim_x, dim_y, dim_z, thumbnail_path)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)
         ON CONFLICT(path) DO UPDATE SET
           filename=excluded.filename, folder=excluded.folder, file_size=excluded.file_size,
           title=excluded.title, designer=excluded.designer, description=excluded.description,
           application=excluded.application, license=excluded.license,
           created_at=excluded.created_at, object_count=excluded.object_count,
           triangle_count=excluded.triangle_count, dim_x=excluded.dim_x,
           dim_y=excluded.dim_y, dim_z=excluded.dim_z,
           thumbnail_path=COALESCE(excluded.thumbnail_path, thumbnail_path),
           indexed_at=datetime('now')",
        params![
            m.path, m.filename, m.folder, m.format, m.file_size,
            m.title, m.designer, m.description, m.application, m.license, m.created_at,
            m.object_count, m.triangle_count, m.dim_x, m.dim_y, m.dim_z, m.thumbnail_path
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn update_thumbnail(conn: &Connection, id: i64, path: &str) -> Result<()> {
    conn.execute(
        "UPDATE models SET thumbnail_path = ?1 WHERE id = ?2",
        params![path, id],
    )?;
    Ok(())
}

// ── Projects ─────────────────────────────────────────────────────────────────

pub fn list_projects(conn: &Connection) -> Result<Vec<Project>> {
    let mut stmt = conn.prepare(
        "SELECT p.id, p.name, p.notes, COUNT(m.id) as model_count
         FROM projects p
         LEFT JOIN models m ON m.project_id = p.id
         GROUP BY p.id ORDER BY p.name COLLATE NOCASE"
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Project { id: r.get(0)?, name: r.get(1)?, notes: r.get(2)?, model_count: r.get(3)? })
    })?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn find_or_create_project(conn: &Connection, name: &str) -> Result<i64> {
    conn.execute(
        "INSERT INTO projects (name) VALUES (?1) ON CONFLICT(name) DO NOTHING",
        [name],
    )?;
    let id: i64 = conn.query_row(
        "SELECT id FROM projects WHERE name = ?1",
        [name],
        |r| r.get(0),
    )?;
    Ok(id)
}

pub fn assign_project(conn: &Connection, model_id: i64, project_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE models
         SET project_id = ?1,
             project_name = (SELECT name FROM projects WHERE id = ?1)
         WHERE id = ?2",
        params![project_id, model_id],
    )?;
    Ok(())
}

pub fn remove_from_project(conn: &Connection, model_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE models SET project_id = NULL, project_name = NULL WHERE id = ?1",
        [model_id],
    )?;
    Ok(())
}

pub fn get_project_name(conn: &Connection, project_id: i64) -> Result<Option<String>> {
    let mut stmt = conn.prepare("SELECT name FROM projects WHERE id = ?1")?;
    let mut rows = stmt.query_map([project_id], |r| r.get(0))?;
    Ok(rows.next().transpose()?)
}

pub struct SearchParams<'a> {
    pub query: Option<&'a str>,
    pub designer: Option<&'a str>,
    pub folder: Option<&'a str>,
    pub format: Option<&'a str>,
    pub project: Option<i64>,
    pub limit: i64,
    pub offset: i64,
}

pub fn search(conn: &Connection, p: &SearchParams) -> Result<Vec<ModelRow>> {
    let rows = if let Some(q) = p.query.filter(|s| !s.is_empty()) {
        let fts_query = format!("{q}*");
        let sql = "
            SELECT m.id, m.path, m.filename, m.folder, m.format, m.file_size,
                   m.title, m.designer, m.description, m.application, m.license,
                   m.created_at, m.object_count, m.triangle_count,
                   m.dim_x, m.dim_y, m.dim_z, m.thumbnail_path, m.project_id
            FROM models m
            JOIN models_fts f ON f.rowid = m.id
            WHERE models_fts MATCH ?1
              AND (?2 IS NULL OR m.designer = ?2)
              AND (?3 IS NULL OR m.folder = ?3)
              AND (?4 IS NULL OR m.format = ?4)
              AND (?5 IS NULL OR m.project_id = ?5)
            ORDER BY rank
            LIMIT ?6 OFFSET ?7";
        match conn.prepare(sql)?.query_map(
            params![fts_query, p.designer, p.folder, p.format, p.project, p.limit, p.offset],
            row_to_model,
        ) {
            Ok(mapped) => mapped.collect::<rusqlite::Result<Vec<_>>>()?,
            Err(e) => {
                tracing::warn!("FTS search error for {:?}: {e}", q);
                vec![]
            }
        }
    } else {
        let sql = "
            SELECT id, path, filename, folder, format, file_size,
                   title, designer, description, application, license,
                   created_at, object_count, triangle_count,
                   dim_x, dim_y, dim_z, thumbnail_path, project_id
            FROM models
            WHERE (?1 IS NULL OR designer = ?1)
              AND (?2 IS NULL OR folder = ?2)
              AND (?3 IS NULL OR format = ?3)
              AND (?4 IS NULL OR project_id = ?4)
            ORDER BY COALESCE(title, filename) COLLATE NOCASE
            LIMIT ?5 OFFSET ?6";
        conn.prepare(sql)?.query_map(
            params![p.designer, p.folder, p.format, p.project, p.limit, p.offset],
            row_to_model,
        )?.collect::<rusqlite::Result<Vec<_>>>()?
    };
    Ok(rows)
}

pub fn get_by_id(conn: &Connection, id: i64) -> Result<Option<ModelRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, path, filename, folder, format, file_size,
                title, designer, description, application, license,
                created_at, object_count, triangle_count,
                dim_x, dim_y, dim_z, thumbnail_path, project_id
         FROM models WHERE id = ?1"
    )?;
    let mut rows = stmt.query_map([id], row_to_model)?;
    Ok(rows.next().transpose()?)
}

pub fn list_designers(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT designer FROM models WHERE designer IS NOT NULL ORDER BY designer COLLATE NOCASE"
    )?;
    let rows = stmt.query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(rows)
}

pub fn list_folders(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT folder FROM models ORDER BY folder COLLATE NOCASE"
    )?;
    let rows = stmt.query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(rows)
}

pub fn count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM models", [], |r| r.get(0))?)
}

pub fn search_count(conn: &Connection, p: &SearchParams) -> Result<i64> {
    let n = if let Some(q) = p.query.filter(|s| !s.is_empty()) {
        let fts_query = format!("{q}*");
        conn.query_row(
            "SELECT COUNT(*) FROM models m
             JOIN models_fts f ON f.rowid = m.id
             WHERE models_fts MATCH ?1
               AND (?2 IS NULL OR m.designer = ?2)
               AND (?3 IS NULL OR m.folder = ?3)
               AND (?4 IS NULL OR m.format = ?4)
               AND (?5 IS NULL OR m.project_id = ?5)",
            params![fts_query, p.designer, p.folder, p.format, p.project],
            |r| r.get(0),
        )?
    } else {
        conn.query_row(
            "SELECT COUNT(*) FROM models
             WHERE (?1 IS NULL OR designer = ?1)
               AND (?2 IS NULL OR folder = ?2)
               AND (?3 IS NULL OR format = ?3)
               AND (?4 IS NULL OR project_id = ?4)",
            params![p.designer, p.folder, p.format, p.project],
            |r| r.get(0),
        )?
    };
    Ok(n)
}

pub fn rename_model(conn: &Connection, old_path: &str, new_path: &str, new_filename: &str) -> Result<()> {
    conn.execute(
        "UPDATE models SET path = ?1, filename = ?2 WHERE path = ?3",
        params![new_path, new_filename, old_path],
    )?;
    Ok(())
}

fn row_to_model(r: &rusqlite::Row) -> rusqlite::Result<ModelRow> {
    Ok(ModelRow {
        id:             r.get(0)?,
        path:           r.get(1)?,
        filename:       r.get(2)?,
        folder:         r.get(3)?,
        format:         r.get(4)?,
        file_size:      r.get(5)?,
        title:          r.get(6)?,
        designer:       r.get(7)?,
        description:    r.get(8)?,
        application:    r.get(9)?,
        license:        r.get(10)?,
        created_at:     r.get(11)?,
        object_count:   r.get(12)?,
        triangle_count: r.get(13)?,
        dim_x:          r.get(14)?,
        dim_y:          r.get(15)?,
        dim_z:          r.get(16)?,
        thumbnail_path: r.get(17)?,
        project_id:     r.get(18)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;").unwrap();
        migrate(&conn).unwrap();
        conn
    }

    fn sample_row(path: &str, title: &str, designer: &str, folder: &str, format: &str) -> ModelRow {
        ModelRow {
            id: 0,
            path: path.to_string(),
            filename: path.split('/').last().unwrap_or(path).to_string(),
            folder: folder.to_string(),
            format: format.to_string(),
            file_size: 1024,
            title: Some(title.to_string()),
            designer: Some(designer.to_string()),
            description: Some(format!("Description of {title}")),
            application: None,
            license: None,
            created_at: None,
            object_count: Some(1),
            triangle_count: Some(100),
            dim_x: Some(50.0),
            dim_y: Some(30.0),
            dim_z: Some(20.0),
            thumbnail_path: None,
            project_id: None,
        }
    }

    #[test]
    fn test_upsert_and_retrieve() {
        let conn = test_db();
        let row = sample_row("/tmp/dragon.3mf", "Dragon Model", "DesignerA", "/miniatures", "3MF");
        upsert(&conn, &row).unwrap();

        let result = get_by_id(&conn, 1).unwrap().expect("row should exist");
        assert_eq!(result.title.as_deref(), Some("Dragon Model"));
        assert_eq!(result.designer.as_deref(), Some("DesignerA"));
        assert_eq!(result.format, "3MF");
        assert!((result.dim_x.unwrap() - 50.0).abs() < 0.001);
    }

    #[test]
    fn test_upsert_updates_on_conflict() {
        let conn = test_db();
        let row = sample_row("/tmp/model.3mf", "Original Title", "DesignerA", "/", "3MF");
        upsert(&conn, &row).unwrap();

        let mut updated = sample_row("/tmp/model.3mf", "Updated Title", "DesignerA", "/", "3MF");
        updated.triangle_count = Some(200);
        upsert(&conn, &updated).unwrap();

        let count: i64 = conn.query_row("SELECT COUNT(*) FROM models", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 1, "should not insert duplicate");

        let result = get_by_id(&conn, 1).unwrap().expect("row should exist");
        assert_eq!(result.title.as_deref(), Some("Updated Title"));
        assert_eq!(result.triangle_count, Some(200));
    }

    #[test]
    fn test_search_no_filter_returns_all() {
        let conn = test_db();
        upsert(&conn, &sample_row("/tmp/a.stl", "Alpha", "Alice", "/folder1", "STL")).unwrap();
        upsert(&conn, &sample_row("/tmp/b.stl", "Beta",  "Bob",   "/folder2", "STL")).unwrap();
        upsert(&conn, &sample_row("/tmp/c.3mf", "Gamma", "Alice", "/folder1", "3MF")).unwrap();

        let results = search(&conn, &SearchParams {
            query: None, designer: None, folder: None, format: None, project: None, limit: 50, offset: 0,
        }).unwrap();
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn test_fts_search_by_title() {
        let conn = test_db();
        upsert(&conn, &sample_row("/tmp/a.stl", "Gnarly Dragon", "Alice", "/", "STL")).unwrap();
        upsert(&conn, &sample_row("/tmp/b.stl", "Cute Bunny",   "Bob",   "/", "STL")).unwrap();

        let results = search(&conn, &SearchParams {
            query: Some("dragon"), designer: None, folder: None, format: None, project: None, limit: 50, offset: 0,
        }).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title.as_deref(), Some("Gnarly Dragon"));
    }

    #[test]
    fn test_fts_search_by_project_name() {
        let conn = test_db();
        upsert(&conn, &sample_row("/tmp/a.3mf", "Dragon", "Alice", "/", "3MF")).unwrap();
        let pid = find_or_create_project(&conn, "Fallout Builds").unwrap();
        assign_project(&conn, 1, pid).unwrap();

        let results = search(&conn, &SearchParams {
            query: Some("fallout"), designer: None, folder: None, format: None, project: None, limit: 50, offset: 0,
        }).unwrap();
        assert_eq!(results.len(), 1, "should find model via project name");
        assert_eq!(results[0].title.as_deref(), Some("Dragon"));
    }

    #[test]
    fn test_fts_search_by_description() {
        let conn = test_db();
        upsert(&conn, &sample_row("/tmp/a.stl", "Model A", "Alice", "/", "STL")).unwrap();
        // description is auto-set to "Description of Model A"

        let results = search(&conn, &SearchParams {
            query: Some("Description"), designer: None, folder: None, format: None, project: None, limit: 50, offset: 0,
        }).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_filter_by_designer() {
        let conn = test_db();
        upsert(&conn, &sample_row("/tmp/a.stl", "A", "Alice", "/", "STL")).unwrap();
        upsert(&conn, &sample_row("/tmp/b.stl", "B", "Bob",   "/", "STL")).unwrap();

        let results = search(&conn, &SearchParams {
            query: None, designer: Some("Alice"), folder: None, format: None, project: None, limit: 50, offset: 0,
        }).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].designer.as_deref(), Some("Alice"));
    }

    #[test]
    fn test_filter_by_format() {
        let conn = test_db();
        upsert(&conn, &sample_row("/tmp/a.stl", "A", "Alice", "/", "STL")).unwrap();
        upsert(&conn, &sample_row("/tmp/b.3mf", "B", "Alice", "/", "3MF")).unwrap();

        let results = search(&conn, &SearchParams {
            query: None, designer: None, folder: None, format: Some("3MF"), project: None, limit: 50, offset: 0,
        }).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].format, "3MF");
    }

    #[test]
    fn test_list_designers_distinct_sorted() {
        let conn = test_db();
        upsert(&conn, &sample_row("/tmp/a.stl", "A", "Zara",  "/", "STL")).unwrap();
        upsert(&conn, &sample_row("/tmp/b.stl", "B", "Alice", "/", "STL")).unwrap();
        upsert(&conn, &sample_row("/tmp/c.stl", "C", "Alice", "/", "STL")).unwrap();

        let designers = list_designers(&conn).unwrap();
        assert_eq!(designers, vec!["Alice", "Zara"], "should be sorted, deduplicated");
    }

    #[test]
    fn test_count() {
        let conn = test_db();
        assert_eq!(count(&conn).unwrap(), 0);
        upsert(&conn, &sample_row("/tmp/a.stl", "A", "Alice", "/", "STL")).unwrap();
        upsert(&conn, &sample_row("/tmp/b.stl", "B", "Bob",   "/", "STL")).unwrap();
        assert_eq!(count(&conn).unwrap(), 2);
    }

    #[test]
    fn test_dims_str_format() {
        let conn = test_db();
        upsert(&conn, &sample_row("/tmp/a.stl", "A", "Alice", "/", "STL")).unwrap();
        let row = get_by_id(&conn, 1).unwrap().unwrap();
        let dims = row.dims_str().unwrap();
        assert!(dims.contains("50.0"), "x dim should appear");
        assert!(dims.contains("mm"),   "units should be mm");
    }

    #[test]
    fn test_display_name_falls_back_to_filename() {
        let conn = test_db();
        let mut row = sample_row("/tmp/model.stl", "Named", "Alice", "/", "STL");
        upsert(&conn, &row).unwrap();
        let r = get_by_id(&conn, 1).unwrap().unwrap();
        assert_eq!(r.display_name(), "Named");

        // Now a row with no title
        row.path = "/tmp/unnamed.stl".to_string();
        row.filename = "unnamed.stl".to_string();
        row.title = None;
        upsert(&conn, &row).unwrap();
        let r2 = get_by_id(&conn, 2).unwrap().unwrap();
        assert_eq!(r2.display_name(), "unnamed.stl");
    }
}
