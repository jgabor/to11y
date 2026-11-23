//! A sandbox for running the built binary: its own home and cache
//! directories, a cleared environment, and the committed fixtures in place
//! of logins and the network.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

pub const BIN: &str = env!("CARGO_BIN_EXE_to11y");
pub const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
/// The time the fixtures were captured at.
pub const NOW: &str = "2026-11-23T14:31:10Z";

pub struct Sandbox {
    root: PathBuf,
    fixtures: PathBuf,
}

impl Sandbox {
    pub fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("to11y-it-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        Sandbox {
            root,
            fixtures: PathBuf::from(FIXTURES),
        }
    }

    /// Serves fixtures from a copy that the test can change.
    pub fn own_fixtures(mut self) -> Self {
        let dir = self.root.join("fixtures");
        std::fs::create_dir_all(&dir).unwrap();
        for entry in std::fs::read_dir(FIXTURES).unwrap() {
            let entry = entry.unwrap();
            std::fs::copy(entry.path(), dir.join(entry.file_name())).unwrap();
        }
        self.fixtures = dir;
        self
    }

    pub fn fixture(&self, name: &str, text: &str) {
        std::fs::write(self.fixtures.join(name), text).unwrap();
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The environment the binary runs with, and nothing else.
    pub fn vars(&self) -> Vec<(&'static str, PathBuf)> {
        vec![
            ("HOME", self.root.join("home")),
            ("XDG_CACHE_HOME", self.root.join("cache")),
            ("TO11Y_FIXTURES", self.fixtures.clone()),
            ("TO11Y_NOW", PathBuf::from(NOW)),
        ]
    }

    pub fn std_command(&self) -> std::process::Command {
        let mut command = std::process::Command::new(BIN);
        command.env_clear().envs(self.vars());
        command
    }

    pub fn tokio_command(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(BIN);
        command.env_clear().envs(self.vars());
        command
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
