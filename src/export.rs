use egui::Vec2;
use geo::{Euclidean, LineString, line_measures::LengthMeasurable};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ToolpathStats {
    pub paths: usize,
    pub nodes: usize,
    pub cut_length: f32,
    pub travel_length: f32,
}

pub fn toolpath_stats(paths: &[LineString<f32>]) -> ToolpathStats {
    let mut stats = ToolpathStats {
        paths: paths.len(),
        nodes: paths.iter().map(|path| path.0.len()).sum(),
        cut_length: paths.iter().map(|path| path.length(&Euclidean)).sum(),
        travel_length: 0.0,
    };
    let mut cursor: Option<geo::Coord<f32>> = None;
    for path in paths {
        let (Some(first), Some(last)) = (path.0.first(), path.0.last()) else {
            continue;
        };
        if let Some(previous) = cursor {
            let dx: f32 = first.x - previous.x;
            let dy: f32 = first.y - previous.y;
            stats.travel_length += dx.hypot(dy);
        }
        cursor = Some(*last);
    }
    stats
}

/// Export editable cut geometry as a standalone SVG in canvas coordinates.
pub fn cut_svg(paths: &[LineString<f32>], canvas: Vec2) -> String {
    let mut svg = svg_header(canvas);
    svg.push_str("<g fill=\"none\" stroke=\"#ff2ca8\" stroke-width=\"1\">\n");
    for path in paths.iter().filter(|path| path.0.len() >= 2) {
        svg.push_str("  <path d=\"");
        write_path_data(&mut svg, path);
        svg.push_str("\"/>\n");
    }
    svg.push_str("</g>\n</svg>\n");
    svg
}

/// Export cut and blade-up travel geometry for physical-job diagnostics.
pub fn toolpath_debug_svg(paths: &[LineString<f32>], canvas: Vec2) -> String {
    let mut svg = svg_header(canvas);
    svg.push_str("<g fill=\"none\" stroke=\"#ff2ca8\" stroke-width=\"1\">\n");
    for path in paths.iter().filter(|path| path.0.len() >= 2) {
        svg.push_str("  <path d=\"");
        write_path_data(&mut svg, path);
        svg.push_str("\"/>\n");
    }
    svg.push_str("</g>\n<g fill=\"none\" stroke=\"#35a7ff\" stroke-width=\"0.7\" stroke-dasharray=\"4 3\">\n");
    let mut cursor: Option<geo::Coord<f32>> = None;
    for path in paths {
        let (Some(first), Some(last)) = (path.0.first(), path.0.last()) else {
            continue;
        };
        if let Some(previous) = cursor {
            svg.push_str(&format!(
                "  <path d=\"M{} {} L{} {}\"/>\n",
                number(previous.x),
                number(previous.y),
                number(first.x),
                number(first.y)
            ));
        }
        cursor = Some(*last);
    }
    svg.push_str("</g>\n</svg>\n");
    svg
}

/// Wrap an encoded RGB JPEG in a single-page, dependency-free PDF. Canvas
/// units become PDF points, preserving the exact artwork aspect ratio.
pub fn jpeg_pdf(jpeg: &[u8], width: u32, height: u32, dpi: f32) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(width > 0 && height > 0, "PDF dimensions must be non-zero");
    anyhow::ensure!(dpi.is_finite() && dpi > 0.0, "PDF DPI must be positive");
    anyhow::ensure!(
        jpeg.starts_with(&[0xff, 0xd8]) && jpeg.ends_with(&[0xff, 0xd9]),
        "PDF artwork must be an encoded JPEG"
    );

    let page_width = f64::from(width) * 72.0 / f64::from(dpi);
    let page_height = f64::from(height) * 72.0 / f64::from(dpi);
    let page_width = number(page_width as f32);
    let page_height = number(page_height as f32);
    let content = format!("q\n{page_width} 0 0 {page_height} 0 0 cm\n/Im0 Do\nQ\n");
    let mut objects = Vec::<Vec<u8>>::new();
    objects.push(b"<< /Type /Catalog /Pages 2 0 R >>".to_vec());
    objects.push(b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec());
    objects.push(
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {page_width} {page_height}] /Resources << /XObject << /Im0 4 0 R >> >> /Contents 5 0 R >>"
        )
        .into_bytes(),
    );
    let mut image_object = format!(
        "<< /Type /XObject /Subtype /Image /Width {width} /Height {height} /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode /Length {} >>\nstream\n",
        jpeg.len()
    )
    .into_bytes();
    image_object.extend_from_slice(jpeg);
    image_object.extend_from_slice(b"\nendstream");
    objects.push(image_object);
    objects.push(
        format!(
            "<< /Length {} >>\nstream\n{content}endstream",
            content.len()
        )
        .into_bytes(),
    );

    let mut output = b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for (index, object) in objects.iter().enumerate() {
        offsets.push(output.len());
        output.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
        output.extend_from_slice(object);
        output.extend_from_slice(b"\nendobj\n");
    }
    let xref_offset = output.len();
    output.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
    output.extend_from_slice(b"0000000000 65535 f \n");
    for offset in offsets {
        output.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    output.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    Ok(output)
}

