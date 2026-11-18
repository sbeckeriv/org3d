//! Scanner behaviour that matters for libraries on sleeping disks.

use org3d::{db, scanner};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static ID: AtomicU64 = AtomicU64::new(0);

struct Lib {
    root: PathBuf,
    thumbs: PathBuf,
    conn: rusqlite::Connection,
    base: PathBuf,
}

impl Drop for Lib {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn lib() -> Lib {
    let id = ID.fetch_add(1, Ordering::SeqCst);
    let base = std::env::temp_dir().join(format!("org3d_scan_{}_{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("library");
    std::fs::create_dir_all(&root).unwrap();
    let conn = db::open(base.join("t.db").to_str().unwrap()).unwrap();
    Lib {
        thumbs: base.join("thumbs"),
        root,
        conn,
        base,
    }
}

/// Minimal valid binary STL with `n` copies of one triangle, so different `n`
/// gives files with different sizes.
fn write_stl(path: &Path, n: u32) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut buf = vec![0u8; 80];
    buf.extend_from_slice(&n.to_le_bytes());
    for _ in 0..n {
        buf.extend_from_slice(&[0u8; 12]);
        for v in [[0f32, 0., 0.], [10., 0., 0.], [0., 10., 5.]] {
            for c in v {
                buf.extend_from_slice(&c.to_le_bytes());
            }
        }
        buf.extend_from_slice(&[0u8; 2]);
    }
    std::fs::write(path, buf).unwrap();
}

impl Lib {
    fn scan(&self) -> scanner::ScanStats {
        scanner::scan(&self.conn, &self.root, &self.thumbs).unwrap()
    }
    fn scan_opts(&self, opts: scanner::ScanOptions) -> scanner::ScanStats {
        scanner::scan_with(&self.conn, &self.root, &self.thumbs, &opts).unwrap()
    }
    fn id_of(&self, p: &Path) -> Option<i64> {
        self.conn
            .query_row(
                "SELECT id FROM models WHERE path = ?1",
                [p.to_string_lossy()],
                |r| r.get(0),
            )
            .ok()
    }
}

#[test]
fn unchanged_files_are_not_reopened() {
    let l = lib();
    let f = l.root.join("a.stl");
    write_stl(&f, 1);
    assert_eq!(l.scan().inserted, 1);

    // Replace contents with garbage of the same size, keeping mtime: if the
    // rescan opened the file, parsing would fail and count as an error.
    let mtime = std::fs::metadata(&f).unwrap().modified().unwrap();
    let len = std::fs::metadata(&f).unwrap().len() as usize;
    std::fs::write(&f, vec![0xFFu8; len]).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&f)
        .unwrap()
        .set_modified(mtime)
        .unwrap();

