use crate::{
    catalog::{Kind, Source},
    db::Database,
    paths,
};
use anyhow::{Context, Result};
use rusqlite::params;
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

pub fn kind(path: &Path) -> Kind {
    let ext = path
        .extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();
    if [
        "readme",
        "license",
        "makefile",
        "dockerfile",
        ".gitignore",
        ".editorconfig",
    ]
    .contains(&name.as_str())
    {
        return Kind::Document;
    }
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff" => Kind::Image,
        "mp3" | "wav" | "flac" | "m4a" | "ogg" | "aac" => Kind::Audio,
        "mp4" | "mkv" | "mov" | "avi" | "webm" => Kind::Video,
        "sql" | "tex" | "ini" | "cfg" | "properties" | "log" | "pdf" | "txt" | "md" | "rst"
        | "csv" | "json" | "yaml" | "yml" | "toml" | "xml" | "html" | "css" | "rs" | "py"
        | "js" | "ts" | "tsx" | "jsx" | "java" | "kt" | "kts" | "c" | "cpp" | "h" | "cs" | "go"
        | "sh" | "ps1" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "odt" | "ods" => {
            Kind::Document
        }
        _ => Kind::Other,
    }
}

fn skipped(path: &Path, project: bool) -> Option<&'static str> {
    let name = path.file_name()?.to_string_lossy().to_lowercase();
    if [".git", ".svn", ".hg"].contains(&name.as_str()) {
        return Some("Repository metadata");
    }
    if [
        "node_modules",
        ".venv",
        "venv",
        "__pycache__",
        ".gradle",
        ".cache",
        ".mypy_cache",
        ".pytest_cache",
    ]
    .contains(&name.as_str())
    {
        return Some("Dependencies or cache");
    }
    if project
        && [
            "target", "build", "dist", "out", "bin", "obj", "vendor", ".next", ".nuxt",
        ]
        .contains(&name.as_str())
    {
        return Some("Project dependencies or generated output");
    }
    None
}

pub fn project_fingerprint(db: &Database, root: &Path, cancel: &AtomicBool) -> Result<String> {
    fn gather(
        db: &Database,
        p: &Path,
        files: &mut Vec<PathBuf>,
        cancel: &AtomicBool,
    ) -> Result<()> {
        paths::cancelled(cancel)?;
        let m = fs::symlink_metadata(p)?;
        if paths::is_link(&m) {
            return Ok(());
        }
        if db.override_for(p)? == Some(false)
            || (db.override_for(p)?.is_none() && skipped(p, true).is_some())
        {
            for included in db.included_descendants(p)? {
                paths::ensure_no_link_ancestors(&included)?;
                gather(db, &included, files, cancel)?;
            }
            return Ok(());
        }
        if m.is_dir() {
            let mut entries = fs::read_dir(p)?.collect::<std::io::Result<Vec<_>>>()?;
            entries.sort_by_key(|e| e.path());
            for e in entries {
                gather(db, &e.path(), files, cancel)?;
            }
        } else if m.is_file() {
            files.push(p.to_path_buf());
        }
        Ok(())
    }
    let mut files = Vec::new();
    gather(db, root, &mut files, cancel)?;
    files.sort();
    files.dedup();
    let mut hash = Sha256::new();
    for f in files {
        hash.update(f.strip_prefix(root)?.to_string_lossy().as_bytes());
        hash.update(paths::hash_file(&f, cancel)?.as_bytes());
    }
    Ok(format!("{:x}", hash.finalize()))
}

struct Scan<'a, F: FnMut(&str)> {
    db: &'a Database,
    sources: Vec<Source>,
    id: i64,
    cancel: &'a AtomicBool,
    progress: F,
    count: usize,
}

