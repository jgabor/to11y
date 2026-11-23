//! The environment, read once at startup so the rest of to11y is a function
//! of it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::time;

#[derive(Clone, Debug, Default)]
pub struct Env {
    pub home: Option<PathBuf>,
    pub xdg_cache_home: Option<PathBuf>,
    pub claude_config_dir: Option<PathBuf>,
    pub codex_home: Option<PathBuf>,
    pub dbus_address: Option<String>,
    pub path: Option<OsString>,
    /// Set in the environment of the `agy` that to11y starts, so a to11y
    /// started by that `agy` doesn't start another.
    pub no_agy: bool,
    /// `TO11Y_HARNESS`: the harness tools default to, over detection.
    pub harness: Option<String>,
    /// `TO11Y_NOW`, read only in debug builds.
    pub now: Option<i64>,
    /// `TO11Y_FIXTURES`, read only in debug builds.
    pub fixtures: Option<PathBuf>,
}

impl Env {
    pub fn from_process() -> Self {
        Self::from_vars(|name| std::env::var_os(name))
    }

    /// Unset and empty variables are the same. Relative paths are ignored,
    /// as the XDG spec says.
    pub fn from_vars(var: impl Fn(&str) -> Option<OsString>) -> Self {
        let text = |name: &str| {
            var(name)
                .filter(|value| !value.is_empty())
                .and_then(|value| value.into_string().ok())
        };
        let dir = |name: &str| {
            var(name)
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
        };
        let mut env = Env {
            home: dir("HOME"),
            xdg_cache_home: dir("XDG_CACHE_HOME"),
            claude_config_dir: dir("CLAUDE_CONFIG_DIR"),
            codex_home: dir("CODEX_HOME"),
            dbus_address: text("DBUS_SESSION_BUS_ADDRESS"),
            path: var("PATH").filter(|value| !value.is_empty()),
            no_agy: var("TO11Y_NO_AGY").is_some_and(|value| !value.is_empty()),
            harness: text("TO11Y_HARNESS"),
            now: None,
            fixtures: None,
        };
        if cfg!(debug_assertions) {
            env.now = text("TO11Y_NOW").and_then(|value| time::parse(&value));
            env.fixtures = dir("TO11Y_FIXTURES");
        }
        env
    }

    pub fn now(&self) -> i64 {
        self.now.unwrap_or_else(time::now)
    }

    fn in_home(&self, relative: &str) -> Option<PathBuf> {
        self.home.as_ref().map(|home| home.join(relative))
    }

    pub fn claude_dir(&self) -> Option<PathBuf> {
        self.claude_config_dir
            .clone()
            .or_else(|| self.in_home(".claude"))
    }

    pub fn codex_dir(&self) -> Option<PathBuf> {
        self.codex_home.clone().or_else(|| self.in_home(".codex"))
    }

    pub fn factory_dir(&self) -> Option<PathBuf> {
        self.in_home(".factory")
    }

    pub fn antigravity_dir(&self) -> Option<PathBuf> {
        self.in_home(".gemini/antigravity")
    }

    pub fn cache_dir(&self) -> Option<PathBuf> {
        self.xdg_cache_home
            .clone()
            .or_else(|| self.in_home(".cache"))
            .map(|dir| dir.join("to11y"))
    }

    /// The first executable file called `name` on `PATH`.
    pub fn find_program(&self, name: &str) -> Option<PathBuf> {
        use std::os::unix::fs::PermissionsExt;
        let path = self.path.as_ref()?;
        std::env::split_paths(path)
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join(name))
            .find(|file| {
                Path::new(file)
                    .metadata()
                    .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(vars: &[(&str, &str)]) -> Env {
        let vars: Vec<(String, OsString)> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), OsString::from(v)))
            .collect();
        Env::from_vars(|name| {
            vars.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        })
    }

    #[test]
    fn directories_default_to_the_home_directory() {
        let env = env(&[("HOME", "/home/u")]);
        assert_eq!(env.claude_dir(), Some("/home/u/.claude".into()));
        assert_eq!(env.codex_dir(), Some("/home/u/.codex".into()));
        assert_eq!(env.cache_dir(), Some("/home/u/.cache/to11y".into()));
    }

    #[test]
    fn variables_override_the_defaults() {
        let env = env(&[
            ("HOME", "/home/u"),
            ("CLAUDE_CONFIG_DIR", "/c"),
            ("CODEX_HOME", "/x"),
            ("XDG_CACHE_HOME", "/cache"),
        ]);
        assert_eq!(env.claude_dir(), Some("/c".into()));
        assert_eq!(env.codex_dir(), Some("/x".into()));
        assert_eq!(env.cache_dir(), Some("/cache/to11y".into()));
    }

    #[test]
    fn relative_and_empty_values_are_ignored() {
        let env = env(&[
            ("HOME", "/home/u"),
            ("XDG_CACHE_HOME", "cache"),
            ("TO11Y_HARNESS", ""),
        ]);
        assert_eq!(env.cache_dir(), Some("/home/u/.cache/to11y".into()));
        assert_eq!(env.harness, None);
        assert_eq!(super::Env::from_vars(|_| None).cache_dir(), None);
    }

    #[test]
    fn the_test_clock_is_read_in_debug_builds() {
        let env = env(&[("TO11Y_NOW", "2026-11-23T14:31:10Z")]);
        assert_eq!(env.now(), 1_795_444_270);
    }
}
