use std::{
    fs, io,
    path::{Path, PathBuf},
    process,
};

static INIT_ENV: &str = include_str!("../res/run_redoxer.ion");
static INIT_REDOXER: &str = include_str!("../res/30_redoxer");

/// The files the guest needs for `redoxerd` to run `arguments`.
///
/// These are written into a booted image instead of onto a staging directory,
/// so the same content serves both an installed image and a prepared one. Each
/// entry is a path inside the guest, its contents and its permission bits.
///
/// `arguments` is the guest command and its arguments, one per line, with the
/// first line naming the program inside the guest. A recipe's build tools run
/// their command by **name** (the guest has it on `PATH`), so nothing here
/// needs to name a host path; when `root_dir` is given, any argument that names
/// a path inside it is rewritten to the `/root` the folder is copied to.
///
/// # Errors
/// Returns an I/O error when `root_dir` cannot be canonicalised or is not valid
/// UTF-8.
pub fn redoxerd_config_files(
    arguments: &[String],
    root_dir: Option<&str>,
) -> io::Result<Vec<(PathBuf, Vec<u8>, u16)>> {
    let mut redoxerd_config = String::new();
    for arg in arguments.iter() {
        // Replace absolute path to folder with /root in command name
        // TODO: make this activated by a flag
        if let Some(ref folder) = root_dir {
            let folder_canonical_path = fs::canonicalize(folder)?;
            let folder_canonical = folder_canonical_path
                .to_str()
                .ok_or(io::Error::other("folder path is not valid UTF-8"))?;
            // A bare name such as `mdbook` is resolved by the guest's `PATH`
            // and must stay bare: prefixing it would look for
            // `/root/mdbook`, and `redoxerd` would try to run a directory when
            // the path names the copied `/root` folder itself.
            if arg.starts_with(folder_canonical) && !Path::new(arg).is_dir() {
                let arg_replace = arg.replace(folder_canonical, "/root");
                eprintln!("redoxer: replacing '{arg}' with '{arg_replace}' in arguments");
                redoxerd_config.push_str(&arg_replace);
                redoxerd_config.push('\n');
                continue;
            }
        }

        redoxerd_config.push_str(arg);
        redoxerd_config.push('\n');
    }
    Ok(vec![
        (
            PathBuf::from("/etc/redoxerd"),
            redoxerd_config.into_bytes(),
            0o644,
        ),
        (
            PathBuf::from("/usr/lib/run_redoxer.ion"),
            INIT_ENV.as_bytes().to_vec(),
            0o755,
        ),
        (
            PathBuf::from("/usr/lib/init.d/30_redoxer"),
            INIT_REDOXER.as_bytes().to_vec(),
            0o644,
        ),
    ])
}

/// Writes the guest's `/etc/redoxerd` command description and the init units
/// that start it into a staging directory.
///
/// See [`redoxerd_config_files`] for what the files contain. A file the guest
/// image already provides — the init unit, which an install configuration may
/// also declare — is left alone.
///
/// # Errors
/// Returns an I/O error when `root_dir` cannot be canonicalised, is not valid
/// UTF-8, or when a file cannot be written.
pub fn write_redoxerd_config(
    dest_dir: &Path,
    arguments: &[String],
    root_dir: Option<&str>,
) -> Result<(), io::Error> {
    for (path, data, _mode) in redoxerd_config_files(arguments, root_dir)? {
        let target = dest_dir.join(
            path.strip_prefix("/")
                .map_err(|err| io::Error::other(err.to_string()))?,
        );
        if let Some(parent) = target.parent()
            && !parent.is_dir()
        {
            fs::create_dir_all(parent)?;
        }
        if target.is_file() {
            // The image's own copy wins: an install configuration may declare
            // the same unit, and overwriting it would change what was signed.
            continue;
        }
        fs::write(&target, data)?;
    }
    let root_dir = dest_dir.join("root");
    if !root_dir.is_dir() {
        // needed for cwd
        fs::create_dir(&root_dir)?;
    }
    Ok(())
}

