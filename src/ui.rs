use crate::{
    engine::{self, Action, Download, Event, Status},
    settings::Settings,
};
use adw::prelude::*;
use gtk::{gdk, gio, glib, pango};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    path::PathBuf,
    rc::Rc,
    sync::mpsc::{Receiver, Sender, TryRecvError},
    time::{Duration, Instant},
};

mod dialogs;

use dialogs::{show_add_dialog, show_login_dialog, show_settings_dialog};

const APP_ID: &str = "io.github.potik.Downloads";
const CONNECTION_WAIT: Duration = Duration::from_secs(4);

pub fn run() -> glib::ExitCode {
    gio::resources_register_include!("potik.gresource").expect("Potik's GTK resources are valid");
    let app = adw::Application::builder().application_id(APP_ID).build();
    let worker = Rc::new(RefCell::new(None));
    app.connect_activate({
        let worker = worker.clone();
        move |app| {
            if let Some(window) = app.active_window() {
                window.present();
                return;
            }
            if let Some(display) = gdk::Display::default() {
                let css = gtk::CssProvider::new();
                css.load_from_data(include_str!("style.css"));
                gtk::style_context_add_provider_for_display(
                    &display,
                    &css,
                    gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
                );
            }
            let (actions, events, join) = engine::spawn(glib::user_data_dir().join("potik"));
            *worker.borrow_mut() = Some((actions.clone(), join));
            build_window(app, &actions, events).present();
        }
    });
    app.connect_shutdown(move |_| {
        if let Some((actions, join)) = worker.borrow_mut().take() {
            let _ = actions.send(Action::Shutdown);
            if join.join().is_err() {
                eprintln!("potik: aria2 worker stopped unexpectedly");
            }
        }
    });
    app.set_accels_for_action("win.add-download", &["<Primary>n"]);
    app.run()
}

#[derive(Clone, Copy)]
enum Filter {
    All,
    Active,
    Queue,
    Done,
    Errors,
}

impl Filter {
    const ITEMS: [Self; 5] = [
        Self::All,
        Self::Active,
        Self::Queue,
        Self::Done,
        Self::Errors,
    ];

    fn matches(self, status: Status) -> bool {
        match self {
            Self::All => status != Status::Removed,
            Self::Active => matches!(status, Status::Active | Status::Verifying),
            Self::Queue => matches!(status, Status::Waiting | Status::Paused),
            Self::Done => status == Status::Complete,
            Self::Errors => status == Status::Error,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::All => "Усі завантаження",
            Self::Active => "Активні",
            Self::Queue => "Черга",
            Self::Done => "Завершені",
            Self::Errors => "Помилки",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::All => "view-list-symbolic",
            Self::Active => "go-down-symbolic",
            Self::Queue => "alarm-symbolic",
            Self::Done => "object-select-symbolic",
            Self::Errors => "dialog-error-symbolic",
        }
    }

    fn empty_title(self) -> &'static str {
        match self {
            Self::All => "Завантажень ще немає",
            Self::Active => "Немає активних завантажень",
            Self::Queue => "Черга порожня",
            Self::Done => "Завершених завантажень ще немає",
            Self::Errors => "Помилок завантаження немає",
        }
    }
}

