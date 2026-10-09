mod support;
use document_organizer::{
    catalog::*,
    db::Database,
    model::Models,
    scanner,
    worker::{Command, Event, Worker},
};
use serde_json::{Value, json};
use std::{
    fs,
    sync::{
        atomic::AtomicBool,
        mpsc::{Receiver, RecvTimeoutError},
    },
    time::{Duration, Instant},
};
use support::Mock;

fn response() -> Value {
    json!({"answers":{"relevance":{"choice":"relevant","confidence":0.95},"category":{"choice":"category_1","confidence":0.92}}})
}
fn fixture() -> (tempfile::TempDir, Database, Item) {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(
        source.join("note.txt"),
        b"A useful personal note with readable text",
    )
    .unwrap();
    let db = Database::open(&temp.path().join("catalog.sqlite3")).unwrap();
    db.add_source(&source).unwrap();
    scanner::scan(&db, &AtomicBool::new(false), |_| {}).unwrap();
    let item = db.items().unwrap().remove(0);
    (temp, db, item)
}
fn wait_snapshot(events: &Receiver<Event>, predicate: impl Fn(&Snapshot) -> bool) -> Box<Snapshot> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            Instant::now() < deadline,
            "Worker did not reach expected state"
        );
        if let Event::Snapshot(s) = events
            .recv_timeout(Duration::from_secs(2))
            .expect("worker event")
            && predicate(&s)
        {
            return s;
        }
    }
}
fn stop(worker: Worker, events: Receiver<Event>) {
    worker.cancel();
    worker.send(Command::Shutdown);
    drop(worker);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(Instant::now() < deadline, "Worker shutdown timed out");
        if let Err(RecvTimeoutError::Disconnected) = events.recv_timeout(Duration::from_millis(100))
        {
            break;
        }
    }
}

