use crate::{
    catalog::{Item, Kind, Settings},
    db::Database,
    paths,
};
use anyhow::{Context, Result, bail};
use base64::Engine;
use image::{ImageFormat, ImageReader};
use pdfium_render::prelude::*;
use std::{
    fs,
    io::{Cursor, Read},
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

const MAX_TEXT: usize = 1024 * 1024;
const MODEL_CHARS: usize = 24000;
pub struct Extracted {
    pub text: String,
    pub limitations: String,
}
pub fn excerpt(text: &str, max: usize) -> String {
    let c: Vec<char> = text.chars().collect();
    if c.len() <= max {
        return text.to_string();
    }
    let n = max / 3;
    [
        c[..n].iter().collect::<String>(),
        c[c.len() / 2 - n / 2..c.len() / 2 - n / 2 + n]
            .iter()
            .collect(),
        c[c.len() - n..].iter().collect(),
    ]
    .join("\n[excerpt omitted]\n")
}
pub fn model_text(text: &str) -> String {
    excerpt(text, MODEL_CHARS)
}
pub fn pdfium(settings: &Settings) -> Result<Pdfium> {
    let binding = if settings.pdfium_library.is_empty() {
        Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path(
            std::env::current_exe()?
                .parent()
                .context("Executable path")?,
        ))
        .or_else(|_| Pdfium::bind_to_system_library())
    } else {
        Pdfium::bind_to_library(&settings.pdfium_library)
    };
    Ok(Pdfium::new(binding.context(
        "PDFium unavailable: place pdfium.dll alongside the executable or set its path in Settings",
    )?))
}
pub fn extract(
    db: &Database,
    item: &Item,
    settings: &Settings,
    cancel: &AtomicBool,
) -> Result<Extracted> {
    paths::cancelled(cancel)?;
    if item.kind == Kind::Project {
        let mut members: Vec<_> = db
            .items()?
            .into_iter()
            .filter(|i| i.project_id == Some(item.id) && !i.missing && i.relevance != "excluded")
            .collect();
        members.sort_by_key(|i| {
            let n = Path::new(&i.current_path)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_lowercase();
            if n.starts_with("readme") {
                0
            } else if [
                "cargo.toml",
                "package.json",
                "pyproject.toml",
                "pom.xml",
                "build.gradle",
                "build.gradle.kts",
                "cmakelists.txt",
            ]
            .contains(&n.as_str())
            {
                1
            } else {
                2
            }
        });
        let mut text = String::new();
        let mut sampled = 0;
        for m in members.iter().take(20) {
            paths::cancelled(cancel)?;
            if let Ok(e) = extract_file(Path::new(&m.current_path), settings, cancel)
                && !e.text.is_empty()
            {
                text.push_str(&format!(
                    "\nFILE {}\n{}",
                    m.current_path,
                    excerpt(&e.text, 3000)
                ));
                sampled += 1;
            }
        }
        return Ok(Extracted {
            text,
            limitations: format!(
                "Project sample: {sampled} files, {} relevant members; generated and dependency files omitted",
                members.len()
            ),
        });
    }
    extract_file(Path::new(&item.current_path), settings, cancel)
}
pub fn extract_file(path: &Path, settings: &Settings, cancel: &AtomicBool) -> Result<Extracted> {
    let ext = path
        .extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();
    if ext == "pdf" {
        let engine = pdfium(settings)?;
        let doc = engine.load_pdf_from_file(path, None)?;
        let mut text = String::new();
        let count = doc.pages().len();
        let mut readable = 0;
        for page in 0..count.min(100) {
            paths::cancelled(cancel)?;
            let page_text = doc.pages().get(page)?.text()?.all();
            readable += page_text.chars().filter(|c| c.is_alphanumeric()).count();
            text.push_str(&format!("\n[Page {}]\n{}", page + 1, page_text));
            if text.len() > MAX_TEXT {
                break;
            }
        }
        let limits = if readable < 20 {
            "No readable PDF text; scan or empty document (OCR deferred)".into()
        } else if count > 100 || text.len() > MAX_TEXT {
            "Partial PDF extraction (100 pages / 1 MiB limit)".into()
        } else if text.chars().count() > MODEL_CHARS {
            "Model receives representative excerpts; full extracted text cached".into()
        } else {
            String::new()
        };
        if text.len() > MAX_TEXT {
            text = excerpt(&text, MAX_TEXT / 4)
        }
        return Ok(Extracted {
            text,
            limitations: limits,
        });
    }
    if crate::scanner::kind(path) == Kind::Image {
        return Ok(Extracted {
            text: String::new(),
            limitations:
                "Visual analysis uses a resized image (at most 1024 pixels); original retained"
                    .into(),
        });
    }
    if matches!(
        ext.as_str(),
        "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "odt" | "ods"
    ) || crate::scanner::kind(path) != Kind::Document
    {
        return Ok(Extracted {
            text: String::new(),
            limitations: "Metadata-only analysis; content extraction not supported for this format"
                .into(),
        });
    }
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take((MAX_TEXT + 1) as u64)
        .read_to_end(&mut bytes)?;
    let partial = bytes.len() > MAX_TEXT;
    if partial {
        bytes.truncate(MAX_TEXT);
        while std::str::from_utf8(&bytes)
            .err()
            .is_some_and(|e| e.error_len().is_none())
        {
            bytes.pop();
        }
    }
    let text = String::from_utf8(bytes).context("Not UTF-8 text")?;
    if text.contains('\0') {
        bail!("Binary content in text file")
    }
    Ok(Extracted {
        limitations: if partial {
            "Partial extraction (1 MiB limit)".into()
        } else if text.chars().count() > MODEL_CHARS {
            "Model receives representative excerpts; full text cached".into()
        } else {
            String::new()
        },
        text,
    })
}
pub fn resized_image(path: &Path, width: u32) -> Result<image::DynamicImage> {
    let mut reader = ImageReader::open(path)?.with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(128 * 1024 * 1024);
    limits.max_image_width = Some(20000);
    limits.max_image_height = Some(20000);
    reader.limits(limits);
    Ok(reader.decode()?.thumbnail(width, width))
}
pub fn image_data(path: &Path) -> Result<String> {
    let image = resized_image(path, 1024)?;
    let mut out = Cursor::new(Vec::new());
    image.write_to(&mut out, ImageFormat::Png)?;
    Ok(format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(out.into_inner())
    ))
}
#[derive(Debug, Clone)]
pub struct Preview {
    pub item_id: i64,
    pub revision: i64,
    pub path: Option<PathBuf>,
    pub page: u16,
    pub pages: u16,
    pub error: String,
}
pub fn preview(
    item: &Item,
    page: u16,
    width: u32,
    settings: &Settings,
    cache: &Path,
) -> Result<Preview> {
    fs::create_dir_all(cache)?;
    let mut key = sha2::Sha256::new();
    use sha2::Digest;
    key.update(item.fingerprint.as_bytes());
    let output = cache.join(format!(
        "{}-{:x}-{page}-{width}.png",
        item.id,
        key.finalize()
    ));
    let is_pdf = Path::new(&item.current_path)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("pdf"));
    let pages = if is_pdf {
        let engine = pdfium(settings)?;
        let doc = engine.load_pdf_from_file(&item.current_path, None)?;
        let count = doc.pages().len() as u16;
        if page >= count {
            bail!("Page out of range")
        }
        if !output.exists() {
            doc.pages()
                .get(page)?
                .render_with_config(
                    &PdfRenderConfig::new()
                        .set_target_width(width as i32)
                        .set_maximum_height((width * 2) as i32),
                )?
                .as_image()
                .save(&output)?;
        }
        count
    } else if item.kind == Kind::Image {
        if !output.exists() {
            resized_image(Path::new(&item.current_path), width)?.save(&output)?;
        }
        1
    } else {
        0
    };
    Ok(Preview {
        item_id: item.id,
        revision: item.revision,
        path: if pages > 0 { Some(output) } else { None },
        page,
        pages,
        error: String::new(),
    })
}
