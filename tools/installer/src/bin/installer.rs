extern crate arg_parser;
extern crate redox_installer;
extern crate serde;
extern crate toml;

use std::path::Path;
use std::{env, fs, process};

use arg_parser::ArgParser;

use redox_installer::{Config, PackageConfig};

const HELP_STR: &str = r#"
redox_installer - Redox Installer.
                  Refer to link below for filesystem config reference:
                  https://doc.redox-os.org/book/configuration-settings.html

Using redox_installer as an installer:
  redox_installer <diskpath.img> [--config=file.toml] [--write-bootloader=file.img] [--live] [--no-mount] [--skip-partition]
    <diskpath.img>        Disk file to write
    --config              Path to filesystem config TOML
    --write-bootloader    Path to write UEFI bootloader to in addition to the embedded ESP
    --skip-partition      Skip writing GPT partition tables
                          Use this only if you plan to use other partition tool
    --live                Use bootloader configured for live disk
    --no-mount            Use RedoxFS AR instead of FUSE to write files
    --cookbook            Build system checkout whose repo/<target>/ holds the packages to install
    --size                Exact image size in MiB; fail before writing if the packages do not fit
                          (without it the image grows past filesystem_size to fit them)
    --config-name         Name of the filesystem configuration used for os-release VARIANT
    --build-tools-list    File naming the recipes the image builds with, one per line;
                          installed alongside the packages when general.build_tools is set
    --fox                 Host fox binary to finish the image root with: sync a
                          package set and write the image SBOM
    --fox-packages        With --fox: fox package set to sync into the image
                          (fox sync --target <image target>, fox.lock beside it)
    --fox-locked          With --fox-packages: fail unless fox.lock pins every package
    --fox-level           With --fox-packages: x86-64 level (v3 or v4) to install for
    --fox-sbom            With --fox: write the image's CycloneDX SBOM to this path

Using redox_installer as a configuration parser:
  redox_installer --config=file.toml [--cookbook=dir] [--list-packages|--filesystem-size|--output-config path]
    --list-packages      List packages will be installed
    --filesystem-size    Output the image size in MiB: with --cookbook, grown to fit the packages
    --output-config      Path to write the parsed config as another TOML
"#;

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

/// `parser` with the `--fox` options added.
fn with_fox_options(parser: ArgParser) -> ArgParser {
    parser
        .add_opt("", "fox")
        .add_opt("", "fox-packages")
        .add_opt("", "fox-sbom")
        .add_opt("", "fox-level")
        .add_flag(&["fox-locked"])
}

/// The `--fox` options, or an error message if they are inconsistent.
fn fox_image(parser: &ArgParser) -> Result<Option<redox_installer::FoxImage>, &'static str> {
    let Some(fox) = parser.get_opt("fox") else {
        if parser.get_opt("fox-packages").is_some() || parser.get_opt("fox-sbom").is_some() {
            return Err("--fox-packages and --fox-sbom need --fox");
        }
        return Ok(None);
    };
    let packages = parser.get_opt("fox-packages");
    if parser.found("fox-locked") && packages.is_none() {
        return Err("--fox-locked needs --fox-packages");
    }
    let level = parser.get_opt("fox-level");
    if level.is_some() && packages.is_none() {
        return Err("--fox-level needs --fox-packages");
    }
    Ok(Some(redox_installer::FoxImage {
        fox: fox.into(),
        packages: packages.map(Into::into),
        locked: parser.found("fox-locked"),
        target: redox_installer::get_target(),
        level,
        name: sbom_name(parser.get_opt("config-name").as_deref()),
        sbom: parser.get_opt("fox-sbom").map(Into::into),
    }))
}

