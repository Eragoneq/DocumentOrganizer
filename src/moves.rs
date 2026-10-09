use crate::{catalog::Item, db::Database, paths};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::{fs, path::Path, sync::atomic::AtomicBool, time::UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub relative: String,
    pub kind: String,
    pub hash: String,
    pub modified_ns: u128,
    pub readonly: bool,
    #[serde(default)]
    pub hard_linked: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub entries: Vec<Entry>,
}
impl Manifest {
    pub fn content_matches(&self, other: &Self) -> bool {
        self.entries.len() == other.entries.len()
            && self.entries.iter().zip(&other.entries).all(|(a, b)| {
                a.relative == b.relative
                    && a.kind == b.kind
                    && a.hash == b.hash
                    && a.readonly == b.readonly
            })
    }
    pub fn has_links(&self) -> bool {
        self.entries
            .iter()
            .any(|e| e.kind == "link" || e.hard_linked)
    }
}
#[derive(Debug, Clone)]
pub struct MoveOperation {
    pub id: i64,
    pub item_id: i64,
    pub source: String,
    pub destination: String,
    pub revision: i64,
    pub category_revision: i64,
    pub snapshot: Manifest,
    pub state: String,
    pub temp_path: String,
    pub error: String,
    pub undo_of: Option<i64>,
}

