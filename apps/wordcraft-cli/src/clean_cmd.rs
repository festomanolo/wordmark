//! `clean`, `clean-learn` and `clean-review`: removing watermarks and web addresses from a batch
//! of PDFs and documents, teaching the detector which marks are watermarks, and deciding the
//! ones it was unsure of.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use wordcraft_clean::detect::{Model, normalize, similarity};
use wordcraft_clean::{DocIo, FileReport, Job, Options, RebuildOptions, UrlAction, candidates, extract_pdf, pipeline};

use super::{Failure, OptionSpec, escape_text, password_from, positionals, system_region, validate_options};

pub(crate) const CLEAN_OPTIONS: &[OptionSpec] = &[
    OptionSpec { name: "--out", takes_value: true },
    OptionSpec { name: "--format", takes_value: true },
    OptionSpec { name: "--model", takes_value: true },
    OptionSpec { name: "--urls", takes_value: true },
    OptionSpec { name: "--jobs", takes_value: true },
    OptionSpec { name: "--report", takes_value: true },
    OptionSpec { name: "--drop-flagged", takes_value: false },
    OptionSpec { name: "--no-page-breaks", takes_value: false },
    OptionSpec { name: "--recursive", takes_value: false },
    OptionSpec { name: "--password", takes_value: true },
    OptionSpec { name: "--password-file", takes_value: true },
];
pub(crate) const LEARN_OPTIONS: &[OptionSpec] = &[
    OptionSpec { name: "--model", takes_value: true },
    OptionSpec { name: "--mark", takes_value: true },
    OptionSpec { name: "--not", takes_value: true },
    OptionSpec { name: "--limit", takes_value: true },
    OptionSpec { name: "--password", takes_value: true },
    OptionSpec { name: "--password-file", takes_value: true },
];
pub(crate) const REVIEW_OPTIONS: &[OptionSpec] = &[
    OptionSpec { name: "--model", takes_value: true },
    OptionSpec { name: "--no-rerun", takes_value: false },
    OptionSpec { name: "--password", takes_value: true },
    OptionSpec { name: "--password-file", takes_value: true },
];

/// Where the model is kept when `--model` isn't given.
const DEFAULT_MODEL: &str = "watermark-model.json";
/// Formats `clean` writes.
const OUT_FORMATS: &[&str] = &["docx", "odt", "rtf", "md", "html", "txt", "pdf", "json"];

/// The batch report `clean` writes and `clean-review` reads.
#[derive(Serialize, Deserialize)]
struct BatchReport {
    format: String,
    urls: UrlAction,
    drop_flagged: bool,
    page_breaks: bool,
    model: PathBuf,
    files: Vec<FileReport>,
}

/// Every value an option is given (`--mark A --mark B`).
fn values(args: &[String], name: &str) -> Vec<String> {
    args.windows(2).filter(|w| w[0] == name && !w[1].starts_with("--")).map(|w| w[1].clone()).collect()
}

fn value(args: &[String], name: &str) -> Result<Option<String>, Failure> {
    let v = values(args, name);
    match v.len() {
        0 if args.iter().any(|a| a == name) => Err(format!("{name} requires a value").into()),
        0 => Ok(None),
        1 => Ok(v.into_iter().next()),
        _ => Err(format!("duplicate {name} option").into()),
    }
}

fn model_path(args: &[String]) -> Result<PathBuf, Failure> {
    Ok(PathBuf::from(value(args, "--model")?.unwrap_or_else(|| DEFAULT_MODEL.into())))
}

fn save_model(model: &Model, path: &Path) -> Result<(), Failure> {
    std::fs::write(path, model.to_json()).map_err(|e| Failure::from(format!("{}: {e}", path.display())))
}

/// Opening and saving through the app's format layer.
struct EngineIo {
    password: Option<String>,
}

impl DocIo for EngineIo {
    fn open(&self, path: &Path, bytes: &[u8]) -> Result<wordcraft_doc::Document, String> {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        wordcraft_engine::io::open_bytes_in(&name, bytes, self.password.as_deref(), &system_region())
            .map(|(d, _)| d)
            .map_err(|e| escape_text(&e, false))
    }
    fn save(&self, path: &Path, doc: &wordcraft_doc::Document) -> Result<(), String> {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let bytes = wordcraft_engine::io::save_bytes(&name, doc)?;
        std::fs::write(path, bytes).map_err(|e| format!("{}: {e}", path.display()))
    }
}