fn build_window(
    app: &adw::Application,
    actions: &Sender<Action>,
    events: Receiver<Event>,
) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Потік")
        .icon_name(APP_ID)
        .default_width(1000)
        .default_height(650)
        .width_request(390)
        .height_request(330)
        .build();

    let add = gio::SimpleAction::new("add-download", None);
    add.set_enabled(false);
    window.add_action(&add);
    let preferences = gio::SimpleAction::new("preferences", None);
    window.add_action(&preferences);

    let header = adw::HeaderBar::builder().show_title(false).build();
    let logo = image(APP_ID, 29);
    let brand = gtk::Box::new(gtk::Orientation::Horizontal, 9);
    brand.set_margin_start(7);
    brand.append(&logo);
    brand.append(&label("Потік", "heading"));
    header.pack_start(&brand);

    let nav_toggle = button("sidebar-show-symbolic", "Показати категорії");
    nav_toggle.set_visible(false);
    header.pack_start(&nav_toggle);

    header.pack_end(&add_button("Додати URL"));
    let settings_button = button("preferences-system-symbolic", "Налаштування");
    settings_button.set_action_name(Some("win.preferences"));
    header.pack_end(&settings_button);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    window.set_content(Some(&toolbar));

    let split = adw::OverlaySplitView::builder()
        .min_sidebar_width(228.0)
        .max_sidebar_width(228.0)
        .show_sidebar(true)
        .build();
    toolbar.set_content(Some(&split));

    window.connect_realize(glib::clone!(
        #[weak]
        split,
        #[weak]
        nav_toggle,
        #[weak]
        brand,
        move |window| {
            if let Some(surface) = window.surface() {
                surface.connect_layout(glib::clone!(
                    #[weak]
                    split,
                    #[weak]
                    nav_toggle,
                    #[weak]
                    brand,
                    move |_, width, _| {
                        let collapsed = width <= 720;
                        if split.is_collapsed() != collapsed {
                            split.set_collapsed(collapsed);
                            split.set_show_sidebar(!collapsed);
                            nav_toggle.set_visible(collapsed);
                            brand.set_visible(!collapsed);
                        }
                    }
                ));
            }
        }
    ));
    nav_toggle.connect_clicked(glib::clone!(
        #[weak]
        split,
        move |_| split.set_show_sidebar(!split.shows_sidebar())
    ));

    let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 12);
    sidebar.add_css_class("navigation-pane");
    let section_title = label("Бібліотека", "navigation-caption");
    sidebar.append(&section_title);
    let filters = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .build();
    filters.add_css_class("navigation-list");
    sidebar.append(&filters);

    let counts: Vec<_> = Filter::ITEMS
        .iter()
        .map(|filter| {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            let title = label(filter.title(), "navigation-label");
            title.set_hexpand(true);
            let count = label("0", "navigation-count");
            row.append(&image(filter.icon(), 16));
            row.append(&title);
            row.append(&count);
            filters.append(&row);
            count
        })
        .collect();
    filters.select_row(filters.row_at_index(0).as_ref());
    split.set_sidebar(Some(&sidebar));

    let toasts = adw::ToastOverlay::new();
    split.set_content(Some(&toasts));
    let pane = gtk::Box::new(gtk::Orientation::Vertical, 0);
    toasts.set_child(Some(&pane));

    let layout = adw::Clamp::builder()
        .maximum_size(1080)
        .tightening_threshold(760)
        .build();
    paint_margins(&layout, 24, 28);
    let heading = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let title = label(Filter::All.title(), "title-1");
    title.set_wrap(true);
    title.set_hexpand(true);
    heading.append(&title);
    layout.set_child(Some(&heading));
    pane.append(&layout);

    let body = gtk::Stack::builder().vexpand(true).build();
    pane.append(&body);

    let connecting = gtk::Box::new(gtk::Orientation::Vertical, 16);
    connecting.set_valign(gtk::Align::Center);
    connecting.set_halign(gtk::Align::Center);
    let spinner = gtk::Spinner::builder()
        .spinning(true)
        .width_request(28)
        .height_request(28)
        .build();
    connecting.append(&spinner);
    connecting.append(&label("Запуск aria2c…", "dim-label"));
    body.add_named(&connecting, Some("connecting"));

    let empty = adw::StatusPage::builder()
        .icon_name("folder-download-symbolic")
        .title(Filter::All.empty_title())
        .build();
    empty.add_css_class("compact");
    let empty_add = add_button("Додати URL");
    empty_add.set_halign(gtk::Align::Center);
    empty.set_child(Some(&empty_add));
    body.add_named(&empty, Some("empty"));

    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .build();
    list.add_css_class("download-list");
    let list_clamp = adw::Clamp::builder()
        .maximum_size(1080)
        .tightening_threshold(760)
        .child(&list)
        .build();
    list_clamp.set_margin_start(24);
    list_clamp.set_margin_end(24);
    list_clamp.set_margin_bottom(24);
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&list_clamp)
        .build();
    body.add_named(&scroll, Some("list"));

    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    footer.add_css_class("footer");
    let footer_status = label("Підключення…", "footer-status");
    footer_status.set_ellipsize(pango::EllipsizeMode::End);
    footer_status.set_hexpand(true);
    let footer_speed = label("", "numeric");
    let reconnect = gtk::Button::with_label("Повторити");
    reconnect.add_css_class("flat");
    reconnect.add_css_class("compact-action");
    reconnect.set_visible(false);
    footer.append(&footer_status);
    footer.append(&reconnect);
    footer.append(&footer_speed);
    pane.append(&footer);
    body.set_visible_child_name("connecting");

    let settings_dir = glib::user_config_dir().join("potik");
    let (settings, settings_error) = match Settings::load(&settings_dir) {
        Ok(settings) => (settings, None),
        Err(error) => (
            Settings::default(),
            Some(format!("Не вдалося прочитати налаштування. {error}")),
        ),
    };
    let view = Rc::new(View {
        title,
        counts,
        list,
        empty,
        empty_add,
        body,
        footer_status,
        footer_speed,
        reconnect,
        toasts,
        window: window.downgrade(),
        add,
        settings_dir,
        settings: RefCell::new(settings),
        actions: actions.clone(),
        downloads: RefCell::new(vec![]),
        rows: RefCell::new(HashMap::new()),
        visible: RefCell::new(vec![]),
        filter: Cell::new(Filter::All),
        network: gio::NetworkMonitor::default(),
        last_progress: RefCell::new(HashMap::new()),
    });
    view.network.connect_network_changed(glib::clone!(
        #[weak]
        view,
        move |_, _| view.render()
    ));
    if let Some(error) = settings_error {
        view.notify(&error);
    }
    preferences.connect_activate(glib::clone!(
        #[weak]
        window,
        #[weak]
        view,
        move |_, _| show_settings_dialog(&window, &view)
    ));
    filters.connect_row_selected(glib::clone!(
        #[weak]
        split,
        #[weak]
        view,
        move |_, row| {
            if let Some(row) = row {
                view.filter.set(Filter::ITEMS[row.index() as usize]);
                view.render();
                if split.is_collapsed() {
                    split.set_show_sidebar(false);
                }
            }
        }
    ));
    view.add.connect_activate(glib::clone!(
        #[weak]
        window,
        #[weak]
        view,
        move |_, _| show_add_dialog(&window, &view)
    ));
    view.reconnect.connect_clicked(glib::clone!(
        #[weak]
        view,
        move |_| {
            view.reconnect.set_visible(false);
            view.footer_status.set_label("Підключення…");
            view.send(Action::Reconnect);
        }
    ));
    glib::timeout_add_local(Duration::from_millis(100), move || {
        if view.window.upgrade().is_none() {
            return glib::ControlFlow::Break;
        }
        loop {
            match events.try_recv() {
                Ok(Event::Snapshot(downloads)) => view.update(downloads),
                Ok(Event::Notice(message)) => view.notify(&message),
                Ok(Event::Offline(message)) => {
                    view.add.set_enabled(false);
                    view.footer_status.set_label("aria2c недоступний");
                    view.footer_status.set_tooltip_text(Some(&message));
                    view.footer_speed.set_visible(false);
                    if !view.reconnect.is_visible() {
                        view.notify(&message);
                    }
                    view.reconnect.set_visible(true);
                    view.render();
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    view.add.set_enabled(false);
                    view.footer_status.set_label("З’єднання з aria2c втрачено");
                    return glib::ControlFlow::Break;
                }
            }
        }
        glib::ControlFlow::Continue
    });
    window
}

