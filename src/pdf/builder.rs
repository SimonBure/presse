use std::path::Path;
use lopdf::{Document, Object, Stream, dictionary};
use lopdf::content::{Content, Operation};

/// Build a single-page PDF whose only content is `path`'s image, full-bleed.
/// Decodes via the `image` crate (any supported format) and embeds it as a
/// FlateDecode DeviceRGB Image XObject (1px = 1pt page). If the source has an
/// alpha channel, the alpha is preserved losslessly as a DeviceGray `/SMask`.
pub fn image_to_pdf(path: &Path, verbose: bool) -> Result<Document, Box<dyn std::error::Error>> {
    let dynimg = image::ImageReader::open(path)?
        .with_guessed_format()? // sniff magic bytes instead of trusting the extension
        .decode()?;
    let (w, h) = (dynimg.width(), dynimg.height());

    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id(); // reserve so Page.Parent can point at it

    // Split colour / alpha. has_alpha() avoids paying for an all-opaque SMask.
    let (rgb, alpha): (Vec<u8>, Option<Vec<u8>>) = if dynimg.color().has_alpha() {
        let rgba = dynimg.to_rgba8();
        let mut rgb = Vec::with_capacity((w * h * 3) as usize);
        let mut a = Vec::with_capacity((w * h) as usize);
        for px in rgba.pixels() {
            rgb.extend_from_slice(&[px[0], px[1], px[2]]);
            a.push(px[3]);
        }
        (rgb, Some(a))
    } else {
        (dynimg.to_rgb8().into_raw(), None)
    };

    let mut image_dict = dictionary! {
        "Type" => "XObject",
        "Subtype" => "Image",
        "Width" => w as i64,
        "Height" => h as i64,
        "ColorSpace" => "DeviceRGB",
        "BitsPerComponent" => 8,
    };

    // Alpha → DeviceGray soft mask, referenced from the colour image's /SMask.
    if let Some(a) = alpha {
        let mut smask = Stream::new(dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => w as i64,
            "Height" => h as i64,
            "ColorSpace" => "DeviceGray",
            "BitsPerComponent" => 8,
        }, a);
        smask.compress()?;
        let smask_id = doc.add_object(smask);
        image_dict.set("SMask", smask_id);
    }

    let mut image_stream = Stream::new(image_dict, rgb);
    image_stream.compress()?; // -> Filter FlateDecode (so compress_images sees it)
    let image_id = doc.add_object(image_stream);

    let content = Content { operations: vec![
        Operation::new("q", vec![]),
        Operation::new("cm", vec![w.into(), 0.into(), 0.into(), h.into(), 0.into(), 0.into()]),
        Operation::new("Do", vec![Object::Name(b"Im0".to_vec())]),
        Operation::new("Q", vec![]),
    ]};
    let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode()?));

    // MediaBox + Resources live ON THE PAGE (not inherited from Pages) so merge() preserves them.
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), w.into(), h.into()],
        "Contents" => content_id,
        "Resources" => dictionary! { "XObject" => dictionary! { "Im0" => image_id } },
    });

    doc.objects.insert(pages_id, Object::Dictionary(dictionary! {
        "Type" => "Pages",
        "Kids" => vec![page_id.into()],
        "Count" => 1,
    }));
    let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog_id);

    verbose!(verbose, "[convert] {} -> {}x{} page", path.display(), w, h);
    Ok(doc)
}