fn read_operation(r: &rusqlite::Row<'_>) -> rusqlite::Result<MoveOperation> {
    let s: String = r.get(6)?;
    let snapshot = serde_json::from_str(&s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(MoveOperation {
        id: r.get(0)?,
        item_id: r.get(1)?,
        source: r.get(2)?,
        destination: r.get(3)?,
        revision: r.get(4)?,
        category_revision: r.get(5)?,
        snapshot,
        state: r.get(7)?,
        temp_path: r.get(8)?,
        error: r.get(9)?,
        undo_of: r.get(10)?,
    })
}
const SELECT: &str = "SELECT id,item_id,source,destination,revision,category_revision,snapshot,state,temp_path,error,undo_of FROM moves";
fn at(root: &Path, relative: &str) -> std::path::PathBuf {
    if relative.is_empty() {
        root.to_path_buf()
    } else {
        root.join(relative)
    }
}
pub fn operations(db: &Database) -> Result<Vec<MoveOperation>> {
    Ok(db
        .conn
        .prepare(&format!("{SELECT} ORDER BY id DESC"))?
        .query_map([], read_operation)?
        .collect::<rusqlite::Result<_>>()?)
}
pub fn summaries(db: &Database) -> Result<Vec<MoveOperation>> {
    Ok(db
        .conn
        .prepare(&format!(
            "{} ORDER BY id DESC",
            SELECT.replace(",snapshot,state", ",'{\"entries\":[]}' AS snapshot,state")
        ))?
        .query_map([], read_operation)?
        .collect::<rusqlite::Result<_>>()?)
}
pub fn operation(db: &Database, id: i64) -> Result<MoveOperation> {
    Ok(db
        .conn
        .query_row(&format!("{SELECT} WHERE id=?"), [id], read_operation)?)
}
fn state(db: &Database, id: i64, value: &str, error: &str) -> Result<()> {
    let tx = db.conn.unchecked_transaction()?;
    tx.execute(
        "UPDATE moves SET state=?,error=? WHERE id=?",
        params![value, error, id],
    )?;
    tx.execute("INSERT INTO move_events(move_id,state,temp_path,error,created) SELECT id,state,temp_path,error,? FROM moves WHERE id=?",params![paths::now(),id])?;
    tx.commit()?;
    Ok(())
}

pub fn manifest(root: &Path, cancel: &AtomicBool) -> Result<Manifest> {
    let mut entries = Vec::new();
    fn walk(root: &Path, p: &Path, entries: &mut Vec<Entry>, cancel: &AtomicBool) -> Result<()> {
        paths::cancelled(cancel)?;
        let meta = fs::symlink_metadata(p)?;
        let relative = p
            .strip_prefix(root)?
            .to_str()
            .context("Non-Unicode file path")?
            .to_string();
        let (kind, hash) = if paths::is_link(&meta) {
            ("link", fs::read_link(p)?.to_string_lossy().into_owned())
        } else if meta.is_file() {
            ("file", hash_with_streams(p, cancel)?)
        } else if meta.is_dir() {
            ("directory", String::new())
        } else {
            bail!("Unsupported filesystem entry: {}", p.display())
        };
        entries.push(Entry {
            relative,
            kind: kind.into(),
            hash,
            modified_ns: meta
                .modified()?
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            readonly: meta.permissions().readonly(),
            hard_linked: kind == "file" && has_hard_links(p)?,
        });
        if kind == "directory" {
            let mut children = fs::read_dir(p)?.collect::<std::io::Result<Vec<_>>>()?;
            children.sort_by_key(|e| e.path());
            for child in children {
                walk(root, &child.path(), entries, cancel)?;
            }
        }
        Ok(())
    }
    walk(root, root, &mut entries, cancel)?;
    entries.sort_by(|a, b| a.relative.cmp(&b.relative));
    Ok(Manifest { entries })
}
fn has_hard_links(path: &Path) -> Result<bool> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
        };
        let file = fs::File::open(path)?;
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), &mut info) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(info.nNumberOfLinks > 1)
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(fs::metadata(path)?.nlink() > 1)
    }
    #[cfg(not(any(windows, unix)))]
    Ok(false)
}
fn hash_with_streams(path: &Path, cancel: &AtomicBool) -> Result<String> {
    let hash = paths::hash_file(path, cancel)?;
    #[cfg(windows)]
    {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(hash);
        for name in streams(path)? {
            h.update(name.as_bytes());
            let mut p = path.as_os_str().to_os_string();
            p.push(&name);
            h.update(paths::hash_file(Path::new(&p), cancel)?);
        }
        Ok(format!("{:x}", h.finalize()))
    }
    #[cfg(not(windows))]
    Ok(hash)
}
#[cfg(windows)]
fn wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}
#[cfg(windows)]
fn streams(path: &Path) -> Result<Vec<String>> {
    use windows_sys::Win32::{
        Foundation::{
            ERROR_HANDLE_EOF, ERROR_INVALID_FUNCTION, GetLastError, INVALID_HANDLE_VALUE,
        },
        Storage::FileSystem::{
            FindClose, FindFirstStreamW, FindNextStreamW, FindStreamInfoStandard,
            WIN32_FIND_STREAM_DATA,
        },
    };
    let mut data: WIN32_FIND_STREAM_DATA = unsafe { std::mem::zeroed() };
    let file = wide(path);
    // The buffer lives for the duration of enumeration; every successful handle is closed.
    let h = unsafe {
        FindFirstStreamW(
            file.as_ptr(),
            FindStreamInfoStandard,
            (&mut data as *mut WIN32_FIND_STREAM_DATA).cast(),
            0,
        )
    };
    if h == INVALID_HANDLE_VALUE {
        let err = unsafe { GetLastError() };
        if [ERROR_HANDLE_EOF, ERROR_INVALID_FUNCTION].contains(&err) {
            return Ok(Vec::new());
        }
        return Err(std::io::Error::from_raw_os_error(err as i32).into());
    }
    let mut names = Vec::new();
    loop {
        let end = data
            .cStreamName
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(data.cStreamName.len());
        let name = String::from_utf16(&data.cStreamName[..end])?;
        if name != "::$DATA" {
            names.push(name);
        }
        if unsafe { FindNextStreamW(h, (&mut data as *mut WIN32_FIND_STREAM_DATA).cast()) } == 0 {
            let err = unsafe { GetLastError() };
            unsafe { FindClose(h) };
            if err != ERROR_HANDLE_EOF {
                return Err(std::io::Error::from_raw_os_error(err as i32).into());
            }
            break;
        }
    }
    names.sort();
    Ok(names)
}

