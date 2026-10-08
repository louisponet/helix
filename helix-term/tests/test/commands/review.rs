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
    editor::{AutoSaveAfterDelay, Severity},
    graphics::Rect,
    input::parse_macro,
    review::{OldBlock, ReviewBaseline},
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
    test_key_sequences(
        &mut repo.app()?,
        vec![
            (
                Some(":review<ret>:review-comment retained<ret>"),
                Some(&|app| {
                    assert_status_not_error(&app.editor);
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
        let mut app = repo.app()?;
        let batch = repo.dir.path().join("batch.json");
        let export = format!(":review-export {}<ret>", batch.display());
        send_keys_and_paste(
            &mut app,
            ":review<ret>:review-comment retained<ret>i",
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
                }),
            ),
            (
                // Esc puts the cursor on the line the removed block hangs under.
                Some("geZ<esc>j"),
                Some(&|app| {
                    assert_eq!(old_focus(app), None);
                    assert_eq!(cursor_line(app), 150);
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

    send_keys_and_paste(&mut app, ":review<ret>:review-old<ret>x", "pasted").await?;
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
                    assert_status_not_error(&app.editor);
                    assert_eq!(app.editor.review_batch.comments().len(), 1);
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
    test_key_sequences(
        &mut repo.app()?,
        vec![
            (Some(":review HEAD<ret>:review-old<ret>x:review-comment historical<ret><esc>"), Some(&|app| {
                assert_status_not_error(&app.editor);
                assert_eq!(doc!(app.editor).text(), "latest\nkeep\n");
                assert!(render(app).iter().any(|(_, text)| text == "committed"));
                assert_eq!(doc!(app.editor).review.as_ref().unwrap().baseline(), &baseline);
            })),
            (Some("ix<esc>:review missing-baseline-ref<ret>"), Some(&|app| {
                assert_error(app, "missing-baseline-ref");
                assert_eq!(doc!(app.editor).text(), "xlatest\nkeep\n");
                assert!(doc!(app.editor).is_modified());
                assert_eq!(doc!(app.editor).review.as_ref().unwrap().baseline(), &baseline);
                assert_eq!(app.editor.review_batch.comments().len(), 1);
            })),
            (Some(":review<ret>:review-old<ret>x:review-comment staged<ret><esc>"), Some(&|app| {
                assert_status_not_error(&app.editor);
                assert_eq!(doc!(app.editor).text(), "xlatest\nkeep\n");
                assert!(render(app).iter().any(|(_, text)| text == "staged"));
                assert_eq!(app.editor.review_batch.comments().len(), 2);
            })),
            (Some("u:review HEAD<ret>"), Some(&|app| {
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
                assert_eq!(json["comments"][0]["baseline"], json["files"][0]["baseline"]);
                assert_eq!(json["comments"][0]["selected_text"], "committed\n");
                assert_eq!(json["comments"][1]["baseline"], serde_json::json!({"kind":"git-index", "blob_oid":index_oid}));
                assert_eq!(json["comments"][1]["selected_text"], "staged\n");
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

    test_key_sequences(
        &mut repo.app()?,
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
    let comments = |app: &Application| -> Vec<String> {
        app.editor
            .review_batch
            .comments()
            .iter()
            .map(|comment| comment.comment.clone())
            .collect()
    };
    let status = |app: &Application| app.editor.get_status().unwrap().0.to_string();

    test_key_sequences(
        &mut repo.app()?,
        vec![
            (
                Some(":review-uncomment<ret>"),
                Some(&|app| assert_error(app, "no pending review comments")),
            ),
            (
                Some(concat!(
                    ":review<ret>:review-comment one<ret>:review-comment two<ret>",
                    ":review-old<ret>:review-comment three<ret>:review-comment four<ret>",
                    ":review-uncomment<ret>",
                )),
                Some(&|app| {
                    assert_eq!(comments(app), ["one", "two", "three"]);
                    assert_eq!(status(app), "Dropped 1 review comment, 3 pending");
                }),
            ),
            (
                Some("<esc>:review-uncomment 2<ret>"),
                Some(&|app| {
                    assert_eq!(comments(app), ["one"]);
                    assert_eq!(status(app), "Dropped 2 review comments, 1 pending");
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
        ],
        false,
    )
    .await?;
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
    let batch = repo.dir.path().join("added.json");
    let keys = format!(
        ":review HEAD~1<ret>x:review-comment new file<ret>:review-export {}<ret>",
        batch.display()
    );
    test_key_sequences(&mut repo.app()?, vec![(Some(&keys), Some(&|app| {
        assert_status_not_error(&app.editor);
        assert_eq!(doc!(app.editor).text(), "added later\n");
        assert!(doc!(app.editor).review.as_ref().unwrap().blocks().is_empty());
        let json: serde_json::Value = serde_json::from_str(&fs::read_to_string(&batch).unwrap()).unwrap();
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
    test_key_sequences(
        &mut repo.app()?,
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
async fn handoff_exports_unmodified_reviews_on_write_and_suspends_auto_save() -> anyhow::Result<()>
{
    let repo = Repo::new("old\nkeep\n", "old\nkeep\n", "old\nkeep\n");
    let other = repo.dir.path().join("other.txt");
    fs::write(&other, "other\n")?;
    git_ok(repo.dir.path(), &["add", "other.txt"]);
    git_ok(repo.dir.path(), &["commit", "-m", "other"]);
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

    // Without a handoff directory, auto-save writes reviewed buffers.
    send_keys(&mut app, ":review-baseline HEAD<ret>ix<esc>").await?;
    settle().await;
    send_keys(&mut app, "<esc>").await?;
    assert!(doc!(app.editor).review.is_some());
    assert!(!doc!(app.editor).is_modified());
    assert_eq!(repo.disk(), "xold\nkeep\n");

    let handoff = format!(":review-handoff {}<ret>", outbox.path().display());
    send_keys(&mut app, &format!("{handoff}iy<esc>")).await?;
    settle().await;
    send_keys(&mut app, "<esc>").await?;
    assert!(doc!(app.editor).is_modified());
    assert_eq!(repo.disk(), "xold\nkeep\n");
    assert_eq!(fs::read_dir(outbox.path())?.count(), 0);

    let keys = format!(
        ":o {}<ret>iz<esc>:review-comment still editing<ret>:o {}<ret>:review-comment mine<ret>:w<ret>",
        other.display(),
        repo.file.display()
    );
    send_keys(&mut app, &keys).await?;

    assert_eq!(repo.disk(), "xyold\nkeep\n");
    assert_eq!(fs::read_to_string(&other)?, "other\n");
    let exported: Vec<_> = fs::read_dir(outbox.path())?
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(exported.len(), 1, "{exported:?}");
    let name = exported[0].file_name().unwrap().to_str().unwrap();
    assert!(
        name.ends_with(&format!("-{}.json", std::process::id())),
        "{name}"
    );
    let (status, _) = app.editor.get_status().unwrap();
    assert!(status.contains(name), "{status}");

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
    let pending = app.editor.review_batch.comments();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].comment, "still editing");

    send_keys(&mut app, ":q!<ret>:q!<ret>").await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn review_files_picks_files_changed_since_the_baseline() -> anyhow::Result<()> {
    let repo = Repo::new("old\n", "old\n", "old\n");
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
    repo.write("new\n");
    let _cwd = Cwd::enter(dir);
    let opened = |name: &'static str| {
        move |app: &Application| {
            assert_status_not_error(&app.editor);
            let doc = doc!(app.editor);
            assert_eq!(doc.path().unwrap().file_name().unwrap(), name);
            assert!(doc.review.is_some());
        }
    };
    let (added, staged, gone, file) = (
        opened("added.txt"),
        opened("staged.txt"),
        opened("gone.txt"),
        opened("file.txt"),
    );

    test_key_sequences(
        &mut repo.app()?,
        vec![
            (
                Some(":review-files<ret>"),
                Some(&|app| assert_error(app, "no review baseline")),
            ),
            (
                Some(":review-baseline HEAD<ret>:o same.txt<ret>:review-files<ret>"),
                None,
            ),
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
                Some(":review-close<ret>:reload<ret>"),
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
        ":review-handoff {}<ret>:review-comment looks good<ret>:review-send<ret>",
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
    assert_eq!(json["comments"][0]["comment"], "looks good");
    assert!(app.editor.review_batch.comments().is_empty());

    send_keys(&mut app, "ix<esc>:review-send<ret>").await?;
    assert_error(&app, "unsaved changes: file.txt");
    assert_eq!(exported().len(), 1);
    assert_eq!(modified(), before);

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
