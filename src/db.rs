use crate::{catalog::*, paths};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use std::{collections::BTreeMap, path::Path};

pub struct Database {
    pub conn: Connection,
}

const SCHEMA: &str = r#"
CREATE TABLE sources(id INTEGER PRIMARY KEY,path TEXT NOT NULL,path_key TEXT UNIQUE NOT NULL);
CREATE TABLE categories(id INTEGER PRIMARY KEY,name TEXT NOT NULL UNIQUE COLLATE NOCASE,revision INTEGER NOT NULL DEFAULT 1);
CREATE TABLE items(id INTEGER PRIMARY KEY,original_path TEXT NOT NULL,current_path TEXT NOT NULL,path_key TEXT UNIQUE NOT NULL,
 kind TEXT NOT NULL,project_id INTEGER REFERENCES items(id),fingerprint TEXT NOT NULL,revision INTEGER NOT NULL DEFAULT 1,
 relevance TEXT NOT NULL DEFAULT 'needs review',relevance_manual TEXT,category_id INTEGER NOT NULL REFERENCES categories(id),category_manual INTEGER NOT NULL DEFAULT 0,
 confidence REAL,status TEXT NOT NULL DEFAULT 'queued',missing INTEGER NOT NULL DEFAULT 0,block_reason TEXT NOT NULL DEFAULT '',
 content TEXT NOT NULL DEFAULT '',limitations TEXT NOT NULL DEFAULT '',error TEXT NOT NULL DEFAULT '');
CREATE INDEX item_review ON items(project_id,missing,relevance,status);
CREATE TABLE item_sources(item_id INTEGER REFERENCES items(id),source_id INTEGER REFERENCES sources(id) ON DELETE CASCADE,seen_scan INTEGER NOT NULL,PRIMARY KEY(item_id,source_id));
CREATE TABLE scans(id INTEGER PRIMARY KEY,started INTEGER NOT NULL,finished INTEGER,state TEXT NOT NULL,error TEXT NOT NULL DEFAULT '');
CREATE TABLE exclusions(path TEXT PRIMARY KEY,reason TEXT NOT NULL,scan_id INTEGER REFERENCES scans(id));
CREATE TABLE overrides(path_key TEXT PRIMARY KEY,path TEXT NOT NULL,include INTEGER NOT NULL);
CREATE TABLE tags(id INTEGER PRIMARY KEY,name TEXT NOT NULL UNIQUE COLLATE NOCASE,description TEXT NOT NULL,revision INTEGER NOT NULL DEFAULT 1);
CREATE TABLE item_tags(item_id INTEGER REFERENCES items(id),tag_id INTEGER REFERENCES tags(id),probability REAL,manual INTEGER,tag_revision INTEGER,PRIMARY KEY(item_id,tag_id));
CREATE TABLE suggestions(id INTEGER PRIMARY KEY,name TEXT NOT NULL UNIQUE COLLATE NOCASE,description TEXT NOT NULL,state TEXT NOT NULL DEFAULT 'pending');
CREATE TABLE jobs(id INTEGER PRIMARY KEY,item_id INTEGER NOT NULL REFERENCES items(id),revision INTEGER NOT NULL,kind TEXT NOT NULL,tags_json TEXT NOT NULL,category_revision INTEGER NOT NULL,
 state TEXT NOT NULL DEFAULT 'queued',error TEXT NOT NULL DEFAULT '',created INTEGER NOT NULL);
CREATE INDEX queued_jobs ON jobs(state,id);
CREATE TABLE analyses(id INTEGER PRIMARY KEY,item_id INTEGER REFERENCES items(id),revision INTEGER NOT NULL,kind TEXT NOT NULL,model TEXT NOT NULL,prompt_version TEXT NOT NULL,raw TEXT NOT NULL,created INTEGER NOT NULL);
CREATE TABLE moves(id INTEGER PRIMARY KEY,item_id INTEGER NOT NULL REFERENCES items(id),source TEXT NOT NULL,destination TEXT NOT NULL,revision INTEGER NOT NULL,
 category_revision INTEGER NOT NULL,snapshot TEXT NOT NULL,state TEXT NOT NULL DEFAULT 'planned',temp_path TEXT NOT NULL DEFAULT '',error TEXT NOT NULL DEFAULT '',undo_of INTEGER REFERENCES moves(id),created INTEGER NOT NULL);
