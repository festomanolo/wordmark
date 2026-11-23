//! The Remove Watermarks pane's folder cleaning: every PDF in a folder (and its subfolders) is
//! cleaned with what the detector has learned, into a folder of copies beside it, on a thread of
//! its own while the pane shows how far it has got. What the detector is sure of is removed;
//! files with things it is not sure of are written with those kept and listed for a person to
//! check in the pane. When it finishes, a notification with a sound says so. The work is
//! [`wordcraft_engine::cmd::clean::clean_folder_files`], which `clean.folder` runs too.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use craft::theme::variant::Tone;
use craft::ui::buttons::Button;
use craft::ui::choice::Checkbox;
use craft::ui::contract::{Bind, Props as _, Show as _, ShowContainer as _, Sizing};
use craft::ui::display::{Progress, Text};
use craft::ui::layout_components::Row;
use craft::ui::notices::Notice;
use craft::ui::sections::Section;
use egui::Ui;
use wordcraft_engine::cmd::clean::FolderFile;

/// Files listed for a check, at most (the rest are counted).
const LISTED: usize = 200;

/// How far a run has got; the worker thread writes it, the pane reads it.
#[derive(Debug, Default)]
pub(crate) struct RunState {
    /// PDFs found, once the folder is read.
    pub total: Option<usize>,
    /// Files done, in the order they finished.
    pub files: Vec<FolderFile>,
    pub finished: bool,
    /// Why the run could not start (the folder could not be read…).
    pub error: String,
    pub stopped: bool,
}

/// The folder section's state.
#[derive(Clone, Debug)]
pub(crate) struct Batch {
    pub dir: String,
    /// Where the copies go; empty: beside the folder, named `<folder>-clean`.
    pub out: String,
    pub subfolders: bool,
    /// Post a notification with a sound when a run ends.
    pub notify: bool,
    pub run: Option<Arc<Mutex<RunState>>>,
    pub cancel: Arc<AtomicBool>,
    /// Files a person checked and saved again since the run.
    pub checked: Vec<String>,
}

impl Default for Batch {
    fn default() -> Self {
        Batch {
            dir: String::new(),
            out: String::new(),
            subfolders: true,
            notify: true,
            run: None,
            cancel: Arc::new(AtomicBool::new(false)),
            checked: Vec::new(),
        }
    }
}

impl Batch {
    /// The folder the copies go to.
    pub fn out_dir(&self) -> String {
        if self.out.is_empty() { wordcraft_engine::cmd::clean::default_folder_out(&self.dir) } else { self.out.clone() }
    }

    pub fn running(&self) -> bool {
        self.run.as_ref().is_some_and(|r| r.lock().is_ok_and(|r| !r.finished))
    }
}

/// What the person asked for in the section, for the pane to do.
pub(crate) enum Action {
    None,
    ChooseFolder,
    ChooseOut,
    Start,
    /// Open this file in the pane to check it; saving writes `output`.
    Check {
        input: String,
        output: String,
    },
    /// Stop checking and show the folder's list again.
    Back,
}

/// The words a run's notification uses, translated before the run (the worker thread has no
/// language of its own).
struct Words {
    title: String,
    clean: String,
    check: String,
    failed: String,
    stopped: String,
    error: String,
}

