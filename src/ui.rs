use document_organizer::{
    catalog::*,
    content::Preview,
    worker::{Command, Event, Worker},
};
use gpui_kit::component::{
    ActiveTheme, Disableable, WindowExt,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    input::{Input, InputEvent, InputState},
    resizable::{h_resizable, resizable_panel},
};
use gpui_kit::{
    AnyElement, App, AppContext, Bounds, Context, Div, ElementId, Entity, FocusHandle, KeyBinding,
    ObjectFit, Render, ScrollStrategy, SharedString, Stateful, Subscription, Task, TitlebarOptions,
    UniformListScrollHandle, Window, WindowBounds, WindowOptions, actions, application, assets,
    div, img, init, open_window, prelude::*, px, size, uniform_list,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::mpsc::Receiver,
    time::Duration,
};

actions!(organizer, [Next, Previous, ApproveNext, UndoLast]);
#[derive(Clone, Copy, PartialEq)]
enum Panel {
    Browse,
    Sources,
    Tags,
    Moves,
}

struct Organizer {
    worker: Worker,
    events: Receiver<Event>,
    snapshot: Snapshot,
    panel: Panel,
    search: Entity<InputState>,
    query: String,
    filter: usize,
    tag_filter: Option<i64>,
    visible: Vec<usize>,
    selected: Option<i64>,
    checked: BTreeSet<i64>,
    selected_move: Option<i64>,
    checked_moves: BTreeSet<i64>,
    details: Option<Item>,
    preview: Option<Preview>,
    page: u16,
    zoom: u32,
    text_view: bool,
    message: String,
    tag_name: Entity<InputState>,
    tag_description: Entity<InputState>,
    editing_tag: Option<i64>,
    editing_suggestion: Option<i64>,
    category_name: Entity<InputState>,
    editing_category: Option<i64>,
    destination: Entity<InputState>,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
    _poll: Task<()>,
    thumbnail_requests: BTreeSet<(i64, i64)>,
    search_hits: BTreeSet<i64>,
    list_scroll: UniformListScrollHandle,
}

fn input<T: 'static>(
    value: &str,
    placeholder: &str,
    window: &mut Window,
    cx: &mut Context<T>,
) -> Entity<InputState> {
    let e = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder.to_owned()));
    e.update(cx, |s, cx| s.set_value(value.to_owned(), window, cx));
    e
}
fn set_input<T: 'static>(
    e: &Entity<InputState>,
    value: &str,
    window: &mut Window,
    cx: &mut Context<T>,
) {
    e.update(cx, |s, cx| s.set_value(value.to_owned(), window, cx));
}
fn command_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    worker: &Worker,
    command: Command,
) -> Button {
    let w = worker.clone();
    Button::new(id)
        .label(label)
        .on_click(move |_, _, _| w.send(command.clone()))
}