#[derive(Clone, Default)]
struct RedoxerConfig {
    // Root path
    root: PathBuf,
    // Relative directory of execution
    folder: Option<String>,
    // Commands to execute
    arguments: Vec<String>,
}

impl RedoxerConfig {
    pub fn new(mut args: impl Iterator<Item = String>) -> Self {
        let mut config = RedoxerConfig {
            root: std::env::current_dir().expect("Unable to get current dir"),
            ..Default::default()
        };

        // Matching flags
        let mut matching = true;
        while let Some(arg) = args.next() {
            match (arg.as_str(), matching) {
                ("--root", true) => match args.next() {
                    Some(root) => {
                        config.root = PathBuf::from(root);
                    }
                    None => panic!("--root requires a path to a directory"),
                },
                ("--folder", true) => match args.next() {
                    Some(folder) => {
                        config.folder = Some(folder);
                    }
                    None => panic!("--folder requires a path to a directory"),
                },
                ("--", true) => matching = false,
                _ => {
                    matching = false;
                    config.arguments.push(arg);
                }
            }
        }

        config
    }
}

pub fn main(args: &[String]) {
    let config = RedoxerConfig::new(args.iter().skip(2).cloned());

    match write_redoxerd_config(&config.root, &config.arguments, config.folder.as_deref()) {
        Ok(()) => {
            process::exit(0);
        }
        Err(err) => {
            eprintln!("redoxer write-exec: {err:#}");
            process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A build tool is run by name: the guest has it on `PATH`, so a bare name
    /// must reach `redoxerd` unchanged rather than becoming `/root/<name>`.
    #[test]
    fn a_bare_command_name_is_not_rewritten() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        fs::create_dir(&root).unwrap();
        let dest = temp.path().join("image");

        write_redoxerd_config(
            &dest,
            &["mdbook".to_string(), "build".to_string()],
            Some(root.to_str().unwrap()),
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(dest.join("etc/redoxerd")).unwrap(),
            "mdbook\nbuild\n"
        );
    }

    /// A path inside the copied folder is rewritten to the `/root` the folder
    /// is copied to, which is what makes a recipe's script see its files.
    #[test]
    fn a_path_in_the_root_folder_is_rewritten() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        fs::create_dir(&root).unwrap();
        let dest = temp.path().join("image");
        let script = root.canonicalize().unwrap().join("build.sh");
        fs::write(&script, "true").unwrap();

        write_redoxerd_config(
            &dest,
            &[
                "sh".to_string(),
                script.display().to_string(),
                "arg".to_string(),
            ],
            Some(root.to_str().unwrap()),
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(dest.join("etc/redoxerd")).unwrap(),
            "sh\n/root/build.sh\narg\n"
        );
    }

    /// The `/root` a folder is copied to is a directory, so naming it as the
    /// command would make `redoxerd` try to execute a directory; it is not
    /// rewritten into an indistinguishable one.
    #[test]
    fn the_root_folder_itself_is_not_rewritten_into_a_command() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        fs::create_dir(&root).unwrap();
        let dest = temp.path().join("image");
        let root_path = root.canonicalize().unwrap();

        write_redoxerd_config(
            &dest,
            &[root_path.display().to_string()],
            Some(root.to_str().unwrap()),
        )
        .unwrap();

        let config = fs::read_to_string(dest.join("etc/redoxerd")).unwrap();
        assert_eq!(config, format!("{}\n", root_path.display()));
        assert_ne!(config, "/root\n");
    }

    /// The init unit that starts `redoxerd` waits for the blocking DHCP unit,
    /// which redoxer supplies in its own file rather than overwriting the one
    /// the `base` package signs.
    #[test]
    fn the_redoxer_unit_waits_for_the_blocking_dhcp_unit() {
        assert!(INIT_REDOXER.contains("requires_weak 10_redoxer_dhcp.service"));
        assert!(!INIT_REDOXER.contains("10_dhcpd.service"));
    }
}