fn valid_destination(source: &Path, destination: &Path) -> Result<()> {
    ensure!(destination.is_absolute(), "Destination must be absolute");
    paths::ensure_no_link_ancestors(destination)?;
    ensure!(
        source.file_name() == destination.file_name(),
        "Preserve the original filename or project folder name"
    );
    ensure!(
        !paths::within(destination, source),
        "Destination is unchanged or inside the source"
    );
    ensure!(
        fs::symlink_metadata(destination)
            .err()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "Destination exists or is inaccessible; choose another location"
    );
    paths::safe_filename(
        destination
            .file_name()
            .context("Destination needs a filename")?
            .to_str()
            .context("Non-Unicode destination")?,
    )?;
    Ok(())
}
fn catalog_destination(db: &Database, item: &Item, destination: &Path) -> Result<()> {
    for source in db.sources()? {
        if !paths::within(Path::new(&source.path), Path::new(&item.current_path)) {
            ensure!(
                !paths::within(Path::new(&source.path), destination),
                "Destination conflicts with a selected source directory"
            );
        }
    }
    for other in db.items()? {
        if !paths::within(
            Path::new(&other.current_path),
            Path::new(&item.current_path),
        ) {
            ensure!(
                !paths::within(Path::new(&other.current_path), destination),
                "Destination conflicts with an existing catalog path (including missing items)"
            );
        }
    }
    for op in summaries(db)? {
        if op.item_id != item.id
            && [
                "planned",
                "approved",
                "moving",
                "copying",
                "published",
                "removing",
                "recovery",
            ]
            .contains(&op.state.as_str())
            && db.item(op.item_id)?.kind == crate::catalog::Kind::Project
        {
            ensure!(
                !paths::within(destination, Path::new(&op.source)),
                "Destination is inside a project being moved"
            );
        }
    }
    Ok(())
}
fn invalidate_incoming(db: &Database, source: &Path, except: i64) -> Result<()> {
    for op in summaries(db)? {
        if op.id != except
            && ["planned", "approved"].contains(&op.state.as_str())
            && paths::within(Path::new(&op.destination), source)
        {
            state(
                db,
                op.id,
                "invalidated",
                "Destination is inside a project being moved",
            )?;
        }
    }
    Ok(())
}
pub(crate) fn verify_catalog(db: &Database, item: &Item, cancel: &AtomicBool) -> Result<()> {
    paths::ensure_no_link_ancestors(Path::new(&item.current_path))?;
    if item.kind == crate::catalog::Kind::Project {
        ensure!(
            crate::scanner::project_fingerprint(db, Path::new(&item.current_path), cancel)?
                == item.fingerprint,
            "Project changed; rescan first"
        );
    } else {
        let p = Path::new(&item.current_path);
        let fingerprint = format!(
            "{}:{}",
            paths::metadata_fingerprint(&fs::metadata(p)?),
            paths::hash_file(p, cancel)?
        );
        ensure!(
            fingerprint == item.fingerprint,
            "Source changed; rescan first"
        );
    }
    Ok(())
}
pub fn prepare(
    db: &Database,
    item_id: i64,
    destination: Option<&Path>,
    cancel: &AtomicBool,
) -> Result<i64> {
    let item = db.item(item_id)?;
    ensure!(
        item.project_id.is_none(),
        "Move the whole project, not its members"
    );
    ensure!(
        !item.missing && item.relevance == "relevant",
        "Only present relevant items can move"
    );
    ensure!(item.block_reason.is_empty(), "{}", item.block_reason);
    paths::ensure_no_link_ancestors(Path::new(&item.current_path))?;
    verify_catalog(db, &item, cancel)?;
    let category = db
        .categories()?
        .into_iter()
        .find(|c| c.id == item.category_id)
        .context("Category missing")?;
    paths::safe_component(&category.name)?;
    let source = Path::new(&item.current_path);
    let dest = if let Some(p) = destination {
        paths::absolute(p)?
    } else {
        let settings = db.settings()?;
        let base = settings
            .destinations
            .get(item.kind.as_str())
            .filter(|s| !s.is_empty())
            .context("Configure a destination for this file type")?;
        Path::new(base)
            .join(category.name)
            .join(source.file_name().context("Source filename")?)
    };
    let snapshot = manifest(source, cancel)?;
    db.invalidate_moves(item_id, "New destination prepared")?;
    let error = valid_destination(source, &dest)
        .and_then(|_| catalog_destination(db, &item, &dest))
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    db.conn.execute("INSERT INTO moves(item_id,source,destination,revision,category_revision,snapshot,state,error,created) VALUES(?,?,?,?,?,?,?, ?,?)",params![item_id,item.current_path,dest.to_string_lossy(),item.revision,db.category_epoch()?,serde_json::to_string(&snapshot)?,if error.is_empty(){"planned"}else{"blocked"},error,paths::now()])?;
    let id = db.conn.last_insert_rowid();
    state(
        db,
        id,
        if error.is_empty() {
            "planned"
        } else {
            "blocked"
        },
        &error,
    )?;
    if error.is_empty() && item.kind == crate::catalog::Kind::Project {
        invalidate_incoming(db, source, id)?;
    }
    Ok(id)
}
pub fn approve(db: &Database, id: i64, cancel: &AtomicBool) -> Result<()> {
    let op = operation(db, id)?;
    ensure!(op.state == "planned", "Prepare a valid destination first");
    let item = db.item(op.item_id)?;
    ensure!(
        item.revision == op.revision
            && item.current_path == op.source
            && item.relevance == "relevant"
            && !item.missing,
        "Item changed; prepare again"
    );
    ensure!(
        db.category_epoch()? == op.category_revision,
        "Categories changed; prepare again"
    );
    valid_destination(Path::new(&op.source), Path::new(&op.destination))?;
    catalog_destination(db, &item, Path::new(&op.destination))?;
    ensure!(
        manifest(Path::new(&op.source), cancel)? == op.snapshot,
        "Source changed after preparation"
    );
    state(db, id, "approved", "")
}
pub fn execute(db: &Database, id: i64, cancel: &AtomicBool) -> Result<()> {
    execute_mode(db, id, cancel, false)
}
// Explicit copy mode is also useful for testing the cross-volume path on one drive.
pub fn execute_mode(db: &Database, id: i64, cancel: &AtomicBool, copy_mode: bool) -> Result<()> {
    let op = operation(db, id)?;
    let result = (|| {
        ensure!(op.state == "approved", "Move was not approved");
        let item = db.item(op.item_id)?;
        ensure!(
            item.revision == op.revision && item.current_path == op.source && !item.missing,
            "Item changed"
        );
        if op.undo_of.is_none() {
            ensure!(
                item.relevance == "relevant"
                    && item.block_reason.is_empty()
                    && db.category_epoch()? == op.category_revision,
                "Item or filing categories changed"
            );
        }
        let source = Path::new(&op.source);
        let destination = Path::new(&op.destination);
        valid_destination(source, destination)?;
        catalog_destination(db, &item, destination)?;
        paths::ensure_no_link_ancestors(source)?;
        let mut source_locks = Some(lock_files(source, &op.snapshot)?);
        ensure!(
            manifest(source, cancel)? == op.snapshot,
            "Source changed after approval"
        );
        fs::create_dir_all(destination.parent().context("Destination parent")?)?;
        paths::ensure_no_link_ancestors(destination)?;
        state(db, id, "moving", "")?;
        if !copy_mode {
            if source.is_dir() {
                // Windows directory renames require closed descendant handles.
                // The rename itself is atomic; copy/removal retains writer locks throughout.
                drop(source_locks.take());
                ensure!(
                    manifest(source, cancel)? == op.snapshot,
                    "Source changed before rename"
                );
            }
            match rename_no_replace(source, destination) {
                Ok(()) => {
                    state(db, id, "published", "")?;
                    finish(db, &op)?;
                    return Ok(());
                }
                Err(e) => {
                    if !cross_device(&e) {
                        return Err(e.into());
                    }
                }
            }
        }
        if source_locks.is_none() {
            source_locks = Some(lock_files(source, &op.snapshot)?);
            ensure!(
                manifest(source, cancel)? == op.snapshot,
                "Source changed before copying"
            );
        }
        ensure!(
            !op.snapshot.has_links(),
            "Cross-volume moves containing links or junctions are blocked"
        );
        let temp = destination.parent().unwrap().join(format!(
            ".organizer-{}-{}.partial",
            id,
            uuid::Uuid::new_v4()
        ));
        db.conn.execute(
            "UPDATE moves SET state='copying',temp_path=? WHERE id=?",
            params![temp.to_string_lossy(), id],
        )?;
        state(db, id, "copying", "")?;
        copy_tree(source, &temp, &op.snapshot, cancel)?;
        ensure!(
            op.snapshot.content_matches(&manifest(&temp, cancel)?),
            "Destination verification failed; source retained"
        );
        ensure!(
            manifest(source, cancel)? == op.snapshot,
            "Source changed during copy; source retained"
        );
        rename_no_replace(&temp, destination)?;
        state(db, id, "published", "")?;
        let _destination_locks = lock_files(destination, &op.snapshot)?;
        paths::cancelled(cancel)?;
        ensure!(
            op.snapshot.content_matches(&manifest(destination, cancel)?),
            "Published destination failed verification; source retained"
        );
        ensure!(
            manifest(source, cancel)? == op.snapshot,
            "Source changed before removal; source retained"
        );
        state(db, id, "removing", "")?;
        remove_verified(source, &op.snapshot)?;
        finish(db, &op)?;
        drop(source_locks);
        Ok(())
    })();
    if let Err(error) = &result {
        let current = operation(db, id)?.state;
        if ["moving", "copying", "published", "removing"].contains(&current.as_str()) {
            state(db, id, "recovery", &format!("{error:#}"))?;
        } else if op.state == "approved" {
            state(db, id, "invalidated", &format!("{error:#}"))?;
        }
    }
    result
}
fn finish(db: &Database, op: &MoveOperation) -> Result<()> {
    catalog_destination(db, &db.item(op.item_id)?, Path::new(&op.destination))?;
    invalidate_incoming(db, Path::new(&op.source), op.id)?;
    let tx = db.conn.unchecked_transaction()?;
    for item in db.items()? {
        if paths::within(Path::new(&item.current_path), Path::new(&op.source)) {
            let relative = Path::new(&item.current_path).strip_prefix(&op.source)?;
            let current = at(
                Path::new(&op.destination),
                relative.to_str().context("Non-Unicode relative path")?,
            )
            .to_string_lossy()
            .into_owned();
            tx.execute(
                "UPDATE items SET current_path=?,path_key=? WHERE id=?",
                params![current, paths::key(&current), item.id],
            )?;
            tx.execute("UPDATE moves SET state='invalidated',error='Item moved' WHERE item_id=? AND id!=? AND state IN ('planned','approved')",params![item.id,op.id])?;
        }
    }
    for source in db.sources()? {
        if paths::within(Path::new(&source.path), Path::new(&op.source)) {
            let relative = Path::new(&source.path).strip_prefix(&op.source)?;
            let path = at(
                Path::new(&op.destination),
                relative.to_str().context("Non-Unicode relative path")?,
            )
            .to_string_lossy()
            .into_owned();
            tx.execute(
                "UPDATE sources SET path=?,path_key=? WHERE id=?",
                params![path, paths::key(&path), source.id],
            )?;
        }
    }
    tx.execute(
        "UPDATE moves SET state='completed',error='' WHERE id=?",
        [op.id],
    )?;
    tx.execute("INSERT INTO move_events(move_id,state,temp_path,error,created) SELECT id,state,temp_path,error,? FROM moves WHERE id=?",params![paths::now(),op.id])?;
    if let Some(previous) = op.undo_of {
        tx.execute("UPDATE moves SET state='undone' WHERE id=?", [previous])?;
    }
    tx.commit()?;
    Ok(())
}
pub fn undo(db: &Database, id: i64, cancel: &AtomicBool) -> Result<()> {
    let old = operation(db, id)?;
    ensure!(
        old.state == "completed" && old.undo_of.is_none(),
        "Only completed original moves can be undone"
    );
    let item = db.item(old.item_id)?;
    ensure!(
        item.current_path == old.destination,
        "Item moved again; undo its latest move first"
    );
    valid_destination(Path::new(&old.destination), Path::new(&old.source))?;
    catalog_destination(db, &item, Path::new(&old.source))?;
    let snapshot = manifest(Path::new(&old.destination), cancel)?;
    ensure!(
        old.snapshot.content_matches(&snapshot),
        "Moved content changed; undo blocked"
    );
    db.conn.execute("INSERT INTO moves(item_id,source,destination,revision,category_revision,snapshot,state,undo_of,created) VALUES(?,?,?,?,?,?,'approved',?,?)",params![item.id,old.destination,old.source,item.revision,db.category_epoch()?,serde_json::to_string(&snapshot)?,id,paths::now()])?;
    execute(db, db.conn.last_insert_rowid(), cancel)
}
pub fn recover(db: &Database, cancel: &AtomicBool) -> Result<()> {
    for op in operations(db)?.iter().filter(|o| {
        ["moving", "copying", "published", "removing", "recovery"].contains(&o.state.as_str())
    }) {
        let source = Path::new(&op.source);
        let destination = Path::new(&op.destination);
        if !source.exists()
            && destination.exists()
            && manifest(destination, cancel).is_ok_and(|m| op.snapshot.content_matches(&m))
        {
            if let Err(error) = finish(db, op) {
                state(
                    db,
                    op.id,
                    "recovery",
                    &format!("Move completed on disk but catalog reconciliation failed: {error:#}"),
                )?;
            }
        } else {
            state(
                db,
                op.id,
                "recovery",
                "Interrupted move: originals/copies retained. Inspect the recorded source, destination and temporary path before retrying.",
            )?;
        }
    }
    Ok(())
}
pub fn retry_recovery(db: &Database, id: i64, cancel: &AtomicBool) -> Result<()> {
    let op = operation(db, id)?;
    ensure!(op.state == "recovery", "Not a recoverable move");
    let source = Path::new(&op.source);
    let dest = Path::new(&op.destination);
    if dest.exists() {
        catalog_destination(db, &db.item(op.item_id)?, dest)?;
        ensure!(
            op.snapshot.content_matches(&manifest(dest, cancel)?),
            "Destination differs; retained for manual recovery"
        );
        let _destination_locks = lock_files(dest, &op.snapshot)?;
        ensure!(
            op.snapshot.content_matches(&manifest(dest, cancel)?),
            "Destination changed; source retained"
        );
        if source.exists() {
            let remaining = manifest(source, cancel)?;
            let _source_locks = lock_files(source, &remaining)?;
            ensure!(
                manifest(source, cancel)? == remaining,
                "Source changed; retained for manual recovery"
            );
            ensure!(
                remaining
                    .entries
                    .iter()
                    .all(
                        |entry| op.snapshot.entries.iter().any(|expected| entry.relative
                            == expected.relative
                            && entry.kind == expected.kind
                            && entry.hash == expected.hash
                            && entry.readonly == expected.readonly)
                    ),
                "Source differs; retained for manual recovery"
            );
            remove_verified(source, &remaining)?;
        }
        return finish(db, &op);
    }
    ensure!(
        manifest(source, cancel)? == op.snapshot,
        "Source differs; rescan and prepare again"
    );
    // A partial temporary copy is retained; retry uses a fresh temporary name.
    state(db, id, "approved", "")?;
    execute(db, id, cancel)
}

