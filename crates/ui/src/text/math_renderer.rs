//! LaTeX math rendering pipeline: LaTeX → mitex → Typst → SVG → RenderImage.
//!
//! This module is only available when the `math` feature is enabled.
//! It provides [`render_math_to_element`] which converts a LaTeX string into
//! a GPUI `AnyElement` (an `img` backed by an in-memory rendered SVG).

use std::sync::OnceLock;

use chrono::{Datelike, Timelike};
use gpui::{AnyElement, App, IntoElement, ObjectFit, Styled, StyledImage as _, Window, img};
use typst::foundations::{Bytes, Datetime};
use typst::layout::PagedDocument;
use typst::syntax::{FileId, Source, VirtualPath};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{Library, LibraryExt, World};

// ---------------------------------------------------------------------------
//  MathWorld – minimal Typst World for in-memory formula compilation
// ---------------------------------------------------------------------------

struct MathWorld {
    library: LazyHash<Library>,
    book: LazyHash<FontBook>,
    fonts: Vec<Font>,
    source: Source,
}

/// Global font cache – font parsing is expensive, do it only once.
static FONTS_CACHE: OnceLock<(LazyHash<FontBook>, Vec<Font>)> = OnceLock::new();

fn load_fonts() -> &'static (LazyHash<FontBook>, Vec<Font>) {
    FONTS_CACHE.get_or_init(|| {
        let mut book = FontBook::new();
        let mut fonts = Vec::new();

        for data in typst_assets::fonts() {
            let buffer = Bytes::new(data);
            for font in Font::iter(buffer) {
                book.push(font.info().clone());
                fonts.push(font);
            }
        }

        (LazyHash::new(book), fonts)
    })
}

impl MathWorld {
    fn new(source_text: String) -> Self {
        let (book, fonts) = load_fonts();
        let file_id = FileId::new(None, VirtualPath::new("/main.typ"));
        let source = Source::new(file_id, source_text);

        Self {
            library: LazyHash::new(Library::default()),
            book: book.clone(),
            fonts: fonts.clone(),
            source,
        }
    }
}

impl World for MathWorld {
    fn library(&self) -> &LazyHash<Library> {
        &self.library
    }

    fn book(&self) -> &LazyHash<FontBook> {
        &self.book
    }

    fn main(&self) -> FileId {
        self.source.id()
    }

    fn source(&self, id: FileId) -> typst::diag::FileResult<Source> {
        if id == self.source.id() {
            Ok(self.source.clone())
        } else {
            Err(typst::diag::FileError::NotFound(
                id.vpath().as_rootless_path().into(),
            ))
        }
    }

    fn file(&self, id: FileId) -> typst::diag::FileResult<Bytes> {
        Err(typst::diag::FileError::NotFound(
            id.vpath().as_rootless_path().into(),
        ))
    }

    fn font(&self, index: usize) -> Option<Font> {
        self.fonts.get(index).cloned()
    }

    fn today(&self, offset: Option<i64>) -> Option<Datetime> {
        let now = chrono::Local::now();
        let naive = match offset {
            Some(o) => {
                let utc = chrono::Utc::now().naive_utc();
                utc + chrono::Duration::hours(o)
            }
            None => now.naive_local(),
        };

        Datetime::from_ymd_hms(
            naive.year(),
            naive.month().try_into().ok()?,
            naive.day().try_into().ok()?,
            naive.hour().try_into().ok()?,
            naive.minute().try_into().ok()?,
            naive.second().try_into().ok()?,
        )
    }
}

// ---------------------------------------------------------------------------
//  Step 1: LaTeX → Typst (via mitex) with post-processing fixups
// ---------------------------------------------------------------------------

/// Convert a raw LaTeX math string into a Typst `$ ... $` expression.
fn latex_to_typst(latex: &str) -> Result<String, String> {
    let typst_math = mitex::convert_math(latex, None).map_err(|e| e.to_string())?;
    let typst_math = fixup_mitex_output(&typst_math);
    Ok(format!("$ {} $", typst_math))
}