fn main() {
    let mut parser = with_fox_options(ArgParser::new(4))
        .add_opt("b", "cookbook")
        .add_opt("", "config-name")
        .add_opt("c", "config")
        .add_opt("o", "output-config")
        .add_opt("", "write-bootloader")
        .add_opt("", "size")
        .add_flag(&["skip-partition"])
        .add_flag(&["filesystem-size"])
        .add_flag(&["l", "list-packages"])
        .add_flag(&["live"])
        .add_flag(&["no-mount"])
        .add_opt("", "build-tools-list");
    parser.parse(env::args());

    let skip_partition = parser.found("skip-partition");

    let mut config = if let Some(path) = parser.get_opt("config") {
        match Config::from_file(Path::new(&path)) {
            Ok(config) => config,
            Err(err) => {
                eprintln!("installer: {err}");
                process::exit(1);
            }
        }
    } else {
        redox_installer::Config::default()
    };

    if let Some(size) = parser.get_opt("size") {
        let Ok(mib) = size.parse::<u32>() else {
            eprintln!("installer: --size wants a size in MiB, not {size:?}");
            process::exit(1);
        };
        config.general.filesystem_size = Some(mib);
        config.general.filesystem_size_exact = Some(true);
    }

    // Get toml of merged config
    let merged_toml = toml::to_string_pretty(&config).unwrap();

    // Just output merged config and exit
    if let Some(path) = parser.get_opt("output-config") {
        fs::write(path, merged_toml).unwrap();
        return;
    }

    // Add filesystem.toml to config
    config.files.push(redox_installer::FileConfig {
        path: "filesystem.toml".to_string(),
        data: merged_toml,
        ..Default::default()
    });

    if skip_partition {
        config.general.skip_partitions = Some(true);
    }

    if parser.found("filesystem-size") {
        match parser.get_opt("cookbook") {
            Some(cookbook) => match redox_installer::image_size(&config, Some(&cookbook)) {
                Ok(size) => println!("{}", size.mib),
                Err(err) => {
                    eprintln!("installer: {err:#}");
                    process::exit(1);
                }
            },
            None => println!("{}", config.general.filesystem_size.unwrap_or(0)),
        }
    } else if parser.found("list-packages") {
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
        let cookbook = if let Some(path) = parser.get_opt("cookbook") {
            if !Path::new(&path).is_dir() {
                eprintln!("installer: {}: cookbook not found", path);
                process::exit(1);
            }

            Some(path)
        } else {
            None
        };

        let build_id = cookbook.as_deref().and_then(local_repo_build_id);
        append_os_release_metadata(
            &mut config,
            parser.get_opt("config-name").as_deref(),
            build_id,
        );

        if cookbook.is_some() {
            config.general.cookbook = cookbook;
        }
        if parser.found("live") {
            config.general.live_disk = Some(true);
        }
        if parser.found("no-mount") {
            config.general.no_mount = Some(true);
        }
        // The build tools are listed by `repo build-tools-list`, in a file the
        // build generates; passing the path keeps the installer from having to
        // read recipes. Only a configuration that asks for build tools
        // (`general.build_tools`) installs any.
        if let Some(list) = parser.get_opt("build-tools-list") {
            match config.general.build_tools.as_mut() {
                Some(build_tools) => build_tools.list = Some(list),
                None => {
                    eprintln!(
                        "installer: --build-tools-list needs general.build_tools in the configuration"
                    );
                    process::exit(1);
                }
            }
        }
        let write_bootloader = parser.get_opt("write-bootloader");
        if write_bootloader.is_some() {
            config.general.write_bootloader = write_bootloader;
        }
        match fox_image(&parser) {
            Ok(fox) => config.fox = fox,
            Err(message) => {
                eprintln!("installer: {message}");
                process::exit(1);
            }
        }

        if let Some(path) = parser.args.first() {
            if let Err(err) = redox_installer::install(config, path) {
                eprintln!("installer: failed to install: {:?}", err);
                process::exit(1);
            }
        } else {
            eprint!("{}", HELP_STR);
            process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> ArgParser {
        let mut parser = with_fox_options(ArgParser::new(4).add_opt("", "config-name"));
        parser.parse(
            std::iter::once("redox_installer")
                .chain(args.iter().copied())
                .map(str::to_string),
        );
        parser
    }

    #[test]
    fn fox_options_need_fox() {
        assert_eq!(fox_image(&parse(&["image.img"])), Ok(None));
        assert!(fox_image(&parse(&["--fox-packages=set.toml"])).is_err());
        assert!(fox_image(&parse(&["--fox=fox", "--fox-locked"])).is_err());
        assert!(fox_image(&parse(&["--fox=fox", "--fox-level=v4"])).is_err());
        let fox = fox_image(&parse(&[
            "--fox=target/release/fox",
            "--fox-packages=set.toml",
            "--fox-locked",
            "--fox-level=v4",
            "--fox-sbom=build/image.cdx.json",
            "--config-name=desktop",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(fox.fox, Path::new("target/release/fox"));
        assert_eq!(fox.packages.as_deref(), Some(Path::new("set.toml")));
        assert!(fox.locked);
        assert_eq!(fox.level.as_deref(), Some("v4"));
        assert_eq!(fox.name, "ferric-oxide-desktop");
        assert_eq!(fox.target, redox_installer::get_target());
        assert_eq!(sbom_name(None), "ferric-oxide");
    }
}
