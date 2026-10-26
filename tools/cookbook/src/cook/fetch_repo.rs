//! Prebuilt packages for recipes whose rule is "binary".
//!
//! Ferric Oxide: the remote is whatever `[cook] remote` in `cookbook.toml`
//! (or `COOKBOOK_REMOTE`) names, a URL in which `{target}` stands for the
//! target triple, and its packages must be signed by the key file
//! `[cook] remote_key` (or `COOKBOOK_REMOTE_KEY`) names. There is no
//! default remote: without one, every recipe builds from source.

use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
    time::Duration,
};

use fox_core::cookbook::{REPOSITORY_INDEX, RepositoryIndex};
use pkgar_keys::PublicKeyFile;

use crate::{
    Result, bail_other_err,
    config::{CookConfig, get_config, translate_mirror},
    cook::{
        fs::{create_dir, rename, run_command},
        pty::PtyOut,
    },
    log_to_pty, wrap_io_err,
};

/// How long a downloaded index is used before it is fetched again.
const INDEX_LIFETIME: Duration = Duration::from_secs(8 * 3600);

/// A configured remote and its index.
pub struct BinaryRepo {
    /// The remote's URL for this target, without a trailing `/`.
    url: String,
    key: PathBuf,
    /// The packages the remote has.
    pub index: RepositoryIndex,
}

impl BinaryRepo {
    /// The public key file the remote's packages must be signed by.
    pub fn key(&self) -> &Path {
        &self.key
    }

    /// The URL of `file` on the remote.
    pub fn url(&self, file: &str) -> String {
        format!("{}/{file}", self.url)
    }

    /// Downloads `file` from the remote to `dest`, through a temporary
    /// file so an interrupted download leaves nothing at `dest`.
    pub fn download(&self, file: &str, dest: &Path, logger: &PtyOut) -> Result<()> {
        let url = self.url(file);
        log_to_pty!(logger, "DEBUG: downloading {url}");
        let tmp = dest.with_added_extension("tmp");
        let mut command = curl(&url, &tmp);
        command.arg("--progress-bar");
        run_command(command, logger)?;
        rename(&tmp, dest)
    }
}

fn curl(url: &str, dest: &Path) -> Command {
    let mut command = Command::new("curl");
    command
        .args(["--fail", "--location", "--retry", "3", "--output"])
        .arg(dest)
        .arg(url);
    command
}

/// The configured remote, or `None` if there is none or its index cannot
/// be had (the reason is printed once).
pub fn binary_repo() -> Option<&'static BinaryRepo> {
    static BINARY_REPO: OnceLock<Option<BinaryRepo>> = OnceLock::new();
    BINARY_REPO
        .get_or_init(|| {
            open(&get_config().cook, Path::new("build/remotes")).unwrap_or_else(|e| {
                eprintln!("Unable to load the prebuilt package index, all recipes will build from source: {e}");
                None
            })
        })
        .as_ref()
}

/// Opens the remote `cook` configures, caching its index under
/// `cache_root`.
fn open(cook: &CookConfig, cache_root: &Path) -> Result<Option<BinaryRepo>> {
    let Some(template) = &cook.remote else {
        eprintln!(
            "No prebuilt package remote is configured ([cook] remote in cookbook.toml or COOKBOOK_REMOTE), all recipes will build from source"
        );
        return Ok(None);
    };
    let Some(key) = &cook.remote_key else {
        bail_other_err!(
            "the remote {template} has no pinned key: set [cook] remote_key in cookbook.toml or COOKBOOK_REMOTE_KEY to its public key file"
        );
    };
    // Fail on a bad key now rather than on the first package.
    PublicKeyFile::open(key)?;

    let target = redoxer::target();
    let url = translate_mirror(&template.replace("{target}", target));
    let url = url.trim_end_matches('/').to_string();
    // Keyed by URL, so changing the remote does not reuse another's index.
    let cache_dir = cache_root.join(&blake3::hash(url.as_bytes()).to_hex()[..16]);
    let cache = cache_dir.join(REPOSITORY_INDEX);

    let index = match load_cached_index(&cache, cook.offline) {
        Some(index) => index,
        None if cook.offline => {
            bail_other_err!(
                "{} is not there and unable to continue in offline mode",
                cache.display()
            )
        }
        None => {
            create_dir(&cache_dir)?;
            let tmp = cache.with_added_extension("tmp");
            let mut command = curl(&format!("{url}/{REPOSITORY_INDEX}"), &tmp);
            command.arg("--silent").arg("--show-error");
            let output = command
                .output()
                .map_err(wrap_io_err!("Running curl for the package index"))?;
            if !output.status.success() {
                bail_other_err!(
                    "cannot download {url}/{REPOSITORY_INDEX}: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            rename(&tmp, &cache)?;
            RepositoryIndex::from_file(&cache)?
        }
    };
    Ok(Some(BinaryRepo {
        url,
        key: key.clone(),
        index,
    }))
}

fn load_cached_index(path: &Path, offline: bool) -> Option<RepositoryIndex> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    if !offline && modified.elapsed().is_ok_and(|age| age > INDEX_LIFETIME) {
        let _ = std::fs::remove_file(path);
        return None;
    }
    RepositoryIndex::from_file(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cook(remote: Option<String>, remote_key: Option<PathBuf>) -> CookConfig {
        crate::config::init_config();
        CookConfig {
            remote,
            remote_key,
            ..Default::default()
        }
    }

    #[test]
    fn no_remote_means_no_prebuilt_packages() {
        let dir = tempfile::tempdir().unwrap();
        assert!(open(&cook(None, None), dir.path()).unwrap().is_none());
    }

    #[test]
    fn a_remote_needs_a_pinned_key() {
        let dir = tempfile::tempdir().unwrap();
        let config = cook(Some("https://example.invalid/{target}".into()), None);
        let error = open(&config, dir.path()).err().unwrap();
        assert!(error.to_string().contains("no pinned key"), "{error}");
    }

    #[test]
    fn index_and_packages_come_from_the_target_url() {
        let dir = tempfile::tempdir().unwrap();
        let key = fox_core::fixtures::FixtureKey::generate(dir.path(), "remote").unwrap();
        let target = redoxer::target();
        let served = dir.path().join("served").join(target);
        std::fs::create_dir_all(&served).unwrap();
        let mut index = RepositoryIndex::default();
        index.packages.insert("app".into(), "abc".into());
        std::fs::write(served.join(REPOSITORY_INDEX), index.to_toml().unwrap()).unwrap();
        std::fs::write(served.join("app.toml"), "name = \"app\"\n").unwrap();

        let template = format!("file://{}/served/{{target}}/", dir.path().display());
        let config = cook(Some(template), Some(key.public_path.clone()));
        let cache = dir.path().join("cache");
        let repo = open(&config, &cache).unwrap().unwrap();
        assert_eq!(repo.index, index);
        assert_eq!(repo.key(), key.public_path);
        assert_eq!(
            repo.url("app.toml"),
            format!("file://{}/app.toml", served.display())
        );

        let dest = dir.path().join("app.toml");
        repo.download("app.toml", &dest, &None).unwrap();
        assert_eq!(std::fs::read_to_string(dest).unwrap(), "name = \"app\"\n");
        assert!(
            repo.download("missing.pkgar", &dir.path().join("x"), &None)
                .is_err()
        );

        // The cached index is used while it is fresh.
        std::fs::remove_file(served.join(REPOSITORY_INDEX)).unwrap();
        assert_eq!(open(&config, &cache).unwrap().unwrap().index, index);
    }
}
