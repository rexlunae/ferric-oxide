//! Installer TOML configuration loading and precedence across included files.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Display;
use std::fs;
use std::mem;
use std::path::{Path, PathBuf};

use anyhow::Context;
use anyhow::Result;

use crate::PackageConfig;

/// File and directory declarations.
pub mod file;
#[cfg(feature = "installer")]
/// Host filesystem operations for file declarations.
pub mod file_impl;
/// Image-wide installation settings.
pub mod general;
/// Package selection and source overrides.
pub mod package;
/// User and group account declarations.
pub mod user;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
/// An image configuration with ordered includes and installation declarations.
///
/// # Examples
/// ```
/// let config = redox_installer::Config::default();
/// assert!(config.packages.is_empty());
/// ```
pub struct Config {
    #[serde(default)]
    /// Configurations to load relative to this file before applying its overrides.
    pub include: Vec<PathBuf>,
    #[serde(default)]
    /// Image-wide installation settings.
    pub general: general::GeneralConfig,
    #[serde(default)]
    /// Packages selected for installation, keyed by package name.
    pub packages: BTreeMap<String, package::PackageConfig>,
    #[serde(default)]
    /// Files and directories to create in declaration order.
    pub files: Vec<file::FileConfig>,
    #[serde(default)]
    /// User accounts, keyed by login name.
    pub users: BTreeMap<String, user::UserConfig>,
    #[serde(default)]
    /// Groups, keyed by group name.
    pub groups: BTreeMap<String, user::GroupConfig>,
    #[serde(skip)]
    /// Ferric Oxide: run `fox` against the finished root (set by the
    /// installer's `--fox` options, never read from a configuration file).
    pub fox: Option<crate::FoxImage>,
}

impl Config {
    /// Load installer config from a TOML path, with its includes.
    ///
    /// Includes are loaded depth first, each before the file that includes
    /// it, and every file is loaded once: a file reached again through
    /// another include (`desktop.toml` includes `desktop-minimal.toml` and
    /// `server.toml`, which both include `minimal.toml`) is skipped, so it
    /// cannot undo what the files loaded after it the first time changed.
    ///
    /// # Errors
    /// Returns an error if a file cannot be read or decoded.
    pub fn from_file(path: &Path) -> Result<Self> {
        Self::load(path, &mut BTreeSet::new())
    }

    /// [`Config::from_file`], skipping the files in `loaded` and adding
    /// each file it loads to it.
    fn load(path: &Path, loaded: &mut BTreeSet<PathBuf>) -> Result<Self> {
        let config_data = fs::read_to_string(path)
            .with_context(|| format!("failed to read '{}'", path.display()))?;
        let mut config: Config = toml::from_str(&config_data)
            .with_context(|| format!("failed to decode '{}'", path.display()))?;
        loaded.insert(
            fs::canonicalize(path)
                .with_context(|| format!("failed to resolve '{}'", path.display()))?,
        );

        let config_dir = path.parent().unwrap_or(Path::new(""));
        let mut configs = Vec::new();
        for include in mem::take(&mut config.include) {
            let include_path = config_dir.join(&include);
            let canonical = fs::canonicalize(&include_path)
                .with_context(|| format!("Importing from {}", include.display()))?;
            if loaded.contains(&canonical) {
                continue;
            }
            configs.push(
                Config::load(&include_path, loaded)
                    .with_context(|| format!("Importing from {}", include.display()))?,
            );
        }
        configs.push(config); // Put ourself last to ensure that it overwrites anything else.

        let mut config = configs.remove(0);
        for other_config in configs {
            config.merge(other_config);
        }

        Ok(config)
    }

    /// Load hardcoded install config to fetch bootloaders
    pub fn bootloader_config() -> Self {
        let mut bootloader_config = Config::default();
        // TODO: This is unused
        bootloader_config.files.push(file::FileConfig {
            path: "/etc/pkg.d/50_redox".to_string(),
            data: "https://static.redox-os.org/pkg".to_string(),
            ..Default::default()
        });
        bootloader_config
            .packages
            .insert("bootloader".to_string(), PackageConfig::default());
        bootloader_config
    }

    /// Merge another resolved configuration, preferring its settings and appending files.
    ///
    /// # Panics
    /// Panics if either configuration still has unresolved includes.
    pub fn merge(&mut self, other: Config) {
        assert!(self.include.is_empty());
        assert!(other.include.is_empty());

        let Config {
            include: _,
            general: other_general,
            packages: other_packages,
            files: other_files,
            users: other_users,
            groups: other_groups,
            fox: other_fox,
        } = other;

        if other_fox.is_some() {
            self.fox = other_fox;
        }

        self.general.merge(other_general);

        for (package, package_config) in other_packages {
            self.packages.insert(package, package_config);
        }

        self.files.extend(other_files);

        for (user, user_config) in other_users {
            self.users.insert(user, user_config);
        }

        for (group, group_config) in other_groups {
            self.groups.insert(group, group_config);
        }
    }
}

impl Display for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "files:")?;
        for file in &self.files {
            writeln!(f, "- {}", file)?;
        }
        writeln!(f, "users:")?;
        for (name, user) in &self.users {
            writeln!(f, "- {}:{}", name, user)?;
        }
        write!(f, "packages: ")?;
        for name in self.packages.keys() {
            write!(f, " {}", name)?;
        }
        writeln!(f, "")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, text: &str) {
        fs::write(dir.join(name), text).unwrap();
    }

    #[test]
    fn a_file_included_twice_is_loaded_once() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "base.toml",
            "[packages]\nshell = {}\n\n[[files]]\npath = \"/switch\"\ndata = \"on\"\n",
        );
        write(
            dir.path(),
            "left.toml",
            "include = [\"base.toml\"]\n\n[packages]\nshell = \"ignore\"\n\n[[files]]\npath = \"/switch\"\ndata = \"\"\n",
        );
        write(
            dir.path(),
            "right.toml",
            "include = [\"base.toml\"]\n\n[[files]]\npath = \"/right\"\ndata = \"r\"\n",
        );
        write(
            dir.path(),
            "top.toml",
            "include = [\"left.toml\", \"right.toml\"]\n",
        );

        let config = Config::from_file(&dir.path().join("top.toml")).unwrap();
        let files: Vec<_> = config
            .files
            .iter()
            .map(|file| (file.path.as_str(), file.data.as_str()))
            .collect();
        assert_eq!(files, [("/switch", "on"), ("/switch", ""), ("/right", "r")]);
        assert!(matches!(
            &config.packages["shell"],
            PackageConfig::Build(rule) if rule == "ignore"
        ));
    }

    #[test]
    fn include_cycles_end() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.toml", "include = [\"b.toml\"]\n");
        write(dir.path(), "b.toml", "include = [\"a.toml\"]\n");
        Config::from_file(&dir.path().join("a.toml")).unwrap();
    }
}
