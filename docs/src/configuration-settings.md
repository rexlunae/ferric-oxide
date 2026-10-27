# Configuration Settings

Choices about what to build, such as the target, the CPU baseline, kernel
features, the image contents and QEMU settings, live in **`ferric.toml`**. The
rest of this page describes the layers underneath it: the Make variables and
image configuration files inherited from Redox, which `ferric.toml` generates
and which you can still set directly for one-off builds.

- [ferric.toml and features.toml](#ferrictoml-and-featurestoml)
- [Environment Variables](#environment-variables)
  - [.config](#config)
  - [Cookbook Environment Variables](#cookbook-environment-variables)
    - [cookbook.toml](#cookbooktoml)
  - [Changing the QEMU Properties](#changing-the-qemu-properties)
  - [Command Line](#command-line)
  - [mk/config.mk](#mkconfigmk)
    - [build.sh](#buildsh)
- [Filesystem Configuration](#filesystem-configuration)
  - [CPU Architecture Codenames](#cpu-architecture-codenames)
  - [Filesystem Size](#filesystem-size)
  - [Filesystem Customization](#filesystem-customization)
    - [Creating a custom filesystem configuration](#creating-a-custom-filesystem-configuration)
    - [Adding a package to the filesystem configuration](#adding-a-package-to-the-filesystem-configuration)
  - [Binary Recipe Mode](#binary-recipe-mode)
    - [REPO_BINARY](#repo_binary)
  - [Local Recipe Changes](#local-recipe-changes)
  - [Ignored Recipes](#ignored-recipes)
- [Advanced Cookbook Options](#advanced-cookbook-options)
  - [Cookbook TUI Mode](#cookbook-tui-mode)
  - [Cookbook Offline Mode](#cookbook-offline-mode)
  - [Cookbook Logs](#cookbook-logs)
  - [Cookbook Verbosity](#cookbook-verbosity)
  - [Cookbook Compressed Packages](#cookbook-compressed-packages)
  - [Cookbook Clean Build](#cookbook-clean-build)
  - [Cookbook Clean Target](#cookbook-clean-target)
  - [Cookbook Web Generator](#cookbook-web-generator)

## ferric.toml and features.toml

`features.toml`, at the repository root, is the catalog of every build option:
its type, default, dependencies, help text and what it changes. Your choices go
in `ferric.toml` beside it, which is not tracked by git. Options you leave out
take their catalog defaults. `cargo xtask setup` writes a starting
`ferric.toml` if you don't have one.

Edit it with the menu, the CLI, or by hand:

```sh
make menuconfig
cargo run -q -p ferric-config -- list --changed
cargo run -q -p ferric-config -- set image.config=desktop image.packages_add='["vim"]'
cargo run -q -p ferric-config -- explain kernel.trace
cargo run -q -p ferric-config -- check
```

A small example:

```toml
[target]
arch = "x86_64"

[cpu]
x86_64_level = "v3"

[image]
config = "desktop"
packages_add = ["vim", "htop"]
filesystem_size = 4096

[kernel]
iommu = "auto"
```

On each build, the configuration is resolved and written to `build/ferric/`:
`config.mk` (the Make variables described below), `filesystem.toml` (the
image configuration, based on `config/<arch>/<image.config>.toml` plus your
package changes) and `kernel-config.toml`. Recipes affected by a changed
option are rebuilt automatically.

Generated Make defaults use `?=`, so a `.config` file or a variable on the
command line still overrides them; `ferric-config check` warns when `.config`
shadows a setting. Without a `ferric.toml`, the build uses the inherited
defaults described below.

[Build and Configuration](./ferric-build.md#configuration-inputs-and-precedence)
is the full reference, and `tools/ferric-config/README.md` documents the tool.

## Environment Variables

The default values for the build system's Makefile variables are mostly defined in the `mk` directory&mdash;particularly in [`mk/config.mk`](#mkconfigmk). Local changes from the default values, however, should be applied in the [`.config`](#config) file, or temporarily on the `make` [command line](#command-line).

While if you are using Cookbook environment variables in `.config` you need to use the `export` command.

The build system uses GNU Make and Cookbook to coordinate the build system in order. The [build system reference](./build-system-reference.md#cookbook) and [porting application guide](./porting-applications.md#cookbook) have more information about Cookbook.

Three important Makefile variables of interest are `ARCH`, `CONFIG_NAME`, and `BOARD`, as they specify the system to be built. These, and other important environment variables, can be seen in the following table:

| Variable | Definition |
|:---------|:-----------|
|`ARCH`| Specifies the [CPU architecture](#cpu-architecture-codenames) that the system is to be built for. The default value is `x86_64`. |
| `CONFIG_NAME` | Determines the name of the filesystem configuration, and is normally used to construct the `FILESYSTEM_CONFIG` name (the `desktop` variant is used by default). |
| `BOARD` | For [supported QEMU single board computers](https://www.qemu.org/docs/master/system/targets.html) such as Raspberry Pi 3B+ (`raspi3bp`) that require special configuration. Defaults to empty. |
| `FILESYSTEM_CONFIG` | Determines the filesystem configuration file location. See the [Filesystem Configuration](#filesystem-configuration) section below. The default value is `config/$ARCH/$BOARD/$CONFIG_NAME.toml` or `config/$ARCH/$CONFIG_NAME.toml` if `$BOARD` is empty, but this can be changed if the desired configuration file is in a different location. |
| `QEMU_MEM` | Sets the QEMU RAM memory quantity, e.g., `QEMU_MEM=2048`. |
| `QEMU_SMP` | Sets the QEMU CPU core quantity, e.g.,  `QEMU_SMP=4`. |
| `PREFIX_BINARY` | If set to 0 (`PREFIX_BINARY=0`), the build system will build the Redox toolchain from source and will not download the toolchain binaries from the Redox build server. |
| `PREFIX_USE_UPSTREAM_RUST_COMPILER` | If set to 1 (`PREFIX_BINARY=1`) the build system will download the Rust compiler from rustup (Rust upstream compiler binaries) instead of building it (only used with `PREFIX_BINARY=0`) |
| `export COOKBOOK_GIT_TREELESS` | If set to `true` it will enable treeless Git source clone in all recipes to reduce testing time and save storage |
| `REPO_BINARY` | If set to 1 (`REPO_BINARY=1`), the build system will download/install pre-compiled packages from the Redox package server by default, rather than build them from source (i.e., recipes). |
| `REPO_OFFLINE` | Enable the offline mode of Cookbook where recipe sources will not be updated and download the smallest amount of data from the Internet as possible. |
| `REPO_NONSTOP` | Enable the nonstop mode of Cookbook where recipe build failure will not stop the compilation of other recipes. |
| `FILESYSTEM_SIZE` | The size in MB of the filesystem contained in the final image. See the [Filesystem Size](#filesystem-size) section before changing it. |
| `REDOXFS_MKFS_FLAGS` | Flags to the program that builds the RedoxFS file system. The `--encrypt` option enables disk encryption. |
| `PODMAN_BUILD` | If set to 0 (`PODMAN_BUILD?=0`), the build system will use the build environment from your Linux distribution or Unix-like system instead of Podman, see the [Native Build](./building-redox.md) page for more information. If used in the `.config` file it *must* include the `?` symbol in the end of the variable name to work correctly (`PODMAN_BUILD?=1`) |
| `SCCACHE_BUILD` | If set to 1 it will enable the [sccache](https://github.com/mozilla/sccache) object cache for Native Build. See [sccache in Native Build](./advanced-build.md#sccache) page for more information.
| `PODMAN_CACHE_PULL` | If set to 1 will download the cached Podman container instead of creating one from scratch, disable if the cached container is broken
| `FSTOOLS_IN_PODMAN` | If set to 1 (`FSTOOLS_IN_PODMAN=1`), the build system will build the installer inside Podman to avoid FUSE in the host system. See the [Installing without FUSE](./advanced-podman-build.md#installing-without-fuse) page for more information. |
| `FSTOOLS_NO_MOUNT` | If set to 1 (`FSTOOLS_NO_MOUNT=1`), the installer will not use FUSE to create images. See the [Installing without FUSE](./advanced-podman-build.md#installing-without-fuse) section for more information. |
| `CONTAINERFILE` | The Podman container configuration file. See the [Podman Build](./podman-build.md) page for more information. |
| `COOKBOOK_MAKE_JOBS` | The number of maximum CPU cores used when building recipes, default is using all CPU cores from `nproc`. |
| `CI` | If set to any value (`CI=1`), the build system will not activate TUI, and parallel execution of the build step is disabled .|

The image that is built is typically named `build/$ARCH/$CONFIG_NAME/harddrive.img` or `build/$ARCH/$CONFIG_NAME/redox-live.iso`.

### .config

The purpose of the `.config` file is to allow default configuration settings to be changed without explicitly setting those changes in every `make` command (or modifying the contents of the `mk` directory). The file is also included in the `.gitignore` list to ensure it won't be committed by accident.

To permanently override the settings in the [`mk/config.mk`](#mkconfigmk) section, add a `.config` file to the `redox` base directory (i.e., where `make` commands are run) and set the overriding values in that file.

For example, the following configuration specifies the `desktop-minimal` image variant will be built for the `aarch64` CPU architecture. These settings will be applied implicitly to all subsequent `make` commands:

```
ARCH?=aarch64
CONFIG_NAME?=desktop-minimal
```

Prefer `ferric.toml` for settings you want to keep; `.config` is the older mechanism and can shadow it.

> 📝 **Note:** `PODMAN_BUILD?=1` *must* include the question mark to work correctly
> 📝 **Note:** Comments are supported using the `#` character
> 📝 **Note:** Any QEMU option can be inserted
> 📝 **Note:** if [`podman_bootstrap.sh`](./podman-build.md) was run previously, the `.config` file may already exist.
> 💡 **Tip:** When adding environment variables in the `.config` file you can add the `?` symbol at the end of variable names to allow the variable value to be overridden on the command line shell environment.

### Cookbook Environment Variables

Cookbook (the build system) has environment variables that can be tuned for more customization. To set these environment variables, write `export KEY=VALUE` to `.config`. Note that these are environment, not Makefile variables, so the `export` keyword is necessary.

Each Cookbook configuration defaults to environment variables:

| Environment Variable    | How to use the variable      | Description                            |
|--:----------------------|--:---------------------------|--:-------------------------------------|
| `CI`                    | `CI=1`, `CI=`                | Disables TUI                           |
| `COOKBOOK_MAKE_JOBS`    | `COOKBOOK_MAKE_JOBS=4`       | Override build job count               |
| `COOKBOOK_LOGS`         | `COOKBOOK_LOGS=true`         | Override whether to save logs          |
| `COOKBOOK_GIT_TREELESS` | `COOKBOOK_GIT_TREELESS=true` | Globally enable treeless Git clone     |
| `COOKBOOK_OFFLINE`      | `COOKBOOK_OFFLINE=true`      | Override offline behaviour (see notes) |
| `COOKBOOK_VERBOSE`      | `COOKBOOK_VERBOSE=false`     | Override verbosity                     |
| `COOKBOOK_NONSTOP`      | `COOKBOOK_NONSTOP=true`      | Override nonstop (see notes)           |
| `COOKBOOK_CLEAN_TARGET` | `COOKBOOK_CLEAN_TARGET=true` | Clean target directory after build     |
| `COOKBOOK_CLEAN_BUILD`  | `COOKBOOK_CLEAN_BUILD=true`  | Clean build directory before build     |
| `COOKBOOK_COMPRESSED`   | `COOKBOOK_COMPRESSED=true`   | Override package compression           |
| `COOKBOOK_WEB`          | `COOKBOOK_WEB=true`          | Override web content generation        |

> 📝 **Note:** `REPO_OFFLINE=1` and `REPO_NONSTOP=1` are the recommended ways to set options instead of `export COOKBOOK_OFFLINE=true` and `export COOKBOOK_NONSTOP=true`
> 💡 **Tip:** Running Cookbook with `CI=1 COOKBOOK_LOGS=true COOKBOOK_VERBOSE=false` will hide successful build logs in the terminal

#### `cookbook.toml`

Cookbook also has a configuration file. This file is rarely used because `.config` should be sufficient. Any configuration in this file will override configuration from `.config` or environment variables. The `cookbook.toml` configuration below can be used as a template:

The `cookbook.toml` file mainly configures Cookbook options (`[cook]`) and mirrors (`[mirror]`). Mirrors are used to replace the main code and binary source URLs used in Cookbook recipes, useful for a quick way to use alternative sources when the main server is offline or slow. 


```toml
# These options have defaults set below
#[cook]
#jobs = <nproc>
#nonstop = false
#offline = false
#tui = true
#logs = true
#verbose = true
#web = false
#compressed = false

[mirrors]
# The uncommented option below is the default if [mirrors] is not set
# see the list of GNU FTP mirrors at: https://www.gnu.org/prep/ftp.en.html
"ftp.gnu.org/gnu" = "mirrors.ocf.berkeley.edu/gnu"
# "github.com/foo/bar" = "github.com/baz/bar"
```

> 💡 **Tip:** Mirrors option can also be used to override [precompiled Redox packages](#repo_binary) source URL.

### Changing the QEMU Properties

To change the CPU core and RAM memory quantities used when running the image in QEMU, add the following environment variables to your `.config` file:

```
QEMU_SMP?=<number-of-cpu-threads>
QEMU_MEM?=<number-of-memory-in-mib>
```

There's also an option to customize the QEMU machine and CPU type, which is influenced by how `ARCH` and `BOARD` is set. Read the QEMU documentation for more information:

```
QEMU_MACHINE?=<qemu-machine>
QEMU_CPU?=<qemu-cpu-type>
```

There is more options that can be customized, these are defined in `mk/qemu.mk`:

```
audio?=<no|ac97|hda>
disk?=<ata|nvme|usb|virtio|cdrom|sdcard>
net?=<e1000|rtl8139|virtio|usb-net|redir>
netboot?=<yes|no>
kvm?=<yes|no>
bridge?=...
gpu?=<no|vga|ramfb|multi|virtio|virtio-sdl|virtio-gl>
uefi?=<yes|no>
gdb?=<yes|nonblock|no>
```

### Command Line

The default settings in `mk/config.mk` can be overridden by explicitly setting them on the `make` command line.

For example, the following command builds the `demo` image variant and loads it into QEMU:

```sh
make CONFIG_NAME=demo qemu
```

Some environment variables can also be set for the lifetime of the current shell by setting them at the command line:

```sh
export ARCH=aarch64; make all
```

Overriding settings in this way is only temporary, however. Additionally, for those using the [Podman Build](./podman-build.md), some settings may be ignored when using this method. For best results, use [`.config`](#config).

### mk/config.mk

The build system uses several Makefiles, most of which are in the `mk` directory. Most settings of interest have have been grouped together in `mk/config.mk`.

Feel free to open `mk/config.mk` in your favorite editor and have a look through it; just be sure not to apply any changes:

```sh
nano mk/config.mk
```

The `mk/config.mk` file should never be modified directly, especially if you are contributing, as doing so could create conflicts in the `make pull` command.

To apply lasting changes to environment variables, please refer to the [`.config`](#config) section. To apply changes only *temporarily*, see the [Command Line](#command-line) section.

#### build.sh

The `build.sh` script allows you to easily set `ARCH`, `FILESYSTEM_CONFIG` and `CONFIG_NAME` when running `make`. If you are not changing the values very often, it is recommended that you set the values in [`.config`](#config) rather than use `build.sh`. But if you are testing against different CPU architectures or configurations, this script can help minimize effort, errors and confusion.

```sh
./build.sh [-a <ARCH>] [-c <CONFIG_NAME>] [-f <FILESYSTEM_CONFIG>] <TARGET> ...
```

The `TARGET` parameter may be any valid `make` target, although the recommended target is `qemu`. Additional variable settings may also be included, such as `gpu=no`

| Option | Description |
|:-------|:------------|
| `-a <ARCH>` | The CPU architecture you are building for, `x86_64`, `i586`, `aarch64` or `riscv64gc`. Uppercase options `-X`, `-5`, `-A`, `-R` can be used as shorthands for `-a x86_64`, `-a i586`, `-a aarch64` and `riscv64gc`, respectively. Ferric Oxide supports only `x86_64` and `aarch64`; the others remain for compatibility with imported code. |
| `-c <CONFIG_NAME>` | The name of the filesystem configuration which appears in the name of the image being built. |
| `-f <FILESYSTEM_CONFIG>` | Determines the filesystem configuration file location, which can be in any location but is normally in directory `config/$ARCH`.<br><br>📝 **Note:** If you *do* specify `-f <FILESYSTEM_CONFIG>`, but not `-a` or `-c`, the file path determines the other values. Normally the file would be located at e.g., `config/x86_64/desktop.toml`. `ARCH` is determined from the second-to-last element of the path. If the second last element is not a known `ARCH` value, you must specify `-a ARCH`. `CONFIG_NAME` is determined from the *basename* of the file. |

The default value of `FILESYSTEM_CONFIG` is constructed from `ARCH` and `CONFIG_NAME`: `config/$ARCH/$CONFIG_NAME.toml`.

The default values for `ARCH` and `CONFIG_NAME` are `x86_64` and `desktop`, respectively. These produce a default `FILESYSTEM_CONFIG` value of `config/x86_64/desktop.toml`.

## Filesystem Configuration

The packages to be included in the final image are determined by the chosen **filesystem configuration** file, which is a `.toml` file (e.g., `config/x86_64/desktop.toml`). Open `desktop.toml` and have a look through it:

```sh
nano config/x86_64/desktop.toml
```

For each supported CPU architecture, there are some filesystem configurations to choose from. For `x86_64`, there are `desktop`, `demo` and `server` configurations, as well as a few others. Feel free to browse the `config/x86_64` directory for more examples.

For more details on the filesystem configuration, and how to add additional packages to the build image, please see the [Including Programs in Redox](./including-programs.md) page.

Feel free to create your own **filesystem configuration**.

### CPU Architecture Codenames

The build system supports cross-compilation to other CPU architectures. The CPU architecture that Redox is built for (specified by the `ARCH` environment variable) usually determines the filesystem configuration file that will be used by the build system.

See the currently supported CPU architectures below:

| CPU Architecture | Other Aliases |
|:---------------- |:----------- |
| `i586` | x86 (32-bit), IA32, `x86` (not supported by Ferric Oxide) |
| `x86_64` | x86 (64-bit), `x86-64`, `amd64`, `x64` |
| `aarch64` | ARM (64-bit), ARMv8, `ARM64` |
| `riscv64gc` | RISC-V (64-bit) |

The filesystem configurations for a given CPU architecture can be found in the [`config`](https://github.com/rexlunae/ferric-oxide/tree/main/config) folder's correspondingly named sub-directory (e.g. `config/x86_64`).

### Filesystem Size

The filesystem size is the total size of the image, in MiB, including all programs. For a *livedisk* system the whole image is loaded into RAM, so it must leave room in your system's (or QEMU's `QEMU_MEM`) memory to run.

The size comes from the **filesystem configuration** file, e.g. `config/x86_64/demo.toml`:

```toml
[general]
...
filesystem_size = 768
...
```

That value is where sizing starts. The installer adds up what the configured packages and their dependencies take once installed, reading the archives in `repo/<target>/`, adds the configuration's own files and headroom (a quarter more, plus 64 MiB), and grows the image when `filesystem_size` is smaller. It prints both numbers:

```
Image needs 940 MiB (700 MiB of packages); building it at 940 MiB
```

To fix the size instead, set `filesystem_size_exact = true` beside `filesystem_size`, set `image.filesystem_size` in `ferric.toml` (`make menuconfig`), or pass it on the `make` command line:

```sh
make FILESYSTEM_SIZE=2048 image qemu
```

A fixed size that is too small stops the build before anything is written, with the size the image needs.

### Filesystem Customization

The image can be customized by tweaking the configuration files at `config/$ARCH/*.toml`. However, it is recommended that you create your own configuration file and apply changes there.
(The configuration files at `config/$ARCH` can override the data type values from the filesystem templates at `config`)

#### Creating a custom filesystem configuration

The following items describe the process for creating a custom filesystem configuration file (`my-desktop.toml`):

1. Create the `my-desktop.toml` file from an existing filesystem configuration:

    ```sh
    cp config/$ARCH/desktop.toml config/$ARCH/my_desktop.toml
    ```

2. Add the following to the `.config` file to set the new configuration as the build system's default:

    ```
    CONFIG_NAME?=my_desktop
    ```

Many filesystem configuration settings can be adjusted. See the templates in the `config` folder for reference.

> 💡 **Tip:** files named with the prefix "`my-`" in the `redox` repo are git-ignored. Be sure to follow this convention for all custom filesystem configurations to avoid accidentally committing them to Ferric Oxide.

#### Adding a package to the filesystem configuration

In the following example, the `acid` package is added to the `my_desktop.toml` configuration:

1. Open the `my-desktop.toml` file:

    ```sh
    nano config/$ARCH/my-desktop.toml
    ```

2. Add the `acid` package to the `[packages]` section:

    ```toml
    [packages]
    acid = {}
    ```

3. Build the `acid` package and update the image:

    ```sh
    make rp.acid
    ```

Done! The `acid` package is now included in your image.

### Binary Recipe Mode

Without `ferric.toml`, the build system builds all packages from source (i.e., recipes). The `native` profile that `cargo xtask setup` selects turns on `build.repo_binary` instead, which installs prebuilt packages for applications, and there's also a recipe rule and filesystem configuration option to choose per package. Ferric Oxide's cookbook has no default remote: set `remote` and its signing key `remote_key` in the `[cook]` table of `cookbook.toml` (or `COOKBOOK_REMOTE` and `COOKBOOK_REMOTE_KEY`), as described in [Prebuilt packages](./ferric-packaging.md#prebuilt-packages-repo_binary). Without them, binary recipes are built from source.

This is useful for some purposes, such as producing development builds, confirming package status from the package server, and reducing image build time with large programs.

- This mode will use the pre-built packages of recipe dependencies if compilation is needed
- If a pre-built package of the recipe is not available, it will fallback to source compilation

You can learn how to enable this mode in one recipe below:

- Temporary pre-built recipe package

The following command will change the recipe rule to `binary` and install to an existing image, the package will be removed once a new image is created.

    ```sh
    make bcrp.recipe-name
    ```

- Permanent pre-built recipe package

1. Open the `my-config.toml` file:

    ```sh
    nano config/my-config.toml
    ```

2. Add the binary package below the `[packages]` section:

    ```toml
    [packages]
    ...
    recipe-name = "binary"
    ...
    ```

3. Download and add the binary package on your image:

    ```sh
    make bc.recipe-name p.recipe-name
    ```

4. Open QEMU to verify your package:

    ```sh
    make qemu
    ```

#### REPO_BINARY

In the previous example, the build system's default behavior was overridden by explicitly setting a package to use a pre-built binary. To configure the build system to download pre-built packages by *default*, however, we can set the `REPO_BINARY` environment variable (`REPO_BINARY?=1`).

When `REPO_BINARY` is enabled, the image is made to use pre-built binaries for all packages assigned to `{}`; when `REPO_BINARY` is *disabled*, however, those same packages are compiled from source (i.e., recipes).

(This environment variable has the same source compilation behavior of `recipe = "binary"` seen above)

For example:

```toml
[packages]
...
recipe-name1 = {} # use the REPO_BINARY setting ("source" if 0; "binary" if 1)
recipe-name2 = "binary" # pre-built package
recipe-name3 = "source" # source-based recipe
...
```

If you just want to use pre-built packages in one filesystem configuration you can add the `repo_binary = true` data type below the `[general]` section on it.

You can also use the following command to quickly enable source compilation in one recipe:

```sh
make sc.recipe-name
```

### Local Recipe Changes

By default every time a recipe build is triggered, Cookbook will update the recipe source. Cookbook will check the tarball BLAKE3 hash from the recipe configuration (`recipe.toml`), or pull from the `origin` remote when the recipe source is a Git repository. This will also remove local changes that are not saved in a branch.

To preserve and use local changes and skip updating the source for a specific recipe, change the recipe rule or type to `"local"`. Use one of the following methods:

- Command line

```sh
make lc.recipe-name
```

- Filesystem configuration

You can also use this method to prevent accidents.

```toml
[packages]
...
package-name = "local"
...
```

The old way for preserving and using local changes is by commenting out the `[source]` section at the top of the file of a `recipe.toml` is also working but less recommended as it's prone to merge conflicts when pulling the repository.

Use this if you want to prevent accidents.

Example:

```toml
# [source]
# git = "https://gitlab.redox-os.org/redox-os/games.git"
```

### Ignored Recipes

Included files are loaded depth first, each before the file that includes it, and each file only once: when two includes share a base (`desktop.toml` includes `desktop-minimal.toml` and `server.toml`, which both include `minimal.toml`), the base is loaded the first time and skipped the second, so it cannot undo the overrides loaded after it.

You can disable the installation of some recipes in the image by ignoring, this is commonly used to disable recipes imported from other filesystem configurations to yours using the `include` data type.

To quickly ignore them use the following command:

```sh
make nc.recipe-name
```

You can also do this on your filesystem configuration:

```toml
[packages]
recipe-name = "ignore"
```

## Advanced Cookbook Options

This section has details of the [Cookbook Environment Variables](#cookbook-environment-variables) options.

### Cookbook TUI Mode

Cookbook has a TUI mode which is enabled by default. This mode speedup the build process because it allows the source fetch and compilation to be done in parallel. When the `CI` environment variable is set the TUI mode will be disabled, which is desired for builds under Continuous Integration (CI).

### Cookbook Offline Mode

Cookbook also has a mode where it will reduce Internet activity by adding `REPO_OFFLINE=1` into `.config`. This mode is useful when you are in places where the Internet connection is slow or absent, or when you want a fixed build system state or faster incremental compilation.

In this mode, Cookbook will not update the source of any recipe or a package binary if also set with `REPO_BINARY=1`. It also adds the `--offline` option to some Cargo build methods inside recipes where it is supported. When Cookbook or Cargo must access the Internet because sources do not exist locally it will throw an error instead.

To temporarily allow Cookbook and Cargo to have Internet activity and update sources, run `make f.package-name` (single recipe source fetch) or `make fetch` (to fetch all enabled recipe sources), as these commands will ignore the `REPO_OFFLINE` environment variable.

### Cookbook Logs

When `export COOKBOOK_LOGS=true` is set, all build logs will be written into the `build/logs/$TARGET` directory. This is enabled by default when TUI mode is enabled because build logs are captured anyway. When logs are enabled it can alter the build script and maybe hide some essential progress bar. Which is why when `CI=1`, logs are disabled.

Under the hood when the TUI mode or log capture is enabled, Cookbook will try to provide pseudo-terminal support and pipe them as-is to the terminal, then strip colors before saving them to logs.

### Cookbook Verbosity

When `export COOKBOOK_VERBOSE=false` is set, it disables certain debug logs and disables the GNU Bash's `set -x` command, if combined with `CI=1 COOKBOOK_LOGS=true`, it will hide success build logs to the terminal. 

Usually you want to combine `CI=1 COOKBOOK_LOGS=true COOKBOOK_VERBOSE=false` because it will not replace build logs that are doing the build instead of caching it and creating a minimally distracting build output on terminal.

## Cookbook Compressed Packages

When `export COOKBOOK_COMPRESSED=true` is set, it will package software with compression turned on. Under the hood it's compressing with the [LZMA2](https://en.wikipedia.org/wiki/LZMA) algorithm which is quick to decompress but slow to compress. 

It is not enabled by default because it uses considerable CPU time to compress and storage saving is negligible compared to [Cookbook Clean Target](#cookbook-clean-target).

## Cookbook Clean Build

When `export COOKBOOK_CLEAN_BUILD=true` is set, it will always remove [the build directory](./build-system-reference.md#cookbook) before building. This is intended for build servers where build consistency matters and avoid a build failure due to a stale `build` directory. It has the side effect of disabling incremental compilation thus generally avoided for local development.

## Cookbook Clean Target

When `export COOKBOOK_CLEAN_TARGET=true` is set, it will remove [the build, stage, and sysroot directories](./build-system-reference.md#cookbook) after a build success. This is intended to save storage space for local development that frequently jumps between recipes.

Unlike `COOKBOOK_CLEAN_BUILD`, it allows inspection of `build` directory and allows incremental compilation if build is failing. It does not allow visualization of the `build` and `stage` directories but creates `stage.files` which contains tree of files in the `stage` directory before deletion.

## Cookbook Web Generator

When `export COOKBOOK_WEB=true` is set, it will create a `web` directory containing HTML files for published `repo`. It contains much more information including the published file tree and dependents, intended for easy browsing.
