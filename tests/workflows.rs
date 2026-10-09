use document_organizer::{catalog::*, content, db::Database, model, moves, paths, scanner};
use rusqlite::params;
use serde_json::json;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};
use tempfile::TempDir;

struct Fixture {
    _temp: TempDir,
    db: Database,
    source: PathBuf,
    dest: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let dest = temp.path().join("destination");
        fs::create_dir(&source).unwrap();
        let db = Database::open(&temp.path().join("catalog.sqlite3")).unwrap();
        db.add_source(&source).unwrap();
        Self {
            _temp: temp,
            db,
            source,
            dest,
        }
    }
    fn write(&self, relative: &str, bytes: &[u8]) -> PathBuf {
        let p = self.source.join(relative);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, bytes).unwrap();
        p
    }
    fn scan(&self) {
        scanner::scan(&self.db, &AtomicBool::new(false), |_| {}).unwrap();
    }
    fn item(&self, p: &Path) -> Item {
        self.db
            .item_at(&paths::normalize(p).unwrap())
            .unwrap()
            .unwrap()
    }
    fn ready(&self, p: &Path) -> i64 {
        self.scan();
        let item = self.item(p);
        self.db.set_relevance(item.id, "relevant").unwrap();
        item.id
    }
    fn planned(&self, p: &Path) -> (i64, i64, PathBuf) {
        let item = self.ready(p);
        let dest = self.dest.join(p.file_name().unwrap());
        let op = moves::prepare(&self.db, item, Some(&dest), &AtomicBool::new(false)).unwrap();
        (item, op, dest)
    }
}
fn cancel() -> AtomicBool {
    AtomicBool::new(false)
}