CREATE INDEX item_moves ON moves(item_id,state);
CREATE TABLE previews(item_id INTEGER REFERENCES items(id),fingerprint TEXT NOT NULL,page INTEGER NOT NULL,width INTEGER NOT NULL,path TEXT NOT NULL,PRIMARY KEY(item_id,page,width));
CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);
ALTER TABLE items ADD COLUMN content_hash TEXT NOT NULL DEFAULT '';
CREATE TABLE suggestion_batches(id INTEGER PRIMARY KEY,model TEXT NOT NULL,prompt_version TEXT NOT NULL,raw TEXT NOT NULL,created INTEGER NOT NULL);
"#;
const HISTORY_AND_SEARCH: &str = r#"
CREATE TABLE item_revisions(item_id INTEGER REFERENCES items(id),revision INTEGER NOT NULL,fingerprint TEXT NOT NULL,content_hash TEXT NOT NULL,content TEXT NOT NULL,limitations TEXT NOT NULL,created INTEGER NOT NULL,PRIMARY KEY(item_id,revision));
CREATE TABLE move_events(id INTEGER PRIMARY KEY,move_id INTEGER REFERENCES moves(id),state TEXT NOT NULL,temp_path TEXT NOT NULL,error TEXT NOT NULL,created INTEGER NOT NULL);
CREATE VIRTUAL TABLE item_search USING fts5(current_path,original_path,content,content='items',content_rowid='id');
CREATE TRIGGER item_search_insert AFTER INSERT ON items BEGIN INSERT INTO item_search(rowid,current_path,original_path,content) VALUES(new.id,new.current_path,new.original_path,new.content); END;
CREATE TRIGGER item_search_update AFTER UPDATE OF current_path,original_path,content ON items BEGIN
 INSERT INTO item_search(item_search,rowid,current_path,original_path,content) VALUES('delete',old.id,old.current_path,old.original_path,old.content);
 INSERT INTO item_search(rowid,current_path,original_path,content) VALUES(new.id,new.current_path,new.original_path,new.content); END;
