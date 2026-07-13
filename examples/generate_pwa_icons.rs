use image::imageops::{overlay, FilterType};
use image::{Rgba, RgbaImage};
use std::error::Error;
use std::path::Path;

fn main() -> Result<(), Box<dyn Error>> {
    let source = image::open("packaging/icons/devmanager-512.png")?;
    let output = Path::new("web/public/icons");
    std::fs::create_dir_all(output)?;

    for (size, filename) in [
        (180, "devmanager-180.png"),
        (192, "devmanager-192.png"),
        (512, "devmanager-512.png"),
    ] {
        source
            .resize_exact(size, size, FilterType::Lanczos3)
            .save(output.join(filename))?;
    }

    let foreground = source
        .resize_exact(410, 410, FilterType::Lanczos3)
        .to_rgba8();
    let mut maskable = RgbaImage::from_pixel(512, 512, Rgba([9, 9, 11, 255]));
    overlay(&mut maskable, &foreground, 51, 51);
    maskable.save(output.join("devmanager-maskable-512.png"))?;

    Ok(())
}
