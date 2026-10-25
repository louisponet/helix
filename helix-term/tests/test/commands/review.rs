use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

#[cfg(windows)]
use crossterm::event::{Event as TerminalEvent, KeyEvent as TerminalKey};
use helix_core::syntax::config::SoftWrap;
use helix_core::Range;
use helix_term::{
    application::Application,
    config::{Config, ConfigLoadError},
    keymap::Keymaps,
    ui::EditorView,
};
use helix_view::{
    current_ref, doc,
    editor::{AutoSaveAfterDelay, Severity, StatusLineElement},
    graphics::Rect,
    input::parse_macro,
    review::{HunkStage, OldBlock, ReviewBaseline},
};
use tempfile::TempDir;
#[cfg(not(windows))]
use termina::event::{Event as TerminalEvent, KeyEvent as TerminalKey};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tui::buffer::Buffer;

use super::*;

fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env("GIT_TERMINAL_PROMPT", "false")
        .env("GIT_AUTHOR_NAME", "author")
        .env("GIT_AUTHOR_EMAIL", "author@example.com")
        .env("GIT_COMMITTER_NAME", "committer")
        .env("GIT_COMMITTER_EMAIL", "committer@example.com")
        .env("GIT_CONFIG_COUNT", "3")
        .env("GIT_CONFIG_KEY_0", "commit.gpgsign")
        .env("GIT_CONFIG_VALUE_0", "false")
        .env("GIT_CONFIG_KEY_1", "init.defaultBranch")
        .env("GIT_CONFIG_VALUE_1", "main")
        .env("GIT_CONFIG_KEY_2", "core.autocrlf")
        .env("GIT_CONFIG_VALUE_2", "false")
        .output()
        .expect("failed to run git")
}

fn git_ok(dir: &Path, args: &[&str]) -> String {
    let output = git(dir, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// A git repository with `file.txt` committed as `head`, staged as `index` and written as
/// `worktree`.
struct Repo {
    dir: TempDir,
    file: PathBuf,
}

impl Repo {
    fn new(head: &str, index: &str, worktree: &str) -> Repo {
        let dir = tempfile::tempdir().unwrap();
        git_ok(dir.path(), &["init"]);
        let file = dir.path().join("file.txt");
        let repo = Repo { dir, file };
        repo.write(head);
        repo.stage();
        git_ok(repo.dir.path(), &["commit", "-m", "head"]);
        repo.write(index);
        repo.stage();
        repo.write(worktree);
        repo
    }

    fn write(&self, content: &str) {
        fs::write(&self.file, content).unwrap();
    }

    fn write_external(&self, content: &str) {
        self.write(content);
        fs::File::options()
            .write(true)
            .open(&self.file)
            .unwrap()
            .set_times(
                fs::FileTimes::new()
                    .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(1)),
            )
            .unwrap();
    }

    fn stage(&self) {
        git_ok(self.dir.path(), &["add", "file.txt"]);
    }

    fn index_oid(&self) -> String {
        git_ok(self.dir.path(), &["rev-parse", ":file.txt"])
    }

    fn disk(&self) -> String {
        fs::read_to_string(&self.file).unwrap()
    }

    fn app(&self) -> anyhow::Result<Application> {
        AppBuilder::new().with_file(&self.file, None).build()
    }
}

const INDEX: &str = "gone1\ngone2\nkeep1\nkeep2\ntail1\ntail2\n";
const DISK: &str = "keep1\nkeep2\n";

fn assert_error(app: &Application, contains: &str) {
    let (status, severity) = app.editor.get_status().expect("expected an error");
    assert_eq!(*severity, Severity::Error, "{status}");
    assert!(status.contains(contains), "{status}");
}

fn render_surface(app: &Application) -> Buffer {
    let editor = &app.editor;
    let (view, doc) = current_ref!(editor);
    let mut surface = Buffer::empty(Rect::new(0, 0, view.area.right(), view.area.bottom()));
    EditorView::new(Keymaps::default()).render_view(
        editor,
        doc,
        view,
        view.area,
        &mut surface,
        true,
    );
    surface
}

/// Renders the focused view and returns the gutter column next to the text and the text of
/// every row of the text area.
fn render(app: &Application) -> Vec<(String, String)> {
    let surface = render_surface(app);
    let (view, doc) = current_ref!(app.editor);
    let inner = view.inner_area(doc);
    (inner.top()..inner.bottom())
        .map(|y| {
            let marker = surface[(inner.x - 1, y)].symbol.as_str().to_owned();
            let text: String = (inner.left()..inner.right())
                .map(|x| surface[(x, y)].symbol.as_str())
                .collect();
            (marker, text.trim_end().to_owned())
        })
        .collect()
}

/// Feeds `keys` followed by a bracketed paste of `paste` to the application.
async fn send_keys_and_paste(app: &mut Application, keys: &str, paste: &str) -> anyhow::Result<()> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    for key in parse_macro(keys)? {
        tx.send(Ok(TerminalEvent::Key(TerminalKey::from(key))))?;
    }
    tx.send(Ok(TerminalEvent::Paste(paste.to_owned())))?;
    app.event_loop_until_idle(&mut UnboundedReceiverStream::new(rx))
        .await;
    Ok(())
}

async fn send_keys(app: &mut Application, keys: &str) -> anyhow::Result<()> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    for key in parse_macro(keys)? {
        tx.send(Ok(TerminalEvent::Key(TerminalKey::from(key))))?;
    }
    app.event_loop_until_idle(&mut UnboundedReceiverStream::new(rx))
        .await;
    Ok(())
}

const REVIEW_TOUCH_NONCE: &str = "c983fb0c-3199-4018-9b03-25bd24f498b8";

fn touch_app(repo: &Repo) -> anyhow::Result<Application> {
    let previous = std::env::var_os("HELIX_REVIEW_ACTION_TOKEN");
    std::env::set_var("HELIX_REVIEW_ACTION_TOKEN", REVIEW_TOUCH_NONCE);
    let app = repo.app();
    match previous {
        Some(value) => std::env::set_var("HELIX_REVIEW_ACTION_TOKEN", value),
        None => std::env::remove_var("HELIX_REVIEW_ACTION_TOKEN"),
    }
    app
}

fn touch_frame(id: usize, action: &str, text: Option<&str>) -> String {
    let mut payload = serde_json::json!({
        "v": 1,
        "id": format!("a4f9d271-dcae-40fe-8bec-{id:012}"),
        "action": action,
    });
    if let Some(text) = text {
        payload["text"] = text.into();
    }
    touch_json_frame(&payload)
}

fn touch_json_frame(payload: &serde_json::Value) -> String {
    let encoded: String = serde_json::to_vec(payload)
        .unwrap()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("# hx-review:{REVIEW_TOUCH_NONCE}:{encoded}")
}

/// Sets the working directory, and so the workspace, until dropped.
struct Cwd(PathBuf);

impl Cwd {
    fn enter(dir: &Path) -> Cwd {
        let previous = helix_stdx::env::current_working_dir();
        helix_stdx::env::set_current_working_dir(dir).unwrap();
        Cwd(previous)
    }
}

impl Drop for Cwd {
    fn drop(&mut self) {
        helix_stdx::env::set_current_working_dir(&self.0).unwrap();
    }
}

/// The statusline of the focused view.
fn statusline(app: &Application) -> String {
    let surface = render_surface(app);
    let view = current_ref!(app.editor).0;
    let y = view.area.bottom() - 1;
    (view.area.left()..view.area.right())
        .map(|x| surface[(x, y)].symbol.as_str())
        .collect()
}

/// Points the review handoff at a directory that is then removed, so comments sent to the agent
/// fail and stay pending.
async fn break_handoff(app: &mut Application, dir: &Path) -> anyhow::Result<()> {
    let gone = dir.join("gone");
    fs::create_dir(&gone)?;
    send_keys(app, &format!(":review-handoff {}<ret>", gone.display())).await?;
    fs::remove_dir(&gone)?;
    Ok(())
}

/// The batches in `outbox`, removed so the next export is the only one.
fn take_batches(outbox: &Path) -> Vec<serde_json::Value> {
    let mut paths: Vec<_> = fs::read_dir(outbox)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let json = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
            fs::remove_file(path).unwrap();
            json
        })
        .collect()
}

fn old_focus(app: &Application) -> Option<Range> {
    let (view, doc) = current_ref!(app.editor);
    doc.review.as_ref()?.focus(view.id).map(|focus| focus.range)
}