#[test]
fn overlapping_sources_and_unchanged_rescans_keep_ids_and_jobs() {
    let f = Fixture::new();
    let p = f.write("nested/note.txt", b"Personal note");
    f.db.add_source(&f.source.join("nested")).unwrap();
    f.scan();
    let item = f.item(&p);
    let jobs: i64 =
        f.db.conn
            .query_row("SELECT COUNT(*) FROM jobs", [], |r| r.get(0))
            .unwrap();
    f.scan();
    assert_eq!(f.item(&p).id, item.id);
    assert_eq!(f.item(&p).revision, item.revision);
    assert_eq!(f.db.items().unwrap().len(), 1);
    assert_eq!(
        f.db.conn
            .query_row::<i64, _, _>("SELECT COUNT(*) FROM jobs", [], |r| r.get(0))
            .unwrap(),
        jobs
    );
    assert!(p.exists());
}
#[test]
fn project_unit_prunes_dependencies_but_keeps_authored_members() {
    let f = Fixture::new();
    let manifest = f.write("app/Cargo.toml", b"[package]\nname='demo'\n");
    let source = f.write("app/src/main.rs", b"fn main() {}");
    let generated = f.write("app/target/debug/large.bin", b"generated");
    f.scan();
    let items = f.db.items().unwrap();
    let project = items.iter().find(|i| i.kind == Kind::Project).unwrap();
    assert_eq!(f.item(&source).project_id, Some(project.id));
    assert!(items.iter().all(|i| !i.current_path.contains("large.bin")));
    assert!(
        f.db.snapshot()
            .unwrap()
            .exclusions
            .iter()
            .any(|e| e.path.ends_with("target"))
    );
    assert!(manifest.exists() && generated.exists());
    f.db.set_override(generated.parent().unwrap().to_str().unwrap(), true)
        .unwrap();
    f.scan();
    assert!(f.item(&generated).project_id.is_some());
}
#[test]
fn partial_projects_cannot_be_moved() {
    let f = Fixture::new();
    f.write("app/Cargo.toml", b"[package]");
    f.write("app/src/main.rs", b"fn main() {}");
    for source in f.db.sources().unwrap() {
        f.db.remove_source(source.id).unwrap();
    }
    f.db.add_source(&f.source.join("app/src")).unwrap();
    f.scan();
    let project =
        f.db.items()
            .unwrap()
            .into_iter()
            .find(|i| i.kind == Kind::Project)
            .unwrap();
    assert!(project.block_reason.contains("Partial project"));
    f.db.set_relevance(project.id, "relevant").unwrap();
    assert!(moves::prepare(&f.db, project.id, Some(&f.dest), &cancel()).is_err());
}
#[test]
fn missing_files_keep_original_paths_and_history() {
    let f = Fixture::new();
    let p = f.write("gone.txt", b"note");
    f.scan();
    let id = f.item(&p).id;
    fs::remove_file(&p).unwrap();
    f.scan();
    let item = f.db.item(id).unwrap();
    assert!(item.missing);
    assert_eq!(
        item.original_path,
        paths::absolute(&p).unwrap().to_string_lossy()
    );
}
#[test]
fn cancellation_does_not_touch_sources() {
    let f = Fixture::new();
    let p = f.write("note.txt", b"unchanged");
    let c = AtomicBool::new(true);
    assert!(scanner::scan(&f.db, &c, |_| {}).is_err());
    assert_eq!(fs::read(&p).unwrap(), b"unchanged");
    assert_eq!(
        f.db.conn
            .query_row::<String, _, _>(
                "SELECT state FROM scans ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0)
            )
            .unwrap(),
        "cancelled"
    );
}
#[test]
fn manual_tags_survive_backfill_and_category_is_unchanged() {
    let f = Fixture::new();
    let p = f.write("note.txt", b"Work invoice");
    let id = f.ready(&p);
    let category = f.db.categories().unwrap()[2].id;
    f.db.set_category(id, category).unwrap();
    let t =
        f.db.save_tag(None, "Invoices", "Bills requesting payment")
            .unwrap();
    f.db.set_manual_tag(id, t, Some(false)).unwrap();
    let item = f.db.item(id).unwrap();
    let tag = f.db.tags().unwrap()[0].clone();
    let job = Job {
        id: 0,
        item_id: id,
        revision: item.revision,
        kind: "tags".into(),
        tags: vec![tag.clone()],
        category_revision: f.db.category_epoch().unwrap(),
    };
    let decision = Decision {
        relevance: Some(("excluded".into(), 1.)),
        category: Some((1, 1.)),
        tags: vec![(t, tag.revision, 0.99)],
        raw: "fixture".into(),
    };
    f.db.apply_decision(&job, &decision, "test").unwrap();
    let tag = f.db.item_tags(id).unwrap().into_iter().next().unwrap();
    assert!(!tag.assigned());
    assert_eq!(tag.manual, Some(false));
    assert_eq!(f.db.item(id).unwrap().category_id, category);
    assert_eq!(f.db.item(id).unwrap().relevance, "relevant");
    let queued: i64 =
        f.db.conn
            .query_row(
                "SELECT COUNT(*) FROM jobs WHERE kind='tags' AND item_id=?",
                [id],
                |r| r.get(0),
            )
            .unwrap();
    assert!(queued > 0);
}
#[test]
fn stale_tag_and_content_revisions_cannot_overwrite_current_results() {
    let f = Fixture::new();
    let p = f.write("note.txt", b"Work");
    let id = f.ready(&p);
    let t = f.db.save_tag(None, "Work", "Employment").unwrap();
    let old = f.db.tags().unwrap()[0].clone();
    f.db.save_tag(Some(t), "Work", "Authored work projects")
        .unwrap();
    let item = f.db.item(id).unwrap();
    let job = Job {
        id: 0,
        item_id: id,
        revision: item.revision,
        kind: "tags".into(),
        tags: vec![old.clone()],
        category_revision: 0,
    };
    let d = Decision {
        relevance: None,
        category: None,
        tags: vec![(t, old.revision, 0.9)],
        raw: "old".into(),
    };
    f.db.apply_decision(&job, &d, "test").unwrap();
    assert_eq!(f.db.item_tags(id).unwrap()[0].probability, None);
    fs::write(&p, b"changed content").unwrap();
    f.scan();
    assert!(!f.db.apply_decision(&job, &d, "test").unwrap());
}
#[test]
fn tag_backfill_does_not_invalidate_approved_destination() {
    let f = Fixture::new();
    let p = f.write("note.txt", b"Work");
    let (_, op, _) = f.planned(&p);
    moves::approve(&f.db, op, &cancel()).unwrap();
    f.db.save_tag(None, "Work", "Employment").unwrap();
    assert_eq!(moves::operation(&f.db, op).unwrap().state, "approved");
}
#[test]
fn endpoint_contract_rejects_unknown_choices_and_invalid_probabilities() {
    let categories = vec![Category {
        id: 1,
        name: "Personal".into(),
        revision: 1,
    }];
    let tags = vec![Tag {
        id: 1,
        name: "Work".into(),
        description: "Employment".into(),
        revision: 1,
    }];
    let v = json!({"answers":{"relevance":{"choice":"relevant","confidence":0.9},"category":{"choice":"category_1","confidence":0.8},"tag_1":{"noul":0.75}}});
    assert_eq!(
        model::parse_decision(&v, &categories, &tags, true)
            .unwrap()
            .tags[0]
            .2,
        0.75
    );
    let mut bad = v.clone();
    bad["answers"]["tag_1"]["noul"] = json!(1.2);
    assert!(model::parse_decision(&bad, &categories, &tags, true).is_err());
    bad = v;
    bad["answers"]["category"]["choice"] = json!("category_999");
    assert!(model::parse_decision(&bad, &categories, &tags, true).is_err());
}
#[test]
fn suggested_tags_are_bounded_deduplicated_and_inert_until_accepted() {
    let f = Fixture::new();
    let p = f.write("note.txt", b"invoice");
    let id = f.ready(&p);
    let tags=model::parse_suggestions(r#"{"tags":[{"name":"Invoice","description":"Bills"},{"name":"invoice","description":"Duplicate"}]}"#).unwrap();
    assert_eq!(tags.len(), 1);
    f.db.save_suggestions(&tags).unwrap();
    assert!(f.db.tags().unwrap().is_empty());
    let s = f.db.snapshot().unwrap().suggestions[0].clone();
    f.db.accept_suggestion(s.id, &s.name, &s.description)
        .unwrap();
    assert_eq!(f.db.tags().unwrap().len(), 1);
    assert!(
        f.db.conn
            .query_row::<i64, _, _>(
                "SELECT COUNT(*) FROM jobs WHERE kind='tags' AND item_id=?",
                [id],
                |r| r.get(0)
            )
            .unwrap()
            > 0
    );
    assert!(
        model::parse_suggestions(r#"{"tags":[{"name":"../escape","description":"bad"}]}"#).is_err()
    );
}
#[test]
fn recovery_is_explicit_and_preview_connections_do_not_reset_running_jobs() {
    let f = Fixture::new();
    f.write("note.txt", b"note");
    f.scan();
    let job = f.db.next_job().unwrap().unwrap();
    let other = Database::open(&f._temp.path().join("catalog.sqlite3")).unwrap();
    assert_eq!(
        other
            .conn
            .query_row::<String, _, _>("SELECT state FROM jobs WHERE id=?", [job.id], |r| r.get(0))
            .unwrap(),
        "running"
    );
    other.recover_jobs().unwrap();
    assert_eq!(
        other
            .conn
            .query_row::<String, _, _>("SELECT state FROM jobs WHERE id=?", [job.id], |r| r.get(0))
            .unwrap(),
        "queued"
    );
}
#[test]
fn source_change_invalidates_move_without_touching_destination() {
    let f = Fixture::new();
    let p = f.write("note.txt", b"original");
    let (_, op, dest) = f.planned(&p);
    moves::approve(&f.db, op, &cancel()).unwrap();
    fs::write(&p, b"new data").unwrap();
    assert!(moves::execute(&f.db, op, &cancel()).is_err());
    assert!(p.exists() && !dest.exists());
    assert_eq!(moves::operation(&f.db, op).unwrap().state, "invalidated");
}
#[test]
fn unapproved_moves_and_collisions_never_overwrite() {
    let f = Fixture::new();
    let p = f.write("note.txt", b"original");
    let (_, op, dest) = f.planned(&p);
    assert!(moves::execute(&f.db, op, &cancel()).is_err());
    assert!(p.exists() && !dest.exists());
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    fs::write(&dest, b"existing").unwrap();
    assert!(moves::approve(&f.db, op, &cancel()).is_err());
    assert_eq!(fs::read(&dest).unwrap(), b"existing");
}
#[test]
fn moves_preserve_original_path_and_support_undo() {
    let f = Fixture::new();
    let p = f.write("note.txt", b"unchanged");
    let (id, op, dest) = f.planned(&p);
    let original = f.db.item(id).unwrap().original_path;
    moves::approve(&f.db, op, &cancel()).unwrap();
    moves::execute(&f.db, op, &cancel()).unwrap();
    assert!(!p.exists());
    assert_eq!(fs::read(&dest).unwrap(), b"unchanged");
    assert_eq!(f.db.item(id).unwrap().original_path, original);
    assert_eq!(f.db.item(id).unwrap().current_path, dest.to_string_lossy());
    moves::undo(&f.db, op, &cancel()).unwrap();
    assert!(p.exists() && !dest.exists());
    assert_eq!(moves::operation(&f.db, op).unwrap().state, "undone");
}
#[test]
fn cross_volume_copy_path_verifies_and_preserves_data() {
    let f = Fixture::new();
    let p = f.write("note.txt", b"copied safely");
    let (_, op, dest) = f.planned(&p);
    moves::approve(&f.db, op, &cancel()).unwrap();
    moves::execute_mode(&f.db, op, &cancel(), true).unwrap();
    assert_eq!(fs::read(&dest).unwrap(), b"copied safely");
    assert!(!p.exists());
}
#[test]
fn whole_project_moves_include_ignored_contents_and_update_descendants() {
    let f = Fixture::new();
    let root = f.source.join("app");
    f.write("app/Cargo.toml", b"[package]");
    let child = f.write("app/src/main.rs", b"fn main() {}");
    f.write("app/target/keep.bin", b"generated but preserved");
    let (id, op, dest) = f.planned(&root);
    let child_id = f.item(&child).id;
    moves::approve(&f.db, op, &cancel()).unwrap();
    moves::execute_mode(&f.db, op, &cancel(), true).unwrap();
    assert_eq!(
        fs::read(dest.join("target/keep.bin")).unwrap(),
        b"generated but preserved"
    );
    assert_eq!(f.db.item(child_id).unwrap().project_id, Some(id));
    assert_eq!(
        f.db.item(child_id).unwrap().current_path,
        dest.join("src").join("main.rs").to_string_lossy()
    );
}
#[test]
fn descendant_destinations_are_blocked() {
    let f = Fixture::new();
    let root = f.source.join("app");
    f.write("app/Cargo.toml", b"[package]");
    let id = f.ready(&root);
    let op = moves::prepare(&f.db, id, Some(&root.join("nested")), &cancel()).unwrap();
    assert_eq!(moves::operation(&f.db, op).unwrap().state, "blocked");
    assert!(moves::approve(&f.db, op, &cancel()).is_err());
    assert!(root.exists());
}
#[test]
fn interruption_after_publish_is_reconciled_without_losing_original_path() {
    let f = Fixture::new();
    let p = f.write("note.txt", b"note");
    let (id, op, dest) = f.planned(&p);
    moves::approve(&f.db, op, &cancel()).unwrap();
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    moves::rename_no_replace(&p, &dest).unwrap();
    f.db.conn
        .execute("UPDATE moves SET state='moving' WHERE id=?", [op])
        .unwrap();
    moves::recover(&f.db, &cancel()).unwrap();
    assert_eq!(moves::operation(&f.db, op).unwrap().state, "completed");
    assert_eq!(f.db.item(id).unwrap().current_path, dest.to_string_lossy());
    assert!(f.db.item(id).unwrap().original_path.ends_with("note.txt"));
}
#[test]
fn recovery_retains_both_copies_until_explicit_retry() {
    let f = Fixture::new();
    let p = f.write("note.txt", b"note");
    let (_, op, dest) = f.planned(&p);
    let expected = moves::operation(&f.db, op).unwrap().snapshot;
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    fs::copy(&p, &dest).unwrap();
    assert!(expected.content_matches(&moves::manifest(&dest, &cancel()).unwrap()));
    f.db.conn
        .execute("UPDATE moves SET state='published' WHERE id=?", [op])
        .unwrap();
    moves::recover(&f.db, &cancel()).unwrap();
    assert!(p.exists() && dest.exists());
    assert_eq!(moves::operation(&f.db, op).unwrap().state, "recovery");
    moves::retry_recovery(&f.db, op, &cancel()).unwrap();
    assert!(!p.exists() && dest.exists());
}
#[test]
fn changed_copied_destination_never_causes_source_removal() {
    let f = Fixture::new();
    let p = f.write("note.txt", b"original");
    let (_, op, dest) = f.planned(&p);
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    fs::write(&dest, b"different").unwrap();
    f.db.conn
        .execute("UPDATE moves SET state='recovery' WHERE id=?", [op])
        .unwrap();
    assert!(moves::retry_recovery(&f.db, op, &cancel()).is_err());
    assert_eq!(fs::read(&p).unwrap(), b"original");
}
#[test]
fn edited_categories_and_destinations_invalidate_approval() {
    let f = Fixture::new();
    let p = f.write("note.txt", b"note");
    let (id, op, _) = f.planned(&p);
    moves::approve(&f.db, op, &cancel()).unwrap();
    let category = f.db.item(id).unwrap().category_id;
    f.db.save_category(Some(category), "Unfiled").unwrap();
    assert_eq!(moves::operation(&f.db, op).unwrap().state, "invalidated");
}
#[test]
fn text_excerpts_and_metadata_only_formats_are_explicit() {
    let f = Fixture::new();
    let p = f.write("long.txt", &vec![b'a'; 40000]);
    let e = content::extract_file(&p, &Settings::default(), &cancel()).unwrap();
    assert!(e.limitations.contains("excerpts"));
    assert!(content::model_text(&e.text).len() < 25000);
    let doc = f.write("office.docx", b"zip metadata");
    let e = content::extract_file(&doc, &Settings::default(), &cancel()).unwrap();
    assert!(e.text.is_empty() && e.limitations.contains("Metadata-only"));
}
#[test]
fn images_are_resized_without_changing_original() {
    let f = Fixture::new();
    let p = f.source.join("image.png");
    image::DynamicImage::new_rgb8(1600, 1200).save(&p).unwrap();
    let before = fs::read(&p).unwrap();
    let data = content::image_data(&p).unwrap();
    assert!(data.starts_with("data:image/png;base64,"));
    assert_eq!(fs::read(&p).unwrap(), before);
    assert!(content::resized_image(&p, 1024).unwrap().width() <= 1024);
}
#[test]
fn catalog_snapshot_does_not_load_all_extracted_content() {
    let f = Fixture::new();
    let category = f.db.categories().unwrap()[0].id;
    let tx = f.db.conn.unchecked_transaction().unwrap();
    for n in 0..10000 {
        let path = f
            .source
            .join(format!("{n:05}.txt"))
            .to_string_lossy()
            .into_owned();
        tx.execute("INSERT INTO items(original_path,current_path,path_key,kind,fingerprint,category_id,content) VALUES(?,?,?,'document','fixture',?,?)",params![path,path,paths::key(&path),category,"cached content"]).unwrap();
    }
    tx.commit().unwrap();
    let snapshot = f.db.snapshot().unwrap();
    assert_eq!(snapshot.items.len(), 10000);
    assert!(snapshot.items.iter().all(|i| i.content.is_empty()));
    assert_eq!(
        f.db.item(snapshot.items[0].id).unwrap().content,
        "cached content"
    );
}

#[cfg(windows)]
#[test]
fn windows_alternate_streams_survive_copy_and_undo() {
    let f = Fixture::new();
    let p = f.write("note.txt", b"main data");
    let mut stream = p.as_os_str().to_os_string();
    stream.push(":organizer-test");
    fs::write(&stream, b"alternate data").unwrap();
    let (_, op, dest) = f.planned(&p);
    moves::approve(&f.db, op, &cancel()).unwrap();
    moves::execute_mode(&f.db, op, &cancel(), true).unwrap();
    let mut output = dest.as_os_str().to_os_string();
    output.push(":organizer-test");
    assert_eq!(fs::read(output).unwrap(), b"alternate data");
    moves::undo(&f.db, op, &cancel()).unwrap();
    assert_eq!(fs::read(stream).unwrap(), b"alternate data");
}

#[test]
fn selecting_full_root_absorbs_partial_projects() {
    let f = Fixture::new();
    f.write("app/Cargo.toml", b"[package]");
    let child = f.write("app/src/main.rs", b"fn main() {}");
    for s in f.db.sources().unwrap() {
        f.db.remove_source(s.id).unwrap();
    }
    f.db.add_source(&f.source.join("app/src")).unwrap();
    f.scan();
    let partial =
        f.db.items()
            .unwrap()
            .into_iter()
            .find(|i| i.kind == Kind::Project)
            .unwrap();
    f.db.add_source(&f.source).unwrap();
    f.scan();
    let units: Vec<_> =
        f.db.items()
            .unwrap()
            .into_iter()
            .filter(|i| i.kind == Kind::Project && i.project_id.is_none())
            .collect();
    assert_eq!(units.len(), 1);
    assert!(units[0].block_reason.is_empty());
    assert_eq!(f.db.item(partial.id).unwrap().project_id, Some(units[0].id));
    assert_eq!(f.item(&child).project_id, Some(units[0].id));
}
#[test]
fn file_include_can_reach_a_pruned_non_project_directory() {
    let f = Fixture::new();
    let p = f.write(
        "node_modules/library/readme.txt",
        b"An explicitly selected reference",
    );
    f.scan();
    assert!(f.db.items().unwrap().is_empty());
    f.db.set_override(&paths::normalize(&p).unwrap(), true)
        .unwrap();
    f.scan();
    assert_eq!(f.db.items().unwrap().len(), 1);
    assert_eq!(f.item(&p).project_id, None);
}
#[test]
fn excluding_cataloged_content_invalidates_approval_and_stales_analysis() {
    let f = Fixture::new();
    let p = f.write("notes/note.txt", b"Note");
    let (id, op, _) = f.planned(&p);
    moves::approve(&f.db, op, &cancel()).unwrap();
    let revision = f.db.item(id).unwrap().revision;
    f.db.set_override(&paths::normalize(p.parent().unwrap()).unwrap(), false)
        .unwrap();
    f.scan();
    let item = f.db.item(id).unwrap();
    assert_eq!(item.relevance, "excluded");
    assert!(item.revision > revision);
    assert_eq!(moves::operation(&f.db, op).unwrap().state, "invalidated");
    let job = Job {
        id: 0,
        item_id: id,
        revision,
        kind: "classify".into(),
        tags: vec![],
        category_revision: f.db.category_epoch().unwrap(),
    };
    let d = Decision {
        relevance: Some(("relevant".into(), 1.)),
        category: None,
        tags: vec![],
        raw: "stale".into(),
    };
    assert!(!f.db.apply_decision(&job, &d, "fixture").unwrap());
    f.db.set_override(&paths::normalize(&p).unwrap(), true)
        .unwrap();
    f.scan();
    assert_eq!(f.db.item(id).unwrap().relevance, "relevant");
}
#[test]
fn renamed_default_category_remains_the_default_for_new_files() {
    let f = Fixture::new();
    let default =
        f.db.categories()
            .unwrap()
            .into_iter()
            .find(|c| c.name == "Uncategorized")
            .unwrap();
    f.db.save_category(Some(default.id), "Unfiled").unwrap();
    let p = f.write("new.txt", b"New content");
    f.scan();
    assert_eq!(f.item(&p).category_id, default.id);
}
#[test]
fn edited_tag_scores_expire_without_losing_manual_choices() {
    let f = Fixture::new();
    let p = f.write("note.txt", b"note");
    let id = f.ready(&p);
    let tag = f.db.save_tag(None, "Travel", "Trips").unwrap();
    let t = f.db.tags().unwrap()[0].clone();
    let item = f.db.item(id).unwrap();
    let job = Job {
        id: 0,
        item_id: id,
        revision: item.revision,
        kind: "tags".into(),
        tags: vec![t.clone()],
        category_revision: 0,
    };
    let d = Decision {
        relevance: None,
        category: None,
        tags: vec![(tag, t.revision, 0.9)],
        raw: "score".into(),
    };
    f.db.apply_decision(&job, &d, "fixture").unwrap();
    f.db.set_manual_tag(id, tag, Some(true)).unwrap();
    f.db.save_tag(Some(tag), "Travel", "Only personal holiday documents")
        .unwrap();
    let result = f.db.item_tags(id).unwrap().remove(0);
    assert_eq!(result.probability, None);
    assert_eq!(result.manual, Some(true));
    assert!(result.assigned());
}
#[test]
fn tags_added_during_classification_are_backfilled_after_relevance_arrives() {
    let f = Fixture::new();
    let p = f.write("note.txt", b"note");
    f.scan();
    let item = f.item(&p);
    let job = f.db.next_job().unwrap().unwrap();
    f.db.save_tag(None, "Notes", "Personal notes").unwrap();
    let d = Decision {
        relevance: Some(("relevant".into(), 1.)),
        category: Some((item.category_id, 1.)),
        tags: vec![],
        raw: "classification".into(),
    };
    f.db.apply_decision(&job, &d, "fixture").unwrap();
    assert_eq!(
        f.db.conn
            .query_row::<i64, _, _>(
                "SELECT COUNT(*) FROM jobs WHERE item_id=? AND kind='tags'",
                [item.id],
                |r| r.get(0)
            )
            .unwrap(),
        1
    );
}
#[test]
fn category_revision_changes_requeue_stale_classification() {
    let f = Fixture::new();
    let p = f.write("note.txt", b"note");
    f.scan();
    let item = f.item(&p);
    let job = f.db.next_job().unwrap().unwrap();
    f.db.save_category(Some(item.category_id), "Unfiled")
        .unwrap();
    let d = Decision {
        relevance: Some(("excluded".into(), 1.)),
        category: Some((1, 1.)),
        tags: vec![],
        raw: "old categories".into(),
    };
    f.db.apply_decision(&job, &d, "fixture").unwrap();
    assert_eq!(f.db.item(item.id).unwrap().category_id, item.category_id);
    assert_eq!(f.db.item(item.id).unwrap().relevance, "needs review");
    assert_eq!(
        f.db.conn
            .query_row::<i64, _, _>("SELECT COUNT(*) FROM jobs WHERE state='queued'", [], |r| r
                .get(0))
            .unwrap(),
        1
    );
}
#[test]
fn content_history_and_search_survive_edits_and_moves() {
    let f = Fixture::new();
    let p = f.write("note.txt", b"First content");
    f.scan();
    let item = f.item(&p);
    f.db.store_content(
        item.id,
        item.revision,
        "Quarterly reimbursement invoice",
        "complete",
    )
    .unwrap();
    assert_eq!(f.db.search("reimburs").unwrap(), vec![item.id]);
    assert!(f.db.search("\" OR *").is_ok());
    fs::write(&p, b"Second content").unwrap();
    f.scan();
    let current = f.item(&p);
    assert!(current.revision > item.revision);
    assert!(f.db.search("reimbursement").unwrap().is_empty());
    let old: String =
        f.db.conn
            .query_row(
                "SELECT content FROM item_revisions WHERE item_id=? AND revision=?",
                params![item.id, item.revision],
                |r| r.get(0),
            )
            .unwrap();
    assert_eq!(old, "Quarterly reimbursement invoice");
    f.db.set_relevance(item.id, "relevant").unwrap();
    let op = moves::prepare(&f.db, item.id, Some(&f.dest.join("note.txt")), &cancel()).unwrap();
    moves::approve(&f.db, op, &cancel()).unwrap();
    moves::execute(&f.db, op, &cancel()).unwrap();
    assert_eq!(f.db.search("destination").unwrap(), vec![item.id]);
    assert_eq!(f.db.search("source").unwrap(), vec![item.id]);
}
#[test]
fn completed_move_keeps_its_journal_stages() {
    let f = Fixture::new();
    let p = f.write("note.txt", b"note");
    let (_, op, _) = f.planned(&p);
    moves::approve(&f.db, op, &cancel()).unwrap();
    moves::execute_mode(&f.db, op, &cancel(), true).unwrap();
    let states: Vec<String> =
        f.db.conn
            .prepare("SELECT state FROM move_events WHERE move_id=? ORDER BY id")
            .unwrap()
            .query_map([op], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
    assert_eq!(
        states,
        vec![
            "planned",
            "approved",
            "moving",
            "copying",
            "published",
            "removing",
            "completed"
        ]
    );
}
#[test]
fn altered_moved_content_and_occupied_original_locations_block_undo() {
    let f = Fixture::new();
    let p = f.write("note.txt", b"original");
    let (_, op, dest) = f.planned(&p);
    moves::approve(&f.db, op, &cancel()).unwrap();
    moves::execute(&f.db, op, &cancel()).unwrap();
    fs::write(&dest, b"edited").unwrap();
    assert!(moves::undo(&f.db, op, &cancel()).is_err());
    assert_eq!(fs::read(&dest).unwrap(), b"edited");
    fs::write(&dest, b"original").unwrap();
    fs::write(&p, b"occupied").unwrap();
    assert!(moves::undo(&f.db, op, &cancel()).is_err());
    assert_eq!(fs::read(&p).unwrap(), b"occupied");
    assert!(dest.exists());
}
#[test]
fn custom_destinations_cannot_rename_files() {
    let f = Fixture::new();
    let p = f.write("note.txt", b"note");
    let id = f.ready(&p);
    let op = moves::prepare(&f.db, id, Some(&f.dest.join("renamed.txt")), &cancel()).unwrap();
    assert_eq!(moves::operation(&f.db, op).unwrap().state, "blocked");
    assert!(p.exists());
}
#[test]
fn long_original_filenames_are_preserved() {
    let f = Fixture::new();
    let filename = format!("{}.txt", "a".repeat(100));
    let p = f.write(&filename, b"note");
    let (_, op, dest) = f.planned(&p);
    moves::approve(&f.db, op, &cancel()).unwrap();
    moves::execute(&f.db, op, &cancel()).unwrap();
    assert_eq!(dest.file_name().unwrap(), p.file_name().unwrap());
    assert!(dest.exists());
}
#[test]
fn linked_git_worktrees_are_cataloged_but_cannot_move() {
    let f = Fixture::new();
    f.write("app/.git", b"gitdir: elsewhere");
    f.write("app/README.md", b"Linked checkout");
    f.scan();
    let project =
        f.db.items()
            .unwrap()
            .into_iter()
            .find(|i| i.kind == Kind::Project)
            .unwrap();
    assert!(project.block_reason.contains("Linked Git"));
    f.db.set_relevance(project.id, "relevant").unwrap();
    assert!(moves::prepare(&f.db, project.id, Some(&f.dest.join("app")), &cancel()).is_err());
}
#[cfg(windows)]
#[test]
fn open_writers_block_moves_without_modifying_content() {
    let f = Fixture::new();
    let p = f.write("note.txt", b"original");
    let (_, op, dest) = f.planned(&p);
    moves::approve(&f.db, op, &cancel()).unwrap();
    let _writer = fs::OpenOptions::new().write(true).open(&p).unwrap();
    assert!(moves::execute(&f.db, op, &cancel()).is_err());
    assert!(p.exists() && !dest.exists());
    assert_eq!(fs::read(&p).unwrap(), b"original");
}

#[cfg(windows)]
#[test]
fn locked_files_and_project_members_remain_reviewable_and_rescan_after_unlocking() {
    use std::os::windows::fs::OpenOptionsExt;
    let f = Fixture::new();
    let locked = f.write("locked.txt", b"Locked document");
    let member = f.write("app/README.md", b"Locked project document");
    f.write("app/Cargo.toml", b"[package]");
    let readable = f.write("readable.txt", b"Available document");
    let locks: Vec<_> = [&locked, &member]
        .iter()
        .map(|p| {
            fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(p)
                .unwrap()
        })
        .collect();
    f.scan();
    let items = f.db.items().unwrap();
    for name in ["locked.txt", "README.md", "app"] {
        let item = items
            .iter()
            .find(|i| Path::new(&i.current_path).file_name().unwrap() == name)
            .unwrap();
        assert_eq!(item.relevance, "needs review");
        assert!(item.block_reason.starts_with("Cannot read"));
        assert!(item.error.contains("Cannot read"));
    }
    assert!(f.item(&readable).block_reason.is_empty());
    drop(locks);
    f.scan();
    assert!(f.item(&locked).block_reason.is_empty());
    assert!(f.item(&member).block_reason.is_empty());
    assert!(f.item(&f.source.join("app")).block_reason.is_empty());
}

#[test]
fn changed_content_expires_model_tags_but_keeps_manual_decisions() {
    let f = Fixture::new();
    let p = f.write("note.txt", b"Travel document");
    let id = f.ready(&p);
    let tag = f.db.save_tag(None, "Travel", "Trips").unwrap();
    let t = f.db.tags().unwrap().remove(0);
    let job = Job {
        id: 0,
        item_id: id,
        revision: f.db.item(id).unwrap().revision,
        kind: "tags".into(),
        tags: vec![t.clone()],
        category_revision: 0,
    };
    f.db.apply_decision(
        &job,
        &Decision {
            relevance: None,
            category: None,
            tags: vec![(tag, t.revision, 0.9)],
            raw: "score".into(),
        },
        "fixture",
    )
    .unwrap();
    f.db.set_manual_tag(id, tag, Some(false)).unwrap();
    fs::write(&p, b"Housing document").unwrap();
    f.scan();
    let assignment = f.db.item_tags(id).unwrap().remove(0);
    assert_eq!(assignment.probability, None);
    assert_eq!(assignment.manual, Some(false));
    assert!(!assignment.assigned());
}

#[test]
fn missing_catalog_paths_block_destinations_before_filesystem_changes() {
    let f = Fixture::new();
    let old = f.write("old/note.txt", b"Old content");
    let current = f.write("new/note.txt", b"New content");
    f.scan();
    let old_id = f.item(&old).id;
    fs::remove_file(&old).unwrap();
    f.scan();
    let id = f.ready(&current);
    let op = moves::prepare(&f.db, id, Some(&old), &cancel()).unwrap();
    assert_eq!(moves::operation(&f.db, op).unwrap().state, "blocked");
    assert!(
        moves::operation(&f.db, op)
            .unwrap()
            .error
            .contains("catalog path")
    );
    assert!(moves::approve(&f.db, op, &cancel()).is_err());
    assert!(current.exists() && !old.exists());
    assert!(f.db.item(old_id).unwrap().missing);
}

#[test]
fn preparing_project_moves_invalidates_and_blocks_incoming_destinations() {
    let f = Fixture::new();
    f.write("app/Cargo.toml", b"[package]");
    f.write("app/README.md", b"Authored project");
    let incoming = f.write("note.txt", b"Incoming content");
    let id = f.ready(&incoming);
    let project = f.item(&f.source.join("app"));
    f.db.set_relevance(project.id, "relevant").unwrap();
    let inside = f.source.join("app/inbox/note.txt");
    let op = moves::prepare(&f.db, id, Some(&inside), &cancel()).unwrap();
    moves::approve(&f.db, op, &cancel()).unwrap();
    let project_op =
        moves::prepare(&f.db, project.id, Some(&f.dest.join("app")), &cancel()).unwrap();
    assert_eq!(moves::operation(&f.db, op).unwrap().state, "invalidated");
    let blocked = moves::prepare(&f.db, id, Some(&inside), &cancel()).unwrap();
    assert_eq!(moves::operation(&f.db, blocked).unwrap().state, "blocked");
    moves::approve(&f.db, project_op, &cancel()).unwrap();
    moves::execute(&f.db, project_op, &cancel()).unwrap();
    assert!(moves::execute(&f.db, op, &cancel()).is_err());
    assert!(!f.source.join("app").exists());
    assert!(incoming.exists());
}

#[test]
fn repositories_with_registered_linked_worktrees_cannot_move() {
    let f = Fixture::new();
    f.write("app/.git/worktrees/checkout/gitdir", b"elsewhere");
    f.write("app/README.md", b"Main repository with linked worktree");
    f.scan();
    let item = f.item(&f.source.join("app"));
    assert!(item.block_reason.contains("Linked Git"));
}

#[cfg(windows)]
#[test]
fn copied_readonly_files_keep_attributes_and_can_be_undone() {
    let f = Fixture::new();
    let p = f.write("note.txt", b"Read-only original");
    let mut permissions = fs::metadata(&p).unwrap().permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&p, permissions).unwrap();
    let (_, op, dest) = f.planned(&p);
    moves::approve(&f.db, op, &cancel()).unwrap();
    moves::execute_mode(&f.db, op, &cancel(), true).unwrap();
    assert!(!p.exists());
    assert!(fs::metadata(&dest).unwrap().permissions().readonly());
    moves::undo(&f.db, op, &cancel()).unwrap();
    assert!(fs::metadata(&p).unwrap().permissions().readonly());
    assert_eq!(fs::read(&p).unwrap(), b"Read-only original");
}

#[test]
fn cross_volume_projects_with_hard_links_are_blocked() {
    let f = Fixture::new();
    let p = f.write("app/README.md", b"Linked authored file");
    f.write("app/Cargo.toml", b"[package]");
    let external = f._temp.path().join("external.md");
    fs::hard_link(&p, &external).unwrap();
    let (_, op, dest) = f.planned(&f.source.join("app"));
    moves::approve(&f.db, op, &cancel()).unwrap();
    assert!(moves::execute_mode(&f.db, op, &cancel(), true).is_err());
    assert!(p.exists() && external.exists() && !dest.exists());
    assert_eq!(fs::read(&external).unwrap(), b"Linked authored file");
}

#[test]
fn selected_source_destination_conflicts_are_blocked_before_moving() {
    let f = Fixture::new();
    f.write("app/Cargo.toml", b"[package]");
    let id = f.ready(&f.source.join("app"));
    let destination = f.dest.join("app");
    fs::create_dir_all(&destination).unwrap();
    f.db.add_source(&destination).unwrap();
    fs::remove_dir(&destination).unwrap();
    let op = moves::prepare(&f.db, id, Some(&destination), &cancel()).unwrap();
    assert_eq!(moves::operation(&f.db, op).unwrap().state, "blocked");
    assert!(
        moves::operation(&f.db, op)
            .unwrap()
            .error
            .contains("selected source")
    );
    assert!(f.source.join("app").exists() && !destination.exists());
}

#[test]
fn failed_catalog_reconciliation_is_journaled_without_blocking_startup() {
    let f = Fixture::new();
    let p = f.write("note.txt", b"Recoverable original");
    let (_, op, dest) = f.planned(&p);
    moves::approve(&f.db, op, &cancel()).unwrap();
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    moves::rename_no_replace(&p, &dest).unwrap();
    f.db.conn
        .execute("UPDATE moves SET state='published' WHERE id=?", [op])
        .unwrap();
    let other = f.write("elsewhere.txt", b"Catalog conflict");
    f.scan();
    let other_id = f.item(&other).id;
    f.db.conn
        .execute(
            "UPDATE items SET current_path=?,path_key=? WHERE id=?",
            params![
                dest.to_str().unwrap(),
                paths::key(dest.to_str().unwrap()),
                other_id
            ],
        )
        .unwrap();
    moves::recover(&f.db, &cancel()).unwrap();
    let recovery = moves::operation(&f.db, op).unwrap();
    assert_eq!(recovery.state, "recovery");
    assert!(recovery.error.contains("catalog reconciliation failed"));
    assert_eq!(fs::read(&dest).unwrap(), b"Recoverable original");
}

#[test]
fn tag_edits_and_acceptance_roll_back_when_backfill_queueing_fails() {
    let f = Fixture::new();
    let a = f.write("a.txt", b"Earlier document");
    let b = f.write("b.txt", b"Later document");
    f.ready(&a);
    let second = f.ready(&b);
    let tag =
        f.db.save_tag(None, "Notes", "Original description")
            .unwrap();
    let before = f.db.tags().unwrap().remove(0);
    let jobs: i64 =
        f.db.conn
            .query_row("SELECT COUNT(*) FROM jobs", [], |r| r.get(0))
            .unwrap();
    f.db.conn.execute_batch(&format!("CREATE TRIGGER fail_backfill BEFORE INSERT ON jobs WHEN NEW.kind='tags' AND NEW.item_id={second} BEGIN SELECT RAISE(ABORT,'simulated queue failure'); END;")).unwrap();
    assert!(
        f.db.save_tag(Some(tag), "Notes", "Changed description")
            .is_err()
    );
    let after = f.db.tags().unwrap().remove(0);
    assert_eq!(after.description, before.description);
    assert_eq!(after.revision, before.revision);
    f.db.save_suggestions(&[("Reference".into(), "Useful references".into())])
        .unwrap();
    let suggestion = f.db.snapshot().unwrap().suggestions.remove(0);
    assert!(
        f.db.accept_suggestion(suggestion.id, "Reference", "Useful references")
            .is_err()
    );
    assert_eq!(f.db.tags().unwrap().len(), 1);
    assert_eq!(
        f.db.conn
            .query_row::<String, _, _>(
                "SELECT state FROM suggestions WHERE id=?",
                [suggestion.id],
                |r| r.get(0)
            )
            .unwrap(),
        "pending"
    );
    assert_eq!(
        f.db.conn
            .query_row::<i64, _, _>("SELECT COUNT(*) FROM jobs", [], |r| r.get(0))
            .unwrap(),
        jobs
    );
}

#[test]
fn disappearing_selected_roots_keep_missing_history_and_stable_ids_on_return() {
    let f = Fixture::new();
    let p = f.write("note.txt", b"Original content");
    let id = f.ready(&p);
    let original = f.db.item(id).unwrap().original_path;
    let op = moves::prepare(&f.db, id, Some(&f.dest.join("note.txt")), &cancel()).unwrap();
    moves::approve(&f.db, op, &cancel()).unwrap();
    fs::remove_file(&p).unwrap();
    fs::remove_dir(&f.source).unwrap();
    assert!(scanner::scan(&f.db, &cancel(), |_| {}).is_err());
    assert!(f.db.item(id).unwrap().missing);
    assert_eq!(f.db.item(id).unwrap().original_path, original);
    assert_eq!(moves::operation(&f.db, op).unwrap().state, "invalidated");
    f.write("note.txt", b"Restored content");
    f.scan();
    assert_eq!(f.item(&p).id, id);
    assert!(!f.item(&p).missing);
    assert_eq!(f.item(&p).original_path, original);
}
