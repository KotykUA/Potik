use super::{View, button, image, label, paint_margins};
use crate::{
    engine::{self, Action, Download, Spec},
    probe,
    settings::Settings,
    verification,
};
use adw::prelude::*;
use gtk::{gio, glib, pango};
use std::{
    cell::{Cell, RefCell},
    path::{Path, PathBuf},
    rc::Rc,
    time::Duration,
};

pub(super) fn show_settings_dialog(window: &adw::ApplicationWindow, view: &Rc<View>) {
    let defaults = view.settings.borrow().clone();
    let directory = Rc::new(RefCell::new(defaults.directory));
    let (dialog, form, save) = form_dialog("Налаштування", "Зберегти", false);

    let folder = folder_picker(window, directory.clone());
    form.append(&field("Типова папка завантажень", &folder));

    let (connections_row, connections) =
        connection_field("Типова кількість потоків", defaults.connections);
    form.append(&connections_row);
    let (speed_row, speed) = speed_field("Типовий ліміт швидкості, КіБ/с", defaults.max_speed_kib);
    form.append(&speed_row);

    let problem = label("", "error");
    problem.set_wrap(true);
    problem.set_visible(false);
    form.append(&problem);

    save.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        #[weak]
        connections,
        #[weak]
        speed,
        #[weak]
        problem,
        #[weak]
        view,
        #[strong]
        directory,
        move |_| {
            connections.update();
            speed.update();
            let settings = Settings {
                directory: directory.borrow().clone(),
                connections: connections.value_as_int() as u8,
                max_speed_kib: speed.value() as u32,
            };
            match settings.save(&view.settings_dir) {
                Ok(()) => {
                    *view.settings.borrow_mut() = settings;
                    dialog.close();
                }
                Err(error) => {
                    problem.set_label(&format!("Не вдалося зберегти налаштування. {error}"));
                    problem.set_visible(true);
                }
            }
        }
    ));

    dialog.present(Some(window));
}

