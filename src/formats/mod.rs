pub mod stl;
pub mod threemf;

use anyhow::{Result, bail};
use std::io::Read;

/// Limits for decompressing ZIP entries (3MF parts, extracted archives), so a
/// hostile or corrupt file can't exhaust memory or fill a disk.
#[derive(Debug, Clone, Copy)]
pub struct ZipLimits {
    /// Max uncompressed bytes for one entry.
    pub max_entry: u64,
    /// Max uncompressed/compressed ratio, applied once an entry is larger
    /// than `ratio_min_size`. Real 3MF XML compresses ~10–30×; bombs ~1000×.
    pub max_ratio: u64,
    pub ratio_min_size: u64,
}

/// For reading 3MF parts into memory.
pub const IN_MEMORY: ZipLimits = ZipLimits {
    max_entry: 1 << 30, // 1 GiB
    max_ratio: 200,
    ratio_min_size: 64 << 20,
};

/// For extracting archives to disk.
pub const TO_DISK: ZipLimits = ZipLimits {
    max_entry: 8 << 30, // 8 GiB
    max_ratio: 200,
    ratio_min_size: 64 << 20,
};

impl ZipLimits {
    /// Reject an entry from its header before decompressing anything.
    pub fn check<R: Read + ?Sized>(&self, e: &zip::read::ZipFile<'_, R>) -> Result<()> {
        let size = e.size();
        let packed = e.compressed_size().max(1);
        if size > self.max_entry {
            bail!(
                "zip entry {} is too large ({} MiB uncompressed)",
                e.name().unwrap_or_default(),
                size >> 20
            );
        }
        if size > self.ratio_min_size && size / packed > self.max_ratio {
            bail!(
                "zip entry {} looks like a zip bomb ({}× compression)",
                e.name().unwrap_or_default(),
                size / packed
            );
        }
        Ok(())
    }

    /// Read a whole entry into memory, enforcing the limits even if the
    /// header lies about the uncompressed size.
    pub fn read<R: Read + ?Sized>(&self, e: &mut zip::read::ZipFile<'_, R>) -> Result<Vec<u8>> {
        self.check(e)?;
        let mut buf = Vec::with_capacity(e.size().min(64 << 20) as usize);
        e.take(self.max_entry + 1).read_to_end(&mut buf)?;
        if buf.len() as u64 > self.max_entry {
            bail!("zip entry exceeds {} MiB", self.max_entry >> 20);
        }
        Ok(buf)
    }

    pub fn read_string<R: Read + ?Sized>(
        &self,
        e: &mut zip::read::ZipFile<'_, R>,
    ) -> Result<String> {
        Ok(String::from_utf8(self.read(e)?)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn zip_with(name: &str, data: &[u8]) -> zip::ZipArchive<std::io::Cursor<Vec<u8>>> {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        w.start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        w.write_all(data).unwrap();
        zip::ZipArchive::new(std::io::Cursor::new(w.finish().unwrap().into_inner())).unwrap()
    }

    #[test]
    fn rejects_high_ratio_entries() {
        let limits = ZipLimits {
            max_entry: 1 << 30,
            max_ratio: 50,
            ratio_min_size: 1024,
        };
        let mut z = zip_with("bomb.model", &vec![0u8; 4 << 20]); // compresses ~1000×
        let mut e = z.by_index(0).unwrap();
        let err = limits.read(&mut e).unwrap_err().to_string();
        assert!(err.contains("zip bomb"), "{err}");
    }

    #[test]
    fn rejects_oversized_entries() {
        let limits = ZipLimits {
            max_entry: 1000,
            max_ratio: u64::MAX,
            ratio_min_size: u64::MAX,
        };
        let mut z = zip_with("big.model", &vec![7u8; 2000]);
        let mut e = z.by_index(0).unwrap();
        assert!(limits.read(&mut e).is_err());
    }

    #[test]
    fn reads_normal_entries() {
        let mut z = zip_with("3D/3dmodel.model", b"<model/>");
        let mut e = z.by_index(0).unwrap();
        assert_eq!(IN_MEMORY.read_string(&mut e).unwrap(), "<model/>");
    }
}
