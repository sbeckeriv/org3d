use anyhow::Result;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

pub struct StlMeta {
    pub triangle_count: u64,
    pub dim_x: Option<f64>,
    pub dim_y: Option<f64>,
    pub dim_z: Option<f64>,
    // Only set for ASCII STL with a named solid
    pub name: Option<String>,
    // Only set for binary STL with a non-empty header
    pub header: Option<String>,
}

/// Parse STL metadata by streaming the file, so memory use stays constant
/// no matter how large the mesh is.
pub fn extract(path: &Path) -> Result<StlMeta> {
    let file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut r = BufReader::with_capacity(1 << 20, file);
    let mut head = [0u8; 84];
    let n = read_up_to(&mut r, &mut head)?;
    let head = &head[..n];
    if is_binary(head, len) {
        extract_binary(head, r)
    } else {
        extract_ascii(BufReader::new(head.chain(r)))
    }
}

/// Fill `buf` as far as the input allows (short files are fine).
fn read_up_to(r: &mut impl Read, buf: &mut [u8]) -> Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

fn is_binary(head: &[u8], file_len: u64) -> bool {
    if head.len() < 84 {
        return false;
    }
    // If first 5 bytes are "solid", it *might* be ASCII — but some binary files do this too.
    // Cross-check with the declared triangle count vs file size.
    let claimed = u32::from_le_bytes(head[80..84].try_into().expect("4 bytes")) as u64;
    let expected = 84 + claimed * 50;
    if file_len == expected && claimed > 0 {
        return true;
    }
    !head.starts_with(b"solid")
}

struct Bounds {
    min: [f64; 3],
    max: [f64; 3],
}

impl Bounds {
    fn new() -> Self {
        Self {
            min: [f64::MAX; 3],
            max: [f64::MIN; 3],
        }
    }
    fn add(&mut self, p: [f64; 3]) {
        for (i, v) in p.into_iter().enumerate() {
            self.min[i] = self.min[i].min(v);
            self.max[i] = self.max[i].max(v);
        }
    }
    fn dims(&self, triangles: u64) -> [Option<f64>; 3] {
        if triangles == 0 {
            return [None; 3];
        }
        [0, 1, 2].map(|i| Some(self.max[i] - self.min[i]))
    }
}

fn extract_binary(head: &[u8], mut r: impl Read) -> Result<StlMeta> {
    let header_str = std::str::from_utf8(&head[..80])
        .unwrap_or("")
        .trim_end_matches('\0')
        .trim()
        .to_string();
    let header = (!header_str.is_empty()).then_some(header_str);
    let triangle_count = u32::from_le_bytes(head[80..84].try_into().expect("4 bytes")) as u64;

    let mut b = Bounds::new();
    let mut tri = [0u8; 50];
    for _ in 0..triangle_count {
        // A truncated file ends the loop, same as before streaming.
        if read_up_to(&mut r, &mut tri)? < 50 {
            break;
        }
        // 12 bytes normal, then 3 vertices × 12 bytes
        for v in 0..3usize {
            let off = 12 + v * 12;
            let f =
                |o: usize| f32::from_le_bytes(tri[o..o + 4].try_into().expect("4 bytes")) as f64;
            b.add([f(off), f(off + 4), f(off + 8)]);
        }
    }

    let [dim_x, dim_y, dim_z] = b.dims(triangle_count);
    Ok(StlMeta {
        triangle_count,
        dim_x,
        dim_y,
        dim_z,
        name: None,
        header,
    })
}

/// Longest ASCII line we bother parsing; longer lines (e.g. a binary file
/// without a newline) are skipped without buffering them.
const MAX_LINE: u64 = 4096;