fn lock_files(root: &Path, snapshot: &Manifest) -> Result<Vec<fs::File>> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_READ};
        let mut locks = Vec::new();
        for entry in snapshot.entries.iter().filter(|e| e.kind == "file") {
            let p = at(root, &entry.relative);
            locks.push(
                fs::OpenOptions::new()
                    .read(true)
                    .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
                    .open(&p)
                    .with_context(|| {
                        format!(
                            "Close applications writing to {} before moving",
                            p.display()
                        )
                    })?,
            );
            for name in streams(&p)? {
                let mut stream = p.as_os_str().to_os_string();
                stream.push(name);
                locks.push(
                    fs::OpenOptions::new()
                        .read(true)
                        .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
                        .open(stream)?,
                );
            }
        }
        Ok(locks)
    }
    #[cfg(not(windows))]
    {
        let _ = (root, snapshot);
        Ok(Vec::new())
    }
}

fn copy_tree(source: &Path, dest: &Path, snapshot: &Manifest, cancel: &AtomicBool) -> Result<()> {
    ensure!(!snapshot.has_links(), "Copying links is not supported");
    for entry in &snapshot.entries {
        paths::cancelled(cancel)?;
        let from = at(source, &entry.relative);
        let to = at(dest, &entry.relative);
        if entry.kind == "directory" {
            fs::create_dir(&to)?;
        } else {
            copy_file(&from, &to)?;
            if fs::metadata(&to)?.permissions().readonly() {
                set_readonly(&to, false)?;
            }
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&to)?
                .sync_all()?;
        }
    }
    for entry in snapshot.entries.iter().rev() {
        let p = at(dest, &entry.relative);
        let t = filetime::FileTime::from_unix_time(
            (entry.modified_ns / 1_000_000_000) as i64,
            (entry.modified_ns % 1_000_000_000) as u32,
        );
        filetime::set_file_mtime(&p, t)?;
        set_readonly(&p, entry.readonly)?;
    }
    Ok(())
}
fn remove_verified(source: &Path, snapshot: &Manifest) -> Result<()> {
    ensure!(
        paths::absolute(source)? == source,
        "Removal requires a resolved absolute source"
    );
    paths::ensure_no_link_ancestors(source)?;
    ensure!(
        snapshot.content_matches(&manifest(source, &AtomicBool::new(false))?),
        "Source changed; retained"
    );
    // Remove individually rather than recursively: unrecorded files prevent directory removal.
    let mut entries = snapshot.entries.clone();
    entries.sort_by_key(|e| std::cmp::Reverse(Path::new(&e.relative).components().count()));
    for e in entries {
        let p = at(source, &e.relative);
        ensure!(paths::within(&p, source), "Removal escaped source");
        if e.kind == "file" {
            ensure!(
                hash_with_streams(&p, &AtomicBool::new(false))? == e.hash,
                "File changed; retained"
            );
            remove_file(&p)?;
        } else if e.kind == "directory" {
            fs::remove_dir(&p)?;
        } else {
            bail!("Cannot remove link during copied move")
        }
    }
    Ok(())
}
fn set_readonly(path: &Path, readonly: bool) -> Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = permissions.mode();
        permissions.set_mode(if readonly {
            mode & !0o222
        } else {
            mode | 0o200
        });
    }
    #[cfg(not(unix))]
    permissions.set_readonly(readonly);
    Ok(fs::set_permissions(path, permissions)?)
}
fn remove_file(path: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        let permissions = fs::metadata(path)?.permissions();
        if permissions.readonly() {
            set_readonly(path, false)?;
            if let Err(error) = fs::remove_file(path) {
                fs::set_permissions(path, permissions).context(
                    "Deletion failed and the original read-only attribute could not be restored",
                )?;
                return Err(error.into());
            }
            return Ok(());
        }
    }
    Ok(fs::remove_file(path)?)
}
#[cfg(windows)]
fn copy_file(source: &Path, dest: &Path) -> Result<()> {
    use windows_sys::Win32::Storage::FileSystem::CopyFileW;
    let s = wide(source);
    let d = wide(dest);
    if unsafe { CopyFileW(s.as_ptr(), d.as_ptr(), 1) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
#[cfg(not(windows))]
fn copy_file(source: &Path, dest: &Path) -> Result<()> {
    let mut from = fs::File::open(source)?;
    let mut to = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)?;
    std::io::copy(&mut from, &mut to)?;
    Ok(())
}
#[cfg(windows)]
pub fn rename_no_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};
    let s = wide(source);
    let d = wide(destination);
    if unsafe { MoveFileExW(s.as_ptr(), d.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}
#[cfg(not(windows))]
pub fn rename_no_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    if source.is_file() {
        fs::hard_link(source, destination)?;
        fs::remove_file(source)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Native directory moves currently require Windows",
        ))
    }
}
fn cross_device(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(if cfg!(windows) { 17 } else { 18 })
}
