mod aria;
mod checkpoint;
mod engine;
mod probe;
mod process;
mod settings;
mod ui;
mod verification;

fn main() -> gtk::glib::ExitCode {
    ui::run()
}