/// Whether `clean` takes a file found in a folder: a PDF or a format the app opens.
fn cleanable(path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    name.ends_with(".pdf") || wordcraft_engine::io::OPEN_EXTS.iter().any(|e| name.ends_with(&format!(".{e}")))
}

fn collect_inputs(paths: &[&String], recursive: bool) -> Result<Vec<PathBuf>, Failure> {
    let mut out = Vec::new();
    for p in paths {
        let p = PathBuf::from(p.as_str());
        if p.is_dir() {
            walk(&p, recursive, &mut out)?;
        } else {
            out.push(p);
        }
    }
    Ok(out)
}

fn walk(dir: &Path, recursive: bool, out: &mut Vec<PathBuf>) -> Result<(), Failure> {
    let mut entries: Vec<PathBuf> =
        std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?.filter_map(|e| e.ok().map(|e| e.path())).collect();
    entries.sort();
    for e in entries {
        if e.is_dir() {
            if recursive {
                walk(&e, recursive, out)?;
            }
        } else if cleanable(&e) {
            out.push(e);
        }
    }
    Ok(())
}

/// Output paths in `out`: the input's name with the format's extension; when two inputs would
/// write the same file (`a.pdf` and `a.docx`), the later keeps its old extension in the name.
fn plan_jobs(inputs: Vec<PathBuf>, out: &Path, format: &str) -> Vec<Job> {
    let mut used = std::collections::HashSet::new();
    inputs
        .into_iter()
        .map(|input| {
            let stem = input.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "document".into());
            let mut name = format!("{stem}.{format}");
            if !used.insert(name.clone()) {
                let ext = input.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
                name = format!("{stem}-{ext}.{format}");
                let mut n = 2;
                while !used.insert(name.clone()) {
                    name = format!("{stem}-{ext}-{n}.{format}");
                    n += 1;
                }
            }
            Job { output: out.join(name), input }
        })
        .collect()
}

fn summary(r: &FileReport) -> String {
    if !r.ok() {
        return format!("failed  {}: {}", r.input.display(), escape_text(&r.error, false));
    }
    let mut s = format!("cleaned {} -> {}", r.input.display(), r.output.display());
    let mut parts = Vec::new();
    if !r.removed.is_empty() {
        parts.push(format!("{} watermark spans removed", r.removed.len()));
    }
    if r.native_watermark_removed {
        parts.push("document watermark removed".into());
    }
    if r.learned_text_removed > 0 {
        parts.push(format!("{} learned watermark texts removed", r.learned_text_removed));
    }
    if r.urls_removed > 0 {
        parts.push(format!("{} web addresses removed", r.urls_removed));
    }
    if r.links_unlinked > 0 {
        parts.push(format!("{} links unlinked", r.links_unlinked));
    }
    if !r.flagged.is_empty() {
        parts.push(format!("{} flagged for review", r.flagged.len()));
    }
    if !parts.is_empty() {
        s += &format!(" ({})", parts.join(", "));
    }
    if !r.needs_ocr.is_empty() {
        s += &format!("\n        warning: pages {:?} are scans with no text layer; their text is not in the output (run OCR first)", r.needs_ocr);
    }
    if !r.incomplete_pages.is_empty() {
        s += &format!("\n        warning: pages {:?} could not be read completely", r.incomplete_pages);
    }
    s
}