/// Start cleaning the folder with the model in `model` (or the starting model).
pub(crate) fn start(ctx: &egui::Context, b: &mut Batch, model: Option<String>) {
    let state = Arc::new(Mutex::new(RunState::default()));
    b.cancel = Arc::new(AtomicBool::new(false));
    b.checked.clear();
    b.run = Some(Arc::clone(&state));
    let (dir, out, subfolders, notify, cancel, ctx) = (b.dir.clone(), b.out_dir(), b.subfolders, b.notify, Arc::clone(&b.cancel), ctx.clone());
    let words = Words {
        title: tl!("PDFs cleaned").to_string(),
        clean: tl!("{n} PDFs cleaned. Nothing needs your check.").to_string(),
        check: tl!("{n} PDFs cleaned. {check} need your check: open Remove Watermarks to look at them.").to_string(),
        failed: tl!("{failed} couldn't be cleaned.").to_string(),
        stopped: tl!("Stopped after {n} PDFs.").to_string(),
        error: tl!("Couldn't clean the folder").to_string(),
    };
    std::thread::spawn(move || {
        let set = |f: &mut dyn FnMut(&mut RunState)| {
            if let Ok(mut s) = state.lock() {
                f(&mut s);
            }
            ctx.request_repaint();
        };
        let result = wordcraft_engine::cmd::clean::folder_jobs(&dir, &out, subfolders).and_then(|jobs| {
            set(&mut |s| s.total = Some(jobs.len()));
            wordcraft_engine::cmd::clean::clean_folder_files(&jobs, model.as_deref(), None, &cancel, &|f| set(&mut |s| s.files.push(f.clone())))
        });
        let stopped = cancel.load(Ordering::Relaxed);
        let mut body = String::new();
        set(&mut |s| {
            s.finished = true;
            s.stopped = stopped;
            match &result {
                Err(e) => {
                    s.error = e.clone();
                    body = format!("{}: {e}", words.error);
                }
                Ok(_) => {
                    let n = s.files.iter().filter(|f| f.error.is_empty()).count().to_string();
                    let check = s.files.iter().filter(|f| f.error.is_empty() && f.unsure > 0).count();
                    let failed = s.files.iter().filter(|f| !f.error.is_empty()).count();
                    body = if stopped {
                        crate::i18n::fmt(&words.stopped, &[("n", &s.files.len().to_string())])
                    } else if check > 0 {
                        crate::i18n::fmt(&words.check, &[("n", &n), ("check", &check.to_string())])
                    } else {
                        crate::i18n::fmt(&words.clean, &[("n", &n)])
                    };
                    if failed > 0 {
                        body.push(' ');
                        body.push_str(&crate::i18n::fmt(&words.failed, &[("failed", &failed.to_string())]));
                    }
                }
            }
        });
        if notify {
            crate::notify::notify(&ctx, &words.title, &body);
        }
    });
}

/// The section: the folder, where the copies go, a Start / Stop button, the progress, and the
/// files to check.
pub(crate) fn section(ui: &mut Ui, b: &mut Batch) -> Action {
    let mut action = Action::None;
    Section::fixed(tl!("Or clean a whole folder")).show(ui, |ui, _| {
        if cfg!(target_arch = "wasm32") {
            Text::new(tl!("Cleaning a folder needs the desktop app.")).show(ui);
            return;
        }
        Text::new(tl!("Every PDF in the folder is cleaned with what has been learned here. Things it isn't sure of are kept, and the file is listed for you to check.")).show(ui);
        let running = b.running();
        Row::new().show(ui, |ui, _| {
            if Button::new(tl!("Choose Folder…")).enabled(!running).show(ui).clicked() {
                action = Action::ChooseFolder;
            }
            if !b.dir.is_empty() && Button::new(tl!("Change Output Folder…")).enabled(!running).show(ui).clicked() {
                action = Action::ChooseOut;
            }
        });
        if b.dir.is_empty() {
            return;
        }
        // The folder's own name and its parent's: a whole path is cut off in a narrow pane.
        Text::new(crate::i18n::fmt(tl!("From: {dir}"), &[("dir", &short(&b.dir))])).show(ui);
        Text::new(crate::i18n::fmt(tl!("To: {dir}"), &[("dir", &short(&b.out_dir()))])).show(ui);
        Checkbox::new(Bind::bound(&mut b.subfolders)).label(tl!("Include subfolders")).enabled(!running).show(ui);
        Checkbox::new(Bind::bound(&mut b.notify)).label(tl!("Notify me with a sound when it's done")).show(ui);
        if running {
            if Button::new(tl!("Stop")).show(ui).clicked() {
                b.cancel.store(true, Ordering::Relaxed);
            }
        } else if Button::new(if b.run.is_some() { tl!("Clean Again") } else { tl!("Start Cleaning") }).show(ui).clicked() {
            action = Action::Start;
        }
        if let Some(a) = progress(ui, b) {
            action = a;
        }
    });
    action
}