/// Post-process mitex output to fix constructs that Typst doesn't understand.
fn fixup_mitex_output(typst_math: &str) -> String {
    let mut result = typst_math.to_string();

    // mitexsqrt(\[n\], x) → root(n, x)
    // mitexsqrt(x)        → sqrt(x)
    while let Some(start) = result.find("mitexsqrt(") {
        let inner_start = start + "mitexsqrt(".len();
        if let Some(inner_len) = find_matching_paren(&result[inner_start..]) {
            let inner = &result[inner_start..inner_start + inner_len];
            let replacement = if inner.starts_with("\\[") {
                if let Some(close_bracket) = inner.find("\\]") {
                    let n = inner[2..close_bracket].trim();
                    let rest = inner[close_bracket + 2..].trim_start_matches(',').trim();
                    format!("root({}, {})", n, rest)
                } else {
                    format!("sqrt({})", inner)
                }
            } else {
                format!("sqrt({})", inner)
            };
            result.replace_range(start..inner_start + inner_len + 1, &replacement);
        } else {
            break;
        }
    }

    // aligned(...) → unwrap (Typst natively supports & and \\ alignment)
    result = unwrap_env(&result, "aligned");

    // Matrix environments → mat(delim: ..., ...)
    result = replace_matrix_env(&result, "pmatrix", "\"(\"");
    result = replace_matrix_env(&result, "bmatrix", "\"[\"");
    result = replace_matrix_env(&result, "Bmatrix", "\"{\"");
    result = replace_matrix_env(&result, "vmatrix", "\"|\"");

    // Remove zero-width spaces inserted by mitex
    result = result.replace(" zws ", " ");
    result = result.replace("zws ", "");
    result = result.replace(" zws", "");

    // `\ thick` is mitex's output for `\\\` – just needs `\`
    result = result.replace("\\ thick\n", "\\\n");
    result = result.replace("\\ thick ", "\\ ");

    result
}

