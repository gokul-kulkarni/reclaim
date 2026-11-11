//! `reclaim large`: list the biggest individual files, and optionally move the
//! ones the user approves, one by one, to the Trash.

use std::fmt::Write as _;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use reclaim_core::exec;
use reclaim_core::format::{bytes, ellipsize, epoch_millis, relative_time};
use reclaim_core::journal::Trigger;
use reclaim_core::large_files::{self, LargeFile, LargeFileOptions, LargeFileScan};
use reclaim_core::Paths;

use crate::app::App;
use crate::cli::LargeArgs;
use crate::render::{self, Style};

pub fn run(app: &App, args: &LargeArgs, style: &Style) -> Result<u8> {
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if args.trash && !interactive {
        // Every file is the user's own data; there is no unattended mode.
        bail!("--trash needs an interactive terminal: each file is confirmed individually");
    }

    let roots = resolve_roots(app, &args.paths)?;
    let opts = options(app, args);
    let scan = search(&roots, &opts);

    if args.json {
        println!("{}", serde_json::to_string_pretty(&json_payload(&scan))?);
        return Ok(0);
    }

    print!("{}", table(&scan, &app.paths, &opts, style));
    if !args.trash || scan.files.is_empty() {
        return Ok(0);
    }

    let chosen = pick(&scan.files, app, style)?;
    if chosen.is_empty() {
        println!("Nothing selected. Nothing was moved.");
        return Ok(0);
    }

    let record = exec::trash_files(&chosen, &app.guard, args.dry_run, Trigger::Cli);
    if !args.dry_run {
        if let Err(e) = app.journal.write(&record) {
            eprintln!(
                "{}",
                style.yellow(&format!("warning: could not write run history: {e}"))
            );
        }
        let _ = app.journal.prune(200);
    }

    println!();
    print!("{}", render::run_report(&record, style));
    if args.dry_run {
        println!(
            "\n{}",
            style.dim("This was a dry run. Re-run without --dry-run to apply.")
        );
    }
    Ok(if record.succeeded() { 0 } else { 1 })
}

/// Positional paths, made absolute and checked to exist; the home directory
/// when none are given.
fn resolve_roots(app: &App, raw: &[PathBuf]) -> Result<Vec<PathBuf>> {
    if raw.is_empty() {
        return Ok(vec![app.paths.home().to_path_buf()]);
    }
    raw.iter()
        .map(|p| {
            let expanded = app.paths.expand(&p.to_string_lossy());
            expanded
                .canonicalize()
                .with_context(|| format!("{} does not exist", p.display()))
        })
        .collect()
}

fn options(app: &App, args: &LargeArgs) -> LargeFileOptions {
    LargeFileOptions {
        min_size: args.min_size,
        min_age_days: args.older_than.unwrap_or(0),
        limit: args.limit,
        include_hidden: args.include_hidden,
        cross_device: app.config.scan.cross_device,
        exclude: app.config.scan.exclude.clone(),
        threads: app.config.scan.threads(),
        ..LargeFileOptions::default()
    }
}

/// Run the search, with a self-erasing status line on stderr so a long walk
/// over a home directory does not look like a hang.
fn search(roots: &[PathBuf], opts: &LargeFileOptions) -> LargeFileScan {
    let show = std::io::stderr().is_terminal();
    if show {
        eprint!(
            "reclaim: looking for files of {} or more…",
            bytes(opts.min_size)
        );
        let _ = std::io::stderr().flush();
    }
    let scan = large_files::find_large_files(roots, opts);
    if show {
        eprint!("\r\x1b[2K");
    }
    scan
}

