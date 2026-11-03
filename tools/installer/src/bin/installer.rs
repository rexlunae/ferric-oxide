//! `redox_installer`: build a Redox file system image from a configuration,
//! or parse a configuration and report on it.

use std::path::{Path, PathBuf};
use std::{fs, process};

use clap::{Args, CommandFactory, Parser};

use redox_installer::{Config, PackageConfig};

/// The command line. `--opt value`, `--opt=value` and, where a short form
/// exists, `-o value` and `-ovalue` are all accepted. A repeated option takes
/// its last value.
#[derive(Debug, Parser)]
#[command(
    name = "redox_installer",
    version,
    about = "Redox Installer: build a Redox file system image, or parse a filesystem configuration",
    after_help = "Refer to the link below for the filesystem config reference:\n  \
                  https://doc.redox-os.org/book/configuration-settings.html",
    override_usage = "redox_installer <diskpath.img> [--config=file.toml] [--write-bootloader=file.img] [--live] [--no-mount] [--skip-partition]\n       \
                      redox_installer --config=file.toml [--cookbook=dir] [--list-packages|--filesystem-size|--output-config path]",
    args_override_self = true
)]
struct Cli {
    /// Disk file to write
    #[arg(value_name = "diskpath.img")]
    disk: Option<PathBuf>,

    /// Path to filesystem config TOML
    #[arg(short = 'c', long, value_name = "file.toml")]
    config: Option<PathBuf>,

    /// Path to write UEFI bootloader to in addition to the embedded ESP
    #[arg(long, value_name = "file.img")]
    write_bootloader: Option<String>,

    /// Skip writing GPT partition tables. Use this only if you plan to use
    /// other partition tool
    #[arg(long)]
    skip_partition: bool,

    /// Use bootloader configured for live disk
    #[arg(long)]
    live: bool,

    /// Use RedoxFS AR instead of FUSE to write files
    #[arg(long)]
    no_mount: bool,

    /// Build system checkout whose repo/<target>/ holds the packages to install
    #[arg(short = 'b', long, value_name = "dir")]
    cookbook: Option<String>,

    /// Exact image size in MiB; fail before writing if the packages do not
    /// fit (without it the image grows past filesystem_size to fit them)
    #[arg(long, value_name = "MiB")]
    size: Option<u32>,

    /// Name of the filesystem configuration used for os-release VARIANT
    #[arg(long, value_name = "name")]
    config_name: Option<String>,

    /// File naming the recipes the image builds with, one per line; installed
    /// alongside the packages when general.build_tools is set
    #[arg(long, value_name = "file")]
    build_tools_list: Option<String>,

    #[command(flatten)]
    fox: FoxArgs,

    /// List packages will be installed
    #[arg(short = 'l', long, help_heading = "Configuration parser")]
    list_packages: bool,

    /// Output the image size in MiB: with --cookbook, grown to fit the packages
    #[arg(long, help_heading = "Configuration parser")]
    filesystem_size: bool,

    /// Path to write the parsed config as another TOML
    #[arg(
        short = 'o',
        long,
        value_name = "path",
        help_heading = "Configuration parser"
    )]
    output_config: Option<PathBuf>,
}

/// The `--fox*` options, which finish the image root with a host `fox`.
#[derive(Debug, Args)]
#[command(next_help_heading = "Finishing the image with fox")]
struct FoxArgs {
    /// Host fox binary to finish the image root with: sync a package set and
    /// write the image SBOM
    #[arg(long, value_name = "path")]
    fox: Option<PathBuf>,

    /// With --fox: fox package set to sync into the image (fox sync --target
    /// <image target>, fox.lock beside it)
    #[arg(long, value_name = "file.toml", requires = "fox")]
    fox_packages: Option<PathBuf>,

    /// With --fox-packages: fail unless fox.lock pins every package
    #[arg(long, requires = "fox_packages")]
    fox_locked: bool,

    /// With --fox-packages: x86-64 level (v3 or v4) to install for
    #[arg(long, value_name = "level", requires = "fox_packages")]
    fox_level: Option<String>,

    /// With --fox: write the image's CycloneDX SBOM to this path
    #[arg(long, value_name = "path", requires = "fox")]
    fox_sbom: Option<PathBuf>,
}

fn os_release_quote(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{}\"", escaped)
}

fn build_id_from_repo_toml(repo_toml: &str) -> Option<String> {
    toml::from_str::<toml::Value>(repo_toml)
        .ok()?
        .get("build_id")?
        .as_str()
        .and_then(|value| {
            if value.is_empty() {
                None
            } else {
                Some(value.to_string())
            }
        })
}

fn local_repo_build_id(cookbook: &str) -> Option<String> {
    let repo_toml = Path::new(cookbook)
        .join("repo")
        .join(redox_installer::get_target())
        .join("repo.toml");
    let repo_toml = fs::read_to_string(repo_toml).ok()?;
    build_id_from_repo_toml(&repo_toml)
}

