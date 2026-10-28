//! Build-machine programs that the recipe build templates run.
//!
//! The Meson and CMake helpers in the build prescript call `meson`, `cmake`
//! and `ninja` from the build machine's `PATH`. [`check`] finds every recipe
//! about to be built from source that needs one of them, so a missing program
//! is reported once, before anything is compiled, instead of failing a recipe
//! halfway through the build.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::OsStr;
use std::fmt;
use std::path::Path;

use fox_core::cookbook::RecipeName;

use crate::recipe::{BuildKind, CookRecipe};

/// A build-machine program that a build template runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum HostTool {
    /// The Meson build system, run by `cookbook_meson`.
    Meson,
    /// CMake, run by `cookbook_cmake`.
    Cmake,
    /// Ninja, which runs the builds Meson and CMake generate.
    Ninja,
}

impl HostTool {
    /// The program name looked up on `PATH`.
    pub fn program(self) -> &'static str {
        match self {
            HostTool::Meson => "meson",
            HostTool::Cmake => "cmake",
            HostTool::Ninja => "ninja",
        }
    }

    /// Prescript helpers and variables in a script that run this program.
    fn markers(self) -> &'static [&'static str] {
        match self {
            HostTool::Meson => &["cookbook_meson", "COOKBOOK_MESON"],
            HostTool::Cmake => &["cookbook_cmake", "COOKBOOK_CMAKE"],
            HostTool::Ninja => &[
                "cookbook_meson",
                "cookbook_cmake",
                "COOKBOOK_MESON",
                "COOKBOOK_CMAKE",
                "COOKBOOK_NINJA",
            ],
        }
    }

    /// Programs whose direct invocation in a script needs this one; Meson's
    /// default backend is Ninja.
    fn invoked_by(self) -> &'static [&'static str] {
        match self {
            HostTool::Meson => &["meson"],
            HostTool::Cmake => &["cmake"],
            HostTool::Ninja => &["ninja", "meson"],
        }
    }

    const ALL: [HostTool; 3] = [HostTool::Meson, HostTool::Cmake, HostTool::Ninja];
}

impl fmt::Display for HostTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.program())
    }
}

/// Returns the build-machine programs that building `kind` from source runs.
///
/// Binary packages ([`BuildKind::Remote`]) and metapackages need none.
pub fn required(kind: &BuildKind) -> BTreeSet<HostTool> {
    match kind {
        BuildKind::Meson { .. } => [HostTool::Meson, HostTool::Ninja].into(),
        BuildKind::Cmake { .. } => [HostTool::Cmake, HostTool::Ninja].into(),
        BuildKind::Custom { script } => {
            let words = command_words(script);
            HostTool::ALL
                .into_iter()
                .filter(|tool| {
                    tool.markers().iter().any(|marker| script.contains(marker))
                        || tool
                            .invoked_by()
                            .iter()
                            .any(|program| words.contains(program))
                })
                .collect()
        }
        _ => BTreeSet::new(),
    }
}

/// The programs `script` runs: the first word of each command outside `#`
/// comments, after variable assignments and `env`/`exec`/`command`, so
/// `env DESTDIR=… meson install` yields `meson` and `echo meson` yields `echo`.
fn command_words(script: &str) -> BTreeSet<&str> {
    script
        .lines()
        .map(|line| match line.find('#') {
            Some(at) if at == 0 || line[..at].ends_with(char::is_whitespace) => &line[..at],
            _ => line,
        })
        .flat_map(|line| line.split(|c: char| ";&|()`".contains(c)))
        .filter_map(|command| {
            command.split_whitespace().find(|word| {
                !word.contains('=')
                    && !word.starts_with('-')
                    && !matches!(*word, "env" | "exec" | "command")
            })
        })
        .collect()
}

/// Programs missing from the build machine, each with the recipes that need it.
#[derive(Debug, PartialEq, Eq)]
pub struct MissingHostTools(pub BTreeMap<HostTool, Vec<RecipeName>>);

impl fmt::Display for MissingHostTools {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "missing build-machine programs:")?;
        for (tool, recipes) in &self.0 {
            let names: Vec<&str> = recipes.iter().map(RecipeName::as_str).collect();
            write!(f, "\n  {tool}, needed by {}", names.join(", "))?;
        }
        write!(
            f,
            "\nInstall them with your package manager (for example `meson`, `cmake` and \
             `ninja-build` on Debian or Fedora, `meson cmake ninja` with Homebrew) and run the build again."
        )
    }
}

impl std::error::Error for MissingHostTools {}

