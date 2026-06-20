use anyhow::{Context, Result};
use std::io::Read;
use std::path::Path;

pub struct ThreeMfMeta {
    pub title: Option<String>,
    pub designer: Option<String>,
    pub description: Option<String>,
    pub application: Option<String>,
    pub license: Option<String>,
    pub created_at: Option<String>,
    pub object_count: Option<usize>,
    pub triangle_count: Option<u64>,
    pub dim_x: Option<f64>,
    pub dim_y: Option<f64>,
    pub dim_z: Option<f64>,
    // raw PNG bytes of the best available thumbnail
    pub thumbnail: Option<Vec<u8>>,
}

pub fn extract(path: &Path) -> Result<ThreeMfMeta> {
    let file = std::fs::File::open(path)?;
    let mut archive = zip::ZipArchive::new(file).context("not a valid zip/3mf")?;

    let thumbnail = extract_thumbnail(&mut archive);
    let model_xml = read_model_xml(&mut archive)?;

    let doc = roxmltree::Document::parse(&model_xml).context("invalid XML in 3mf model")?;

    let mut meta = ThreeMfMeta {
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
        thumbnail,
    };

    for node in doc.descendants() {
        if node.tag_name().name() == "metadata" {
            let name = node.attribute("name").unwrap_or("").to_lowercase();
            let value = node.text().unwrap_or("").trim().to_string();
            if value.is_empty() || value == "[]" {
                continue;
            }
            match name.as_str() {
                "title" => meta.title = Some(clean_html(&value)),
                "designer" | "profileusername" if meta.designer.is_none() => {
                    meta.designer = Some(value)
                }
                "description" => meta.description = Some(clean_html(&value)),
                "application" => meta.application = Some(value),
                "licenseterms" | "license" => meta.license = Some(value),
                "creationdate" => meta.created_at = Some(value),
                _ => {}
            }
        }
    }

    let object_count = doc
        .descendants()
        .filter(|n| n.tag_name().name() == "object")
        .count();
    if object_count > 0 {
        meta.object_count = Some(object_count);
    }

    let mut triangles: u64 = 0;
    let mut min = [f64::MAX; 3];
    let mut max = [f64::MIN; 3];
    let mut has_verts = false;

    for node in doc.descendants() {
        match node.tag_name().name() {
            "triangle" => triangles += 1,
            "vertex" => {
                has_verts = true;
                let x: f64 = node
                    .attribute("x")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0.0);
                let y: f64 = node
                    .attribute("y")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0.0);
                let z: f64 = node
                    .attribute("z")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0.0);
                min[0] = min[0].min(x);
                min[1] = min[1].min(y);
                min[2] = min[2].min(z);
                max[0] = max[0].max(x);
                max[1] = max[1].max(y);
                max[2] = max[2].max(z);
            }
            _ => {}
        }
    }

    if triangles > 0 {
        meta.triangle_count = Some(triangles);
    }
    if has_verts {
        meta.dim_x = Some(max[0] - min[0]);
        meta.dim_y = Some(max[1] - min[1]);
        meta.dim_z = Some(max[2] - min[2]);
    }

    Ok(meta)
}

/// 3MF affine transform: 12 values (m00..m32, column-major 3×4 matrix).
/// Per spec: x' = m[0]*x + m[3]*y + m[6]*z + m[9]
#[derive(Clone)]
struct Transform([f32; 12]);

impl Transform {
    fn identity() -> Self {
        Self([1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0])
    }

    fn from_attr(s: &str) -> Option<Self> {
        let v: Vec<f32> = s
            .split_whitespace()
            .filter_map(|x| x.parse().ok())
            .collect();
        if v.len() == 12 {
            Some(Self(v.try_into().unwrap()))
        } else {
            None
        }
    }

    fn apply(&self, v: [f32; 3]) -> [f32; 3] {
        let [x, y, z] = v;
        let m = &self.0;
        [
            m[0] * x + m[3] * y + m[6] * z + m[9],
            m[1] * x + m[4] * y + m[7] * z + m[10],
            m[2] * x + m[5] * y + m[8] * z + m[11],
        ]
    }
}

