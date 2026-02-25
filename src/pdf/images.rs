use lopdf::{Document, Object, Stream};
use image::DynamicImage;
use image::codecs::jpeg::{JpegEncoder};


/// Replace JPEG images in a document by a compressed version to the given quality.
/// Only JPEG images are replaced, the other are skipped.
pub fn compress_images(doc: &mut Document, quality: u8){
    let image_ids: Vec<lopdf::ObjectId> = doc.objects.iter()
    .filter_map(|(id, obj)| match obj {
        // Filter images in the document
        Object::Stream(stream) if is_image_stream(stream) => Some(*id),
        _ => None,
    })
    .collect();

    // eprintln!("Found {} image(s)", image_ids.len());

    for id in image_ids {
        if let Ok(Object::Stream(stream)) = doc.get_object_mut(id) {
            // Compress + replace image
            // Multiple cases depending on the images's filter

            // Skip CMYK as it is not handled by image crate
            let color_space = stream.dict.get(b"ColorSpace")
            .and_then(|f| f.as_name())
            .ok();
            if color_space == Some(b"DeviceCMYK") {
                eprintln!("Image {:?} is CMYK, skipping", id);
                continue
            }
            
            // Different encoding based on image's filter type
            let filter = stream.dict.get(b"Filter")
                .and_then(|f| f.as_name())
                .ok();

            let mut buf: Vec<u8> = Vec::new();
            let cursor = std::io::Cursor::new(&mut buf);

            match filter {
                Some(b"DCTDecode") | Some(b"JPXDecode") => {
                    let img= match image::load_from_memory(&stream.content) {
                        Ok(data) => data,
                        Err(e) => { eprintln!("Skipping image {:?}, {}", id, e); continue }
                    };

                    // stream.content is already JPEG — re-encode at lower quality
                    if let Err(e) = JpegEncoder::new_with_quality(cursor, quality)
                    .encode_image(&img){
                        eprintln!("Failed to encode image {:?} {}", id, e)
                    };
                }
                _ => {
                    let width = stream.dict.get(b"Width").and_then(|w| w.as_i64()).unwrap_or(0) as u32;
                    let height = stream.dict.get(b"Height").and_then(|h| h.as_i64()).unwrap_or(0) as u32;

                    let raw = match stream.decompressed_content() {
                            Ok(data) => data,
                            Err(e) => { eprintln!("Skipping image {:?}, {}", id, e); continue }
                    };
                    
                    // decompressed_content() → raw pixels → build ImageBuffer → encode to JPEG
                    // Different encoding for RBG vs Gray scale images
                    let img = if color_space == Some(b"DeviceGray") {
                        image::GrayImage::from_raw(width, height, raw)
                        .map(DynamicImage::ImageLuma8)
                    } else {
                        image::RgbImage::from_raw(width, height, raw)
                        .map(DynamicImage::ImageRgb8)
                    };

                    let img = match img {
                        Some(i) => i,
                        None => { eprintln!("Invalid Dimension for iamge {:?}", id); continue },
                    };
                    if let Err(e) = JpegEncoder::new_with_quality(cursor, quality)
                    .encode_image(&img){
                        eprintln!("Failed to encode image {:?} {}", id, e)
                    };
                }
            }
            // eprintln!("Image {:?}: {} bytes → {} bytes", id, stream.content.len(), buf.len());
            // Update the stream object with the new image
            if buf.len() < stream.content.len() {
            stream.content = buf;
            stream.dict.set(b"Filter", Object::Name(b"DCTDecode".to_vec()));
            stream.dict.set(b"Length", Object::Integer(stream.content.len() as i64));
            }

        }
    }
}

/// Check if a stream represents an image.
fn is_image_stream(stream: &Stream) -> bool {
    stream.dict.get(b"Subtype")
    .and_then(|s| s.as_name())
    .ok()
    .map_or(false, |name| name == b"Image")
}