pub(super) fn show_add_dialog(window: &adw::ApplicationWindow, view: &Rc<View>) {
    let defaults = view.settings.borrow().clone();
    let (dialog, form, submit) = form_dialog("Нове завантаження", "Додати", true);
    submit.set_sensitive(false);

    let url = gtk::Entry::builder()
        .placeholder_text("https://…")
        .input_purpose(gtk::InputPurpose::Url)
        .secondary_icon_name("edit-paste-symbolic")
        .build();
    url.set_icon_tooltip_text(
        gtk::EntryIconPosition::Secondary,
        Some("Вставити з буфера обміну"),
    );
    let url_field = field("Посилання", &url);
    let url_error = label("Дайте дійсну URL", "url-error");
    url_error.add_css_class("caption");
    url_error.set_visible(false);
    url_field.append(&url_error);
    let link_warning = label(
        "Це ймовірно не пряме URL, качайте на свій страх і ризик",
        "link-warning",
    );
    link_warning.add_css_class("caption");
    link_warning.set_wrap(true);
    link_warning.set_visible(false);
    url_field.append(&link_warning);
    let range_status = label("", "dim-label");
    range_status.add_css_class("caption");
    range_status.set_wrap(true);
    range_status.set_visible(false);
    url_field.append(&range_status);
    form.append(&url_field);
    url.connect_icon_press(|entry, position| {
        if position == gtk::EntryIconPosition::Secondary {
            entry.clipboard().read_text_async(
                gio::Cancellable::NONE,
                glib::clone!(
                    #[weak]
                    entry,
                    move |result| {
                        if let Ok(Some(text)) = result {
                            entry.set_text(text.trim());
                        }
                    }
                ),
            );
        }
    });
    let name = gtk::Entry::builder()
        .placeholder_text("Визначити автоматично")
        .build();
    form.append(&field("Назва файлу", &name));

    let directory = Rc::new(RefCell::new(defaults.directory));
    let folder = folder_picker(window, directory.clone());
    form.append(&field("Зберегти в", &folder));

    let resume_file = gtk::ToggleButton::with_label("Докачати наявний файл");
    resume_file.set_halign(gtk::Align::Start);
    resume_file.set_sensitive(false);
    let resume_hint = label(
        "Вкажіть папку й точну назву часткового файлу з цього посилання.",
        "dim-label",
    );
    resume_hint.add_css_class("caption");
    resume_hint.set_wrap(true);
    resume_hint.set_visible(false);
    let resume_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    resume_box.append(&resume_file);
    resume_box.append(&resume_hint);
    form.append(&resume_box);
    resume_file.connect_toggled(glib::clone!(
        #[weak]
        resume_hint,
        #[weak]
        name,
        move |button| {
            let active = button.is_active();
            resume_hint.set_visible(active);
            name.set_placeholder_text(Some(if active {
                "Точна назва часткового файлу"
            } else {
                "Визначити автоматично"
            }));
        }
    ));

    let (connections_row, connections) =
        connection_field("Кількість потоків", defaults.connections);
    form.append(&connections_row);
    let (speed_row, speed) = speed_field("Ліміт швидкості, КіБ/с", defaults.max_speed_kib);
    form.append(&speed_row);
    let start_now = adw::SwitchRow::builder()
        .title("Почати одразу")
        .active(true)
        .build();
    let options = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .build();
    options.add_css_class("boxed-list");
    options.append(&start_now);
    form.append(&options);

    let network_fields = gtk::Box::new(gtk::Orientation::Vertical, 10);
    network_fields.set_margin_top(8);
    let mirrors = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::Char)
        .accepts_tab(false)
        .build();
    let mirror_field = field("Дзеркала того самого файлу", &mirrors);
    let mirror_help = label("Одне HTTP або HTTPS-посилання на рядок.", "dim-label");
    mirror_help.add_css_class("caption");
    mirror_field.append(&mirror_help);
    network_fields.append(&mirror_field);
    let http = Rc::new(HttpFields::new(&network_fields));
    let network = gtk::Expander::builder()
        .label("Параметри з’єднання (необов’язково)")
        .child(&network_fields)
        .build();
    form.append(&network);
    let range_support = check_download_link(
        &dialog,
        &url,
        &range_status,
        &link_warning,
        &resume_file,
        &http,
    );

    let verification_fields = gtk::Box::new(gtk::Orientation::Vertical, 10);
    verification_fields.set_margin_top(8);
    let sha256 = gtk::Entry::builder()
        .placeholder_text("64 шістнадцяткові символи")
        .build();
    let sha_field = field("SHA-256", &sha256);
    let sha_error = label("", "url-error");
    sha_error.add_css_class("caption");
    sha_error.set_wrap(true);
    sha_error.set_visible(false);
    sha_field.append(&sha_error);
    verification_fields.append(&sha_field);
    let (signature_picker, signature) = signature_picker(window);
    verification_fields.append(&field("Відокремлений підпис GPG/PGP", &signature_picker));
    let verification = gtk::Expander::builder()
        .label("Перевірка файлу (необов’язково)")
        .child(&verification_fields)
        .build();
    form.append(&verification);

    let problem = label("", "error");
    problem.set_wrap(true);
    problem.set_visible(false);
    form.append(&problem);

    let validate = glib::clone!(
        #[weak]
        url,
        #[weak]
        url_error,
        #[weak]
        submit,
        #[weak]
        sha256,
        #[weak]
        sha_error,
        #[weak]
        problem,
        move |url_changed| {
            let url_valid = engine::validate_url(url.text().trim()).is_ok();
            let checksum = verification::sha256_value(&sha256.text());
            if url_changed {
                url_error.set_visible(!url_valid);
            }
            sha_error.set_visible(checksum.is_err());
            if let Err(message) = &checksum {
                sha_error.set_label(message);
            }
            submit.set_sensitive(url_valid && checksum.is_ok());
            problem.set_visible(false);
        }
    );
    url.connect_changed({
        let validate = validate.clone();
        move |_| validate(true)
    });
    sha256.connect_changed(move |_| validate(false));
    submit.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        #[weak]
        url,
        #[weak]
        name,
        #[weak]
        sha256,
        #[weak]
        resume_file,
        #[weak]
        connections,
        #[weak]
        speed,
        #[weak]
        mirrors,
        #[weak]
        start_now,
        #[weak]
        problem,
        #[weak]
        view,
        #[strong]
        directory,
        #[strong]
        range_support,
        #[strong]
        signature,
        #[strong]
        http,
        move |_| {
            if !view.add.is_enabled() {
                problem.set_label("aria2c наразі недоступний.");
                problem.set_visible(true);
                return;
            }
            connections.update();
            speed.update();
            let mirrors = mirrors.buffer();
            let mirror_urls = mirrors.text(&mirrors.start_iter(), &mirrors.end_iter(), false);
            let spec = Spec::new(
                &url.text(),
                &directory.borrow(),
                &name.text(),
                connections.value_as_int() as u8,
            )
            .and_then(|spec| {
                spec.with_options(
                    &mirror_urls,
                    speed.value() as u32,
                    http.read()?,
                    range_support.get(),
                )
            })
            .and_then(|spec| spec.with_sha256(&sha256.text()))
            .and_then(|spec| {
                if resume_file.is_active() {
                    let probe::RangeSupport::Available(length) = range_support.get() else {
                        return Err(
                            "Підтримку докачування для цього посилання ще не підтверджено.".into(),
                        );
                    };
                    spec.resume_existing(length)
                } else {
                    Ok(spec)
                }
            });
            match spec {
                Ok(spec) => {
                    view.send(Action::Add {
                        spec: Box::new(spec),
                        paused: !start_now.is_active(),
                        signature: signature.borrow().clone(),
                    });
                    dialog.close();
                }
                Err(error) => {
                    problem.set_label(&error.to_string());
                    problem.set_visible(true);
                }
            }
        }
    ));
    for entry in [&url, &name] {
        entry.connect_activate(glib::clone!(
            #[weak]
            submit,
            move |_| if submit.is_sensitive() {
                submit.emit_clicked();
            }
        ));
    }

    dialog.set_focus(Some(&url));
    dialog.present(Some(window));
}