/// `clean <in>… --out DIR`.
pub(crate) fn run_clean(rest: &[String]) -> Result<(), Failure> {
    validate_options("clean", rest, CLEAN_OPTIONS)?;
    let inputs = positionals(rest, CLEAN_OPTIONS);
    if inputs.is_empty() {
        return Err("clean: name at least one file or folder".into());
    }
    let out = PathBuf::from(value(rest, "--out")?.ok_or("clean: --out DIR is required")?);
    let format = value(rest, "--format")?.unwrap_or_else(|| "docx".into()).to_ascii_lowercase();
    if !OUT_FORMATS.contains(&format.as_str()) {
        return Err(format!("clean: --format must be one of {}", OUT_FORMATS.join(", ")).into());
    }
    let urls = match value(rest, "--urls")? {
        None => UrlAction::Remove,
        Some(v) => UrlAction::parse(&v).ok_or("clean: --urls must be remove, unlink or keep")?,
    };
    let workers = match value(rest, "--jobs")? {
        None => std::thread::available_parallelism().map_or(4, |n| n.get()),
        Some(v) => v.parse::<usize>().ok().filter(|n| *n > 0).ok_or("clean: --jobs must be a positive number")?,
    };
    let model_file = model_path(rest)?;
    let model = Model::load(&model_file)?;
    let drop_flagged = rest.iter().any(|a| a == "--drop-flagged");
    let page_breaks = !rest.iter().any(|a| a == "--no-page-breaks");
    let password = password_from(rest)?;
    let files = collect_inputs(&inputs, rest.iter().any(|a| a == "--recursive"))?;
    if files.is_empty() {
        return Err("clean: no PDF or document files found".into());
    }
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let jobs = plan_jobs(files, &out, &format);
    let opts = Options { model: Arc::new(model), rebuild: RebuildOptions { drop_flagged, urls, page_breaks }, password: password.clone(), workers };
    let started = std::time::Instant::now();
    let reports = pipeline::run(&jobs, &opts, &EngineIo { password }, &|r| eprintln!("{}", summary(r)));
    let failed = reports.iter().filter(|r| !r.ok()).count();
    let flagged: usize = reports.iter().map(|r| r.flagged.len()).sum();
    let report_path = value(rest, "--report")?.map_or_else(|| out.join("clean-report.json"), PathBuf::from);
    let batch = BatchReport { format, urls, drop_flagged, page_breaks, model: model_file, files: reports };
    std::fs::write(&report_path, serde_json::to_string_pretty(&batch).unwrap_or_default()).map_err(|e| format!("{}: {e}", report_path.display()))?;
    eprintln!(
        "{} of {} files cleaned in {:.1}s with {workers} workers per stage; report: {}",
        batch.files.len() - failed,
        batch.files.len(),
        started.elapsed().as_secs_f32(),
        report_path.display()
    );
    if flagged > 0 {
        eprintln!("{flagged} spans were flagged and kept; decide them with: wordcraft-cli clean-review {}", report_path.display());
    }
    if failed > 0 {
        return Err(format!("clean: {failed} files failed").into());
    }
    Ok(())
}

fn ask(prompt: &str) -> Result<Option<String>, Failure> {
    let mut err = std::io::stderr();
    write!(err, "{prompt}")?;
    err.flush()?;
    let mut line = String::new();
    let n = std::io::stdin().lock().read_line(&mut line)?;
    Ok((n > 0).then(|| line.trim().to_string()))
}

/// `1,3 5-7` → the 1-based numbers it names, within `1..=max`.
fn parse_numbers(s: &str, max: usize) -> Result<Vec<usize>, String> {
    let mut out = Vec::new();
    for part in s.split(|c: char| c == ',' || c.is_whitespace()).filter(|p| !p.is_empty()) {
        let (a, b) = part.split_once('-').unwrap_or((part, part));
        let (a, b) = (
            a.trim().parse::<usize>().map_err(|_| format!("not a number: {part}"))?,
            b.trim().parse::<usize>().map_err(|_| format!("not a number: {part}"))?,
        );
        if a == 0 || b > max || a > b {
            return Err(format!("{part} is not between 1 and {max}"));
        }
        out.extend(a..=b);
    }
    Ok(out)
}

