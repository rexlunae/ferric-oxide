# Configuration file of the build system environment variables

-include .config

## Ferric Oxide feature configuration. ferric.toml holds the options chosen with
## `make menuconfig` (or edited by hand; features.toml describes them all) and is
## turned into build/ferric/config.mk, which sets the variables below with ?=.
## Without ferric.toml nothing changes. See tools/ferric-config/README.md.
FERRIC_CONFIG?=ferric.toml
FERRIC_OUT=build/ferric
FERRIC_CONFIG_TOOL?=env -u RUSTUP_TOOLCHAIN -u CC -u TARGET -u RUSTFLAGS cargo run --quiet --release \
	--manifest-path tools/ferric-config/Cargo.toml -- --config $(FERRIC_CONFIG) --out $(FERRIC_OUT)
# Capture explicit target assignments before generated/default values are loaded.
FERRIC_TARGET_OVERRIDES:=$(if $(filter undefined,$(origin ARCH)),,--override target.arch=$(ARCH)) $(if $(filter undefined,$(origin BOARD)),,--override target.board=$(BOARD)) $(if $(filter undefined,$(origin CONFIG_NAME)),,--override image.config=$(CONFIG_NAME)) $(if $(FERRIC_CPU_VARIANT),--override cpu.$(ARCH)_level=$(FERRIC_CPU_VARIANT))
FERRIC_CONFIG_GOALS:=menuconfig config-check config-list config-generate
FERRIC_CONFIG_ONLY:=$(if $(strip $(MAKECMDGOALS)),$(if $(filter-out $(FERRIC_CONFIG_GOALS),$(MAKECMDGOALS)),,1))
ifneq ($(FERRIC_CONFIG_ONLY),1)
ifneq ($(wildcard $(FERRIC_CONFIG)),)
# Resolve before reading the include. Target overrides need not change any
# file's timestamp, and Make 3.81 cannot reliably detect same-second rewrites.
FERRIC_CONFIG_STATUS:=$(shell $(FERRIC_CONFIG_TOOL) generate $(FERRIC_TARGET_OVERRIDES) >&2; echo $$?)
ifneq ($(FERRIC_CONFIG_STATUS),0)
$(error ferric-config generation failed)
endif
include $(FERRIC_OUT)/config.mk
endif
endif

HOST_ARCH?=$(shell uname -m)

# Configuration
## Architecture to build Redox for (aarch64, i586, or x86_64). Defaults to a host one
ARCH?=$(subst arm64,aarch64,$(HOST_ARCH))
## Sub-device type for aarch64 if needed
BOARD?=
## Enable to use binary prefix (much faster)
PREFIX_BINARY?=1
## llvm uses host Clang and rustup; recipe preserves the inherited toolchain.
PREFIX_TOOLCHAIN?=recipe
## Enable to use up-to-date rust compiler (experimental, only available to Tier 2 targets)
## Even more experimental, add -Zbuild-std to cookbook.toml to allow compilation to Tier 3 targets
PREFIX_USE_UPSTREAM_RUST_COMPILER?=0
## Enable to use binary packages (much faster)
REPO_BINARY?=
## Name of the configuration to include in the image name e.g. desktop or server
CONFIG_NAME?=desktop
## Build appstream data for repo
REPO_APPSTREAM?=0
## Ignore errors when building the repo, attempt to build every package
REPO_NONSTOP?=0
## Do not update source repos, attempt to build in offline condition
REPO_OFFLINE?=0
## Build local packages unoptimized, with debug assertions and debug info (slow at run time)
REPO_DEBUG?=0
## Ferric Oxide: keep debug info in optimized local builds and do not strip them
REPO_DEBUGINFO?=0
## Old config value that need to be corrected
ifeq ($(ARCH),i686)
	ARCH=i586