fn extract_ascii(mut r: impl BufRead) -> Result<StlMeta> {
    let mut triangles: u64 = 0;
    let mut b = Bounds::new();
    let mut name: Option<String> = None;
    let mut buf = Vec::with_capacity(256);
    let mut first = true;

    loop {
        buf.clear();
        let n = (&mut r).take(MAX_LINE).read_until(b'\n', &mut buf)?;
        if n == 0 {
            break;
        }
        if n as u64 == MAX_LINE && buf.last() != Some(&b'\n') {
            // Discard the rest of an over-long line.
            let mut sink = Vec::new();
            loop {
                sink.clear();
                let k = (&mut r).take(MAX_LINE).read_until(b'\n', &mut sink)?;
                if k == 0 || sink.last() == Some(&b'\n') {
                    break;
                }
            }
            first = false;
            continue;
        }
        let text = String::from_utf8_lossy(&buf);
        let line = text.trim();
        if first && let Some(rest) = line.strip_prefix("solid") {
            let n = rest.trim();
            if !n.is_empty() {
                name = Some(n.to_string());
            }
        } else if line.starts_with("facet normal") {
            triangles += 1;
        } else if let Some(rest) = line.strip_prefix("vertex ") {
            let c: Vec<f64> = rest
                .split_whitespace()
                .filter_map(|s| s.parse().ok())
                .collect();
            if c.len() == 3 {
                b.add([c[0], c[1], c[2]]);
            }
        }
        first = false;
    }

    let [dim_x, dim_y, dim_z] = b.dims(triangles);
    Ok(StlMeta {
        triangle_count: triangles,
        dim_x,
        dim_y,
        dim_z,
        name,
        header: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static ID: AtomicU64 = AtomicU64::new(0);

    fn tmp_path(suffix: &str) -> std::path::PathBuf {
        let id = ID.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("org3d_stl_{id}_{suffix}"))
    }

    fn make_binary_stl(header_str: &str, tris: &[[[f32; 3]; 4]]) -> Vec<u8> {
        let mut buf = vec![0u8; 80];
        let hb = header_str.as_bytes();
        buf[..hb.len().min(80)].copy_from_slice(&hb[..hb.len().min(80)]);
        buf.extend_from_slice(&(tris.len() as u32).to_le_bytes());
        for tri in tris {
            for v3 in tri {
                for &f in v3 {
                    buf.extend_from_slice(&f.to_le_bytes());
                }
            }
            buf.extend_from_slice(&[0u8, 0u8]); // attribute byte count
        }
        buf
    }

    #[test]
    fn test_binary_triangle_count_and_bbox() {
        let tris = [
            [
                [0.0f32, 0.0, 1.0],
                [0.0, 0.0, 0.0],
                [10.0, 0.0, 0.0],
                [0.0, 20.0, 0.0],
            ],
            [
                [0.0f32, 0.0, 1.0],
                [10.0, 0.0, 0.0],
                [10.0, 0.0, 5.0],
                [0.0, 20.0, 0.0],
            ],
        ];
        let data = make_binary_stl("", &tris);
        let path = tmp_path("binary.stl");
        std::fs::write(&path, &data).unwrap();
        let meta = extract(&path).unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(meta.triangle_count, 2);
        assert!(
            (meta.dim_x.unwrap() - 10.0).abs() < 0.001,
            "x dim should be 10"
        );
        assert!(
            (meta.dim_y.unwrap() - 20.0).abs() < 0.001,
            "y dim should be 20"
        );
        assert!(
            (meta.dim_z.unwrap() - 5.0).abs() < 0.001,
            "z dim should be 5"
        );
        assert!(meta.name.is_none());
    }

    #[test]
    fn test_binary_header_string() {
        let data = make_binary_stl("PrusaSlicer 2.7", &[[[0.0f32; 3]; 4]]);
        // Single all-zero triangle: size is 84 + 50 = 134, tri count = 1 → detected as binary
        let path = tmp_path("header.stl");
        std::fs::write(&path, &data).unwrap();
        let meta = extract(&path).unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(meta.header.as_deref(), Some("PrusaSlicer 2.7"));
    }

    #[test]
    fn test_ascii_parse() {
        let stl = b"solid my_model\n\
            facet normal 0 0 1\n  outer loop\n\
              vertex 0 0 0\n    vertex 5 0 0\n    vertex 0 8 0\n  endloop\nendfacet\n\
            endsolid my_model\n";
        let path = tmp_path("ascii.stl");
        std::fs::write(&path, stl).unwrap();
        let meta = extract(&path).unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(meta.triangle_count, 1);
        assert_eq!(meta.name.as_deref(), Some("my_model"));
        assert!((meta.dim_x.unwrap() - 5.0).abs() < 0.001);
        assert!((meta.dim_y.unwrap() - 8.0).abs() < 0.001);
        assert!(meta.header.is_none());
    }

    #[test]
    fn test_ascii_unnamed_solid() {
        let stl = b"solid\nfacet normal 0 0 1\n  outer loop\n\
            vertex 0 0 0\n    vertex 1 0 0\n    vertex 0 1 0\n  endloop\nendfacet\nendsolid\n";
        let path = tmp_path("unnamed.stl");
        std::fs::write(&path, stl).unwrap();
        let meta = extract(&path).unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(meta.triangle_count, 1);
        assert!(meta.name.is_none(), "empty solid name should not be stored");
    }

    #[test]
    fn test_ascii_skips_overlong_lines() {
        let mut stl = b"solid big\n".to_vec();
        stl.extend(std::iter::repeat_n(b'x', 20_000));
        stl.extend_from_slice(
            b"\nfacet normal 0 0 1\n outer loop\n vertex 0 0 0\n vertex 3 0 0\n vertex 0 4 0\n endloop\nendfacet\nendsolid\n",
        );
        let path = tmp_path("long.stl");
        std::fs::write(&path, &stl).unwrap();
        let meta = extract(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(meta.triangle_count, 1);
        assert_eq!(meta.name.as_deref(), Some("big"));
        assert!((meta.dim_x.unwrap() - 3.0).abs() < 0.001);
    }

    #[test]
    fn test_truncated_binary_does_not_error() {
        let mut data = vec![0u8; 80];
        data.extend_from_slice(&5u32.to_le_bytes()); // claims 5 triangles
        data.extend_from_slice(&[0u8; 60]); // only 1.2 present
        let path = tmp_path("trunc.stl");
        std::fs::write(&path, &data).unwrap();
        let meta = extract(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(meta.triangle_count, 5);
    }

    #[test]
    fn test_binary_detection_by_size() {
        // File size that exactly matches binary format → detected as binary even if starts with "solid"
        let mut header = [0u8; 80];
        header[..5].copy_from_slice(b"solid");
        let mut data = header.to_vec();
        data.extend_from_slice(&1u32.to_le_bytes()); // 1 triangle
        data.extend_from_slice(&[0u8; 50]); // 1 × 50 bytes
        // Total: 84 + 50 = 134 bytes → binary
        let path = tmp_path("ambiguous.stl");
        std::fs::write(&path, &data).unwrap();
        let meta = extract(&path).unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(meta.triangle_count, 1);
        assert!(
            meta.name.is_none(),
            "binary STL should not have a solid name"
        );
    }
}