    let s = l.scan();
    assert_eq!((s.unchanged, s.inserted, s.errors), (1, 0, 0));
}

#[test]
fn moved_file_keeps_id_and_project() {
    let l = lib();
    let old = l.root.join("a.stl");
    write_stl(&old, 1);
    write_stl(&l.root.join("other.stl"), 2);
    l.scan();
    let id = l.id_of(&old).unwrap();
    let pid = db::find_or_create_project(&l.conn, "Keep me").unwrap();
    db::assign_project(&l.conn, id, pid).unwrap();

    let new = l.root.join("sub/renamed.stl");
    std::fs::create_dir_all(new.parent().unwrap()).unwrap();
    std::fs::rename(&old, &new).unwrap(); // rename preserves mtime

    let s = l.scan();
    assert_eq!((s.moved, s.inserted, s.missing), (1, 0, 0));
    assert_eq!(l.id_of(&new), Some(id));
    let row = db::get_by_id(&l.conn, id).unwrap().unwrap();
    assert_eq!(row.project_id, Some(pid));
    assert_eq!(row.folder, "/sub");
    assert_eq!(row.filename, "renamed.stl");
}

#[test]
fn deleted_file_is_hidden_then_restored_with_project() {
    let l = lib();
    let gone = l.root.join("gone.stl");
    write_stl(&gone, 1);
    write_stl(&l.root.join("b.stl"), 2);
    write_stl(&l.root.join("c.stl"), 3);
    l.scan();
    let id = l.id_of(&gone).unwrap();
    let pid = db::find_or_create_project(&l.conn, "P").unwrap();
    db::assign_project(&l.conn, id, pid).unwrap();

    let stash = l.base.join("stash.stl");
    std::fs::rename(&gone, &stash).unwrap();
    assert_eq!(l.scan().missing, 1);
    assert_eq!(db::count(&l.conn).unwrap(), 2, "missing row hidden");
    assert_eq!(db::count_all(&l.conn).unwrap(), 3, "but not deleted");

    std::fs::rename(&stash, &gone).unwrap();
    let s = l.scan();
    assert_eq!(s.restored, 1);
    assert_eq!(db::count(&l.conn).unwrap(), 3);
    let row = db::get_by_id(&l.conn, id).unwrap().unwrap();
    assert_eq!(row.project_id, Some(pid), "project survives a disk outage");
}

#[test]
fn mass_disappearance_is_refused_unless_allowed() {
    let l = lib();
    for i in 0..40 {
        write_stl(&l.root.join(format!("d{i}.stl")), i + 1);
    }
    l.scan();
    // Simulate an array disk dropping out: most files vanish, no I/O errors.
    for i in 0..30 {
        std::fs::remove_file(l.root.join(format!("d{i}.stl"))).unwrap();
    }
    let s = l.scan();
    assert_eq!(s.missing, 0, "guard should refuse");
    assert!(
        s.warnings.iter().any(|w| w.contains("vanished at once")),
        "refusal must be reported, not just logged: {:?}",
        s.warnings
    );
    assert_eq!(db::count(&l.conn).unwrap(), 40);

    let s = l.scan_opts(scanner::ScanOptions {
        allow_mass_missing: true,
        ..Default::default()
    });
    assert_eq!(s.missing, 30);
    assert_eq!(db::count(&l.conn).unwrap(), 10);
}

#[test]
fn empty_library_folder_hides_nothing() {
    let l = lib();
    write_stl(&l.root.join("a.stl"), 1);
    l.scan();
    std::fs::remove_file(l.root.join("a.stl")).unwrap();
    let s = l.scan();
    assert_eq!(s.missing, 0);
    assert!(s.warnings.iter().any(|w| w.contains("mounted")));
    assert_eq!(db::count(&l.conn).unwrap(), 1);
}

#[test]
fn missing_library_folder_is_an_error_not_a_wipe() {
    let l = lib();
    write_stl(&l.root.join("a.stl"), 1);
    l.scan();
    std::fs::remove_dir_all(&l.root).unwrap();
    assert!(scanner::scan(&l.conn, &l.root, &l.thumbs).is_err());
    assert_eq!(db::count(&l.conn).unwrap(), 1);
}

#[test]
fn purge_removes_rows_missing_past_grace_period() {
    let l = lib();
    write_stl(&l.root.join("a.stl"), 1);
    write_stl(&l.root.join("b.stl"), 2);
    write_stl(&l.root.join("c.stl"), 3);
    l.scan();
    std::fs::remove_file(l.root.join("a.stl")).unwrap();
    l.scan();
    l.conn
        .execute(
            "UPDATE models SET missing_since = datetime('now', '-40 days') WHERE missing_since IS NOT NULL",
            [],
        )
        .unwrap();
    assert_eq!(l.scan().purged, 1);
    assert_eq!(db::count_all(&l.conn).unwrap(), 2);
}

#[test]
fn macos_metadata_files_are_ignored_and_old_rows_dropped() {
    let l = lib();
    write_stl(&l.root.join("a/cube.stl"), 1);
    // AppleDouble twins and a zip-made __MACOSX folder: never models.
    std::fs::write(l.root.join("a/._cube.stl"), vec![0u8; 4096]).unwrap();
    std::fs::write(l.root.join("a/._wall.stl"), vec![0u8; 4096]).unwrap();
    write_stl(&l.root.join("__MACOSX/a/cube.stl"), 1);

    // Simulate rows indexed by an older version that didn't skip them.
    for name in ["._cube.stl", "._wall.stl"] {
        let path = l.root.join("a").join(name);
        db::upsert(
            &l.conn,
            &db::ModelRow {
                id: 0,
                path: path.to_string_lossy().into_owned(),
                filename: name.into(),
                folder: "a".into(),
                format: "STL".into(),
                file_size: 4096,
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
                mtime: Some(1),
            },
        )
        .unwrap();
    }

    let s = l.scan();
    assert_eq!(s.scanned, 1, "only the real model is found");
    assert_eq!(s.junk_removed, 2);
    assert_eq!(s.missing, 0, "junk is dropped, not soft-deleted");
    assert!(s.warnings.is_empty(), "{:?}", s.warnings);
    assert_eq!(db::count_all(&l.conn).unwrap(), 1);
}
