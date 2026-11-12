//! QR codes in posted images: Discord login codes and scam links that text filters cannot see.
use crate::app::App;
use twilight_model::channel::Attachment;

const MAX_BYTES: usize = 8 << 20;
const MAX_IMAGES: usize = 3;
/// Bigger images are scaled down first; a QR code meant to be scanned stays readable.
const MAX_SIDE: u32 = 1024;

/// The text of every QR code in the first few image attachments, one per line.
pub async fn texts(app: &App, attachments: &[Attachment]) -> String {
    let mut out = String::new();
    let images = attachments.iter().filter(|a| {
        a.size <= MAX_BYTES as u64
            && a.content_type
                .as_deref()
                .is_some_and(|t| t.starts_with("image/"))
    });
    for a in images.take(MAX_IMAGES) {
        let Some(bytes) = app.sites.fetch(&a.url, MAX_BYTES).await else {
            continue;
        };
        if let Ok(found) = tokio::task::spawn_blocking(move || decode(&bytes)).await {
            for t in found {
                out.push_str(&t);
                out.push('\n');
            }
        }
    }
    out
}

/// Decodes an untrusted image (the first frame of an animation). A tiny file can claim a huge size;
/// the limits stop it before it allocates.
pub(crate) fn read_image(bytes: &[u8]) -> Option<image::DynamicImage> {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(128 << 20);
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    reader.limits(limits);
    reader.decode().ok()
}

fn decode(bytes: &[u8]) -> Vec<String> {
    let Some(img) = read_image(bytes) else {
        return vec![];
    };
    let img = if img.width().max(img.height()) > MAX_SIDE {
        img.thumbnail(MAX_SIDE, MAX_SIDE)
    } else {
        img
    };
    let g = img.to_luma8();
    let (w, h) = (g.width() as usize, g.height() as usize);
    let mut prepared = rqrr::PreparedImage::prepare_from_greyscale(w, h, |x, y| {
        g.get_pixel(x as u32, y as u32).0[0]
    });
    prepared
        .detect_grids()
        .iter()
        .filter_map(|grid| grid.decode().ok().map(|(_, text)| text))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn qr_png(text: &str, scale: u32) -> Vec<u8> {
        let code = qrcode::QrCode::new(text).unwrap();
        let n = code.width() as u32;
        let quiet = 4;
        let side = (n + 2 * quiet) * scale;
        let colors = code.to_colors();
        let img = image::GrayImage::from_fn(side, side, |x, y| {
            let (cx, cy) = (x / scale, y / scale);
            let dark = cx >= quiet
                && cy >= quiet
                && cx < n + quiet
                && cy < n + quiet
                && colors[((cy - quiet) * n + cx - quiet) as usize] == qrcode::Color::Dark;
            image::Luma([if dark { 0 } else { 255 }])
        });
        let mut out = std::io::Cursor::new(vec![]);
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn reads_a_login_qr_code_also_from_a_big_image() {
        let url = "https://discord.com/ra/abcdefGHIJ123";
        assert_eq!(decode(&qr_png(url, 8)), [url]);
        assert_eq!(decode(&qr_png(url, 60)), [url]);
        assert!(decode(b"not an image").is_empty());
    }
}
