use anyhow::Result;
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

pub fn extract(path: &Path) -> Result<StlMeta> {
    let data = std::fs::read(path)?;
    if is_binary(&data) {
        extract_binary(&data)
    } else {
        extract_ascii(&data)
    }
}

fn is_binary(data: &[u8]) -> bool {
    if data.len() < 84 {
        return false;
    }
    // If first 5 bytes are "solid", it *might* be ASCII — but some binary files do this too.
    // Cross-check with the declared triangle count vs file size.
    let claimed = u32::from_le_bytes(data[80..84].try_into().unwrap()) as u64;
    let expected = 84 + claimed * 50;
    if data.len() as u64 == expected && claimed > 0 {
        return true;
    }
    !data.starts_with(b"solid")
}

fn extract_binary(data: &[u8]) -> Result<StlMeta> {
    if data.len() < 84 {
        anyhow::bail!("binary STL too short");
    }

    let header_bytes = &data[..80];
    let header_str = std::str::from_utf8(header_bytes)
        .unwrap_or("")
        .trim_end_matches('\0')
        .trim()
        .to_string();
    let header = if header_str.is_empty() {
        None
    } else {
        Some(header_str)
    };

    let triangle_count = u32::from_le_bytes(data[80..84].try_into().unwrap()) as u64;

    let mut min = [f64::MAX; 3];
    let mut max = [f64::MIN; 3];

    for i in 0..triangle_count as usize {
        let base = 84 + i * 50;
        if base + 50 > data.len() {
            break;
        }
        // 12 bytes normal, then 3 vertices × 12 bytes
        for v in 0..3usize {
            let off = base + 12 + v * 12;
            let x = f32::from_le_bytes(data[off..off + 4].try_into().unwrap()) as f64;
            let y = f32::from_le_bytes(data[off + 4..off + 8].try_into().unwrap()) as f64;
            let z = f32::from_le_bytes(data[off + 8..off + 12].try_into().unwrap()) as f64;
            min[0] = min[0].min(x);
            min[1] = min[1].min(y);
            min[2] = min[2].min(z);
            max[0] = max[0].max(x);
            max[1] = max[1].max(y);
            max[2] = max[2].max(z);
        }
    }

    Ok(StlMeta {
        triangle_count,
        dim_x: if triangle_count > 0 {
            Some(max[0] - min[0])
        } else {
            None
        },
        dim_y: if triangle_count > 0 {
            Some(max[1] - min[1])
        } else {
            None
        },
        dim_z: if triangle_count > 0 {
            Some(max[2] - min[2])
        } else {
            None
        },
        name: None,
        header,
    })
}

fn extract_ascii(data: &[u8]) -> Result<StlMeta> {
    let text = std::str::from_utf8(data).unwrap_or("");
    let mut triangles: u64 = 0;
    let mut min = [f64::MAX; 3];
    let mut max = [f64::MIN; 3];
    let mut name: Option<String> = None;

    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if i == 0 && line.starts_with("solid") {
            let n = line[5..].trim().to_string();
            if !n.is_empty() {
                name = Some(n);
            }
        } else if line.starts_with("facet normal") {
            triangles += 1;
        } else if let Some(rest) = line.strip_prefix("vertex ") {
            let coords: Vec<f64> = rest
                .split_whitespace()
                .filter_map(|s| s.parse().ok())
                .collect();
            if coords.len() == 3 {
                min[0] = min[0].min(coords[0]);
                min[1] = min[1].min(coords[1]);
                min[2] = min[2].min(coords[2]);
                max[0] = max[0].max(coords[0]);
                max[1] = max[1].max(coords[1]);
                max[2] = max[2].max(coords[2]);
            }
        }
    }

    Ok(StlMeta {
        triangle_count: triangles,
        dim_x: if triangles > 0 {
            Some(max[0] - min[0])
        } else {
            None
        },
        dim_y: if triangles > 0 {
            Some(max[1] - min[1])
        } else {
            None
        },
        dim_z: if triangles > 0 {
            Some(max[2] - min[2])
        } else {
            None
        },
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
