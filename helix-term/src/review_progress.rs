//! Per-file review progress kept in the file `HELIX_REVIEW_PROGRESS` names, for T3 Code.

use std::path::PathBuf;

use helix_view::{review::ReviewBaseline, Editor};
use serde_json::{json, Value};

pub(crate) struct Progress {
    path: PathBuf,
    last: Option<Value>,
}

impl Progress {
    pub(crate) fn from_env() -> Option<Self> {
        let path = std::env::var_os("HELIX_REVIEW_PROGRESS").filter(|path| !path.is_empty())?;
        Some(Self {
            path: path.into(),
            last: None,
        })
    }

    pub(crate) fn sync(&mut self, editor: &Editor) {
        let snapshot = snapshot(editor);
        if self.last.as_ref() == Some(&snapshot) {
            return;
        }
        let mut tmp = self.path.clone().into_os_string();
        tmp.push(".tmp");
        let result = std::fs::write(&tmp, snapshot.to_string())
            .and_then(|()| std::fs::rename(&tmp, &self.path));
        if let Err(err) = result {
            log::warn!(
                "could not write review progress to {}: {err}",
                self.path.display()
            );
        }
        // Recorded even on failure, so a broken path logs once per change rather than per frame.
        self.last = Some(snapshot);
    }
}

/// Reviewed and total changes of each open file reviewed against a commit, counted as the
/// statusline counts them.
pub fn snapshot(editor: &Editor) -> Value {
    let pr = editor.review_pr.is_some();
    let mut files: Vec<_> = editor
        .documents()
        .filter(|doc| !editor.review_pr_loading && editor.review_diff != Some(doc.id()))
        .filter_map(|doc| {
            let path = doc.path()?;
            let review = doc
                .review
                .as_ref()
                .filter(|review| !review.is_suspended())?;
            let ReviewBaseline::GitCommit { commit_oid, .. } = review.baseline() else {
                return None;
            };
            let (reviewed, total) = review
                .review_changes(pr)
                .fold((0, 0), |(reviewed, total), (_, _, marked)| {
                    (reviewed + usize::from(marked), total + 1)
                });
            (total > 0).then(|| (path.to_string_lossy(), commit_oid, reviewed, total))
        })
        .collect();
    files.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let files: Vec<_> = files
        .into_iter()
        .map(|(path, baseline, reviewed, total)| {
            json!({ "path": path, "baseline": baseline, "reviewed": reviewed, "total": total })
        })
        .collect();
    json!({ "v": 1, "files": files })
}