pub fn scan(db: &Database, cancel: &AtomicBool, progress: impl FnMut(&str)) -> Result<usize> {
    let sources = db.sources()?;
    db.conn.execute(
        "INSERT INTO scans(started,state) VALUES(?,'running')",
        [paths::now()],
    )?;
    let id = db.conn.last_insert_rowid();
    let mut s = Scan {
        db,
        sources,
        id,
        cancel,
        progress,
        count: 0,
    };
    let roots: Vec<PathBuf> = s
        .sources
        .iter()
        .map(|s| PathBuf::from(&s.path))
        .filter(|p| {
            !s.sources
                .iter()
                .any(|s| Path::new(&s.path) != p && paths::within(p, Path::new(&s.path)))
        })
        .collect();
    let result: Result<usize> = (|| {
        for root in roots {
            paths::cancelled(cancel)?;
            let block = root
                .parent()
                .and_then(|p| p.ancestors().filter(|p| paths::project_marker(p)).last())
                .map(|p| format!("Partial project; select its full root: {}", p.display()))
                .unwrap_or_default();
            if !block.is_empty() {
                s.project(&root, &block)?;
            } else {
                s.walk(&root)?;
            }
        }
        Ok(s.count)
    })();
    // Reconcile known paths even when an entire selected root disappeared.
    for item in db.items()? {
        if s.sources
            .iter()
            .any(|src| paths::within(Path::new(&item.current_path), Path::new(&src.path)))
            && fs::symlink_metadata(&item.current_path)
                .err()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        {
            db.conn.execute(
                "UPDATE items SET missing=1,status='missing' WHERE id=?",
                [item.id],
            )?;
            db.invalidate_moves(item.id, "Source missing")?;
        }
    }
    let (state, error) = match &result {
        Ok(_) => ("completed", String::new()),
        Err(e) => {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                ("cancelled", e.to_string())
            } else {
                ("failed", format!("{e:#}"))
            }
        }
    };
    db.conn.execute(
        "UPDATE scans SET finished=?,state=?,error=? WHERE id=?",
        params![paths::now(), state, error, id],
    )?;
    result
}

