//! The skills plugin of Claude Code and Codex in `plugins/loop-manager/` (docs/guide/recovery.md): its manifests are the
//! shape both harnesses install, and its skills name only commands the CLI has.

use clap::{CommandFactory, Parser};
use loop_manager_front::cli::Cli;
use serde_json::Value;
use std::{fs, path::Path};

fn json(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

/// A named plugin in a marketplace file.
fn entry<'a>(market: &'a Value, name: &str) -> &'a Value {
    assert!(market["name"].is_string(), "{market}");
    market["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == name)
        .unwrap()
}

/// The repository is a marketplace of plugins in the current format of each harness: Claude Code reads
/// `.claude-plugin/marketplace.json` and the plugin's `.claude-plugin/plugin.json`, Codex reads
/// `.agents/plugins/marketplace.json` and the plugin's root `plugin.json` (Claude Code 2.1, Codex 0.171). Codex loads
/// no hooks of a root `plugin.json`, so a plugin with hooks has `.codex-plugin/plugin.json` instead. Neither relies on a
/// format the other harness reads only for compatibility.
#[test]
fn the_manifests_are_what_claude_and_codex_install() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let claude = json(&root.join(".claude-plugin/marketplace.json"));
    assert!(claude["owner"]["name"].is_string(), "{claude}");
    let codex = json(&root.join(".agents/plugins/marketplace.json"));
    assert_eq!(claude["name"], "loop-manager");
    assert_eq!(codex["name"], "loop-manager");
    for name in ["loop-manager", "t3-run-card"] {
        check_plugin(&root, entry(&claude, name), entry(&codex, name));
    }
}

fn check_plugin(root: &Path, claude_entry: &Value, codex_entry: &Value) {
    assert_eq!(codex_entry["source"]["source"], "local", "{codex_entry}");
    for key in ["installation", "authentication"] {
        assert!(codex_entry["policy"][key].is_string(), "{codex_entry}");
    }
    assert!(codex_entry["category"].is_string(), "{codex_entry}");
    let source = claude_entry["source"].as_str().unwrap();
    assert!(source.starts_with("./") && !source.contains(".."), "{source}");
    assert_eq!(
        codex_entry["source"]["path"], source,
        "both marketplaces point to one plugin"
    );
    let dir = root.join(source);
    // A root `plugin.json` would shadow `.codex-plugin/plugin.json` in Codex.
    let codex = match dir.join("hooks").exists() {
        true => ".codex-plugin/plugin.json",
        false => "plugin.json",
    };
    assert!(
        !dir.join(".codex-plugin").exists() || !dir.join("plugin.json").exists(),
        "one manifest of Codex"
    );
    let version = json(&dir.join(codex))["version"].clone();
    assert!(version.is_string());
    for manifest in [".claude-plugin/plugin.json", codex] {
        let plugin = json(&dir.join(manifest));
        let name = plugin["name"].as_str().unwrap();
        assert!(
            name.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "{name}"
        );
        assert_eq!(
            claude_entry["name"], name,
            "the Claude marketplace entry names the plugin of {manifest}"
        );
        assert_eq!(
            codex_entry["name"], name,
            "the Codex marketplace entry names the plugin of {manifest}"
        );
        // Both harnesses cache a plugin by its version: a release that changes a skill changes the version.
        assert_eq!(plugin["version"], version, "{manifest}");
    }
    if claude_entry["name"] == "t3-run-card" {
        // Codex starts a plugin's MCP server in its own cwd and expands no plugin root in it.
        let server = &json(&dir.join("mcp.json"))["mcpServers"]["t3-run-card"];
        assert_eq!(server["type"], "stdio");
        assert_eq!(server["cwd"], ".");
        assert_eq!(json(&dir.join(codex))["mcpServers"], "./mcp.json");
        return;
    }
    assert_eq!(version, env!("CARGO_PKG_VERSION"));
    for skill in fs::read_dir(dir.join("skills")).unwrap() {
        let skill = skill.unwrap().path();
        let text = fs::read_to_string(skill.join("SKILL.md")).unwrap();
        let front = text
            .strip_prefix("---\n")
            .and_then(|t| t.split_once("\n---\n"))
            .unwrap()
            .0;
        let field = |key: &str| front.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix(": "));
        assert_eq!(
            field("name"),
            skill.file_name().unwrap().to_str(),
            "{}",
            skill.display()
        );
        assert!(
            field("description").is_some_and(|d| !d.is_empty()),
            "{}",
            skill.display()
        );
    }
}

/// Words of the first command in a shell pipeline: a single-quoted part is one word, even with a pipe inside.
fn words(line: &str) -> Vec<String> {
    let (mut out, mut word, mut quoted) = (Vec::new(), String::new(), false);
    for c in line.chars() {
        match c {
            '\'' => quoted = !quoted,
            '|' if !quoted => break,
            ' ' if !quoted => out.extend((!word.is_empty()).then(|| std::mem::take(&mut word))),
            c => word.push(c),
        }
    }
    out.extend((!word.is_empty()).then_some(word));
    out
}

