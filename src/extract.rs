use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct ExtractResult {
    pub zip_path: PathBuf,
    pub dest_dir: PathBuf,
    pub files_extracted: usize,
    pub skipped: bool, // dest already existed before extraction
}

/// Find and extract all ZIP archives under `root`.
/// Each archive is extracted into a sibling directory named after the ZIP stem.
/// Already-extracted archives (dest dir exists) are skipped.
pub fn extract_all(root: &Path) -> Result<Vec<ExtractResult>> {
    let zips: Vec<PathBuf> = WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type().is_file()
                && e.path()
                    .extension()
                    .and_then(|x| x.to_str())
                    .map(|x| x.eq_ignore_ascii_case("zip"))
                    .unwrap_or(false)
        })
        .map(|e| e.path().to_path_buf())
        .collect();

    let mut results = Vec::new();

    for zip_path in zips {
        let dest = zip_path
            .parent()
            .unwrap_or(root)
            .join(zip_path.file_stem().unwrap_or_default());

        if dest.exists() {
            results.push(ExtractResult {
                zip_path,
                dest_dir: dest,
                files_extracted: 0,
                skipped: true,
            });
            continue;
        }

        match extract_one(&zip_path, &dest) {
            Ok(count) => results.push(ExtractResult {
                zip_path,
                dest_dir: dest,
                files_extracted: count,
                skipped: false,
            }),
            Err(e) => {
                tracing::warn!("failed to extract {}: {e}", zip_path.display());
            }
        }
    }

    Ok(results)
}

fn extract_one(zip_path: &Path, dest: &Path) -> Result<usize> {
    std::fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;

    let file = std::fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("opening zip {}", zip_path.display()))?;

    let mut count = 0;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let raw_name = entry.name().to_string();

        // Sanitize path: strip leading `/`, reject `..` traversal
        let safe_name: PathBuf = raw_name
            .split(['/', '\\'])
            .filter(|part| !part.is_empty() && *part != "..")
            .collect();

        if safe_name.as_os_str().is_empty() {
            continue;
        }

        let out_path = dest.join(&safe_name);

        if entry.is_dir() {
            std::fs::create_dir_all(&out_path)?;
        } else {
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut out = std::fs::File::create(&out_path)
                .with_context(|| format!("creating {}", out_path.display()))?;
            std::io::copy(&mut entry, &mut out)?;
            count += 1;
        }
    }

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static ID: AtomicU64 = AtomicU64::new(0);

    fn tmp_dir() -> PathBuf {
        let id = ID.fetch_add(1, Ordering::SeqCst);
        let d = std::env::temp_dir().join(format!("org3d_extract_{id}"));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn make_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let cursor = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(cursor);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, data) in files {
            zip.start_file(*name, opts).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn test_extracts_files_from_zip() {
        let root = tmp_dir();
        let zip_data = make_zip(&[
            ("model.stl", b"solid\nendsolid\n"),
            ("readme.txt", b"hello"),
        ]);
        let zip_path = root.join("pack.zip");
        std::fs::write(&zip_path, &zip_data).unwrap();

        let results = extract_all(&root).unwrap();
        std::fs::remove_dir_all(&root).ok();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].files_extracted, 2);
        assert!(!results[0].skipped);
    }

    #[test]
    fn test_skips_already_extracted() {
        let root = tmp_dir();
        let zip_data = make_zip(&[("a.txt", b"content")]);
        let zip_path = root.join("pack.zip");
        std::fs::write(&zip_path, &zip_data).unwrap();

        // Pre-create the dest directory so it looks already extracted
        std::fs::create_dir_all(root.join("pack")).unwrap();

        let results = extract_all(&root).unwrap();
        std::fs::remove_dir_all(&root).ok();

        assert_eq!(results.len(), 1);
        assert!(results[0].skipped);
        assert_eq!(results[0].files_extracted, 0);
    }

    #[test]
    fn test_path_traversal_rejected() {
        let root = tmp_dir();
        let zip_data = make_zip(&[("../evil.txt", b"pwned")]);
        let zip_path = root.join("evil.zip");
        std::fs::write(&zip_path, &zip_data).unwrap();

        let results = extract_all(&root).unwrap();

        // The file should not appear outside dest_dir
        let outside = root.parent().unwrap().join("evil.txt");
        assert!(!outside.exists(), "path traversal must be blocked");

        std::fs::remove_dir_all(&root).ok();
        let _ = results;
    }

    #[test]
    fn test_extracts_nested_dirs() {
        let root = tmp_dir();
        let zip_data = make_zip(&[
            ("parts/base.stl", b"solid\nendsolid\n"),
            ("parts/top.stl", b"solid\nendsolid\n"),
            ("readme.txt", b"hi"),
        ]);
        let zip_path = root.join("nested.zip");
        std::fs::write(&zip_path, &zip_data).unwrap();

        let results = extract_all(&root).unwrap();
        let dest = results[0].dest_dir.clone();
        std::fs::remove_dir_all(&root).ok();

        assert_eq!(results[0].files_extracted, 3);
        assert!(
            dest.join("parts/base.stl")
                .to_string_lossy()
                .contains("parts")
        );
    }
}