impl<F: FnMut(&str)> Scan<'_, F> {
    fn exclusion(&self, p: &Path, reason: &str) -> Result<()> {
        self.db.conn.execute("INSERT INTO exclusions(path,reason,scan_id) VALUES(?,?,?) ON CONFLICT(path) DO UPDATE SET reason=excluded.reason,scan_id=excluded.scan_id",params![p.to_string_lossy(),reason,self.id])?;
        Ok(())
    }
    fn allowed(&self, p: &Path, project: bool) -> Result<bool> {
        let meta =
            fs::symlink_metadata(p).with_context(|| format!("Cannot inspect {}", p.display()))?;
        if paths::is_link(&meta) {
            self.exclusion(p, "Symbolic link or junction (not followed)")?;
            self.db
                .exclude_catalog_subtree(p, "Symbolic link or junction (not followed)", true)?;
            return Ok(false);
        }
        let reason = match self.db.override_for(p)? {
            Some(true) => None,
            Some(false) => Some("Explicit exclusion"),
            None => skipped(p, project),
        };
        if let Some(reason) = reason {
            self.exclusion(p, reason)?;
            self.db.exclude_catalog_subtree(p, reason, false)?;
            return Ok(false);
        }
        self.db
            .conn
            .execute("DELETE FROM exclusions WHERE path=?", [p.to_string_lossy()])?;
        Ok(true)
    }
    fn walk(&mut self, p: &Path) -> Result<()> {
        paths::cancelled(self.cancel)?;
        if !self.allowed(p, false)? {
            if !paths::is_link(&fs::symlink_metadata(p)?) {
                for included in self.db.included_descendants(p)? {
                    paths::ensure_no_link_ancestors(&included)?;
                    self.walk(&included)?;
                }
            }
            return Ok(());
        }
        if p.is_dir() {
            if paths::project_marker(p) {
                return self.project(p, "");
            }
            let mut entries = fs::read_dir(p)?.collect::<std::io::Result<Vec<_>>>()?;
            entries.sort_by_key(|e| e.path());
            for entry in entries {
                self.walk(&entry.path())?;
            }
        } else if p.is_file() {
            self.file(p, None)?;
        }
        Ok(())
    }
    fn file(&mut self, p: &Path, project: Option<i64>) -> Result<()> {
        let meta = fs::metadata(p)?;
        let (hash, block) = match paths::hash_file(p, self.cancel) {
            Ok(hash) => (hash, String::new()),
            Err(error) => {
                paths::cancelled(self.cancel)?;
                (
                    "unreadable".into(),
                    format!("Cannot read file; retry a scan after restoring access: {error:#}"),
                )
            }
        };
        let fingerprint = format!("{}:{}", paths::metadata_fingerprint(&meta), hash);
        let path = if block.is_empty() {
            paths::normalize(p)?
        } else {
            paths::absolute(p)?
                .to_str()
                .context("Non-Unicode file path")?
                .to_string()
        };
        let (id, _) = self.db.upsert_item(
            &path,
            kind(p),
            project,
            &fingerprint,
            &block,
            self.id,
            &self.sources,
        )?;
        if project.is_some() && block.is_empty() {
            self.db.conn.execute("UPDATE items SET status='project member',relevance='relevant' WHERE id=? AND relevance_manual IS NULL",[id])?;
        }
        self.count += 1;
        if self.count.is_multiple_of(25) {
            (self.progress)(&format!("Scanning: {} files", self.count));
        }
        Ok(())
    }
    fn gather(&self, p: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
        paths::cancelled(self.cancel)?;
        if !self.allowed(p, true)? {
            for included in self.db.included_descendants(p)? {
                paths::ensure_no_link_ancestors(&included)?;
                self.gather(&included, files)?;
            }
            return Ok(());
        }
        if p.is_dir() {
            let mut entries = fs::read_dir(p)?.collect::<std::io::Result<Vec<_>>>()?;
            entries.sort_by_key(|e| e.path());
            for e in entries {
                self.gather(&e.path(), files)?;
            }
        } else if p.is_file() {
            files.push(p.to_path_buf());
        }
        Ok(())
    }
    fn project(&mut self, p: &Path, block: &str) -> Result<()> {
        let mut files = Vec::new();
        self.gather(p, &mut files)?;
        files.sort();
        files.dedup();
        let mut hash = Sha256::new();
        let mut read_error = None;
        for f in &files {
            paths::cancelled(self.cancel)?;
            hash.update(f.strip_prefix(p)?.to_string_lossy().as_bytes());
            match paths::hash_file(f, self.cancel) {
                Ok(value) => hash.update(value.as_bytes()),
                Err(error) => {
                    paths::cancelled(self.cancel)?;
                    hash.update(b"unreadable");
                    read_error = Some(format!(
                        "Cannot read project member {}: {error:#}; retry a scan after restoring access",
                        f.display()
                    ));
                }
            }
        }
        let fingerprint = format!("{:x}", hash.finalize());
        let linked_git = |root: &Path| {
            root.join(".git").is_file()
                || fs::read_dir(root.join(".git/worktrees"))
                    .is_ok_and(|mut entries| entries.next().is_some())
        };
        let block = if let Some(error) = read_error.as_deref() {
            error
        } else if linked_git(p)
            || files.iter().any(|f| {
                f.ancestors()
                    .filter(|a| paths::within(a, p))
                    .any(linked_git)
            })
        {
            "Linked Git worktrees/submodules cannot be moved"
        } else {
            block
        };
        let path = paths::normalize(p)?;
        let (id, _) = self.db.upsert_item(
            &path,
            Kind::Project,
            None,
            &fingerprint,
            block,
            self.id,
            &self.sources,
        )?;
        // A newly selected outer root absorbs previously cataloged partial or nested units.
        for previous in self.db.items()?.iter().filter(|i| {
            i.id != id && i.kind == Kind::Project && paths::within(Path::new(&i.current_path), p)
        }) {
            if previous.project_id != Some(id) {
                self.db.conn.execute("UPDATE items SET project_id=?,revision=revision+1,status='project member',block_reason='' WHERE id=?",params![id,previous.id])?;
                self.db
                    .invalidate_moves(previous.id, "Project now belongs to its outer root")?;
            }
        }
        for f in files {
            self.file(&f, Some(id))?;
        }
        Ok(())
    }
}