fn form_dialog(
    title: &str,
    submit_label: &str,
    scroll: bool,
) -> (adw::Dialog, gtk::Box, gtk::Button) {
    let dialog = adw::Dialog::builder()
        .title(title)
        .content_width(490)
        .build();
    let header = adw::HeaderBar::builder()
        .show_end_title_buttons(false)
        .build();
    let cancel = gtk::Button::with_label("Скасувати");
    let submit = gtk::Button::with_label(submit_label);
    submit.add_css_class("suggested-action");
    submit.add_css_class("potik-primary");
    header.pack_start(&cancel);
    header.pack_end(&submit);
    cancel.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| {
            dialog.close();
        }
    ));

    let layout = adw::ToolbarView::new();
    layout.add_top_bar(&header);
    let form = gtk::Box::new(gtk::Orientation::Vertical, 18);
    paint_margins(&form, 22, 22);
    if scroll {
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .child(&form)
            .build();
        layout.set_content(Some(&scroll));
    } else {
        layout.set_content(Some(&form));
    }
    dialog.set_child(Some(&layout));
    (dialog, form, submit)
}

struct HttpFields {
    user_agent: gtk::Entry,
    referer: gtk::Entry,
    username: gtk::Entry,
    password: gtk::PasswordEntry,
}

impl HttpFields {
    fn new(form: &gtk::Box) -> Self {
        let user_agent = gtk::Entry::new();
        let referer = gtk::Entry::builder()
            .placeholder_text("https://…")
            .input_purpose(gtk::InputPurpose::Url)
            .build();
        form.append(&field("User-Agent", &user_agent));
        form.append(&field("Сторінка завантаження (Referer)", &referer));
        let (username, password) = login_fields(form);
        Self {
            user_agent,
            referer,
            username,
            password,
        }
    }

    fn read(&self) -> Result<probe::HttpOptions, &'static str> {
        probe::HttpOptions::new(
            &self.user_agent.text(),
            &self.referer.text(),
            &self.username.text(),
            &self.password.text(),
        )
    }
}

fn login_fields(form: &gtk::Box) -> (gtk::Entry, gtk::PasswordEntry) {
    let username = gtk::Entry::new();
    let password = gtk::PasswordEntry::builder().show_peek_icon(true).build();
    form.append(&field("Ім’я користувача", &username));
    form.append(&field("Пароль", &password));
    (username, password)
}

pub(super) fn show_login_dialog(window: &adw::ApplicationWindow, view: &Rc<View>, item: &Download) {
    let dialog = adw::AlertDialog::builder()
        .heading("HTTP-авторизація")
        .body(format!("Введіть дані для входу на {}.", item.host()))
        .body_use_markup(false)
        .build();
    let form = gtk::Box::new(gtk::Orientation::Vertical, 10);
    let (username, password) = login_fields(&form);
    let problem = label("", "url-error");
    problem.set_wrap(true);
    problem.set_visible(false);
    form.append(&problem);
    dialog.set_extra_child(Some(&form));
    dialog.add_response("cancel", "Скасувати");
    dialog.add_response("continue", "Продовжити");
    dialog.set_close_response("cancel");
    dialog.set_default_response(Some("continue"));
    dialog.set_response_appearance("continue", adw::ResponseAppearance::Suggested);
    dialog.set_response_enabled("continue", false);
    let validate = glib::clone!(
        #[weak]
        dialog,
        #[weak]
        username,
        #[weak]
        password,
        #[weak]
        problem,
        move || {
            let login = probe::Login::optional(&username.text(), &password.text());
            dialog.set_response_enabled("continue", matches!(login, Ok(Some(_))));
            problem.set_visible(login.is_err());
            if let Err(error) = login {
                problem.set_label(error);
            }
        }
    );
    username.connect_changed({
        let validate = validate.clone();
        move |_| validate()
    });
    password.connect_changed(move |_| validate());
    dialog.connect_response(
        Some("continue"),
        glib::clone!(
            #[weak]
            view,
            #[strong(rename_to = gid)]
            item.gid,
            #[strong]
            username,
            #[strong]
            password,
            move |_, _| {
                if let Ok(Some(login)) = probe::Login::optional(&username.text(), &password.text())
                {
                    view.send(Action::Retry {
                        gid: gid.clone(),
                        login: Some(login),
                    });
                }
            }
        ),
    );
    dialog.set_focus(Some(&username));
    dialog.present(Some(window));
}