#[tokio::test(flavor = "multi_thread")]
async fn review_loads_external_edits_only_after_index_validation() -> anyhow::Result<()> {
    let repo = Repo::new(INDEX, INDEX, INDEX);
    let mut app = repo.app()?;
    repo.write_external(DISK);
    test_key_sequence(
        &mut app,
        Some(":review<ret>"),
        Some(&|app| {
            assert_status_not_error(&app.editor);
            let doc = doc!(app.editor);
            assert_eq!(doc.text(), DISK);
            assert!(!doc.is_modified());
            assert_eq!(doc.review.as_ref().unwrap().blocks().len(), 2);
            assert_eq!(repo.disk(), DISK);
            assert!(render(app)
                .iter()
                .any(|(marker, text)| marker == "-" && text == "gone1"));
        }),
        false,
    )
    .await?;

    let mut app = repo.app()?;
    repo.write_external("external\n");
    git_ok(repo.dir.path(), &["rm", "--cached", "file.txt"]);
    test_key_sequence(
        &mut app,
        Some(":review<ret>"),
        Some(&|app| {
            assert_error(app, "not in the git index");
            assert_eq!(doc!(app.editor).text(), DISK);
            assert!(doc!(app.editor).review.is_none());
            assert_eq!(repo.disk(), "external\n");
        }),
        false,
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn review_refuses_dirty_external_conflicts_without_losing_either_text() -> anyhow::Result<()>
{
    let repo = Repo::new(INDEX, INDEX, DISK);
    let mut app = repo.app()?;
    send_keys_and_paste(&mut app, "i", "local ").await?;
    repo.write_external("external\n");
    test_key_sequence(
        &mut app,
        Some("<esc>:review<ret>"),
        Some(&|app| {
            assert_error(app, "unsaved changes and was modified on disk");
            let doc = doc!(app.editor);
            assert_eq!(doc.text(), "local keep1\nkeep2\n");
            assert!(doc.is_modified());
            assert!(doc.review.is_none());
            assert_eq!(repo.disk(), "external\n");
        }),
        false,
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn export_refuses_external_changes_and_preserves_comments_until_refresh() -> anyhow::Result<()>
{
    let repo = Repo::new(INDEX, INDEX, DISK);
    let batch = repo.dir.path().join("batch.json");
    let export = format!(":review-export {}<ret>", batch.display());
    let mut app = repo.app()?;
    break_handoff(&mut app, repo.dir.path()).await?;
    test_key_sequences(
        &mut app,
        vec![
            (
                // The comment cannot be sent, so it stays pending.
                Some(":review<ret>:review-comment retained<ret>"),
                Some(&|app| {
                    assert_eq!(app.editor.review_batch.comments().len(), 1);
                    repo.write_external("external\n");
                }),
            ),
            (
                Some(&export),
                Some(&|app| {
                    assert_error(app, "changed on disk; use :review-refresh");
                    assert!(!batch.exists());
                    assert_eq!(app.editor.review_batch.comments().len(), 1);
                    assert_eq!(doc!(app.editor).text(), DISK);
                    assert_eq!(repo.disk(), "external\n");
                }),
            ),
            (
                Some(":review-refresh<ret>"),
                Some(&|app| {
                    assert_status_not_error(&app.editor);
                    assert_eq!(doc!(app.editor).text(), "external\n");
                    assert_eq!(app.editor.review_batch.comments().len(), 1);
                }),
            ),
            (
                Some(&export),
                Some(&|app| {
                    assert_status_not_error(&app.editor);
                    assert!(app.editor.review_batch.comments().is_empty());
                    let json: serde_json::Value =
                        serde_json::from_str(&fs::read_to_string(&batch).unwrap()).unwrap();
                    assert_eq!(json["comments"][0]["comment"], "retained");
                    assert_eq!(json["comments"][0]["selected_text"], "k");
                    assert_eq!(repo.disk(), "external\n");
                }),
            ),
        ],
        false,
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn review_limits_refuse_large_files_but_leave_them_editable() -> anyhow::Result<()> {
    use helix_view::review::Review;
    for oversized in [
        "x".repeat(Review::MAX_BYTES + 1),
        "\n".repeat(Review::MAX_LINES),
    ] {
        for (index, disk) in [
            (oversized.as_str(), "small\n"),
            ("small\n", oversized.as_str()),
        ] {
            let repo = Repo::new("small\n", index, disk);
            test_key_sequences(
                &mut repo.app()?,
                vec![
                    (
                        Some(":review<ret>"),
                        Some(&|app| {
                            assert_error(app, "review limit exceeded");
                            assert!(doc!(app.editor).review.is_none());
                            assert_eq!(doc!(app.editor).text(), disk);
                        }),
                    ),
                    (
                        Some("ix<esc>"),
                        Some(&|app| {
                            assert_eq!(doc!(app.editor).text().char(0), 'x');
                            assert!(doc!(app.editor).is_modified());
                            assert_eq!(repo.disk(), disk);
                        }),
                    ),
                ],
                false,
            )
            .await?;
        }
        let repo = Repo::new("small\n", "small\n", "small\n");
        let mut app = repo.app()?;
        repo.write_external(&oversized);
        test_key_sequence(
            &mut app,
            Some(":review<ret>"),
            Some(&|app| {
                assert_error(app, "review limit exceeded");
                assert!(doc!(app.editor).review.is_none());
                assert_eq!(doc!(app.editor).text(), oversized.as_str());
            }),
            false,
        )
        .await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn review_suspends_visibly_on_large_edits_and_recovers_after_refresh() -> anyhow::Result<()> {
    use helix_view::review::Review;
    for oversized in [
        "x".repeat(Review::MAX_BYTES),
        "\n".repeat(Review::MAX_LINES),
    ] {
        let repo = Repo::new(INDEX, INDEX, DISK);
        // Outside the repository, a pi mailbox in the host workspace would take the write's batch.
        let _cwd = Cwd::enter(repo.dir.path());
        let mut app = repo.app()?;
        let batch = repo.dir.path().join("batch.json");
        let export = format!(":review-export {}<ret>", batch.display());
        break_handoff(&mut app, repo.dir.path()).await?;
        send_keys_and_paste(
            &mut app,
            ":review<ret>:review-comment retained<ret>:review-handoff clear<ret>i",
            &oversized,
        )
        .await?;
        test_key_sequences(
            &mut app,
            vec![
                (
                    Some("z<esc>:w<ret>"),
                    Some(&|app| {
                        assert_status_not_error(&app.editor);
                        assert_eq!(repo.disk().len(), oversized.len() + DISK.len() + 1);
                        assert_eq!(doc!(app.editor).text(), repo.disk().as_str());
                        let (view, doc) = current_ref!(app.editor);
                        assert!(doc.review.as_ref().unwrap().is_suspended());
                        let surface = render_surface(app);
                        let row: String = (view.area.left()..view.area.right())
                            .map(|x| surface[(x, view.area.bottom() - 1)].symbol.as_str())
                            .collect();
                        assert!(row.contains("Review suspended"), "{row}");
                    }),
                ),
                (
                    Some(&export),
                    Some(&|app| {
                        assert_error(app, "review limit exceeded");
                        assert!(!batch.exists());
                        assert_eq!(app.editor.review_batch.comments().len(), 1);
                    }),
                ),
                (
                    Some("2u"),
                    Some(&|app| {
                        assert_eq!(doc!(app.editor).text(), DISK);
                        assert!(doc!(app.editor).review.as_ref().unwrap().is_suspended());
                    }),
                ),
                (
                    Some(":w<ret>"),
                    Some(&|app| {
                        assert_status_not_error(&app.editor);
                        assert_eq!(repo.disk(), DISK);
                        assert!(!doc!(app.editor).is_modified());
                    }),
                ),
                (
                    Some(":review-refresh<ret>"),
                    Some(&|app| {
                        assert_status_not_error(&app.editor);
                        assert!(!doc!(app.editor).review.as_ref().unwrap().is_suspended());
                        assert_eq!(doc!(app.editor).review.as_ref().unwrap().blocks().len(), 2);
                    }),
                ),
                (
                    Some(&export),
                    Some(&|app| {
                        assert_status_not_error(&app.editor);
                        assert!(batch.exists());
                        assert!(app.editor.review_batch.comments().is_empty());
                    }),
                ),
            ],
            false,
        )
        .await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn old_focus_keymaps_and_exit_keep_the_correct_cursor_visible() -> anyhow::Result<()> {
    let index: String = (0..300).map(|i| format!("line {i}\n")).collect();
    let disk: String = (0..300)
        .filter(|i| !(150..250).contains(i))
        .map(|i| match i {
            280 => "new 280\n".to_owned(),
            _ => format!("line {i}\n"),
        })
        .collect();
    let repo = Repo::new(&index, &index, &disk);
    let cursor_line = |app: &Application| {
        let (view, doc) = current_ref!(app.editor);
        doc.selection(view.id)
            .primary()
            .cursor_line(doc.text().slice(..))
    };
    let shows = |app: &Application, line: &str| render(app).iter().any(|(_, text)| text == line);
    // Vertical motions that would leave the block keep the cursor where it is.
    let at_old = |position: &'static str| {
        move |app: &Application| {
            let status = statusline(app);
            assert!(status.contains(&format!(" old {position} ")), "{status}");
        }
    };
    let (first_line, last_line, one_line, back) = (
        at_old("151:1"),
        at_old("250:1"),
        at_old("281:2"),
        at_old("151:1"),
    );
    let mut config = Config::load(
        Ok(&"[keys.normal]\nF9 = ':review-old'\n".to_owned()),
        Err(ConfigLoadError::default()),
    )
    .map_err(|err| anyhow::anyhow!("{err}"))?;
    config.editor = test_editor_config();
    test_key_sequences(
        &mut AppBuilder::new()
            .with_config(config)
            .with_file(&repo.file, None)
            .build()?,
        vec![
            (
                Some(":review<ret><F9>"),
                Some(&|app| {
                    assert!(render(app)
                        .iter()
                        .any(|(marker, text)| marker == "-" && text == "line 150"));
                    assert!(
                        statusline(app).contains(" old 151:1 "),
                        "{}",
                        statusline(app)
                    );
                }),
            ),
            (Some("k"), Some(&first_line)),
            (Some("gej"), Some(&last_line)),
            (Some("]glj3jk"), Some(&one_line)),
            (Some("[gk"), Some(&back)),
            (
                // Esc puts the cursor on the line the removed block hangs under.
                Some("geZ<esc>j"),
                Some(&|app| {
                    assert_eq!(old_focus(app), None);
                    assert_eq!(cursor_line(app), 150);
                    let status = statusline(app);
                    assert!(
                        status.contains(" 151:1 ") && !status.contains(" old "),
                        "{status}"
                    );
                    assert!(shows(app, "line 149"));
                    assert!(shows(app, "line 250"));
                }),
            ),
            (
                // After ]g, Esc puts the cursor on the first line replacing the later block.
                Some("<F9>]g<esc>"),
                Some(&|app| {
                    assert_eq!(old_focus(app), None);
                    assert_eq!(cursor_line(app), 180);
                    assert!(shows(app, "new 280"));
                }),
            ),
            (
                Some("<F9>ge:review-close<ret>"),
                Some(&|app| {
                    assert!(doc!(app.editor).review.is_none());
                    assert_eq!(cursor_line(app), 180);
                    assert!(shows(app, "new 280"));
                    assert_eq!(repo.disk(), disk);
                }),
            ),
        ],
        false,
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn old_blocks_are_centered_with_their_replacement_unless_visible() -> anyhow::Result<()> {
    let index: String = (0..1000).map(|i| format!("line {i}\n")).collect();
    let disk: String = (0..1000)
        .map(|i| match i {
            400 | 800 | 810 => format!("new {i}\n"),
            _ => format!("line {i}\n"),
        })
        .collect();
    let repo = Repo::new(&index, &index, &disk);
    let row_of = |app: &Application, line: &str| {
        render(app)
            .iter()
            .position(|(_, text)| text == line)
            .unwrap_or_else(|| panic!("{line} is not visible"))
    };
    let assert_centered = |app: &Application, i: usize| {
        let height = render(app).len();
        let old = row_of(app, &format!("line {i}"));
        assert_eq!(row_of(app, &format!("new {i}")), old + 1);
        assert!(old.abs_diff((height - 2) / 2) <= 1, "{old} of {height}");
    };
    let top_row = |app: &Application| render(app)[0].1.clone();
    let top = std::cell::RefCell::new(String::new());
    test_key_sequences(
        &mut repo.app()?,
        vec![
            (
                Some(":review<ret>:review-old<ret>"),
                Some(&|app| assert_centered(app, 400)),
            ),
            (
                Some("]g"),
                Some(&|app| {
                    assert_centered(app, 800);
                    *top.borrow_mut() = top_row(app);
                }),
            ),
            (
                // The next block is already visible with its replacement, so nothing scrolls.
                Some("]g"),
                Some(&|app| {
                    assert_eq!(top_row(app), *top.borrow());
                    assert!(row_of(app, "line 810") + 1 == row_of(app, "new 810"));
                }),
            ),
            (
                Some("<esc>"),
                Some(&|app| {
                    assert_eq!(old_focus(app), None);
                    assert_eq!(top_row(app), *top.borrow());
                    let (view, doc) = current_ref!(app.editor);
                    let text = doc.text().slice(..);
                    assert_eq!(doc.selection(view.id).primary().cursor_line(text), 810);
                }),
            ),
            (
                Some(":review-old<ret>[g[g"),
                Some(&|app| assert_centered(app, 400)),
            ),
        ],
        false,
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn review_compares_against_the_index() -> anyhow::Result<()> {
    // Only the unstaged change of the last line is under review.
    let repo = Repo::new(
        "one\ntwo\nthree\nfour\n",
        "one\nTWO\nthree\nfour\n",
        "one\nTWO\nthree\nFOUR\n",
    );
    test_key_sequence(
        &mut repo.app()?,
        Some(":review<ret>"),
        Some(&|app| {
            assert_status_not_error(&app.editor);
            let review = doc!(app.editor).review.as_ref().unwrap();
            assert_eq!(review.snapshot(), "one\nTWO\nthree\nfour\n");
            assert_eq!(
                review.baseline(),
                &ReviewBaseline::GitIndex {
                    blob_oid: repo.index_oid()
                }
            );
            assert_eq!(
                review.blocks(),
                [OldBlock {
                    old: 3..4,
                    current: 3..4,
                    anchor_line: 2
                }]
            );
            assert_eq!(review.changed_lines().collect::<Vec<_>>(), [3..4]);
            let rows = render(app);
            assert_eq!(rows[3], ("-".to_owned(), "four".to_owned()));
            assert_eq!(rows[4].1, "FOUR");
        }),
        false,
    )
    .await?;

    // A file whose changes are all staged has nothing to review.
    let repo = Repo::new("a\n", "b\n", "b\n");
    test_key_sequence(
        &mut repo.app()?,
        Some(":review<ret>"),
        Some(&|app| {
            let review = doc!(app.editor).review.as_ref().unwrap();
            assert_eq!(review.snapshot(), "b\n");
            assert!(review.blocks().is_empty());
            assert_eq!(review.changed_lines().count(), 0);
        }),
        false,
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn review_refuses_untracked_and_unmerged_files() -> anyhow::Result<()> {
    let repo = Repo::new("base\n", "base\n", "base\n");
    let untracked = repo.dir.path().join("untracked.txt");
    fs::write(&untracked, "new\n")?;
    test_key_sequence(
        &mut AppBuilder::new().with_file(&untracked, None).build()?,
        Some(":review<ret>"),
        Some(&|app| {
            assert_error(app, "not in the git index");
            assert!(doc!(app.editor).review.is_none());
        }),
        false,
    )
    .await?;

    let dir = repo.dir.path();
    git_ok(dir, &["checkout", "-b", "other"]);
    repo.write("other\n");
    git_ok(dir, &["commit", "-am", "other"]);
    git_ok(dir, &["checkout", "main"]);
    repo.write("main\n");
    git_ok(dir, &["commit", "-am", "main"]);
    assert!(!git(dir, &["merge", "other"]).status.success());
    test_key_sequence(
        &mut repo.app()?,
        Some(":review<ret>"),
        Some(&|app| {
            assert_error(app, "unmerged");
            assert!(doc!(app.editor).review.is_none());
        }),
        false,
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn old_lines_render_inline_at_file_boundaries() -> anyhow::Result<()> {
    let repo = Repo::new(INDEX, INDEX, DISK);
    test_key_sequences(
        &mut repo.app()?,
        vec![
            (
                Some(":review<ret>"),
                Some(&|app| {
                    let rows = render(app);
                    let text: Vec<_> = rows.iter().map(|(_, text)| text.as_str()).collect();
                    assert_eq!(
                        text[..9],
                        [
                            "keep1",
                            "old lines 1-2 removed before line 1:",
                            "gone1",
                            "gone2",
                            "keep2",
                            "tail1",
                            "tail2",
                            "",
                            "",
                        ]
                    );
                    for row in [2, 3, 5, 6] {
                        assert_eq!(rows[row].0, "-");
                    }
                }),
            ),
            (
                Some(":review-old<ret>"),
                Some(&|app| {
                    assert_eq!(old_focus(app), Some(Range::new(0, 1)));
                }),
            ),
            (
                Some("]g"),
                Some(&|app| {
                    // tail1 starts at char 24 of the index snapshot.
                    assert_eq!(old_focus(app), Some(Range::new(24, 25)));
                }),
            ),
            (
                Some("xx"),
                Some(&|app| {
                    // Selections stay inside the old block.
                    assert_eq!(old_focus(app), Some(Range::new(24, 36)));
                }),
            ),
        ],
        false,
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn old_lines_exclude_the_phantom_trailing_line_and_clip_wide_graphemes() -> anyhow::Result<()>
{
    for disk in ["", "current"] {
        let repo = Repo::new("old\n", "old\n", disk);
        test_key_sequence(
            &mut repo.app()?,
            Some(":review<ret>"),
            Some(&|app| {
                let rows = render(app);
                let old: Vec<_> = rows.iter().filter(|(marker, _)| marker == "-").collect();
                assert_eq!(old.len(), 1);
                assert_eq!(old[0].1, "old");
                assert!(!rows.iter().any(|(_, text)| text.contains("old lines 1-2")));
            }),
            false,
        )
        .await?;
    }

    let repo = Repo::new("old\n", "old\n", "current\n");
    let mut app = repo.app()?;
    let (view, doc) = current_ref!(app.editor);
    let width = view.inner_width(doc);
    let index = format!("{}界\n", "x".repeat(width as usize - 1));
    repo.write(&index);
    repo.stage();
    repo.write("current\n");
    test_key_sequence(
        &mut app,
        Some(":review<ret>"),
        Some(&|app| {
            let (view, doc) = current_ref!(app.editor);
            let mut surface =
                Buffer::empty(Rect::new(0, 0, view.area.right() + 2, view.area.bottom()));
            EditorView::new(Keymaps::default()).render_view(
                &app.editor,
                doc,
                view,
                view.area,
                &mut surface,
                true,
            );
            let inner = view.inner_area(doc);
            let row = inner.top() + 2;
            assert_eq!(surface[(inner.right() - 2, row)].symbol.as_str(), "x");
            for x in inner.right() - 1..view.area.right() + 2 {
                assert_eq!(surface[(x, row)].symbol.as_str(), " ");
            }
        }),
        false,
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn old_lines_follow_scrolling_and_soft_wrap() -> anyhow::Result<()> {
    // Lines removed at the start, in the middle and at the end of a file taller than the view.
    let index: String = (0..300).map(|i| format!("line {i}\n")).collect();
    let disk: String = (0..300)
        .filter(|i| ![0, 150, 299].contains(i))
        .map(|i| format!("line {i}\n"))
        .collect();
    let repo = Repo::new(&index, &index, &disk);
    let old_row = |rows: &[(String, String)], text: &str| {
        rows.iter()
            .position(|(marker, row)| marker == "-" && row == text)
    };
    test_key_sequences(
        &mut repo.app()?,
        vec![
            (
                Some(":review<ret>:review-old<ret>"),
                Some(&|app| {
                    let rows = render(app);
                    assert_eq!(rows[0].1, "line 1");
                    assert_eq!(rows[1].1, "old line 1 removed before line 1:");
                    assert_eq!(old_row(&rows, "line 0"), Some(2));
                    assert_eq!(rows[3].1, "line 2");
                    assert_eq!(old_row(&rows, "line 150"), None);
                }),
            ),
            (
                Some("]g"),
                Some(&|app| {
                    let rows = render(app);
                    let row = old_row(&rows, "line 150").expect("scrolled to the middle block");
                    assert_eq!(rows[row - 1].1, "line 149");
                    assert_eq!(rows[row + 1].1, "line 151");
                    assert_eq!(old_row(&rows, "line 0"), None);
                }),
            ),
            (
                Some("]G"),
                Some(&|app| {
                    let rows = render(app);
                    let row = old_row(&rows, "line 299").expect("scrolled to the last block");
                    assert_eq!(rows[row - 1].1, "line 298");
                    assert!(rows[row + 1..].iter().all(|(_, row)| row.is_empty()));
                    assert_eq!(old_row(&rows, "line 150"), None);
                }),
            ),
            (
                Some("[G"),
                Some(&|app| {
                    let rows = render(app);
                    assert_eq!(rows[0].1, "line 1");
                    assert_eq!(old_row(&rows, "line 0"), Some(2));
                }),
            ),
        ],
        false,
    )
    .await?;

    let long = "x".repeat(300);
    let repo = Repo::new(
        &format!("short\n{long}\nend\n"),
        &format!("short\n{long}\nend\n"),
        "short\nend\n",
    );
    let mut config = test_config();
    config.editor.soft_wrap = SoftWrap {
        enable: Some(true),
        ..Default::default()
    };
    test_key_sequence(
        &mut AppBuilder::new()
            .with_config(config)
            .with_file(&repo.file, None)
            .build()?,
        Some(":review<ret>"),
        Some(&|app| {
            let rows = render(app);
            assert_eq!(rows[0].1, "short");
            let end = rows.iter().position(|(_, row)| row == "end").unwrap();
            let old = &rows[1..end];
            assert!(old.len() > 1, "the long line wraps");
            assert_eq!(old[0].0, "-");
            let shown: usize = old.iter().map(|(_, row)| row.matches('x').count()).sum();
            assert_eq!(shown, 300);
        }),
        false,
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn old_text_is_read_only_but_yankable() -> anyhow::Result<()> {
    let repo = Repo::new(INDEX, INDEX, DISK);
    let mut app = repo.app()?;
    let initial_selection = {
        let (view, doc) = current_ref!(app.editor);
        doc.selection(view.id).clone()
    };
    let unchanged = |app: &Application| {
        let doc = doc!(app.editor);
        assert_eq!(doc.text(), DISK);
        assert!(!doc.is_modified());
        assert_eq!(doc.review.as_ref().unwrap().snapshot(), INDEX);
    };
    test_key_sequences(
        &mut app,
        vec![
            (
                Some(":review<ret>:review-old<ret>xy"),
                Some(&|app| {
                    assert_eq!(old_focus(app), Some(Range::new(0, 6)));
                    unchanged(app);
                }),
            ),
            (
                Some("d"),
                Some(&|app| {
                    assert_error(app, "'delete_selection' is not available");
                    unchanged(app);
                }),
            ),
            (
                Some("cfoo<C-c>ifoo<C-c>afoo<C-c>pP~<gt>J.Ru"),
                Some(&|app| {
                    assert!(old_focus(app).is_some());
                    unchanged(app);
                }),
            ),
            (
                Some(":sort<ret>"),
                Some(&|app| {
                    assert_error(app, "'sort' is not available");
                    assert!(old_focus(app).is_some());
                    unchanged(app);
                }),
            ),
            (
                Some("<esc>"),
                Some(&|app| {
                    assert_eq!(old_focus(app), None);
                    let (view, doc) = current_ref!(app.editor);
                    assert_eq!(doc.selection(view.id), &initial_selection);
                    unchanged(app);
                }),
            ),
            (
                // The yanked old text pastes into the current text like any other yank.
                Some("p"),
                Some(&|app| {
                    assert_eq!(doc!(app.editor).text(), "keep1\ngone1\nkeep2\n");
                }),
            ),
        ],
        false,
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn mapped_commands_and_pastes_cannot_edit_old_text() -> anyhow::Result<()> {
    let repo = Repo::new(INDEX, INDEX, DISK);
    let keys = r#"
        [keys.normal]
        F5 = ":sort"
        F6 = ["collapse_selection", "delete_selection"]
        F7 = "@d"
        F8 = ":review-comment mapped"
    "#
    .to_owned();
    let mut config = Config::load(Ok(&keys), Err(ConfigLoadError::default()))
        .map_err(|err| anyhow::anyhow!("{err}"))?;
    config.editor = test_editor_config();
    let mut app = AppBuilder::new()
        .with_config(config)
        .with_file(&repo.file, None)
        .build()?;
    let unchanged = |app: &Application| {
        let doc = doc!(app.editor);
        assert_eq!(doc.text(), DISK);
        assert!(!doc.is_modified());
        assert!(old_focus(app).is_some());
    };

    let outbox = tempfile::tempdir()?;
    let keys = format!(
        ":review-handoff {}<ret>:review<ret>:review-old<ret>x",
        outbox.path().display()
    );
    send_keys_and_paste(&mut app, &keys, "pasted").await?;
    assert_error(&app, "read-only");
    unchanged(&app);

    test_key_sequences(
        &mut app,
        vec![
            (
                Some("<F5>"),
                Some(&|app| {
                    assert_error(app, "'sort' is not available");
                    unchanged(app);
                }),
            ),
            (
                // The allowed first command of the sequence runs, the edit does not.
                Some("<F6>"),
                Some(&|app| {
                    assert_error(app, "'delete_selection' is not available");
                    assert_eq!(old_focus(app), Some(Range::new(5, 6)));
                    unchanged(app);
                }),
            ),
            (
                Some("<F7>"),
                Some(&|app| {
                    assert_error(app, "is not available");
                    unchanged(app);
                }),
            ),
            (
                Some("<F8>"),
                Some(&|app| {
                    assert_eq!(
                        app.editor.get_status().unwrap().0,
                        "Sent 1 comment to the agent"
                    );
                    let batches = take_batches(outbox.path());
                    assert_eq!(batches[0]["comments"][0]["comment"], "mapped");
                    unchanged(app);
                }),
            ),
        ],
        false,
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn edits_and_saves_only_contain_current_text() -> anyhow::Result<()> {
    let repo = Repo::new(INDEX, INDEX, DISK);
    test_key_sequences(
        &mut repo.app()?,
        vec![
            (
                Some(":review<ret>iNEW <esc>:w<ret>"),
                Some(&|app| {
                    assert_eq!(repo.disk(), "NEW keep1\nkeep2\n");
                    let review = doc!(app.editor).review.as_ref().unwrap();
                    assert_eq!(review.changed_lines().collect::<Vec<_>>(), [0..1]);
                    let rows = render(app);
                    assert_eq!(rows[0].1, "NEW keep1");
                    assert_eq!(rows[2].1, "gone1");
                }),
            ),
            (
                Some("u:w<ret>"),
                Some(&|app| {
                    assert_eq!(repo.disk(), DISK);
                    let review = doc!(app.editor).review.as_ref().unwrap();
                    assert_eq!(review.changed_lines().count(), 0);
                    assert_eq!(review.blocks().len(), 2);
                }),
            ),
        ],
        false,
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn refresh_refuses_unsaved_changes_and_reloads_disk_and_index() -> anyhow::Result<()> {
    let repo = Repo::new(INDEX, INDEX, DISK);
    test_key_sequences(
        &mut repo.app()?,
        vec![
            (
                Some(":review<ret>ix<esc>:review-refresh<ret>"),
                Some(&|app| {
                    assert_error(app, "unsaved changes");
                    let doc = doc!(app.editor);
                    assert_eq!(doc.text(), "xkeep1\nkeep2\n");
                    assert!(doc.is_modified());
                    assert_eq!(doc.review.as_ref().unwrap().snapshot(), INDEX);
                    assert_eq!(repo.disk(), DISK);

                    // Stage some work and change the file again outside of helix.
                    repo.write("keep1\nkeep2\nnew\n");
                    repo.stage();
                    repo.write("keep1\nkeep2\nnew\nnewer\n");
                }),
            ),
            (
                Some("u:review-refresh<ret>"),
                Some(&|app| {
                    assert_status_not_error(&app.editor);
                    let doc = doc!(app.editor);
                    assert_eq!(doc.text(), "keep1\nkeep2\nnew\nnewer\n");
                    assert!(!doc.is_modified());
                    let review = doc.review.as_ref().unwrap();
                    assert_eq!(review.snapshot(), "keep1\nkeep2\nnew\n");
                    assert_eq!(
                        review.baseline(),
                        &ReviewBaseline::GitIndex {
                            blob_oid: repo.index_oid()
                        }
                    );
                    assert!(review.blocks().is_empty());
                    assert_eq!(review.changed_lines().collect::<Vec<_>>(), [3..4]);
                }),
            ),
        ],
        false,
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn commit_review_pins_history_and_preserves_edits_and_comment_baselines() -> anyhow::Result<()>
{
    let repo = Repo::new("committed\nkeep\n", "staged\nkeep\n", "latest\nkeep\n");
    let commit_oid = git_ok(repo.dir.path(), &["rev-parse", "HEAD"]);
    let blob_oid = git_ok(repo.dir.path(), &["rev-parse", "HEAD:file.txt"]);
    let index_oid = repo.index_oid();
    let baseline = ReviewBaseline::GitCommit {
        commit_oid: commit_oid.clone(),
        blob_oid: Some(blob_oid.clone()),
    };
    let batch = repo.dir.path().join("commits.json");
    let export = format!(":review-export {}<ret>", batch.display());
    let outbox = tempfile::tempdir()?;
    let handoff = format!(
        ":review-handoff {}<ret>:review HEAD<ret>:review-old<ret>x:review-comment historical<ret><esc>",
        outbox.path().display()
    );
    test_key_sequences(
        &mut repo.app()?,
        vec![
            (Some(&handoff), Some(&|app| {
                assert_status_not_error(&app.editor);
                assert_eq!(doc!(app.editor).text(), "latest\nkeep\n");
                assert!(render(app).iter().any(|(_, text)| text == "committed"));
                assert_eq!(doc!(app.editor).review.as_ref().unwrap().baseline(), &baseline);
                let json = &take_batches(outbox.path())[0];
                assert_eq!(json["comments"][0]["baseline"], serde_json::json!({"kind":"git-commit", "commit_oid":commit_oid, "blob_oid":blob_oid}));
                assert_eq!(json["comments"][0]["selected_text"], "committed\n");
            })),
            (Some("ix<esc>:review missing-baseline-ref<ret>"), Some(&|app| {
                assert_error(app, "missing-baseline-ref");
                assert_eq!(doc!(app.editor).text(), "xlatest\nkeep\n");
                assert!(doc!(app.editor).is_modified());
                assert_eq!(doc!(app.editor).review.as_ref().unwrap().baseline(), &baseline);
            })),
            (Some("u:review<ret>:review-old<ret>x:review-comment staged<ret><esc>"), Some(&|app| {
                assert_status_not_error(&app.editor);
                assert!(render(app).iter().any(|(_, text)| text == "staged"));
                let json = &take_batches(outbox.path())[0];
                assert_eq!(json["comments"][0]["baseline"], serde_json::json!({"kind":"git-index", "blob_oid":index_oid}));
                assert_eq!(json["comments"][0]["selected_text"], "staged\n");
            })),
            (Some(":review HEAD<ret>"), Some(&|app| {
                assert_status_not_error(&app.editor);
                assert_eq!(doc!(app.editor).review.as_ref().unwrap().baseline(), &baseline);
                repo.write("new commit\nkeep\n");
                repo.stage();
                git_ok(repo.dir.path(), &["commit", "-m", "advance"]);
                repo.write_external("agent latest\nkeep\n");
            })),
            (Some(":review-refresh<ret>"), Some(&|app| {
                assert_status_not_error(&app.editor);
                assert_eq!(doc!(app.editor).text(), "agent latest\nkeep\n");
                assert_eq!(doc!(app.editor).review.as_ref().unwrap().baseline(), &baseline);
                assert!(render(app).iter().any(|(_, text)| text == "committed"));
                assert_ne!(git_ok(repo.dir.path(), &["rev-parse", "HEAD"]), commit_oid);
            })),
            (Some(&export), Some(&|app| {
                assert_status_not_error(&app.editor);
                let json: serde_json::Value = serde_json::from_str(&fs::read_to_string(&batch).unwrap()).unwrap();
                assert_eq!(json["version"], 2);
                assert_eq!(json["files"][0]["baseline"], serde_json::json!({"kind":"git-commit", "commit_oid":commit_oid, "blob_oid":blob_oid}));
                assert_eq!(repo.disk(), "agent latest\nkeep\n");
            })),
        ], false,
    ).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn commit_picker_selects_history_and_cancellation_preserves_edits() -> anyhow::Result<()> {
    let repo = Repo::new("first\n", "second\n", "second\n");
    git_ok(repo.dir.path(), &["commit", "-m", "second"]);
    let commit_oid = git_ok(repo.dir.path(), &["rev-parse", "HEAD~1"]);
    test_key_sequences(
        &mut repo.app()?,
        vec![
            (Some(":review-commits<ret>"), None),
            (
                Some("head<ret>"),
                Some(&|app| {
                    assert_status_not_error(&app.editor);
                    assert_eq!(doc!(app.editor).text(), "second\n");
                    assert_eq!(
                        doc!(app.editor)
                            .review
                            .as_ref()
                            .unwrap()
                            .baseline()
                            .revision(),
                        Some(commit_oid.as_str())
                    );
                    assert!(render(app).iter().any(|(_, text)| text == "first"));
                }),
            ),
            (Some("ix<esc>:review-commits<ret>"), None),
            (
                Some("<esc>"),
                Some(&|app| {
                    assert_status_not_error(&app.editor);
                    assert_eq!(doc!(app.editor).text(), "xsecond\n");
                    assert!(doc!(app.editor).is_modified());
                    assert_eq!(
                        doc!(app.editor)
                            .review
                            .as_ref()
                            .unwrap()
                            .baseline()
                            .revision(),
                        Some(commit_oid.as_str())
                    );
                    assert_eq!(repo.disk(), "second\n");
                }),
            ),
        ],
        false,
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn comment_picker_jumps_to_pending_comments() -> anyhow::Result<()> {
    let repo = Repo::new("a\nb\nc\nd\ne\n", "a\nb\nc\nd\ne\n", "a\nb\nC\nd\ne\n");
    let dir = repo.dir.path();
    fs::write(dir.join("other.txt"), "other\n")?;
    git_ok(dir, &["add", "other.txt"]);
    let _cwd = Cwd::enter(dir);
    let at = |name: &'static str, line: usize| {
        move |app: &Application| {
            if let Some((status, severity)) = app.editor.get_status() {
                assert_ne!(*severity, Severity::Error, "{status}");
            }
            assert_eq!(old_focus(app), None);
            let (view, doc) = current_ref!(app.editor);
            assert_eq!(doc.path().unwrap().file_name().unwrap(), name);
            let text = doc.text().slice(..);
            assert_eq!(doc.selection(view.id).primary().cursor_line(text), line);
        }
    };
    let (on_d, on_old_c) = (at("file.txt", 3), at("file.txt", 2));
    let mut app = repo.app()?;
    // Comments that cannot be sent stay pending.
    break_handoff(&mut app, dir).await?;

    test_key_sequences(
        &mut app,
        vec![
            (
                Some(":review-comments<ret>"),
                Some(&|app| assert_error(app, "no pending review comments")),
            ),
            (
                Some(concat!(
                    ":review<ret>3j:review-comment on d<ret>",
                    ":review-old<ret>:review-comment old c<ret><esc>",
                    ":o other.txt<ret>:review<ret>:review-comment elsewhere<ret>",
                    ":review-comments<ret>",
                )),
                None,
            ),
            (Some("on d<ret>"), Some(&on_d)),
            // From old-text focus, the picker leaves it and jumps to the adjacent current line.
            (Some("gg:review-old<ret>:review-comments<ret>"), None),
            (Some("old c<ret>"), Some(&on_old_c)),
        ],
        false,
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn uncomment_drops_the_latest_pending_comments() -> anyhow::Result<()> {
    let repo = Repo::new(INDEX, INDEX, DISK);
    let _cwd = Cwd::enter(repo.dir.path());
    let comments = |app: &Application| -> Vec<String> {
        app.editor
            .review_batch
            .comments()
            .iter()
            .map(|comment| comment.comment.clone())
            .collect()
    };
    let status = |app: &Application| app.editor.get_status().unwrap().0.to_string();
    let head = git_ok(repo.dir.path(), &["rev-parse", "HEAD"]);
    let mut config = test_config();
    config.editor.statusline.right = vec![StatusLineElement::Review];
    let review_element = |app: &Application| {
        let line = statusline(app);
        line.split_once(" review ")
            .map(|(_, element)| element.trim_end().to_owned())
    };

    let mut app = AppBuilder::new()
        .with_config(config)
        .with_file(&repo.file, None)
        .build()?;
    // Comments that cannot be sent stay pending.
    break_handoff(&mut app, repo.dir.path()).await?;
    test_key_sequences(
        &mut app,
        vec![
            (
                // Review commands enable review against HEAD on their own.
                Some(":review-uncomment<ret>"),
                Some(&|app| {
                    assert_error(app, "no pending review comments");
                    assert_eq!(review_element(app), Some(format!("{} 2 hunks", &head[..8])));
                }),
            ),
            (
                Some(concat!(
                    ":review index<ret>:review-comment one<ret>:review-comment two<ret>",
                    ":review-old<ret>:review-comment three<ret>:review-comment four<ret>",
                    ":review-uncomment<ret>",
                )),
                Some(&|app| {
                    assert_eq!(comments(app), ["one", "two", "three"]);
                    assert_eq!(status(app), "Dropped 1 review comment, 3 pending");
                    assert_eq!(
                        review_element(app).as_deref(),
                        Some("index 2 hunks 3 comments")
                    );
                }),
            ),
            (
                Some("<esc>:review-uncomment 2<ret>"),
                Some(&|app| {
                    assert_eq!(comments(app), ["one"]);
                    assert_eq!(status(app), "Dropped 2 review comments, 1 pending");
                    assert_eq!(
                        review_element(app).as_deref(),
                        Some("index 2 hunks 1 comment")
                    );
                }),
            ),
            (
                Some(":review-uncomment 0<ret>"),
                Some(&|app| {
                    assert_error(app, "positive number");
                    assert_eq!(comments(app), ["one"]);
                }),
            ),
            (
                Some(":review-uncomment 5<ret>:review-uncomment<ret>"),
                Some(&|app| {
                    assert!(comments(app).is_empty());
                    assert_error(app, "no pending review comments");
                }),
            ),
            (
                Some(":review HEAD<ret>"),
                Some(&|app| {
                    assert_eq!(review_element(app), Some(format!("{} 2 hunks", &head[..8])));
                }),
            ),
        ],
        false,
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn baseline_names_unsaved_buffers_whose_file_also_changed_on_disk() -> anyhow::Result<()> {
    let repo = Repo::new("a\n", "a\n", "a\n");
    let dir = repo.dir.path();
    fs::write(dir.join("other.txt"), "o\n")?;
    git_ok(dir, &["add", "other.txt"]);
    git_ok(dir, &["commit", "-m", "other"]);
    let commit = git_ok(dir, &["rev-parse", "HEAD"]);
    let _cwd = Cwd::enter(dir);
    let mut app = repo.app()?;

    // Both buffers have unsaved edits; only file.txt is also changed on disk by the agent.
    send_keys(
        &mut app,
        ":review-baseline HEAD<ret>ix<esc>:o other.txt<ret>iy<esc>",
    )
    .await?;
    repo.write_external("agent\n");
    send_keys(&mut app, ":review-baseline HEAD<ret>").await?;

    assert_eq!(
        app.editor.get_status().unwrap().0.as_ref(),
        format!(
            "Reviewing 0 buffers against {}; 1 unsaved skipped; file.txt also changed on disk: :reload discards your edits, :w! overwrites the agent's",
            &commit[..8]
        )
    );
    let file = app.editor.document_by_path(&repo.file).unwrap();
    assert_eq!(file.text(), "xa\n");
    assert_eq!(repo.disk(), "agent\n");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn closing_a_review_survives_refreshes_of_the_same_baseline() -> anyhow::Result<()> {
    let repo = Repo::new("a\n", "a\n", "a\n");
    repo.write("b\n");
    repo.stage();
    git_ok(repo.dir.path(), &["commit", "-m", "b"]);
    repo.write("c\n");
    let head = git_ok(repo.dir.path(), &["rev-parse", "HEAD"]);
    let parent = git_ok(repo.dir.path(), &["rev-parse", "HEAD~1"]);
    let _cwd = Cwd::enter(repo.dir.path());
    let mut app = repo.app()?;
    let status = |app: &Application| app.editor.get_status().unwrap().0.to_string();
    let revision = |app: &Application| {
        let review = doc!(app.editor).review.as_ref()?;
        Some(review.baseline().revision().map(str::to_owned))
    };

    send_keys(&mut app, ":review-baseline HEAD<ret>:review-close<ret>").await?;
    assert_eq!(
        status(&app),
        format!("Review closed; :review {} resumes it", &head[..8])
    );

    // The hook's refresh and a reload leave the closed buffer alone.
    send_keys(&mut app, ":review-baseline HEAD<ret>").await?;
    assert_eq!(revision(&app), None);
    assert_eq!(
        status(&app),
        format!("Reviewing 0 buffers against {}; 1 closed", &head[..8])
    );
    send_keys(&mut app, ":reload<ret>").await?;
    assert_eq!(revision(&app), None);

    // A different baseline reviews it again.
    send_keys(&mut app, ":review-baseline HEAD~1<ret>").await?;
    assert_eq!(revision(&app), Some(Some(parent.clone())));

    // Plain :review resumes the editor-wide baseline rather than the index.
    send_keys(&mut app, ":review-close<ret>:review<ret>").await?;
    assert_eq!(revision(&app), Some(Some(parent.clone())));
    assert_eq!(doc!(app.editor).review_opt_out, None);

    // :review index compares against the index, and the same baseline then keeps it.
    send_keys(
        &mut app,
        ":review-close<ret>:review index<ret>:review-baseline HEAD~1<ret>",
    )
    .await?;
    assert_eq!(revision(&app), Some(None));
    assert_eq!(doc!(app.editor).review_opt_out, None);

    // Without an editor-wide baseline, plain :review uses the index.
    send_keys(&mut app, ":review-baseline clear<ret>:review<ret>").await?;
    assert_eq!(revision(&app), Some(None));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn revert_restores_baseline_lines_as_one_undoable_edit() -> anyhow::Result<()> {
    let disk = "a\nB\nc\ne\nX\nY\n";
    let repo = Repo::new("a\nb\nc\nd\ne\n", "a\nb\nc\nd\ne\n", disk);
    let state = |app: &Application| {
        let (view, doc) = current_ref!(app.editor);
        let text = doc.text().slice(..);
        (
            doc.text().to_string(),
            doc.selection(view.id).primary().cursor_line(text),
            app.editor.get_status().unwrap().0.to_string(),
        )
    };
    let reverted = |lines: &str| format!("Reverted {lines} to the baseline");

    test_key_sequences(
        &mut repo.app()?,
        vec![
            (
                Some(":review<ret>:review-revert<ret>"),
                Some(&|app| {
                    assert_error(app, "no changed lines here");
                    assert_eq!(doc!(app.editor).text(), disk);
                }),
            ),
            (
                Some("j:review-revert<ret>"),
                Some(&|app| {
                    let text = "a\nb\nc\ne\nX\nY\n".to_owned();
                    assert_eq!(state(app), (text, 1, reverted("1 line")));
                    let review = doc!(app.editor).review.as_ref().unwrap();
                    assert_eq!(review.changed_lines().collect::<Vec<_>>(), [4..6]);
                }),
            ),
            (
                Some("u"),
                Some(&|app| assert_eq!(doc!(app.editor).text(), disk)),
            ),
            (
                // A pure addition is reverted by deleting the added lines.
                Some("gg4j:review-revert<ret>"),
                Some(&|app| {
                    assert_eq!(doc!(app.editor).text(), "a\nB\nc\ne\n");
                    assert_eq!(state(app).2, reverted("2 lines"));
                }),
            ),
            (
                // From old-text focus, the focused removal is restored and focus is left.
                Some("gg:review-old<ret>]g:review-revert<ret>"),
                Some(&|app| {
                    assert_eq!(old_focus(app), None);
                    let text = "a\nB\nc\nd\ne\n".to_owned();
                    assert_eq!(state(app), (text, 3, reverted("1 line")));
                    assert!(doc!(app.editor).is_modified());
                    assert_eq!(repo.disk(), disk);
                }),
            ),
            (
                // In normal mode, a removal is reverted from the line its old lines follow.
                Some("ugg2j:review-revert<ret>"),
                Some(&|app| {
                    let text = "a\nB\nc\nd\ne\n".to_owned();
                    assert_eq!(state(app), (text, 3, reverted("1 line")));
                }),
            ),
        ],
        false,
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn review_files_reports_when_nothing_changed() -> anyhow::Result<()> {
    let repo = Repo::new("same\n", "same\n", "same\n");
    let commit = git_ok(repo.dir.path(), &["rev-parse", "HEAD"]);
    let _cwd = Cwd::enter(repo.dir.path());
    let mut app = repo.app()?;

    // No picker opens, so the following keys edit the buffer.
    send_keys(&mut app, ":review-baseline HEAD<ret>:review-files<ret>").await?;
    let (status, severity) = app.editor.get_status().unwrap();
    assert_eq!(
        status.as_ref(),
        format!("no files changed since {}", &commit[..8])
    );
    assert_ne!(*severity, Severity::Error);
    send_keys(&mut app, "ix<esc>").await?;
    assert_eq!(doc!(app.editor).text(), "xsame\n");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn review_diff_shows_every_change_in_one_scratch_buffer() -> anyhow::Result<()> {
    let repo = Repo::new("a\nb\nc\n", "a\nb\nc\n", "a\nB\nc\n");
    let dir = repo.dir.path();
    let new = dir.join("new.txt");
    fs::write(&new, "n\n")?;
    let _cwd = Cwd::enter(dir);
    let mut app = repo.app()?;
    let diff_buffer = |app: &Application| {
        let doc = doc!(app.editor);
        assert_eq!(doc.path(), None);
        assert_eq!(doc.language_name(), Some("diff"));
        assert!(doc.readonly && !doc.is_modified());
        (doc.id(), doc.text().to_string())
    };

    // Without a baseline, HEAD is pinned as the editor-wide one.
    send_keys(&mut app, ":review-diff<ret>").await?;
    let head = git_ok(dir, &["rev-parse", "HEAD"]);
    assert_eq!(app.editor.review_baseline.as_deref(), Some(head.as_str()));
    let (first, text) = diff_buffer(&app);
    assert!(
        text.contains("--- file.txt\n+++ file.txt\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n"),
        "{text}"
    );
    assert!(text.contains("--- new.txt\n+++ new.txt\n"), "{text}");
    assert!(text.contains("\n+n\n"), "{text}");
    let documents = app.editor.documents().count();

    // Running it again refreshes the same scratch buffer.
    fs::write(&new, "m\n")?;
    send_keys(&mut app, ":review-diff<ret>").await?;
    let (second, text) = diff_buffer(&app);
    assert_eq!(second, first);
    assert_eq!(app.editor.documents().count(), documents);
    assert!(
        text.contains("\n+m\n") && !text.contains("\n+n\n"),
        "{text}"
    );

    // The hook's refresh regenerates the open diff buffer without switching to it.
    send_keys(&mut app, ":o file.txt<ret>").await?;
    fs::write(&new, "agent\n")?;
    send_keys(&mut app, ":review-baseline HEAD<ret>").await?;
    assert_eq!(
        doc!(app.editor).path().unwrap().file_name().unwrap(),
        "file.txt"
    );
    let refreshed = app.editor.documents[&first].text().to_string();
    assert!(refreshed.contains("\n+agent\n"), "{refreshed}");
    send_keys(&mut app, ":review-diff<ret>").await?;

    // Opening from the diff: the "+B" line is current line 2 of file.txt.
    let opened = |app: &Application| {
        let (view, doc) = current_ref!(app.editor);
        let name = doc.path().unwrap().file_name().unwrap().to_owned();
        let text = doc.text().slice(..);
        (name, doc.selection(view.id).primary().cursor_line(text))
    };
    send_keys(&mut app, "gg5j:review-diff-open<ret>").await?;
    assert_eq!(opened(&app), ("file.txt".into(), 1));
    send_keys(&mut app, ":review-diff-open<ret>").await?;
    assert_error(&app, "not the :review-diff buffer");
    // A file header opens the file at its first line.
    send_keys(&mut app, ":review-diff<ret>gg:review-diff-open<ret>").await?;
    assert_eq!(opened(&app), ("file.txt".into(), 0));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn n_walks_review_hunks_until_something_is_searched() -> anyhow::Result<()> {
    let index = "a\nb\nc\nd\ne\nf\ng\n";
    let repo = Repo::new(index, index, "a\nB\nc\nd\ne\nF\ng\n");
    let mut app = repo.app()?;
    let cursor_line = |app: &Application| {
        let (view, doc) = current_ref!(app.editor);
        let text = doc.text().slice(..);
        doc.selection(view.id).primary().cursor_line(text)
    };
    // The differ runs in the background; waits until it reports both hunks.
    let wait_for_hunks = |app: &Application| {
        let doc = doc!(app.editor);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while doc.diff_handle().map_or(0, |diff| diff.load().len()) != 2 {
            assert!(std::time::Instant::now() < deadline, "expected 2 hunks");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    };

    send_keys(&mut app, ":review<ret>:clear-register /<ret>").await?;
    wait_for_hunks(&app);
    send_keys(&mut app, "n").await?;
    assert_eq!(cursor_line(&app), 1);
    send_keys(&mut app, "n").await?;
    assert_eq!(cursor_line(&app), 5);
    send_keys(&mut app, "N").await?;
    assert_eq!(cursor_line(&app), 1);

    // Once something is searched, n repeats the search.
    send_keys(&mut app, "gg/c<ret>ggn").await?;
    assert_eq!(cursor_line(&app), 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn stage_and_unstage_single_hunks_in_the_git_index() -> anyhow::Result<()> {
    // Twenty lines, changed far apart so each change is its own hunk in every diff.
    let index: String = ('a'..='t').map(|c| format!("{c}\n")).collect();
    let worktree = index.replace("b\n", "B\n").replace("s\n", "S\n");
    let repo = Repo::new(&index, &index, &worktree);
    let dir = repo.dir.path();
    let _cwd = Cwd::enter(dir);
    let mut app = repo.app()?;
    let cached = || git_ok(dir, &["diff", "--cached", "--", "file.txt"]);
    let first = "-b\n+B";
    let second = "-s\n+S";
    let status = |app: &Application| app.editor.get_status().unwrap().0.to_string();
    let blob = |content: &str| {
        let path = dir.join("blob.tmp");
        fs::write(&path, content).unwrap();
        let oid = git_ok(dir, &["hash-object", "blob.tmp"]);
        fs::remove_file(path).unwrap();
        oid
    };
    let staged_hunks = |app: &Application| {
        let doc = app.editor.document_by_path(&repo.file).unwrap();
        doc.review.as_ref().unwrap().staged_hunks()
    };

    // Outside any unstaged hunk, and with unsaved changes, nothing is staged.
    send_keys(&mut app, ":review<ret>:review-stage<ret>").await?;
    assert_error(&app, "nothing to stage here");
    send_keys(&mut app, "jix<esc>:review-stage<ret>").await?;
    assert_error(&app, "unsaved changes");
    send_keys(&mut app, "u").await?;
    assert_eq!(cached(), "");

    // Enter stages the first of two hunks: the index gets exactly that hunk.
    send_keys(&mut app, "<ret>").await?;
    assert_eq!(status(&app), "Staged 1 hunk; 1 of 2 staged in file.txt");
    let staged = cached();
    assert!(
        staged.contains(first) && !staged.contains(second),
        "{staged}"
    );
    let ls = git_ok(dir, &["ls-files", "-s", "file.txt"]);
    assert!(ls.contains(&blob(&index.replace("b\n", "B\n"))), "{ls}");
    assert_eq!(repo.disk(), worktree);
    assert_eq!(staged_hunks(&app), 1);
    let doc = app.editor.document_by_path(&repo.file).unwrap();
    let flags: Vec<_> = doc.review.as_ref().unwrap().changed_lines_stage().collect();
    assert_eq!(
        flags,
        [(1..2, HunkStage::Staged), (18..19, HunkStage::Unstaged)]
    );
    // Staged rows use the .staged scopes: their background differs from the unstaged hunk's.
    let surface = render_surface(&app);
    let (view, doc) = current_ref!(app.editor);
    let inner = view.inner_area(doc);
    let row_bg = |text: &str| {
        let y = render(&app)
            .iter()
            .position(|(_, row)| row == text)
            .unwrap_or_else(|| panic!("{text} is not visible")) as u16;
        surface[(inner.x, inner.y + y)].bg
    };
    assert_ne!(row_bg("B"), row_bg("S"));
    assert_ne!(row_bg("b"), row_bg("s"));

    // Edits keep staged hunks staged unless they touch them; undo restores the marks.
    let flags = |app: &Application| -> Vec<bool> {
        let doc = app.editor.document_by_path(&repo.file).unwrap();
        let review = doc.review.as_ref().unwrap();
        review
            .changed_lines_stage()
            .map(|(_, stage)| stage == HunkStage::Staged)
            .collect()
    };
    send_keys(&mut app, "ggjjjix<esc>").await?;
    assert_eq!(flags(&app), [true, false, false]);
    assert_eq!(staged_hunks(&app), 1);
    send_keys(&mut app, "uggjiy<esc>").await?;
    assert_eq!(flags(&app), [false, false]);
    send_keys(&mut app, "u").await?;
    assert_eq!(flags(&app), [true, false]);
    assert!(!doc!(app.editor).is_modified());
    send_keys(&mut app, "ggj").await?;

    // :review-unstage only unstages staged hunks, and :review-stage only unstaged ones.
    send_keys(&mut app, "17j:review-unstage<ret>").await?;
    assert_error(&app, "nothing staged here");
    send_keys(&mut app, "gg17k:review-stage<ret>").await?;
    assert_error(&app, "nothing to stage here");

    // Stage the second as well: the index matches the worktree.
    send_keys(&mut app, "ggj17j:review-toggle-stage<ret>").await?;
    assert_eq!(status(&app), "Staged 1 hunk; 2 of 2 staged in file.txt");
    let staged = cached();
    assert!(
        staged.contains(first) && staged.contains(second),
        "{staged}"
    );
    assert_eq!(git_ok(dir, &["diff", "--", "file.txt"]), "");

    // Toggling the second again unstages only it; the first stays staged.
    send_keys(&mut app, "<ret>").await?;
    assert_eq!(status(&app), "Unstaged 1 hunk; 1 of 2 staged in file.txt");
    let staged = cached();
    assert!(
        staged.contains(first) && !staged.contains(second),
        "{staged}"
    );

    // Toggling the first twice in total returns the index to its original content.
    send_keys(&mut app, "ggj<ret>").await?;
    assert_eq!(status(&app), "Unstaged 1 hunk; 0 of 2 staged in file.txt");
    assert_eq!(cached(), "");
    assert!(git_ok(dir, &["ls-files", "-s", "file.txt"]).contains(&blob(&index)));
    assert_eq!(staged_hunks(&app), 0);

    // A lock held by another git command is refused, and the index stays as it was.
    let lock = dir.join(".git").join("index.lock");
    fs::write(&lock, "")?;
    send_keys(&mut app, "<ret>").await?;
    assert_error(&app, "index.lock exists");
    fs::remove_file(&lock)?;
    assert_eq!(cached(), "");

    // In the :review-diff buffer, Enter toggles the hunk under the cursor and marks it.
    let diff_text = |app: &Application| doc!(app.editor).text().to_string();
    send_keys(&mut app, ":review-diff<ret>").await?;
    let line = diff_text(&app)
        .lines()
        .position(|line| line == "+S")
        .unwrap();
    send_keys(&mut app, &format!("gg{line}j<ret>")).await?;
    let staged = cached();
    assert!(
        staged.contains(second) && !staged.contains(first),
        "{staged}"
    );
    let headers = |app: &Application| -> Vec<bool> {
        diff_text(app)
            .lines()
            .filter(|line| line.starts_with("@@ "))
            .map(|line| line.ends_with(" (staged)"))
            .collect()
    };
    assert_eq!(headers(&app), [false, true]);
    send_keys(&mut app, "<ret>").await?;
    assert_eq!(status(&app), "Unstaged 1 hunk; 0 of 2 staged in file.txt");
    assert_eq!(cached(), "");
    assert_eq!(headers(&app), [false, false]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn handoff_requests_ask_the_agent_to_commit_or_follow_a_message() -> anyhow::Result<()> {
    let repo = Repo::new("a\nb\n", "a\nb\n", "a\nb\n");
    let dir = repo.dir.path();
    let _cwd = Cwd::enter(dir);
    let outbox = tempfile::tempdir()?;
    let mut app = repo.app()?;
    // Reads the only batch in the outbox and removes it.
    let take_batch = || -> serde_json::Value {
        let batches: Vec<_> = fs::read_dir(outbox.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(batches.len(), 1, "{batches:?}");
        let json = serde_json::from_str(&fs::read_to_string(&batches[0]).unwrap()).unwrap();
        fs::remove_file(&batches[0]).unwrap();
        json
    };

    let handoff = format!(":review-handoff {}<ret>", outbox.path().display());
    send_keys(
        &mut app,
        &format!("{handoff}:review<ret>:review-commit<ret>"),
    )
    .await?;
    assert_error(&app, "nothing is staged");
    assert_eq!(fs::read_dir(outbox.path())?.count(), 0);

    // :review-send with text carries it as a message, and only once.
    send_keys(&mut app, ":review-send please rename b<ret>").await?;
    assert_eq!(
        take_batch()["request"],
        serde_json::json!({ "kind": "message", "text": "please rename b" })
    );
    send_keys(&mut app, ":review-send<ret>").await?;
    assert!(take_batch().get("request").is_none());

    // With a staged change, :review-commit asks for a commit of it with the subject.
    repo.write("a\nB\n");
    repo.stage();
    send_keys(
        &mut app,
        ":reload<ret>:review-commit fix: capitalize b<ret>",
    )
    .await?;
    let (status, _) = app.editor.get_status().unwrap();
    assert_eq!(status.as_ref(), "Handed off: commit staged hunks");
    assert_eq!(
        take_batch()["request"],
        serde_json::json!({ "kind": "commit-staged", "subject": "fix: capitalize b" })
    );
    send_keys(&mut app, ":review-commit<ret>").await?;
    assert_eq!(
        take_batch()["request"],
        serde_json::json!({ "kind": "commit-staged" })
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn review_commands_enable_review_against_head_on_their_own() -> anyhow::Result<()> {
    let repo = Repo::new("a\ngone\nb\n", "a\ngone\nb\n", "a\nb\n");
    let head = git_ok(repo.dir.path(), &["rev-parse", "HEAD"]);
    let _cwd = Cwd::enter(repo.dir.path());
    let mut app = repo.app()?;
    assert!(doc!(app.editor).review.is_none());

    // Without a baseline or a review, :review-old pins HEAD, reviews the buffer and focuses.
    send_keys(&mut app, ":review-old<ret>").await?;
    assert_eq!(app.editor.review_baseline.as_deref(), Some(head.as_str()));
    let review = doc!(app.editor).review.as_ref().unwrap();
    assert_eq!(review.baseline().revision(), Some(head.as_str()));
    let focus = old_focus(&app).expect("old text is focused");
    let text = review.snapshot().slice(..);
    assert_eq!(focus.fragment(text), "g");

    // A closed review stays closed for commands that only need the baseline.
    send_keys(&mut app, "<esc>:review-close<ret>:review-comments<ret>").await?;
    assert!(doc!(app.editor).review.is_none());
    assert_error(&app, "no pending review comments");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn one_enter_stages_without_or_right_after_a_review() -> anyhow::Result<()> {
    let repo = Repo::new("a\nb\nc\n", "a\nb\nc\n", "a\nB\nc\n");
    let dir = repo.dir.path();
    let _cwd = Cwd::enter(dir);
    let cached = || git_ok(dir, &["diff", "--cached", "--", "file.txt"]);
    let status = |app: &Application| app.editor.get_status().unwrap().0.to_string();

    // No review and no baseline: Enter on an unchanged line does nothing, on the change it
    // enables review against HEAD and stages in the same press.
    let mut app = repo.app()?;
    send_keys(&mut app, "<ret>").await?;
    assert!(doc!(app.editor).review.is_none());
    assert_eq!(app.editor.review_baseline, None);
    send_keys(&mut app, "j<ret>").await?;
    assert_eq!(status(&app), "Staged 1 hunk; 1 of 1 staged in file.txt");
    assert!(cached().contains("-b\n+B"), "{}", cached());
    assert!(doc!(app.editor).review.is_some());
    send_keys(&mut app, "<ret>").await?;
    assert_eq!(cached(), "");

    // Right after the hook's review-baseline, one Enter stages too.
    let mut app = repo.app()?;
    send_keys(&mut app, ":review-baseline HEAD<ret>j<ret>").await?;
    assert_eq!(status(&app), "Staged 1 hunk; 1 of 1 staged in file.txt");
    assert!(cached().contains("-b\n+B"), "{}", cached());

    // A file the agent changed after the last refresh is refused visibly, not silently.
    repo.write_external("a\nB\nC\n");
    send_keys(&mut app, "<ret>").await?;
    assert_error(&app, "changed on disk");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn review_mark_committed_primary_changes_advance_and_can_be_unmarked() -> anyhow::Result<()> {
    let before = "a\nb\nc\nd\ne\nf\n";
    let after = "a\nB\nc\nd\ne\nF\n";
    let repo = Repo::new(before, before, before);
    let _cwd = Cwd::enter(repo.dir.path());
    let baseline = git_ok(repo.dir.path(), &["rev-parse", "HEAD"]);
    repo.write(after);
    fs::write(repo.dir.path().join("other.txt"), "other\n")?;
    git_ok(repo.dir.path(), &["add", "."]);
    git_ok(repo.dir.path(), &["commit", "-m", "agent work"]);
    let index = git_ok(repo.dir.path(), &["write-tree"]);
    let mut app = repo.app()?;
    let pin = format!(":review-baseline {baseline}<ret>");
    send_keys(&mut app, &pin).await?;
    // Multiple selections still mark only the primary current change.
    {
        let (view, doc) = helix_view::current!(app.editor);
        let selection = helix_core::Selection::new(
            smallvec::smallvec![
                Range::point(doc.text().line_to_char(1)),
                Range::point(doc.text().line_to_char(5)),
            ],
            0,
        );
        doc.set_selection(view.id, selection);
    }
    send_keys(&mut app, ":review-mark<ret>").await?;
    assert_status_not_error(&app.editor);
    let (view, doc) = current_ref!(app.editor);
    let review = doc.review.as_ref().unwrap();
    assert!(review.hunk_reviewed(0));
    assert!(!review.hunk_reviewed(1));
    assert_eq!(
        doc.selection(view.id)
            .primary()
            .cursor_line(doc.text().slice(..)),
        5
    );
    assert!(app.editor.get_status().unwrap().0.contains("1/2 reviewed"));
    send_keys(&mut app, ":review-mark<ret>").await?;
    assert_eq!(
        doc!(app.editor).path().unwrap().file_name().unwrap(),
        "other.txt"
    );
    send_keys(&mut app, ":review-mark<ret>").await?;
    assert!(app
        .editor
        .get_status()
        .unwrap()
        .0
        .contains("changes reviewed"));
    send_keys(&mut app, ":review-prev<ret>:review-unmark<ret>").await?;
    assert_eq!(doc!(app.editor).path(), Some(repo.file.as_path()));
    let review = doc!(app.editor).review.as_ref().unwrap();
    assert!(review.hunk_reviewed(0));
    assert!(!review.hunk_reviewed(1));
    send_keys(
        &mut app,
        ":review-next<ret>:review-prev<ret>:review-mark<ret>",
    )
    .await?;
    assert!(app
        .editor
        .get_status()
        .unwrap()
        .0
        .contains("changes reviewed"));
    send_keys(&mut app, &pin).await?;
    let review = doc!(app.editor).review.as_ref().unwrap();
    assert_eq!(review.reviewed_hunks(), 2);

    // Editing one marked hunk invalidates it; neither marking nor editing touches the index.
    send_keys(&mut app, "iX<esc>").await?;
    let review = doc!(app.editor).review.as_ref().unwrap();
    assert_eq!(review.reviewed_hunks(), 1);
    assert!(review.hunk_reviewed(0));
    assert!(!review.hunk_reviewed(1));
    send_keys(&mut app, ":review HEAD<ret>").await?;
    assert_eq!(
        doc!(app.editor).review.as_ref().unwrap().reviewed_hunks(),
        0
    );
    assert_eq!(git_ok(repo.dir.path(), &["write-tree"]), index);
    assert_eq!(repo.disk(), after);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn review_mark_skips_unreviewable_files_without_claiming_completion() -> anyhow::Result<()> {
    let repo = Repo::new("before\n", "before\n", "before\n");
    let dir = repo.dir.path();
    let _cwd = Cwd::enter(dir);
    let baseline = git_ok(dir, &["rev-parse", "HEAD"]);
    repo.write("after\n");
    fs::write(dir.join("middle.txt"), "x\n".repeat(160_000))?;
    let suspended = dir.join("m-suspended.txt");
    fs::write(&suspended, "small\n")?;
    let last = dir.join("z-last.txt");
    fs::write(&last, "last\n")?;
    git_ok(dir, &["add", "."]);
    git_ok(dir, &["commit", "-m", "agent work"]);
    let index = git_ok(dir, &["write-tree"]);
    let mut app = repo.app()?;
    send_keys(
        &mut app,
        &format!(
            ":review-baseline {baseline}<ret>:open '{}'<ret>",
            suspended.display()
        ),
    )
    .await?;
    // A previously valid review can suspend after a large unsaved edit.
    send_keys_and_paste(&mut app, "i", &"x".repeat(300_000)).await?;
    assert!(doc!(app.editor).review.as_ref().unwrap().is_suspended());
    send_keys(
        &mut app,
        &format!("<esc>:open '{}'<ret>:review-mark<ret>", repo.file.display()),
    )
    .await?;
    assert_status_not_error(&app.editor);
    assert_eq!(doc!(app.editor).path(), Some(last.as_path()));
    send_keys(&mut app, ":review-mark<ret>").await?;
    assert_status_not_error(&app.editor);
    assert_eq!(doc!(app.editor).path(), Some(last.as_path()));
    let status = app.editor.get_status().unwrap().0;
    assert!(
        status.contains("Marked reviewed; could not find the next change"),
        "{status}"
    );
    assert!(status.contains("middle.txt"), "{status}");
    assert!(status.contains("m-suspended.txt"), "{status}");
    assert!(
        !status.contains("All committed changes reviewed"),
        "{status}"
    );
    assert_eq!(git_ok(dir, &["write-tree"]), index);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn hunks_committed_after_the_baseline_are_neither_staged_nor_stageable() -> anyhow::Result<()>
{
    let repo = Repo::new("a\nb\nc\n", "a\nb\nc\n", "a\nb\nc\n");
    let dir = repo.dir.path();
    let _cwd = Cwd::enter(dir);
    let baseline = git_ok(dir, &["rev-parse", "HEAD"]);
    let mut config = test_config();
    config.editor.statusline.right = vec![StatusLineElement::Review];
    let mut app = AppBuilder::new()
        .with_file(&repo.file, None)
        .with_config(config)
        .build()?;
    let pin = format!(":review-baseline {baseline}<ret>");
    send_keys(&mut app, &pin).await?;

    // The agent commits a change on its own; the refresh keeps the pinned baseline.
    repo.write("a\nB\nc\n");
    repo.stage();
    git_ok(dir, &["commit", "-m", "agent"]);
    let head = git_ok(dir, &["rev-parse", "HEAD"]);
    send_keys(&mut app, &pin).await?;
    let status = statusline(&app);
    assert!(status.contains(" 1 hunk 1 committed "), "{status}");
    // The committed hunk uses its own scope.
    let review = doc!(app.editor).review.as_ref().unwrap();
    let stages: Vec<_> = review.changed_lines_stage().collect();
    assert_eq!(stages, [(1..2, HunkStage::Committed)]);
    let surface = render_surface(&app);
    let (view, doc) = current_ref!(app.editor);
    let inner = view.inner_area(doc);
    let row_bg = |text: &str| {
        let y = render(&app)
            .iter()
            .position(|(_, row)| row == text)
            .unwrap_or_else(|| panic!("{text} is not visible")) as u16;
        Some(surface[(inner.x, inner.y + y)].bg)
    };
    let theme = &app.editor.theme;
    // A muted tint, still clearly off the editor background.
    use helix_view::graphics::Color;
    for (row, scope, tint) in [
        ("B", "diff.plus.review", Color::Rgb(0x35, 0x33, 0x57)),
        ("b", "diff.minus.review", Color::Rgb(0x4b, 0x2c, 0x4f)),
    ] {
        let committed = theme.get(&format!("{scope}.committed")).bg;
        assert_eq!(committed, Some(tint), "{scope}.committed");
        assert_eq!(row_bg(row), committed, "{row}");
    }

    send_keys(&mut app, "j:review-commit<ret>").await?;
    assert_error(
        &app,
        &format!(
            "nothing is staged; 1 hunk already committed in {}",
            &head[..8]
        ),
    );
    // Enter on a committed hunk records reading progress without changing the index.
    send_keys(&mut app, "<ret>").await?;
    assert_status_not_error(&app.editor);
    assert!(app
        .editor
        .get_status()
        .unwrap()
        .0
        .contains("changes reviewed"));
    assert!(doc!(app.editor).review.as_ref().unwrap().hunk_reviewed(0));
    assert_eq!(git_ok(dir, &["diff", "--cached"]), "");
    // An uncommitted change elsewhere is left for Stage, without losing the review mark.
    fs::write(
        dir.join("other.txt"),
        "x
",
    )?;
    git_ok(dir, &["add", "other.txt"]);
    send_keys(&mut app, "<ret>").await?;
    assert_status_not_error(&app.editor);
    assert!(app
        .editor
        .get_status()
        .unwrap()
        .0
        .contains("changes reviewed"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_hooks_refresh_marks_hunks_committed_outside_helix() -> anyhow::Result<()> {
    let lines = (1..=12).map(|n| format!("{n}\n")).collect::<String>();
    let changed = (1..=12)
        .map(|n| match n {
            3 | 9 => format!("changed {n}\n"),
            n => format!("{n}\n"),
        })
        .collect::<String>();
    let repo = Repo::new(&lines, &lines, &changed);
    let dir = repo.dir.path();
    let _cwd = Cwd::enter(dir);
    let baseline = git_ok(dir, &["rev-parse", "HEAD"]);
    let mut config = test_config();
    config.editor.statusline.right = vec![StatusLineElement::Review];
    let mut app = AppBuilder::new()
        .with_file(&repo.file, None)
        .with_config(config)
        .build()?;
    let pin = format!(":review-baseline {baseline}<ret>");
    send_keys(&mut app, &pin).await?;
    send_keys(&mut app, "3gg<ret>").await?;
    let status = statusline(&app);
    assert!(status.contains(" 2 hunks 1 staged "), "{status}");

    // The agent commits the index; the worktree stays as it is.
    git_ok(dir, &["commit", "-m", "agent"]);
    let head = git_ok(dir, &["rev-parse", "HEAD"]);
    app.editor.socket_command = true;
    send_keys(&mut app, &pin).await?;
    app.editor.socket_command = false;
    let status = statusline(&app);
    assert!(status.contains(" 2 hunks 1 committed "), "{status}");
    send_keys(&mut app, "3gg<ret>").await?;
    assert_error(&app, &format!("already committed in {}", &head[..8]));
    send_keys(&mut app, ":review-files<ret><esc>").await?;
    assert_eq!(
        app.editor.get_status().unwrap().0,
        "1 file, +2 -2, 0/2 hunks staged, 1 committed"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn enter_stages_the_uncommitted_part_of_a_partly_committed_block() -> anyhow::Result<()> {
    let lines = (1..=12).map(|n| format!("{n}\n")).collect::<String>();
    let changing = |changed: &[usize]| {
        (1..=12)
            .map(|n| match changed.contains(&n) {
                true => format!("changed {n}\n"),
                false => format!("{n}\n"),
            })
            .collect::<String>()
    };
    let repo = Repo::new(&lines, &lines, &lines);
    let dir = repo.dir.path();
    let _cwd = Cwd::enter(dir);
    let baseline = git_ok(dir, &["rev-parse", "HEAD"]);
    // After the baseline the agent commits lines 3-4 and 10, then edits line 5 next to them.
    repo.write(&changing(&[3, 4, 10]));
    repo.stage();
    git_ok(dir, &["commit", "-m", "agent"]);
    let head = git_ok(dir, &["rev-parse", "HEAD"]);
    repo.write(&changing(&[3, 4, 5, 10]));
    let mut app = repo.app()?;
    send_keys(&mut app, &format!(":review-baseline {baseline}<ret>")).await?;
    let review = doc!(app.editor).review.as_ref().unwrap();
    assert_eq!(
        review.changed_lines_stage().collect::<Vec<_>>(),
        [(2..5, HunkStage::Unstaged), (9..10, HunkStage::Committed)]
    );

    // On the committed part of the mixed block, Enter stages the block's new change.
    send_keys(&mut app, "3gg<ret>").await?;
    assert_status_not_error(&app.editor);
    let status = app.editor.get_status().unwrap().0.to_string();
    assert!(
        status.starts_with("Staged 1 hunk, 1 already committed;"),
        "{status}"
    );
    let staged = git_ok(dir, &["diff", "--cached"]);
    assert!(staged.contains("+changed 5"), "{staged}");
    assert!(!staged.contains("+changed 3"), "{staged}");
    assert_eq!(git_ok(dir, &["diff", "--name-only"]), "");

    // A block that is committed as a whole still has nothing to stage.
    send_keys(&mut app, "10gg<ret>").await?;
    assert_error(&app, &format!("already committed in {}", &head[..8]));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_baseline_steps_through_history_and_survives_the_hooks_refresh() -> anyhow::Result<()> {
    let repo = Repo::new("a\nb\nc\nd\ne\n", "a\nb\nc\nd\ne\n", "a\nb\nc\nd\ne\n");
    let dir = repo.dir.path();
    let _cwd = Cwd::enter(dir);
    let commit = |content: &str, subject: &str| {
        repo.write(content);
        repo.stage();
        git_ok(dir, &["commit", "-m", subject]);
        git_ok(dir, &["rev-parse", "HEAD"])
    };
    let first = git_ok(dir, &["rev-parse", "HEAD"]);
    let second = commit("A\nb\nc\nd\ne\n", "second");
    let third = commit("A\nb\nC\nd\ne\n", "third");
    repo.write("A\nb\nC\nd\nE\n");
    let mut config = test_config();
    config.editor.statusline.right = vec![StatusLineElement::Review];
    let mut app = AppBuilder::new()
        .with_file(&repo.file, None)
        .with_config(config)
        .build()?;
    let review = |app: &Application| {
        let line = statusline(app);
        line.split_once(" review ")
            .map(|(_, element)| element.trim_end().to_owned())
            .unwrap_or_default()
    };
    let status = |app: &Application| app.editor.get_status().unwrap().0.to_string();
    // The pi hook sends its baselines over the command socket.
    async fn hook(app: &mut Application, commit: &str) -> anyhow::Result<()> {
        app.editor.socket_command = true;
        let result = send_keys(app, &format!(":review-baseline {commit}<ret>")).await;
        app.editor.socket_command = false;
        result
    }

    hook(&mut app, "HEAD").await?;
    assert_eq!(review(&app), format!("{} 1 hunk", &third[..8]));
    send_keys(&mut app, ":review-older<ret>").await?;
    assert_eq!(
        status(&app),
        format!("Baseline {} second (1 behind HEAD)", &second[..8])
    );
    assert_eq!(
        review(&app),
        format!("{}* 2 hunks 1 committed", &second[..8])
    );
    send_keys(&mut app, "<space>A-").await?;
    assert_eq!(
        status(&app),
        format!("Baseline {} head (2 behind HEAD)", &first[..8])
    );
    assert_eq!(
        review(&app),
        format!("{}* 3 hunks 2 committed", &first[..8])
    );
    send_keys(&mut app, ":review-older<ret>").await?;
    assert_error(&app, "has no parent");

    // The hook re-sending its baseline keeps the one the reviewer stepped to.
    hook(&mut app, &third).await?;
    assert_eq!(
        review(&app),
        format!("{}* 3 hunks 2 committed", &first[..8])
    );

    send_keys(&mut app, ":review-newer<ret>").await?;
    assert_eq!(
        review(&app),
        format!("{}* 2 hunks 1 committed", &second[..8])
    );
    send_keys(&mut app, "<space>A+").await?;
    assert_eq!(
        status(&app),
        format!("Baseline {} third (0 behind HEAD)", &third[..8])
    );
    assert_eq!(review(&app), format!("{} 1 hunk", &third[..8]));
    send_keys(&mut app, ":review-newer<ret>").await?;
    assert_error(&app, "already at HEAD");

    // Nor does a baseline the hook moved on to replace it, until the pin is cleared.
    send_keys(&mut app, ":review-older<ret>").await?;
    let fourth = commit("A\nb\nC\nd\nE\n", "fourth");
    hook(&mut app, &fourth).await?;
    assert_eq!(app.editor.review_baseline.as_deref(), Some(second.as_str()));
    send_keys(&mut app, ":review-baseline clear<ret>").await?;
    hook(&mut app, &fourth).await?;
    assert!(app.editor.review_user_pin.is_none());
    assert_eq!(app.editor.review_baseline.as_deref(), Some(fourth.as_str()));
    assert_eq!(
        review(&app),
        &fourth[..8],
        "no changes since it, and no longer pinned"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn review_next_walks_changes_across_files_and_wraps_around() -> anyhow::Result<()> {
    let lines = (1..=20).map(|n| format!("{n}\n")).collect::<String>();
    let changed = (1..=20)
        .map(|n| match n {
            2 => "two\n".to_owned(),
            15 => "fifteen\n".to_owned(),
            n => format!("{n}\n"),
        })
        .collect::<String>();
    let repo = Repo::new(&lines, &lines, &changed);
    let dir = repo.dir.path();
    let _cwd = Cwd::enter(dir);
    fs::write(dir.join("other.txt"), "x\ny\n")?;
    git_ok(dir, &["add", "other.txt"]);
    git_ok(dir, &["commit", "-m", "other"]);
    fs::write(dir.join("other.txt"), "X\ny\n")?;
    let mut app = repo.app()?;
    let status = |app: &Application| app.editor.get_status().unwrap().0.to_string();
    let at = |app: &Application| {
        let (view, doc) = current_ref!(app.editor);
        let name = doc
            .path()
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        (
            name,
            doc.selection(view.id)
                .primary()
                .cursor_line(doc.text().slice(..)),
        )
    };

    // file.txt's hunks are on lines 2 and 15 (1-based); other.txt's on line 1.
    send_keys(&mut app, "<space>An").await?;
    assert_eq!(at(&app), ("file.txt".to_owned(), 1));
    assert_eq!(status(&app), "file.txt: hunk 1 of 2");
    send_keys(&mut app, "<space>An").await?;
    assert_eq!(at(&app), ("file.txt".to_owned(), 14));
    send_keys(&mut app, "<space>An").await?;
    assert_eq!(at(&app), ("other.txt".to_owned(), 0));
    assert_eq!(status(&app), "other.txt: hunk 1 of 1");
    send_keys(&mut app, "<space>AN").await?;
    assert_eq!(at(&app), ("file.txt".to_owned(), 14));
    assert_eq!(status(&app), "file.txt: hunk 2 of 2");
    // Wrapping: past other.txt's last hunk comes file.txt's first, and back again.
    send_keys(&mut app, "2<space>An").await?;
    assert_eq!(at(&app), ("file.txt".to_owned(), 1));
    send_keys(&mut app, "<space>AN").await?;
    assert_eq!(at(&app), ("other.txt".to_owned(), 0));
    send_keys(&mut app, ":review-prev<ret>").await?;
    assert_eq!(at(&app), ("file.txt".to_owned(), 14));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn review_next_skips_committed_hunks_and_files() -> anyhow::Result<()> {
    let lines = (1..=20).map(|n| format!("{n}\n")).collect::<String>();
    let changing = |changed: &[usize]| {
        (1..=20)
            .map(|n| match changed.contains(&n) {
                true => format!("changed {n}\n"),
                false => format!("{n}\n"),
            })
            .collect::<String>()
    };
    let repo = Repo::new(&lines, &lines, &lines);
    let dir = repo.dir.path();
    let _cwd = Cwd::enter(dir);
    fs::write(dir.join("other.txt"), "x\ny\n")?;
    git_ok(dir, &["add", "other.txt"]);
    git_ok(dir, &["commit", "-m", "other"]);
    let baseline = git_ok(dir, &["rev-parse", "HEAD"]);
    // The agent commits line 3 and all of other.txt, line 9 is staged and line 15 unstaged.
    repo.write(&changing(&[3]));
    fs::write(dir.join("other.txt"), "XX\ny\n")?;
    git_ok(dir, &["add", "file.txt", "other.txt"]);
    git_ok(dir, &["commit", "-m", "agent"]);
    repo.write(&changing(&[3, 9]));
    repo.stage();
    repo.write(&changing(&[3, 9, 15]));
    let mut app = repo.app()?;
    let status = |app: &Application| app.editor.get_status().unwrap().0.to_string();
    let at = |app: &Application| {
        let (view, doc) = current_ref!(app.editor);
        let name = doc.path().unwrap().file_name().unwrap().to_owned();
        let line = doc
            .selection(view.id)
            .primary()
            .cursor_line(doc.text().slice(..));
        (name.into_string().unwrap(), line)
    };
    let pin = format!(":review-baseline {baseline}<ret>");
    send_keys(&mut app, &pin).await?;

    send_keys(&mut app, "gg<space>An").await?;
    assert_eq!(at(&app), ("file.txt".to_owned(), 8));
    assert_eq!(status(&app), "file.txt: hunk 1 of 2");
    send_keys(&mut app, "<space>An").await?;
    assert_eq!(at(&app), ("file.txt".to_owned(), 14));
    assert_eq!(status(&app), "file.txt: hunk 2 of 2");
    // other.txt holds only committed hunks: rolling over skips it and wraps around.
    send_keys(&mut app, "<space>An").await?;
    assert_eq!(at(&app), ("file.txt".to_owned(), 8));
    send_keys(&mut app, "<space>AN").await?;
    assert_eq!(at(&app), ("file.txt".to_owned(), 14));

    // With everything committed, every block counts again, other.txt's too.
    git_ok(dir, &["commit", "-am", "rest"]);
    send_keys(&mut app, &pin).await?;
    send_keys(&mut app, "<space>An").await?;
    assert_eq!(at(&app), ("other.txt".to_owned(), 0));
    assert_eq!(status(&app), "other.txt: hunk 1 of 1");
    send_keys(&mut app, "<space>An").await?;
    assert_eq!(at(&app), ("file.txt".to_owned(), 2));
    assert_eq!(status(&app), "file.txt: hunk 1 of 3");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn navigation_follows_staging_and_commits_made_outside_helix() -> anyhow::Result<()> {
    let lines = (1..=12).map(|n| format!("{n}\n")).collect::<String>();
    let changed = lines.replacen("3\n", "changed 3\n", 1);
    let repo = Repo::new(&lines, &lines, &changed);
    let dir = repo.dir.path();
    let _cwd = Cwd::enter(dir);
    fs::write(dir.join("other.txt"), "x\ny\n")?;
    git_ok(dir, &["add", "other.txt"]);
    git_ok(dir, &["commit", "-m", "other"]);
    let baseline = git_ok(dir, &["rev-parse", "HEAD"]);
    // other.txt's change is staged; file.txt's line 3 is not.
    fs::write(dir.join("other.txt"), "X\ny\n")?;
    git_ok(dir, &["add", "other.txt"]);
    let mut app = repo.app()?;
    let status = |app: &Application| app.editor.get_status().unwrap().0.to_string();
    let at = |app: &Application| {
        let (view, doc) = current_ref!(app.editor);
        let name = doc.path().unwrap().file_name().unwrap().to_owned();
        let line = doc
            .selection(view.id)
            .primary()
            .cursor_line(doc.text().slice(..));
        (name.into_string().unwrap(), line)
    };
    send_keys(&mut app, &format!(":review-baseline {baseline}<ret>")).await?;

    // Staging file.txt's hunk leaves nothing unstaged: Enter stays.
    send_keys(&mut app, "3gg<ret>").await?;
    assert_eq!(status(&app), "Staged 1 hunk; 1 of 1 staged in file.txt");
    assert_eq!(at(&app), ("file.txt".to_owned(), 2));
    // Unstaged outside Helix, other.txt's change is where Enter's jump goes.
    git_ok(dir, &["reset", "-q", "--", "other.txt"]);
    send_keys(&mut app, "gg<ret>").await?;
    assert_eq!(at(&app), ("other.txt".to_owned(), 0));
    assert_eq!(status(&app), "next: other.txt hunk 1 of 1");

    // Committed outside Helix, every change is committed: n walks them all again.
    git_ok(dir, &["commit", "-qam", "agent"]);
    send_keys(&mut app, "<space>An").await?;
    assert_eq!(at(&app), ("file.txt".to_owned(), 2));
    assert_eq!(status(&app), "file.txt: hunk 1 of 1");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn commit_all_stages_every_changed_file_and_asks_for_a_commit() -> anyhow::Result<()> {
    let repo = Repo::new("a\n", "a\n", "A\n");
    let dir = repo.dir.path();
    let _cwd = Cwd::enter(dir);
    fs::write(dir.join("gone.txt"), "gone\n")?;
    git_ok(dir, &["add", "gone.txt"]);
    git_ok(dir, &["commit", "-m", "gone"]);
    fs::remove_file(dir.join("gone.txt"))?;
    fs::write(dir.join("new.txt"), "new\n")?;
    let outbox = tempfile::tempdir()?;
    let mut app = repo.app()?;
    let handoff = format!(":review-handoff {}<ret>", outbox.path().display());
    send_keys(&mut app, &handoff).await?;

    send_keys(&mut app, ":review-commit-all fix: everything<ret>").await?;
    assert_eq!(
        app.editor.get_status().unwrap().0,
        "Staged 3 files; handed off: commit staged hunks"
    );
    let staged = git_ok(dir, &["diff", "--cached", "--name-status"]);
    assert_eq!(staged, "M\tfile.txt\nD\tgone.txt\nA\tnew.txt");
    assert_eq!(git_ok(dir, &["diff", "--name-only"]), "");
    let batches: Vec<_> = fs::read_dir(outbox.path())?.collect::<Result<_, _>>()?;
    assert_eq!(batches.len(), 1, "{batches:?}");
    let json: serde_json::Value = serde_json::from_str(&fs::read_to_string(batches[0].path())?)?;
    assert_eq!(
        json["request"],
        serde_json::json!({ "kind": "commit-staged", "subject": "fix: everything" })
    );

    git_ok(dir, &["commit", "-m", "all"]);
    send_keys(&mut app, ":review-baseline HEAD<ret><space>AM").await?;
    assert_error(&app, "no changes since");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn enter_stages_and_unstages_every_hunk_the_selections_touch() -> anyhow::Result<()> {
    let lines = (1..=20).map(|n| format!("{n}\n")).collect::<String>();
    let changed = (1..=20)
        .map(|n| match n {
            3 | 9 | 15 => format!("changed {n}\n"),
            n => format!("{n}\n"),
        })
        .collect::<String>();
    let repo = Repo::new(&lines, &lines, &changed);
    let dir = repo.dir.path();
    let _cwd = Cwd::enter(dir);
    let cached = || git_ok(dir, &["diff", "--cached", "--stat"]);
    let status = |app: &Application| app.editor.get_status().unwrap().0.to_string();
    let mut app = repo.app()?;
    send_keys(&mut app, ":review-baseline HEAD<ret>").await?;

    // One selection from line 3 to line 15 spans all three hunks.
    send_keys(&mut app, "3ggv14jx<ret>").await?;
    assert_eq!(status(&app), "Staged 3 hunks; 3 of 3 staged in file.txt");
    assert!(
        cached().contains("3 insertions(+), 3 deletions(-)"),
        "{}",
        cached()
    );
    send_keys(&mut app, "<ret>").await?;
    assert_eq!(status(&app), "Unstaged 3 hunks; 0 of 3 staged in file.txt");
    assert_eq!(cached(), "");

    // With the middle hunk already staged, the set stages only the other two.
    send_keys(&mut app, "<esc>9gg<ret>").await?;
    assert_eq!(status(&app), "Staged 1 hunk; 1 of 3 staged in file.txt");
    send_keys(&mut app, "3ggv14jx<ret>").await?;
    assert_eq!(status(&app), "Staged 2 hunks; 3 of 3 staged in file.txt");

    // Off every hunk, with all of them staged, Enter has nowhere to jump to.
    send_keys(&mut app, "<esc>gg<ret>").await?;
    assert_error(&app, "nothing unstaged since");
    Ok(())
}

/// A fake `gh` (through `$HELIX_GH`) that appends its arguments to `log` and answers from
/// `script`, a shell `case "$*"` body; unset again when dropped.
struct FakeGh {
    log: PathBuf,
    _dir: TempDir,
}

impl FakeGh {
    fn new(script: &str) -> FakeGh {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("argv.log");
        let path = dir.path().join("gh");
        fs::write(
            &path,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$*\" in\n{script}\n*) echo \"unexpected: $*\" >&2; exit 1;;\nesac\n",
                log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        std::env::set_var("HELIX_GH", &path);
        FakeGh { log, _dir: dir }
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

impl Drop for FakeGh {
    fn drop(&mut self) {
        std::env::remove_var("HELIX_GH");
    }
}

/// Runs the event loop until `done` holds, for jobs such as `gh` calls.
async fn wait_for(
    app: &mut Application,
    done: impl Fn(&Application) -> bool,
) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !done(app) {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out: {:?}",
            app.editor.get_status()
        );
        send_keys(app, "").await?;
    }
    Ok(())
}

/// Pull request 7 of `o/r`: branch `feature` changes file.txt from `base` to `head` and adds
/// new.txt while `main` moves on, served by a fake `gh` with `extra` cases.
struct PrFixture {
    repo: Repo,
    merge_base: String,
    head: String,
    gh: FakeGh,
    /// The directory gtc gives `:review-pr` for review batches.
    batches: TempDir,
}

impl PrFixture {
    fn new(base: &str, head: &str, extra: &str) -> PrFixture {
        let repo = Repo::new(base, base, base);
        let dir = repo.dir.path();
        let merge_base = git_ok(dir, &["rev-parse", "HEAD"]);
        git_ok(dir, &["branch", "-M", "main"]);
        // The base branch moves on after the pull request branched off.
        fs::write(dir.join("base.txt"), "base\n").unwrap();
        git_ok(dir, &["add", "base.txt"]);
        git_ok(dir, &["commit", "-m", "base moves on"]);
        let main = git_ok(dir, &["rev-parse", "HEAD"]);
        git_ok(dir, &["checkout", "-q", "-b", "feature", &merge_base]);
        repo.write(head);
        fs::write(dir.join("new.txt"), "new\n").unwrap();
        git_ok(dir, &["add", "file.txt", "new.txt"]);
        git_ok(dir, &["commit", "-m", "feature"]);
        let head = git_ok(dir, &["rev-parse", "HEAD"]);
        git_ok(dir, &["checkout", "-q", "main"]);
        let gh = FakeGh::new(&format!(
            r#"pr\ view\ 7*) echo '{{"number":7,"title":"Add new","url":"https://github.com/o/r/pull/7","headRefOid":"{head}","baseRefName":"main","baseRefOid":"{main}","isCrossRepository":false,"headRepositoryOwner":{{"login":"o"}}}}';;
repo\ view*) echo '{{"nameWithOwner":"o/r"}}';;
pr\ checkout\ 7) git checkout -q feature;;
{extra}"#
        ));
        PrFixture {
            repo,
            merge_base,
            head,
            gh,
            batches: tempfile::tempdir().unwrap(),
        }
    }

    /// The review batches written so far, oldest first, checking their names.
    fn batches(&self) -> Vec<serde_json::Value> {
        let mut names: Vec<_> = fs::read_dir(self.batches.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
            .iter()
            .map(|name| {
                let millis = name
                    .strip_prefix("review-7-")
                    .and_then(|name| name.strip_suffix(".json"))
                    .unwrap_or_else(|| panic!("unexpected file {name}"));
                assert!(millis.parse::<u128>().is_ok(), "{name}");
                let text = fs::read_to_string(self.batches.path().join(name)).unwrap();
                serde_json::from_str(&text).unwrap()
            })
            .collect()
    }

    /// Whether every `gh` call so far only read from GitHub.
    fn only_read(&self) -> bool {
        self.gh.calls().iter().all(|call| {
            ["pr view 7 ", "repo view ", "pr checkout 7"]
                .iter()
                .any(|prefix| call.starts_with(prefix))
                || call == "api repos/o/r/pulls/7/comments --paginate"
        })
    }

    /// An app on file.txt with the review statusline element, in PR mode for pull request 7.
    async fn open(&self) -> anyhow::Result<Application> {
        let mut config = test_config();
        config.editor.statusline.right = vec![StatusLineElement::Review];
        let mut app = AppBuilder::new()
            .with_file(&self.repo.file, None)
            .with_config(config)
            .build()?;
        send_keys(&mut app, &self.review_pr()).await?;
        wait_for(&mut app, |app| app.editor.review_pr.is_some()).await?;
        Ok(app)
    }

    /// The keys opening pull request 7 as gtc does, with the batch directory.
    fn review_pr(&self) -> String {
        format!(":review-pr 7 {}<ret>", self.batches.path().display())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn review_pr_checks_out_a_pull_request_and_reviews_it_against_the_merge_base(
) -> anyhow::Result<()> {
    let fixture = PrFixture::new("a\nb\n", "a\nB\n", r#"api\ *) echo '[]';;"#);
    let (fork, head, gh) = (&fixture.merge_base, &fixture.head, &fixture.gh);
    let _cwd = Cwd::enter(fixture.repo.dir.path());

    let mut app = fixture.open().await?;
    let status = app.editor.get_status().unwrap().0.to_string();
    assert_eq!(
        status,
        format!(
            "Reviewing PR #7 Add new: 2 files against main at {}",
            &fork[..8]
        )
    );
    assert_eq!(
        gh.calls()[..3],
        [
            "pr view 7 --json number,title,url,headRefOid,baseRefName,baseRefOid,isCrossRepository,headRepositoryOwner".to_owned(),
            "repo view --json nameWithOwner".to_owned(),
            "pr checkout 7".to_owned(),
        ]
    );
    wait_for(&mut app, |_| gh.calls().len() == 4).await?;
    assert_eq!(gh.calls()[3], "api repos/o/r/pulls/7/comments --paginate");
    let pr = app.editor.review_pr.clone().unwrap();
    assert_eq!(
        (pr.repo.as_str(), pr.head_oid.as_str()),
        ("o/r", head.as_str())
    );
    assert_eq!(app.editor.review_baseline.as_deref(), Some(fork.as_str()));
    assert_eq!(doc!(app.editor).text().to_string(), "a\nB\n");
    let element = |app: &Application| statusline(app);
    assert!(
        element(&app).contains(&format!(" review #7 {} ", &fork[..8])),
        "{}",
        element(&app)
    );

    send_keys(&mut app, ":review-files<ret>").await?;
    send_keys(&mut app, "<esc>").await?;
    assert_eq!(
        app.editor.get_status().unwrap().0,
        "2 files, +2 -1, 0/2 hunks staged, 2 committed"
    );

    // The hook re-sending its baseline over the socket keeps the merge base.
    app.editor.socket_command = true;
    send_keys(&mut app, ":review-baseline HEAD<ret>").await?;
    app.editor.socket_command = false;
    assert_eq!(app.editor.review_baseline.as_deref(), Some(fork.as_str()));
    assert!(app.editor.review_pr.is_some());

    send_keys(&mut app, ":review-pr close<ret>").await?;
    assert!(app.editor.review_pr.is_none());
    assert!(!element(&app).contains("#7"), "{}", element(&app));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn review_navigation_waits_for_the_pull_requests_base() -> anyhow::Result<()> {
    let fixture = PrFixture::new("a\nb\n", "a\nB\n", r#"api\ *) echo '[]';;"#);
    let dir = fixture.repo.dir.path();
    let _cwd = Cwd::enter(dir);
    let gate = fixture.gh._dir.path().join("checkout-gate");
    let started = fixture.gh._dir.path().join("checkout-started");
    assert!(Command::new("mkfifo")
        .args([&gate, &started])
        .status()?
        .success());
    let gh = std::env::var("HELIX_GH")?;
    let original = fs::read_to_string(&gh)?;
    let script = original.replace(
        "git checkout -q feature",
        &format!(
            "printf started > '{}'; read ready < '{}'; git checkout -q feature",
            started.display(),
            gate.display()
        ),
    );
    fs::write(&gh, script)?;
    let mut app = AppBuilder::new().build()?;
    send_keys(&mut app, &fixture.review_pr()).await?;
    tokio::time::timeout(std::time::Duration::from_secs(5), tokio::fs::read(&started)).await??;
    send_keys(&mut app, "<space>An").await?;
    let early_base = app.editor.review_baseline.clone();
    let early_status = app.editor.get_status().map(|status| status.0.to_string());
    send_keys(&mut app, "<ret>").await?;
    let enter_base = app.editor.review_baseline.clone();
    send_keys(&mut app, &fixture.review_pr()).await?;
    let duplicate_status = app.editor.get_status().map(|status| status.0.to_string());
    fs::write(&gate, "ready\n")?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while app.editor.review_pr_loading {
        assert!(
            std::time::Instant::now() < deadline,
            "checkout did not finish"
        );
        send_keys(&mut app, "<esc>").await?;
    }
    assert!(
        early_base.is_none(),
        "navigation pinned HEAD while the PR was still loading: {early_base:?}"
    );
    assert!(early_status.is_some_and(|status| status.contains("pull request is loading")));
    assert!(enter_base.is_none());
    assert!(duplicate_status.is_some_and(|status| status.contains("pull request is loading")));
    assert!(!app.editor.review_pr_loading);
    assert_eq!(
        app.editor.review_baseline.as_deref(),
        Some(fixture.merge_base.as_str())
    );
    send_keys(&mut app, "<ret>").await?;
    assert_status_not_error(&app.editor);
    assert_eq!(
        doc!(app.editor).path(),
        Some(dir.join("file.txt").as_path())
    );
    send_keys(&mut app, ":new<ret><S-ret>").await?;
    assert_status_not_error(&app.editor);
    assert_eq!(doc!(app.editor).path(), Some(dir.join("new.txt").as_path()));
    assert_eq!(git_ok(dir, &["diff", "--cached"]), "");
    send_keys(&mut app, ":review-pr close<ret>").await?;
    fs::write(
        &gh,
        original.replace(
            "git checkout -q feature",
            "echo checkout-failed >&2; exit 1",
        ),
    )?;
    send_keys(&mut app, &fixture.review_pr()).await?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while app.editor.review_pr_loading {
        assert!(
            std::time::Instant::now() < deadline,
            "failed checkout did not finish"
        );
        send_keys(&mut app, "<esc>").await?;
    }
    assert!(app.editor.review_pr.is_none());
    assert_error(&app, "checkout-failed");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn review_mark_pull_request_old_deletion_advances_without_staging() -> anyhow::Result<()> {
    let fixture = PrFixture::new("keep\ngone\nend\n", "keep\nend\n", r#"api\ *) echo '[]';;"#);
    let dir = fixture.repo.dir.path();
    let _cwd = Cwd::enter(dir);
    let mut app = fixture.open().await?;
    let index = git_ok(dir, &["write-tree"]);
    send_keys(&mut app, ":review-old<ret>vl:review-mark<ret>").await?;
    assert_status_not_error(&app.editor);
    assert_eq!(
        doc!(app.editor).path().unwrap().file_name().unwrap(),
        "new.txt"
    );
    let previous = app.editor.document_by_path(&fixture.repo.file).unwrap();
    assert!(previous.review.as_ref().unwrap().hunk_reviewed(0));
    send_keys(&mut app, "<ret>").await?;
    assert!(app
        .editor
        .get_status()
        .unwrap()
        .0
        .contains("changes reviewed"));
    send_keys(
        &mut app,
        ":review-prev<ret>:review-old<ret>:review-unmark<ret>",
    )
    .await?;
    assert_eq!(doc!(app.editor).path(), Some(fixture.repo.file.as_path()));
    assert!(old_focus(&app).is_some());
    assert!(!doc!(app.editor).review.as_ref().unwrap().hunk_reviewed(0));
    assert_eq!(git_ok(dir, &["write-tree"]), index);
    assert_eq!(git_ok(dir, &["diff", "--cached"]), "");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn review_mark_committed_deleted_file_is_read_only_and_not_recreated() -> anyhow::Result<()> {
    let repo = Repo::new("before\n", "before\n", "before\n");
    let dir = repo.dir.path();
    let _cwd = Cwd::enter(dir);
    let deleted = dir.join("gone.txt");
    fs::write(&deleted, "old deleted text\n")?;
    git_ok(dir, &["add", "gone.txt"]);
    git_ok(dir, &["commit", "-m", "baseline"]);
    let baseline = git_ok(dir, &["rev-parse", "HEAD"]);
    repo.write("after\n");
    git_ok(dir, &["rm", "gone.txt"]);
    git_ok(dir, &["add", "file.txt"]);
    git_ok(dir, &["commit", "-m", "agent work"]);
    let index = git_ok(dir, &["write-tree"]);
    let mut app = repo.app()?;
    send_keys(
        &mut app,
        &format!(":review-baseline {baseline}<ret>:review-mark<ret>"),
    )
    .await?;
    assert_status_not_error(&app.editor);
    assert_eq!(doc!(app.editor).path(), Some(deleted.as_path()));
    assert!(doc!(app.editor).readonly);
    assert_eq!(doc!(app.editor).text().len_bytes(), 0);
    send_keys(&mut app, ":review-old<ret>:review-mark<ret>").await?;
    assert_status_not_error(&app.editor);
    assert!(app
        .editor
        .get_status()
        .unwrap()
        .0
        .contains("changes reviewed"));
    assert!(!deleted.exists());
    assert_eq!(git_ok(dir, &["write-tree"]), index);
    send_keys(&mut app, ":review-prev<ret>:review-next<ret>").await?;
    assert_eq!(doc!(app.editor).path(), Some(deleted.as_path()));
    assert!(doc!(app.editor).review.as_ref().unwrap().hunk_reviewed(0));
    // Rebase the same empty review buffer without trying to reload its absent path.
    let head = git_ok(dir, &["rev-parse", "HEAD"]);
    send_keys(&mut app, ":review-baseline HEAD<ret>").await?;
    assert_status_not_error(&app.editor);
    let review = doc!(app.editor).review.as_ref().unwrap();
    assert_eq!(review.baseline().revision(), Some(head.as_str()));
    assert_eq!(review.reviewed_hunks(), 0);
    assert_eq!(review.hunk_count(), 0);
    assert!(!deleted.exists());
    assert_eq!(git_ok(dir, &["write-tree"]), index);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn review_next_and_enter_walk_the_committed_hunks_of_a_pull_request() -> anyhow::Result<()> {
    let fixture = PrFixture::new(
        "a\nb\nc\nd\ne\nf\n",
        "a\nB\nc\nd\ne\nF\n",
        r#"api\ *) echo '[]';;"#,
    );
    let dir = fixture.repo.dir.path();
    let _cwd = Cwd::enter(dir);
    let mut app = fixture.open().await?;
    let status = |app: &Application| app.editor.get_status().unwrap().0.to_string();
    let at = |app: &Application| {
        let (view, doc) = current_ref!(app.editor);
        let name = doc.path().unwrap().file_name().unwrap().to_owned();
        let line = doc
            .selection(view.id)
            .primary()
            .cursor_line(doc.text().slice(..));
        (name.into_string().unwrap(), line)
    };

    // A local suggestion must not hide the PR's committed changes.
    fs::write(dir.join("new.txt"), "suggested\n")?;
    app.editor.socket_command = true;
    send_keys(&mut app, ":review-baseline HEAD<ret>").await?;
    app.editor.socket_command = false;
    assert_eq!(
        app.editor.review_baseline.as_deref(),
        Some(fixture.merge_base.as_str())
    );

    // file.txt's hunks are on lines 2 and 6 (1-based); new.txt is new.
    send_keys(&mut app, "gg<space>An").await?;
    assert_eq!(at(&app), ("file.txt".to_owned(), 1));
    assert_eq!(status(&app), "file.txt: hunk 1 of 2");
    send_keys(&mut app, "<space>An").await?;
    assert_eq!(at(&app), ("file.txt".to_owned(), 5));
    assert_eq!(status(&app), "file.txt: hunk 2 of 2");
    send_keys(&mut app, "<space>An").await?;
    assert_eq!(at(&app), ("new.txt".to_owned(), 0));
    assert_eq!(status(&app), "new.txt: hunk 1 of 1");
    send_keys(&mut app, "<space>AN").await?;
    assert_eq!(at(&app), ("file.txt".to_owned(), 5));
    assert_eq!(status(&app), "file.txt: hunk 2 of 2");

    // Enter records this change as reviewed and advances; Shift-Enter revisits it.
    send_keys(&mut app, "<ret>").await?;
    assert_status_not_error(&app.editor);
    assert_eq!(at(&app), ("new.txt".to_owned(), 0));
    assert!(status(&app).contains("Marked reviewed; next"));
    assert_eq!(git_ok(dir, &["diff", "--cached"]), "");
    send_keys(&mut app, "<S-ret>").await?;
    assert_eq!(at(&app), ("file.txt".to_owned(), 5));
    assert!(status(&app).contains("reviewed; 1/2 reviewed"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn review_next_and_enter_walk_a_pull_request_with_a_committed_rename() -> anyhow::Result<()> {
    let mut fixture = PrFixture::new("a\nb\n", "a\nB\n", r#"api\ *) echo '[]';;"#);
    let dir = fixture.repo.dir.path();
    let _cwd = Cwd::enter(dir);
    git_ok(dir, &["checkout", "-q", "feature"]);
    git_ok(dir, &["mv", "file.txt", "renamed.txt"]);
    git_ok(dir, &["commit", "-m", "rename"]);
    let head = git_ok(dir, &["rev-parse", "HEAD"]);
    let gh = std::env::var("HELIX_GH")?;
    fs::write(&gh, fs::read_to_string(&gh)?.replace(&fixture.head, &head))?;
    fixture.head = head;
    git_ok(dir, &["checkout", "-q", "main"]);

    let mut app = fixture.open().await?;
    app.editor.socket_command = true;
    send_keys(&mut app, ":review-baseline HEAD force<ret>").await?;
    app.editor.socket_command = false;
    assert_eq!(
        app.editor.review_baseline.as_deref(),
        Some(fixture.merge_base.as_str())
    );
    assert_eq!(git_ok(dir, &["status", "--porcelain"]), "");
    send_keys(&mut app, "<space>An").await?;
    assert_status_not_error(&app.editor);
    assert_eq!(doc!(app.editor).path(), Some(dir.join("new.txt").as_path()));
    send_keys(&mut app, "<ret>").await?;
    assert_eq!(
        doc!(app.editor).path(),
        Some(dir.join("renamed.txt").as_path())
    );
    send_keys(&mut app, "<S-ret>").await?;
    assert_eq!(doc!(app.editor).path(), Some(dir.join("new.txt").as_path()));
    assert_eq!(git_ok(dir, &["diff", "--cached"]), "");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn review_pr_shows_comment_threads_and_keeps_pr_comments_out_of_the_mailbox(
) -> anyhow::Result<()> {
    // Base line 2 (b) is replaced; line 4 (d) has a thread with a reply; line 5 an outdated one.
    let threads = r#"[
{"id":1,"in_reply_to_id":null,"path":"file.txt","line":4,"original_line":4,"start_line":null,"side":"RIGHT","body":"why d?\nreally","user":{"login":"alice"},"created_at":"t","commit_id":"c"},
{"id":2,"in_reply_to_id":null,"path":"file.txt","line":2,"original_line":2,"start_line":null,"side":"LEFT","body":"keep b","user":{"login":"bob"},"created_at":"t","commit_id":"c"},
{"id":3,"in_reply_to_id":1,"path":"file.txt","line":4,"original_line":4,"start_line":null,"side":"RIGHT","body":"because","user":{"login":"carol"},"created_at":"t","commit_id":"c"}
][
{"id":4,"in_reply_to_id":null,"path":"file.txt","line":null,"original_line":5,"start_line":null,"side":"RIGHT","body":"old note","user":{"login":"dave"},"created_at":"t","commit_id":"c"}
]"#;
    let fixture = PrFixture::new(
        "a\nb\nc\nd\ne\n",
        "a\nB\nc\nd\ne\n",
        &format!("api\\ repos/o/r/pulls/7/comments*) cat <<'EOF'\n{threads}\nEOF\n;;"),
    );
    let dir = fixture.repo.dir.path();
    let _cwd = Cwd::enter(dir);
    let outbox = dir.join(".pi/helix-review/outbox");
    fs::create_dir_all(&outbox)?;
    let mut app = fixture.open().await?;
    wait_for(&mut app, |app| app.editor.review_pr_threads.len() == 3).await?;

    let rows: Vec<String> = render(&app).into_iter().map(|(_, row)| row).collect();
    let at = |text: &str| {
        rows.iter()
            .position(|row| row == text)
            .unwrap_or_else(|| panic!("{text:?} not in {rows:#?}"))
    };
    // Each thread sits right above its line; the base-side one above the change that replaced
    // its line, the outdated one at its original line.
    assert_eq!(at("bob │ keep b (old side)") + 1, at("B"));
    assert_eq!(at("alice │ why d?") + 3, at("d"));
    assert_eq!(at("      │ really") + 2, at("d"));
    assert_eq!(at("  carol │ because") + 1, at("d"));
    assert_eq!(at("dave │ old note (outdated)") + 1, at("e"));

    // Space A C lists the threads; picking one jumps to its line.
    send_keys(&mut app, "<space>AC").await?;
    send_keys(&mut app, "keep b<ret>").await?;
    let (view, doc) = current_ref!(app.editor);
    let text = doc.text().slice(..);
    assert_eq!(doc.selection(view.id).primary().cursor_line(text), 1);

    // Space A c on a thread's line replies to it, and PR comments stay out of pi's batches.
    send_keys(&mut app, "4gg<space>Acsure<ret>gg:review-comment top<ret>").await?;
    let replies: Vec<_> = app
        .editor
        .review_pr_comments
        .iter()
        .map(|comment| comment.reply_to)
        .collect();
    assert_eq!(replies, [Some(1), None]);
    assert!(app.editor.review_batch.comments().is_empty());
    // The write finishes before the explicit handoff, which refuses unsaved buffers.
    send_keys(&mut app, "ix<esc>:w<ret>").await?;
    send_keys(&mut app, ":review-send<ret>").await?;
    assert_status_not_error(&app.editor);
    let batches: Vec<_> = fs::read_dir(&outbox)?.collect::<Result<_, _>>()?;
    assert_eq!(batches.len(), 1, "{batches:?}");
    let json: serde_json::Value = serde_json::from_str(&fs::read_to_string(batches[0].path())?)?;
    assert_eq!(json["comments"], serde_json::json!([]));
    assert_eq!(app.editor.review_pr_comments.len(), 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn n_mapped_to_search_or_review_follows_whichever_was_used_last() -> anyhow::Result<()> {
    let lines = (1..=20).map(|n| format!("line {n}\n")).collect::<String>();
    let changed = lines
        .replace("line 4\n", "word 4\n")
        .replace("line 16\n", "word 16\n");
    let repo = Repo::new(&lines, &lines, &changed);
    let dir = repo.dir.path();
    let _cwd = Cwd::enter(dir);
    fs::write(dir.join("other.txt"), "x\n")?;
    git_ok(dir, &["add", "other.txt"]);
    git_ok(dir, &["commit", "-m", "other"]);
    fs::write(dir.join("other.txt"), "X\n")?;
    let mut config = Config::load(
        Ok(&"[keys.normal]\nn = \"search_next_or_review_next\"\n".to_owned()),
        Err(ConfigLoadError::default()),
    )
    .map_err(|err| anyhow::anyhow!("{err}"))?;
    config.editor = test_editor_config();
    let mut app = AppBuilder::new()
        .with_config(config)
        .with_file(&repo.file, None)
        .build()?;
    let line = |app: &Application| {
        let (view, doc) = current_ref!(app.editor);
        doc.selection(view.id)
            .primary()
            .cursor_line(doc.text().slice(..))
    };

    // Nothing used yet: n walks the review hunks.
    send_keys(&mut app, "n").await?;
    assert_eq!(line(&app), 3);
    // With a search, n finds the next match instead of the next hunk.
    // Searching from the top skips the match under the cursor: "line 10" is the first.
    send_keys(&mut app, "gg/line 1<ret>").await?;
    assert_eq!(line(&app), 9);
    send_keys(&mut app, "n").await?;
    assert_eq!(line(&app), 10);
    // A review jump (the search is still set) makes n walk the hunks again.
    send_keys(&mut app, "<space>An").await?;
    assert_eq!(line(&app), 15);
    send_keys(&mut app, "ggn").await?;
    assert_eq!(line(&app), 3);
    // And a new search makes it follow the matches again.
    send_keys(&mut app, "/line 1<ret>n").await?;
    assert_eq!(line(&app), 10);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn staging_moves_on_to_the_next_unstaged_hunk_across_files() -> anyhow::Result<()> {
    let lines = (1..=20).map(|n| format!("{n}\n")).collect::<String>();
    let changed = (1..=20)
        .map(|n| match n {
            3 | 9 | 15 => format!("changed {n}\n"),
            n => format!("{n}\n"),
        })
        .collect::<String>();
    let repo = Repo::new(&lines, &lines, &changed);
    let dir = repo.dir.path();
    let _cwd = Cwd::enter(dir);
    fs::write(dir.join("other.txt"), "x\ny\n")?;
    git_ok(dir, &["add", "other.txt"]);
    git_ok(dir, &["commit", "-m", "other"]);
    // A different size: a same-size rewrite within the second of the commit can look
    // unchanged by its index stat once Helix rewrites the index (racy git).
    fs::write(dir.join("other.txt"), "XX\ny\n")?;
    let mut app = repo.app()?;
    let status = |app: &Application| app.editor.get_status().unwrap().0.to_string();
    let at = |app: &Application| {
        let (view, doc) = current_ref!(app.editor);
        let name = doc
            .path()
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        (
            name,
            doc.selection(view.id)
                .primary()
                .cursor_line(doc.text().slice(..)),
        )
    };
    send_keys(&mut app, ":review-baseline HEAD<ret>").await?;

    send_keys(&mut app, "3gg<ret>").await?;
    assert_eq!(at(&app), ("file.txt".to_owned(), 8));
    assert_eq!(status(&app), "Staged 1 hunk; 1 of 3 staged in file.txt");
    send_keys(&mut app, "<ret>").await?;
    assert_eq!(at(&app), ("file.txt".to_owned(), 14));
    send_keys(&mut app, "<ret>").await?;
    assert_eq!(at(&app), ("other.txt".to_owned(), 0));
    assert_eq!(
        status(&app),
        "Staged 1 hunk; 3 of 3 staged in file.txt; next: other.txt hunk 1 of 1"
    );
    // Nothing unstaged is left anywhere: the cursor stays.
    send_keys(&mut app, "<ret>").await?;
    assert_eq!(at(&app), ("other.txt".to_owned(), 0));
    assert_eq!(status(&app), "Staged 1 hunk; 1 of 1 staged in other.txt");
    assert_eq!(git_ok(dir, &["diff", "--name-only"]), "");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn enter_stays_on_a_hunk_it_unstages() -> anyhow::Result<()> {
    let lines = (1..=20).map(|n| format!("{n}\n")).collect::<String>();
    let changed = (1..=20)
        .map(|n| match n {
            9 | 15 => format!("changed {n}\n"),
            n => format!("{n}\n"),
        })
        .collect::<String>();
    let repo = Repo::new(&lines, &lines, &changed);
    let dir = repo.dir.path();
    let _cwd = Cwd::enter(dir);
    let mut app = repo.app()?;
    let status = |app: &Application| app.editor.get_status().unwrap().0.to_string();
    let line = |app: &Application| {
        let (view, doc) = current_ref!(app.editor);
        doc.selection(view.id)
            .primary()
            .cursor_line(doc.text().slice(..))
    };
    send_keys(&mut app, ":review-baseline HEAD<ret>").await?;

    send_keys(&mut app, "9gg<ret>").await?;
    assert_eq!(line(&app), 14);
    assert_eq!(status(&app), "Staged 1 hunk; 1 of 2 staged in file.txt");
    send_keys(&mut app, "N").await?;
    assert_eq!(line(&app), 8);
    send_keys(&mut app, "<ret>").await?;
    assert_eq!(line(&app), 8);
    assert_eq!(status(&app), "Unstaged 1 hunk; 0 of 2 staged in file.txt");
    assert_eq!(git_ok(dir, &["diff", "--cached"]), "");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn review_submit_hands_comments_and_replies_to_gtc() -> anyhow::Result<()> {
    let base: String = "abcdefghijkl".chars().map(|c| format!("{c}\n")).collect();
    let head = base.replacen("b\n", "B\n", 1);
    let threads = r#"[{"id":1,"in_reply_to_id":null,"path":"file.txt","line":8,"original_line":8,"start_line":null,"side":"RIGHT","body":"why h?","user":{"login":"alice"},"created_at":"t","commit_id":"c"}]"#;
    let extra = format!(r#"api\ repos/o/r/pulls/7/comments*) echo '{threads}';;"#);
    let fixture = PrFixture::new(&base, &head, &extra);
    let dir = fixture.repo.dir.path();
    let _cwd = Cwd::enter(dir);
    let mut app = fixture.open().await?;
    wait_for(&mut app, |app| app.editor.review_pr_threads.len() == 1).await?;

    // A comment on lines 2-3 and a reply to the thread on line 8.
    send_keys(&mut app, "2ggxx:review-comment two lines<ret>").await?;
    send_keys(&mut app, "<esc>8gg<space>Acthanks<ret>").await?;
    assert_eq!(app.editor.review_pr_comments.len(), 2);

    // A batch that cannot be written keeps everything pending.
    fs::remove_dir(fixture.batches.path())?;
    send_keys(&mut app, ":review-submit approve looks good<ret>").await?;
    assert_error(&app, "writing the review batch to");
    assert_eq!(app.editor.review_pr_comments.len(), 2);
    assert!(app.editor.review_pr.is_some());

    fs::create_dir(fixture.batches.path())?;
    send_keys(&mut app, ":review-submit approve looks good<ret>").await?;
    assert_eq!(
        app.editor.get_status().unwrap().0,
        "Handed the review of #7 to gtc: 1 comment, 1 reply, 0 suggestions; back on main"
    );
    assert_eq!(
        fixture.batches(),
        [serde_json::json!({
            "version": 1,
            "kind": "pull-request-review",
            "repo": "o/r",
            "number": 7,
            "head": fixture.head,
            "event": "APPROVE",
            "body": "looks good",
            "comments": [
                {
                    "path": "file.txt", "side": "RIGHT", "start_line": 2, "line": 3,
                    "body": "two lines", "reply_to": null
                },
                {
                    "path": "file.txt", "side": "RIGHT", "start_line": 8, "line": 8,
                    "body": "thanks", "reply_to": 1
                },
            ],
            "suggestions": [],
        })]
    );
    assert!(app.editor.review_pr_comments.is_empty());
    assert!(app.editor.review_pr.is_none());
    assert!(app.editor.review_baseline.is_none());
    assert!(app.editor.review_user_pin.is_none());
    assert!(app.editor.review_hook_baseline.is_none());
    assert!(app.editor.documents().all(|doc| doc.review.is_none()));
    assert_eq!(git_ok(dir, &["symbolic-ref", "--short", "HEAD"]), "main");
    assert!(fixture.only_read(), "{:?}", fixture.gh.calls());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn leaving_a_pull_request_review_checks_out_the_previous_branch() -> anyhow::Result<()> {
    let fixture = PrFixture::new("a\nb\n", "a\nB\n", r#"api\ *) echo '[]';;"#);
    let dir = fixture.repo.dir.path();
    let _cwd = Cwd::enter(dir);
    let branch = |dir: &Path| git_ok(dir, &["symbolic-ref", "--short", "HEAD"]);

    let mut app = fixture.open().await?;
    assert_eq!(branch(dir), "feature");
    send_keys(&mut app, ":review-pr close<ret>").await?;
    assert_eq!(
        app.editor.get_status().unwrap().0,
        "Stopped reviewing PR #7; back on main"
    );
    assert_eq!(branch(dir), "main");
    assert_eq!(doc!(app.editor).text().to_string(), "a\nb\n");

    // Uncommitted changes keep the pull request's branch.
    send_keys(&mut app, ":review-pr 7<ret>").await?;
    wait_for(&mut app, |app| app.editor.review_pr.is_some()).await?;
    fs::write(&fixture.repo.file, "a\nB\nx\n")?;
    send_keys(&mut app, ":review-pr close<ret>").await?;
    assert_eq!(
        app.editor.get_status().unwrap().0,
        "Stopped reviewing PR #7; kept feature checked out: the worktree has uncommitted changes"
    );
    assert_eq!(branch(dir), "feature");
    git_ok(dir, &["checkout", "-q", "--", "file.txt"]);
    git_ok(dir, &["checkout", "-q", "main"]);

    // Quitting during a review goes back too.
    send_keys(&mut app, ":reload<ret>:review-pr 7<ret>").await?;
    wait_for(&mut app, |app| app.editor.review_pr.is_some()).await?;
    assert_eq!(branch(dir), "feature");
    send_keys(&mut app, ":q<ret>").await?;
    assert!(app.close().await.is_empty());
    assert_eq!(branch(dir), "main");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn review_submit_hands_your_edits_to_gtc_as_suggestions() -> anyhow::Result<()> {
    let base: String = "abcdefghijkl".chars().map(|c| format!("{c}\n")).collect();
    let head = base.replacen("b\n", "B\n", 1);
    let fixture = PrFixture::new(&base, &head, r#"api\ *) echo '[]';;"#);
    let dir = fixture.repo.dir.path();
    let _cwd = Cwd::enter(dir);
    let mut app = fixture.open().await?;

    // Lines 2-3 are replaced and a line is inserted after line 5; a comment sits on lines 2-3.
    let edited = head
        .replacen("B\nc\n", "B2\nC\n", 1)
        .replacen("e\n", "e\nx\n", 1);
    fs::write(&fixture.repo.file, &edited)?;
    send_keys(&mut app, ":reload<ret>2ggxx:review-comment both<ret>").await?;

    send_keys(&mut app, ":review-submit comment ok<ret>").await?;
    assert_eq!(
        app.editor.get_status().unwrap().0,
        "Handed the review of #7 to gtc: 0 comments, 0 replies, 2 suggestions; back on main"
    );
    let batches = fixture.batches();
    assert_eq!(batches.len(), 1);
    assert_eq!(
        (
            &batches[0]["event"],
            &batches[0]["body"],
            &batches[0]["comments"]
        ),
        (
            &serde_json::json!("COMMENT"),
            &serde_json::json!("ok"),
            &serde_json::json!([])
        )
    );
    assert_eq!(
        batches[0]["suggestions"],
        serde_json::json!([
            {
                "path": "file.txt", "start_line": 2, "line": 3,
                "replacement": "B2\nC", "comment": "both"
            },
            {
                "path": "file.txt", "start_line": 5, "line": 5,
                "replacement": "e\nx", "comment": null
            },
        ])
    );
    // The edits were reverted before going back to main.
    assert_eq!(git_ok(dir, &["status", "--porcelain"]), "");
    assert_eq!(git_ok(dir, &["symbolic-ref", "--short", "HEAD"]), "main");
    assert_eq!(doc!(app.editor).text().to_string(), base);
    assert!(app.editor.review_pr_comments.is_empty());
    assert!(app.editor.review_pr.is_none());
    assert!(fixture.only_read(), "{:?}", fixture.gh.calls());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn space_a_m_opens_a_modal_to_pick_the_verb_and_write_the_summary() -> anyhow::Result<()> {
    let fixture = PrFixture::new("a\nb\n", "a\nB\n", r#"api\ *) echo '[]';;"#);
    let dir = fixture.repo.dir.path();
    let _cwd = Cwd::enter(dir);
    let mut app = fixture.open().await?;
    send_keys(&mut app, "2gg:review-comment fix<ret>").await?;

    // Esc cancels: the keys typed went to the modal, nothing is written and the comment waits.
    send_keys(&mut app, "<space>Amdd<esc>").await?;
    assert_eq!(doc!(app.editor).text().to_string(), "a\nB\n");
    assert!(fixture.batches().is_empty());
    assert_eq!(app.editor.review_pr_comments.len(), 1);

    // Tab selects approve.
    send_keys(&mut app, "<space>Am<tab>fine<ret>").await?;
    assert!(app.editor.review_pr.is_none());
    let batches = fixture.batches();
    assert_eq!(
        (batches[0]["event"].as_str(), batches[0]["body"].as_str()),
        (Some("APPROVE"), Some("fine"))
    );
    assert_eq!(batches[0]["comments"].as_array().map(Vec::len), Some(1));

    // Enter hands over a comment review with the typed summary; Alt-Enter and Shift-Enter break
    // lines.
    send_keys(&mut app, &fixture.review_pr()).await?;
    wait_for(&mut app, |app| app.editor.review_pr.is_some()).await?;
    send_keys(
        &mut app,
        "<space>Amlgtm<A-ret>really<S-ret>x<backspace>y<ret>",
    )
    .await?;
    assert!(app.editor.review_pr.is_none());
    let batches = fixture.batches();
    assert_eq!(
        (batches[1]["event"].as_str(), batches[1]["body"].as_str()),
        (Some("COMMENT"), Some("lgtm\nreally\ny"))
    );

    // Without a batch directory there is nobody to hand the review to.
    send_keys(&mut app, ":review-pr 7<ret>").await?;
    wait_for(&mut app, |app| app.editor.review_pr.is_some()).await?;
    send_keys(&mut app, "2gg:review-comment fix<ret><space>Am<ret>").await?;
    assert_error(
        &app,
        "no reviewer attached: open the pull request from gtc prs",
    );
    send_keys(&mut app, ":review-submit approve fine<ret>").await?;
    assert_error(
        &app,
        "no reviewer attached: open the pull request from gtc prs",
    );
    assert_eq!(app.editor.review_pr_comments.len(), 1);
    assert!(app.editor.review_pr.is_some());
    assert_eq!(fixture.batches().len(), 2);
    assert!(fixture.only_read(), "{:?}", fixture.gh.calls());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn review_pr_follows_a_rebase_and_refuses_commits_outside_the_branch() -> anyhow::Result<()> {
    let fixture = PrFixture::new("a\nb\n", "a\nB\n", r#"api\ *) echo '[]';;"#);
    let dir = fixture.repo.dir.path();
    let _cwd = Cwd::enter(dir);
    let mut app = fixture.open().await?;
    assert_eq!(
        app.editor.review_baseline.as_deref(),
        Some(fixture.merge_base.as_str())
    );

    // The pull request branch is rebased onto the moved-on base branch.
    git_ok(dir, &["rebase", "-q", "main"]);
    let main = git_ok(dir, &["rev-parse", "main"]);
    send_keys(&mut app, ":reload<ret>:review-refresh<ret>").await?;
    assert_eq!(app.editor.review_baseline.as_deref(), Some(main.as_str()));
    assert_eq!(app.editor.review_pr.as_ref().unwrap().merge_base, main);
    assert_eq!(
        app.editor.get_status().unwrap().0.to_string(),
        format!("Baseline moved to {} after a rebase", &main[..8])
    );

    // The pre-rebase head is no longer in the branch's history, from the socket or the user.
    app.editor.socket_command = true;
    send_keys(&mut app, &format!(":review-baseline {}<ret>", fixture.head)).await?;
    app.editor.socket_command = false;
    assert_error(
        &app,
        &format!(
            "commit {} is not in this branch's history",
            &fixture.head[..8]
        ),
    );
    send_keys(&mut app, &format!(":review-baseline {}<ret>", fixture.head)).await?;
    assert_error(&app, "is not in this branch's history");
    assert_eq!(app.editor.review_baseline.as_deref(), Some(main.as_str()));
    assert!(app.editor.review_pr.is_some());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn enter_off_a_hunk_jumps_to_the_next_unstaged_one() -> anyhow::Result<()> {
    let lines = (1..=20).map(|n| format!("{n}\n")).collect::<String>();
    let changed = (1..=20)
        .map(|n| match n {
            9 | 15 => format!("changed {n}\n"),
            n => format!("{n}\n"),
        })
        .collect::<String>();
    let repo = Repo::new(&lines, &lines, &changed);
    let dir = repo.dir.path();
    let _cwd = Cwd::enter(dir);
    let mut app = repo.app()?;
    let status = |app: &Application| app.editor.get_status().unwrap().0.to_string();
    let line = |app: &Application| {
        let (view, doc) = current_ref!(app.editor);
        doc.selection(view.id)
            .primary()
            .cursor_line(doc.text().slice(..))
    };
    send_keys(&mut app, ":review-baseline HEAD<ret>").await?;
    let head = git_ok(dir, &["rev-parse", "HEAD"]);

    // On an unchanged line, Enter jumps to the next unstaged hunk without staging.
    send_keys(&mut app, "3gg<ret>").await?;
    assert_eq!(line(&app), 8);
    assert_eq!(status(&app), "next: file.txt hunk 1 of 2");
    assert_eq!(git_ok(dir, &["diff", "--cached"]), "");
    // On the hunk, Enter stages it and moves on; Space A s likewise.
    send_keys(&mut app, "<ret>").await?;
    assert_eq!(line(&app), 14);
    send_keys(&mut app, "<space>As").await?;
    assert_eq!(git_ok(dir, &["diff", "--name-only"]), "");
    // With everything staged, an unchanged line has nowhere to go.
    send_keys(&mut app, "gg<ret>").await?;
    assert_error(&app, &format!("nothing unstaged since {}", &head[..8]));
    // The typable command keeps its own error.
    send_keys(&mut app, ":review-stage<ret>").await?;
    assert_error(&app, "nothing to stage here");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn space_a_b_picks_the_baseline_from_the_branch_history() -> anyhow::Result<()> {
    let repo = Repo::new("a\nb\nc\n", "a\nb\nc\n", "a\nb\nc\n");
    let dir = repo.dir.path();
    let _cwd = Cwd::enter(dir);
    let first = git_ok(dir, &["rev-parse", "HEAD"]);
    git_ok(dir, &["branch", "-M", "main"]);
    git_ok(dir, &["checkout", "-q", "-b", "topic"]);
    let commit = |content: &str, subject: &str| {
        repo.write(content);
        repo.stage();
        git_ok(dir, &["commit", "-m", subject]);
        git_ok(dir, &["rev-parse", "HEAD"])
    };
    let second = commit("A\nb\nc\n", "second");
    let third = commit("A\nB\nc\n", "third");
    let mut config = test_config();
    config.editor.statusline.right = vec![StatusLineElement::Review];
    let mut app = AppBuilder::new()
        .with_file(&repo.file, None)
        .with_config(config)
        .build()?;
    let baseline = |app: &Application| app.editor.review_baseline.clone().unwrap_or_default();
    send_keys(&mut app, &format!(":review-baseline {second}<ret>")).await?;

    // The marker column names the current baseline, the merge base with main, and HEAD.
    send_keys(&mut app, "<space>Ab").await?;
    send_keys(&mut app, "%mark HEAD<ret>").await?;
    assert_eq!(baseline(&app), third);
    send_keys(&mut app, "<space>Ab").await?;
    send_keys(&mut app, "%mark ●<ret>").await?;
    assert_eq!(baseline(&app), third);
    send_keys(&mut app, "<space>Ab").await?;
    send_keys(&mut app, "%mark base<ret>").await?;
    assert_eq!(baseline(&app), first);
    assert_eq!(
        app.editor.get_status().unwrap().0.to_string(),
        format!("Baseline {} head (2 behind HEAD)", &first[..8])
    );
    let status = statusline(&app);
    // The reviewer chose it, and the hook keeps away from it.
    assert!(
        status.contains(&format!(" review {}* ", &first[..8])),
        "{status}"
    );
    app.editor.socket_command = true;
    send_keys(&mut app, ":review-baseline HEAD<ret>").await?;
    app.editor.socket_command = false;
    assert_eq!(baseline(&app), first);
    let status = statusline(&app);
    assert!(
        status.contains(&format!(" review {}* ", &first[..8])),
        "{status}"
    );
    // The row marked pi is the hook's baseline: picking it ends the pin.
    send_keys(&mut app, "<space>Ab").await?;
    send_keys(&mut app, "%mark pi<ret>").await?;
    assert_eq!(baseline(&app), third);
    assert!(app.editor.review_user_pin.is_none());
    let status = statusline(&app);
    assert!(
        status.contains(&format!(" review {} ", &third[..8])),
        "{status}"
    );

    // A forced hook baseline (after a commit the reviewer asked for) releases a pin.
    send_keys(&mut app, "<space>Ab").await?;
    send_keys(&mut app, "%mark base<ret>").await?;
    assert_eq!(baseline(&app), first);
    assert!(app.editor.review_user_pin.is_some());
    app.editor.socket_command = true;
    send_keys(&mut app, &format!(":review-baseline {third} force<ret>")).await?;
    assert_eq!(baseline(&app), third);
    assert!(app.editor.review_user_pin.is_none());
    let status = statusline(&app);
    assert!(
        status.contains(&format!(" review {} ", &third[..8])),
        "{status}"
    );
    // The plain form afterwards still follows the hook.
    send_keys(&mut app, &format!(":review-baseline {second}<ret>")).await?;
    app.editor.socket_command = false;
    assert_eq!(baseline(&app), second);
    send_keys(&mut app, ":review-baseline HEAD sometimes<ret>").await?;
    assert_error(&app, "only a trailing 'force'");

    // Space A B still refreshes the buffer's review.
    send_keys(&mut app, "<space>AB").await?;
    assert!(app
        .editor
        .get_status()
        .unwrap()
        .0
        .starts_with("Review refreshed"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_space_a_menu_runs_review_commands_next_to_user_keys() -> anyhow::Result<()> {
    let repo = Repo::new("one\ntwo\n", "one\ntwo\n", "one\nTWO\n");
    let _cwd = Cwd::enter(repo.dir.path());
    // A user table under Space A, as in the reviewer's config, merges with the built-in menu.
    let mut config = Config::load(
        Ok(&"[keys.normal.space.A]\nl = \"extend_char_right\"\n".to_owned()),
        Err(ConfigLoadError::default()),
    )
    .map_err(|err| anyhow::anyhow!("{err}"))?;
    config.editor = test_editor_config();
    let mut app = AppBuilder::new()
        .with_config(config)
        .with_file(&repo.file, None)
        .build()?;

    // Space A f opens the review-files picker, which pins HEAD; picking opens the file reviewed.
    send_keys(&mut app, "<space>Af").await?;
    send_keys(&mut app, "file.txt<ret>").await?;
    assert_status_not_error(&app.editor);
    let doc = doc!(app.editor);
    assert!(doc.review.is_some());
    let (view, doc) = current_ref!(app.editor);
    let text = doc.text().slice(..);
    assert_eq!(doc.selection(view.id).primary().cursor_line(text), 1);

    // The user's Space A l still extends, and Space A c (like Space A a) sends a comment to the
    // agent at once.
    send_keys(&mut app, "gg<space>Al").await?;
    let (view, doc) = current_ref!(app.editor);
    assert_eq!(doc.selection(view.id).primary().to(), 2);
    let outbox = tempfile::tempdir()?;
    let handoff = format!(":review-handoff {}<ret>", outbox.path().display());
    send_keys(&mut app, &handoff).await?;
    for (keys, comment) in [
        ("<space>Aclooks good<ret>", "looks good"),
        ("<space>Aaand this<ret>", "and this"),
    ] {
        send_keys(&mut app, keys).await?;
        assert_eq!(
            app.editor.get_status().unwrap().0,
            "Sent 1 comment to the agent"
        );
        let batches = take_batches(outbox.path());
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0]["request"], serde_json::json!({ "kind": "ask" }));
        assert_eq!(batches[0]["comments"][0]["comment"], comment);
        assert_eq!(batches[0]["comments"][0]["selected_text"], "on");
    }
    assert!(app.editor.review_batch.comments().is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn review_ask_writes_the_buffer_and_hands_off_every_selection_once() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let repo = dir.path();
    git_ok(repo, &["init"]);
    let file = repo.join("lib.rs");
    let source = "fn alpha() {\n    let a = 1;\n    let b = 2;\n}\n\nfn beta() {}\n";
    let other = repo.join("other.rs");
    fs::write(&file, source)?;
    fs::write(&other, "fn unrelated() {}\n")?;
    git_ok(repo, &["add", "lib.rs", "other.rs"]);
    git_ok(repo, &["commit", "-m", "head"]);
    let _cwd = Cwd::enter(repo);
    let outbox = tempfile::tempdir()?;
    let mut app = AppBuilder::new().with_file(&file, None).build()?;

    send_keys(&mut app, ":review-baseline HEAD<ret>:o other.rs<ret>:review HEAD<ret>i// unfinished<esc>:o lib.rs<ret>").await?;

    // An edit leaves the buffer modified; lines 2 and 3 are selected separately.
    let keys = format!(
        ":review-handoff {}<ret>geo// end<esc>2ggxC:review-ask rename<ret>",
        outbox.path().display()
    );
    send_keys(&mut app, &keys).await?;
    assert_status_not_error(&app.editor);
    assert!(!doc!(app.editor).is_modified());
    assert!(fs::read_to_string(&file)?.ends_with("// end\n"));

    let batches: Vec<_> = fs::read_dir(outbox.path())?
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(batches.len(), 1, "{batches:?}");
    let json: serde_json::Value = serde_json::from_str(&fs::read_to_string(&batches[0])?)?;
    assert_eq!(json["request"], serde_json::json!({ "kind": "ask" }));
    assert_eq!(json["files"].as_array().unwrap().len(), 1);
    assert_eq!(json["files"][0]["path"], file.to_string_lossy().as_ref());
    assert!(app.editor.review_batch.files().any(|path| path == other));
    let mut comments = json["comments"].as_array().unwrap().clone();
    assert_eq!(comments.len(), 2, "{comments:?}");
    // The primary selection (the copy on line 3) comes first.
    assert_eq!(comments[0]["start_line"], 3);
    comments.sort_by_key(|comment| comment["start_line"].as_u64());
    for (comment, selected) in comments.iter().zip(["let a = 1;", "let b = 2;"]) {
        assert_eq!(comment["comment"], "rename");
        assert!(comment["selected_text"]
            .as_str()
            .unwrap()
            .contains(selected));
        let enclosing = comment["enclosing_text"].as_str().unwrap();
        assert!(enclosing.starts_with("fn alpha() {"), "{enclosing}");
        assert!(!enclosing.contains("beta"), "{enclosing}");
    }
    assert!(app.editor.review_batch.comments().is_empty());
    // A saved-file comment is not blocked by another reviewed buffer's unsaved edits.
    send_keys(&mut app, ":review-comment follow up<ret>").await?;
    assert_status_not_error(&app.editor);
    assert_eq!(fs::read_dir(outbox.path())?.count(), 2);
    send_keys(&mut app, ":review-send full review<ret>").await?;
    assert_error(&app, "reviewed buffers have unsaved changes");
    send_keys(&mut app, ":o other.rs<ret>:w<ret>").await?;
    send_keys(&mut app, ":review-send full review<ret>").await?;
    assert_status_not_error(&app.editor);
    let mut batches = fs::read_dir(outbox.path())?
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    batches.sort();
    let full: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(batches.last().unwrap())?)?;
    assert!(full["files"]
        .as_array()
        .unwrap()
        .iter()
        .any(|file| file["path"] == other.to_string_lossy().as_ref()));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn repeating_the_baseline_only_reviews_buffers_changed_on_disk() -> anyhow::Result<()> {
    let repo = Repo::new("a\nb\n", "a\nb\n", "a\nB\n");
    let dir = repo.dir.path();
    let other = dir.join("other.txt");
    fs::write(&other, "x\ny\n")?;
    git_ok(dir, &["add", "other.txt"]);
    git_ok(dir, &["commit", "-m", "other"]);
    fs::write(&other, "x\nY\n")?;
    let commit = git_ok(dir, &["rev-parse", "HEAD"]);
    let short = commit[..8].to_owned();
    let _cwd = Cwd::enter(dir);
    let mut app = repo.app()?;

    send_keys(&mut app, ":o other.txt<ret>j:review-baseline HEAD<ret>").await?;
    assert_eq!(
        app.editor.get_status().unwrap().0.as_ref(),
        format!("Reviewing 2 buffers against {short}")
    );
    let other_state = |app: &Application| {
        let doc = app.editor.document_by_path(&other).unwrap();
        let (_, selection) = doc.selections().iter().next().unwrap();
        (doc.text().to_string(), doc.version(), selection.clone())
    };
    let before = other_state(&app);

    repo.write_external("a\nBB\n");
    // Skipping is decided by modification time: other.txt is rewritten but keeps its time,
    // so it is not reloaded.
    let mtime = fs::metadata(&other)?.modified()?;
    fs::write(&other, "rewritten\n")?;
    fs::File::options()
        .write(true)
        .open(&other)?
        .set_times(fs::FileTimes::new().set_modified(mtime))?;
    send_keys(&mut app, ":review-baseline HEAD<ret>").await?;

    assert_eq!(
        app.editor.get_status().unwrap().0.as_ref(),
        format!("Reviewing 1 buffer against {short}; 1 unchanged")
    );
    assert_eq!(other_state(&app), before);
    let file = app.editor.document_by_path(&repo.file).unwrap();
    assert_eq!(file.text(), "a\nBB\n");
    let review = file.review.as_ref().unwrap();
    assert_eq!(review.baseline().revision(), Some(commit.as_str()));
    assert_eq!(review.changed_lines().collect::<Vec<_>>(), [1..2]);

    // With nothing changed on disk, every buffer is up to date.
    send_keys(&mut app, ":review-baseline HEAD<ret>").await?;
    assert_eq!(
        app.editor.get_status().unwrap().0.as_ref(),
        format!("2 buffers up to date against {short}")
    );
    assert_eq!(other_state(&app), before);

    // A different baseline reviews every buffer again.
    send_keys(&mut app, ":review-baseline HEAD~1<ret>").await?;
    let (status, _) = app.editor.get_status().unwrap();
    assert!(
        status.starts_with("Reviewing 2 buffers against "),
        "{status}"
    );
    assert_eq!(
        app.editor.document_by_path(&other).unwrap().text(),
        "rewritten\n"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn commit_review_handles_a_file_introduced_after_the_baseline() -> anyhow::Result<()> {
    let repo = Repo::new("initial\n", "initial\n", "initial\n");
    git_ok(repo.dir.path(), &["rm", "file.txt"]);
    git_ok(repo.dir.path(), &["commit", "-m", "without file"]);
    let commit_oid = git_ok(repo.dir.path(), &["rev-parse", "HEAD"]);
    repo.write("added later\n");
    repo.stage();
    git_ok(repo.dir.path(), &["commit", "-m", "add file"]);
    let outbox = tempfile::tempdir()?;
    let keys = format!(
        ":review-handoff {}<ret>:review HEAD~1<ret>x:review-comment new file<ret>",
        outbox.path().display()
    );
    test_key_sequences(&mut repo.app()?, vec![(Some(&keys), Some(&|app| {
        assert_status_not_error(&app.editor);
        assert_eq!(doc!(app.editor).text(), "added later\n");
        assert!(doc!(app.editor).review.as_ref().unwrap().blocks().is_empty());
        let json = &take_batches(outbox.path())[0];
        assert_eq!(json["files"][0]["baseline"], serde_json::json!({"kind":"git-commit", "commit_oid":commit_oid, "blob_oid":null}));
        assert_eq!(json["comments"][0]["baseline"], json["files"][0]["baseline"]);
        assert_eq!(json["comments"][0]["side"], "current");
        assert_eq!(repo.disk(), "added later\n");
    }))], false).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn comments_export_as_one_batch() -> anyhow::Result<()> {
    let repo = Repo::new(INDEX, INDEX, DISK);
    let existing = repo.dir.path().join("existing.json");
    fs::write(&existing, "do not touch")?;
    let batch = repo.dir.path().join("batch.json");
    let file = helix_stdx::path::canonicalize(&repo.file);
    let pending = |app: &Application| app.editor.review_batch.comments().len();
    let commented_oid = repo.index_oid();
    let staged = "keep1\nkeep2\nstaged\n";

    let export_existing = format!(":review-export {}<ret>", existing.display());
    let export_unsaved = format!("ix<esc>:review-export {}<ret>", batch.display());
    let export = format!("u:review-export {}<ret>", batch.display());
    let mut app = repo.app()?;
    // Comments that cannot be sent stay pending.
    break_handoff(&mut app, repo.dir.path()).await?;
    test_key_sequences(
        &mut app,
        vec![
            (
                Some(":review<ret>:review-old<ret>x:review-comment why gone?<ret><esc>x:review-comment keep this<ret>"),
                Some(&|app| {
                    assert_eq!(pending(app), 2);
                    repo.write(staged);
                    repo.stage();
                }),
            ),
            (
                // Neither a new baseline nor closing the review drops pending comments.
                Some(":review-refresh<ret>:review-close<ret>"),
                Some(&|app| {
                    assert_status_not_error(&app.editor);
                    let doc = doc!(app.editor);
                    assert_eq!(doc.text(), staged);
                    assert!(doc.review.is_none());
                    assert_eq!(pending(app), 2);
                    assert_ne!(repo.index_oid(), commented_oid);
                }),
            ),
            (
                Some(&export_existing),
                Some(&|app| {
                    assert_error(app, "failed to create");
                    assert_eq!(fs::read_to_string(&existing).unwrap(), "do not touch");
                    assert_eq!(pending(app), 2);
                }),
            ),
            (
                Some(&export_unsaved),
                Some(&|app| {
                    assert_error(app, "write reviewed buffers");
                    assert!(!batch.exists());
                    assert_eq!(pending(app), 2);
                }),
            ),
            (
                Some(&export),
                Some(&|app| {
                    assert_status_not_error(&app.editor);
                    assert_eq!(pending(app), 0);
                    assert_eq!(repo.disk(), staged);
                    let json: serde_json::Value =
                        serde_json::from_str(&fs::read_to_string(&batch).unwrap()).unwrap();
                    let (oid, file_oid) = (&commented_oid, repo.index_oid());
                    assert_eq!(
                        json,
                        serde_json::json!({
                            "format": "helix-review-batch",
                            "version": 2,
                            "request": { "kind": "ask" },
                            "files": [{ "path": file, "baseline": { "kind": "git-index", "blob_oid": file_oid } }],
                            "comments": [
                                {
                                    "path": file,
                                    "side": "old",
                                    "baseline": { "kind": "git-index", "blob_oid": oid },
                                    "start_line": 1,
                                    "end_line": 1,
                                    "current_line": 1,
                                    "selected_text": "gone1\n",
                                    "context_before": "",
                                    "context_after": "gone2\nkeep1\nkeep2\n",
                                    "comment": "why gone?",
                                },
                                {
                                    "path": file,
                                    "side": "current",
                                    "baseline": { "kind": "git-index", "blob_oid": oid },
                                    "start_line": 1,
                                    "end_line": 1,
                                    "current_line": 1,
                                    "selected_text": "keep1\n",
                                    "context_before": "",
                                    "context_after": "keep2\n",
                                    "comment": "keep this",
                                },
                            ],
                        })
                    );
                }),
            ),
        ],
        false,
    )
    .await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn command_socket_runs_typed_commands_with_one_reply_per_line() -> anyhow::Result<()> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let repo = Repo::new("old\nkeep\n", "old\nkeep\n", "new\nkeep\n");
    let _cwd = Cwd::enter(repo.dir.path());
    let commit = git_ok(repo.dir.path(), &["rev-parse", "HEAD"]);
    let socket = repo.dir.path().join("helix.sock");
    std::env::set_var("HELIX_COMMAND_SOCKET", &socket);
    let app = repo.app();
    std::env::remove_var("HELIX_COMMAND_SOCKET");
    let mut app = app?;
    assert!(socket.exists());

    let request = format!("review-baseline {commit}\nno-such-command\n");
    let path = socket.clone();
    let client = std::thread::spawn(move || -> std::io::Result<Vec<String>> {
        let mut stream = UnixStream::connect(path)?;
        stream.write_all(request.as_bytes())?;
        BufReader::new(stream).lines().take(2).collect()
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !client.is_finished() {
            run_event_loop_until_idle(&mut app).await;
        }
    })
    .await?;
    let replies = client.join().unwrap()?;

    assert!(
        replies[0].starts_with(&format!("ok Reviewing 1 buffer against {}", &commit[..8])),
        "{replies:?}"
    );
    assert!(
        replies[1].starts_with("error no such command"),
        "{replies:?}"
    );
    assert_eq!(
        doc!(app.editor)
            .review
            .as_ref()
            .unwrap()
            .baseline()
            .revision(),
        Some(commit.as_str())
    );
    assert!(render(&app).iter().any(|(_, text)| text == "old"));

    assert!(app.close().await.is_empty());
    drop(app);
    assert!(!socket.exists());
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn command_socket_is_taken_over_when_free_and_left_to_a_successor() -> anyhow::Result<()> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::{UnixListener, UnixStream};

    let repo = Repo::new("old\n", "old\n", "old\n");
    let _cwd = Cwd::enter(repo.dir.path());
    let socket = repo.dir.path().join("helix.sock");
    let owner = UnixListener::bind(&socket)?;
    std::env::set_var("HELIX_COMMAND_SOCKET", &socket);
    let app = repo.app();
    std::env::remove_var("HELIX_COMMAND_SOCKET");
    let mut app = app?;

    // The live owner keeps the socket: connections still reach it.
    let _probe = UnixStream::connect(&socket)?;
    owner.set_nonblocking(true)?;
    owner.accept()?;
    // The owner exits without removing its file, like a crashed editor.
    drop(owner);
    // A key re-arms the idle timer; an idle wait with nothing pending never returns once it
    // has fired. The timeout retries binding before `send_keys` returns.
    send_keys(&mut app, "<esc>").await?;

    let path = socket.clone();
    let client = std::thread::spawn(move || -> std::io::Result<String> {
        let mut stream = UnixStream::connect(path)?;
        stream.write_all(b"review-close\n")?;
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply)?;
        Ok(reply)
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !client.is_finished() {
            run_event_loop_until_idle(&mut app).await;
        }
    })
    .await?;
    let reply = client.join().unwrap()?;
    assert_eq!(reply, "error 'review-close': not reviewing this buffer\n");

    // A newer editor replaces the socket before this one exits.
    fs::remove_file(&socket)?;
    let successor = UnixListener::bind(&socket)?;
    assert!(app.close().await.is_empty());
    drop(app);
    assert!(socket.exists());
    let _probe = UnixStream::connect(&socket)?;
    successor.set_nonblocking(true)?;
    successor.accept()?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn writes_never_hand_off_but_explicit_commands_do() -> anyhow::Result<()> {
    let repo = Repo::new("old\nkeep\n", "old\nkeep\n", "old\nkeep\n");
    let commit = git_ok(repo.dir.path(), &["rev-parse", "HEAD"]);
    let blob = git_ok(repo.dir.path(), &["rev-parse", "HEAD:file.txt"]);
    let _cwd = Cwd::enter(repo.dir.path());
    let outbox = tempfile::tempdir()?;
    let file = helix_stdx::path::canonicalize(&repo.file);
    // Lets the auto-save debounce fire; the following key makes the event loop process it,
    // since an idle wait with nothing pending never returns.
    let settle = || tokio::time::sleep(std::time::Duration::from_millis(100));

    let mut config = test_config();
    config.editor.auto_save.after_delay = AutoSaveAfterDelay {
        enable: true,
        timeout: 10,
    };
    let mut app = AppBuilder::new()
        .with_file(&repo.file, None)
        .with_config(config)
        .build()?;

    // With a handoff directory, auto-save and :w write reviewed buffers, handing nothing off.
    let handoff = format!(":review-handoff {}<ret>", outbox.path().display());
    send_keys(
        &mut app,
        &format!(":review-baseline HEAD<ret>{handoff}ix<esc>"),
    )
    .await?;
    settle().await;
    send_keys(&mut app, "<esc>").await?;
    assert!(doc!(app.editor).review.is_some());
    assert!(!doc!(app.editor).is_modified());
    assert_eq!(repo.disk(), "xold\nkeep\n");
    send_keys(&mut app, "iy<esc>:w<ret>").await?;
    assert_eq!(repo.disk(), "xyold\nkeep\n");
    assert_eq!(fs::read_dir(outbox.path())?.count(), 0);

    // An explicit command hands off at once (writing a modified buffer first is covered by the
    // :review-ask test, which has no auto-save racing it).
    send_keys(&mut app, ":review-comment mine<ret>").await?;
    assert_eq!(repo.disk(), "xyold\nkeep\n");
    let exported: Vec<_> = fs::read_dir(outbox.path())?
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(exported.len(), 1, "{exported:?}");
    let name = exported[0].file_name().unwrap().to_str().unwrap();
    assert!(
        name.ends_with(&format!("-{}.json", std::process::id())),
        "{name}"
    );
    assert_eq!(
        app.editor.get_status().unwrap().0,
        "Sent 1 comment to the agent"
    );

    let json: serde_json::Value = serde_json::from_str(&fs::read_to_string(&exported[0])?)?;
    let baseline =
        serde_json::json!({ "kind": "git-commit", "commit_oid": commit, "blob_oid": blob });
    assert_eq!(json["format"], "helix-review-batch");
    assert_eq!(json["version"], 2);
    assert_eq!(
        json["files"],
        serde_json::json!([{ "path": file, "baseline": baseline }])
    );
    assert_eq!(json["comments"].as_array().unwrap().len(), 1);
    assert_eq!(json["comments"][0]["comment"], "mine");
    assert_eq!(json["comments"][0]["baseline"], baseline);
    assert!(app.editor.review_batch.comments().is_empty());

    send_keys(&mut app, ":q!<ret>:q!<ret>").await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn review_files_picks_files_changed_since_the_baseline() -> anyhow::Result<()> {
    let repo = Repo::new("1\n2\n3\n4\n5\n", "1\n2\n3\n4\n5\n", "1\n2\n3\n4\n5\n");
    let dir = repo.dir.path();
    for name in ["same.txt", "gone.txt"] {
        fs::write(dir.join(name), "content\n")?;
    }
    git_ok(dir, &["add", "same.txt", "gone.txt"]);
    git_ok(dir, &["commit", "-m", "more"]);
    fs::remove_file(dir.join("gone.txt"))?;
    fs::write(dir.join("staged.txt"), "staged\n")?;
    git_ok(dir, &["add", "staged.txt"]);
    fs::write(dir.join("added.txt"), "added\n")?;
    repo.write("1\n2\n3\nfour\n5\n");
    let _cwd = Cwd::enter(dir);
    // The cursor starts on the first changed line.
    let opened = |name: &'static str, line: usize| {
        move |app: &Application| {
            assert_status_not_error(&app.editor);
            let (view, doc) = current_ref!(app.editor);
            assert_eq!(doc.path().unwrap().file_name().unwrap(), name);
            assert!(doc.review.is_some());
            let text = doc.text().slice(..);
            assert_eq!(doc.selection(view.id).primary().cursor_line(text), line);
        }
    };
    let (added, staged, gone, file) = (
        opened("added.txt", 0),
        opened("staged.txt", 0),
        opened("gone.txt", 0),
        opened("file.txt", 3),
    );

    // Without a baseline, :review-files pins HEAD and reviews the pick against it.
    let head = git_ok(dir, &["rev-parse", "HEAD"]);
    let pinned = |app: &Application| {
        file(app);
        assert_eq!(app.editor.review_baseline.as_deref(), Some(head.as_str()));
        let review = doc!(app.editor).review.as_ref().unwrap();
        assert_eq!(review.baseline().revision(), Some(head.as_str()));
    };
    // file.txt +1 -1, added.txt and staged.txt +1 each (staged.txt in the index), gone.txt -1.
    let totals = |app: &Application| {
        let (status, _) = app.editor.get_status().unwrap();
        assert_eq!(status.as_ref(), "4 files, +3 -2, 1/4 hunks staged");
    };

    test_key_sequences(
        &mut repo.app()?,
        vec![
            (Some(":review-files<ret>"), Some(&totals)),
            (Some("file.txt<ret>"), Some(&pinned)),
            // The stats columns can be filtered: only file.txt has "+1 -1", only staged.txt "1/1".
            (Some(":review-files<ret>"), None),
            (Some("%+/- ^+1 -1<ret>"), Some(&file)),
            (Some(":review-files<ret>"), None),
            (Some("%hunks ^1/1<ret>"), Some(&staged)),
            (Some(":o same.txt<ret>:review-files<ret>"), None),
            (Some("added<ret>"), Some(&added)),
            (Some(":review-files<ret>"), None),
            (Some("staged<ret>"), Some(&staged)),
            (Some(":review-files<ret>"), None),
            (Some("gone<ret>"), Some(&gone)),
            (Some(":review-files<ret>"), None),
            (Some("file.txt<ret>"), Some(&file)),
            (Some(":review-files<ret>"), None),
            (Some("same<ret><esc>"), Some(&file)),
        ],
        false,
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn files_show_the_agent_that_changed_them_and_handoffs_can_address_one() -> anyhow::Result<()>
{
    let repo = Repo::new("1\n", "1\n", "one\n");
    let dir = repo.dir.path();
    let _cwd = Cwd::enter(dir);
    fs::write(dir.join("other.txt"), "other\n")?;
    let mailbox = dir.join(".pi/helix-review");
    fs::create_dir_all(mailbox.join("outbox"))?;
    fs::create_dir_all(mailbox.join("state"))?;
    let file = helix_stdx::path::canonicalize(&repo.file);
    let other = helix_stdx::path::canonicalize(dir.join("other.txt"));
    // pi-1 changed both files, then pi-2, active later, changed file.txt again.
    let state = |label: &str, last_active: u64, files: &[&Path]| {
        let snapshots: serde_json::Map<_, _> = files
            .iter()
            .map(|path| (path.display().to_string(), serde_json::json!("0")))
            .collect();
        serde_json::json!({ "label": label, "last_active": last_active, "snapshots": snapshots })
            .to_string()
    };
    fs::write(
        mailbox.join("state/a.json"),
        state("pi-1", 1, &[&file, &other]),
    )?;
    fs::write(mailbox.join("state/b.json"), state("pi-2", 2, &[&file]))?;
    let mut config = test_config();
    config.editor.statusline.right = vec![StatusLineElement::Review];
    let mut app = AppBuilder::new()
        .with_file(&repo.file, None)
        .with_config(config)
        .build()?;

    // The picker opens from a job callback, so filtering it takes a separate key sequence.
    send_keys(&mut app, ":review-files<ret>").await?;
    send_keys(&mut app, "%agent ^pi-1<ret>").await?;
    assert_eq!(doc!(app.editor).path(), Some(other.as_path()));
    send_keys(&mut app, ":review-files<ret>").await?;
    send_keys(&mut app, "%agent ^pi-2<ret>").await?;
    assert_eq!(doc!(app.editor).path(), Some(file.as_path()));

    send_keys(&mut app, ":review-send @pi-2 rename it<ret>").await?;
    assert_status_not_error(&app.editor);
    let batches: Vec<_> = fs::read_dir(mailbox.join("outbox"))?.collect::<Result<_, _>>()?;
    assert_eq!(batches.len(), 1, "{batches:?}");
    let json: serde_json::Value = serde_json::from_str(&fs::read_to_string(batches[0].path())?)?;
    assert_eq!(json["target"], "pi-2");
    assert_eq!(
        json["request"],
        serde_json::json!({ "kind": "message", "text": "rename it" })
    );
    let status = statusline(&app);
    assert!(status.contains(" @pi-2 "), "{status}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn blocks_show_the_owners_from_the_hooks_ledger() -> anyhow::Result<()> {
    let head = "1\n2\n3\n4\n5\n6\n7\n8\n";
    let repo = Repo::new(head, head, "1\nTWO\n3\n4\nFIVE\n6\nSEVEN\n8\n");
    let dir = repo.dir.path();
    let _cwd = Cwd::enter(dir);
    let file = helix_stdx::path::canonicalize(&repo.file);
    let blob = git_ok(dir, &["hash-object", "file.txt"]);
    // Another changed file, without owners.
    fs::write(dir.join("a.txt"), "a\n")?;
    git_ok(dir, &["add", "a.txt"]);
    git_ok(dir, &["commit", "-m", "a"]);
    fs::write(dir.join("a.txt"), "A\n")?;
    let state = dir.join(".pi/helix-review/state");
    fs::create_dir_all(&state)?;
    let range = |start: usize, owner: &str, agent: &str| serde_json::json!({ "start": start, "end": start + 1, "owner": owner, "agent": agent, "at": start });
    let ledger = serde_json::json!({ "files": { file.display().to_string(): {
        "blob": blob,
        "ranges": [range(2, "pi-1", "pi-1"), range(5, "pi-2", "pi-2"), range(7, "user", "pi-1")],
    } } });
    fs::write(state.join("ownership.json"), ledger.to_string())?;
    let mut app = repo.app()?;
    send_keys(&mut app, ":review-baseline HEAD<ret>").await?;

    // Each block's first line ends with its owner, `you` for the reviewer's own edit.
    let labelled = |app: &Application| -> Vec<String> {
        render(app)
            .into_iter()
            .filter_map(|(_, text)| {
                let (line, label) = text.split_once(' ')?;
                let label = label.trim();
                ["pi-1", "pi-2", "you"]
                    .contains(&label)
                    .then(|| format!("{line} {label}"))
            })
            .collect()
    };
    assert_eq!(labelled(&app), ["TWO pi-1", "FIVE pi-2", "SEVEN you"]);

    // :review-files lists every owner of the file's hunks in its agent column. The picker opens
    // from a job callback, so filtering it takes a separate key sequence.
    for owner in ["pi-1", "pi-2", "you"] {
        send_keys(&mut app, ":o a.txt<ret>:review-files<ret>").await?;
        send_keys(&mut app, &format!("%agent {owner}<ret>")).await?;
        assert_eq!(doc!(app.editor).path(), Some(file.as_path()), "{owner}");
    }

    // An edit makes the ledger's lines stale: the labels go until the next refresh.
    send_keys(&mut app, "ggix<esc>").await?;
    assert_eq!(labelled(&app), Vec::<String>::new());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn buffers_reviewed_against_the_baseline_show_a_summary() -> anyhow::Result<()> {
    let repo = Repo::new("old\n", "old\n", "old\n");
    let dir = repo.dir.path();
    fs::write(dir.join("same.txt"), "same\n")?;
    fs::write(dir.join("changed.txt"), "a\nb\nc\n")?;
    fs::write(dir.join("grown.txt"), "a\nb\n")?;
    git_ok(dir, &["add", "same.txt", "changed.txt", "grown.txt"]);
    git_ok(dir, &["commit", "-m", "more"]);
    fs::write(dir.join("changed.txt"), "a\nB\n")?;
    fs::write(dir.join("grown.txt"), "0\na\nx\nb\n")?;
    let commit = git_ok(dir, &["rev-parse", "HEAD"]);
    let short = commit[..8].to_owned();
    let _cwd = Cwd::enter(dir);
    let status = |app: &Application| {
        let (status, severity) = app.editor.get_status().expect("expected a status");
        assert_ne!(*severity, Severity::Error, "{status}");
        status.to_string()
    };
    let changed = format!(
        "Reviewing against the git commit {short}: 1 block of old text, :review-old to inspect them"
    );

    test_key_sequences(
        &mut repo.app()?,
        vec![
            (
                Some(":review-baseline HEAD<ret>"),
                Some(&|app| assert_eq!(status(app), format!("Reviewing 1 buffer against {short}"))),
            ),
            (
                Some(":o same.txt<ret>"),
                Some(&|app| {
                    assert!(doc!(app.editor).review.is_some());
                    assert_eq!(
                        status(app),
                        format!("Reviewing against the git commit {short}: no changes")
                    );
                }),
            ),
            (
                Some(":o grown.txt<ret>"),
                Some(&|app| {
                    assert_eq!(
                        status(app),
                        format!(
                            "Reviewing against the git commit {short}: 2 added hunks, no old text"
                        )
                    );
                }),
            ),
            (
                Some(":o changed.txt<ret>"),
                Some(&|app| assert_eq!(status(app), changed)),
            ),
            (
                // The baseline skips the unsaved buffer; once clean, reloading reviews it.
                Some(concat!(
                    "ix<esc>:review-baseline clear<ret>:review-baseline HEAD<ret>",
                    "u:reload<ret>",
                )),
                Some(&|app| {
                    assert!(doc!(app.editor).review.is_some());
                    assert_eq!(status(app), changed);
                }),
            ),
        ],
        false,
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn review_send_exports_the_handoff_batch_without_writing() -> anyhow::Result<()> {
    let repo = Repo::new("old\nkeep\n", "old\nkeep\n", "old\nkeep\n");
    let commit = git_ok(repo.dir.path(), &["rev-parse", "HEAD"]);
    let _cwd = Cwd::enter(repo.dir.path());
    let outbox = tempfile::tempdir()?;
    let file = helix_stdx::path::canonicalize(&repo.file);
    let modified = || fs::metadata(&repo.file).unwrap().modified().unwrap();
    let exported = || -> Vec<PathBuf> {
        fs::read_dir(outbox.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect()
    };
    let mut app = repo.app()?;

    send_keys(&mut app, ":review-baseline HEAD<ret>:review-send<ret>").await?;
    assert_error(&app, "no review handoff directory");

    let before = modified();
    let keys = format!(
        ":review-handoff {}<ret>:review-send<ret>",
        outbox.path().display()
    );
    send_keys(&mut app, &keys).await?;
    assert_status_not_error(&app.editor);
    assert_eq!(modified(), before);
    assert_eq!(repo.disk(), "old\nkeep\n");
    let batches = exported();
    assert_eq!(batches.len(), 1, "{batches:?}");
    let (status, _) = app.editor.get_status().unwrap();
    let name = batches[0].file_name().unwrap().to_str().unwrap();
    assert!(status.contains(name), "{status}");
    let json: serde_json::Value = serde_json::from_str(&fs::read_to_string(&batches[0])?)?;
    assert_eq!(json["files"][0]["path"], serde_json::json!(file));
    assert_eq!(json["files"][0]["baseline"]["commit_oid"], commit);

    send_keys(&mut app, "ix<esc>:review-send<ret>").await?;
    assert_error(&app, "unsaved changes: file.txt");
    assert_eq!(exported().len(), 1);
    assert_eq!(modified(), before);

    send_keys(&mut app, ":q!<ret>").await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn work_session_comments_wait_for_a_send_as_pull_request_review() -> anyhow::Result<()> {
    use helix_view::editor::ReviewMode;
    let repo = Repo::new("a\nb\n", "a\nb\n", "a\nB\n");
    let _cwd = Cwd::enter(repo.dir.path());
    let outbox = tempfile::tempdir()?;
    let mut app = repo.app()?;
    app.editor.review_mode = Some(ReviewMode::Work);
    let keys = format!(
        ":review-handoff {}<ret>:review HEAD<ret>2gg:review-comment rename this<ret>",
        outbox.path().display()
    );
    send_keys(&mut app, &keys).await?;
    assert_status_not_error(&app.editor);
    assert_eq!(app.editor.review_batch.comments().len(), 1);
    assert!(take_batches(outbox.path()).is_empty());

    send_keys(&mut app, ":review-send-pr Looks close<ret>").await?;
    assert_status_not_error(&app.editor);
    let batches = take_batches(outbox.path());
    assert_eq!(batches.len(), 1);
    assert_eq!(
        batches[0]["request"],
        serde_json::json!({ "kind": "pr-review", "summary": "Looks close" })
    );
    assert_eq!(batches[0]["comments"][0]["comment"], "rename this");
    assert_eq!(batches[0]["comments"][0]["start_line"], 2);
    assert_eq!(
        batches[0]["files"][0]["path"],
        serde_json::json!(helix_stdx::path::canonicalize(&repo.file))
    );
    assert!(app.editor.review_batch.comments().is_empty());

    send_keys(&mut app, "<space>AP").await?;
    assert_status_not_error(&app.editor);
    let batches = take_batches(outbox.path());
    assert_eq!(batches.len(), 1);
    assert_eq!(
        batches[0]["request"],
        serde_json::json!({ "kind": "pr-review" })
    );
    assert_eq!(batches[0]["comments"], serde_json::json!([]));

    // Without comments, changes since HEAD or a summary there is nothing to publish.
    git_ok(repo.dir.path(), &["commit", "-am", "edit"]);
    send_keys(&mut app, "<space>AP").await?;
    assert_error(&app, "nothing to publish");
    assert!(take_batches(outbox.path()).is_empty());
    send_keys(&mut app, ":review-send-pr Ship it<ret>").await?;
    assert_status_not_error(&app.editor);
    let batches = take_batches(outbox.path());
    assert_eq!(batches.len(), 1);
    assert_eq!(
        batches[0]["request"],
        serde_json::json!({ "kind": "pr-review", "summary": "Ship it" })
    );

    app.editor.review_mode = Some(ReviewMode::Review);
    send_keys(&mut app, ":review-send-pr<ret>").await?;
    assert_error(&app, "Publish review");
    assert!(take_batches(outbox.path()).is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn handoff_labels_stay_visible_without_review_and_do_not_address_batches(
) -> anyhow::Result<()> {
    let repo = Repo::new("old\n", "old\n", "new\n");
    let _cwd = Cwd::enter(repo.dir.path());
    let outbox = repo.dir.path().join("quoted outbox");
    fs::create_dir(&outbox)?;
    let mut config = test_config();
    config.editor.statusline.left.clear();
    config.editor.statusline.right = vec![StatusLineElement::Review];
    let mut app = AppBuilder::new()
        .with_file(&repo.file, None)
        .with_config(config)
        .build()?;
    let bind = format!(
        ":review-handoff '{}' \"T3: Fix parser's café\"<ret>",
        outbox.display()
    );
    send_keys(&mut app, &bind).await?;
    assert_status_not_error(&app.editor);
    assert!(doc!(app.editor).review.is_none());
    assert!(statusline(&app).contains("T3: Fix parser's café → review"));

    send_keys(&mut app, ":review HEAD<ret>:review-send @pi-2 check<ret>").await?;
    assert_status_not_error(&app.editor);
    let batches = take_batches(&outbox);
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0]["target"], "pi-2");
    let status = statusline(&app);
    assert!(
        status.contains("T3: Fix parser's café → review"),
        "{status}"
    );
    assert!(status.contains("@pi-2"), "{status}");

    send_keys(&mut app, ":review-send check again<ret>").await?;
    assert_status_not_error(&app.editor);
    let batches = take_batches(&outbox);
    assert_eq!(batches.len(), 1);
    assert!(batches[0].get("target").is_none());
    assert_eq!(app.editor.review_target, None);
    assert_eq!(
        app.editor.review_handoff_label.as_deref(),
        Some("T3: Fix parser's café")
    );

    send_keys(&mut app, ":new<ret>").await?;
    assert!(doc!(app.editor).path().is_none());
    assert!(statusline(&app).contains("T3: Fix parser's café → review"));
    // The original one-argument command remains valid and removes a previous label.
    send_keys(
        &mut app,
        &format!(":review-handoff '{}'<ret>", outbox.display()),
    )
    .await?;
    assert_status_not_error(&app.editor);
    assert_eq!(app.editor.review_handoff, Some(outbox.clone()));
    assert_eq!(app.editor.review_handoff_label, None);
    assert!(!statusline(&app).contains("review"));
    send_keys(&mut app, &bind).await?;
    send_keys(&mut app, ":review-handoff clear<ret>").await?;
    assert_status_not_error(&app.editor);
    assert_eq!(app.editor.review_handoff, None);
    assert_eq!(app.editor.review_handoff_label, None);
    assert!(!statusline(&app).contains("review"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn handoff_bridge_refresh_preserves_old_selection() -> anyhow::Result<()> {
    let original = "keep\ngone one\ngone two\nend\n";
    let repo = Repo::new(original, original, "keep\nend\n");
    let _cwd = Cwd::enter(repo.dir.path());
    let outbox = tempfile::tempdir()?;
    let mut app = repo.app()?;
    send_keys(&mut app, ":review-baseline HEAD<ret>:review-old<ret>vlll").await?;
    let focus = {
        let (view, doc) = current_ref!(app.editor);
        doc.review.as_ref().unwrap().focus(view.id).unwrap()
    };
    let current_selection = {
        let (view, doc) = current_ref!(app.editor);
        doc.selection(view.id).clone()
    };
    assert!(focus.extend);

    // Only socket metadata bypasses the restrictions on commands in old text.
    send_keys(&mut app, ":echo user command<ret>").await?;
    assert_error(&app, "is not available on old review text");
    app.editor.socket_command = true;
    send_keys(
        &mut app,
        &format!(
            ":echo 'T3: connected'<ret>:review-handoff '{}' 'T3: Parser'<ret>:review-baseline HEAD<ret>",
            outbox.path().display()
        ),
    )
    .await?;
    assert_status_not_error(&app.editor);
    assert_eq!(old_focus(&app), Some(focus.range));

    // An agent edit reloads the current file while the historical selection stays valid.
    repo.write_external("keep\nend\nagent tail\n");
    send_keys(&mut app, ":review-baseline HEAD<ret>").await?;
    assert_status_not_error(&app.editor);
    let (view, doc) = current_ref!(app.editor);
    let review = doc.review.as_ref().unwrap();
    assert_eq!(review.focus(view.id), Some(focus));
    assert_eq!(doc.selection(view.id), &current_selection);
    assert_eq!(doc.text(), "keep\nend\nagent tail\n");
    let selected = focus
        .range
        .fragment(review.snapshot().slice(..))
        .to_string();

    app.editor.socket_command = false;
    send_keys(&mut app, ":review-ask Why remove this?<ret>").await?;
    assert_status_not_error(&app.editor);
    let batches = take_batches(outbox.path());
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0]["comments"][0]["side"], "old");
    assert_eq!(batches[0]["comments"][0]["selected_text"], selected);

    // Restoring those lines removes the old block and ends its now-invalid focus.
    repo.write_external(original);
    app.editor.socket_command = true;
    send_keys(&mut app, ":review-baseline HEAD<ret>").await?;
    app.editor.socket_command = false;
    assert_status_not_error(&app.editor);
    assert_eq!(old_focus(&app), None);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn review_touch_ask_preserves_old_selection_and_deduplicates_literal_text(
) -> anyhow::Result<()> {
    let original = "keep\ngone one\ngone two\nend\n";
    let repo = Repo::new(original, original, "keep\nend\n");
    let _cwd = Cwd::enter(repo.dir.path());
    let outbox = tempfile::tempdir()?;
    let mut app = touch_app(&repo)?;
    send_keys(
        &mut app,
        &format!(
            ":review-handoff '{}'<ret>:review-baseline HEAD<ret>:review-old<ret>vlll",
            outbox.path().display()
        ),
    )
    .await?;
    let focus = old_focus(&app).unwrap();
    let selected = {
        let (view, doc) = current_ref!(app.editor);
        let review = doc.review.as_ref().unwrap();
        assert!(review.focus(view.id).unwrap().extend);
        focus.fragment(review.snapshot().slice(..)).to_string()
    };
    let current_selection = {
        let (view, doc) = current_ref!(app.editor);
        doc.selection(view.id).clone()
    };
    let text =
        "@another-session keep this literal\n%sh{touch should-not-exist} '%{buffer_name}' 🦀";
    let ask = touch_frame(1, "ask", Some(text));
    send_keys_and_paste(&mut app, "", &ask).await?;
    assert_status_not_error(&app.editor);
    assert_eq!(old_focus(&app), Some(focus));
    let (view, doc) = current_ref!(app.editor);
    assert_eq!(doc.selection(view.id), &current_selection);
    assert_eq!(repo.disk(), "keep\nend\n");
    assert!(!repo.dir.path().join("should-not-exist").exists());
    let batches = take_batches(outbox.path());
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0]["comments"][0]["side"], "old");
    assert_eq!(batches[0]["comments"][0]["selected_text"], selected);
    assert_eq!(batches[0]["comments"][0]["comment"], text);
    assert!(batches[0]
        .get("target")
        .is_none_or(serde_json::Value::is_null));

    // Lost acknowledgments can be retried without exporting a second batch.
    send_keys_and_paste(&mut app, "", &ask).await?;
    assert_status_not_error(&app.editor);
    assert!(take_batches(outbox.path()).is_empty());
    send_keys_and_paste(&mut app, "", &touch_frame(1, "ask", Some("different"))).await?;
    assert_error(&app, "reused with different contents");
    assert!(take_batches(outbox.path()).is_empty());
    assert_eq!(old_focus(&app), Some(focus));

    let send = touch_frame(2, "send", Some(text));
    send_keys_and_paste(&mut app, "", &send).await?;
    assert_status_not_error(&app.editor);
    let batches = take_batches(outbox.path());
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0]["request"]["text"], text);
    assert!(batches[0]
        .get("target")
        .is_none_or(serde_json::Value::is_null));
    send_keys_and_paste(&mut app, "", &send).await?;
    assert!(take_batches(outbox.path()).is_empty());
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn review_touch_discovery_preserves_insert_prompt_picker_and_old_selection(
) -> anyhow::Result<()> {
    let original = "keep\ngone one\ngone two\nend\n";
    let repo = Repo::new(original, original, "keep\nend\n");
    let _cwd = Cwd::enter(repo.dir.path());
    let mut app = touch_app(&repo)?;
    // Reclaiming the integration terminal follows the real resume path and advertises ready.
    assert!(app.handle_signals(libc::SIGCONT).await);
    send_keys_and_paste(&mut app, "i", "# hx-review:discover").await?;
    assert_eq!(app.editor.mode(), helix_view::document::Mode::Insert);
    assert_eq!(doc!(app.editor).text(), "keep\nend\n");
    send_keys_and_paste(&mut app, "<esc>:echo untouched", "# hx-review:discover").await?;
    send_keys(&mut app, "<ret>").await?;
    assert_eq!(app.editor.get_status().unwrap().0, "untouched");
    send_keys(&mut app, ":review-baseline HEAD<ret>").await?;
    send_keys_and_paste(&mut app, "", &touch_frame(30, "files", None)).await?;
    send_keys_and_paste(&mut app, "", "# hx-review:discover").await?;
    send_keys_and_paste(&mut app, "", &touch_frame(31, "stage", None)).await?;
    assert_error(&app, "close the HX prompt or picker");
    send_keys(&mut app, "<esc>:review-old<ret>vlll").await?;
    let focus = old_focus(&app).unwrap();
    send_keys_and_paste(&mut app, "", "# hx-review:discover").await?;
    assert_eq!(old_focus(&app), Some(focus));
    assert_eq!(doc!(app.editor).text(), "keep\nend\n");

    let mut plain = repo.app()?;
    send_keys_and_paste(&mut plain, "i", "# hx-review:discover").await?;
    assert_error(&plain, "not enabled");
    assert_eq!(plain.editor.mode(), helix_view::document::Mode::Insert);
    assert_eq!(doc!(plain.editor).text(), "keep\nend\n");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn review_touch_rejects_unsafe_states_and_frames_without_typing() -> anyhow::Result<()> {
    let repo = Repo::new("old\nkeep\n", "old\nkeep\n", "new\nkeep\n");
    let _cwd = Cwd::enter(repo.dir.path());
    let mut app = touch_app(&repo)?;
    send_keys(&mut app, ":review-baseline HEAD<ret>").await?;
    let original_index = repo.index_oid();

    send_keys_and_paste(&mut app, "i", &touch_frame(10, "stage", None)).await?;
    assert_error(&app, "finish inserting");
    assert_eq!(app.editor.mode(), helix_view::document::Mode::Insert);
    assert_eq!(repo.index_oid(), original_index);
    assert_eq!(doc!(app.editor).text(), "new\nkeep\n");
    send_keys(&mut app, "<esc>").await?;
    send_keys_and_paste(&mut app, ":echo preserved", &touch_frame(11, "stage", None)).await?;
    assert_error(&app, "close the HX prompt or picker");
    send_keys(&mut app, "<ret>").await?;
    assert_eq!(app.editor.get_status().unwrap().0, "preserved");

    // A pending normal-mode command must also survive a rejected action.
    send_keys_and_paste(&mut app, "f", &touch_frame(12, "stage", None)).await?;
    assert_error(&app, "pending HX key command");
    send_keys(&mut app, "<esc>").await?;

    let mut malformed = vec![
        "# hx-review:wrong:00".to_owned(),
        format!("# hx-review:{REVIEW_TOUCH_NONCE}:0"),
        format!("# hx-review:{REVIEW_TOUCH_NONCE}:zz"),
        format!("# hx-review:{REVIEW_TOUCH_NONCE}:{}", "00".repeat(16385)),
        touch_frame(13, "unknown", None),
        touch_frame(14, "ask", Some(&"x".repeat(8193))),
        touch_frame(15, "stage", Some("unexpected")),
        touch_frame(16, "ask", Some(" \n")),
    ];
    let mut invalid = serde_json::json!({"v": 2, "id": "a4f9d271-dcae-40fe-8bec-000000000017", "action": "stage"});
    malformed.push(touch_json_frame(&invalid));
    invalid["v"] = 1.into();
    invalid["extra"] = true.into();
    malformed.push(touch_json_frame(&invalid));
    for frame in malformed {
        send_keys_and_paste(&mut app, "", &frame).await?;
        assert_eq!(repo.index_oid(), original_index);
        assert_eq!(doc!(app.editor).text(), "new\nkeep\n");
        assert_eq!(*app.editor.get_status().unwrap().1, Severity::Error);
    }

    // Files opens the actual HX picker; another action must not dismiss it.
    send_keys_and_paste(&mut app, "", &touch_frame(20, "files", None)).await?;
    assert_status_not_error(&app.editor);
    send_keys_and_paste(&mut app, "", &touch_frame(21, "stage", None)).await?;
    assert_error(&app, "close the HX prompt or picker");
    send_keys(&mut app, "<esc>").await?;
    send_keys_and_paste(&mut app, "", &touch_frame(22, "next", None)).await?;
    assert_status_not_error(&app.editor);
    send_keys_and_paste(&mut app, "", &touch_frame(23, "previous", None)).await?;
    assert_status_not_error(&app.editor);
    send_keys_and_paste(&mut app, "", &touch_frame(24, "stage", None)).await?;
    assert_status_not_error(&app.editor);
    assert_ne!(repo.index_oid(), original_index);

    // The reserved prefix is consumed even without opt-in; ordinary paste still works.
    let mut plain = repo.app()?;
    send_keys_and_paste(&mut plain, "i", &touch_frame(25, "stage", None)).await?;
    assert_error(&plain, "not enabled");
    assert_eq!(doc!(plain.editor).text(), "new\nkeep\n");
    send_keys_and_paste(&mut plain, "", "plain").await?;
    assert_eq!(doc!(plain.editor).text(), "plainnew\nkeep\n");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn handoff_destination_precedes_review_details_in_a_narrow_center() -> anyhow::Result<()> {
    let repo = Repo::new("old\n", "old\n", "new\n");
    let _cwd = Cwd::enter(repo.dir.path());
    let outbox = tempfile::tempdir()?;
    let mut config = test_config();
    config.editor.statusline.left = vec![
        StatusLineElement::Mode,
        StatusLineElement::FileBaseName,
        StatusLineElement::FileEncoding,
    ];
    config.editor.statusline.center = vec![StatusLineElement::Review];
    config.editor.statusline.right = vec![StatusLineElement::Position];
    let mut app = AppBuilder::new()
        .with_file(&repo.file, None)
        .with_config(config)
        .build()?;
    send_keys(
        &mut app,
        &format!(
            ":review-handoff '{}' 'T3: Fix parser'<ret>:review HEAD<ret>:review-stage<ret>",
            outbox.path().display()
        ),
    )
    .await?;
    assert_status_not_error(&app.editor);
    app.editor.resize(Rect::new(0, 0, 80, 24));
    let narrow = statusline(&app);
    assert!(narrow.contains("T3: Fix parser → review"), "{narrow}");
    assert!(!narrow.contains("staged"), "{narrow}");

    app.editor.resize(Rect::new(0, 0, 160, 24));
    let wide = statusline(&app);
    assert!(wide.contains("T3: Fix parser → review"), "{wide}");
    assert!(wide.contains("1 hunk 1 staged"), "{wide}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn handoff_labels_validate_without_losing_binding_or_pending_comments() -> anyhow::Result<()>
{
    let repo = Repo::new("old\n", "old\n", "new\n");
    let outbox = tempfile::tempdir()?;
    let mut app = repo.app()?;
    break_handoff(&mut app, repo.dir.path()).await?;
    send_keys(&mut app, ":review<ret>:review-comment retained<ret>").await?;
    assert_eq!(app.editor.review_batch.comments().len(), 1);
    let bind = format!(
        ":review-handoff '{}' '  T3: Parser  '<ret>",
        outbox.path().display()
    );
    send_keys(&mut app, &bind).await?;
    assert_status_not_error(&app.editor);
    assert_eq!(
        app.editor.review_handoff_label.as_deref(),
        Some("T3: Parser")
    );
    assert_eq!(app.editor.review_batch.comments().len(), 1);

    for invalid in [
        String::new(),
        "  ".to_owned(),
        "line\nbreak".to_owned(),
        "line\rbreak".to_owned(),
        "tab\tlabel".to_owned(),
        "escape\u{1b}".to_owned(),
        "line\u{2028}break".to_owned(),
        "line\u{2029}break".to_owned(),
        "x".repeat(121),
    ] {
        send_keys_and_paste(
            &mut app,
            ":",
            &format!("review-handoff '{}' '{invalid}'", outbox.path().display()),
        )
        .await?;
        send_keys(&mut app, "<ret>").await?;
        assert_error(&app, "label must be a single line");
        assert_eq!(app.editor.review_handoff.as_deref(), Some(outbox.path()));
        assert_eq!(
            app.editor.review_handoff_label.as_deref(),
            Some("T3: Parser")
        );
        assert_eq!(app.editor.review_batch.comments().len(), 1);
    }
    send_keys(&mut app, ":review-handoff clear 'extra label'<ret>").await?;
    assert_error(&app, "clear does not take a label");
    assert_eq!(app.editor.review_handoff.as_deref(), Some(outbox.path()));

    send_keys(&mut app, ":review-handoff clear<ret>").await?;
    assert_status_not_error(&app.editor);
    assert_eq!(app.editor.review_handoff_label, None);
    assert_eq!(app.editor.review_batch.comments().len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn handoff_rebinding_waits_for_an_in_flight_comment_save() -> anyhow::Result<()> {
    let repo = Repo::new("old\n", "old\n", "new\n");
    let first = tempfile::tempdir()?;
    let second = tempfile::tempdir()?;
    let mut app = repo.app()?;
    send_keys(
        &mut app,
        &format!(":review-handoff '{}' 'First'<ret>", first.path().display()),
    )
    .await?;
    // Hold the marker that :review-comment sets until its asynchronous write finishes.
    app.editor.review_send_on_write = Some(doc!(app.editor).id());
    send_keys(
        &mut app,
        &format!(
            ":review-handoff '{}' 'Second'<ret>",
            second.path().display()
        ),
    )
    .await?;
    assert_error(&app, "a review comment is being saved");
    assert_eq!(app.editor.review_handoff.as_deref(), Some(first.path()));
    assert_eq!(app.editor.review_handoff_label.as_deref(), Some("First"));
    send_keys(&mut app, ":review-handoff clear<ret>").await?;
    assert_error(&app, "a review comment is being saved");
    assert_eq!(app.editor.review_handoff.as_deref(), Some(first.path()));

    // Updating presentation on the same destination is safe during the save.
    send_keys(
        &mut app,
        &format!(
            ":review-handoff '{}' 'Renamed'<ret>",
            first.path().display()
        ),
    )
    .await?;
    assert_status_not_error(&app.editor);
    assert_eq!(app.editor.review_handoff_label.as_deref(), Some("Renamed"));
    app.editor.review_send_on_write = None;
    send_keys(
        &mut app,
        &format!(
            ":review-handoff '{}' 'Second'<ret>",
            second.path().display()
        ),
    )
    .await?;
    assert_status_not_error(&app.editor);
    assert_eq!(app.editor.review_handoff.as_deref(), Some(second.path()));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn handoff_defaults_to_the_pi_mailbox_in_the_workspace() -> anyhow::Result<()> {
    let repo = Repo::new("old\n", "old\n", "old\n");
    let _cwd = Cwd::enter(repo.dir.path());
    let outbox = repo.dir.path().join(".pi/helix-review/outbox");
    fs::create_dir_all(&outbox)?;
    let mut app = repo.app()?;

    send_keys(&mut app, ":review-baseline HEAD<ret>:review-send<ret>").await?;
    assert_status_not_error(&app.editor);
    let batches: Vec<_> = fs::read_dir(&outbox)?.collect::<Result<_, _>>()?;
    assert_eq!(batches.len(), 1, "{batches:?}");
    let json: serde_json::Value = serde_json::from_str(&fs::read_to_string(batches[0].path())?)?;
    assert_eq!(
        json["files"][0]["path"],
        serde_json::json!(helix_stdx::path::canonicalize(&repo.file))
    );

    // A reviewed file outside the workspace keeps its batches out of the workspace's mailbox.
    let outside = Repo::new("other\n", "other\n", "other\n");
    let keys = format!(
        ":review-baseline clear<ret>:o {}<ret>:review<ret>",
        outside.file.display()
    );
    send_keys(&mut app, &keys).await?;
    assert!(
        doc!(app.editor).review.is_some(),
        "{:?}",
        app.editor.get_status()
    );
    send_keys(&mut app, ":review-send<ret>").await?;
    assert_error(&app, "no review handoff directory");
    assert_eq!(fs::read_dir(&outbox)?.count(), 1);

    send_keys(&mut app, ":q!<ret>").await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn change_motions_follow_the_review_baseline_until_it_closes() -> anyhow::Result<()> {
    let repo = Repo::new("a\nb\nc\nd\ne\n", "a\nb\nc\nd\ne\n", "a\nb\nc\nd\ne\n");
    repo.write("a\nb\nC\nd\ne\n");
    repo.stage();
    git_ok(repo.dir.path(), &["commit", "-m", "agent"]);
    let mut app = repo.app()?;
    let cursor_line = |app: &Application| {
        let (view, doc) = current_ref!(app.editor);
        doc.selection(view.id)
            .primary()
            .cursor_line(doc.text().slice(..))
    };
    // The differ runs in the background; waits until it reports `hunks` hunks.
    let wait_for_hunks = |app: &Application, hunks: u32| {
        let doc = doc!(app.editor);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while doc.diff_handle().map_or(0, |diff| diff.load().len()) != hunks {
            assert!(
                std::time::Instant::now() < deadline,
                "expected {hunks} hunks"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    };

    // The index is clean, so the ordinary diff has nothing to visit.
    wait_for_hunks(&app, 0);
    send_keys(&mut app, "]g").await?;
    assert_eq!(cursor_line(&app), 0);

    send_keys(&mut app, ":review HEAD~1<ret>").await?;
    assert_status_not_error(&app.editor);
    wait_for_hunks(&app, 1);
    send_keys(&mut app, "]g").await?;
    assert_eq!(cursor_line(&app), 2);
    assert_eq!(old_focus(&app), None);

    send_keys(&mut app, ":review-close<ret>").await?;
    wait_for_hunks(&app, 0);
    send_keys(&mut app, "gg]g").await?;
    assert_eq!(cursor_line(&app), 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn pending_comments_are_drawn_below_the_lines_they_are_on() -> anyhow::Result<()> {
    let repo = Repo::new(
        "a
b
c
d
e
",
        "a
b
c
d
e
",
        "a
b
C
d
e
",
    );
    let _cwd = Cwd::enter(repo.dir.path());
    let mut app = repo.app()?;
    // Comments that cannot be sent stay pending.
    break_handoff(&mut app, repo.dir.path()).await?;
    let rows = |app: &Application| -> Vec<String> {
        let mut rows: Vec<_> = render(app).into_iter().map(|(_, text)| text).collect();
        while rows.last().is_some_and(String::is_empty) {
            rows.pop();
        }
        rows
    };

    send_keys(&mut app, ":review<ret>ggxx:review-comment on a and b<ret>").await?;
    assert_eq!(
        rows(&app),
        ["a", "b", "▲ pi │ on a and b", "c", "C", "d", "e"]
    );

    // A comment on old text is drawn below the old lines it is on.
    send_keys(&mut app, ":review-old<ret>:review-comment old c<ret><esc>").await?;
    assert_eq!(
        rows(&app),
        [
            "a",
            "b",
            "▲ pi │ on a and b",
            "c",
            "▲ pi │ old c",
            "C",
            "d",
            "e"
        ]
    );

    send_keys(&mut app, ":review-uncomment<ret>").await?;
    assert_eq!(
        app.editor.get_status().unwrap().0,
        "Dropped 1 review comment, 1 pending"
    );
    assert_eq!(
        rows(&app),
        ["a", "b", "▲ pi │ on a and b", "c", "C", "d", "e"]
    );
    send_keys(&mut app, ":review-uncomment<ret>").await?;
    assert_eq!(rows(&app), ["a", "b", "c", "C", "d", "e"]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn review_ask_goes_to_the_agent_while_review_comment_goes_to_the_pull_request(
) -> anyhow::Result<()> {
    let fixture = PrFixture::new(
        "a
b
c
",
        "a
B
c
",
        r#"api\ *) echo '[]';;"#,
    );
    let dir = fixture.repo.dir.path();
    let _cwd = Cwd::enter(dir);
    let outbox = dir.join(".pi/helix-review/outbox");
    fs::create_dir_all(&outbox)?;
    let mut app = fixture.open().await?;
    wait_for(&mut app, |_| fixture.gh.calls().len() == 4).await?;
    let texts = |app: &Application| -> Vec<String> {
        render(app).into_iter().map(|(_, text)| text).collect()
    };

    send_keys(&mut app, "gg:review-comment for the pr<ret>").await?;
    assert_status_not_error(&app.editor);
    send_keys(&mut app, "3gg:review-ask for pi<ret>").await?;
    assert_status_not_error(&app.editor);
    send_keys(&mut app, ":review-send<ret>").await?;
    assert_status_not_error(&app.editor);

    let sent: Vec<_> = take_batches(&outbox)
        .iter()
        .flat_map(|batch| batch["comments"].as_array().unwrap().clone())
        .map(|comment| comment["comment"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(sent, ["for pi"]);
    let pending: Vec<_> = (app.editor.review_pr_comments.iter())
        .map(|comment| comment.comment.as_str())
        .collect();
    assert_eq!(pending, ["for the pr"]);
    assert!(app.editor.review_batch.comments().is_empty());
    // Only the pull request comment is still pending, so only it is drawn.
    let rows = texts(&app);
    assert_eq!(rows[..3], ["a", "▲ pr │ for the pr", "b"], "{rows:#?}");
    assert!(!rows.iter().any(|row| row.contains("for pi")), "{rows:#?}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn review_pr_attach_preserves_saved_agent_edits_index_and_current_branch(
) -> anyhow::Result<()> {
    let fixture = PrFixture::new("base\n", "head\n", r#"api\ *) echo '[]';;"#);
    let dir = fixture.repo.dir.path();
    let _cwd = Cwd::enter(dir);
    git_ok(dir, &["checkout", "-q", "feature"]);
    fixture.repo.write("staged agent edit\n");
    git_ok(dir, &["add", "file.txt"]);
    fixture.repo.write("saved agent edit\n");
    fs::write(dir.join("new.txt"), "unopened agent edit\n")?;
    let index = git_ok(dir, &["diff", "--cached"]);
    let status = git_ok(dir, &["status", "--porcelain"]);
    let mut app = AppBuilder::new()
        .with_file(&fixture.repo.file, None)
        .build()?;
    send_keys(&mut app, ":review-pr-attach 7<ret>").await?;
    wait_for(&mut app, |app| !app.editor.review_pr_loading).await?;
    assert_status_not_error(&app.editor);
    let pr = app.editor.review_pr.as_ref().unwrap();
    assert_eq!(pr.head_oid, fixture.head);
    assert_eq!(pr.merge_base, fixture.merge_base);
    assert_eq!(pr.repo, "o/r");
    assert!(pr.batch_dir.is_none());
    assert_eq!(doc!(app.editor).text(), "saved agent edit\n");
    assert_eq!(
        fs::read_to_string(&fixture.repo.file)?,
        "saved agent edit\n"
    );
    assert_eq!(
        fs::read_to_string(dir.join("new.txt"))?,
        "unopened agent edit\n"
    );
    assert_eq!(git_ok(dir, &["diff", "--cached"]), index);
    assert_eq!(git_ok(dir, &["status", "--porcelain"]), status);
    assert_eq!(git_ok(dir, &["branch", "--show-current"]), "feature");
    assert_eq!(git_ok(dir, &["rev-parse", "HEAD"]), fixture.head);
    assert!(!fixture
        .gh
        .calls()
        .iter()
        .any(|call| call.starts_with("pr checkout")));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn review_pr_attach_rejects_an_unrelated_checkout_without_switching_branches(
) -> anyhow::Result<()> {
    let fixture = PrFixture::new("base\n", "head\n", r#"api\ *) echo '[]';;"#);
    let dir = fixture.repo.dir.path();
    let _cwd = Cwd::enter(dir);
    let head = git_ok(dir, &["rev-parse", "HEAD"]);
    let mut app = AppBuilder::new()
        .with_file(&fixture.repo.file, None)
        .build()?;
    send_keys(&mut app, ":review-pr-attach 7<ret>").await?;
    wait_for(&mut app, |app| !app.editor.review_pr_loading).await?;
    assert!(app.editor.review_pr.is_none());
    assert!(app.editor.get_status().unwrap().0.contains("current head"));
    assert_eq!(git_ok(dir, &["rev-parse", "HEAD"]), head);
    assert_eq!(git_ok(dir, &["branch", "--show-current"]), "main");
    assert_eq!(git_ok(dir, &["status", "--porcelain"]), "");
    assert_eq!(fixture.gh.calls().len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn conversation_checkpoint_opens_editable_current_file_and_keeps_its_pin(
) -> anyhow::Result<()> {
    let repo = Repo::new(
        "head\nkeep\nold\n",
        "pre-turn\nkeep\nold\n",
        "pre-turn\nkeep\nnew\n",
    );
    let dir = repo.dir.path();
    let _cwd = Cwd::enter(dir);
    let tree = git_ok(dir, &["write-tree"]);
    let checkpoint = git_ok(
        dir,
        &["commit-tree", &tree, "-p", "HEAD", "-m", "checkpoint"],
    );
    let reference = "refs/t3/orchestration-v2/checkpoints/thread/turn";
    git_ok(dir, &["update-ref", reference, &checkpoint]);
    assert!(
        !git(dir, &["merge-base", "--is-ancestor", &checkpoint, "HEAD"])
            .status
            .success()
    );
    let mut app = repo.app()?;
    let nested = dir.join("apps/server");
    fs::create_dir_all(&nested)?;
    let _nested_cwd = Cwd::enter(&nested);
    let location =
        serde_json::json!({"baseline": reference, "filePath": "file.txt", "worktreePath": nested});
    let open = format!(":review-open '{location}'<ret>");
    send_keys(&mut app, &open).await?;
    assert_eq!(
        app.editor.review_baseline.as_deref(),
        Some(checkpoint.as_str())
    );
    {
        let (view, doc) = current_ref!(app.editor);
        assert_eq!(doc.path(), Some(repo.file.as_path()));
        assert!(!doc.readonly);
        assert_eq!(doc.text().to_string(), "pre-turn\nkeep\nnew\n");
        let review = doc.review.as_ref().unwrap();
        assert_eq!(review.first_change_line(), Some(2));
        assert_eq!(
            doc.selection(view.id)
                .primary()
                .cursor_line(doc.text().slice(..)),
            2
        );
    }
    send_keys(&mut app, "gg").await?;
    send_keys(&mut app, &open).await?;
    {
        let (view, doc) = current_ref!(app.editor);
        assert_eq!(
            doc.selection(view.id)
                .primary()
                .cursor_line(doc.text().slice(..)),
            2
        );
    }
    let explicit = serde_json::json!({"baseline": reference, "filePath": "file.txt", "line": 3, "column": 2, "side": "new"});
    send_keys(&mut app, &format!(":review-open '{explicit}'<ret>")).await?;
    {
        let (view, doc) = current_ref!(app.editor);
        assert_eq!(
            doc.selection(view.id)
                .primary()
                .cursor(doc.text().slice(..)),
            15
        );
    }
    let old = serde_json::json!({"baseline": reference, "filePath": "file.txt", "line": 3, "side": "old"});
    send_keys(&mut app, &format!(":review-open '{old}'<ret>")).await?;
    {
        let (view, doc) = current_ref!(app.editor);
        assert!(doc.review.as_ref().unwrap().focus(view.id).is_some());
    }
    app.editor.socket_command = true;
    send_keys(&mut app, &open).await?;
    {
        let (view, doc) = current_ref!(app.editor);
        assert!(doc.review.as_ref().unwrap().focus(view.id).is_none());
    }
    send_keys(&mut app, ":review-baseline HEAD<ret>").await?;
    app.editor.socket_command = false;
    assert_eq!(
        app.editor.review_baseline.as_deref(),
        Some(checkpoint.as_str())
    );
    assert_eq!(
        app.editor.review_user_pin.as_deref(),
        Some(checkpoint.as_str())
    );
    send_keys(&mut app, "iEDIT<esc>").await?;
    assert!(
        doc!(app.editor).is_modified(),
        "text: {} status: {:?}",
        doc!(app.editor).text(),
        app.editor.get_status()
    );
    let edited = doc!(app.editor).text().to_string();
    let other_ref = "refs/t3/orchestration-v2/checkpoints/thread/other";
    git_ok(dir, &["update-ref", other_ref, "HEAD"]);
    let other = serde_json::json!({"baseline": other_ref, "filePath": "file.txt"});
    send_keys(&mut app, &format!(":review-open '{other}'<ret>")).await?;
    assert_error(&app, "unsaved changes under another review baseline");
    assert_eq!(doc!(app.editor).text().to_string(), edited);
    assert_eq!(
        app.editor.review_baseline.as_deref(),
        Some(checkpoint.as_str())
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn conversation_checkpoint_rejects_non_checkpoint_refs_outside_history() -> anyhow::Result<()>
{
    let repo = Repo::new("head\n", "before\n", "after\n");
    let dir = repo.dir.path();
    let _cwd = Cwd::enter(dir);
    let tree = git_ok(dir, &["write-tree"]);
    let checkpoint = git_ok(
        dir,
        &["commit-tree", &tree, "-p", "HEAD", "-m", "checkpoint"],
    );
    git_ok(dir, &["update-ref", "refs/heads/checkpoint", &checkpoint]);
    let mut app = repo.app()?;
    for baseline in ["refs/heads/checkpoint", checkpoint.as_str()] {
        let location = serde_json::json!({"baseline": baseline, "filePath": "file.txt"});
        send_keys(&mut app, &format!(":review-open '{location}'<ret>")).await?;
        assert_error(&app, "baseline is not in this branch's history");
    }
    send_keys(&mut app, &format!(":review-baseline {checkpoint}<ret>")).await?;
    assert_error(&app, "not in this branch");
    assert!(app.editor.review_baseline.is_none());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn conversation_checkpoint_opens_deleted_current_file_without_recreating_it(
) -> anyhow::Result<()> {
    let repo = Repo::new("head\n", "pre-turn\n", "pre-turn\n");
    let dir = repo.dir.path();
    let _cwd = Cwd::enter(dir);
    let tree = git_ok(dir, &["write-tree"]);
    let checkpoint = git_ok(
        dir,
        &["commit-tree", &tree, "-p", "HEAD", "-m", "checkpoint"],
    );
    let reference = "refs/t3/orchestration-v2/checkpoints/thread/deletion";
    git_ok(dir, &["update-ref", reference, &checkpoint]);
    fs::remove_file(&repo.file)?;
    let mut app = AppBuilder::new().build()?;
    let location = serde_json::json!({"baseline": reference, "filePath": "file.txt"});
    send_keys(&mut app, &format!(":review-open '{location}'<ret>")).await?;
    let doc = doc!(app.editor);
    assert_eq!(doc.path(), Some(repo.file.as_path()));
    assert!(doc.review.is_some());
    assert!(!repo.file.exists());
    assert!(!doc.is_modified());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn conversation_checkpoint_leaves_attached_pr_and_preserves_drafts() -> anyhow::Result<()> {
    let fixture = PrFixture::new(
        "head\nkeep\nold\n",
        "head\nkeep\nnew\n",
        r#"api\ *) echo '[]';;"#,
    );
    let repo = &fixture.repo;
    let dir = repo.dir.path();
    let _cwd = Cwd::enter(dir);
    let mut app = fixture.open().await?;
    wait_for(&mut app, |_| fixture.gh.calls().len() == 4).await?;
    send_keys(&mut app, "3gg:review-comment keep draft<ret>").await?;
    assert_eq!(app.editor.review_pr_comments.len(), 1);
    repo.write("pre-turn\nkeep\nold\n");
    repo.stage();
    let tree = git_ok(dir, &["write-tree"]);
    let checkpoint = git_ok(
        dir,
        &["commit-tree", &tree, "-p", "HEAD", "-m", "checkpoint"],
    );
    let reference = "refs/t3/orchestration-v2/checkpoints/thread/pr";
    git_ok(dir, &["update-ref", reference, &checkpoint]);
    repo.write("pre-turn\nkeep\nnew\n");
    let location = serde_json::json!({"baseline": reference, "filePath": "file.txt"});
    let open = format!(":review-open '{location}'<ret>");
    app.editor.review_pr_loading = true;
    send_keys(&mut app, &open).await?;
    assert_error(&app, "loading");
    assert!(app.editor.review_pr.is_some());
    assert_eq!(
        app.editor.review_baseline.as_deref(),
        Some(fixture.merge_base.as_str())
    );
    app.editor.review_pr_loading = false;
    send_keys(&mut app, &open).await?;
    assert_status_not_error(&app.editor);
    assert!(app.editor.review_pr.is_none());
    assert!(app.editor.review_pr_threads.is_empty());
    assert_eq!(app.editor.review_pr_comments.len(), 1);
    assert_eq!(app.editor.review_pr_comments[0].comment, "keep draft");
    assert_eq!(
        app.editor.review_baseline.as_deref(),
        Some(checkpoint.as_str())
    );
    let (view, doc) = current_ref!(app.editor);
    assert_eq!(doc.text().to_string(), "pre-turn\nkeep\nnew\n");
    assert!(!doc.readonly);
    assert_eq!(
        doc.selection(view.id)
            .primary()
            .cursor_line(doc.text().slice(..)),
        2
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn conversation_checkpoint_reports_review_limits_without_blocking_plain_file_opens(
) -> anyhow::Result<()> {
    use helix_view::review::Review;

    let oversized = "x".repeat(Review::MAX_BYTES + 1);
    let repo = Repo::new("head\n", "pre-turn\n", &oversized);
    let dir = repo.dir.path();
    let _cwd = Cwd::enter(dir);
    let tree = git_ok(dir, &["write-tree"]);
    let checkpoint = git_ok(
        dir,
        &["commit-tree", &tree, "-p", "HEAD", "-m", "checkpoint"],
    );
    let reference = "refs/t3/orchestration-v2/checkpoints/thread/large";
    git_ok(dir, &["update-ref", reference, &checkpoint]);
    let mut app = AppBuilder::new().build()?;
    let plain = serde_json::json!({"filePath": "file.txt"});
    send_keys(&mut app, &format!(":review-open '{plain}'<ret>")).await?;
    assert_status_not_error(&app.editor);
    let location = serde_json::json!({"baseline": reference, "filePath": "file.txt"});
    send_keys(&mut app, &format!(":review-open '{location}'<ret>")).await?;
    assert_error(&app, Review::LIMIT_MESSAGE);
    assert_eq!(
        app.editor.review_baseline.as_deref(),
        Some(checkpoint.as_str())
    );
    let doc = doc!(app.editor);
    assert!(doc.review.is_none());
    assert!(!doc.readonly);
    assert_eq!(doc.text().to_string(), oversized);
    assert_eq!(repo.disk(), oversized);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_pr_review_waits_for_attachment_before_opening_files() -> anyhow::Result<()> {
    use helix_view::editor::ReviewMode;
    let fixture = PrFixture::new("base\nkeep\n", "head\nkeep\n", r#"api\ *) echo '[]';;"#);
    let dir = fixture.repo.dir.path();
    let _cwd = Cwd::enter(dir);
    git_ok(dir, &["checkout", "-q", "feature"]);
    let gate = fixture.gh._dir.path().join("attach-gate");
    let started = fixture.gh._dir.path().join("attach-started");
    assert!(Command::new("mkfifo")
        .args([&gate, &started])
        .status()?
        .success());
    let gh = std::env::var("HELIX_GH")?;
    let original = fs::read_to_string(&gh)?;
    fs::write(
        &gh,
        original.replace(
            r"pr\ view\ 7*) ",
            &format!(
                "pr\\ view\\ 7*) printf started > '{}'; read ready < '{}'; ",
                started.display(),
                gate.display()
            ),
        ),
    )?;
    let mut app = AppBuilder::new().build()?;
    app.editor.review_mode = Some(ReviewMode::Review);
    send_keys(&mut app, ":review-pr-attach 7<ret>").await?;
    tokio::time::timeout(std::time::Duration::from_secs(5), tokio::fs::read(&started)).await??;
    let mut early_statuses = Vec::new();
    for location in [
        serde_json::json!({"mode": "review", "worktreePath": dir}),
        serde_json::json!({"mode": "review", "filePath": "file.txt", "line": 2}),
    ] {
        send_keys(&mut app, &format!(":review-open '{location}'<ret>")).await?;
        early_statuses.push(app.editor.get_status().map(|status| status.0.to_string()));
    }
    fs::write(&gate, "ready\n")?;
    wait_for(&mut app, |app| !app.editor.review_pr_loading).await?;
    for status in early_statuses {
        assert!(
            status
                .as_deref()
                .is_some_and(|text| text.contains("pull request is loading")),
            "{status:?}"
        );
    }
    assert_eq!(
        app.editor.review_pr.as_ref().unwrap().head_oid,
        fixture.head
    );
    let location = serde_json::json!({"mode": "review", "filePath": "file.txt", "line": 2});
    send_keys(&mut app, &format!(":review-open '{location}'<ret>")).await?;
    assert_status_not_error(&app.editor);
    let (view, doc) = current_ref!(app.editor);
    assert_eq!(doc.path(), Some(fixture.repo.file.as_path()));
    assert!(doc.review_draft);
    assert_eq!(
        doc.selection(view.id)
            .primary()
            .cursor_line(doc.text().slice(..)),
        1
    );
    assert_eq!(git_ok(dir, &["status", "--porcelain"]), "");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_pr_review_edits_suggestion_buffers_without_writing_the_checkout(
) -> anyhow::Result<()> {
    use helix_view::editor::ReviewMode;
    let fixture = PrFixture::new("base\n", "head\n", r#"api\ *) echo '[]';;"#);
    let dir = fixture.repo.dir.path();
    let _cwd = Cwd::enter(dir);
    git_ok(dir, &["checkout", "-q", "feature"]);
    fixture.repo.write("staged work\n");
    git_ok(dir, &["add", "file.txt"]);
    fixture.repo.write("saved work\n");
    fs::write(dir.join("new.txt"), "other saved work\n")?;
    let index = git_ok(dir, &["diff", "--cached"]);
    let status = git_ok(dir, &["status", "--porcelain"]);
    let mut app = fixture.repo.app()?;
    app.editor.review_mode = Some(ReviewMode::Review);
    send_keys(&mut app, ":review-pr-attach 7<ret>").await?;
    wait_for(&mut app, |app| !app.editor.review_pr_loading).await?;
    assert_status_not_error(&app.editor);
    assert_eq!(doc!(app.editor).text(), "head\n");
    assert!(doc!(app.editor).review_draft);
    let external = tempfile::tempdir()?;
    let file = external.path().join("external.txt");
    fs::write(&file, "external\n")?;
    let location = serde_json::json!({"mode": "review", "filePath": file});
    send_keys(&mut app, &format!(":review-open '{location}'<ret>")).await?;
    assert_error(&app, "outside this worktree");
    assert_eq!(doc!(app.editor).path(), Some(fixture.repo.file.as_path()));
    assert!(app.editor.document_by_path(&file).is_none());
    assert_eq!(fs::read_to_string(&file)?, "external\n");
    send_keys(&mut app, "ggcsuggested<esc>").await?;
    let draft = doc!(app.editor).text().to_string();
    assert!(draft.contains("suggested"));
    assert!(doc!(app.editor).is_modified());
    send_keys(&mut app, ":w<ret>").await?;
    assert_error(&app, "Suggestion drafts stay in HX");
    send_keys(&mut app, ":w!<ret>").await?;
    assert_error(&app, "Suggestion drafts stay in HX");
    let alternate = dir.join("must-not-exist.txt");
    send_keys(&mut app, &format!(":w! {}<ret>", alternate.display())).await?;
    assert_error(&app, "Suggestion drafts stay in HX");
    assert!(!alternate.exists());
    assert_eq!(doc!(app.editor).path(), Some(fixture.repo.file.as_path()));
    let destination = dir.join("new-parent/renamed.txt");
    send_keys(&mut app, &format!(":mv! {}<ret>", destination.display())).await?;
    assert_error(&app, "keeps checkout files unchanged");
    assert!(!destination.parent().unwrap().exists());
    assert!(app
        .editor
        .move_path(&fixture.repo.file, &destination)
        .is_err());
    assert!(app.editor.create_path(&alternate, false).is_err());
    assert!(app
        .editor
        .create_path(&dir.join("created-directory"), true)
        .is_err());
    assert!(app.editor.delete_path(&fixture.repo.file, false).is_err());
    assert!(!alternate.exists());
    assert!(!dir.join("created-directory").exists());
    assert!(statusline(&app).contains("PR DRAFT"));
    let doc = app
        .editor
        .documents_mut()
        .find(|doc| doc.path() == Some(fixture.repo.file.as_path()))
        .unwrap();
    assert!(doc.save(Some(&alternate), true).is_err());
    send_keys(&mut app, ":open new.txt<ret>").await?;
    assert_eq!(doc!(app.editor).text(), "new\n");
    send_keys(&mut app, ":open file.txt<ret>").await?;
    assert_eq!(doc!(app.editor).text().to_string(), draft);
    for command in [
        ":review-stage<ret>",
        ":review-commit<ret>",
        ":review-commit-all<ret>",
        ":review-submit approve<ret>",
    ] {
        send_keys(&mut app, command).await?;
        assert!(app
            .editor
            .get_status()
            .is_some_and(|(_, severity)| *severity == Severity::Error));
    }
    send_keys(&mut app, ":reload<ret>").await?;
    assert_error(&app, "attached PR snapshot");
    assert_eq!(doc!(app.editor).text().to_string(), draft);
    assert_eq!(fixture.repo.disk(), "saved work\n");
    assert_eq!(
        fs::read_to_string(dir.join("new.txt"))?,
        "other saved work\n"
    );
    assert_eq!(git_ok(dir, &["diff", "--cached"]), index);
    assert_eq!(git_ok(dir, &["status", "--porcelain"]), status);
    assert_eq!(git_ok(dir, &["branch", "--show-current"]), "feature");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_pr_review_ask_exports_draft_selection_without_saving() -> anyhow::Result<()> {
    use helix_view::editor::ReviewMode;
    let fixture = PrFixture::new("base\n", "head\n", r#"api\ *) echo '[]';;"#);
    let dir = fixture.repo.dir.path();
    let _cwd = Cwd::enter(dir);
    git_ok(dir, &["checkout", "-q", "feature"]);
    let outbox = tempfile::tempdir()?;
    let mut app = fixture.repo.app()?;
    app.editor.review_mode = Some(ReviewMode::Review);
    send_keys(&mut app, ":review-pr-attach 7<ret>").await?;
    wait_for(&mut app, |app| !app.editor.review_pr_loading).await?;
    send_keys(
        &mut app,
        &format!(":review-handoff {}<ret>", outbox.path().display()),
    )
    .await?;
    send_keys(
        &mut app,
        "ggcsuggestion<esc>ggx:review-ask Explain this proposal<ret>",
    )
    .await?;
    assert_status_not_error(&app.editor);
    assert_eq!(fixture.repo.disk(), "head\n");
    assert!(doc!(app.editor).is_modified());
    assert!(app.editor.review_send_on_write.is_none());
    let batches: Vec<_> = fs::read_dir(outbox.path())?.collect::<Result<_, _>>()?;
    assert_eq!(batches.len(), 1);
    let batch: serde_json::Value = serde_json::from_slice(&fs::read(batches[0].path())?)?;
    assert_eq!(batch["request"]["kind"], "ask");
    assert_eq!(batch["comments"].as_array().unwrap().len(), 1);
    assert!(batch["comments"][0]["selected_text"]
        .as_str()
        .unwrap()
        .contains("suggestion"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_work_opens_external_absolute_files_from_a_scratch_directory() -> anyhow::Result<()>
{
    use helix_view::editor::ReviewMode;
    let scratch = tempfile::tempdir()?;
    let external = tempfile::tempdir()?;
    let file = external.path().join("external.txt");
    fs::write(&file, "first\nsecond\n")?;
    let _cwd = Cwd::enter(scratch.path());
    let mut app = AppBuilder::new().build()?;
    app.editor.review_mode = Some(ReviewMode::Work);
    let location = serde_json::json!({
        "mode": "work", "filePath": file, "worktreePath": scratch.path(),
        "line": 2, "column": 2, "side": "new"
    });
    send_keys(&mut app, &format!(":review-open '{location}'<ret>")).await?;
    assert_status_not_error(&app.editor);
    {
        let (view, doc) = current_ref!(app.editor);
        assert_eq!(doc.path(), Some(file.as_path()));
        assert_eq!(doc.text(), "first\nsecond\n");
        assert_eq!(
            doc.selection(view.id)
                .primary()
                .cursor(doc.text().slice(..)),
            7
        );
        assert!(!doc.readonly);
        assert!(!doc.review_draft);
        assert!(doc.review.is_none());
    }
    send_keys(&mut app, "iEDIT<esc>:w<ret>").await?;
    assert_status_not_error(&app.editor);
    assert_eq!(fs::read_to_string(&file)?, "first\nsEDITecond\n");
    assert!(app.editor.review_baseline.is_none());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_work_rejects_external_comparison_locations() -> anyhow::Result<()> {
    use helix_view::editor::ReviewMode;
    let repo = Repo::new("head\n", "head\n", "head\n");
    let external = tempfile::tempdir()?;
    let file = external.path().join("external.txt");
    fs::write(&file, "external\n")?;
    let _cwd = Cwd::enter(repo.dir.path());
    let mut app = repo.app()?;
    app.editor.review_mode = Some(ReviewMode::Work);
    for location in [
        serde_json::json!({"filePath": file, "baseline": "HEAD"}),
        serde_json::json!({"filePath": file, "baseline": "HEAD", "revision": "HEAD"}),
        serde_json::json!({"filePath": file, "side": "old", "line": 1}),
    ] {
        send_keys(&mut app, &format!(":review-open '{location}'<ret>")).await?;
        assert_error(&app, "outside this worktree");
        assert_eq!(doc!(app.editor).path(), Some(repo.file.as_path()));
        assert!(app.editor.document_by_path(&file).is_none());
        assert!(app.editor.review_baseline.is_none());
        assert!(app.editor.review_snapshot.is_none());
    }
    assert_eq!(fs::read_to_string(&file)?, "external\n");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_work_rejects_pr_attachment_and_other_mode_locations() -> anyhow::Result<()> {
    use helix_view::editor::ReviewMode;
    let fixture = PrFixture::new("base\n", "head\n", r#"api\ *) echo '[]';;"#);
    let _cwd = Cwd::enter(fixture.repo.dir.path());
    let mut app = fixture.repo.app()?;
    app.editor.review_mode = Some(ReviewMode::Work);
    send_keys(&mut app, ":review-pr-attach 7<ret>").await?;
    assert_error(&app, "open a separate Review a PR session");
    assert!(app.editor.review_pr.is_none());
    let location = serde_json::json!({"mode": "review", "filePath": "file.txt"});
    send_keys(&mut app, &format!(":review-open '{location}'<ret>")).await?;
    assert_error(&app, "open a separate Work together or Review a PR session");
    assert!(fixture.gh.calls().is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_pr_review_opens_missing_checkout_files_and_preserves_their_drafts(
) -> anyhow::Result<()> {
    use helix_view::editor::ReviewMode;
    let fixture = PrFixture::new("base\n", "head\n", r#"api\ *) echo '[]';;"#);
    let dir = fixture.repo.dir.path();
    let _cwd = Cwd::enter(dir);
    git_ok(dir, &["checkout", "-q", "feature"]);
    fs::remove_file(&fixture.repo.file)?;
    fs::remove_file(dir.join("new.txt"))?;
    let status = git_ok(dir, &["status", "--porcelain"]);
    let mut app = fixture.repo.app()?;
    app.editor.review_mode = Some(ReviewMode::Review);
    send_keys(&mut app, ":review-open '{\"mode\":\"review\"}'<ret>").await?;
    assert_status_not_error(&app.editor);
    assert!(app.editor.review_pr.is_none());
    send_keys(&mut app, ":review-pr-attach 7<ret>").await?;
    wait_for(&mut app, |app| !app.editor.review_pr_loading).await?;
    assert_status_not_error(&app.editor);
    assert_eq!(doc!(app.editor).text(), "head\n");
    send_keys(&mut app, "ggiproposal <esc>").await?;
    let draft = doc!(app.editor).text().to_string();
    let draft_id = doc!(app.editor).id();
    let nested = dir.join("apps/server");
    fs::create_dir_all(&nested)?;
    let _nested_cwd = Cwd::enter(&nested);
    for (file, expected) in [("new.txt", "new\n"), ("file.txt", draft.as_str())] {
        let location =
            serde_json::json!({"mode": "review", "filePath": file, "worktreePath": nested});
        send_keys(&mut app, &format!(":review-open '{location}'<ret>")).await?;
        assert_status_not_error(&app.editor);
        assert_eq!(doc!(app.editor).text(), expected);
    }
    let snapshot = serde_json::json!({"mode": "review", "baseline": fixture.merge_base, "revision": fixture.head, "filePath": "file.txt", "worktreePath": nested});
    send_keys(&mut app, &format!(":review-open '{snapshot}'<ret>")).await?;
    assert_status_not_error(&app.editor);
    assert_eq!(app.editor.review_snapshot, Some(doc!(app.editor).id()));
    assert!(doc!(app.editor).text().to_string().contains("+head"));
    assert!(app.editor.review_pr.is_some());
    let current =
        serde_json::json!({"mode": "review", "filePath": "file.txt", "worktreePath": nested});
    send_keys(&mut app, &format!(":review-open '{current}'<ret>")).await?;
    assert_eq!(doc!(app.editor).id(), draft_id);
    assert_eq!(doc!(app.editor).text().to_string(), draft);
    assert!(!fixture.repo.file.exists());
    assert!(!dir.join("new.txt").exists());
    assert_eq!(git_ok(dir, &["status", "--porcelain"]), status);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_pr_review_failed_snapshot_never_leaves_a_pr_attached() -> anyhow::Result<()> {
    use helix_view::editor::ReviewMode;
    let oversized = "head\n".repeat(helix_view::review::Review::MAX_LINES + 1);
    let fixture = PrFixture::new("base\n", &oversized, r#"api\ *) echo '[]';;"#);
    let dir = fixture.repo.dir.path();
    let _cwd = Cwd::enter(dir);
    git_ok(dir, &["checkout", "-q", "feature"]);
    fixture.repo.write("saved work\n");
    let mut app = fixture.repo.app()?;
    app.editor.review_mode = Some(ReviewMode::Review);
    for _ in 0..2 {
        send_keys(&mut app, ":review-pr-attach 7<ret>").await?;
        wait_for(&mut app, |app| !app.editor.review_pr_loading).await?;
        assert_error(&app, "review limit exceeded");
        assert!(app.editor.review_pr.is_none());
        assert!(app.editor.review_baseline.is_none());
        let location = serde_json::json!({"mode": "review", "filePath": "file.txt"});
        send_keys(&mut app, &format!(":review-open '{location}'<ret>")).await?;
        assert_error(&app, "Attach the requested pull request");
    }
    assert_eq!(fixture.repo.disk(), "saved work\n");
    Ok(())
}