/// `clean-learn <file>`: show the most watermark-like texts of a file and learn from the ones
/// a person marks.
pub(crate) fn run_learn(rest: &[String]) -> Result<(), Failure> {
    validate_options("clean-learn", rest, LEARN_OPTIONS)?;
    let paths = positionals(rest, LEARN_OPTIONS);
    let file = PathBuf::from(paths.first().ok_or("clean-learn: name a PDF to learn from")?.as_str());
    let model_file = model_path(rest)?;
    let mut model = Model::load(&model_file)?;
    let limit = match value(rest, "--limit")? {
        None => 25,
        Some(v) => v.parse::<usize>().ok().filter(|n| *n > 0).ok_or("clean-learn: --limit must be a positive number")?,
    };
    let (marks, nots) = (values(rest, "--mark"), values(rest, "--not"));
    let bytes = std::fs::read(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    let mut labelled = Vec::new();
    let mut unmarked = Vec::new();
    if pipeline::is_pdf(&file, &bytes) {
        let pages = extract_pdf(&bytes, password_from(rest)?.as_deref()).map_err(|e| e.to_string())?;
        let cands = candidates(&pages, &model, limit.max(marks.len() + nots.len() + 50));
        if marks.is_empty() && nots.is_empty() {
            let shown = &cands[..cands.len().min(limit)];
            if shown.is_empty() {
                return Err("clean-learn: the file has no text to learn from".into());
            }
            eprintln!("The most watermark-like texts in {} (score: how sure the detector is now):\n", file.display());
            for (i, c) in shown.iter().enumerate() {
                let pages = if c.pages.len() > 6 { format!("{} pages", c.pages.len()) } else { format!("pages {:?}", c.pages) };
                eprintln!("{:>3}. [{:>3.0}%] {:<60} {pages}, {:.0}°", i + 1, c.score * 100.0, clip(&escape_text(&c.text, false), 60), c.angle);
            }
            eprintln!();
            let yes = ask("Numbers of the REAL watermarks (e.g. 1,3 or 2-4; empty for none): ")?.unwrap_or_default();
            let no = ask("Numbers that are NOT watermarks, to teach it what to keep (optional): ")?.unwrap_or_default();
            let (yes, no) = (parse_numbers(&yes, shown.len())?, parse_numbers(&no, shown.len())?);
            for &n in &yes {
                labelled.push((shown[n - 1].text.clone(), shown[n - 1].features, true));
            }
            for &n in &no {
                labelled.push((shown[n - 1].text.clone(), shown[n - 1].features, false));
            }
            // What was shown and left unmarked trains the weights as content too.
            unmarked = (1..=shown.len())
                .filter(|n| !yes.contains(n) && !no.contains(n))
                .map(|n| (shown[n - 1].text.clone(), shown[n - 1].features))
                .collect();
        } else {
            for (texts, watermark) in [(&marks, true), (&nots, false)] {
                for t in texts {
                    let norm = normalize(t);
                    let hits: Vec<_> = cands.iter().filter(|c| similarity(&normalize(&c.text), &norm) >= 0.8).collect();
                    if hits.is_empty() {
                        eprintln!("note: \"{}\" is not in {}; learned from its text only", escape_text(t, false), file.display());
                        labelled.push((t.clone(), [0.0; wordcraft_clean::detect::N], watermark));
                    }
                    for c in hits {
                        labelled.push((c.text.clone(), c.features, watermark));
                    }
                }
            }
        }
    } else {
        // An editable document has no drawing features to learn from: learn the texts.
        if marks.is_empty() && nots.is_empty() {
            return Err("clean-learn: for a document that is not a PDF, give the watermark text with --mark TEXT".into());
        }
        for (texts, watermark) in [(&marks, true), (&nots, false)] {
            labelled.extend(texts.iter().map(|t| (t.clone(), [0.0; wordcraft_clean::detect::N], watermark)));
        }
    }
    if labelled.is_empty() {
        eprintln!("nothing marked; the model is unchanged");
        return Ok(());
    }
    let marked = labelled.len();
    let total = model.learn_with(labelled, unmarked);
    save_model(&model, &model_file)?;
    eprintln!("learned from {marked} marks ({total} examples in all); model saved to {}", model_file.display());
    Ok(())
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { s.chars().take(n - 1).chain(['…']).collect() }
}

/// `clean-review <report.json>`: decide each flagged text, learn from the answers, and clean the
/// files again with what was learned.
pub(crate) fn run_review(rest: &[String]) -> Result<(), Failure> {
    validate_options("clean-review", rest, REVIEW_OPTIONS)?;
    let paths = positionals(rest, REVIEW_OPTIONS);
    let report_path = PathBuf::from(paths.first().ok_or("clean-review: name the report clean wrote")?.as_str());
    let raw = std::fs::read(&report_path).map_err(|e| format!("{}: {e}", report_path.display()))?;
    let mut batch: BatchReport = serde_json::from_slice(&raw).map_err(|e| format!("{}: {e}", report_path.display()))?;
    let model_file = match value(rest, "--model")? {
        Some(m) => PathBuf::from(m),
        None => batch.model.clone(),
    };
    let mut model = Model::load(&model_file)?;
    // One question per distinct text, however many pages and files it is on.
    let mut groups: Vec<(String, Vec<(usize, usize)>)> = Vec::new(); // normalised text, (file, item)
    for (fi, f) in batch.files.iter().enumerate() {
        for (ii, it) in f.flagged.iter().enumerate() {
            let norm = normalize(&it.text);
            match groups.iter_mut().find(|(t, _)| *t == norm) {
                Some((_, v)) => v.push((fi, ii)),
                None => groups.push((norm, vec![(fi, ii)])),
            }
        }
    }
    if groups.is_empty() {
        eprintln!("nothing was flagged in {}", report_path.display());
        return Ok(());
    }
    eprintln!("{} flagged texts. For each: y = it is a watermark, n = it is content, s = skip, q = stop.\n", groups.len());
    let mut labelled = Vec::new();
    // Every file with something flagged is cleaned again: what is learned from one answer can
    // settle flagged text in files that weren't asked about.
    let rerun: std::collections::BTreeSet<usize> = batch.files.iter().enumerate().filter(|(_, f)| !f.flagged.is_empty()).map(|(i, _)| i).collect();
    for (gi, (_, members)) in groups.iter().enumerate() {
        let (fi, ii) = members[0];
        let it = &batch.files[fi].flagged[ii];
        let files: std::collections::BTreeSet<usize> = members.iter().map(|m| m.0).collect();
        eprintln!(
            "{}/{}  [{:.0}%] \"{}\"  (page {} of {}{})",
            gi + 1,
            groups.len(),
            it.score * 100.0,
            escape_text(&it.text, false),
            it.page,
            batch.files[fi].input.display(),
            if members.len() > 1 { format!(", {} places in {} files", members.len(), files.len()) } else { String::new() }
        );
        let answer = loop {
            match ask("  watermark? [y/n/s/q] ")?.as_deref().map(str::to_ascii_lowercase).as_deref() {
                Some("y" | "yes") => break Some(true),
                Some("n" | "no") => break Some(false),
                Some("s" | "skip" | "") => break None,
                Some("q" | "quit") | None => {
                    return finish_review(&mut model, labelled, &model_file, (&mut batch, &report_path), &rerun, rest);
                }
                Some(_) => eprintln!("  answer y, n, s or q"),
            }
        };
        if let Some(watermark) = answer {
            labelled.push((it.text.clone(), it.features, watermark));
        }
    }
    finish_review(&mut model, labelled, &model_file, (&mut batch, &report_path), &rerun, rest)
}

fn finish_review(
    model: &mut Model,
    labelled: Vec<(String, [f32; wordcraft_clean::detect::N], bool)>,
    model_file: &Path,
    (batch, report_path): (&mut BatchReport, &Path),
    rerun: &std::collections::BTreeSet<usize>,
    rest: &[String],
) -> Result<(), Failure> {
    if labelled.is_empty() {
        eprintln!("no answers; the model is unchanged");
        return Ok(());
    }
    let n = labelled.len();
    model.learn(labelled);
    save_model(model, model_file)?;
    eprintln!("learned from {n} answers; model saved to {}", model_file.display());
    if rest.iter().any(|a| a == "--no-rerun") || rerun.is_empty() {
        return Ok(());
    }
    let jobs: Vec<Job> = rerun.iter().filter_map(|&i| batch.files.get(i)).map(|f| Job { input: f.input.clone(), output: f.output.clone() }).collect();
    let password = password_from(rest)?;
    let opts = Options {
        model: Arc::new(model.clone()),
        rebuild: RebuildOptions { drop_flagged: batch.drop_flagged, urls: batch.urls, page_breaks: batch.page_breaks },
        password: password.clone(),
        workers: std::thread::available_parallelism().map_or(4, |n| n.get()),
    };
    eprintln!("cleaning {} files again with what was learned:", jobs.len());
    let reports = pipeline::run(&jobs, &opts, &EngineIo { password }, &|r| eprintln!("{}", summary(r)));
    let failed = reports.iter().filter(|r| !r.ok()).count();
    // The report now describes the files as cleaned again, so a second review asks only what is
    // still flagged.
    for (&i, r) in rerun.iter().zip(reports) {
        if let Some(slot) = batch.files.get_mut(i) {
            *slot = r;
        }
    }
    batch.model = model_file.to_path_buf();
    std::fs::write(report_path, serde_json::to_string_pretty(&*batch).unwrap_or_default()).map_err(|e| format!("{}: {e}", report_path.display()))?;
    if failed > 0 {
        return Err(format!("clean-review: {failed} files failed").into());
    }
    Ok(())
}