/// The files the run left to check, `(input, output)`, by name.
fn to_check(b: &Batch) -> Vec<(String, String)> {
    let Some(s) = b.run.as_ref().and_then(|r| r.lock().ok()) else { return Vec::new() };
/// How far the run has got, and what it left for a person.
fn progress(ui: &mut Ui, b: &Batch) -> Option<Action> {
    let run = b.run.as_ref()?;
    let s = run.lock().ok()?;
    if !s.error.is_empty() {
        Notice::new(Tone::Bad, s.error.clone()).show(ui);
        return None;
    }
    let done = s.files.len();
    match s.total {
        None => {
            Progress::indeterminate().label(tl!("Finding PDFs")).sizing(Sizing::FillWidth).show(ui);
            Text::new(tl!("Finding PDFs…")).show(ui);
        }
        Some(0) => {
            Notice::new(Tone::Warn, tl!("There are no PDFs in this folder.")).show(ui);
            return None;
        }
        Some(total) => {
            Progress::new(done as f32 / total as f32).label(tl!("Cleaning PDFs")).sizing(Sizing::FillWidth).show(ui);
            let line = if s.finished && s.stopped {
                crate::i18n::fmt(tl!("Stopped: {done} of {total} PDFs done."), &[("done", &done.to_string()), ("total", &total.to_string())])
            } else if s.finished {
                crate::i18n::fmt(tl!("Done: {total} PDFs."), &[("total", &total.to_string())])
            } else {
                crate::i18n::fmt(tl!("{done} of {total} PDFs…"), &[("done", &done.to_string()), ("total", &total.to_string())])
            };
            Text::new(line).show(ui);
        }
    }
    let ok = s.files.iter().filter(|f| f.error.is_empty()).count();
    let check: Vec<&FolderFile> = s.files.iter().filter(|f| f.error.is_empty() && f.unsure > 0).collect();
    let failed: Vec<&FolderFile> = s.files.iter().filter(|f| !f.error.is_empty()).collect();
    let removed: usize = s.files.iter().map(|f| f.removed).sum();
    Text::new(crate::i18n::fmt(
        tl!("Cleaned: {ok}. Things removed: {removed}. To check: {check}. Failed: {failed}."),
        &[("ok", &ok.to_string()), ("removed", &removed.to_string()), ("check", &check.len().to_string()), ("failed", &failed.len().to_string())],
    ))
    .show(ui);
    let mut action = None;
    if !check.is_empty() {
        Text::new(tl!("Check these: things it isn't sure of were kept. Check opens the file here; saving writes its clean copy again.")).show(ui);
        let mut check = check;
        check.sort_by(|a, b| a.input.cmp(&b.input));
        for (at, f) in check.iter().take(LISTED).enumerate() {
            let done = b.checked.contains(&f.input);
            Row::new().show(ui, |ui, _| {
                if Button::new(tl!("Check")).id_salt(("clean-check", at)).show(ui).clicked() {
                    action = Some(Action::Check { input: f.input.clone(), output: f.output.clone() });
                }
                let status = if done { tl!("checked") } else { tl!("not sure of {n}") };
                let status = crate::i18n::fmt(status, &[("n", &f.unsure.to_string())]);
                // What to do first: a narrow pane cuts off the end of the name, not the count.
                Text::new(format!("{status} · {}", relative(&b.dir, &f.input))).show(ui);
            });
        }
        if check.len() > LISTED {
            Text::new(crate::i18n::fmt(tl!("and {n} more"), &[("n", &(check.len() - LISTED).to_string())])).show(ui);
        }
    }
    for f in failed.iter().take(LISTED) {
        Notice::new(Tone::Bad, format!("{}: {}", relative(&b.dir, &f.input), f.error)).show(ui);
    }
    action
}

/// A folder's name with its parent's ("past-papers/term 2"), for a narrow pane.
fn short(dir: &str) -> String {
    let p = std::path::Path::new(dir);
    let name = |p: &std::path::Path| p.file_name().map(|n| n.to_string_lossy().into_owned());
    match (p.parent().and_then(name), name(p)) {
        (Some(parent), Some(n)) => format!("{parent}/{n}"),
        (None, Some(n)) => n,
        _ => dir.to_string(),
    }
}

/// `path` relative to `dir`, for a short row.
fn relative(dir: &str, path: &str) -> String {
    std::path::Path::new(path).strip_prefix(dir).map_or_else(|_| path.to_string(), |p| p.to_string_lossy().into_owned())
}
