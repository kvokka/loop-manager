//! External programs an addon asks doctor to check.
use regex::Regex;
use semver::Version;
use serde::{Deserialize, Deserializer, de::Error as _};
use std::fmt;

/// A version probe from an addon's manifest, run with the addon directory as its cwd.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dependency {
    pub command: Vec<String>,
    #[serde(deserialize_with = "minimum")]
    pub minimum: Version,
    #[serde(default)]
    lock: Lock,
    #[serde(default, deserialize_with = "pattern")]
    pattern: Option<Regex>,
    #[serde(default)]
    pub scope: DependencyScope,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DependencyScope {
    #[default]
    Session,
    Daemon,
    Both,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Lock {
    #[default]
    Minimum,
    Major,
    Minor,
    Exact,
}

/// A numeric major or major.minor has zeroes for its omitted components.
fn dependency_version(text: &str) -> Result<Version, semver::Error> {
    if text.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        match text.matches('.').count() {
            0 => return Version::parse(&format!("{text}.0.0")),
            1 => return Version::parse(&format!("{text}.0")),
            _ => {}
        }
    }
    Version::parse(text)
}

fn minimum<'de, D: Deserializer<'de>>(d: D) -> Result<Version, D::Error> {
    dependency_version(&String::deserialize(d)?).map_err(D::Error::custom)
}

fn pattern<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Regex>, D::Error> {
    let re = Regex::new(&String::deserialize(d)?).map_err(D::Error::custom)?;
    if re.captures_len() != 2 {
        return Err(D::Error::custom(
            "dependency pattern needs one capture group for the version",
        ));
    }
    Ok(Some(re))
}

impl fmt::Display for Dependency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let min = &self.minimum;
        match self.lock {
            Lock::Minimum => write!(f, ">= {min}"),
            Lock::Major => write!(f, ">= {min} within {}.x", min.major),
            Lock::Minor => write!(f, ">= {min} within {}.{}.x", min.major, min.minor),
            Lock::Exact => write!(f, "= {min}"),
        }
    }
}

impl Dependency {
    /// Read the probed version and check the inclusive floor and optional upper bound.
    pub fn check(&self, output: &str) -> Result<String, String> {
        let version = match &self.pattern {
            None => output.trim(),
            Some(re) => re
                .captures(output)
                .and_then(|m| m.get(1))
                .ok_or("version pattern did not match the probe output")?
                .as_str(),
        };
        let version = dependency_version(version).map_err(|e| format!("invalid SemVer from probe: {e}"))?;
        let min = &self.minimum;
        let order = version.cmp_precedence(min);
        let within = match self.lock {
            Lock::Minimum => true,
            Lock::Major => version.major == min.major,
            Lock::Minor => (version.major, version.minor) == (min.major, min.minor),
            Lock::Exact => order.is_eq(),
        };
        if order.is_lt() || !within {
            return Err(format!("installed {version} is outside the required range"));
        }
        Ok(format!("installed {version}"))
    }
}