struct View {
    title: gtk::Label,
    counts: Vec<gtk::Label>,
    list: gtk::ListBox,
    empty: adw::StatusPage,
    empty_add: gtk::Button,
    body: gtk::Stack,
    footer_status: gtk::Label,
    footer_speed: gtk::Label,
    reconnect: gtk::Button,
    toasts: adw::ToastOverlay,
    window: glib::WeakRef<adw::ApplicationWindow>,
    add: gio::SimpleAction,
    settings_dir: PathBuf,
    settings: RefCell<Settings>,
    actions: Sender<Action>,
    downloads: RefCell<Vec<Download>>,
    rows: RefCell<HashMap<String, DownloadRow>>,
    visible: RefCell<Vec<String>>,
    filter: Cell<Filter>,
    network: gio::NetworkMonitor,
    last_progress: RefCell<HashMap<String, (u64, Instant)>>,
}

impl View {
    fn update(self: &Rc<Self>, downloads: Vec<Download>) {
        self.add.set_enabled(true);
        self.reconnect.set_visible(false);
        self.footer_status.set_tooltip_text(None);
        *self.downloads.borrow_mut() = downloads;
        self.render();
    }

    fn render(self: &Rc<Self>) {
        let downloads = self.downloads.borrow();
        let connected = self.add.is_enabled();
        let online = self.network.is_network_available();
        let mut last_progress = self.last_progress.borrow_mut();
        last_progress.retain(|gid, _| {
            downloads
                .iter()
                .any(|item| &item.gid == gid && item.status == Status::Active)
        });
        for item in downloads
            .iter()
            .filter(|item| item.status == Status::Active)
        {
            let progress = last_progress
                .entry(item.gid.clone())
                .or_insert_with(|| (item.completed, Instant::now()));
            if progress.0 != item.completed {
                *progress = (item.completed, Instant::now());
            }
        }
        let waiting_for_connection = |item: &Download| {
            matches!(item.status, Status::Active | Status::Waiting)
                && ((!online && item.speed == 0)
                    || last_progress
                        .get(&item.gid)
                        .is_some_and(|(_, since)| since.elapsed() >= CONNECTION_WAIT))
        };
        if connected {
            let active = downloads
                .iter()
                .filter(|item| item.status == Status::Active)
                .count();
            let speed = downloads
                .iter()
                .filter(|item| !waiting_for_connection(item))
                .map(|item| item.speed)
                .sum::<u64>();
            let waiting = speed == 0 && downloads.iter().any(&waiting_for_connection);
            if waiting {
                self.footer_status.set_label("Очікування з’єднання");
            } else {
                self.footer_status
                    .set_label(&format!("Активні · {active} / 3"));
            }
            self.footer_speed
                .set_label(&format!("↓ {}", speed_text(speed)));
            self.footer_speed.set_visible(active > 0 && !waiting);
        }
        let filter = self.filter.get();
        self.title.set_label(filter.title());
        for (filter, count) in Filter::ITEMS.into_iter().zip(&self.counts) {
            let matching = downloads
                .iter()
                .filter(|item| filter.matches(item.status))
                .count();
            count.set_label(&matching.to_string());
        }

        let matching: Vec<_> = downloads
            .iter()
            .filter(|item| filter.matches(item.status))
            .collect();
        let ids: Vec<_> = matching.iter().map(|item| item.gid.clone()).collect();
        if self.body.visible_child_name().as_deref() != Some("connecting")
            || self.add.is_enabled()
            || self.reconnect.is_visible()
        {
            if matching.is_empty() {
                self.empty.set_title(filter.empty_title());
                self.empty_add
                    .set_visible(matches!(filter, Filter::All) && self.add.is_enabled());
                self.body.set_visible_child_name("empty");
            } else {
                self.body.set_visible_child_name("list");
            }
        }

        let mut rows = self.rows.borrow_mut();
        let mut visible = self.visible.borrow_mut();
        if *visible != ids {
            while let Some(row) = self.list.first_child() {
                self.list.remove(&row);
            }
            rows.retain(|id, _| ids.contains(id));
            for item in &matching {
                let row = rows
                    .entry(item.gid.clone())
                    .or_insert_with(|| DownloadRow::new(self, &item.gid));
                self.list.append(&row.root);
            }
            *visible = ids;
        }
        for item in matching {
            if let Some(row) = rows.get(&item.gid) {
                row.update(item, connected, connected && waiting_for_connection(item));
            }
        }
    }

