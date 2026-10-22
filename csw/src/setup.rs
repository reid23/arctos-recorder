//! The rig's setup as a base station last gave it (see
//! [`protocol::Instructions::setup`]), kept so that whichever base station
//! the camera connects to next stitches it the same way.

use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;

/// The setup's file in the camera's data directory.
pub const FILE: &str = "setup.json";

pub struct KeptSetup {
    path: PathBuf,
    /// What the file holds.
    setup: Option<serde_json::Value>,
}

impl KeptSetup {
    /// Reads the setup kept at `path`. One that can't be read counts as
    /// none, so that the next base station's replaces it.
    pub fn load(path: PathBuf) -> Self {
        fn read(path: &Path) -> Result<Option<serde_json::Value>> {
            match std::fs::read(path) {
                Ok(json) => Ok(Some(serde_json::from_slice(&json)?)),
                Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            }
        }
        let setup = read(&path).unwrap_or_else(|e| {
            tracing::warn!("ignoring the setup at {}: {e:#}", path.display());
            None
        });
        Self { path, setup }
    }

    pub fn current(&self) -> Option<&serde_json::Value> {
        self.setup.as_ref()
    }

    /// Keeps `setup` in place of the current one, if it differs. The file is
    /// replaced whole, so losing power partway leaves the old setup.
    pub fn keep(&mut self, setup: serde_json::Value) -> Result<()> {
        if self.setup.as_ref() == Some(&setup) {
            return Ok(());
        }
        let partial = self.path.with_extension("json.partial");
        let mut file = std::fs::File::create(&partial)?;
        file.write_all(&serde_json::to_vec_pretty(&setup)?)?;
        file.sync_all()?;
        std::fs::rename(&partial, &self.path)?;
        self.setup = Some(setup);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_directory(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("csw-{name}-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn a_kept_setup_outlasts_a_restart() {
        let path = empty_directory("kept-setup").join(FILE);
        let mut kept = KeptSetup::load(path.clone());
        assert_eq!(kept.current(), None);
        let setup = serde_json::json!({"swap_sides": true, "blend": {"layers": 4}});
        kept.keep(setup.clone()).unwrap();
        assert_eq!(KeptSetup::load(path).current(), Some(&setup));
    }

    #[test]
    fn an_unreadable_setup_counts_as_none() {
        let path = empty_directory("unreadable-setup").join(FILE);
        std::fs::write(&path, r#"{"swap_sides": tr"#).unwrap();
        assert_eq!(KeptSetup::load(path).current(), None);
    }
}
