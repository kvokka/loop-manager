//! The version checks of the binaries the daemon runs: a harness binary from the inclusive minimum of its adapter,
//! with no upper bound (contracts 4.5); a tracker binary within the range its adapter was tested on. The daemon warns
//! at start, and `loop-manager doctor` does, when a binary fails its check.

use std::path::Path;

type Version = (u32, u32, u32);

/// The `--version` output of `binary`, or why it is not `minimum` or newer.
pub fn supported_version(binary: &Path, minimum: Version) -> Result<String, String> {
    let out = version_output(binary)?;
    if supported(&out, minimum) {
        return Ok(out);
    }
    Err(format!(
        "{} is `{out}`; supported: {} or newer",
        binary.display(),
        dotted(minimum)
    ))
}

/// The `--version` output of `binary`, or why it is not a tested version.
pub fn tested_version(binary: &Path, tested: [Version; 2]) -> Result<String, String> {
    let out = version_output(binary)?;
    match parse(&out) {
        Some(v) if (tested[0]..=tested[1]).contains(&v) => Ok(out),
        _ => Err(format!(
            "{} is `{out}`; tested: {} to {}",
            binary.display(),
            dotted(tested[0]),
            dotted(tested[1])
        )),
    }
}

/// The trimmed standard output of `binary --version`.
fn version_output(binary: &Path) -> Result<String, String> {
    match crate::orphans::output_std(std::process::Command::new(binary).arg("--version")) {
        Ok(out) => Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned()),
        Err(e) => Err(format!("{}: {e}", binary.display())),
    }
}

/// `--version` output that names `minimum` or a newer version.
pub(crate) fn supported(out: &str, minimum: Version) -> bool {
    parse(out).is_some_and(|v| v >= minimum)
}

fn dotted((a, b, c): Version) -> String {
    format!("{a}.{b}.{c}")
}

/// The first `major.minor.patch` word of `--version` output: `2.1.322 (Some Tool)`, `some-cli 0.167.2`.
pub(crate) fn parse(text: &str) -> Option<Version> {
    text.split_whitespace().find_map(|word| {
        let mut parts = word.split('.').map(|p| p.parse().ok());
        let version = (parts.next()??, parts.next()??, parts.next()??);
        parts.next().is_none().then_some(version)
    })
}

/// Run an addon-defined version probe with the same bounded process runner as an orchestrator hook.
pub async fn addon_version_probe(command: &[String], cwd: &Path, path: &str) -> Result<String, String> {
    use crate::process::{Exit, Spec, base_env, run};
    let mut env = base_env();
    env.retain(|(key, _)| key != "PATH");
    env.push(("PATH".into(), path.into()));
    let out = run(Spec {
        command,
        cwd,
        env: &env,
        stdin: &[],
        timeout: std::time::Duration::from_secs(5),
        keep: 16 * 1024,
        stop: None,
    })
    .await;
    let stderr = String::from_utf8_lossy(&out.stderr);
    match out.exit {
        Exit::Code(0) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            Ok(if stdout.trim().is_empty() {
                stderr.into_owned()
            } else {
                stdout.into_owned()
            })
        }
        Exit::SpawnFailed(e) => Err(format!("cannot run probe: {e}")),
        Exit::TimedOut => Err("version probe timed out".into()),
        exit => Err(format!(
            "probe failed ({exit:?}): {}",
            stderr.lines().next().unwrap_or_default()
        )),
    }
}