    fn send(&self, action: Action) {
        if self.actions.send(action).is_err() {
            self.notify("З’єднання з aria2c втрачено.");
        }
    }

    fn notify(&self, message: &str) {
        let toast = adw::Toast::new(message);
        toast.set_use_markup(false);
        self.toasts.add_toast(toast);
    }

    fn action_for(self: &Rc<Self>, gid: &str) {
        let Some(item) = self.find(gid) else { return };
        match item.status {
            Status::Active | Status::Waiting => self.send(Action::Pause(item.gid)),
            Status::Paused => self.send(Action::Resume(item.gid)),
            Status::Verifying => {}
            Status::Error if item.requires_login_for_retry() => {
                if let Some(window) = self.window.upgrade() {
                    show_login_dialog(&window, self, &item);
                }
            }
            Status::Error => self.send(Action::Retry {
                gid: item.gid,
                login: None,
            }),
            Status::Complete => self.open(&item, false),
            Status::Removed => {}
        }
    }

    fn find(&self, gid: &str) -> Option<Download> {
        self.downloads
            .borrow()
            .iter()
            .find(|item| item.gid == gid)
            .cloned()
    }

    fn open(&self, item: &Download, folder: bool) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let file = gio::File::for_path(&item.path);
        let launcher = gtk::FileLauncher::new(Some(&file));
        let toasts = self.toasts.downgrade();
        let done = move |result: Result<(), glib::Error>| {
            if let (Err(error), Some(toasts)) = (result, toasts.upgrade()) {
                let toast = adw::Toast::new(&format!("Не вдалося відкрити. {error}"));
                toast.set_use_markup(false);
                toasts.add_toast(toast);
            }
        };
        if folder {
            launcher.open_containing_folder(Some(&window), gio::Cancellable::NONE, done);
        } else {
            launcher.launch(Some(&window), gio::Cancellable::NONE, done);
        }
    }

    fn remove(self: &Rc<Self>, gid: &str) {
        let (Some(item), Some(window)) = (self.find(gid), self.window.upgrade()) else {
            return;
        };
        if !item.status.is_live() {
            self.send(Action::Remove(gid.into()));
            return;
        }
        let dialog = adw::AlertDialog::new(
            Some("Прибрати завантаження?"),
            Some("Потік зупинить завдання. Частково завантажений файл залишиться в папці."),
        );
        dialog.add_response("cancel", "Назад");
        dialog.add_response("remove", "Прибрати");
        dialog.set_close_response("cancel");
        dialog.set_default_response(Some("cancel"));
        dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
        dialog.choose(
            Some(&window),
            gio::Cancellable::NONE,
            glib::clone!(
                #[weak(rename_to = view)]
                self,
                move |response| {
                    if response == "remove" {
                        view.send(Action::Remove(item.gid));
                    }
                }
            ),
        );
    }
}