/// Checks, before a cook starts, that every program the source-built
/// `recipes` need is on `PATH`.
///
/// Recipes that already have a staged package are skipped: an unchanged
/// recipe reuses its stage without running its build script. One whose stage
/// turns out to be stale is checked by [`require`] when it is rebuilt.
///
/// # Errors
///
/// Returns [`MissingHostTools`] listing each absent program and the recipes
/// that need it.
pub fn check<'a>(
    recipes: impl IntoIterator<Item = &'a CookRecipe>,
) -> Result<(), MissingHostTools> {
    let path = env::var_os("PATH").unwrap_or_default();
    let unstaged = recipes
        .into_iter()
        .filter(|recipe| !recipe.stage_paths().1.is_file());
    check_with(unstaged, |tool| on_path(&path, tool.program()))
}

/// Checks that the programs `recipe`'s build script runs are on `PATH`, just
/// before that script runs.
///
/// # Errors
///
/// Returns [`MissingHostTools`] listing each absent program.
pub fn require(recipe: &CookRecipe) -> Result<(), MissingHostTools> {
    let path = env::var_os("PATH").unwrap_or_default();
    check_with([recipe], |tool| on_path(&path, tool.program()))
}

fn check_with<'a>(
    recipes: impl IntoIterator<Item = &'a CookRecipe>,
    mut present: impl FnMut(HostTool) -> bool,
) -> Result<(), MissingHostTools> {
    let mut found = BTreeMap::new();
    let mut missing: BTreeMap<HostTool, Vec<RecipeName>> = BTreeMap::new();
    for recipe in recipes {
        for tool in required(&recipe.recipe.build.kind) {
            if !*found.entry(tool).or_insert_with(|| present(tool)) {
                missing.entry(tool).or_default().push(recipe.name.clone());
            }
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(MissingHostTools(missing))
    }
}

fn on_path(path: &OsStr, program: &str) -> bool {
    env::split_paths(path).any(|dir| is_executable(&dir.join(program)))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recipe(name: &str, build: &str) -> CookRecipe {
        let recipe = toml::from_str(&format!("[build]\n{build}")).unwrap();
        CookRecipe {
            name: RecipeName::new(name).unwrap(),
            dir: Default::default(),
            recipe,
            target: "x86_64-unknown-redox",
            is_deps: false,
            rule: String::new(),
            pinned: false,
        }
    }

    #[test]
    fn templates_and_custom_scripts_name_their_programs() {
        let meson = recipe("a", "template = \"meson\"");
        assert_eq!(
            required(&meson.recipe.build.kind),
            [HostTool::Meson, HostTool::Ninja].into()
        );
        let custom = recipe(
            "b",
            "template = \"custom\"\nscript = \"DYNAMIC_INIT\\ncookbook_cmake\"",
        );
        assert_eq!(
            required(&custom.recipe.build.kind),
            [HostTool::Cmake, HostTool::Ninja].into()
        );
        let direct = recipe(
            "hicolor-icon-theme",
            "template = \"custom\"\nscript = \"\"\"\nmeson setup . src\nenv DESTDIR=x meson install\n\"\"\"",
        );
        assert_eq!(
            required(&direct.recipe.build.kind),
            [HostTool::Meson, HostTool::Ninja].into()
        );
        let commented = recipe(
            "d",
            "template = \"custom\"\nscript = \"\"\"\n# see the cmake log\nmake # not cmake\n\"\"\"",
        );
        assert!(required(&commented.recipe.build.kind).is_empty());
        let printed = recipe(
            "e",
            "template = \"custom\"\nscript = \"echo meson; X=1 cmake --version | ninja-free\"",
        );
        assert_eq!(
            required(&printed.recipe.build.kind),
            [HostTool::Cmake].into()
        );
        let configure = recipe("c", "template = \"configure\"");
        assert!(required(&configure.recipe.build.kind).is_empty());
    }

    #[test]
    fn check_lists_each_missing_program_with_its_recipes() {
        let recipes = [
            recipe(
                "xkeyboard-config",
                "template = \"custom\"\nscript = \"cookbook_meson\"",
            ),
            recipe("glib", "template = \"meson\""),
            recipe("zlib", "template = \"cmake\""),
        ];
        let err = check_with(&recipes, |tool| tool == HostTool::Cmake).unwrap_err();
        let names = |list: &[&str]| -> Vec<RecipeName> {
            list.iter().map(|n| RecipeName::new(*n).unwrap()).collect()
        };
        assert_eq!(
            err,
            MissingHostTools(BTreeMap::from([
                (HostTool::Meson, names(&["xkeyboard-config", "glib"])),
                (
                    HostTool::Ninja,
                    names(&["xkeyboard-config", "glib", "zlib"])
                ),
            ]))
        );
        assert!(
            err.to_string()
                .contains("meson, needed by xkeyboard-config, glib")
        );
        assert!(check_with(&recipes, |_| true).is_ok());
    }

    #[test]
    fn on_path_needs_an_executable_file() {
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("meson");
        std::fs::write(&tool, "").unwrap();
        let path = env::join_paths([dir.path()]).unwrap();
        #[cfg(unix)]
        {
            assert!(!on_path(&path, "meson"));
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert!(on_path(&path, "meson"));
        assert!(!on_path(&path, "ninja"));
    }
}
