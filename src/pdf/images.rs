use lopdf::{Document, Object, Stream};
use image::DynamicImage;
use image::codecs::jpeg::JpegEncoder;

/// Replace JPEG images in a document by a compressed version to the given quality.
/// Only JPEG images are replaced, the other are skipped.
pub fn compress_images(doc: &mut Document, quality: u8, verbose: bool) {
    let image_ids: Vec<lopdf::ObjectId> = doc.objects.iter()
        .filter_map(|(id, obj)| match obj {
            Object::Stream(stream) if is_image_stream(stream) => Some(*id),
            _ => None,
        })
        .collect();

    if verbose {
        eprintln!("[images] Found {} image object(s)", image_ids.len());
    }

    for id in image_ids {
        if let Ok(Object::Stream(stream)) = doc.get_object_mut(id) {
            let color_space_raw = stream.dict.get(b"ColorSpace")
                .and_then(|f| f.as_name())
                .ok();
            let width = stream.dict.get(b"Width").and_then(|w| w.as_i64()).unwrap_or(0) as u32;
            let height = stream.dict.get(b"Height").and_then(|h| h.as_i64()).unwrap_or(0) as u32;

            // Detect filter: may be a Name or an Array
            let filter_name = stream.dict.get(b"Filter").ok();
            let filter_str = filter_name.map(|f| match f {
                Object::Name(n) => String::from_utf8_lossy(n).into_owned(),
                Object::Array(arr) => {
                    let names: Vec<String> = arr.iter()
                        .filter_map(|e| e.as_name().ok())
                        .map(|n| String::from_utf8_lossy(n).into_owned())
                        .collect();
                    format!("[{}]", names.join(", "))
                }
                _ => format!("{:?}", f),
            });
            let color_str = color_space_raw
                .map(|c| String::from_utf8_lossy(c).into_owned())
                .unwrap_or_else(|| "unknown".to_string());

            if verbose {
                eprintln!(
                    "[img {:?}] filter={} colorspace={} size={}x{} raw_content={}B",
                    id,
                    filter_str.as_deref().unwrap_or("none"),
                    color_str,
                    width, height,
                    stream.content.len()
                );
            }

            // CMYK images not yet supported by image crate
            if color_space_raw == Some(b"DeviceCMYK") {
                if verbose {
                    eprintln!("[img {:?}] → skipped: CMYK not supported", id);
                }
                continue;
            }

            // Resolve the effective filter. PDF allows Filter to be a Name or a
            // single-element Array — both mean the same thing. A multi-element Array
            // is a filter pipeline (e.g. [FlateDecode, DCTDecode]) which we skip.
            let filter: Option<&[u8]> = match stream.dict.get(b"Filter").ok() {
                Some(Object::Name(n)) => Some(n.as_slice()),
                Some(Object::Array(arr)) if arr.len() == 1 => {
                    arr[0].as_name().ok()
                }
                Some(Object::Array(arr)) => {
                    if verbose {
                        let names: Vec<String> = arr.iter()
                            .filter_map(|e| e.as_name().ok())
                            .map(|n| String::from_utf8_lossy(n).into_owned())
                            .collect();
                        eprintln!("[img {:?}] → skipped: multi-filter pipeline [{}] not supported", id, names.join(", "));
                    }
                    continue;
                }
                _ => None,
            };

            let mut buf: Vec<u8> = Vec::new();
            let cursor = std::io::Cursor::new(&mut buf);

            match filter {
                Some(b"DCTDecode") | Some(b"JPXDecode") => {
                    if verbose {
                        eprintln!("[img {:?}] → processing JPEG/JPX via image::load_from_memory", id);
                    }
                    let img = match image::load_from_memory(&stream.content) {
                        Ok(data) => data,
                        Err(e) => {
                            if verbose {
                                eprintln!("[img {:?}] → skipped: load_from_memory failed: {}", id, e);
                            }
                            continue;
                        }
                    };
                    let _ = JpegEncoder::new_with_quality(cursor, quality).encode_image(&img);
                }
                Some(other_filter) => {
                    if verbose {
                        eprintln!(
                            "[img {:?}] → processing raw pixels (filter={}, colorspace={})",
                            id,
                            String::from_utf8_lossy(other_filter),
                            color_str
                        );
                    }
                    let raw = match stream.decompressed_content() {
                        Ok(data) => data,
                        Err(e) => {
                            if verbose {
                                eprintln!("[img {:?}] → skipped: decompressed_content failed: {}", id, e);
                            }
                            continue;
                        }
                    };

                    let img = if color_space_raw == Some(b"DeviceGray") {
                        image::GrayImage::from_raw(width, height, raw).map(DynamicImage::ImageLuma8)
                    } else {
                        image::RgbImage::from_raw(width, height, raw).map(DynamicImage::ImageRgb8)
                    };

                    let img = match img {
                        Some(i) => i,
                        None => {
                            if verbose {
                                eprintln!(
                                    "[img {:?}] → skipped: from_raw failed (mismatched dimensions or unsupported format, expected {}x{}x{} bytes)",
                                    id, width, height,
                                    if color_space_raw == Some(b"DeviceGray") { width * height } else { width * height * 3 }
                                );
                            }
                            continue;
                        }
                    };
                    let _ = JpegEncoder::new_with_quality(cursor, quality).encode_image(&img);
                }
                None => {
                    if verbose {
                        eprintln!("[img {:?}] → skipped: no Filter entry (uncompressed stream)", id);
                    }
                    continue;
                }
            }

            if buf.is_empty() {
                if verbose {
                    eprintln!("[img {:?}] → skipped: JPEG encoding produced empty output", id);
                }
                continue;
            }

            if buf.len() < stream.content.len() {
                if verbose {
                    eprintln!("[img {:?}] → compressed {}B → {}B", id, stream.content.len(), buf.len());
                }
                stream.content = buf;
                stream.dict.set(b"Filter", Object::Name(b"DCTDecode".to_vec()));
                stream.dict.set(b"Length", Object::Integer(stream.content.len() as i64));
            } else {
                if verbose {
                    eprintln!(
                        "[img {:?}] → skipped: re-encoded ({}B) not smaller than original ({}B)",
                        id, buf.len(), stream.content.len()
                    );
                }
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