impl Organizer {
    fn new(
        worker: Worker,
        events: Receiver<Event>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = input("", "Search filenames and extracted text", window, cx);
        let subscription = cx.subscribe(&search, |this, state, event, cx| {
            if matches!(event, InputEvent::Change) {
                this.query = state.read(cx).value().to_string().to_lowercase();
                this.search_hits.clear();
                this.worker.search(this.query.clone());
                this.refilter();
                cx.notify();
            }
        });
        let poll = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                if this.update(cx, |this, cx| this.poll(cx)).is_err() {
                    break;
                }
            }
        });
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        Self {
            worker,
            events,
            snapshot: Snapshot::default(),
            panel: Panel::Browse,
            search,
            query: String::new(),
            filter: 0,
            tag_filter: None,
            visible: Vec::new(),
            selected: None,
            checked: BTreeSet::new(),
            selected_move: None,
            checked_moves: BTreeSet::new(),
            details: None,
            preview: None,
            page: 0,
            zoom: 1000,
            text_view: false,
            message:
                "Add source directories to begin. Nothing moves until you approve a destination."
                    .into(),
            tag_name: input("", "Tag name", window, cx),
            tag_description: input("", "Description: what belongs under this tag?", window, cx),
            editing_tag: None,
            editing_suggestion: None,
            category_name: input("", "Filing category", window, cx),
            editing_category: None,
            destination: input("", "Absolute destination path", window, cx),
            focus,
            _subscriptions: vec![subscription],
            _poll: poll,
            thumbnail_requests: BTreeSet::new(),
            search_hits: BTreeSet::new(),
            list_scroll: UniformListScrollHandle::new(),
        }
    }
    fn refilter(&mut self) {
        self.visible =
            self.snapshot
                .items
                .iter()
                .enumerate()
                .filter(|(_, i)| i.project_id.is_none())
                .filter(|(_, i)| {
                    self.query.is_empty()
                        || (i.current_path.to_lowercase().contains(&self.query)
                            || self.search_hits.contains(&i.id))
                })
                .filter(|(_, i)| match self.filter {
                    1 => i.relevance == "needs review",
                    2 => i.relevance == "relevant",
                    3 => i.relevance == "excluded",
                    4 => i.status == "failed",
                    _ => true,
                })
                .filter(|(_, i)| {
                    self.tag_filter.is_none_or(|tag| {
                        self.snapshot.item_tags.get(&i.id).is_some_and(|tags| {
                            tags.iter().any(|t| t.tag_id == tag && t.assigned())
                        })
                    })
                })
                .map(|(n, _)| n)
                .collect();
    }
    fn item(&self) -> Option<&Item> {
        self.selected
            .and_then(|id| self.snapshot.items.iter().find(|i| i.id == id))
    }
    fn poll(&mut self, cx: &mut Context<Self>) {
        let mut changed = false;
        while let Ok(event) = self.events.try_recv() {
            changed = true;
            match event {
                Event::Snapshot(snapshot) => {
                    let pdf_changed =
                        self.snapshot.settings.pdfium_library != snapshot.settings.pdfium_library;
                    if pdf_changed {
                        self.thumbnail_requests.clear();
                    }
                    let old = self
                        .item()
                        .map(|i| (i.id, i.revision, i.current_path.clone(), i.status.clone()));
                    self.snapshot = *snapshot;
                    self.refilter();
                    if self.selected.is_none() {
                        self.selected = self.visible.first().map(|n| self.snapshot.items[*n].id);
                    }
                    let new = self
                        .item()
                        .map(|i| (i.id, i.revision, i.current_path.clone(), i.status.clone()));
                    if old != new || pdf_changed {
                        self.request_preview();
                    }
                    if !self.query.is_empty() {
                        self.worker.search(self.query.clone());
                    }
                }
                Event::Details(item) => {
                    if self.selected == Some(item.id)
                        && self.item().is_some_and(|i| i.revision == item.revision)
                    {
                        self.details = Some(*item);
                    }
                }
                Event::Preview(p) => {
                    if self.selected == Some(p.item_id)
                        && self.page == p.page
                        && self.item().is_some_and(|i| i.revision == p.revision)
                    {
                        self.preview = Some(p);
                    }
                }
                Event::Thumbnail(id, revision, path) => {
                    if self
                        .snapshot
                        .items
                        .iter()
                        .any(|i| i.id == id && i.revision == revision)
                        && let Some(p) = path
                    {
                        self.snapshot
                            .thumbnails
                            .insert(id, p.to_string_lossy().into_owned());
                    }
                }
                Event::Search(query, ids) => {
                    if self.query == query {
                        self.search_hits = ids.into_iter().collect();
                        self.refilter();
                    }
                }
                Event::Progress(s) => self.message = s,
                Event::Error(s) => self.message = s,
            }
        }
        if changed {
            cx.notify();
        }
    }
    fn request_preview(&mut self) {
        self.preview = None;
        self.details = None;
        if let Some(item) = self.item().cloned() {
            let next = self
                .visible
                .iter()
                .position(|n| self.snapshot.items[*n].id == item.id)
                .and_then(|n| self.visible.get(n + 1))
                .map(|n| self.snapshot.items[*n].clone());
            self.worker.preview(
                item,
                self.page,
                self.zoom,
                self.snapshot.settings.clone(),
                next,
            );
        }
    }
    fn select(&mut self, id: i64, cx: &mut Context<Self>) {
        self.selected = Some(id);
        if let Some(position) = self
            .visible
            .iter()
            .position(|n| self.snapshot.items[*n].id == id)
        {
            self.list_scroll
                .scroll_to_item(position, ScrollStrategy::Nearest);
        }
        self.page = 0;
        self.request_preview();
        cx.notify();
    }
    fn advance(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.visible.is_empty() {
            return;
        }
        let pos = self
            .visible
            .iter()
            .position(|n| Some(self.snapshot.items[*n].id) == self.selected)
            .unwrap_or(0);
        let next = (pos as isize + delta).clamp(0, self.visible.len() as isize - 1) as usize;
        self.select(self.snapshot.items[self.visible[next]].id, cx);
    }
    fn approve_next(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.selected {
            if let Some(op) = self
                .snapshot
                .moves
                .iter()
                .find(|o| o.item_id == id && o.state == "planned")
            {
                self.worker.send(Command::Approve(vec![op.id]));
                self.advance(1, cx);
            } else {
                self.message = "Prepare a valid move destination before approving.".into();
                cx.notify();
            }
        }
    }
    fn undo_last(&mut self) {
        if let Some(op) = self
            .snapshot
            .moves
            .iter()
            .find(|o| o.state == "completed" && o.undo_of.is_none())
        {
            self.worker.send(Command::Undo(op.id));
        }
    }
    fn choose_source(&mut self, cx: &mut Context<Self>) {
        let worker = self.worker.clone();
        cx.spawn(async move |_, cx| {
            let path = cx
                .background_executor()
                .spawn(async move {
                    rfd::FileDialog::new()
                        .set_title("Choose a source directory")
                        .pick_folder()
                })
                .await;
            if let Some(path) = path {
                worker.send(Command::AddSource(path));
            }
        })
        .detach();
    }
    fn settings(&self, window: &mut Window, cx: &mut Context<Self>) {
        let s = self.snapshot.settings.clone();
        let worker = self.worker.clone();
        let view = cx.new(|cx| SettingsView::new(s, worker, window, cx));
        window.open_dialog(cx, move |d, _, _| {
            d.title("Settings").w(px(720.)).child(view.clone())
        });
    }
    fn prepare(&mut self) {
        let ids = if self.checked.is_empty() {
            self.visible
                .iter()
                .map(|n| &self.snapshot.items[*n])
                .filter(|i| i.relevance == "relevant" && !i.missing)
                .map(|i| i.id)
                .collect()
        } else {
            self.checked.iter().copied().collect()
        };
        self.worker.send(Command::Prepare(ids));
        self.panel = Panel::Moves;
    }
    fn row(&mut self, item: &Item, cx: &mut Context<Self>) -> Stateful<Div> {
        let id = item.id;
        let category = self
            .snapshot
            .categories
            .iter()
            .find(|c| c.id == item.category_id)
            .map(|c| c.name.as_str())
            .unwrap_or("Uncategorized");
        let selected = self.selected == Some(id);
        let visual = item.kind == Kind::Image
            || Path::new(&item.current_path)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("pdf"));
        if visual
            && !item.missing
            && !self.thumbnail_requests.contains(&(id, item.revision))
            && self
                .worker
                .thumbnail(item.clone(), self.snapshot.settings.clone())
        {
            self.thumbnail_requests.insert((id, item.revision));
        }
        let thumb = if let Some(p) = self.snapshot.thumbnails.get(&id) {
            img(PathBuf::from(p))
                .size(px(40.))
                .object_fit(ObjectFit::Contain)
                .into_any_element()
        } else {
            div()
                .size(px(40.))
                .text_xs()
                .flex()
                .items_center()
                .justify_center()
                .child(item.kind.as_str())
                .into_any_element()
        };
        div()
            .id(("item", id as u64))
            .h(px(68.))
            .px_3()
            .py_2()
            .flex()
            .items_center()
            .gap_3()
            .border_b_1()
            .border_color(cx.theme().border)
            .bg(if selected {
                cx.theme().secondary
            } else {
                cx.theme().background
            })
            .cursor_pointer()
            .child(
                Checkbox::new(("check", id as u64))
                    .checked(self.checked.contains(&id))
                    .on_change(cx.listener(move |this, value, _, cx| {
                        if *value {
                            this.checked.insert(id);
                        } else {
                            this.checked.remove(&id);
                        }
                        cx.notify();
                    })),
            )
            .child(thumb)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div().truncate().child(
                            Path::new(&item.current_path)
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .into_owned(),
                        ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .truncate()
                            .child(format!(
                                "{} · {} · {}",
                                item.kind.as_str(),
                                category,
                                item.relevance
                            )),
                    ),
            )
            .child(div().text_xs().child(if item.missing {
                "missing".into()
            } else {
                item.status.clone()
            }))
            .on_click(cx.listener(move |this, _, _, cx| this.select(id, cx)))
    }
    fn browse(&self, cx: &mut Context<Self>) -> AnyElement {
        let entity = cx.entity().downgrade();
        let list = uniform_list("documents", self.visible.len(), move |range, _, cx| {
            entity
                .update(cx, |this, cx| {
                    range
                        .map(|n| {
                            let item = this.snapshot.items[this.visible[n]].clone();
                            this.row(&item, cx)
                        })
                        .collect()
                })
                .unwrap_or_default()
        })
        .track_scroll(&self.list_scroll)
        .h_full();
        let filter_names = ["All", "Needs review", "Relevant", "Excluded", "Failed"];
        let tag_label = self
            .tag_filter
            .and_then(|id| self.snapshot.tags.iter().find(|t| t.id == id))
            .map(|t| t.name.clone())
            .unwrap_or("All tags".into());
        h_resizable("review-panes")
            .child(
                resizable_panel()
                    .size(px(430.))
                    .size_range(px(260.)..px(750.))
                    .child(
                        div()
                            .w_full()
                            .h_full()
                            .flex()
                            .flex_col()
                            .border_r_1()
                            .border_color(cx.theme().border)
                            .child(
                                div()
                                    .p_3()
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .child(Input::new(&self.search))
                                    .child(
                                        div()
                                            .flex()
                                            .gap_2()
                                            .child(
                                                Button::new("filter")
                                                    .label(filter_names[self.filter])
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.filter = (this.filter + 1) % 5;
                                                        this.refilter();
                                                        cx.notify();
                                                    })),
                                            )
                                            .child(
                                                Button::new("tag-filter")
                                                    .label(tag_label)
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        let current =
                                                            this.tag_filter.and_then(|id| {
                                                                this.snapshot
                                                                    .tags
                                                                    .iter()
                                                                    .position(|t| t.id == id)
                                                            });
                                                        this.tag_filter = match current {
                                                            None => this
                                                                .snapshot
                                                                .tags
                                                                .first()
                                                                .map(|t| t.id),
                                                            Some(n) => this
                                                                .snapshot
                                                                .tags
                                                                .get(n + 1)
                                                                .map(|t| t.id),
                                                        };
                                                        this.refilter();
                                                        cx.notify();
                                                    })),
                                            ),
                                    ),
                            )
                            .child(div().px_3().pb_2().text_xs().child(format!(
                                "{} items · {} selected",
                                self.visible.len(),
                                self.checked.len()
                            )))
                            .child(div().flex_1().min_h_0().child(list)),
                    ),
            )
            .child(resizable_panel().child(self.detail(cx)))
            .into_any_element()
    }
    fn detail(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(item) = self.item() else {
            return div()
                .flex_1()
                .p_8()
                .child("Choose source directories, then Scan. Select an item to review it.")
                .into_any_element();
        };
        let id = item.id;
        let path = item.current_path.clone();
        let open_path = path.clone();
        let extracted = self
            .details
            .as_ref()
            .filter(|i| i.id == id && i.revision == item.revision)
            .map(|i| i.content.as_str())
            .unwrap_or("");
        let mut category_row = div().flex().flex_wrap().gap_2();
        for category in &self.snapshot.categories {
            let c = category.id;
            let button = command_button(
                ("category", c as u64),
                category.name.clone(),
                &self.worker,
                Command::Category(id, c),
            );
            category_row = category_row.child(if c == item.category_id {
                button.primary()
            } else {
                button
            });
        }
        let mut tag_row = div().flex().flex_wrap().gap_2();
        for tag in &self.snapshot.tags {
            let t = tag.id;
            let assigned = self
                .snapshot
                .item_tags
                .get(&id)
                .and_then(|tags| tags.iter().find(|v| v.tag_id == t));
            let checked = assigned.is_some_and(ItemTag::assigned);
            let p = assigned
                .and_then(|t| t.probability)
                .map(|p| format!(" {:.0}%", p * 100.))
                .unwrap_or_default();
            tag_row = tag_row.child(
                Checkbox::new(("manual-tag", t as u64))
                    .label(format!("{}{p}", tag.name))
                    .checked(checked)
                    .on_change(cx.listener(move |this, v, _, _| {
                        this.worker.send(Command::ManualTag(id, t, Some(*v)))
                    })),
            );
        }
        let preview = if self.text_view {
            div()
                .p_3()
                .child(if extracted.is_empty() {
                    "No extracted text. See limitations below or open the original.".into()
                } else {
                    document_organizer::content::excerpt(extracted, 24000)
                })
                .into_any_element()
        } else if let Some(preview) = &self.preview {
            if let Some(path) = &preview.path {
                img(path.clone())
                    .w(px(self.zoom as f32))
                    .object_fit(ObjectFit::Contain)
                    .into_any_element()
            } else {
                div()
                    .p_4()
                    .child(if preview.error.is_empty() {
                        "No visual preview for this format. Use Text or Open original.".into()
                    } else {
                        preview.error.clone()
                    })
                    .into_any_element()
            }
        } else {
            div().p_4().child("Loading preview…").into_any_element()
        };
        let pages = self.preview.as_ref().map(|p| p.pages).unwrap_or(0);
        let op = self.snapshot.moves.iter().find(|o| {
            o.item_id == id && ["planned", "approved", "blocked"].contains(&o.state.as_str())
        });
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .h_full()
            .child(
                div()
                    .p_3()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        Button::new("previous")
                            .label("Previous")
                            .on_click(cx.listener(|this, _, _, cx| this.advance(-1, cx))),
                    )
                    .child(
                        Button::new("next")
                            .label("Next")
                            .on_click(cx.listener(|this, _, _, cx| this.advance(1, cx))),
                    )
                    .child(
                        Button::new("approve-next")
                            .label("Approve + next")
                            .disabled(op.is_none_or(|o| o.state != "planned"))
                            .on_click(cx.listener(|this, _, _, cx| this.approve_next(cx))),
                    )
                    .child(
                        Button::new("open-original")
                            .label("Open original")
                            .on_click(move |_, _, _| {
                                let path = open_path.clone();
                                std::thread::spawn(move || {
                                    let _ = open::that(path);
                                });
                            }),
                    )
                    .child(
                        Button::new("text-view")
                            .label(if self.text_view { "Preview" } else { "Text" })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.text_view = !this.text_view;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("page-back")
                            .label("Page −")
                            .disabled(self.page == 0)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.page = this.page.saturating_sub(1);
                                this.request_preview();
                                cx.notify();
                            })),
                    )
                    .child(div().child(format!("{} / {}", self.page + 1, pages.max(1))))
                    .child(
                        Button::new("page-next")
                            .label("Page +")
                            .disabled(self.page + 1 >= pages)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.page += 1;
                                this.request_preview();
                                cx.notify();
                            })),
                    )
                    .child(Button::new("zoom-out").label("−").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.zoom = (this.zoom.saturating_sub(250)).max(500);
                            this.request_preview();
                            cx.notify();
                        },
                    )))
                    .child(Button::new("zoom-in").label("+").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.zoom = (this.zoom + 250).min(2500);
                            this.request_preview();
                            cx.notify();
                        },
                    ))),
            )
            .child(
                div()
                    .id("preview-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_scroll()
                    .bg(cx.theme().muted)
                    .child(preview),
            )
            .child(
                div()
                    .id("details-scroll")
                    .max_h(px(310.))
                    .overflow_y_scroll()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(div().text_xs().child(format!(
                        "Original: {}\nCurrent: {}",
                        item.original_path, path
                    )))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(command_button(
                                "include",
                                "Include",
                                &self.worker,
                                Command::Relevance(id, "relevant".into()),
                            ))
                            .child(command_button(
                                "exclude",
                                "Exclude",
                                &self.worker,
                                Command::Relevance(id, "excluded".into()),
                            ))
                            .child(div().child(format!(
                                    "{} · confidence {}",
                                    item.relevance,
                                    item.confidence
                                        .map(|p| format!("{:.0}%", p * 100.))
                                        .unwrap_or("unknown".into())
                                ))),
                    )
                    .child(category_row)
                    .child(tag_row)
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!(
                                "{}\n{}\n{}",
                                item.limitations, item.block_reason, item.error
                            )),
                    )
                    .child(
                        div().text_xs().child(
                            op.map(|o| {
                                format!("Destination ({}): {}\n{}", o.state, o.destination, o.error)
                            })
                            .unwrap_or_default(),
                        ),
                    ),
            )
            .into_any_element()
    }
    fn sources(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut body=div().id("sources-scroll").overflow_y_scroll().size_full().p_5().flex().flex_col().gap_3()
            .child(div().text_lg().child("Source directories"))
            .child(Button::new("add-source").label("Add directory").on_click(cx.listener(|this,_,_,cx|this.choose_source(cx))))
            .child("Overlapping roots are deduplicated. Projects move as complete folders; generated contents stay with them.");
        for source in &self.snapshot.sources {
            body = body.child(
                div()
                    .flex()
                    .gap_3()
                    .items_center()
                    .child(div().flex_1().child(source.path.clone()))
                    .child(command_button(
                        ("remove-source", source.id as u64),
                        "Remove",
                        &self.worker,
                        Command::RemoveSource(source.id),
                    )),
            );
        }
        body = body.child(
            div()
                .mt_4()
                .child("Skipped paths (Include override takes effect on the next scan)"),
        );
        for (n, e) in self.snapshot.exclusions.iter().take(200).enumerate() {
            body = body.child(
                div()
                    .flex()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .child(format!("{}\n{}", e.path, e.reason)),
                    )
                    .child(command_button(
                        ("include-exclusion", n as u64),
                        "Include",
                        &self.worker,
                        Command::Override(e.path.clone(), true),
                    )),
            );
        }
        body.into_any_element()
    }
    fn tags(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut body=div().id("tags-scroll").overflow_y_scroll().size_full().p_5().flex().flex_col().gap_3().child(div().text_lg().child("Searchable tags"))
            .child("Tags do not determine folders. Saving a tag queues scoring of previously relevant items.")
            .child(Input::new(&self.tag_name)).child(Input::new(&self.tag_description))
            .child(div().flex().gap_2().child(Button::new("save-tag").primary().label(if self.editing_suggestion.is_some(){"Accept edited suggestion"}else if self.editing_tag.is_some(){"Save tag changes"}else{"Add tag"}).on_click(cx.listener(|this,_,window,cx|{
                let name=this.tag_name.read(cx).value().to_string();let desc=this.tag_description.read(cx).value().to_string();
                if let Some(id)=this.editing_suggestion.take(){this.worker.send(Command::AcceptSuggestion(id,name,desc));}else{this.worker.send(Command::SaveTag(this.editing_tag.take(),name,desc));}
                set_input(&this.tag_name,"",window,cx);set_input(&this.tag_description,"",window,cx);cx.notify();
            }))).child(Button::new("clear-tag-edit").label("New tag").on_click(cx.listener(|this,_,window,cx|{this.editing_tag=None;this.editing_suggestion=None;set_input(&this.tag_name,"",window,cx);set_input(&this.tag_description,"",window,cx);cx.notify();}))).child(command_button("generate-tags","Suggest tags",&self.worker,Command::Suggest)));
        for tag in &self.snapshot.tags {
            let t = tag.clone();
            body = body.child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .child(format!("{} — {}", tag.name, tag.description)),
                    )
                    .child(
                        Button::new(("edit-tag", tag.id as u64))
                            .label("Edit")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.editing_tag = Some(t.id);
                                this.editing_suggestion = None;
                                set_input(&this.tag_name, &t.name, window, cx);
                                set_input(&this.tag_description, &t.description, window, cx);
                                cx.notify();
                            })),
                    ),
            );
        }
        body = body.child(div().mt_4().text_lg().child("Suggested tags"));
        for s in &self.snapshot.suggestions {
            let suggestion = s.clone();
            body = body.child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .child(format!("{} — {}", s.name, s.description)),
                    )
                    .child(command_button(
                        ("accept", s.id as u64),
                        "Accept",
                        &self.worker,
                        Command::AcceptSuggestion(s.id, s.name.clone(), s.description.clone()),
                    ))
                    .child(
                        Button::new(("edit-suggestion", s.id as u64))
                            .label("Edit")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.editing_suggestion = Some(suggestion.id);
                                this.editing_tag = None;
                                set_input(&this.tag_name, &suggestion.name, window, cx);
                                set_input(
                                    &this.tag_description,
                                    &suggestion.description,
                                    window,
                                    cx,
                                );
                                cx.notify();
                            })),
                    )
                    .child(command_button(
                        ("reject", s.id as u64),
                        "Reject",
                        &self.worker,
                        Command::RejectSuggestion(s.id),
                    )),
            );
        }
        body = body
            .child(div().mt_5().text_lg().child("Filing categories"))
            .child(Input::new(&self.category_name))
            .child(
                Button::new("save-category")
                    .label(if self.editing_category.is_some() {
                        "Save category"
                    } else {
                        "Add category"
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.worker.send(Command::SaveCategory(
                            this.editing_category.take(),
                            this.category_name.read(cx).value().to_string(),
                        ));
                        set_input(&this.category_name, "", window, cx);
                        cx.notify();
                    })),
            );
        let mut categories = div().flex().flex_wrap().gap_2();
        for c in &self.snapshot.categories {
            let category = c.clone();
            categories = categories.child(
                Button::new(("edit-category", c.id as u64))
                    .label(c.name.clone())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.editing_category = Some(category.id);
                        set_input(&this.category_name, &category.name, window, cx);
                        cx.notify();
                    })),
            );
        }
        body.child(categories).into_any_element()
    }
    fn moves(&self, cx: &mut Context<Self>) -> AnyElement {
        let entity = cx.entity().downgrade();
        let list = uniform_list(
            "move-list",
            self.snapshot.moves.len(),
            move |range, _, cx| {
                entity
                    .update(cx, |this, cx| {
                        range
                            .map(|n| {
                                let op = &this.snapshot.moves[n];
                                let id = op.id;
                                let destination = op.destination.clone();
                                div()
                                    .id(("move", id as u64))
                                    .h(px(84.))
                                    .p_3()
                                    .flex()
                                    .gap_3()
                                    .border_b_1()
                                    .border_color(cx.theme().border)
                                    .child(
                                        Checkbox::new(("move-checkbox", id as u64))
                                            .checked(this.checked_moves.contains(&id))
                                            .on_change(cx.listener(move |this, v, _, cx| {
                                                if *v {
                                                    this.checked_moves.insert(id);
                                                } else {
                                                    this.checked_moves.remove(&id);
                                                }
                                                cx.notify();
                                            })),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .child(div().truncate().child(op.source.clone()))
                                            .child(
                                                div()
                                                    .truncate()
                                                    .text_xs()
                                                    .child(format!("→ {}", op.destination)),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .child(format!("{} {}", op.state, op.error)),
                                            ),
                                    )
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.selected_move = Some(id);
                                        set_input(&this.destination, &destination, window, cx);
                                        cx.notify();
                                    }))
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            },
        )
        .h_full();
        let op = self
            .selected_move
            .and_then(|id| self.snapshot.moves.iter().find(|o| o.id == id));
        let mut details = div()
            .w(px(430.))
            .p_4()
            .flex()
            .flex_col()
            .gap_3()
            .child("Review the exact destination, then approve. Only approved operations run.");
        if let Some(op) = op {
            let id = op.id;
            let item_id = op.item_id;
            details = details
                .child(format!(
                    "Source:\n{}\n\nDestination:\n{}\n\nState: {}\n{}\nTemporary copy: {}",
                    op.source, op.destination, op.state, op.error, op.temp_path
                ))
                .child(Input::new(&self.destination))
                .child(
                    Button::new("change-destination")
                        .label("Prepare edited destination")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.worker.send(Command::Destination(
                                item_id,
                                PathBuf::from(this.destination.read(cx).value().to_string()),
                            ))
                        })),
                )
                .child(
                    command_button(
                        "approve-move",
                        "Approve destination",
                        &self.worker,
                        Command::Approve(vec![id]),
                    )
                    .disabled(op.state != "planned"),
                )
                .child(
                    command_button("undo-move", "Undo move", &self.worker, Command::Undo(id))
                        .disabled(op.state != "completed" || op.undo_of.is_some()),
                )
                .child(
                    command_button(
                        "recover-move",
                        "Retry recovery",
                        &self.worker,
                        Command::Recover(id),
                    )
                    .disabled(op.state != "recovery"),
                );
        }
        div()
            .size_full()
            .flex()
            .flex_col()
            .min_h_0()
            .child(
                div()
                    .p_3()
                    .flex()
                    .gap_2()
                    .child(
                        Button::new("approve-selected")
                            .label("Approve selected destinations")
                            .disabled(self.checked_moves.is_empty())
                            .on_click(cx.listener(|this, _, _, _| {
                                this.worker.send(Command::Approve(
                                    this.checked_moves.iter().copied().collect(),
                                ))
                            })),
                    )
                    .child(
                        command_button(
                            "apply-moves",
                            "Move approved",
                            &self.worker,
                            Command::Apply,
                        )
                        .primary(),
                    )
                    .child(command_button(
                        "retry-analysis",
                        "Retry failed analysis",
                        &self.worker,
                        Command::Retry,
                    )),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(div().flex_1().min_w_0().child(list))
                    .child(details),
            )
            .into_any_element()
    }
}
impl Render for Organizer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut toolbar = div()
            .p_3()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(div().text_lg().mr_4().child("Document Organizer"));
        for (id, label, panel) in [
            ("browse", "Review", Panel::Browse),
            ("sources", "Sources", Panel::Sources),
            ("tags", "Tags", Panel::Tags),
            ("moves", "Moves", Panel::Moves),
        ] {
            let b = Button::new(id)
                .label(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.panel = panel;
                    cx.notify();
                }));
            toolbar = toolbar.child(if self.panel == panel { b.primary() } else { b });
        }
        toolbar = toolbar
            .child(command_button("scan", "Scan", &self.worker, Command::Scan))
            .child(
                Button::new("suggest")
                    .label("Suggest tags")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.panel = Panel::Tags;
                        this.worker.send(Command::Suggest);
                        cx.notify();
                    })),
            )
            .child(
                Button::new("prepare")
                    .label("Prepare moves")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.prepare();
                        cx.notify();
                    })),
            )
            .child(
                Button::new("settings")
                    .label("Settings")
                    .on_click(cx.listener(|this, _, window, cx| this.settings(window, cx))),
            );
        let body = match self.panel {
            Panel::Browse => self.browse(cx),
            Panel::Sources => self.sources(cx),
            Panel::Tags => self.tags(cx),
            Panel::Moves => self.moves(cx),
        };
        let count = |key: &str| self.snapshot.counts.get(key).copied().unwrap_or(0);
        div().size_full().flex().flex_col().bg(cx.theme().background).text_color(cx.theme().foreground).key_context("Organizer").track_focus(&self.focus)
            .on_action(cx.listener(|this,_:&Next,_,cx|this.advance(1,cx))).on_action(cx.listener(|this,_:&Previous,_,cx|this.advance(-1,cx)))
            .on_action(cx.listener(|this,_:&ApproveNext,_,cx|this.approve_next(cx))).on_action(cx.listener(|this,_:&UndoLast,_,_|this.undo_last()))
            .child(toolbar).child(div().flex_1().min_h_0().child(body))
            .child(div().border_t_1().border_color(cx.theme().border).p_2().flex().items_center().gap_3()
                .child(div().text_xs().child(format!("Queued {} · Processing {} · Review {} · Completed {} · Failed {}",count("queued"),count("running"),self.snapshot.items.iter().filter(|i|i.project_id.is_none()&&i.relevance=="needs review").count(),count("completed"),count("failed"))))
                .child(div().flex_1().min_w_0().truncate().text_xs().child(self.message.clone()))
                .child(Button::new("cancel").label("Cancel").on_click(cx.listener(|this,_,_,cx|{this.worker.cancel();this.message="Cancellation requested; current HTTP request can take up to 60 seconds.".into();cx.notify();})))
                .child(command_button("retry","Retry",&self.worker,Command::Retry)))
    }
}
impl Drop for Organizer {
    fn drop(&mut self) {
        self.worker.cancel();
        self.worker.send(Command::Shutdown);
    }
}

