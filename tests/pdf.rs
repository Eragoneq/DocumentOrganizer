use document_organizer::{catalog::Settings, content, db::Database, scanner};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

fn settings() -> Settings {
    let library = std::env::var_os("PDFIUM_LIBRARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("runtimes/pdfium-7543/bin/pdfium.dll")
        });
    assert!(
        library.is_file(),
        "Run scripts/setup-pdfium.ps1 or set PDFIUM_LIBRARY"
    );
    Settings {
        pdfium_library: library.to_string_lossy().into_owned(),
        ..Settings::default()
    }
}
fn make_pdf(path: &Path, pages: &[&str]) {
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        format!(
            "<< /Type /Pages /Kids [{}] /Count {} >>",
            (0..pages.len())
                .map(|i| format!("{} 0 R", 4 + 2 * i))
                .collect::<Vec<_>>()
                .join(" "),
            pages.len()
        ),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
    ];
    for (i, text) in pages.iter().enumerate() {
        let stream = if text.is_empty() {
            String::new()
        } else {
            format!("BT /F1 20 Tf 72 720 Td ({text}) Tj ET")
        };
        objects.push(format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>",5+2*i));
        objects.push(format!(
            "<< /Length {} >>\nstream\n{stream}\nendstream",
            stream.len()
        ));
    }
    let mut pdf = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", i + 1).as_bytes());
    }
    let xref = pdf.len();
    pdf.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    fs::write(path, pdf).unwrap();
}

#[test]
#[ignore = "Requires the pinned PDFium runtime; run with --test pdf -- --ignored"]
fn digital_pdf_text_and_lazy_page_previews_preserve_originals() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let path = source.join("invoice.pdf");
    make_pdf(
        &path,
        &[
            "Quarterly income invoice for personal documents",
            "Second page with readable invoice details",
        ],
    );
    let original = fs::read(&path).unwrap();
    let s = settings();
    let e = content::extract_file(&path, &s, &AtomicBool::new(false)).unwrap();
    assert!(e.text.contains("Quarterly income invoice"));
    assert!(e.text.contains("Second page"));
    assert!(e.limitations.is_empty());
    let db = Database::open(&temp.path().join("catalog.sqlite3")).unwrap();
    db.add_source(&source).unwrap();
    scanner::scan(&db, &AtomicBool::new(false), |_| {}).unwrap();
    let item = db.items().unwrap().remove(0);
    let cache = temp.path().join("previews");
    let first = content::preview(&item, 0, 64, &s, &cache).unwrap();
    assert_eq!(first.pages, 2);
    assert_eq!(fs::read_dir(&cache).unwrap().count(), 1);
    let rendered = content::preview(&item, 1, 600, &s, &cache).unwrap();
    assert_eq!(fs::read_dir(&cache).unwrap().count(), 2);
    assert_eq!(image::open(rendered.path.unwrap()).unwrap().width(), 600);
    let cached = first.path.unwrap();
    let modified = fs::metadata(&cached).unwrap().modified().unwrap();
    content::preview(&item, 0, 64, &s, &cache).unwrap();
    assert_eq!(fs::metadata(&cached).unwrap().modified().unwrap(), modified);
    assert_eq!(fs::read(path).unwrap(), original);
}
#[test]
#[ignore = "Requires the pinned PDFium runtime; run with --test pdf -- --ignored"]
fn empty_pdf_page_labels_are_not_mistaken_for_extracted_text() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("scan.pdf");
    make_pdf(&path, &[""; 12]);
    let e = content::extract_file(&path, &settings(), &AtomicBool::new(false)).unwrap();
    assert!(e.limitations.contains("No readable PDF text"));
    assert!(e.limitations.contains("OCR deferred"));
}
#[test]
#[ignore = "Requires the pinned PDFium runtime; run with --test pdf -- --ignored"]
fn invalid_pdfs_return_an_extraction_error_without_mutation() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("invalid.pdf");
    fs::write(&path, b"This is not a PDF").unwrap();
    assert!(content::extract_file(&path, &settings(), &AtomicBool::new(false)).is_err());
    assert_eq!(fs::read(path).unwrap(), b"This is not a PDF");
}
