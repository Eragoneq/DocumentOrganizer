use crate::{
    catalog::*,
    content::{self, Preview},
    db::Database,
    model::Models,
    moves, paths, scanner,
};
use anyhow::{Context, Result};
use rusqlite::params;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::Duration,
};

#[derive(Debug, Clone)]
pub enum Command {
    Refresh,
    Details(i64),
    AddSource(PathBuf),
    RemoveSource(i64),
    Scan,
    Retry,
    Relevance(i64, String),
    Override(String, bool),
    Category(i64, i64),
    ManualTag(i64, i64, Option<bool>),
    SaveTag(Option<i64>, String, String),
    SaveCategory(Option<i64>, String),
    Suggest,
    AcceptSuggestion(i64, String, String),
    RejectSuggestion(i64),
    Settings(Settings),
    TestDecision(Settings),
    TestGenerative(Settings),
    Prepare(Vec<i64>),
    Destination(i64, PathBuf),
    Approve(Vec<i64>),
    Apply,
    Undo(i64),
    Recover(i64),
    Shutdown,
}
pub enum Event {
    Snapshot(Box<Snapshot>),
    Details(Box<Item>),
    Progress(String),
    Error(String),
    Preview(Preview),
    Thumbnail(i64, i64, Option<PathBuf>),
    Search(String, Vec<i64>),
}
#[derive(Clone)]
pub struct Worker {
    sender: Sender<Command>,
    pub cancel: Arc<AtomicBool>,
    preview_sender: Sender<(Item, u16, u32, Settings, Option<Item>)>,
    thumbnail_sender: mpsc::SyncSender<(Item, Settings)>,
    search_sender: Sender<String>,
}
impl Worker {
    pub fn start(data: &Path) -> Result<(Self, Receiver<Event>)> {
        std::fs::create_dir_all(data)?;
        let database = data.join("catalog.sqlite3");
        let db = Database::open(&database)?;
        db.recover_jobs()?;
        moves::recover(&db, &AtomicBool::new(false))?;
        let initial = db.snapshot()?;
        drop(db);
        let (tx, rx) = mpsc::channel();
        let (events, receiver) = mpsc::channel();
        events.send(Event::Snapshot(Box::new(initial)))?;
        let cancel = Arc::new(AtomicBool::new(false));
        let thread_cancel = cancel.clone();
        let thread_events = events.clone();
        thread::Builder::new()
            .name("catalog-worker".into())
            .spawn(move || {
                let run = (|| -> Result<()> {
                    let db = Database::open(&database)?;
                    let mut analysis_paused = false;
                    loop {
                        if thread_cancel.load(Ordering::Relaxed) {
                            analysis_paused = true;
                        }
                        match rx.recv_timeout(Duration::from_millis(100)) {
                            Ok(Command::Shutdown) => break,
                            Ok(command) => {
                                if matches!(command, Command::Retry | Command::Scan) {
                                    analysis_paused = false;
                                }
                                thread_cancel.store(false, Ordering::Relaxed);
                                let result = handle(&db, command, &thread_cancel, &thread_events);
                                if let Err(e) = result {
                                    let _ = thread_events.send(Event::Error(format!("{e:#}")));
                                }
                                if let Ok(s) = db.snapshot() {
                                    let _ = thread_events.send(Event::Snapshot(Box::new(s)));
                                }
                            }
                            Err(mpsc::RecvTimeoutError::Disconnected) => break,
                            Err(mpsc::RecvTimeoutError::Timeout) => {
                                if !analysis_paused
                                    && !thread_cancel.load(Ordering::Relaxed)
                                    && let Some(job) = db.next_job()?
                                {
                                    analysis_paused =
                                        !process_job(&db, &job, &thread_cancel, &thread_events)?;
                                    if let Ok(s) = db.snapshot() {
                                        let _ = thread_events.send(Event::Snapshot(Box::new(s)));
                                    }
                                }
                            }
                        }
                    }
                    Ok(())
                })();
                if let Err(e) = run {
                    let _ = thread_events.send(Event::Error(format!("Worker stopped: {e:#}")));
                }
            })?;
        let (stx, srx) = mpsc::channel::<String>();
        let search_events = events.clone();
        let search_database = data.join("catalog.sqlite3");
        thread::Builder::new()
            .name("search-worker".into())
            .spawn(move || {
                while let Ok(mut query) = srx.recv() {
                    while let Ok(newer) = srx.try_recv() {
                        query = newer;
                    }
                    if let Ok(db) = Database::open(&search_database) {
                        match db.search(&query) {
                            Ok(ids) => {
                                let _ = search_events.send(Event::Search(query, ids));
                            }
                            Err(e) => {
                                let _ = search_events.send(Event::Error(format!("Search: {e:#}")));
                            }
                        }
                    }
                }
            })?;
        let (ptx, prx) = mpsc::channel::<(Item, u16, u32, Settings, Option<Item>)>();
        let cache = data.join("previews");
        let (ttx, trx) = mpsc::sync_channel::<(Item, Settings)>(32);
        let thumbnail_events = events.clone();
        let thumbnail_cache = cache.clone();
        thread::Builder::new().name("thumbnail-worker".into()).spawn(move||{
            while let Ok((item,settings))=trx.recv(){
                let output=content::preview(&item,0,64,&settings,&thumbnail_cache).ok().and_then(|p|p.path);
                if let Some(p)=&output && let Ok(db)=Database::open(&data_path_for_preview(&thumbnail_cache)){let _=db.conn.execute("INSERT INTO previews(item_id,fingerprint,page,width,path) VALUES(?,?,0,64,?) ON CONFLICT(item_id,page,width) DO UPDATE SET fingerprint=excluded.fingerprint,path=excluded.path",params![item.id,item.fingerprint,p.to_string_lossy()]);}
                let _=thumbnail_events.send(Event::Thumbnail(item.id,item.revision,output));
            }
        })?;
        thread::Builder::new().name("preview-worker".into()).spawn(move||{
            while let Ok(mut request)=prx.recv(){while let Ok(newer)=prx.try_recv(){request=newer;}
                let(item,page,width,settings,next)=request;
                if let Ok(db)=Database::open(&data_path_for_preview(&cache)) && let Ok(details)=db.item(item.id){let _=events.send(Event::Details(Box::new(details)));}
                let result=content::preview(&item,page,width,&settings,&cache).unwrap_or_else(|e|Preview{item_id:item.id,revision:item.revision,path:None,page,pages:0,error:format!("{e:#}")});
                if let Some(p)=&result.path && let Ok(db)=Database::open(&data_path_for_preview(&cache)){let _=db.conn.execute("INSERT INTO previews(item_id,fingerprint,page,width,path) VALUES(?,?,?,?,?) ON CONFLICT(item_id,page,width) DO UPDATE SET fingerprint=excluded.fingerprint,path=excluded.path",params![item.id,item.fingerprint,page,width,p.to_string_lossy()]);}
                let _=events.send(Event::Preview(result));
                if let Some(next)=next{let _=content::preview(&next,0,width,&settings,&cache);}
            }
        })?;
        Ok((
            Self {
                sender: tx,
                cancel,
                preview_sender: ptx,
                thumbnail_sender: ttx,
                search_sender: stx,
            },
            receiver,
        ))
    }
    pub fn send(&self, command: Command) {
        let _ = self.sender.send(command);
    }
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
    pub fn preview(
        &self,
        item: Item,
        page: u16,
        width: u32,
        settings: Settings,
        next: Option<Item>,
    ) {
        let _ = self
            .preview_sender
            .send((item, page, width, settings, next));
    }
    pub fn search(&self, query: String) {
        let _ = self.search_sender.send(query);
    }
    pub fn thumbnail(&self, item: Item, settings: Settings) -> bool {
        self.thumbnail_sender.try_send((item, settings)).is_ok()
    }
}
fn data_path_for_preview(cache: &Path) -> PathBuf {
    cache.parent().unwrap_or(cache).join("catalog.sqlite3")
}