fn table(scan: &LargeFileScan, paths: &Paths, opts: &LargeFileOptions, style: &Style) -> String {
    let mut out = String::new();
    if scan.files.is_empty() {
        let _ = writeln!(
            out,
            "No files of {} or more found ({} files checked).",
            bytes(opts.min_size),
            scan.files_scanned
        );
        return out;
    }

    let _ = writeln!(
        out,
        "{}",
        style.bold(&format!(
            "{:>4}  {:>9}  {:<10}  {:<14}  {}",
            "#", "SIZE", "KIND", "MODIFIED", "PATH"
        ))
    );
    for (i, file) in scan.files.iter().enumerate() {
        let _ = writeln!(
            out,
            "{:>4}  {:>9}  {:<10}  {:<14}  {}",
            i + 1,
            bytes(file.on_disk),
            file.kind.as_str(),
            file.modified
                .map(relative_time)
                .unwrap_or_else(|| "-".into()),
            ellipsize(&paths.contract(&file.path), 72)
        );
        for note in notes(file) {
            let _ = writeln!(out, "{:>42}{}", "", style.yellow(&note));
        }
    }

    let _ = writeln!(out);
    let shown: u64 = scan.files.iter().map(|f| f.on_disk).sum();
    let _ = writeln!(
        out,
        "{}",
        style.bold(&format!(
            "{} in {} file(s) shown.",
            bytes(shown),
            scan.files.len()
        ))
    );
    if scan.matched > scan.files.len() {
        let _ = writeln!(
            out,
            "{}",
            style.dim(&format!(
                "{} more file(s), {} in total, hidden by --limit.",
                scan.matched - scan.files.len(),
                bytes(scan.matched_bytes - shown)
            ))
        );
    }
    let _ = writeln!(
        out,
        "{}",
        style.dim(&format!(
            "Checked {} files in {} ms. Caches and build artifacts are skipped; `reclaim scan` covers those.",
            scan.files_scanned, scan.elapsed_ms
        ))
    );
    if !scan.unreadable.is_empty() {
        let _ = writeln!(
            out,
            "{}",
            style.yellow(&format!(
                "{} director(ies) could not be read, so the list may be incomplete.",
                scan.unreadable.len()
            ))
        );
    }
    out
}

/// Things worth knowing before deleting a particular file.
fn notes(file: &LargeFile) -> Vec<String> {
    let mut notes = Vec::new();
    if file.hardlinked {
        notes.push("hardlinked elsewhere: removing this path frees nothing".to_string());
    }
    if file.logical > file.on_disk.saturating_mul(2) {
        notes.push(format!(
            "sparse: appears as {} but only {} is on disk",
            bytes(file.logical),
            bytes(file.on_disk)
        ));
    }
    notes
}

/// Ask about each file in turn. `q` stops asking and keeps what was approved.
fn pick(files: &[LargeFile], app: &App, style: &Style) -> Result<Vec<LargeFile>> {
    println!();
    println!(
        "{}",
        style
            .bold("Choose files to move to the Trash. y = yes, n = no (default), q = stop asking.")
    );

    let mut chosen = Vec::new();
    for (i, file) in files.iter().enumerate() {
        let label = format!(
            "[{}/{}] {} {}",
            i + 1,
            files.len(),
            bytes(file.on_disk),
            app.paths.contract(&file.path)
        );
        // Do not ask about something the guard would refuse anyway.
        if let Err(e) = app.guard.check_user_file(&file.path) {
            println!("  {label}\n    {}", style.dim(&format!("skipped: {e}")));
            continue;
        }
        match ask(&format!("  {label}\n    Move to Trash?"))? {
            Answer::Yes => chosen.push(file.clone()),
            Answer::No => {}
            Answer::Quit => break,
        }
    }
    Ok(chosen)
}

enum Answer {
    Yes,
    No,
    Quit,
}

fn ask(prompt: &str) -> Result<Answer> {
    print!("{prompt} [y/N/q] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer)? == 0 {
        return Ok(Answer::Quit); // EOF: stop asking, never assume yes
    }
    Ok(parse_answer(&answer))
}

fn parse_answer(raw: &str) -> Answer {
    match raw.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Answer::Yes,
        "q" | "quit" => Answer::Quit,
        _ => Answer::No,
    }
}

