use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
pub fn cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        bail!("Cancelled")
    }
    Ok(())
}
pub fn is_link(meta: &fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    false
}
pub fn absolute(path: &Path) -> Result<PathBuf> {
    let p = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            _ => out.push(c.as_os_str()),
        }
    }
    Ok(out)
}
pub fn normalize(path: &Path) -> Result<String> {
    let p = fs::canonicalize(path).with_context(|| format!("Cannot access {}", path.display()))?;
    let p = p.to_str().context("Non-Unicode paths are not supported")?;
    #[cfg(windows)]
    let p = p
        .strip_prefix("\\\\?\\UNC\\")
        .map(|s| format!("\\\\{s}"))
        .unwrap_or_else(|| p.strip_prefix("\\\\?\\").unwrap_or(p).to_string());
    #[cfg(not(windows))]
    let p = p.to_string();
    Ok(p)
}
pub fn key(path: &str) -> String {
    if cfg!(windows) {
        path.replace('/', "\\").to_lowercase()
    } else {
        path.into()
    }
}
pub fn within(child: &Path, parent: &Path) -> bool {
    let c = key(&child.to_string_lossy());
    let p = key(&parent.to_string_lossy());
    Path::new(&c).starts_with(Path::new(&p))
}
pub fn ensure_no_link_ancestors(path: &Path) -> Result<()> {
    for ancestor in path.ancestors() {
        if let Ok(m) = fs::symlink_metadata(ancestor)
            && is_link(&m)
        {
            bail!("Link or junction in path: {}", ancestor.display())
        }
    }
    Ok(())
}
pub fn metadata_fingerprint(meta: &fs::Metadata) -> String {
    let m = meta
        .modified()
        .unwrap_or(UNIX_EPOCH)
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{}:{m}", meta.len())
}
pub fn hash_file(path: &Path, cancel: &AtomicBool) -> Result<String> {
    let mut f = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        cancelled(cancel)?;
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
pub fn safe_component(s: &str) -> Result<()> {
    safe_filename(s)?;
    if s != s.trim() || s.chars().count() > 80 {
        bail!("Folder/tag names must have no surrounding whitespace and be at most 80 characters")
    }
    Ok(())
}
pub fn safe_filename(s: &str) -> Result<()> {
    if s.trim().is_empty()
        || s.ends_with('.')
        || s.ends_with(' ')
        || s.encode_utf16().count() > 255
        || s.chars()
            .any(|c| c.is_control() || "<>:\"/\\|?*".contains(c))
        || s == "."
        || s == ".."
    {
        bail!("Invalid Windows filename")
    }
    let stem = s.split('.').next().unwrap_or("").to_uppercase();
    if ["CON", "PRN", "AUX", "NUL"].contains(&stem.as_str())
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit())
    {
        bail!("Reserved Windows name")
    }
    Ok(())
}

pub fn project_marker(path: &Path) -> bool {
    [
        ".git",
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "pom.xml",
        "build.gradle",
        "build.gradle.kts",
        "CMakeLists.txt",
        "go.mod",
    ]
    .iter()
    .any(|n| path.join(n).exists())
        || fs::read_dir(path)
            .ok()
            .map(|e| {
                e.filter_map(|e| e.ok()).any(|e| {
                    e.path()
                        .extension()
                        .is_some_and(|x| x == "csproj" || x == "sln")
                })
            })
            .unwrap_or(false)
}