struct DownloadRow {
    root: gtk::ListBoxRow,
    name: gtk::Label,
    details: gtk::Label,
    progress: gtk::ProgressBar,
    badge: gtk::Label,
    action: gtk::Button,
    folder: gtk::Button,
    remove: gtk::Button,
}

impl DownloadRow {
    fn new(view: &Rc<View>, gid: &str) -> Self {
        let root = gtk::ListBoxRow::builder()
            .activatable(false)
            .selectable(false)
            .build();
        let line = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        line.add_css_class("download-row");
        root.set_child(Some(&line));

        let icon = image("text-x-generic-symbolic", 20);
        icon.add_css_class("file-icon");
        icon.set_valign(gtk::Align::Center);
        line.append(&icon);

        let text = gtk::Box::new(gtk::Orientation::Vertical, 6);
        text.set_hexpand(true);
        let name = label("", "heading");
        name.set_ellipsize(pango::EllipsizeMode::Middle);
        let details = label("", "dim-label");
        details.add_css_class("caption");
        details.set_ellipsize(pango::EllipsizeMode::End);
        let progress = gtk::ProgressBar::new();
        progress.add_css_class("transfer-progress");
        text.append(&name);
        text.append(&details);
        text.append(&progress);
        line.append(&text);

        let meta = gtk::Box::new(gtk::Orientation::Vertical, 4);
        meta.set_valign(gtk::Align::Center);
        meta.set_halign(gtk::Align::End);
        let badge = label("", "status-badge");
        badge.set_halign(gtk::Align::End);
        meta.append(&badge);
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 1);
        buttons.set_halign(gtk::Align::End);
        let action = button("media-playback-pause-symbolic", "Пауза");
        let folder = button("folder-open-symbolic", "Показати в папці");
        let remove = button("edit-delete-symbolic", "Прибрати зі списку");
        buttons.append(&action);
        buttons.append(&folder);
        buttons.append(&remove);
        meta.append(&buttons);
        line.append(&meta);

        let gid = gid.to_owned();
        action.connect_clicked(glib::clone!(
            #[weak]
            view,
            #[strong]
            gid,
            move |_| view.action_for(&gid)
        ));
        folder.connect_clicked(glib::clone!(
            #[weak]
            view,
            #[strong]
            gid,
            move |_| {
                if let Some(item) = view.find(&gid) {
                    view.open(&item, true);
                }
            }
        ));
        remove.connect_clicked(glib::clone!(
            #[weak]
            view,
            #[strong]
            gid,
            move |_| view.remove(&gid)
        ));