fn handle(
    db: &Database,
    command: Command,
    cancel: &AtomicBool,
    events: &Sender<Event>,
) -> Result<()> {
    match command {
        Command::Refresh => {}
        Command::Details(id) => {
            events.send(Event::Details(Box::new(db.item(id)?)))?;
        }
        Command::AddSource(p) => db.add_source(&p)?,
        Command::RemoveSource(id) => db.remove_source(id)?,
        Command::Scan => {
            events.send(Event::Progress("Scanning directories…".into()))?;
            let n = scanner::scan(db, cancel, |s| {
                let _ = events.send(Event::Progress(s.into()));
            })?;
            events.send(Event::Progress(format!(
                "Scan complete: {n} files; analysis queued"
            )))?;
        }
        Command::Retry => {
            db.conn.execute(
                "UPDATE jobs SET state='queued',error='' WHERE state IN ('failed','cancelled')",
                [],
            )?;
        }
        Command::Relevance(id, r) => {
            db.set_relevance(id, &r)?;
            db.set_override(&db.item(id)?.current_path, r != "excluded")?;
            if r == "relevant" {
                db.queue(id, "classify", &db.tags()?)?;
            }
        }
        Command::Override(p, v) => db.set_override(&p, v)?,
        Command::Category(id, c) => db.set_category(id, c)?,
        Command::ManualTag(i, t, v) => db.set_manual_tag(i, t, v)?,
        Command::SaveTag(id, n, d) => {
            db.save_tag(id, &n, &d)?;
        }
        Command::SaveCategory(id, n) => db.save_category(id, &n)?,
        Command::Suggest => {
            let items = db
                .items()?
                .into_iter()
                .filter(|i| i.project_id.is_none() && !i.missing && i.relevance == "relevant")
                .take(20)
                .map(|i| db.item(i.id))
                .collect::<Result<Vec<_>>>()?;
            let settings = db.settings()?;
            let (values, raw) =
                Models::new(settings.clone())?.suggest_with_audit(&items, &db.tags()?, cancel)?;
            db.audit_suggestions(&settings.generative_model, &raw)?;
            db.save_suggestions(&values)?;
        }
        Command::AcceptSuggestion(id, n, d) => db.accept_suggestion(id, &n, &d)?,
        Command::RejectSuggestion(id) => db.reject_suggestion(id)?,
        Command::Settings(s) => db.save_settings(&s)?,
        Command::TestDecision(s) => {
            db.save_settings(&s)?;
            Models::new(s)?.test_decision(cancel)?;
            events.send(Event::Progress(
                "Decision endpoint accepts text and images".into(),
            ))?;
        }
        Command::TestGenerative(s) => {
            db.save_settings(&s)?;
            Models::new(s)?.test_generative(cancel)?;
            events.send(Event::Progress("Generative endpoint connected".into()))?;
        }
        Command::Prepare(ids) => {
            for id in ids {
                paths::cancelled(cancel)?;
                if let Err(e) = moves::prepare(db, id, None, cancel) {
                    events.send(Event::Error(format!("Item {id}: {e:#}")))?;
                }
            }
        }
        Command::Destination(id, p) => {
            moves::prepare(db, id, Some(&p), cancel)?;
        }
        Command::Approve(ids) => {
            for id in ids {
                moves::approve(db, id, cancel)?;
            }
        }
        Command::Apply => {
            for op in moves::operations(db)?
                .iter()
                .rev()
                .filter(|o| o.state == "approved")
            {
                paths::cancelled(cancel)?;
                if let Err(e) = moves::execute(db, op.id, cancel) {
                    events.send(Event::Error(format!("Move {}: {e:#}", op.id)))?;
                }
            }
        }
        Command::Undo(id) => moves::undo(db, id, cancel)?,
        Command::Recover(id) => moves::retry_recovery(db, id, cancel)?,
        Command::Shutdown => {}
    }
    Ok(())
}
fn process_job(
    db: &Database,
    job: &Job,
    cancel: &AtomicBool,
    events: &Sender<Event>,
) -> Result<bool> {
    let result = (|| -> Result<()> {
        let mut item = db.item(job.item_id)?;
        if item.revision != job.revision || item.missing || item.project_id.is_some() {
            db.finish_job(job.id, "stale", "")?;
            return Ok(());
        }
        events.send(Event::Progress(format!("Analyzing {}", item.current_path)))?;
        moves::verify_catalog(db, &item, cancel)?;
        let settings = db.settings()?;
        if job.kind == "classify" {
            let e = content::extract(db, &item, &settings, cancel).unwrap_or_else(|e| {
                content::Extracted {
                    text: String::new(),
                    limitations: format!("Extraction failed: {e:#}"),
                }
            });
            db.store_content(item.id, item.revision, &e.text, &e.limitations)?;
            item.content = e.text;
            item.limitations = e.limitations;
        }
        let model_version = settings.decision_model_version.clone();
        let mut decision = Models::new(settings)?.classify(
            &item,
            &db.categories()?,
            &job.tags,
            job.kind == "tags",
            cancel,
        )?;
        if job.kind == "classify"
            && (item.limitations.starts_with("Extraction failed")
                || item.limitations.starts_with("No readable PDF"))
            && item.relevance_manual.is_none()
        {
            decision.relevance = Some(("needs review".into(), 1.0));
        }
        moves::verify_catalog(db, &item, cancel)?;
        db.apply_decision(job, &decision, &model_version)?;
        db.conn.execute("UPDATE items SET status='completed',error='' WHERE id=? AND revision=? AND status IN ('failed','cancelled')",params![item.id,item.revision])?;
        db.finish_job(job.id, "completed", "")?;
        Ok(())
    })();
    if let Err(e) = result {
        let stopped = cancel.load(Ordering::Relaxed);
        db.finish_job(
            job.id,
            if stopped { "cancelled" } else { "failed" },
            &format!("{e:#}"),
        )?;
        db.conn.execute(
            "UPDATE items SET status=?,error=? WHERE id=? AND revision=?",
            params![
                if stopped { "cancelled" } else { "failed" },
                format!("{e:#}"),
                job.item_id,
                job.revision
            ],
        )?;
        events
            .send(Event::Error(format!(
                "Analysis paused: {e:#}. Use Retry to resume."
            )))
            .context("UI disconnected")?;
        return Ok(false);
    }
    Ok(true)
}