fn signature_picker(window: &adw::ApplicationWindow) -> (gtk::Box, Rc<RefCell<Option<PathBuf>>>) {
    let signature = Rc::new(RefCell::new(None));
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let choose = gtk::Button::new();
    choose.set_hexpand(true);
    choose.update_property(&[gtk::accessible::Property::Label("Вибрати файл підпису")]);
    let filename = label("Вибрати файл…", "");
    filename.set_ellipsize(pango::EllipsizeMode::Middle);
    choose.set_child(Some(&filename));
    let clear = button("edit-clear-symbolic", "Прибрати файл підпису");
    clear.set_visible(false);
    row.append(&choose);
    row.append(&clear);
    choose.connect_clicked(glib::clone!(
        #[weak]
        window,
        #[weak]
        filename,
        #[weak]
        clear,
        #[strong]
        signature,
        move |_| {
            let chooser = gtk::FileDialog::builder()
                .title("Вибрати підпис GPG/PGP")
                .build();
            chooser.open(
                Some(&window),
                gio::Cancellable::NONE,
                glib::clone!(
                    #[weak]
                    filename,
                    #[weak]
                    clear,
                    #[strong]
                    signature,
                    move |result| {
                        if let Ok(file) = result
                            && let Some(path) = file.path()
                            && let Some(name) = path.file_name()
                        {
                            filename.set_label(&name.to_string_lossy());
                            filename.set_tooltip_text(Some(&path.to_string_lossy()));
                            clear.set_visible(true);
                            *signature.borrow_mut() = Some(path);
                        }
                    }
                ),
            );
        }
    ));
    clear.connect_clicked(glib::clone!(
        #[weak]
        filename,
        #[strong]
        signature,
        move |button| {
            *signature.borrow_mut() = None;
            filename.set_label("Вибрати файл…");
            filename.set_tooltip_text(None);
            button.set_visible(false);
        }
    ));
    (row, signature)
}

fn check_download_link(
    dialog: &adw::Dialog,
    url: &gtk::Entry,
    status: &gtk::Label,
    warning: &gtk::Label,
    resume_file: &gtk::ToggleButton,
    http: &Rc<HttpFields>,
) -> Rc<Cell<probe::RangeSupport>> {
    let range_support = Rc::new(Cell::new(probe::RangeSupport::Unknown));
    let task = Rc::new(RefCell::new(None::<glib::JoinHandle<()>>));
    let changed = glib::clone!(
        #[weak]
        url,
        #[weak]
        http,
        #[weak]
        status,
        #[weak]
        warning,
        #[weak]
        resume_file,
        #[strong]
        range_support,
        #[strong]
        task,
        move || {
            if let Some(pending) = task.borrow_mut().take() {
                pending.abort();
            }
            range_support.set(probe::RangeSupport::Unknown);
            warning.set_visible(false);
            resume_file.set_active(false);
            resume_file.set_sensitive(false);
            let address = url.text().trim().to_owned();
            let valid = engine::validate_url(&address).is_ok() && !engine::url_has_login(&address);
            status.set_visible(valid);
            if !valid {
                return;
            }
            let Ok(http) = http.read() else {
                status.set_label("Перевірте параметри з’єднання.");
                return;
            };
            status.set_label("Перевіряємо підтримку докачування…");

            let status = status.downgrade();
            let warning = warning.downgrade();
            let resume_file = resume_file.downgrade();
            let range_support = range_support.clone();
            *task.borrow_mut() = Some(glib::MainContext::default().spawn_local(async move {
                glib::timeout_future(Duration::from_millis(500)).await;
                let check = gio::spawn_blocking(move || probe::check(&address, &http))
                    .await
                    .unwrap_or_default();
                let (Some(status), Some(warning), Some(resume_file)) =
                    (status.upgrade(), warning.upgrade(), resume_file.upgrade())
                else {
                    return;
                };
                warning.set_visible(check.likely_indirect);
                range_support.set(check.range_support);
                match check.range_support {
                    probe::RangeSupport::Available(_) => {
                        resume_file.set_sensitive(true);
                        status.set_label("Сервер підтримує докачування");
                    }
                    probe::RangeSupport::Unavailable => {
                        status.set_label("Сервер не підтримує докачування цього файлу");
                    }
                    probe::RangeSupport::Unknown => {
                        status.set_label("Не вдалося визначити підтримку докачування");
                    }
                }
            }));
        }
    );
    for entry in [
        url.upcast_ref::<gtk::Editable>(),
        http.user_agent.upcast_ref(),
        http.referer.upcast_ref(),
        http.username.upcast_ref(),
        http.password.upcast_ref(),
    ] {
        let changed = changed.clone();
        entry.connect_changed(move |_| changed());
    }
    dialog.connect_closed(move |_| {
        if let Some(pending) = task.borrow_mut().take() {
            pending.abort();
        }
    });
    range_support
}

