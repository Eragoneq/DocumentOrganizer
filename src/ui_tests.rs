use super::{Next, Organizer, Panel, Previous};
use document_organizer::worker::{Command, Worker};
use document_organizer::{db::Database, paths};
use gpui_kit::test::TestWindowExt;
use gpui_kit::{
    AppContext, Bounds, KeyBinding, Point, TestAppContext, WindowBounds, WindowOptions, init,
    open_window, px, size,
};
use rusqlite::params;

#[gpui_kit::test]
fn ten_thousand_items_are_virtualized_and_navigation_keeps_selection_visible(
    cx: &mut TestAppContext,
) {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("catalog.sqlite3")).unwrap();
    let tx = db.conn.unchecked_transaction().unwrap();
    for n in 0..10000 {
        let path = temp
            .path()
            .join(format!("catalog/{n:05}.txt"))
            .to_string_lossy()
            .into_owned();
        tx.execute("INSERT INTO items(original_path,current_path,path_key,kind,fingerprint,category_id,relevance,status) VALUES(?,?,?,'document','fixture',8,'relevant','completed')",params![path,path,paths::key(&path)]).unwrap();
    }
    tx.commit().unwrap();
    let (worker, events) = Worker::start(temp.path()).unwrap();
    let stop_worker = worker.clone();
    cx.update(|cx| {
        init(cx);
        cx.bind_keys([
            KeyBinding::new("ctrl-down", Next, Some("Organizer")),
            KeyBinding::new("ctrl-up", Previous, Some("Organizer")),
        ]);
    });
    let (handle, view) = cx.update(|cx| {
        open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: size(px(1320.), px(900.)),
                })),
                ..Default::default()
            },
            cx,
            |window, cx| cx.new(|cx| Organizer::new(worker, events, window, cx)),
        )
        .unwrap()
    });
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |view, cx| view.poll(cx));
        window.render_frame(cx);
        assert_eq!(view.read(cx).visible.len(), 10000);
        assert!(window.try_find(("check", 9000u64)).is_none());
        assert!(window.find(("check", 1u64)).visible());
        window.press("ctrl-down", cx);
        assert_eq!(view.read(cx).selected, Some(2));
        view.update(cx, |view, cx| view.select(9500, cx));
        window.render_frame(cx);
        window.render_frame(cx);
        let top = window.find("filter").bounds().bottom();
        let selected = window.find(("check", 9500u64)).bounds();
        assert!(selected.top() > top);
        assert!(selected.bottom() < window.viewport_size().height);
        assert!(window.try_find(("check", 1u64)).is_none());
        window.press("ctrl-up", cx);
        assert_eq!(view.read(cx).selected, Some(9499));
        assert!(window.find(("check", 9499u64)).visible());
        window.click("sources", cx);
        assert!(view.read(cx).panel == Panel::Sources);
        assert!(window.find("add-source").visible());
        window.click("browse", cx);
        assert_eq!(view.read(cx).visible.len(), 10000);
        // A background status event reaches the same view without rebuilding 10,000 row elements.
        stop_worker.send(Command::Refresh);
        view.update(cx, |view, cx| view.poll(cx));
        window.render_frame(cx);
        assert!(view.read(cx).list_scroll.is_scrollable());
    })
    .unwrap();
    stop_worker.cancel();
    stop_worker.send(Command::Shutdown);
}
