//! Regenerate PNG/ICO assets: cargo run -p nbq-app --example generate_icon
use std::{fs, path::Path};
use resvg::{tiny_skia, usvg};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
    let tree = usvg::Tree::from_data(&fs::read(assets.join("app.svg"))?, &usvg::Options::default())?;
    let sizes = [16u32, 20, 24, 32, 40, 48, 64, 128, 256];
    let mut images = Vec::new();
    for size in sizes {
        let mut pixmap = tiny_skia::Pixmap::new(size, size).ok_or("Invalid icon size")?;
        let scale = size as f32 / tree.size().width();
        resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
        let png = pixmap.encode_png()?;
        if size == 64 || size == 256 {
            fs::write(assets.join(format!("app-{size}.png")), &png)?;
        }
        images.push(png);
    }
    // Windows Vista+ supports lossless PNG frames in an ICO directory.
    let mut ico = Vec::new();
    ico.extend_from_slice(&[0, 0, 1, 0]);
    ico.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = (6 + 16 * sizes.len()) as u32;
    for (size, png) in sizes.into_iter().zip(&images) {
        let dimension = if size == 256 { 0 } else { size as u8 };
        ico.extend_from_slice(&[dimension, dimension, 0, 0]);
        ico.extend_from_slice(&1u16.to_le_bytes());
        ico.extend_from_slice(&32u16.to_le_bytes());
        ico.extend_from_slice(&(png.len() as u32).to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset += png.len() as u32;
    }
    for png in images { ico.extend_from_slice(&png); }
    fs::write(assets.join("app.ico"), ico)?;
    println!("Generated 64/256px PNGs and a 9-size Windows icon in {}", assets.display());
    Ok(())
}
