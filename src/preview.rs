//! On-disk cache of the STL bytes served to the 3D viewer.
//!
//! Lives on the app-data volume (SSD/cache pool on Unraid), so opening the
//! same model again doesn't wake the array disk holding the original. Entries
//! are keyed by `id-size-mtime` (all from the DB), so a hit needs no access to
//! the library at all, and a changed file gets a new key automatically.
//! Size-capped with least-recently-used eviction (hits refresh the mtime).

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::db::FileRef;

/// Where to get preview bytes from.
pub enum Preview {
    /// Stream this file (a cache entry, or an STL straight from the library).
    File(PathBuf),
    /// Freshly converted mesh already in memory.
    Bytes(Vec<u8>),
}

pub struct PreviewCache {
    dir: PathBuf,
    max_bytes: u64,
}

impl PreviewCache {
    /// `max_bytes == 0` disables caching (every view reads the library).
    pub fn new(dir: impl Into<PathBuf>, max_bytes: u64) -> Self {
        Self {
            dir: dir.into(),
            max_bytes,
        }
    }

    pub fn key(id: i64, f: &FileRef) -> String {
        format!("{id}-{}-{}", f.file_size, f.mtime.unwrap_or(0))
    }

    fn path_for(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.stl"))
    }

    /// Cached preview for this model, if any. Touches it for LRU. Never
    /// reads the library, so it's safe to call before taking a build slot.
    pub fn cached(&self, id: i64, f: &FileRef) -> Option<PathBuf> {
        if self.max_bytes == 0 {
            return None;
        }
        let p = self.path_for(&Self::key(id, f));
        p.is_file().then(|| {
            touch(&p);
            p
        })
    }

    /// Produce STL for the model, reading the library. Blocking.
    ///
    /// STL sources are never loaded into memory: they're copied into the
    /// cache (or, if caching is off/too small, served straight from the
    /// library). Only 3MF conversion has to build the mesh in memory.
    pub fn build(&self, id: i64, f: &FileRef) -> Result<Preview> {
        let key = Self::key(id, f);
        let cached = self.path_for(&key);
        let fits = self.max_bytes > 0 && (f.file_size.max(0) as u64) <= self.max_bytes;

        if f.format == "STL" {
            if fits {
                match self.store_with(&key, &cached, |tmp| std::fs::copy(&f.path, tmp).map(|_| ()))
                {
                    Ok(()) => return Ok(Preview::File(cached)),
                    Err(e) => tracing::warn!("preview cache write failed: {e:#}"),
                }
            }
            return Ok(Preview::File(PathBuf::from(&f.path)));
        }

        let bytes = crate::formats::threemf::to_binary_stl(Path::new(&f.path))?;
        if self.max_bytes > 0
            && (bytes.len() as u64) <= self.max_bytes
            && let Err(e) = self.store_with(&key, &cached, |tmp| std::fs::write(tmp, &bytes))
        {
            tracing::warn!("preview cache write failed: {e:#}");
        }
        Ok(Preview::Bytes(bytes))
    }

    /// Convenience for callers that want bytes regardless of source.
    pub fn get_or_build(&self, id: i64, f: &FileRef) -> Result<Vec<u8>> {
        if let Some(p) = self.cached(id, f) {
            return Ok(std::fs::read(p)?);
        }
        match self.build(id, f)? {
            Preview::File(p) => Ok(std::fs::read(p)?),
            Preview::Bytes(b) => Ok(b),
        }
    }

    fn store_with(
        &self,
        key: &str,
        path: &Path,
        write: impl FnOnce(&Path) -> std::io::Result<()>,
    ) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let tmp = self.dir.join(format!("{key}.tmp"));
        if let Err(e) = write(&tmp) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        std::fs::rename(&tmp, path)?;
        self.evict(key)?;
        Ok(())
    }

    /// Drop stale versions of `key`'s model, then oldest entries until under cap.
    fn evict(&self, key: &str) -> Result<()> {
        let id_prefix = format!("{}-", key.split('-').next().unwrap_or(""));
        let mut entries = Vec::new();
        let mut total = 0u64;
        for e in std::fs::read_dir(&self.dir)? {
            let e = e?;
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(stem) = name.strip_suffix(".stl") else {
                continue;
            };
            if stem != key && stem.starts_with(&id_prefix) {
                let _ = std::fs::remove_file(e.path());
                continue;
            }
            let md = e.metadata()?;
            total += md.len();
            entries.push((
                md.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                md.len(),
                e.path(),
            ));
        }
        if total <= self.max_bytes {
            return Ok(());
        }
        entries.sort_by_key(|(t, _, _)| *t);
        for (_, len, path) in entries {
            if total <= self.max_bytes {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                total -= len;
            }
        }
        Ok(())
    }
}

fn touch(path: &Path) {
    if let Ok(f) = std::fs::File::options().write(true).open(path) {
        let _ = f.set_modified(SystemTime::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("org3d_preview_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn stl_ref(dir: &Path, name: &str, len: usize, mtime: i64) -> FileRef {
        let p = dir.join(name);
        std::fs::write(&p, vec![7u8; len]).unwrap();
        FileRef {
            path: p.to_string_lossy().into_owned(),
            filename: name.into(),
            format: "STL".into(),
            file_size: len as i64,
            mtime: Some(mtime),
        }
    }

    #[test]
    fn hit_does_not_touch_source() {
        let lib = tmp_dir("lib_hit");
        let cache = PreviewCache::new(tmp_dir("cache_hit"), 1 << 20);
        let f = stl_ref(&lib, "a.stl", 100, 1);
        assert_eq!(cache.get_or_build(1, &f).unwrap().len(), 100);
        // Simulate the library disk being unavailable: a hit must still work.
        std::fs::remove_file(&f.path).unwrap();
        assert_eq!(cache.get_or_build(1, &f).unwrap().len(), 100);
    }

    #[test]
    fn changed_file_replaces_old_entry_and_cap_is_enforced() {
        let lib = tmp_dir("lib_cap");
        let cdir = tmp_dir("cache_cap");
        let cache = PreviewCache::new(&cdir, 250);
        let a1 = stl_ref(&lib, "a.stl", 100, 1);
        cache.get_or_build(1, &a1).unwrap();
        let a2 = stl_ref(&lib, "a.stl", 100, 2);
        cache.get_or_build(1, &a2).unwrap();
        let count = |d: &Path| std::fs::read_dir(d).unwrap().count();
        assert_eq!(
            count(&cdir),
            1,
            "stale version of model 1 should be removed"
        );

        let b = stl_ref(&lib, "b.stl", 100, 1);
        let c = stl_ref(&lib, "c.stl", 100, 1);
        cache.get_or_build(2, &b).unwrap();
        cache.get_or_build(3, &c).unwrap();
        let total: u64 = std::fs::read_dir(&cdir)
            .unwrap()
            .map(|e| e.unwrap().metadata().unwrap().len())
            .sum();
        assert!(total <= 250, "cache over cap: {total}");
    }

    #[test]
    fn disabled_cache_writes_nothing() {
        let lib = tmp_dir("lib_off");
        let cdir = tmp_dir("cache_off");
        let cache = PreviewCache::new(&cdir, 0);
        let f = stl_ref(&lib, "a.stl", 10, 1);
        cache.get_or_build(1, &f).unwrap();
        assert_eq!(std::fs::read_dir(&cdir).unwrap().count(), 0);
    }
}