fn svg_header(canvas: Vec2) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {} {}\" width=\"{}\" height=\"{}\">\n",
        number(canvas.x),
        number(canvas.y),
        number(canvas.x),
        number(canvas.y)
    )
}

fn write_path_data(output: &mut String, path: &LineString<f32>) {
    for (index, point) in path.0.iter().enumerate() {
        if index == 0 {
            output.push('M');
        } else {
            output.push_str(" L");
        }
        output.push_str(&number(point.x));
        output.push(' ');
        output.push_str(&number(point.y));
    }
    if path.0.first() == path.0.last() {
        output.push_str(" Z");
    }
}

fn number(value: f32) -> String {
    let mut text = format!("{value:.3}");
    while text.contains('.') && text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn svg_export_preserves_multiple_closed_and_open_paths() {
        let paths = vec![
            LineString::from(vec![(0., 0.), (10., 0.), (0., 0.)]),
            LineString::from(vec![(2., 3.), (4., 5.)]),
        ];
        let svg = cut_svg(&paths, Vec2::new(100., 200.));
        assert!(svg.contains("viewBox=\"0 0 100 200\""));
        assert!(svg.contains("M0 0 L10 0 L0 0 Z"));
        assert!(svg.contains("M2 3 L4 5"));
    }

    #[test]
    fn stats_separate_cut_and_travel_lengths() {
        let paths = vec![
            LineString::from(vec![(0., 0.), (3., 4.)]),
            LineString::from(vec![(6., 8.), (6., 10.)]),
        ];
        let stats = toolpath_stats(&paths);
        assert_eq!(stats.paths, 2);
        assert_eq!(stats.nodes, 4);
        assert!((stats.cut_length - 7.).abs() < 0.001);
        assert!((stats.travel_length - 5.).abs() < 0.001);
        assert!(toolpath_debug_svg(&paths, Vec2::splat(20.)).contains("stroke-dasharray"));
    }

    #[test]
    fn jpeg_pdf_has_valid_object_offsets_and_embeds_image() {
        let jpeg = [0xff, 0xd8, 1, 2, 3, 0xff, 0xd9];
        let pdf = jpeg_pdf(&jpeg, 10, 20, 72.0).unwrap();
        assert!(pdf.starts_with(b"%PDF-1.4"));
        assert!(pdf.ends_with(b"%%EOF\n"));
        assert!(pdf.windows(jpeg.len()).any(|window| window == jpeg));
        let text = String::from_utf8_lossy(&pdf);
        assert!(text.contains("/MediaBox [0 0 10 20]"));
        assert!(text.contains("/Filter /DCTDecode"));
        for object in 1..=5 {
            let marker = format!("{object} 0 obj").into_bytes();
            let actual = pdf
                .windows(marker.len())
                .position(|window| window == marker)
                .unwrap();
            assert!(text.contains(&format!("{actual:010} 00000 n")));
        }
    }

    #[test]
    fn jpeg_pdf_converts_raster_dpi_to_physical_page_points() {
        let jpeg = [0xff, 0xd8, 0xff, 0xd9];
        let pdf = jpeg_pdf(&jpeg, 1200, 2100, 300.0).unwrap();
        let text = String::from_utf8_lossy(&pdf);
        assert!(text.contains("/MediaBox [0 0 288 504]"));
        assert!(jpeg_pdf(&jpeg, 1, 1, 0.0).is_err());
    }
}