fn append_os_release_metadata(
    config: &mut Config,
    config_name: Option<&str>,
    build_id: Option<String>,
) {
    let mut data = String::new();
    if let Some(config_name) = config_name.filter(|value| !value.is_empty()) {
        data.push_str("VARIANT=");
        data.push_str(&os_release_quote(config_name));
        data.push('\n');
    }
    if let Some(build_id) = build_id.filter(|value| !value.is_empty()) {
        data.push_str("BUILD_ID=");
        data.push_str(&os_release_quote(&build_id));
        data.push('\n');
    }

    if !data.is_empty() {
        config.files.push(redox_installer::FileConfig {
            path: "/usr/lib/os-release".to_string(),
            data,
            append: true,
            ..Default::default()
        });
    }
}

/// The system name in the image SBOM: the product, and the configuration
/// when one is named.
fn sbom_name(config_name: Option<&str>) -> String {
    match config_name.filter(|name| !name.is_empty()) {
        Some(name) => format!("ferric-oxide-{name}"),
        None => "ferric-oxide".to_string(),
    }
}

/// The image's fox finishing step, if `--fox` was given. Clap has already
/// rejected options that need an option that is missing.
fn fox_image(fox: FoxArgs, config_name: Option<&str>) -> Option<redox_installer::FoxImage> {
    Some(redox_installer::FoxImage {
        fox: fox.fox?,
        packages: fox.fox_packages,
        locked: fox.fox_locked,
        target: redox_installer::get_target(),
        level: fox.fox_level,
        name: sbom_name(config_name),
        sbom: fox.fox_sbom,
    })
}

fn fail(message: impl std::fmt::Display) -> ! {
    eprintln!("installer: {message}");
    process::exit(1);
}

fn main() {
    let cli = Cli::parse();

    let mut config = match &cli.config {
        Some(path) => Config::from_file(path).unwrap_or_else(|err| fail(err)),
        None => Config::default(),
    };

    if let Some(mib) = cli.size {
        config.general.filesystem_size = Some(mib);
        config.general.filesystem_size_exact = Some(true);
    }

    // Get toml of merged config
    let merged_toml = toml::to_string_pretty(&config)
        .unwrap_or_else(|err| fail(format_args!("failed to serialize the configuration: {err}")));

    // Just output merged config and exit
    if let Some(path) = &cli.output_config {
        if let Err(err) = fs::write(path, merged_toml) {
            fail(format_args!("failed to write '{}': {err}", path.display()));
        }
        return;
    }

    // Add filesystem.toml to config
    config.files.push(redox_installer::FileConfig {
        path: "filesystem.toml".to_string(),
        data: merged_toml,
        ..Default::default()
    });

    if cli.skip_partition {
        config.general.skip_partitions = Some(true);
    }

    if cli.filesystem_size {
        match &cli.cookbook {
            Some(cookbook) => match redox_installer::image_size(&config, Some(cookbook)) {
                Ok(size) => println!("{}", size.mib),
                Err(err) => fail(format_args!("{err:#}")),
            },
            None => println!("{}", config.general.filesystem_size.unwrap_or(0)),
        }
    } else if cli.list_packages {
        // List the packages that should be fetched or built by the cookbook
        for (packagename, package) in &config.packages {
            match package {
                PackageConfig::Build(rule) if rule == "ignore" => {
                    // skip this package
                }
                _ => {
                    println!("{}", packagename);
                }
            }
        }
    } else {
        install(cli, config);
    }
}

