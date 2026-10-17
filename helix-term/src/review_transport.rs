//! Nonce-bound review actions carried by bracketed paste, independent of editor keymaps.

use std::{collections::VecDeque, io::Write};

use anyhow::{bail, ensure, Context as _};
use helix_view::document::Mode;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{commands, compositor, ui::EditorView};

const PREFIX: &str = "# hx-review:";
const MAX_JSON: usize = 16 * 1024;
const MAX_TEXT: usize = 8 * 1024;
const MAX_RESPONSES: usize = 128;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Action {
    OpenFile,
    Comment,
    DiscardLastComment,
    Files,
    Previous,
    Next,
    Ask,
    Stage,
    MarkReviewed,
    UnmarkReviewed,
    Send,
    PrReview,
    PrComment,
    PrSuggestion,
    PrComplete,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Request {
    v: u8,
    id: String,
    action: Action,
    text: Option<String>,
}

pub(crate) struct Session {
    nonce: String,
    active: bool,
    deferred: bool,
    responses: VecDeque<(Request, Value, Option<Value>)>,
    last_state: Option<Value>,
    mobile: commands::review_mobile::Draft,
}

impl Session {
    pub(crate) fn from_env() -> Option<Self> {
        let nonce = std::env::var("HELIX_REVIEW_ACTION_TOKEN").ok()?;
        valid_uuid(&nonce).then(|| Self {
            nonce,
            active: false,
            deferred: std::env::var_os("HELIX_REVIEW_DEFER_READY").is_some(),
            responses: VecDeque::new(),
            last_state: None,
            mobile: commands::review_mobile::Draft::default(),
        })
    }

    fn emit(&self, payload: &Value) {
        let bytes = serde_json::to_vec(payload).expect("review response serializes");
        let mut stdout = std::io::stdout().lock();
        let _ = write!(
            stdout,
            "\x1b]777;hx-review;{};{}\x07",
            self.nonce,
            hex(&bytes)
        );
        let _ = stdout.flush();
    }

    pub(crate) fn ready(&mut self) {
        if self.deferred {
            return;
        }
        self.active = true;
        self.last_state = None;
        self.emit(&json!({ "v": 1, "kind": "ready" }));
    }

    pub(crate) fn sync_state(&mut self, editor: &helix_view::Editor) {
        if self.deferred && editor.review_launch_ready {
            self.deferred = false;
            self.ready();
        }
        if !self.active || editor.should_close() {
            return;
        }
        let controls = commands::review::review_controls(editor);
        if editor.review_pr.is_none() {
            self.mobile.clear();
        }
        let state = json!({
            "v": 1,
            "kind": "state",
            "canStage": controls.can_stage,
            "canMarkReviewed": controls.can_mark_reviewed,
            "reviewed": controls.reviewed,
            "reviewedCount": controls.reviewed_count,
            "totalCount": controls.total_count,
            "canOpenFile": true,
            "canOpenSnapshot": true,
            "canAsk": editor.review_handoff_dir().is_some() && helix_view::doc!(editor).path().is_some(),
            "canSend": editor.review_handoff_dir().is_some() && editor.review_snapshot != Some(helix_view::doc!(editor).id()),
            "canComment": editor.review_handoff_dir().is_some() && helix_view::doc!(editor).path().is_some(),
            "pendingCommentCount": editor.review_batch.comments().len(),
            "canReviewPr": editor.review_pr.is_some() && helix_view::doc!(editor).path().is_some(),
        });
        if self.last_state.as_ref() != Some(&state) {
            self.emit(&state);
            self.last_state = Some(state);
        }
    }

    pub(crate) fn closed(&mut self) {
        if self.active {
            self.active = false;
            self.emit(&json!({ "v": 1, "kind": "closed" }));
        }
    }

    fn response(
        &mut self,
        frame: &str,
        compositor: &mut compositor::Compositor,
        cx: &mut compositor::Context,
    ) -> (Value, Option<Value>) {
        let mut id = String::new();
        let parsed = (|| {
            ensure!(
                frame.len() <= PREFIX.len() + 36 + 1 + MAX_JSON * 2,
                "review action is too large"
            );
            let (nonce, payload) = frame[PREFIX.len()..]
                .split_once(':')
                .context("invalid review action frame")?;
            ensure!(
                nonce == self.nonce,
                "review action belongs to another HX session"
            );
            let bytes = unhex(payload)?;
            let value: Value =
                serde_json::from_slice(&bytes).context("invalid review action JSON")?;
            if let Some(request_id) = value
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| valid_uuid(id))
            {
                id = request_id.to_owned();
            }
            let request: Request =
                serde_json::from_value(value).context("invalid review action")?;
            ensure!(valid_uuid(&request.id), "invalid review action request id");
            Ok::<_, anyhow::Error>(request)
        })();
        let request = match parsed {
            Ok(request) => request,
            Err(err) => return (result(&id, Err(err)), None),
        };

        if let Some((previous, response, review)) = self
            .responses
            .iter()
            .find(|(previous, _, _)| previous.id == request.id)
        {
            return if previous == &request {
                (response.clone(), review.clone())
            } else {
                (
                    result(
                        &request.id,
                        Err(anyhow::anyhow!(
                            "review action request id was reused with different contents"
                        )),
                    ),
                    None,
                )
            };
        }

        let outcome = (|| {
            ensure!(request.v == 1, "unsupported review action version");
            if let Some(text) = &request.text {
                ensure!(text.len() <= MAX_TEXT, "review action text exceeds 8 KiB");
                ensure!(
                    matches!(
                        request.action,
                        Action::OpenFile
                            | Action::Comment
                            | Action::Ask
                            | Action::Send
                            | Action::PrComment
                            | Action::PrSuggestion
                            | Action::PrComplete
                    ),
                    "this review action does not accept text"
                );
            }
            if matches!(request.action, Action::Ask | Action::Comment) {
                ensure!(
                    request
                        .text
                        .as_ref()
                        .is_some_and(|text| !text.trim().is_empty()),
                    "the comment is empty"
                );
            }
            ensure!(
                cx.editor.mode() != Mode::Insert,
                "finish inserting before using review actions"
            );
            ensure!(
                compositor.layer_count() == 1,
                "close the HX prompt or picker before using review actions"
            );
            let view = compositor
                .find::<EditorView>()
                .context("HX editor is unavailable")?;
            ensure!(
                view.review_action_ready(),
                "finish the pending HX key command before using review actions"
            );
            cx.editor.clear_status();
            match request.action {
                Action::PrReview => {}
                Action::PrComment => self.mobile.comment(
                    cx.editor,
                    request.text.as_deref().context("missing comment action")?,
                )?,
                Action::PrSuggestion => self.mobile.suggestion(
                    cx.editor,
                    request
                        .text
                        .as_deref()
                        .context("missing suggestion action")?,
                )?,
                Action::PrComplete => self.mobile.complete(
                    cx.editor,
                    request
                        .text
                        .as_deref()
                        .context("missing completion action")?,
                )?,
                _ => commands::review::run_terminal_action(
                    cx,
                    request.action,
                    request.text.as_deref(),
                )?,
            }
            if let Some((status, helix_core::diagnostic::Severity::Error)) = cx.editor.get_status()
            {
                bail!("{status}");
            }
            Ok(())
        })();
        let review = matches!(
            request.action,
            Action::PrReview | Action::PrComment | Action::PrSuggestion | Action::PrComplete
        )
        .then(|| self.review_payload(cx.editor, Some(&request.id)));
        let response = result(&request.id, outcome);
        self.responses
            .push_back((request, response.clone(), review.clone()));
        if self.responses.len() > MAX_RESPONSES {
            self.responses.pop_front();
        }
        (response, review)
    }

    fn review_payload(&mut self, editor: &helix_view::Editor, request_id: Option<&str>) -> Value {
        let mut payload = match self.mobile.snapshot(editor) {
            Ok(snapshot) => json!({ "v": 1, "kind": "pr-review", "review": snapshot }),
            Err(error) => {
                json!({ "v": 1, "kind": "pr-review", "review": null, "error": format!("{error:#}") })
            }
        };
        if let Some(id) = request_id {
            payload["requestId"] = json!(id);
        }
        payload
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.closed();
    }
}

