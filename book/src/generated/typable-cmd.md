| Name | Description |
| --- | --- |
| `:exit`, `:x`, `:xit` | Write changes to disk if the buffer is modified and then quit. Accepts an optional path (:exit some/path.txt). |
| `:exit!`, `:x!`, `:xit!` | Force write changes to disk, creating necessary subdirectories, if the buffer is modified and then quit. Accepts an optional path (:exit! some/path.txt). |
| `:quit`, `:q` | Close the current view. |
| `:quit!`, `:q!` | Force close the current view, ignoring unsaved changes. |
| `:open`, `:o`, `:edit`, `:e` | Open a file from disk into the current view. |
| `:buffer-close`, `:bc`, `:bclose` | Close the current buffer. |
| `:buffer-close!`, `:bc!`, `:bclose!` | Close the current buffer forcefully, ignoring unsaved changes. |
| `:buffer-close-others`, `:bco`, `:bcloseother` | Close all buffers but the currently focused one. |
| `:buffer-close-others!`, `:bco!`, `:bcloseother!` | Force close all buffers but the currently focused one. |
| `:buffer-close-all`, `:bca`, `:bcloseall` | Close all buffers without quitting. |
| `:buffer-close-all!`, `:bca!`, `:bcloseall!` | Force close all buffers ignoring unsaved changes without quitting. |
| `:buffer-next`, `:bn`, `:bnext` | Goto next buffer. |
| `:buffer-previous`, `:bp`, `:bprev` | Goto previous buffer. |
| `:write`, `:w` | Write changes to disk. Accepts an optional path (:write some/path.txt) |
| `:write!`, `:w!` | Force write changes to disk creating necessary subdirectories. Accepts an optional path (:write! some/path.txt) |
| `:write-buffer-close`, `:wbc` | Write changes to disk and closes the buffer. Accepts an optional path (:write-buffer-close some/path.txt) |
| `:write-buffer-close!`, `:wbc!` | Force write changes to disk creating necessary subdirectories and closes the buffer. Accepts an optional path (:write-buffer-close! some/path.txt) |
| `:new`, `:n` | Create a new scratch buffer. |
| `:format`, `:fmt` | Format the file using an external formatter or language server. |
| `:indent-style` | Set the indentation style for editing. ('t' for tabs or 1-16 for number of spaces.) |
| `:line-ending` | Set the document's default line ending. Options: crlf, lf. |
| `:earlier`, `:ear` | Jump back to an earlier point in edit history. Accepts a number of steps or a time span. |
| `:later`, `:lat` | Jump to a later point in edit history. Accepts a number of steps or a time span. |
| `:write-quit`, `:wq` | Write changes to disk and close the current view. Accepts an optional path (:wq some/path.txt) |
| `:write-quit!`, `:wq!` | Write changes to disk and close the current view forcefully. Accepts an optional path (:wq! some/path.txt) |
| `:write-all`, `:wa` | Write changes from all buffers to disk. |
| `:write-all!`, `:wa!` | Forcefully write changes from all buffers to disk creating necessary subdirectories. |
| `:write-quit-all`, `:wqa`, `:xa` | Write changes from all buffers to disk and close all views. |
| `:write-quit-all!`, `:wqa!`, `:xa!` | Forcefully write changes from all buffers to disk, creating necessary subdirectories, and close all views (ignoring unsaved changes). |
| `:quit-all`, `:qa` | Close all views. |
| `:quit-all!`, `:qa!` | Force close all views ignoring unsaved changes. |
| `:cquit`, `:cq` | Quit with exit code (default 1). Accepts an optional integer exit code (:cq 2). |
| `:cquit!`, `:cq!` | Force quit with exit code (default 1) ignoring unsaved changes. Accepts an optional integer exit code (:cq! 2). |
| `:theme` | Change the editor theme (show current theme if no name specified). |
| `:yank-join` | Yank joined selections. A separator can be provided as first argument. Default value is newline. |
| `:clipboard-yank` | Yank main selection into system clipboard. |
| `:clipboard-yank-join` | Yank joined selections into system clipboard. A separator can be provided as first argument. Default value is newline. |
| `:primary-clipboard-yank` | Yank main selection into system primary clipboard. |
| `:primary-clipboard-yank-join` | Yank joined selections into system primary clipboard. A separator can be provided as first argument. Default value is newline. |
| `:clipboard-paste-after` | Paste system clipboard after selections. |
| `:clipboard-paste-before` | Paste system clipboard before selections. |
| `:clipboard-paste-replace` | Replace selections with content of system clipboard. |
| `:primary-clipboard-paste-after` | Paste primary clipboard after selections. |
| `:primary-clipboard-paste-before` | Paste primary clipboard before selections. |
| `:primary-clipboard-paste-replace` | Replace selections with content of system primary clipboard. |
| `:show-clipboard-provider` | Show clipboard provider name in status bar. |
| `:change-current-directory`, `:cd` | Change the current working directory. |
| `:show-directory-stack` | Show the directory stack as a <space> delimited string. |
| `:push-directory`, `:pushd` | Save and then change the current directory. |
| `:pop-directory`, `:popd` | Remove the top entry from the directory stack, and cd to the new top directory.. |
| `:show-directory`, `:pwd` | Show the current working directory. |
| `:encoding` | Set encoding. Based on `https://encoding.spec.whatwg.org`. |
| `:character-info`, `:char` | Get info about the character under the primary cursor. |
| `:reload`, `:rl` | Discard changes and reload from the source file. |
| `:reload-all`, `:rla` | Discard changes and reload all documents from the source files. |
| `:update`, `:u` | Write changes only if the file has been modified. |
| `:lsp-workspace-command` | Open workspace command picker |
| `:lsp-restart` | Restarts the given language servers, or all language servers that are used by the current file if no arguments are supplied |
| `:lsp-stop` | Stops the given language servers, or all language servers that are used by the current file if no arguments are supplied |
| `:tree-sitter-scopes` | Display tree sitter scopes, primarily for theming and development. |
| `:tree-sitter-highlight-name` | Display name of tree-sitter highlight scope under the cursor. |
| `:tree-sitter-layers` | Display language names of tree-sitter injection layers under the cursor. |
| `:debug-start`, `:dbg` | Start a debug session from a given template with given parameters. |
| `:debug-remote`, `:dbg-tcp` | Connect to a debug adapter by TCP address and start a debugging session from a given template with given parameters. |
| `:debug-eval` | Evaluate expression in current debug context. |
| `:vsplit`, `:vs` | Open the file in a vertical split. |
| `:vsplit-new`, `:vnew` | Open a scratch buffer in a vertical split. |
| `:hsplit`, `:hs`, `:sp` | Open the file in a horizontal split. |
| `:hsplit-new`, `:hnew` | Open a scratch buffer in a horizontal split. |
| `:tutor` | Open the tutorial. |
| `:goto`, `:g` | Goto line number. |
| `:set-language`, `:lang` | Set the language of current buffer (show current language if no value specified). |
| `:set-option`, `:set` | Set a config option at runtime.<br>For example to disable smart case search, use `:set search.smart-case false`. |
| `:toggle-option`, `:toggle` | Toggle a config option at runtime.<br>For example to toggle smart case search, use `:toggle search.smart-case`. |
| `:get-option`, `:get` | Get the current value of a config option. |
| `:sort` | Sort ranges in selection. |
| `:reflow` | Hard-wrap the current selection of lines to a given width. |
| `:tree-sitter-subtree`, `:ts-subtree` | Display the smallest tree-sitter subtree that spans the primary selection, primarily for debugging queries. |
| `:config-reload` | Refresh user config. |
| `:config-open` | Open the user config.toml file. |
| `:config-open-workspace` | Open the workspace config.toml file. |
| `:log-open` | Open the helix log file. |
| `:insert-output` | Run shell command, inserting output before each selection. |
| `:append-output` | Run shell command, appending output after each selection. |
| `:pipe`, `:\|` | Pipe each selection to the shell command. |
| `:pipe-to` | Pipe each selection to the shell command, ignoring output. |
| `:run-shell-command`, `:sh`, `:!` | Run a shell command |
| `:review` | Review the buffer against the :review-baseline commit if one is set, else the git index, showing old lines inline. 'index' selects the git index, any other argument a commit revision such as HEAD~1. |
| `:review-commits` | Choose a commit baseline from the latest 200 commits reachable from HEAD, then review against the current buffer. |
| `:review-old` | Focus the old text nearest the cursor. It can be selected, yanked and commented on but not edited; Esc returns. |
| `:review-refresh` | Reload an unmodified reviewed buffer from disk and refresh its index baseline or retain its selected commit. |
| `:review-comment` | Add a pending review comment on the primary selection, or on the selected old text. Use %% for a literal %. |
| `:review-comments` | Pick a pending review comment and jump to its line. |
| `:review-uncomment` | Drop the most recently added pending review comment, or the given number of latest ones. |
| `:review-export` | Write pending review comments and reviewed files as JSON to a new file. Reviewed buffers must be written first. |
| `:review-revert` | Replace the changed lines under the cursor, or the focused old text's change, with the review baseline's lines. |
| `:review-stage` | Stage the unstaged hunk under the cursor (or the focused old text's, or the :review-diff hunk's) in the git index, like git add -p. Refuses unsaved buffers. |
| `:review-unstage` | Unstage the staged hunk under the cursor (or the focused old text's, or the :review-diff hunk's): the index gets the baseline's lines back for it. |
| `:review-toggle-stage` | Stage the hunk under the cursor, or unstage it if it is already staged (Enter while reviewing). |
| `:review-close` | Stop reviewing the buffer. Pending comments are kept for :review-export. |
| `:review-baseline` | Review every unmodified buffer in the workspace, and buffers opened later, against a commit revision such as HEAD. Repeating the same commit refreshes the reviews; `clear` closes them. |
| `:review-handoff` | On every write of a reviewed buffer, export a review batch to a new file in the given directory; auto-save skips reviewed buffers meanwhile. `clear` stops it. |
| `:review-send` | Export a review batch to the handoff directory now, as a write would, without writing any file. Refuses while a reviewed buffer has unsaved changes. |
| `:review-diff` | Show a unified diff of every workspace file changed since the review baseline commit in a scratch buffer. |
| `:review-diff-open` | In the :review-diff buffer, open the file of the hunk under the cursor at the matching current line. |
| `:review-files` | Pick a workspace file added (A), modified (M) or deleted (D) since the review baseline commit, and open it at its first change. |
| `:reset-diff-change`, `:diffget`, `:diffg` | Reset the diff change at the cursor position. |
| `:clear-register` | Clear given register. If no argument is provided, clear all registers. |
| `:set-register` | Set contents of the given register. |
| `:redraw` | Clear and re-render the whole UI |
| `:move`, `:mv` | Move the current buffer and its corresponding file to a different path |
| `:move!`, `:mv!` | Move the current buffer and its corresponding file to a different path creating necessary subdirectories |
| `:yank-diagnostic` | Yank diagnostic(s) under primary cursor to register, or clipboard by default |
| `:read`, `:r` | Load a file into buffer |
| `:echo` | Prints the given arguments to the statusline. |
| `:noop` | Does nothing. |
| `:workspace-trust` | Allow language servers and local config for the current workspace. |
| `:workspace-untrust` | Revoke the current workspace's trust grant or exclusion. |
| `:workspace-exclude` | Mark the current workspace as never-prompt. Never prompts for trust again. |