endif
## Select filesystem config
ifeq ($(BOARD),)
ifeq ($(wildcard config/$(ARCH)/$(CONFIG_NAME).toml),)
FILESYSTEM_CONFIG?=config/$(CONFIG_NAME).toml
else
FILESYSTEM_CONFIG?=config/$(ARCH)/$(CONFIG_NAME).toml
endif
else
FILESYSTEM_CONFIG?=config/$(ARCH)/$(BOARD)/$(CONFIG_NAME).toml
endif
HOST_CARGO=env -u RUSTUP_TOOLCHAIN -u CC -u TARGET cargo
## Filesystem size in MB (default comes from filesystem_size in the FILESYSTEM_CONFIG)
## FILESYSTEM_SIZE?=$(shell $(INSTALLER) --filesystem-size -c $(FILESYSTEM_CONFIG))
## Flags to pass to redoxfs-mkfs. Add --encrypt to set up disk encryption
REDOXFS_MKFS_FLAGS?=
## Set to 1 to enable Podman build, any other value will disable it
PODMAN_BUILD?=1
## Set to 1 to put filesystem tools inside podman, any other value will install it to host
FSTOOLS_IN_PODMAN?=0
## Set to 1 if FUSE is not available and we are running in a container
FSTOOLS_NO_MOUNT?=0
## Enable sccache to speed up cargo builds
## only do this by default if this is inside podman
SCCACHE_BUILD?=$(shell [ -f /run/.containerenv ] && echo 1 || echo 0)
## The containerfile to use for the Podman base image
CONTAINERFILE?=podman/redox-base-containerfile

# Per host variables
NPROC=nproc
SED=sed
FIND=find
REPO_BIN=./target/release/repo

ifneq ($(PODMAN_BUILD),1)
FSTOOLS_IN_PODMAN=0
HOST_TARGET := $(shell env -u RUSTUP_TOOLCHAIN rustc -vV | grep host | cut -d: -f2 | tr -d " ")
# x86_64 linux hosts have all toolchains
ifeq ($(PREFIX_BINARY),1)
ifneq ($(PREFIX_TOOLCHAIN),llvm)
ifeq ($(HOST_TARGET),aarch64-unknown-linux-gnu)
	ifneq ($(ARCH),aarch64)
	ifneq ($(ARCH),x86_64)
        $(info The $(ARCH) binary prefix is only built for x86_64 Linux hosts)
		PREFIX_BINARY=0
	endif
	endif
else ifeq ($(HOST_TARGET),x86_64-unknown-linux-gnu)
else
    $(info The $(ARCH) binary prefix is only built for Linux hosts)
	PREFIX_BINARY=0
endif
endif
endif
endif

ifeq ($(SCCACHE_BUILD),1)
ifeq (,$(shell command -v sccache))
    $(info sccache not found in PATH)
	SCCACHE_BUILD=0
endif
endif

ifeq ($(REPO_APPSTREAM),1)
	export COOKBOOK_APPSTREAM=true
endif
ifeq ($(REPO_NONSTOP),1)
	export COOKBOOK_NONSTOP=true
endif
ifeq ($(REPO_OFFLINE),1)
	export COOKBOOK_OFFLINE=true
endif
ifeq ($(REPO_DEBUG),1)
	export COOKBOOK_NOSTRIP=true
	export COOKBOOK_DEBUG=true
#TODO: https://gitlab.redox-os.org/redox-os/relibc/-/issues/226
#	export PROFILE=debug
#	export RUSTCFLAGS="-Cdebuginfo=2"
endif
ifeq ($(REPO_DEBUGINFO),1)
	export COOKBOOK_NOSTRIP=true
	export COOKBOOK_DEBUGINFO=true
endif

UNAME := $(shell uname)
ifeq ($(UNAME),Darwin)
	FUMOUNT=umount
	NPROC=sysctl -n hw.ncpu
	SED=gsed
	FIND=gfind
	ifeq ($(PREFIX_TOOLCHAIN),llvm)
		SED=sed
		FIND=find
	endif
	VB_AUDIO=coreaudio
	VBM=/Applications/VirtualBox.app/Contents/MacOS/VBoxManage
else ifeq ($(UNAME),FreeBSD)
	FIND=gfind
	FUMOUNT=sudo umount
	VB_AUDIO=pulse # To check, will probably be OSS on most setups
	VBM=VBoxManage
else ifeq ($(UNAME),Redox)
	PODMAN_BUILD=0
	export CARGO_NET_GIT_FETCH_WITH_CLI=true
# TODO: allow overriding to cross compiler toolchain when build server have one prebuilt
	HOSTED_REDOX=1
# Don't check for dependencies if you will be using Hosted Redox
	SKIP_CHECK_TOOLS=1
ifneq ($(shell which repo),)
	REPO_BIN=repo
endif
else
	# Detect which version of the fusermount binary is available.
	ifneq (, $(shell which fusermount3))
		FUMOUNT=fusermount3 -u
	else
		FUMOUNT=fusermount -u
	endif

	VB_AUDIO=pulse
	VBM=VBoxManage
endif