/// Reserved frames never reach a document or prompt, including stale or malformed frames.
pub(crate) fn handle_paste(
    session: Option<&mut Session>,
    paste: &str,
    compositor: &mut compositor::Compositor,
    cx: &mut compositor::Context,
) -> bool {
    if !paste.starts_with(PREFIX) {
        return false;
    }
    let Some(session) = session else {
        cx.editor
            .set_error("Review touch actions are not enabled in this HX session");
        return true;
    };
    // Discovery only advertises an editor which currently owns the terminal. It never
    // executes an action, and remains available while a prompt or insert mode is active.
    if paste == "# hx-review:discover" {
        if session.active {
            session.ready();
            session.sync_state(cx.editor);
            if cx.editor.review_pr.is_some() {
                let review = session.review_payload(cx.editor, None);
                session.emit(&review);
            }
        }
        return true;
    }
    let (response, review) = session.response(paste, compositor, cx);
    if let Some(error) = response.get("error").and_then(Value::as_str) {
        cx.editor.set_error(error.to_owned());
    }
    // Publish the new selection/progress before acknowledging a mark that moved on.
    session.sync_state(cx.editor);
    if let Some(review) = review {
        session.emit(&review);
    }
    session.emit(&response);
    true
}

fn result(id: &str, outcome: anyhow::Result<()>) -> Value {
    match outcome {
        Ok(()) => json!({ "v": 1, "kind": "result", "id": id, "ok": true }),
        Err(error) => {
            json!({ "v": 1, "kind": "result", "id": id, "ok": false, "error": format!("{error:#}") })
        }
    }
}

fn valid_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        encoded.push(DIGITS[(byte >> 4) as usize] as char);
        encoded.push(DIGITS[(byte & 15) as usize] as char);
    }
    encoded
}

fn unhex(encoded: &str) -> anyhow::Result<Vec<u8>> {
    ensure!(
        encoded.len() <= MAX_JSON * 2 && encoded.len().is_multiple_of(2),
        "invalid review action payload size"
    );
    encoded
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = (pair[0] as char)
                .to_digit(16)
                .context("invalid review action hex")?;
            let low = (pair[1] as char)
                .to_digit(16)
                .context("invalid review action hex")?;
            Ok(((high << 4) | low) as u8)
        })
        .collect()
}