#[test]
fn command_words_keep_a_quoted_pipe_and_stop_at_the_next_command() {
    assert_eq!(
        words("loop-manager run start shop check --input 'name=left | right' --json | jq -r .run"),
        [
            "loop-manager",
            "run",
            "start",
            "shop",
            "check",
            "--input",
            "name=left | right",
            "--json"
        ]
    );
}

#[test]
fn every_command_a_skill_names_exists() {
    let skills = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/loop-manager/skills");
    let mut seen = 0;
    for skill in fs::read_dir(&skills).unwrap() {
        let text = fs::read_to_string(skill.unwrap().path().join("SKILL.md")).unwrap();
        let mut block = false;
        for line in text.lines().map(str::trim) {
            if line.starts_with("```") {
                block = line == "```sh";
                continue;
            }
            // A command line of a `sh` block parses with its options and values.
            if block && line.starts_with("loop-manager ") {
                if let Err(e) = Cli::try_parse_from(words(line)) {
                    panic!("`{line}`: {e}");
                }
                seen += 1;
            }
            // A command in running text names an existing command, and its flags exist on it.
            for span in line.split('`').skip(1).step_by(2) {
                if let Some(rest) = span.strip_prefix("loop-manager ") {
                    let mut command = Cli::command();
                    command.build();
                    let words = words(rest);
                    let path = words
                        .iter()
                        .position(|w| w.starts_with(['-', '<']))
                        .unwrap_or(words.len());
                    for word in &words[..path] {
                        let sub = command
                            .find_subcommand(word)
                            .unwrap_or_else(|| panic!("no command `{span}`"));
                        command = sub.clone();
                    }
                    for flag in words[path..].iter().filter_map(|w| w.strip_prefix("--")) {
                        let known = command.get_arguments().any(|a| a.get_long() == Some(flag));
                        assert!(known, "no flag `--{flag}` in `{span}`");
                    }
                    seen += 1;
                }
            }
        }
    }
    assert!(seen > 0, "no command found in {}", skills.display());
}

/// The t3-run-card hook, given a PostToolUse event with the option `auto_open` set to `option`: the context it adds.
fn auto_open(option: &str, event: Value) -> Option<String> {
    use std::io::Write;
    let hook = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/t3-run-card/hooks/auto_open.py");
    let mut child = std::process::Command::new("python3")
        .arg(hook)
        .env("T3CODE_THREAD_ID", "test-thread")
        .env("CLAUDE_PLUGIN_OPTION_AUTO_OPEN", option)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    write!(child.stdin.take().unwrap(), "{event}").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let reply: Value = serde_json::from_slice(&out.stdout).ok()?;
    Some(reply["hookSpecificOutput"]["additionalContext"].as_str()?.to_owned())
}

/// With auto-open on (the Claude Code option, or a Codex event), a started run, by the CLI or the MCP tool, asks for its card on the daemon the command named.
#[test]
fn the_run_card_opens_after_a_run_start_only_when_the_user_asked() {
    let bash = |command: &str, stdout: &str| {
        serde_json::json!({ "tool_name": "Bash", "tool_input": { "command": command },
            "tool_response": { "stdout": stdout, "stderr": "" } })
    };
    let bare = bash("loop-manager run start shop check", "run_7\n");
    assert!(auto_open("true", bare.clone()).unwrap().contains(r#"{"run": "run_7"}"#));
    assert_eq!(auto_open("false", bare), None);
    let other = bash(
        "loop-manager --socket /tmp/w.sock run start shop check --json",
        "{\n  \"run\": \"run_9\",\n  \"continue_from\": \"run_2\"\n}",
    );
    assert!(
        auto_open("true", other)
            .unwrap()
            .contains(r#"{"socket": "/tmp/w.sock", "run": "run_9"}"#)
    );
    assert_eq!(auto_open("true", bash("loop-manager run watch run_7", "run_7\n")), None);
    let mcp = serde_json::json!({ "tool_name": "mcp__loop-manager__start_run",
        "tool_response": [{ "type": "text", "text": "run: run_3\nnothing_ready: false" }] });
    assert!(auto_open("true", mcp).unwrap().contains(r#"{"run": "run_3"}"#));
    // Codex has no plugin options: the hook runs only once the user trusts it, and its event has a turn.
    let codex = serde_json::json!({ "tool_name": "Bash", "turn_id": "t1",
        "tool_input": { "command": "loop-manager run start shop check" },
        "tool_response": "Exit code: 0\nOutput:\nrun_5\n" });
    assert!(auto_open("", codex).unwrap().contains(r#"{"run": "run_5"}"#));
}