# Automatic variables
ROOT=$(CURDIR)
export RUST_COMPILER_RT_ROOT=$(ROOT)/rust/src/llvm-project/compiler-rt
export TESTBIN?=
RUNNING_IN_PODMAN=$(shell [ -f /run/.containerenv ] && echo 1 || echo 0)
ifeq ($(PODMAN_BUILD),1)
# Don't check for dependencies if you will be using Podman
SKIP_CHECK_TOOLS=1
ifeq ($(RUNNING_IN_PODMAN),1)
$(info Please unset PODMAN_BUILD=1 in .config!)
endif
endif

ALLOW_FSTOOLS?=0
ifeq ($(FSTOOLS_IN_PODMAN),0)
ifeq ($(RUNNING_IN_PODMAN),0)
ALLOW_FSTOOLS=1
endif
endif

## Userspace variables
ifeq ($(ARCH),riscv64gc)
	export TARGET=riscv64gc-unknown-redox
	export GNU_TARGET=riscv64-unknown-redox
else
	export TARGET=$(ARCH)-unknown-redox
	export GNU_TARGET=$(ARCH)-unknown-redox
endif
BUILD=build/$(ARCH)/$(CONFIG_NAME)
MOUNT_DIR=$(BUILD)/filesystem
FSTOOLS=build/fstools
INSTALLER=$(FSTOOLS)/bin/redox_installer
REDOXFS=$(FSTOOLS)/bin/redoxfs
REDOXFS_MKFS=$(FSTOOLS)/bin/redoxfs-mkfs
INSTALLER_OPTS=--cookbook=. --config-name=$(CONFIG_NAME)
INSTALLER_FEATURES=
REDOXFS_FEATURES=
COOKBOOK_OPTS="--filesystem=$(FILESYSTEM_CONFIG)"
ifeq ($(REPO_BINARY),1)
INSTALLER_OPTS+=--repo-binary
COOKBOOK_OPTS+=--repo-binary
endif
## Ferric Oxide: finish the image root with fox (features.toml
## packages.manager = "fox" sets FERRIC_FOX_IMAGE=1). fox adopts the packages
## the installer placed, syncs the package set FERRIC_FOX_PACKAGES if one is
## given (packages.set; fox.lock is written beside it, and
## FERRIC_FOX_LOCKED=1 from packages.locked refuses anything it does not pin),
## and writes the image SBOM to $(FERRIC_FOX_SBOM). FERRIC_FOX_LEVEL (v3 or v4,
## from cpu.x86_64_level) is the x86-64 level the set's packages are
## installed for; without it fox uses the set's policy, else v3.
FERRIC_FOX_IMAGE?=0
FERRIC_FOX_PACKAGES?=
FERRIC_FOX_LOCKED?=0
FERRIC_FOX_LEVEL?=
FERRIC_FOX_BIN?=target/release/fox
ifeq ($(HOSTED_REDOX),1)
FERRIC_FOX_BIN=fox
endif
FERRIC_FOX_SBOM=$(BUILD)/image.cdx.json
ifeq ($(FERRIC_FOX_IMAGE),1)
INSTALLER_OPTS+=--fox=$(FERRIC_FOX_BIN) --fox-sbom=$(FERRIC_FOX_SBOM)
ifneq ($(FERRIC_FOX_PACKAGES),)
INSTALLER_OPTS+=--fox-packages=$(FERRIC_FOX_PACKAGES)
ifeq ($(FERRIC_FOX_LOCKED),1)
INSTALLER_OPTS+=--fox-locked
endif
ifneq ($(FERRIC_FOX_LEVEL),)
INSTALLER_OPTS+=--fox-level=$(FERRIC_FOX_LEVEL)
endif
endif
endif
ifeq ($(FSTOOLS_NO_MOUNT),1)
INSTALLER_OPTS+=--no-mount
INSTALLER_FEATURES=--no-default-features --features installer
REDOXFS_FEATURES= --no-default-features --features std,log
endif

REPO_TAG=$(BUILD)/repo.tag
FSTOOLS_TAG=build/fstools.tag
export BOARD FIND

ifeq ($(SCCACHE_BUILD),1)
	export CC_WRAPPER:=sccache
	export RUSTC_WRAPPER:=$(CC_WRAPPER)
endif

ifeq ($(HOSTED_REDOX),1)
FSTOOLS_TAG=
endif

## If Podman is being used, a container is required
ifeq ($(PODMAN_BUILD),1)
CONTAINER_TAG=build/container.tag
else
CONTAINER_TAG=
endif