/// Apply the installation options to `config` and write the image.
fn install(cli: Cli, mut config: Config) {
    if let Some(path) = &cli.cookbook
        && !Path::new(path).is_dir()
    {
        fail(format_args!("{path}: cookbook not found"));
    }

    let build_id = cli.cookbook.as_deref().and_then(local_repo_build_id);
    append_os_release_metadata(&mut config, cli.config_name.as_deref(), build_id);

    if cli.cookbook.is_some() {
        config.general.cookbook = cli.cookbook;
    }
    if cli.live {
        config.general.live_disk = Some(true);
    }
    if cli.no_mount {
        config.general.no_mount = Some(true);
    }
    // The build tools are listed by `repo build-tools-list`, in a file the
    // build generates; passing the path keeps the installer from having to
    // read recipes. Only a configuration that asks for build tools
    // (`general.build_tools`) installs any.
    if let Some(list) = cli.build_tools_list {
        match config.general.build_tools.as_mut() {
            Some(build_tools) => build_tools.list = Some(list),
            None => fail("--build-tools-list needs general.build_tools in the configuration"),
        }
    }
    if cli.write_bootloader.is_some() {
        config.general.write_bootloader = cli.write_bootloader;
    }
    config.fox = fox_image(cli.fox, cli.config_name.as_deref());

    let Some(path) = cli.disk else {
        eprint!("{}", Cli::command().render_help());
        process::exit(1);
    };
    if let Err(err) = redox_installer::install(config, path) {
        fail(format_args!("failed to install: {err:?}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("redox_installer").chain(args.iter().copied()))
    }

    #[test]
    fn command_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn config_value_forms_are_equivalent() {
        for args in [
            &["--config", "some.toml", "disk.img"][..],
            &["--config=some.toml", "disk.img"],
            &["-c", "some.toml", "disk.img"],
            &["-csome.toml", "disk.img"],
            &["disk.img", "--config", "some.toml"],
        ] {
            let cli = parse(args).unwrap();
            assert_eq!(
                cli.config.as_deref(),
                Some(Path::new("some.toml")),
                "{args:?}"
            );
            assert_eq!(cli.disk.as_deref(), Some(Path::new("disk.img")), "{args:?}");
        }
    }

    #[test]
    fn long_options_take_separate_values() {
        let cli = parse(&[
            "--cookbook",
            ".",
            "--config-name",
            "desktop",
            "--output-config",
            "out.toml",
            "--write-bootloader",
            "boot.efi",
            "--size",
            "512",
            "--build-tools-list",
            "tools.txt",
        ])
        .unwrap();
        assert_eq!(cli.cookbook.as_deref(), Some("."));
        assert_eq!(cli.config_name.as_deref(), Some("desktop"));
        assert_eq!(cli.output_config.as_deref(), Some(Path::new("out.toml")));
        assert_eq!(cli.write_bootloader.as_deref(), Some("boot.efi"));
        assert_eq!(cli.size, Some(512));
        assert_eq!(cli.build_tools_list.as_deref(), Some("tools.txt"));
        assert_eq!(cli.disk, None);
    }

    #[test]
    fn short_forms() {
        let cli = parse(&["-b", ".", "-o", "out.toml", "-l"]).unwrap();
        assert_eq!(cli.cookbook.as_deref(), Some("."));
        assert_eq!(cli.output_config.as_deref(), Some(Path::new("out.toml")));
        assert!(cli.list_packages);
    }

    #[test]
    fn flags() {
        let cli = parse(&[
            "--skip-partition",
            "--filesystem-size",
            "--list-packages",
            "--live",
            "--no-mount",
            "disk.img",
        ])
        .unwrap();
        assert!(cli.skip_partition && cli.filesystem_size && cli.list_packages);
        assert!(cli.live && cli.no_mount);
        let cli = parse(&["disk.img"]).unwrap();
        assert!(!(cli.skip_partition || cli.filesystem_size || cli.list_packages));
        assert!(!(cli.live || cli.no_mount));
    }

    #[test]
    fn repeated_options_take_the_last_value() {
        let cli = parse(&["-c", "a.toml", "--config=b.toml", "--live", "--live"]).unwrap();
        assert_eq!(cli.config.as_deref(), Some(Path::new("b.toml")));
        assert!(cli.live);
    }

    #[test]
    fn rejects_malformed_arguments() {
        assert!(parse(&["--config"]).is_err());
        assert!(parse(&["--size", "big"]).is_err());
        assert!(parse(&["--no-such-option", "disk.img"]).is_err());
        assert!(parse(&["a.img", "b.img"]).is_err());
    }

    #[test]
    fn fox_options_need_fox() {
        assert!(parse(&["image.img"]).unwrap().fox.fox.is_none());
        assert!(parse(&["--fox-packages=set.toml"]).is_err());
        assert!(parse(&["--fox-sbom=sbom.json"]).is_err());
        assert!(parse(&["--fox=fox", "--fox-locked"]).is_err());
        assert!(parse(&["--fox=fox", "--fox-level=v4"]).is_err());
        let cli = parse(&[
            "--fox=target/release/fox",
            "--fox-packages",
            "set.toml",
            "--fox-locked",
            "--fox-level=v4",
            "--fox-sbom=build/image.cdx.json",
            "--config-name=desktop",
        ])
        .unwrap();
        let fox = fox_image(cli.fox, cli.config_name.as_deref()).unwrap();
        assert_eq!(fox.fox, Path::new("target/release/fox"));
        assert_eq!(fox.packages.as_deref(), Some(Path::new("set.toml")));
        assert!(fox.locked);
        assert_eq!(fox.level.as_deref(), Some("v4"));
        assert_eq!(fox.sbom.as_deref(), Some(Path::new("build/image.cdx.json")));
        assert_eq!(fox.name, "ferric-oxide-desktop");
        assert_eq!(fox.target, redox_installer::get_target());
        assert_eq!(sbom_name(None), "ferric-oxide");
        let cli = parse(&["image.img"]).unwrap();
        assert!(fox_image(cli.fox, None).is_none());
    }
}