struct SettingsView {
    worker: Worker,
    decision: Entity<InputState>,
    decision_version: Entity<InputState>,
    generative: Entity<InputState>,
    model: Entity<InputState>,
    pdfium: Entity<InputState>,
    destinations: BTreeMap<String, Entity<InputState>>,
}
impl SettingsView {
    fn new(
        settings: Settings,
        worker: Worker,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let destinations = ["image", "document", "audio", "video", "project", "other"]
            .into_iter()
            .map(|k| {
                (
                    k.into(),
                    input(
                        settings
                            .destinations
                            .get(k)
                            .map(String::as_str)
                            .unwrap_or(""),
                        "Absolute folder path",
                        window,
                        cx,
                    ),
                )
            })
            .collect();
        Self {
            worker,
            decision: input(
                &settings.decision_url,
                "Decision URL (/v1/systemone)",
                window,
                cx,
            ),
            decision_version: input(
                &settings.decision_model_version,
                "Model revision / quantization for analysis history",
                window,
                cx,
            ),
            generative: input(
                &settings.generative_url,
                "Generative URL (/v1/chat/completions)",
                window,
                cx,
            ),
            model: input(
                &settings.generative_model,
                "Generative model ID",
                window,
                cx,
            ),
            pdfium: input(
                &settings.pdfium_library,
                "Optional path to pdfium.dll",
                window,
                cx,
            ),
            destinations,
        }
    }
    fn config(&self, cx: &App) -> Settings {
        Settings {
            decision_url: self.decision.read(cx).value().to_string(),
            decision_model_version: self.decision_version.read(cx).value().to_string(),
            generative_url: self.generative.read(cx).value().to_string(),
            generative_model: self.model.read(cx).value().to_string(),
            pdfium_library: self.pdfium.read(cx).value().to_string(),
            destinations: self
                .destinations
                .iter()
                .map(|(k, v)| (k.clone(), v.read(cx).value().to_string()))
                .collect(),
        }
    }
}
impl Render for SettingsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut body = div()
            .id("settings-scroll")
            .max_h(px(650.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_2()
            .child("Decision endpoint (must accept images)")
            .child(Input::new(&self.decision))
            .child("Decision model version (history label)")
            .child(Input::new(&self.decision_version))
            .child("Generative endpoint and model")
            .child(Input::new(&self.generative))
            .child(Input::new(&self.model))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        Button::new("test-decision")
                            .label("Save + test decision")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.worker.send(Command::TestDecision(this.config(cx)));
                            })),
                    )
                    .child(
                        Button::new("test-generative")
                            .label("Save + test generative")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.worker.send(Command::TestGenerative(this.config(cx)));
                            })),
                    ),
            )
            .child("PDFium library (blank: beside executable or system library)")
            .child(Input::new(&self.pdfium))
            .child("Destination folders; category and original filename are appended");
        for (k, v) in &self.destinations {
            body = body.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(90.)).child(k.clone()))
                    .child(Input::new(v)),
            );
        }
        body.child(
            Button::new("save-settings")
                .primary()
                .label("Save settings")
                .on_click(cx.listener(|this, _, window, cx| {
                    this.worker.send(Command::Settings(this.config(cx)));
                    window.close_dialog(cx);
                })),
        )
    }
}