INSERT INTO item_search(item_search) VALUES('rebuild');
PRAGMA user_version=3;
"#;

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;",
        )?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version == 0 {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(SCHEMA)?;
            tx.execute_batch(HISTORY_AND_SEARCH)?;
            for name in [
                "Personal",
                "Work",
                "Finance",
                "Housing",
                "Health",
                "Education",
                "Reference",
                "Uncategorized",
            ] {
                tx.execute("INSERT INTO categories(name) VALUES(?)", [name])?;
            }
            tx.commit()?;
        } else if version == 1 || version == 2 {
            let tx = conn.unchecked_transaction()?;
            if version == 1 {
                tx.execute_batch("ALTER TABLE items ADD COLUMN content_hash TEXT NOT NULL DEFAULT ''; CREATE TABLE suggestion_batches(id INTEGER PRIMARY KEY,model TEXT NOT NULL,prompt_version TEXT NOT NULL,raw TEXT NOT NULL,created INTEGER NOT NULL);")?;
            }
            tx.execute_batch(HISTORY_AND_SEARCH)?;
            tx.commit()?;
        } else if version != 3 {
            bail!("Unsupported database schema {version}")
        }
        Ok(Self { conn })
    }
    pub fn recover_jobs(&self) -> Result<()> {
        self.conn.execute(
            "UPDATE jobs SET state='queued',error='' WHERE state='running'",
            [],
        )?;
        self.conn.execute(
            "UPDATE scans SET state='interrupted',finished=? WHERE state='running'",
            [paths::now()],
        )?;
        Ok(())
    }
    pub fn settings(&self) -> Result<Settings> {
        let v: Option<String> = self
            .conn
            .query_row("SELECT value FROM settings WHERE key='config'", [], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(v.map(|s| serde_json::from_str(&s))
            .transpose()?
            .unwrap_or_default())
    }
    pub fn save_settings(&self, s: &Settings) -> Result<()> {
        for url in [&s.decision_url, &s.generative_url] {
            let u = reqwest::Url::parse(url)?;
            if !["http", "https"].contains(&u.scheme()) {
                bail!("Endpoint must use HTTP or HTTPS")
            }
        }
        for dest in s.destinations.values() {
            if !dest.is_empty() && !Path::new(dest).is_absolute() {
                bail!("Destinations must be absolute")
            }
        }
        let tx = self.conn.unchecked_transaction()?;
        if self.settings()?.destinations != s.destinations {
            tx.execute("UPDATE moves SET state='invalidated',error='Destination settings changed' WHERE state IN ('planned','approved')",[])?;
        }
        tx.execute("INSERT INTO settings(key,value) VALUES('config',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[serde_json::to_string(s)?])?;
        tx.commit()?;
        Ok(())
    }
    pub fn add_source(&self, path: &Path) -> Result<()> {
        paths::ensure_no_link_ancestors(path)?;
        let p = paths::normalize(path)?;
        if !Path::new(&p).is_dir() {
            bail!("Choose a directory")
        }
        self.conn.execute(
            "INSERT OR IGNORE INTO sources(path,path_key) VALUES(?,?)",
            params![p, paths::key(&p)],
        )?;
        Ok(())
    }
    pub fn sources(&self) -> Result<Vec<Source>> {
        Ok(self
            .conn
            .prepare("SELECT id,path FROM sources ORDER BY path")?
            .query_map([], |r| {
                Ok(Source {
                    id: r.get(0)?,
                    path: r.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }
    pub fn remove_source(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM sources WHERE id=?", [id])?;
        Ok(())
    }
    pub fn categories(&self) -> Result<Vec<Category>> {
        Ok(self
            .conn
            .prepare("SELECT id,name,revision FROM categories ORDER BY id")?
            .query_map([], |r| {
                Ok(Category {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    revision: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }
    pub fn tags(&self) -> Result<Vec<Tag>> {
        Ok(self
            .conn
            .prepare("SELECT id,name,description,revision FROM tags ORDER BY name")?
            .query_map([], |r| {
                Ok(Tag {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    description: r.get(2)?,
                    revision: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }
    pub fn category_epoch(&self) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(SUM(revision),0) FROM categories",
            [],
            |r| r.get(0),
        )?)
    }
    pub fn save_category(&self, id: Option<i64>, name: &str) -> Result<()> {
        paths::safe_component(name)?;
        let tx = self.conn.unchecked_transaction()?;
        if let Some(id) = id {
            tx.execute(
                "UPDATE categories SET name=?,revision=revision+1 WHERE id=?",
                params![name, id],
            )?;
            tx.execute("UPDATE moves SET state='invalidated',error='Category edited' WHERE item_id IN (SELECT id FROM items WHERE category_id=?) AND state IN ('planned','approved')",[id])?;
        } else {
            tx.execute("INSERT INTO categories(name) VALUES(?)", [name])?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn item(&self, id: i64) -> Result<Item> {
        self.conn
            .query_row(
                &format!("{} WHERE id=?", Self::item_select()),
                [id],
                Self::read_item,
            )
            .context("Item not found")
    }
    pub fn item_at(&self, path: &str) -> Result<Option<Item>> {
        Ok(self
            .conn
            .query_row(
                &format!("{} WHERE path_key=?", Self::item_select()),
                [paths::key(path)],
                Self::read_item,
            )
            .optional()?)
    }
    pub fn items(&self) -> Result<Vec<Item>> {
        Ok(self
            .conn
            .prepare(&format!(
                "{} ORDER BY current_path",
                Self::item_select().replace(",content,limitations", ",'' AS content,limitations")
            ))?
            .query_map([], Self::read_item)?
            .collect::<rusqlite::Result<_>>()?)
    }
    fn item_select() -> &'static str {
        "SELECT id,original_path,current_path,kind,project_id,fingerprint,revision,relevance,relevance_manual,category_id,category_manual,confidence,status,missing,block_reason,content,limitations,error FROM items"
    }
    fn read_item(r: &rusqlite::Row<'_>) -> rusqlite::Result<Item> {
        Ok(Item {
            id: r.get(0)?,
            original_path: r.get(1)?,
            current_path: r.get(2)?,
            kind: Kind::parse(&r.get::<_, String>(3)?),
            project_id: r.get(4)?,
            fingerprint: r.get(5)?,
            revision: r.get(6)?,
            relevance: r.get(7)?,
            relevance_manual: r.get(8)?,
            category_id: r.get(9)?,
            category_manual: r.get(10)?,
            confidence: r.get(11)?,
            status: r.get(12)?,
            missing: r.get(13)?,
            block_reason: r.get(14)?,
            content: r.get(15)?,
            limitations: r.get(16)?,
            error: r.get(17)?,
        })
    }
    pub fn invalidate_moves(&self, id: i64, reason: &str) -> Result<()> {
        self.conn.execute("UPDATE moves SET state='invalidated',error=? WHERE item_id=? AND state IN ('planned','approved')",params![reason,id])?;
        Ok(())
    }
    // These fields and source associations are persisted as one catalog update.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_item(
        &self,
        path: &str,
        kind: Kind,
        project: Option<i64>,
        fingerprint: &str,
        block: &str,
        scan: i64,
        sources: &[Source],
    ) -> Result<(i64, bool)> {
        let old = self.item_at(path)?;
        let changed = old.as_ref().is_none_or(|i| {
            i.fingerprint != fingerprint
                || i.kind != kind
                || i.project_id != project
                || i.block_reason != block
                || i.missing
                || i.status == "excluded by rule"
        });
        let tx = self.conn.unchecked_transaction()?;
        let id = if let Some(old) = old {
            tx.execute("INSERT OR IGNORE INTO item_revisions(item_id,revision,fingerprint,content_hash,content,limitations,created) SELECT id,revision,fingerprint,content_hash,content,limitations,? FROM items WHERE id=?",params![paths::now(),old.id])?;
            if changed {
                tx.execute("UPDATE items SET fingerprint=?,kind=?,project_id=?,block_reason=?,revision=revision+1,content='',limitations='',error='',status='queued',confidence=NULL,missing=0 WHERE id=?",params![fingerprint,kind.as_str(),project,block,old.id])?;
                tx.execute(
                    "UPDATE item_tags SET probability=NULL,tag_revision=0 WHERE item_id=?",
                    [old.id],
                )?;
                if old.status == "excluded by rule" {
                    tx.execute("UPDATE items SET relevance=COALESCE(relevance_manual,'needs review') WHERE id=?",[old.id])?;
                }
                tx.execute("UPDATE moves SET state='invalidated',error='Source changed' WHERE item_id=? AND state IN ('planned','approved')",[old.id])?;
            } else {
                tx.execute("UPDATE items SET missing=0 WHERE id=?", [old.id])?;
            }
            old.id
        } else {
            tx.execute("INSERT INTO items(original_path,current_path,path_key,kind,project_id,fingerprint,block_reason,category_id) VALUES(?,?,?,?,?,?,?,8)",params![path,path,paths::key(path),kind.as_str(),project,fingerprint,block])?;
            tx.last_insert_rowid()
        };
        for source in sources {
            if paths::within(Path::new(path), Path::new(&source.path)) {
                tx.execute("INSERT INTO item_sources(item_id,source_id,seen_scan) VALUES(?,?,?) ON CONFLICT(item_id,source_id) DO UPDATE SET seen_scan=excluded.seen_scan",params![id,source.id,scan])?;
            }
        }
        let unreadable = block.starts_with("Cannot read");
        let content_hash = if unreadable {
            ""
        } else if kind == Kind::Project {
            fingerprint
        } else {
            fingerprint.rsplit(':').next().unwrap_or_default()
        };
        tx.execute(
            "UPDATE items SET content_hash=? WHERE id=?",
            params![content_hash, id],
        )?;
        if unreadable {
            tx.execute("UPDATE items SET status='needs review',relevance=COALESCE(relevance_manual,'needs review'),error=? WHERE id=?",params![block,id])?;
        }
        tx.execute("INSERT OR IGNORE INTO item_revisions(item_id,revision,fingerprint,content_hash,content,limitations,created) SELECT id,revision,fingerprint,content_hash,content,limitations,? FROM items WHERE id=?",params![paths::now(),id])?;
        tx.commit()?;
        if changed && project.is_none() && !unreadable {
            self.queue(id, "classify", &self.tags()?)?;
        }
        Ok((id, changed))
    }
    pub fn queue(&self, id: i64, kind: &str, tags: &[Tag]) -> Result<()> {
        let item = self.item(id)?;
        let json = serde_json::to_string(tags)?;
        let epoch = self.category_epoch()?;
        self.conn.execute("INSERT INTO jobs(item_id,revision,kind,tags_json,category_revision,created) SELECT ?,?,?,?,?,? WHERE NOT EXISTS(SELECT 1 FROM jobs WHERE item_id=? AND revision=? AND kind=? AND tags_json=? AND category_revision=? AND state IN ('queued','running'))",params![id,item.revision,kind,json,epoch,paths::now(),id,item.revision,kind,json,epoch])?;
        Ok(())
    }
    pub fn next_job(&self) -> Result<Option<Job>> {
        let tx = self.conn.unchecked_transaction()?;
        let row=tx.query_row("SELECT id,item_id,revision,kind,tags_json,category_revision FROM jobs WHERE state='queued' ORDER BY id LIMIT 1",[],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,i64>(5)?))).optional()?;
        let result = if let Some((id, item_id, revision, kind, tags, category_revision)) = row {
            tx.execute("UPDATE jobs SET state='running' WHERE id=?", [id])?;
            Some(Job {
                id,
                item_id,
                revision,
                kind,
                tags: serde_json::from_str(&tags)?,
                category_revision,
            })
        } else {
            None
        };
        tx.commit()?;
        Ok(result)
    }
    pub fn finish_job(&self, id: i64, state: &str, error: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE jobs SET state=?,error=? WHERE id=?",
            params![state, error, id],
        )?;
        Ok(())
    }
    pub fn store_content(
        &self,
        id: i64,
        revision: i64,
        text: &str,
        limitations: &str,
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE items SET content=?,limitations=? WHERE id=? AND revision=?",
            params![text, limitations, id, revision],
        )?;
        tx.execute("INSERT INTO item_revisions(item_id,revision,fingerprint,content_hash,content,limitations,created) SELECT id,revision,fingerprint,content_hash,content,limitations,? FROM items WHERE id=? AND revision=? ON CONFLICT(item_id,revision) DO UPDATE SET content=excluded.content,limitations=excluded.limitations",params![paths::now(),id,revision])?;
        tx.commit()?;
        Ok(())
    }
    pub fn search(&self, query: &str) -> Result<Vec<i64>> {
        let words: Vec<String> = query
            .chars()
            .take(500)
            .collect::<String>()
            .split_whitespace()
            .map(|w| format!("\"{}\"*", w.replace('"', "\"\"")))
            .collect();
        if words.is_empty() {
            return Ok(Vec::new());
        }
        Ok(self
            .conn
            .prepare("SELECT rowid FROM item_search WHERE item_search MATCH ?")?
            .query_map([words.join(" AND ")], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }
    pub fn apply_decision(&self, job: &Job, d: &Decision, model: &str) -> Result<bool> {
        let item = self.item(job.item_id)?;
        if item.revision != job.revision || item.missing {
            return Ok(false);
        }
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("INSERT INTO analyses(item_id,revision,kind,model,prompt_version,raw,created) VALUES(?,?,?,?,'organizer-v1',?,?)",params![item.id,item.revision,job.kind,model,d.raw,paths::now()])?;
        let stale_categories =
            job.kind == "classify" && job.category_revision != self.category_epoch()?;
        if job.kind == "classify" && !stale_categories {
            if let Some((value, p)) = &d.relevance
                && item.relevance_manual.is_none()
            {
                let v = if *p >= 0.8 {
                    value.as_str()
                } else {
                    "needs review"
                };
                if v != item.relevance {
                    tx.execute("UPDATE moves SET state='invalidated',error='Relevance classification changed' WHERE item_id=? AND state IN ('planned','approved')",[item.id])?;
                }
                tx.execute(
                    "UPDATE items SET relevance=? WHERE id=?",
                    params![v, item.id],
                )?;
            }
            if let Some((category, p)) = d.category
                && !item.category_manual
            {
                if category != item.category_id {
                    tx.execute("UPDATE moves SET state='invalidated',error='Classification changed' WHERE item_id=? AND state IN ('planned','approved')",[item.id])?;
                }
                tx.execute(
                    "UPDATE items SET category_id=?,confidence=? WHERE id=?",
                    params![category, p, item.id],
                )?;
            }
            tx.execute(
                "UPDATE items SET status='completed',error='' WHERE id=?",
                [item.id],
            )?;
        }
        for (tag, revision, p) in &d.tags {
            tx.execute("INSERT INTO item_tags(item_id,tag_id,probability,tag_revision) SELECT ?,id,?,revision FROM tags WHERE id=? AND revision=? ON CONFLICT(item_id,tag_id) DO UPDATE SET probability=excluded.probability,tag_revision=excluded.tag_revision",params![item.id,p,tag,revision])?;
        }
        tx.commit()?;
        if stale_categories {
            self.queue(item.id, "classify", &self.tags()?)?;
        }
        if self.item(item.id)?.relevance == "relevant" {
            let scored:Vec<(i64,i64)>=self.conn.prepare("SELECT tag_id,tag_revision FROM item_tags WHERE item_id=? AND probability IS NOT NULL")?.query_map([item.id],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
            let pending: Vec<Tag> = self
                .tags()?
                .into_iter()
                .filter(|t| {
                    !job.tags
                        .iter()
                        .any(|j| j.id == t.id && j.revision == t.revision)
                        && !scored.contains(&(t.id, t.revision))
                })
                .collect();
            if !pending.is_empty() {
                self.queue(item.id, "tags", &pending)?;
            }
        }
        Ok(true)
    }
    pub fn set_relevance(&self, id: i64, value: &str) -> Result<()> {
        if !["relevant", "excluded", "needs review"].contains(&value) {
            bail!("Invalid relevance")
        }
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE items SET relevance=?,relevance_manual=? WHERE id=?",
            params![value, value, id],
        )?;
        tx.execute("UPDATE moves SET state='invalidated',error='Relevance changed' WHERE item_id=? AND state IN ('planned','approved')",[id])?;
        tx.commit()?;
        Ok(())
    }
    pub fn set_category(&self, id: i64, category: i64) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE items SET category_id=?,category_manual=1 WHERE id=?",
            params![category, id],
        )?;
        tx.execute("UPDATE moves SET state='invalidated',error='Category changed' WHERE item_id=? AND state IN ('planned','approved')",[id])?;
        tx.commit()?;
        Ok(())
    }
    pub fn set_manual_tag(&self, item: i64, tag: i64, value: Option<bool>) -> Result<()> {
        self.conn.execute("INSERT INTO item_tags(item_id,tag_id,manual) VALUES(?,?,?) ON CONFLICT(item_id,tag_id) DO UPDATE SET manual=excluded.manual",params![item,tag,value])?;
        Ok(())
    }
    pub fn save_tag(&self, id: Option<i64>, name: &str, description: &str) -> Result<i64> {
        let tx = self.conn.unchecked_transaction()?;
        let id = self.save_tag_inner(id, name, description)?;
        tx.commit()?;
        Ok(id)
    }
    fn save_tag_inner(&self, id: Option<i64>, name: &str, description: &str) -> Result<i64> {
        paths::safe_component(name)?;
        if description.trim().is_empty() || description.chars().count() > 1000 {
            bail!("Provide a tag description (1–1000 characters)")
        }
        let id = if let Some(id) = id {
            self.conn.execute(
                "UPDATE tags SET name=?,description=?,revision=revision+1 WHERE id=?",
                params![name, description, id],
            )?;
            id
        } else {
            self.conn.execute(
                "INSERT INTO tags(name,description) VALUES(?,?)",
                params![name, description],
            )?;
            self.conn.last_insert_rowid()
        };
        let tag = self
            .tags()?
            .into_iter()
            .find(|t| t.id == id)
            .context("Tag missing")?;
        for item in self
            .items()?
            .iter()
            .filter(|i| i.project_id.is_none() && !i.missing && i.relevance == "relevant")
        {
            self.queue(item.id, "tags", std::slice::from_ref(&tag))?;
        }
        Ok(id)
    }
    pub fn save_suggestions(&self, values: &[(String, String)]) -> Result<()> {
        for (name, description) in values.iter().take(10) {
            paths::safe_component(name)?;
            if description.trim().is_empty() || description.chars().count() > 1000 {
                continue;
            }
            self.conn.execute("INSERT OR IGNORE INTO suggestions(name,description) SELECT ?,? WHERE NOT EXISTS(SELECT 1 FROM tags WHERE name=? COLLATE NOCASE)",params![name,description,name])?;
        }
        Ok(())
    }
    pub fn audit_suggestions(&self, model: &str, raw: &str) -> Result<()> {
        self.conn.execute("INSERT INTO suggestion_batches(model,prompt_version,raw,created) VALUES(?,'tag-suggestions-v1',?,?)",params![model,raw,paths::now()])?;
        Ok(())
    }
    pub fn accept_suggestion(&self, id: i64, name: &str, description: &str) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        if tx.execute(
            "UPDATE suggestions SET state='accepted' WHERE id=? AND state='pending'",
            [id],
        )? != 1
        {
            bail!("Suggestion is no longer pending")
        }
        self.save_tag_inner(None, name, description)?;
        tx.commit()?;
        Ok(())
    }
    pub fn reject_suggestion(&self, id: i64) -> Result<()> {
        self.conn
            .execute("UPDATE suggestions SET state='rejected' WHERE id=?", [id])?;
        Ok(())
    }
    pub fn item_tags(&self, id: i64) -> Result<Vec<ItemTag>> {
        Ok(self.conn.prepare("SELECT t.id,t.name,CASE WHEN a.tag_revision=t.revision THEN a.probability END,a.manual FROM tags t LEFT JOIN item_tags a ON a.tag_id=t.id AND a.item_id=? ORDER BY t.name")?.query_map([id],|r|Ok(ItemTag{tag_id:r.get(0)?,name:r.get(1)?,probability:r.get(2)?,manual:r.get(3)?}))?.collect::<rusqlite::Result<_>>()?)
    }
    pub fn set_override(&self, path: &str, include: bool) -> Result<()> {
        self.conn.execute("INSERT INTO overrides(path_key,path,include) VALUES(?,?,?) ON CONFLICT(path_key) DO UPDATE SET include=excluded.include",params![paths::key(path),path,include])?;
        Ok(())
    }
    pub fn override_for(&self, path: &Path) -> Result<Option<bool>> {
        for p in path.ancestors() {
            let v = self
                .conn
                .query_row(
                    "SELECT include FROM overrides WHERE path_key=?",
                    [paths::key(&p.to_string_lossy())],
                    |r| r.get(0),
                )
                .optional()?;
            if v.is_some() {
                return Ok(v);
            }
        }
        Ok(None)
    }
    pub fn included_descendants(&self, path: &Path) -> Result<Vec<std::path::PathBuf>> {
        let overrides = self
            .conn
            .prepare("SELECT path FROM overrides WHERE include=1")?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(overrides
            .into_iter()
            .map(std::path::PathBuf::from)
            .filter(|p| p != path && paths::within(p, path))
            .collect())
    }
    pub fn exclude_catalog_subtree(&self, path: &Path, reason: &str, link: bool) -> Result<()> {
        for item in self
            .items()?
            .iter()
            .filter(|i| paths::within(Path::new(&i.current_path), path))
        {
            if link || self.override_for(Path::new(&item.current_path))? != Some(true) {
                self.conn.execute("UPDATE items SET relevance='excluded',status='excluded by rule',error=?,revision=revision+CASE WHEN status='excluded by rule' THEN 0 ELSE 1 END WHERE id=?",params![reason,item.id])?;
                self.invalidate_moves(item.id, reason)?;
            }
        }
        Ok(())
    }
    pub fn snapshot(&self) -> Result<Snapshot> {
        let items = self.items()?;
        let mut item_tags = BTreeMap::new();
        let mut stmt=self.conn.prepare("SELECT a.item_id,t.id,t.name,CASE WHEN a.tag_revision=t.revision THEN a.probability END,a.manual FROM item_tags a JOIN tags t ON t.id=a.tag_id")?;
        for row in stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                ItemTag {
                    tag_id: r.get(1)?,
                    name: r.get(2)?,
                    probability: r.get(3)?,
                    manual: r.get(4)?,
                },
            ))
        })? {
            let (id, t) = row?;
            item_tags.entry(id).or_insert_with(Vec::new).push(t);
        }
        let suggestions = self
            .conn
            .prepare("SELECT id,name,description FROM suggestions WHERE state='pending'")?
            .query_map([], |r| {
                Ok(Suggestion {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    description: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        let exclusions = self
            .conn
            .prepare("SELECT path,reason FROM exclusions ORDER BY path")?
            .query_map([], |r| {
                Ok(Exclusion {
                    path: r.get(0)?,
                    reason: r.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        let counts = self
            .conn
            .prepare("SELECT state,COUNT(*) FROM jobs GROUP BY state")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let thumbnails=self.conn.prepare("SELECT p.item_id,p.path FROM previews p JOIN items i ON i.id=p.item_id AND i.fingerprint=p.fingerprint WHERE p.page=0 AND p.width=64")?.query_map([],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        Ok(Snapshot {
            items,
            sources: self.sources()?,
            tags: self.tags()?,
            categories: self.categories()?,
            suggestions,
            exclusions,
            item_tags,
            moves: crate::moves::summaries(self)?,
            counts,
            thumbnails,
            settings: self.settings()?,
        })
    }
}