/// Extract mesh geometry from a 3MF and return it as binary STL bytes.
///
/// Handles Bambu Studio's multi-file layout where geometry lives in
/// `3D/Objects/*.model` files referenced by `<component p:path="..." transform="..."/>`
/// elements. Transforms are applied so multi-object files appear correctly positioned
/// instead of all overlapping at the origin.
pub fn to_binary_stl(path: &Path) -> Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    let mut archive = zip::ZipArchive::new(file).context("not a valid zip/3mf")?;

    // Load every .model file in the ZIP into memory, keyed by normalised path
    let all_names: Vec<String> = archive.file_names().map(|s| s.to_string()).collect();
    let mut xml_by_path: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    for name in &all_names {
        if !name.to_lowercase().ends_with(".model") {
            continue;
        }
        let key = name.trim_start_matches('/').to_lowercase();
        if let Ok(mut entry) = archive.by_name(name) {
            let mut buf = String::new();
            if entry.read_to_string(&mut buf).is_ok() {
                xml_by_path.insert(key, buf);
            }
        }
    }

    // Locate the main model file
    let main_key = xml_by_path
        .keys()
        .find(|k| k.eq_ignore_ascii_case("3d/3dmodel.model"))
        .or_else(|| xml_by_path.keys().find(|k| k.ends_with(".model")))
        .cloned()
        .context("no .model file found in 3mf")?;

    let main_xml = xml_by_path.get(&main_key).unwrap().clone();
    let main_doc = roxmltree::Document::parse(&main_xml).context("invalid main model XML")?;

    let mut all_verts: Vec<[f32; 3]> = Vec::new();
    let mut all_tris: Vec<[u32; 3]> = Vec::new();

    // Collect per-object transforms from <build><item objectid transform> so that
    // multi-object standard 3MFs are positioned correctly rather than all overlapping.
    let mut build_transforms: std::collections::HashMap<String, Transform> =
        std::collections::HashMap::new();
    for node in main_doc.descendants() {
        if node.tag_name().name() == "item" {
            if let Some(oid) = node.attribute("objectid") {
                let xform = node
                    .attributes()
                    .find(|a| a.name() == "transform")
                    .and_then(|a| Transform::from_attr(a.value()))
                    .unwrap_or_else(Transform::identity);
                build_transforms.insert(oid.to_string(), xform);
            }
        }
    }

    // Walk every <object> in the main model
    for obj in main_doc
        .descendants()
        .filter(|n| n.tag_name().name() == "object")
    {
        let obj_type = obj.attribute("type").unwrap_or("model");
        if obj_type != "model" {
            continue;
        }

        let build_xform = obj
            .attribute("id")
            .and_then(|id| build_transforms.get(id))
            .cloned()
            .unwrap_or_else(Transform::identity);

        // Case A: Object has a direct <mesh> — simple single-file 3MF.
        if let Some(mesh) = obj.children().find(|n| n.tag_name().name() == "mesh") {
            add_mesh(
                mesh,
                &build_xform,
                &Transform::identity(),
                &mut all_verts,
                &mut all_tris,
            );
            continue;
        }

        // Case B: Object has <components> referencing external files (Bambu style).
        // Compose the build-level item transform (outer) with the component transform (inner).
        if let Some(comps) = obj.children().find(|n| n.tag_name().name() == "components") {
            for comp in comps
                .children()
                .filter(|n| n.tag_name().name() == "component")
            {
                let comp_xform = comp
                    .attributes()
                    .find(|a| a.name() == "transform")
                    .and_then(|a| Transform::from_attr(a.value()))
                    .unwrap_or_else(Transform::identity);

                let ext_path = comp
                    .attributes()
                    .find(|a| a.name() == "path")
                    .map(|a| a.value().trim_start_matches('/').to_lowercase());

                let want_id: Option<&str> = comp.attribute("objectid");

                if let Some(ep) = ext_path {
                    if let Some(ext_xml) = xml_by_path.get(&ep) {
                        if let Ok(ext_doc) = roxmltree::Document::parse(ext_xml) {
                            for ext_obj in ext_doc
                                .descendants()
                                .filter(|n| n.tag_name().name() == "object")
                            {
                                if let Some(wid) = want_id {
                                    if ext_obj.attribute("id") != Some(wid) {
                                        continue;
                                    }
                                }
                                let ot = ext_obj.attribute("type").unwrap_or("model");
                                if ot != "model" {
                                    continue;
                                }

                                if let Some(mesh) =
                                    ext_obj.children().find(|n| n.tag_name().name() == "mesh")
                                {
                                    add_mesh(
                                        mesh,
                                        &build_xform,
                                        &comp_xform,
                                        &mut all_verts,
                                        &mut all_tris,
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    if all_tris.is_empty() {
        anyhow::bail!("no mesh geometry found in 3MF");
    }

    Ok(write_binary_stl(&all_verts, &all_tris))
}

fn add_mesh(
    mesh: roxmltree::Node,
    outer: &Transform,
    inner: &Transform,
    all_verts: &mut Vec<[f32; 3]>,
    all_tris: &mut Vec<[u32; 3]>,
) {
    let base = all_verts.len() as u32;

    if let Some(verts_node) = mesh.children().find(|n| n.tag_name().name() == "vertices") {
        for v in verts_node
            .children()
            .filter(|n| n.tag_name().name() == "vertex")
        {
            let x = v
                .attribute("x")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.0f32);
            let y = v
                .attribute("y")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.0f32);
            let z = v
                .attribute("z")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.0f32);
            all_verts.push(outer.apply(inner.apply([x, y, z])));
        }
    }

    if let Some(tris_node) = mesh.children().find(|n| n.tag_name().name() == "triangles") {
        for t in tris_node
            .children()
            .filter(|n| n.tag_name().name() == "triangle")
        {
            let v1: u32 = t.attribute("v1").and_then(|s| s.parse().ok()).unwrap_or(0);
            let v2: u32 = t.attribute("v2").and_then(|s| s.parse().ok()).unwrap_or(0);
            let v3: u32 = t.attribute("v3").and_then(|s| s.parse().ok()).unwrap_or(0);
            all_tris.push([base + v1, base + v2, base + v3]);
        }
    }
}

fn write_binary_stl(verts: &[[f32; 3]], tris: &[[u32; 3]]) -> Vec<u8> {
    let mut buf = vec![0u8; 80]; // header
    let n = tris.len() as u32;
    buf.extend_from_slice(&n.to_le_bytes());
    for tri in tris {
        // Face normal — write zeros, renderer will compute it
        buf.extend_from_slice(&[0u8; 12]);
        for &vi in tri {
            let v = verts.get(vi as usize).copied().unwrap_or([0.0; 3]);
            for f in v {
                buf.extend_from_slice(&f.to_le_bytes());
            }
        }
        buf.extend_from_slice(&[0u8; 2]); // attribute byte count
    }
    buf
}

fn read_model_xml(archive: &mut zip::ZipArchive<std::fs::File>) -> Result<String> {
    // Try the standard path first, then scan for any .model file
    let names: Vec<String> = archive.file_names().map(|s| s.to_string()).collect();

    let model_path = names
        .iter()
        .find(|n| n.eq_ignore_ascii_case("3d/3dmodel.model"))
        .or_else(|| names.iter().find(|n| n.ends_with(".model")))
        .cloned()
        .context("no .model file found in 3mf")?;

    let mut entry = archive.by_name(&model_path)?;
    let mut buf = String::new();
    entry.read_to_string(&mut buf)?;
    Ok(buf)
}

fn extract_thumbnail(archive: &mut zip::ZipArchive<std::fs::File>) -> Option<Vec<u8>> {
    let names: Vec<String> = archive.file_names().map(|s| s.to_string()).collect();

    // Prefer the plate render, fall back to smaller thumbnails
    let preferred = [
        "Metadata/plate_1.png",
        "Auxiliaries/.thumbnails/thumbnail_middle.png",
        "Auxiliaries/.thumbnails/thumbnail_3mf.png",
        "Auxiliaries/.thumbnails/thumbnail_small.png",
    ];

    for candidate in &preferred {
        if let Some(name) = names.iter().find(|n| n.eq_ignore_ascii_case(candidate)) {
            if let Ok(mut entry) = archive.by_name(name) {
                let mut buf = Vec::new();
                if entry.read_to_end(&mut buf).is_ok() && !buf.is_empty() {
                    return Some(buf);
                }
            }
        }
    }

    // Last resort: any png under Metadata/ or Auxiliaries/
    for name in &names {
        let lower = name.to_lowercase();
        if lower.ends_with(".png") && (lower.contains("metadata") || lower.contains("auxiliaries"))
        {
            if let Ok(mut entry) = archive.by_name(name) {
                let mut buf = Vec::new();
                if entry.read_to_end(&mut buf).is_ok() && !buf.is_empty() {
                    return Some(buf);
                }
            }
        }
    }

    None
}

// Strip HTML tags and decode common entities from Makerworld descriptions
fn clean_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    static ID: AtomicU64 = AtomicU64::new(0);

    fn tmp_path(suffix: &str) -> std::path::PathBuf {
        let id = ID.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("org3d_3mf_{id}_{suffix}"))
    }

    fn make_3mf(metadata: &[(&str, &str)], include_thumbnail: bool) -> Vec<u8> {
        let cursor = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(cursor);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        zip.start_file("3D/3dmodel.model", opts).unwrap();
        let mut xml = String::from(
            r#"<?xml version="1.0"?><model xmlns="http://schemas.microsoft.com/3dmanufacturing/core/2015/02"><resources><object id="1" type="model"><mesh><vertices><vertex x="0" y="0" z="0"/><vertex x="10" y="0" z="0"/><vertex x="0" y="20" z="0"/><vertex x="0" y="0" z="5"/></vertices><triangles><triangle v1="0" v2="1" v3="2"/><triangle v1="0" v2="1" v3="3"/></triangles></mesh></object></resources>"#,
        );
        for (name, value) in metadata {
            xml.push_str(&format!("<metadata name=\"{name}\">{value}</metadata>"));
        }
        xml.push_str("</model>");
        zip.write_all(xml.as_bytes()).unwrap();

        if include_thumbnail {
            // Minimal 1×1 PNG (valid PNG header + IHDR + IDAT + IEND)
            let png: &[u8] = &[
                0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, // PNG sig
                0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, // IHDR len + type
                0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, // 1×1
                0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53, // 8bit RGB
                0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, // IDAT
                0x54, 0x08, 0xd7, 0x63, 0xf8, 0xcf, 0xc0, 0x00, 0x00, 0x00, 0x02, 0x00, 0x01, 0xe2,
                0x21, 0xbc, 0x33, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, // IEND
                0x44, 0xae, 0x42, 0x60, 0x82,
            ];
            zip.start_file("Metadata/plate_1.png", opts).unwrap();
            zip.write_all(png).unwrap();
        }

        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn test_metadata_extraction() {
        let data = make_3mf(
            &[
                ("Title", "Test Dragon"),
                ("Designer", "TestUser"),
                ("Application", "BambuStudio-01.10.01"),
                ("CreationDate", "2025-01-15"),
                ("LicenseTerms", "CC BY"),
            ],
            false,
        );
        let path = tmp_path("meta.3mf");
        std::fs::write(&path, &data).unwrap();
        let meta = extract(&path).unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(meta.title.as_deref(), Some("Test Dragon"));
        assert_eq!(meta.designer.as_deref(), Some("TestUser"));
        assert_eq!(meta.application.as_deref(), Some("BambuStudio-01.10.01"));
        assert_eq!(meta.created_at.as_deref(), Some("2025-01-15"));
        assert_eq!(meta.license.as_deref(), Some("CC BY"));
    }

    #[test]
    fn test_geometry_extraction() {
        let data = make_3mf(&[], false);
        let path = tmp_path("geo.3mf");
        std::fs::write(&path, &data).unwrap();
        let meta = extract(&path).unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(meta.triangle_count, Some(2));
        assert_eq!(meta.object_count, Some(1));
        assert!((meta.dim_x.unwrap() - 10.0).abs() < 0.001);
        assert!((meta.dim_y.unwrap() - 20.0).abs() < 0.001);
        assert!((meta.dim_z.unwrap() - 5.0).abs() < 0.001);
    }

    #[test]
    fn test_thumbnail_extracted() {
        let data = make_3mf(&[], true);
        let path = tmp_path("thumb.3mf");
        std::fs::write(&path, &data).unwrap();
        let meta = extract(&path).unwrap();
        std::fs::remove_file(&path).ok();

        let thumb = meta.thumbnail.expect("thumbnail should be present");
        assert!(
            thumb.starts_with(&[0x89, 0x50, 0x4e, 0x47]),
            "should start with PNG magic"
        );
    }

    #[test]
    fn test_no_thumbnail_returns_none() {
        let data = make_3mf(&[], false);
        let path = tmp_path("nothumb.3mf");
        std::fs::write(&path, &data).unwrap();
        let meta = extract(&path).unwrap();
        std::fs::remove_file(&path).ok();

        assert!(meta.thumbnail.is_none());
    }

    #[test]
    fn test_html_stripped_from_description() {
        let data = make_3mf(
            // Real Makerworld files HTML-encode the markup inside the XML value
            &[(
                "Description",
                "&lt;p&gt;Hello &lt;strong&gt;world&lt;/strong&gt;&lt;/p&gt;",
            )],
            false,
        );
        let path = tmp_path("html.3mf");
        std::fs::write(&path, &data).unwrap();
        let meta = extract(&path).unwrap();
        std::fs::remove_file(&path).ok();

        let desc = meta.description.unwrap();
        assert!(!desc.contains('<'), "HTML tags should be stripped");
        assert!(desc.contains("Hello"), "text content should remain");
    }

    #[test]
    fn test_empty_metadata_ignored() {
        let data = make_3mf(&[("Title", ""), ("Designer", "[]")], false);
        let path = tmp_path("empty.3mf");
        std::fs::write(&path, &data).unwrap();
        let meta = extract(&path).unwrap();
        std::fs::remove_file(&path).ok();

        assert!(meta.title.is_none(), "empty title should not be stored");
        assert!(meta.designer.is_none(), "[] designer should not be stored");
    }

    // ── to_binary_stl tests ──────────────────────────────────────────────────

    fn make_obj_xml_id(id: u32, v_x: f32, v_y: f32, v_z: f32) -> String {
        format!(
            r#"<?xml version="1.0"?><model xmlns="http://schemas.microsoft.com/3dmanufacturing/core/2015/02"><resources><object id="{id}" type="model"><mesh><vertices><vertex x="0" y="0" z="0"/><vertex x="{v_x}" y="0" z="0"/><vertex x="0" y="{v_y}" z="0"/><vertex x="0" y="0" z="{v_z}"/></vertices><triangles><triangle v1="0" v2="1" v3="2"/><triangle v1="0" v2="1" v3="3"/></triangles></mesh></object></resources></model>"#
        )
    }

    fn make_obj_xml(v_x: f32, v_y: f32, v_z: f32) -> String {
        make_obj_xml_id(1, v_x, v_y, v_z)
    }

    /// Build a Bambu-style 3MF where the main model only has an assembly object
    /// and geometry lives in 3D/Objects/. Each entry is (objectid, transform_str, xml).
    fn make_bambu_3mf(components: &[(u32, &str, &str)]) -> Vec<u8> {
        let cursor = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(cursor);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        // Build main model with component references
        let mut comp_xml = String::new();
        for (oid, xform, _) in components {
            comp_xml.push_str(&format!(
                r#"<component p:path="/3D/Objects/object_{oid}.model" objectid="{oid}" transform="{xform}"/>"#
            ));
        }
        let main = format!(
            r#"<?xml version="1.0"?><model xmlns="http://schemas.microsoft.com/3dmanufacturing/core/2015/02" xmlns:p="http://schemas.microsoft.com/3dmanufacturing/production/2015/06"><resources><object id="99" type="model"><components>{comp_xml}</components></object></resources><build><item objectid="99"/></build></model>"#
        );
        zip.start_file("3D/3dmodel.model", opts).unwrap();
        zip.write_all(main.as_bytes()).unwrap();

        for (oid, _, xml) in components {
            zip.start_file(format!("3D/Objects/object_{oid}.model"), opts)
                .unwrap();
            zip.write_all(xml.as_bytes()).unwrap();
        }

        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn test_stl_from_simple_3mf() {
        // Standard single-file 3MF with mesh in main model
        let data = make_3mf(&[], false);
        let path = tmp_path("stl_simple.3mf");
        std::fs::write(&path, &data).unwrap();
        let stl = to_binary_stl(&path).unwrap();
        std::fs::remove_file(&path).ok();

        assert!(stl.len() > 84, "STL should contain geometry");
        let tri_count = u32::from_le_bytes(stl[80..84].try_into().unwrap());
        assert_eq!(tri_count, 2, "should have 2 triangles");
    }

    #[test]
    fn test_stl_from_bambu_style_3mf() {
        let obj_xml = make_obj_xml(10.0, 20.0, 5.0);
        let data = make_bambu_3mf(&[(1, "1 0 0 0 1 0 0 0 1 0 0 0", &obj_xml)]);
        let path = tmp_path("stl_bambu.3mf");
        std::fs::write(&path, &data).unwrap();
        let stl = to_binary_stl(&path).unwrap();
        std::fs::remove_file(&path).ok();

        let tri_count = u32::from_le_bytes(stl[80..84].try_into().unwrap());
        assert_eq!(tri_count, 2, "should have 2 triangles from external object");
    }

    #[test]
    fn test_stl_transform_applied() {
        // Two instances of the same shape: one at identity, one translated by (100, 0, 0)
        // The second triangle's vertices should have x-coords around 100.
        let obj_xml = make_obj_xml(10.0, 10.0, 10.0);
        // identity transform for instance 1, translate +100 on X for instance 2
        let obj1 = make_obj_xml_id(1, 10.0, 10.0, 10.0);
        let obj2 = make_obj_xml_id(2, 10.0, 10.0, 10.0);
        let data = make_bambu_3mf(&[
            (1, "1 0 0 0 1 0 0 0 1 0 0 0", &obj1),
            (2, "1 0 0 0 1 0 0 0 1 100 0 0", &obj2),
        ]);
        let path = tmp_path("stl_transform.3mf");
        std::fs::write(&path, &data).unwrap();
        let stl = to_binary_stl(&path).unwrap();
        std::fs::remove_file(&path).ok();

        let tri_count = u32::from_le_bytes(stl[80..84].try_into().unwrap());
        assert_eq!(tri_count, 4, "should have 2 triangles × 2 instances");

        // Scan all vertices to find the max X — should be ~110 (100 offset + 10 size)
        let mut max_x = 0.0f32;
        for i in 0..tri_count as usize {
            let base = 84 + i * 50 + 12;
            for v in 0..3usize {
                let off = base + v * 12;
                let x = f32::from_le_bytes(stl[off..off + 4].try_into().unwrap());
                max_x = max_x.max(x);
            }
        }
        assert!(
            max_x > 100.0,
            "translated instance should have x > 100, got {max_x}"
        );
    }

    #[test]
    fn test_stl_no_duplicate_without_transform() {
        // Two components with the same geometry at the same position (no transform)
        // should produce 2×triangles, both overlapping — but we don't merge duplicates.
        let obj1 = make_obj_xml_id(1, 5.0, 5.0, 5.0);
        let obj2 = make_obj_xml_id(2, 5.0, 5.0, 5.0);
        let data = make_bambu_3mf(&[
            (1, "1 0 0 0 1 0 0 0 1 0 0 0", &obj1),
            (2, "1 0 0 0 1 0 0 0 1 0 0 0", &obj2),
        ]);
        let path = tmp_path("stl_nodupe.3mf");
        std::fs::write(&path, &data).unwrap();
        let stl = to_binary_stl(&path).unwrap();
        std::fs::remove_file(&path).ok();

        let tri_count = u32::from_le_bytes(stl[80..84].try_into().unwrap());
        assert_eq!(
            tri_count, 4,
            "each component instance should add its own triangles"
        );
    }

    #[test]
    fn test_stl_build_transform_positions_objects() {
        // Real-world Bambu pattern: each printable object is a separate top-level entry
        // in <build> with its own transform; component transforms are identity.
        // Without the build-transform fix, both objects land at the origin and overlap.
        let identity = "1 0 0 0 1 0 0 0 1 0 0 0";
        let translate_100 = "1 0 0 0 1 0 0 0 1 100 0 0";

        let obj1_xml = make_obj_xml_id(1, 10.0, 10.0, 10.0);
        let obj2_xml = make_obj_xml_id(2, 10.0, 10.0, 10.0);

        // Build a 3MF where the two objects have identity component transforms,
        // but different build-level item transforms.
        let cursor = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(cursor);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        let main = format!(
            r#"<?xml version="1.0"?><model xmlns="http://schemas.microsoft.com/3dmanufacturing/core/2015/02" xmlns:p="http://schemas.microsoft.com/3dmanufacturing/production/2015/06"><resources><object id="2" type="model"><components><component p:path="/3D/Objects/object_1.model" objectid="1" transform="{identity}"/></components></object><object id="4" type="model"><components><component p:path="/3D/Objects/object_2.model" objectid="2" transform="{identity}"/></components></object></resources><build><item objectid="2" transform="{identity}"/><item objectid="4" transform="{translate_100}"/></build></model>"#
        );
        zip.start_file("3D/3dmodel.model", opts).unwrap();
        zip.write_all(main.as_bytes()).unwrap();
        zip.start_file("3D/Objects/object_1.model", opts).unwrap();
        zip.write_all(obj1_xml.as_bytes()).unwrap();
        zip.start_file("3D/Objects/object_2.model", opts).unwrap();
        zip.write_all(obj2_xml.as_bytes()).unwrap();
        let data = zip.finish().unwrap().into_inner();

        let path = tmp_path("stl_build_xform.3mf");
        std::fs::write(&path, &data).unwrap();
        let stl = to_binary_stl(&path).unwrap();
        std::fs::remove_file(&path).ok();

        let tri_count = u32::from_le_bytes(stl[80..84].try_into().unwrap());
        assert_eq!(tri_count, 4);

        let mut max_x = 0.0f32;
        for i in 0..tri_count as usize {
            let base = 84 + i * 50 + 12;
            for v in 0..3usize {
                let off = base + v * 12;
                let x = f32::from_le_bytes(stl[off..off + 4].try_into().unwrap());
                max_x = max_x.max(x);
            }
        }
        assert!(
            max_x > 100.0,
            "build-translated object should have x > 100, got {max_x}"
        );
    }

    #[test]
    fn test_stl_empty_returns_error() {
        // 3MF with only component references and no accessible geometry
        let cursor = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(cursor);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file("3D/3dmodel.model", opts).unwrap();
        zip.write_all(b"<?xml version=\"1.0\"?><model xmlns=\"http://schemas.microsoft.com/3dmanufacturing/core/2015/02\"><resources><object id=\"1\" type=\"model\"><components/></object></resources></model>").unwrap();
        let data = zip.finish().unwrap().into_inner();

        let path = tmp_path("stl_empty.3mf");
        std::fs::write(&path, &data).unwrap();
        let result = to_binary_stl(&path);
        std::fs::remove_file(&path).ok();

        assert!(result.is_err(), "should error when no geometry found");
    }

    #[test]
    fn test_stl_bounding_box() {
        let obj_xml = make_obj_xml(50.0, 30.0, 15.0);
        let data = make_bambu_3mf(&[(1, "1 0 0 0 1 0 0 0 1 0 0 0", &obj_xml)]);
        let path = tmp_path("stl_bbox.3mf");
        std::fs::write(&path, &data).unwrap();
        let stl = to_binary_stl(&path).unwrap();
        std::fs::remove_file(&path).ok();

        // Scan all vertices in the binary STL to find bounding box
        let tri_count = u32::from_le_bytes(stl[80..84].try_into().unwrap()) as usize;
        let mut max_x = 0.0f32;
        let mut max_y = 0.0f32;
        let mut max_z = 0.0f32;
        for i in 0..tri_count {
            let base = 84 + i * 50 + 12; // skip normal
            for v in 0..3usize {
                let off = base + v * 12;
                let x = f32::from_le_bytes(stl[off..off + 4].try_into().unwrap());
                let y = f32::from_le_bytes(stl[off + 4..off + 8].try_into().unwrap());
                let z = f32::from_le_bytes(stl[off + 8..off + 12].try_into().unwrap());
                max_x = max_x.max(x);
                max_y = max_y.max(y);
                max_z = max_z.max(z);
            }
        }
        assert!((max_x - 50.0).abs() < 0.001, "max x should be 50");
        assert!((max_y - 30.0).abs() < 0.001, "max y should be 30");
        assert!((max_z - 15.0).abs() < 0.001, "max z should be 15");
    }
}