        Self {
            root,
            name,
            details,
            progress,
            badge,
            action,
            folder,
            remove,
        }
    }

    fn update(&self, item: &Download, connected: bool, waiting_for_connection: bool) {
        let title = item.file_name();
        self.name.set_label(title);
        self.name.set_tooltip_text(Some(title));
        let host = item.host();
        let transfer = match (item.status, item.total) {
            (Status::Complete, total) => bytes(total),
            (_, 0) if item.completed > 0 => bytes(item.completed),
            (_, 0) => String::new(),
            (_, total) => format!("{} / {}", bytes(item.completed), bytes(total)),
        };
        let mut details = vec![host.as_str(), transfer.as_str()];
        if let Some(report) = &item.verification {
            details.push(&report.message);
        } else if let Some(error) = &item.error {
            details.push(error);
        }
        let speed = speed_text(item.speed);
        if item.status == Status::Active && !waiting_for_connection {
            details.push(&speed);
        }
        let detail = details
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" · ");
        self.details.set_label(&detail);
        self.details.set_tooltip_text(Some(&detail));

        let (status, class, icon, tooltip) = match item.status {
            Status::Active | Status::Waiting if waiting_for_connection => (
                "Очікування з’єднання",
                "pending",
                "media-playback-pause-symbolic",
                "Пауза",
            ),
            Status::Active => (
                "Завантаження",
                "in-progress",
                "media-playback-pause-symbolic",
                "Пауза",
            ),
            Status::Waiting => (
                "У черзі",
                "pending",
                "media-playback-pause-symbolic",
                "Пауза",
            ),
            Status::Paused => (
                "На паузі",
                "pending",
                "media-playback-start-symbolic",
                "Продовжити",
            ),
            Status::Verifying => (
                "Перевіряємо файл",
                "pending",
                "emblem-system-symbolic",
                "Перевіряємо файл",
            ),
            Status::Complete => (
                "Завершено",
                "complete",
                "document-open-symbolic",
                "Відкрити файл",
            ),
            Status::Error if item.verification.is_some() => (
                "Перевірка не пройдена",
                "failed",
                "view-refresh-symbolic",
                "Повторити перевірку",
            ),
            Status::Error => (
                "Помилка",
                "failed",
                "view-refresh-symbolic",
                "Повторити завантаження",
            ),
            Status::Removed => return,
        };
        self.badge.set_label(status);
        self.badge.set_css_classes(&["status-badge", class]);
        self.progress.set_visible(item.status.is_live());
        if item.total > 0 {
            self.progress
                .set_fraction((item.completed as f64 / item.total as f64).clamp(0.0, 1.0));
        } else if item.status == Status::Active && !waiting_for_connection {
            self.progress.pulse();
        } else {
            self.progress.set_fraction(0.0);
        }
        self.action.set_icon_name(icon);
        self.action.set_tooltip_text(Some(tooltip));
        self.action
            .update_property(&[gtk::accessible::Property::Label(tooltip)]);
        self.action
            .set_sensitive(connected || item.status == Status::Complete);
        self.action.set_visible(item.status != Status::Verifying);
        self.folder.set_visible(
            (item.status == Status::Complete || item.verification.is_some())
                && !item.path.is_empty(),
        );
        self.remove.set_sensitive(connected);
    }
}

fn add_button(text: &str) -> gtk::Button {
    let content = adw::ButtonContent::builder()
        .icon_name("list-add-symbolic")
        .label(text)
        .build();
    let button = gtk::Button::builder()
        .child(&content)
        .action_name("win.add-download")
        .build();
    button.add_css_class("suggested-action");
    button.add_css_class("potik-primary");
    button.update_property(&[gtk::accessible::Property::Label(text)]);
    button
}

fn image(name: &str, size: i32) -> gtk::Image {
    gtk::Image::builder()
        .icon_name(name)
        .pixel_size(size)
        .build()
}

fn button(icon: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::from_icon_name(icon);
    button.add_css_class("flat");
    button.set_tooltip_text(Some(tooltip));
    button.update_property(&[gtk::accessible::Property::Label(tooltip)]);
    button
}

fn label(text: &str, class: &str) -> gtk::Label {
    let label = gtk::Label::builder().label(text).xalign(0.0).build();
    if !class.is_empty() {
        label.add_css_class(class);
    }
    label
}

fn paint_margins(widget: &impl IsA<gtk::Widget>, horizontal: i32, vertical: i32) {
    widget.set_margin_start(horizontal);
    widget.set_margin_end(horizontal);
    widget.set_margin_top(vertical);
    widget.set_margin_bottom(vertical);
}

fn bytes(bytes: u64) -> String {
    glib::format_size_full(bytes, glib::FormatSizeFlags::IEC_UNITS).to_string()
}

fn speed_text(speed: u64) -> String {
    format!("{}/с", bytes(speed))
}