fn connection_field(title: &str, value: u8) -> (gtk::Box, gtk::SpinButton) {
    number_field(
        title,
        "До 16 на сервер. Для більшої кількості додайте дзеркала.",
        1.0,
        f64::from(*engine::CONNECTION_RANGE.end()),
        f64::from(value),
    )
}

fn speed_field(title: &str, value: u32) -> (gtk::Box, gtk::SpinButton) {
    number_field(
        title,
        "0 — без обмежень",
        0.0,
        f64::from(u32::MAX),
        f64::from(value),
    )
}

fn number_field(
    title: &str,
    hint: &str,
    min: f64,
    max: f64,
    value: f64,
) -> (gtk::Box, gtk::SpinButton) {
    let field = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.add_css_class("connection-row");
    let title = label(title, "");
    title.set_hexpand(true);
    title.set_wrap(true);
    let spin = gtk::SpinButton::with_range(min, max, 1.0);
    spin.set_numeric(true);
    spin.set_update_policy(gtk::SpinButtonUpdatePolicy::IfValid);
    spin.set_width_chars(8);
    spin.set_value(value);
    spin.update_relation(&[gtk::accessible::Relation::LabelledBy(&[title.upcast_ref()])]);
    row.append(&title);
    row.append(&spin);
    field.append(&row);
    let hint = label(hint, "dim-label");
    hint.set_wrap(true);
    hint.add_css_class("caption");
    field.append(&hint);
    (field, spin)
}

fn folder_picker(window: &adw::ApplicationWindow, directory: Rc<RefCell<PathBuf>>) -> gtk::Button {
    let folder = gtk::Button::new();
    let folder_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let folder_name = label("", "");
    set_folder_label(&folder_name, &directory.borrow());
    folder_name.set_hexpand(true);
    folder_name.set_ellipsize(pango::EllipsizeMode::Middle);
    folder_box.append(&image("folder-symbolic", 16));
    folder_box.append(&folder_name);
    folder_box.append(&image("pan-down-symbolic", 12));
    folder.set_child(Some(&folder_box));
    folder.connect_clicked(glib::clone!(
        #[weak]
        window,
        #[weak]
        folder_name,
        move |_| {
            let chooser = gtk::FileDialog::builder()
                .title("Папка завантаження")
                .initial_folder(&gio::File::for_path(directory.borrow().as_path()))
                .build();
            chooser.select_folder(
                Some(&window),
                gio::Cancellable::NONE,
                glib::clone!(
                    #[weak]
                    folder_name,
                    #[strong]
                    directory,
                    move |selection| {
                        if let Ok(file) = selection
                            && let Some(path) = file.path()
                        {
                            set_folder_label(&folder_name, &path);
                            *directory.borrow_mut() = path;
                        }
                    }
                ),
            );
        }
    ));
    folder
}

fn set_folder_label(label: &gtk::Label, path: &Path) {
    let full = path.to_string_lossy();
    match path.strip_prefix(glib::home_dir()) {
        Ok(relative) if !relative.as_os_str().is_empty() => {
            label.set_label(&format!("~/{}", relative.display()));
        }
        _ => label.set_label(&full),
    }
    label.set_tooltip_text(Some(&full));
}

fn field(title: &str, widget: &impl IsA<gtk::Widget>) -> gtk::Box {
    let container = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let title = label(title, "heading");
    widget
        .upcast_ref::<gtk::Widget>()
        .update_relation(&[gtk::accessible::Relation::LabelledBy(&[title.upcast_ref()])]);
    container.append(&title);
    container.append(widget);
    container
}