pub fn run() -> anyhow::Result<()> {
    let data = std::env::var_os("DOCUMENT_ORGANIZER_DATA")
        .map(PathBuf::from)
        .or_else(|| {
            directories::ProjectDirs::from("local", "DocumentOrganizer", "DocumentOrganizer")
                .map(|d| d.data_local_dir().to_path_buf())
        })
        .ok_or_else(|| anyhow::anyhow!("Cannot find application data directory"))?;
    let (worker, events) = Worker::start(&data)?;
    application().with_assets(assets::Assets).run(move |cx| {
        init(cx);
        cx.bind_keys([
            KeyBinding::new("ctrl-down", Next, Some("Organizer")),
            KeyBinding::new("ctrl-up", Previous, Some("Organizer")),
            KeyBinding::new("ctrl-enter", ApproveNext, Some("Organizer")),
            KeyBinding::new("ctrl-alt-z", UndoLast, Some("Organizer")),
        ]);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1320.), px(900.)),
                cx,
            ))),
            titlebar: Some(TitlebarOptions {
                title: Some("Document Organizer".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        open_window(options, cx, move |window, cx| {
            cx.new(|cx| Organizer::new(worker, events, window, cx))
        })
        .expect("Cannot open organizer window");
        cx.activate(true);
    });
    Ok(())
}

#[cfg(all(test, feature = "ui-tests"))]
#[path = "ui_tests.rs"]
mod tests;