#[test]
fn text_and_image_requests_follow_the_systemone_contract() {
    let (temp, db, mut item) = fixture();
    let mut answer = response();
    answer["answers"]["tag_1"] = json!({"noul":0.9});
    answer["answers"]["tag_2"] = json!({"noul":0.8});
    let server = Mock::new(vec![
        (200, answer.clone(), Duration::ZERO),
        (200, answer, Duration::ZERO),
    ]);
    let settings = Settings {
        decision_url: format!("{}/v1/systemone", server.url),
        ..Settings::default()
    };
    let models = Models::new(settings).unwrap();
    let tags = vec![
        Tag {
            id: 1,
            name: "Work".into(),
            description: "Work related".into(),
            revision: 1,
        },
        Tag {
            id: 2,
            name: "Reference".into(),
            description: "Reusable knowledge".into(),
            revision: 1,
        },
    ];
    item.content = "A useful personal reference document".into();
    let d = models
        .classify(
            &item,
            &db.categories().unwrap(),
            &tags,
            false,
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(d.tags.len(), 2);
    assert!(d.tags.iter().all(|t| t.2 > 0.5));
    let (path, request) = server
        .requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert_eq!(path, "/v1/systemone");
    assert_eq!(request["questions"]["tag_1"]["type"], "noul");
    assert_eq!(request["questions"]["tag_2"]["type"], "noul");
    assert!(request.get("images").is_none());
    assert!(
        request["state"]["text"]
            .as_str()
            .unwrap()
            .contains("reference")
    );
    let image = temp.path().join("photo.png");
    image::DynamicImage::new_rgb8(1600, 1200)
        .save(&image)
        .unwrap();
    let before = fs::read(&image).unwrap();
    item.current_path = image.to_string_lossy().into_owned();
    item.kind = Kind::Image;
    models
        .classify(
            &item,
            &db.categories().unwrap(),
            &tags,
            false,
            &AtomicBool::new(false),
        )
        .unwrap();
    let (_, request) = server
        .requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert!(
        request["images"][0]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,")
    );
    assert_eq!(fs::read(image).unwrap(), before);
    server.finish();
}
#[test]
fn tag_backfill_sends_only_independent_tag_questions() {
    let (_, db, item) = fixture();
    let server = Mock::new(vec![(
        200,
        json!({"answers":{"tag_1":{"noul":0.87}}}),
        Duration::ZERO,
    )]);
    let models = Models::new(Settings {
        decision_url: format!("{}/v1/systemone", server.url),
        ..Settings::default()
    })
    .unwrap();
    let tag = Tag {
        id: 1,
        name: "Notes".into(),
        description: "Personal notes".into(),
        revision: 3,
    };
    let d = models
        .classify(
            &item,
            &db.categories().unwrap(),
            &[tag],
            true,
            &AtomicBool::new(false),
        )
        .unwrap();
    assert!(d.category.is_none() && d.relevance.is_none());
    let (_, request) = server
        .requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert_eq!(request["questions"].as_object().unwrap().len(), 1);
    assert!(request["questions"].get("category").is_none());
    server.finish();
}
#[test]
fn generative_requests_are_bounded_and_only_propose_tags() {
    let (_, _, mut item) = fixture();
    item.relevance = "relevant".into();
    item.content = "Long document context ".repeat(3000);
    let server = Mock::new(vec![(
        200,
        json!({"choices":[{"message":{"content":"{\"tags\":[{\"name\":\"Notes\",\"description\":\"Personal notes\"}]}"}}]}),
        Duration::ZERO,
    )]);
    let models = Models::new(Settings {
        generative_url: format!("{}/v1/chat/completions", server.url),
        generative_model: "local-generator".into(),
        ..Settings::default()
    })
    .unwrap();
    let result = models
        .suggest(&vec![item; 35], &[], &AtomicBool::new(false))
        .unwrap();
    assert_eq!(result.len(), 1);
    let (path, request) = server
        .requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert_eq!(path, "/v1/chat/completions");
    assert_eq!(request["model"], "local-generator");
    let context: Value =
        serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(context["items"].as_array().unwrap().len(), 20);
    assert!(
        context["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["text"].as_str().unwrap().chars().count() < 1300)
    );
    server.finish();
}
#[test]
fn outage_and_invalid_response_preserve_progress_and_allow_retry() {
    let (temp, db, item) = fixture();
    let failed = Mock::new(vec![(503, json!({"error":"offline"}), Duration::ZERO)]);
    let mut settings = Settings {
        decision_url: format!("{}/v1/systemone", failed.url),
        ..Settings::default()
    };
    db.save_settings(&settings).unwrap();
    let (worker, events) = Worker::start(temp.path()).unwrap();
    wait_snapshot(&events, |s| s.counts.get("failed") == Some(&1));
    assert!(!db.item(item.id).unwrap().content.is_empty());
    assert!(std::path::Path::new(&item.current_path).exists());
    failed.finish();
    let invalid = Mock::new(vec![(200, json!({"unexpected":true}), Duration::ZERO)]);
    settings.decision_url = format!("{}/v1/systemone", invalid.url);
    worker.send(Command::Settings(settings.clone()));
    worker.send(Command::Retry);
    wait_snapshot(&events, |s| {
        s.counts.get("failed") == Some(&1) && s.items[0].error.contains("Missing decision answers")
    });
    invalid.finish();
    let success = Mock::new(vec![(200, response(), Duration::ZERO)]);
    settings.decision_url = format!("{}/v1/systemone", success.url);
    worker.send(Command::Settings(settings));
    worker.send(Command::Retry);
    let s = wait_snapshot(&events, |s| s.counts.get("completed") == Some(&1));
    assert_eq!(s.items[0].relevance, "relevant");
    assert!(s.moves.is_empty());
    success.finish();
    stop(worker, events);
}
#[test]
fn cancellation_after_request_keeps_the_job_retryable() {
    let (temp, db, item) = fixture();
    let server = Mock::new(vec![
        (200, response(), Duration::from_millis(400)),
        (200, response(), Duration::ZERO),
    ]);
    db.save_settings(&Settings {
        decision_url: format!("{}/v1/systemone", server.url),
        ..Settings::default()
    })
    .unwrap();
    let (worker, events) = Worker::start(temp.path()).unwrap();
    server
        .requests
        .recv_timeout(Duration::from_secs(3))
        .unwrap();
    worker.cancel();
    wait_snapshot(&events, |s| s.counts.get("cancelled") == Some(&1));
    assert_eq!(db.item(item.id).unwrap().relevance, "needs review");
    worker.send(Command::Retry);
    wait_snapshot(&events, |s| s.counts.get("completed") == Some(&1));
    assert!(std::path::Path::new(&item.current_path).exists());
    server.finish();
    stop(worker, events);
}
#[test]
fn restart_requeues_running_analysis_and_search_remains_available() {
    let (temp, db, item) = fixture();
    let job = db.next_job().unwrap().unwrap();
    db.store_content(item.id, item.revision, "Unique reimbursement receipt", "")
        .unwrap();
    assert_eq!(
        db.conn
            .query_row::<String, _, _>("SELECT state FROM jobs WHERE id=?", [job.id], |r| r.get(0))
            .unwrap(),
        "running"
    );
    let server = Mock::new(vec![(200, response(), Duration::from_millis(500))]);
    db.save_settings(&Settings {
        decision_url: format!("{}/v1/systemone", server.url),
        ..Settings::default()
    })
    .unwrap();
    let (worker, events) = Worker::start(temp.path()).unwrap();
    worker.search("reimburs".into());
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        assert!(Instant::now() < deadline);
        if let Event::Search(q, ids) = events.recv_timeout(Duration::from_secs(1)).unwrap() {
            assert_eq!(q, "reimburs");
            assert_eq!(ids, vec![item.id]);
            break;
        }
    }
    wait_snapshot(&events, |s| s.counts.get("completed") == Some(&1));
    assert_eq!(db.item(item.id).unwrap().original_path, item.original_path);
    server.finish();
    stop(worker, events);
}
#[test]
fn failed_extraction_remains_reviewable_even_with_a_confident_model() {
    let (temp, db, item) = fixture();
    let original = std::path::Path::new(&item.current_path);
    fs::write(original, [0xff, 0xfe, 0xfd]).unwrap();
    scanner::scan(&db, &AtomicBool::new(false), |_| {}).unwrap();
    let server = Mock::new(vec![(200, response(), Duration::ZERO)]);
    db.save_settings(&Settings {
        decision_url: format!("{}/v1/systemone", server.url),
        ..Settings::default()
    })
    .unwrap();
    let (worker, events) = Worker::start(temp.path()).unwrap();
    let s = wait_snapshot(&events, |s| s.items[0].status == "completed");
    assert_eq!(s.items[0].relevance, "needs review");
    assert!(s.items[0].limitations.contains("Not UTF-8"));
    assert_eq!(fs::read(original).unwrap(), vec![0xff, 0xfe, 0xfd]);
    server.finish();
    stop(worker, events);
}
