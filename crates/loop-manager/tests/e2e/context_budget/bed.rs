//! The bed of the hook tests: a stand-in `loop-manager`, a `br` that fails, transcripts and the hook run on them.

use super::*;

/// A hook test bed: a stand-in `loop-manager` that records the outcomes and the comments it is given, a `br` that
/// fails, so that a comment can reach the bead only through `loop-manager`, and the addon's data directory, with a
/// budget of 1000 soft, 7000 hard and a note every 500.
pub(super) struct Bed {
    dir: tempfile::TempDir,
    /// What the hooks printed, to look for the hard limit in.
    said: std::cell::RefCell<String>,
    /// Whether the run has a bead: `LOOP_MANAGER_BEAD` is set for the hook.
    pub(super) with_bead: std::cell::Cell<bool>,
}

impl Bed {
    pub(super) fn new(cut: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        fs::create_dir_all(d.join("bin")).unwrap();
        fs::create_dir_all(d.join("data")).unwrap();
        fs::create_dir_all(d.join("work")).unwrap();
        git(&d.join("work"), &["init", "-q"]);
        commit(&d.join("work"), "start");
        fs::write(d.join("work/notes.md"), "half done").unwrap();
        // A comment's text is one JSON string per line of `comments`; any other call is one line of `finished`.
        let fake = "#!/bin/sh\nbin=$(dirname \"$0\")\ncase $1 in\n\
                    comment) [ \"$2\" = --file ] && jq -Rs . \"$3\" >>\"$bin/comments\" || exit 2 ;;\n\
                    *) echo \"$*\" >>\"$bin/finished\" ;;\nesac\necho accepted\n";
        let br = "#!/bin/sh\necho \"$*\" >>\"$(dirname \"$0\")/br-called\"\nexit 127\n";
        for (name, text) in [("loop-manager", fake), ("br", br)] {
            fs::write(d.join("bin").join(name), text).unwrap();
            fs::set_permissions(d.join("bin").join(name), fs::Permissions::from_mode(0o755)).unwrap();
        }
        fs::write(d.join("cut"), cut).unwrap();
        Bed {
            dir,
            said: Default::default(),
            with_bead: std::cell::Cell::new(true),
        }
    }

    pub(super) fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Writes the transcript `name` whose last answer of the model holds `tokens` of context, after a tool call.
    pub(super) fn transcript(&self, name: &str, tokens: u64) -> String {
        let usage = |input: u64| {
            json!({ "input_tokens": input / 2, "cache_creation_input_tokens": 10, "cache_read_input_tokens":
                input - input / 2 - 30, "output_tokens": 20 })
        };
        let lines = [
            json!({ "type": "user", "message": { "role": "user", "content": "the usage of the parser" } }),
            json!({ "type": "assistant", "message": { "model": "m", "usage": usage(100), "content": [
                { "type": "text", "text": "I read the parser first." },
                { "type": "tool_use", "name": "Bash", "input": { "command": "cargo test -p parser" } }] } }),
            json!({ "type": "assistant", "message": { "model": "m", "usage": usage(tokens), "content": [
                { "type": "tool_use", "name": "Edit", "input": { "file_path": "src/parser.rs" } }] } }),
            json!({ "type": "assistant", "message": { "model": "<synthetic>", "usage": usage(99999),
                "content": [{ "type": "text", "text": "No response requested." }] } }),
        ];
        let path = self.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        fs::write(&path, text).unwrap();
        path.display().to_string()
    }

    /// The hook on `input` (with `cwd` and `transcript_path` of the session `s1` added), its stdout parsed, or null
    /// when it printed nothing; the hook succeeded.
    pub(super) fn hook(&self, mut input: Value) -> Value {
        input["session_id"] = json!("s1");
        input["cwd"] = json!(self.path().join("work"));
        if input.get("transcript_path").is_none() {
            input["transcript_path"] = json!(self.path().join("s1.jsonl"));
        }
        let d = self.path();
        let cut = fs::read_to_string(d.join("cut")).unwrap();
        let data = d.join("data");
        let mut command = common::program(addon().join("hooks/budget"));
        command
            .args([data.to_str().unwrap(), "1000", "7000", "500", &cut])
            .env("LOOP_MANAGER_RUN", "r1")
            .env("RUST_LOG", "error")
            .env(
                "PATH",
                format!("{}:{}", d.join("bin").display(), std::env::var("PATH").unwrap()),
            );
        if self.with_bead.get() {
            command.env("LOOP_MANAGER_BEAD", "t-1");
        }
        let mut child = command.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "budget on {input}");
        let text = String::from_utf8(out.stdout).unwrap();
        self.said.borrow_mut().push_str(&text);
        if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap()
        }
    }

    pub(super) fn tool(&self, event: &str, tool: &str) -> Value {
        self.hook(json!({ "hook_event_name": event, "tool_name": tool }))
    }

    /// The texts of the comments the hooks left with `loop-manager comment`, oldest first; no hook called `br`.
    pub(super) fn comments(&self) -> Vec<String> {
        let br = fs::read_to_string(self.path().join("bin/br-called")).unwrap_or_default();
        assert_eq!(br, "", "br was called");
        let out = fs::read_to_string(self.path().join("bin/comments")).unwrap_or_default();
        out.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    pub(super) fn finished(&self) -> String {
        fs::read_to_string(self.path().join("bin/finished")).unwrap_or_default()
    }

    /// Nothing the hooks said holds the hard limit.
    pub(super) fn kept_hard_quiet(&self) {
        let said = self.said.borrow();
        assert!(!said.contains("7k") && !said.contains("7000"), "{said}");
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = common::program("git").args(args).current_dir(dir).output().unwrap();
    assert!(out.status.success(), "git {args:?}");
}

pub(super) fn commit(dir: &Path, message: &str) {
    let id = ["-c", "user.name=t", "-c", "user.email=t@t"];
    git(
        dir,
        &[&id[..], &["commit", "-q", "--allow-empty", "-m", message]].concat(),
    );
}

pub(super) fn note(answer: &Value) -> &str {
    answer["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or_default()
}