/// JSON with timestamps as epoch milliseconds rather than serde's `{secs, nanos}`.
fn json_payload(scan: &LargeFileScan) -> serde_json::Value {
    let files: Vec<serde_json::Value> = scan
        .files
        .iter()
        .map(|f| {
            serde_json::json!({
                "path": f.path,
                "on_disk": f.on_disk,
                "logical": f.logical,
                "kind": f.kind,
                "modified_ms": f.modified.map(epoch_millis),
                "accessed_ms": f.accessed.map(epoch_millis),
                "hardlinked": f.hardlinked,
            })
        })
        .collect();
    serde_json::json!({
        "files": files,
        "matched": scan.matched,
        "matched_bytes": scan.matched_bytes,
        "files_scanned": scan.files_scanned,
        "unreadable": scan.unreadable,
        "elapsed_ms": scan.elapsed_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::GlobalArgs;
    use reclaim_core::large_files::FileKind;

    fn app_at(root: &std::path::Path) -> App {
        App::new(&GlobalArgs {
            config: None,
            root: Some(root.to_path_buf()),
            concurrency: Some(2),
            verbose: 0,
            no_color: true,
        })
        .unwrap()
    }

    fn file(path: &str, on_disk: u64, logical: u64, hardlinked: bool) -> LargeFile {
        LargeFile {
            path: PathBuf::from(path),
            on_disk,
            logical,
            modified: None,
            accessed: None,
            kind: FileKind::Video,
            hardlinked,
        }
    }

    #[test]
    fn no_paths_means_the_home_directory() {
        let tmp = tempfile::TempDir::new().unwrap();
        let app = app_at(tmp.path());
        let roots = resolve_roots(&app, &[]).unwrap();
        assert_eq!(roots, vec![app.paths.home().to_path_buf()]);
    }

    #[test]
    fn a_missing_path_is_an_error_not_an_empty_result() {
        let tmp = tempfile::TempDir::new().unwrap();
        let app = app_at(tmp.path());
        let err = resolve_roots(&app, &[PathBuf::from("/definitely/not/here")]).unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    #[test]
    fn the_table_lists_files_and_explains_what_the_limit_hid() {
        let tmp = tempfile::TempDir::new().unwrap();
        let app = app_at(tmp.path());
        let scan = LargeFileScan {
            files: vec![file("/x/movie.mov", 2048, 2048, false)],
            matched: 3,
            matched_bytes: 5000,
            files_scanned: 10,
            ..LargeFileScan::default()
        };
        let text = table(
            &scan,
            &app.paths,
            &LargeFileOptions::default(),
            &Style::plain(),
        );
        assert!(text.contains("movie.mov"), "{text}");
        assert!(text.contains("video"), "{text}");
        assert!(text.contains("2 more file(s)"), "{text}");
    }

    #[test]
    fn an_empty_result_says_so() {
        let tmp = tempfile::TempDir::new().unwrap();
        let app = app_at(tmp.path());
        let text = table(
            &LargeFileScan::default(),
            &app.paths,
            &LargeFileOptions::default(),
            &Style::plain(),
        );
        assert!(text.starts_with("No files of"), "{text}");
    }

    #[test]
    fn hardlinked_and_sparse_files_carry_a_note() {
        assert!(notes(&file("/a", 100, 100, false)).is_empty());
        assert_eq!(notes(&file("/a", 100, 100, true)).len(), 1);
        assert!(notes(&file("/a", 100, 10_000, false))[0].contains("sparse"));
    }

    #[test]
    fn only_an_explicit_yes_selects_a_file() {
        assert!(matches!(parse_answer("y\n"), Answer::Yes));
        assert!(matches!(parse_answer("YES"), Answer::Yes));
        assert!(matches!(parse_answer("\n"), Answer::No));
        assert!(matches!(parse_answer("sure"), Answer::No));
        assert!(matches!(parse_answer("q"), Answer::Quit));
    }

    #[test]
    fn json_uses_epoch_millis_for_timestamps() {
        let mut f = file("/a.mov", 1, 1, false);
        f.modified = Some(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(2));
        let scan = LargeFileScan {
            files: vec![f],
            matched: 1,
            ..LargeFileScan::default()
        };
        let json = json_payload(&scan);
        assert_eq!(json["files"][0]["modified_ms"], 2000);
        assert_eq!(json["files"][0]["kind"], "video");
    }
}