/// Find the position of the closing `)` that matches the first `(` at depth 1.
fn find_matching_paren(s: &str) -> Option<usize> {
    let mut depth = 1usize;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Strip the `envname(...)` wrapper, keeping only the inner content.
fn unwrap_env(input: &str, env_name: &str) -> String {
    let mut result = input.to_string();
    let pattern = format!("{}(", env_name);

    while let Some(start) = result.find(&pattern) {
        let inner_start = start + pattern.len();
        if let Some(inner_len) = find_matching_paren(&result[inner_start..]) {
            let inner = result[inner_start..inner_start + inner_len].to_string();
            result.replace_range(start..inner_start + inner_len + 1, inner.trim());
        } else {
            break;
        }
    }

    result
}

/// Replace `envname(body)` with `mat(delim: delim_str, body)`.
fn replace_matrix_env(input: &str, env_name: &str, delim_str: &str) -> String {
    let mut result = input.to_string();
    let pattern = format!("{}(", env_name);

    while let Some(start) = result.find(&pattern) {
        let inner_start = start + pattern.len();
        if let Some(inner_len) = find_matching_paren(&result[inner_start..]) {
            let inner = result[inner_start..inner_start + inner_len].to_string();
            let replacement = format!("mat(delim: {}, {})", delim_str, inner);
            result.replace_range(start..inner_start + inner_len + 1, &replacement);
        } else {
            break;
        }
    }

    result
}

// ---------------------------------------------------------------------------
//  Step 2: Typst formula → SVG string (via typst + typst-svg)
// ---------------------------------------------------------------------------

/// Compile a Typst formula expression and render the first page to SVG.
///
/// `formula` should be a complete Typst math expression, e.g. `$ E = m c^2 $`.
/// `font_size` controls the text size in points (e.g. 16.0).
fn typst_to_svg(formula: &str, font_size: f32) -> Result<String, Vec<String>> {
    let source = format!(
        "#set page(fill: none, width: auto, height: auto, margin: 0pt)\n\
         #set text(size: {font_size}pt)\n\
         {formula}"
    );

    let world = MathWorld::new(source);

    let warned = typst::compile::<PagedDocument>(&world);
    let document = warned.output.map_err(|diags| {
        diags
            .iter()
            .map(|d| d.message.to_string())
            .collect::<Vec<_>>()
    })?;

    if let Some(page) = document.pages.first() {
        Ok(typst_svg::svg(page))
    } else {
        Err(vec!["No pages produced".to_string()])
    }
}

// ---------------------------------------------------------------------------
//  Full pipeline: LaTeX → Typst → SVG string
// ---------------------------------------------------------------------------

/// Convert a raw LaTeX math string to an SVG string.
///
/// `font_size` is the Typst text size in points (e.g. 16.0).
pub fn latex_to_svg(latex: &str, font_size: f32) -> Result<String, String> {
    let typst_formula = latex_to_typst(latex)?;
    typst_to_svg(&typst_formula, font_size).map_err(|errs| errs.join("; "))
}

// ---------------------------------------------------------------------------
//  GPUI integration: SVG string → RenderImage → AnyElement
// ---------------------------------------------------------------------------

/// The default font size (in pt) used when rendering math formulas.
const DEFAULT_MATH_FONT_SIZE: f32 = 16.0;

/// The default scale factor for SVG → pixel rendering.
/// Higher values produce crisper output on HiDPI displays.
const DEFAULT_SVG_SCALE: f32 = 2.0;

/// Render a LaTeX math string into a GPUI `AnyElement`.
///
/// This is the main entry point used by [`MathNode::render`](super::node::MathNode).
///
/// * `latex`   – raw LaTeX source (without `$` delimiters)
/// * `display` – `true` for display/block mode, `false` for inline
/// * `window`  – the current GPUI window
/// * `cx`      – the GPUI app context
///
/// Returns `Some(element)` on success, or `None` if rendering fails
/// (the caller should fall back to showing the raw source).
pub fn render_math_to_element(
    latex: &str,
    _display: bool,
    _window: &mut Window,
    cx: &mut App,
) -> Option<AnyElement> {
    // 1. LaTeX → SVG string
    let svg_string = match latex_to_svg(latex, DEFAULT_MATH_FONT_SIZE) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("math render failed: {}", e);
            return None;
        }
    };

    // 2. SVG bytes → RenderImage via GPUI's built-in resvg pipeline
    let svg_renderer = cx.svg_renderer();
    let render_image = match svg_renderer.render_single_frame(
        svg_string.as_bytes(),
        DEFAULT_SVG_SCALE,
        true, // convert to BGRA (required for GPU upload)
    ) {
        Ok(image) => image,
        Err(e) => {
            tracing::warn!("SVG render failed: {}", e);
            return None;
        }
    };

    // 3. Build an img() element from the in-memory RenderImage
    let element = img(render_image)
        .object_fit(ObjectFit::Contain)
        .flex_shrink_0()
        .into_any_element();

    Some(element)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_latex_to_typst_simple() {
        let result = latex_to_typst(r"e^{i\pi} + 1 = 0");
        assert!(result.is_ok(), "should convert simple formula");
        let typst = result.unwrap();
        assert!(typst.starts_with("$ "), "should wrap in $ delimiters");
        assert!(typst.ends_with(" $"), "should wrap in $ delimiters");
    }

    #[test]
    fn test_latex_to_typst_fraction() {
        let result = latex_to_typst(r"\frac{a}{b}");
        assert!(result.is_ok());
    }

    #[test]
    fn test_latex_to_svg_produces_svg() {
        let result = latex_to_svg(r"x^2 + y^2 = z^2", 16.0);
        assert!(result.is_ok(), "should produce SVG: {:?}", result.err());
        let svg = result.unwrap();
        assert!(svg.contains("<svg"), "output should be SVG markup");
        assert!(svg.contains("</svg>"), "output should be complete SVG");
    }

    #[test]
    fn test_latex_to_svg_euler() {
        let result = latex_to_svg(r"e^{i\pi} + 1 = 0", 16.0);
        assert!(result.is_ok());
    }

    #[test]
    fn test_latex_to_svg_integral() {
        let result = latex_to_svg(r"\int_0^\infty e^{-x^2} \, dx = \frac{\sqrt{\pi}}{2}", 16.0);
        assert!(result.is_ok());
    }

    #[test]
    fn test_latex_to_svg_matrix() {
        let result = latex_to_svg(r"\begin{pmatrix} 1 & 2 \\ 3 & 4 \end{pmatrix}", 16.0);
        assert!(result.is_ok());
    }

    #[test]
    fn test_fixup_sqrt() {
        let input = "mitexsqrt(x)";
        assert_eq!(fixup_mitex_output(input), "sqrt(x)");
    }

    #[test]
    fn test_fixup_nth_root() {
        let input = r"mitexsqrt(\[3 \],x)";
        let result = fixup_mitex_output(input);
        assert_eq!(result, "root(3, x)");
    }

    #[test]
    fn test_fixup_pmatrix() {
        let input = "pmatrix(a, b; c, d)";
        let result = fixup_mitex_output(input);
        assert!(result.contains("mat(delim:"));
    }

    #[test]
    fn test_fixup_zws_removal() {
        let input = "a zws b";
        assert_eq!(fixup_mitex_output(input), "a b");
    }

    #[test]
    fn test_fixup_thick_newline() {
        let input = "a \\ thick\nb";
        assert_eq!(fixup_mitex_output(input), "a \\\nb");
    }

    #[test]
    fn test_invalid_latex_returns_error() {
        // Completely broken LaTeX should not panic
        let result = latex_to_svg(r"\begin{nonexistent}", 16.0);
        // It may succeed (mitex tries its best) or fail – either way, no panic.
        let _ = result;
    }
}
