# Configuration file for redox-installer, Cookbook and RedoxFS FUSE

fstools: $(FSTOOLS_TAG) $(FSTOOLS)

GOING_TO_PODMAN_AGAIN?=0

# These tools run inside Podman if it is used, or on the host if Podman is not used
$(FSTOOLS): $(shell find tools/installer crates/fox-core crates/fox-store -type f -o -type d) Cargo.lock
$(FSTOOLS): | prefix $(CONTAINER_TAG) $(FSTOOLS_TAG)
ifeq ($(PODMAN_BUILD),1)
ifeq ($(FSTOOLS_IN_PODMAN),1)
	$(PODMAN_RUN) make $@
else
	$(MAKE) $@ PODMAN_BUILD=0 SKIP_CHECK_TOOLS=1 GOING_TO_PODMAN_AGAIN=1
endif
else
	rm -rf $@ $@.partial
	mkdir -p $@.partial
	ln -s ../../recipes $@.partial/recipes
	$(MAKE) fstools_fetch PODMAN_BUILD=$(GOING_TO_PODMAN_AGAIN)

	# Compile installer and redoxfs for host (may be outside of podman container)
	# Ferric Oxide: redoxfs's copy in recipes/core/redoxfs/source reaches the
	# shared crates at ../../crates, as in the redoxfs and base recipes.
	ln -sfn $(ROOT)/crates $(ROOT)/recipes/core/crates
	cd $@.partial && \
		export CARGO_TARGET_DIR=../$@-target && \
		$(HOST_CARGO) install --root . --path $(ROOT)/tools/installer --locked $(INSTALLER_FEATURES) && \
		$(HOST_CARGO) install --root . --path recipes/core/redoxfs/source --locked $(REDOXFS_FEATURES)

	mv $@.partial $@
	touch $@
endif

fstools_fetch: $(FSTOOLS_TAG) FORCE
ifeq ($(PODMAN_BUILD),1)
	$(PODMAN_RUN) make $@
else
	$(REPO_BIN) fetch host:redoxfs
endif

CARGO_OFFLINE_FLAG=
ifeq ($(REPO_OFFLINE),1)
CARGO_OFFLINE_FLAG=--offline
endif

$(FSTOOLS_TAG): $(CONTAINER_TAG) Cargo.toml Cargo.lock $(wildcard tools/*/Cargo.toml crates/fox-*/Cargo.toml)
$(FSTOOLS_TAG): $(shell find tools/cookbook/src tools/redoxer/src tools/installer/src tools/fox/src crates/fox-* -type f)
ifeq ($(PODMAN_BUILD),1)
	$(PODMAN_RUN) make $@
else
	$(HOST_CARGO) build --manifest-path Cargo.toml --release --locked -p redox_cookbook --bins -p fox $(CARGO_OFFLINE_FLAG)
	mkdir -p $(@D)
	touch $@
endif

repo_editor: $(FSTOOLS_TAG)
ifeq ($(PODMAN_BUILD),1)
	$(PODMAN_RUN) make $@
else
	CONFIG=$(FILESYSTEM_CONFIG) $(REPO_BIN)_editor
endif

# --- Guest build tools (features.toml build.build_tools)
#
# Recipes name the tools they need at compile time in `build.dev-dependencies`;
# a "host:" tool is such a tool. Building those on the host makes every
# contributor install things the image does not want, and the tools are then the
# only part of the image's software never exercised the way the image uses them.
# With build.build_tools = "guest" they are built for the target and run inside
# a guest instead.
#
# One guest environment is installed here and reused for the whole image build
# ($(GUEST_TOOL_ENV)); each tool is then cooked inside it. The environment is
# keyed on the tool set and the repository's lockfile, so a rebuild does not
# pay for a fresh installation. build.keep_build_tools leaves the tools in the
# final image as the build-tools meta-package rather than in this scratch copy.
#
# A tool built for a foreign architecture cannot run inside a guest on this
# host, so cross-compiled builds keep the host tools: FERRIC_BUILD_TOOLS_EFFECTIVE
# falls back to "host" and config-check warns about it.
#
# The environment directory holds everything a run compares: the tool list
# ($(GUEST_TOOL_MANIFEST)), the installer configuration generated from it
# ($(GUEST_TOOL_CONFIG)) and the installed image ($(GUEST_TOOL_IMAGE)). The
# installation is built in $(GUEST_TOOL_ENV_PARTIAL) and moved in as the last
# step, and it leaves the manifest and configuration in place, so a finished
# build leaves every target newer than its own prerequisites and a second make
# is a no-op.

GUEST_TOOL_ENV=$(BUILD)/build-tools
GUEST_TOOL_ENV_PARTIAL=$(BUILD)/.build-tools.partial
GUEST_TOOL_MANIFEST=$(GUEST_TOOL_ENV)/manifest.txt
GUEST_TOOL_CONFIG=$(GUEST_TOOL_ENV)/build-tools.toml
GUEST_TOOL_IMAGE=$(GUEST_TOOL_ENV)/guest.img
GUEST_TOOL_IMAGE_PARTIAL=$(BUILD)/.build-tools.partial/guest.img
GUEST_TOOL_STAMP=$(GUEST_TOOL_ENV).stamp
GUEST_TOOL_SIZE_MIB?=4096
# Recipes whose build tools the environment holds. Empty means "the image's
# packages", which the installer already knows; make passes this when it only
# wants the tool set rather than an image.
GUEST_TOOL_RECIPES?=
FERRIC_BUILD_TOOLS?=host
FERRIC_KEEP_BUILD_TOOLS?=0
FERRIC_GUEST_BUILD_ENV?=0

# The tools are built for the target, so a guest can run them only when the
# guest architecture is this host's. Anything else keeps the host tools.
# Both names go through the same arm64 -> aarch64 normalization ARCH uses.
NORMALIZED_HOST_ARCH=$(subst arm64,aarch64,$(HOST_ARCH))
ifeq ($(ARCH),$(NORMALIZED_HOST_ARCH))
FERRIC_BUILD_TOOLS_EFFECTIVE=$(FERRIC_BUILD_TOOLS)
else
FERRIC_BUILD_TOOLS_EFFECTIVE=host
endif

# The cookbook reads this to build a recipe's tools for the target and run them
# in the guest, so it must see the value this build actually uses, not the one
# the configuration asked for: a cross build keeps the host tools and says so.
export FERRIC_BUILD_TOOLS:=$(FERRIC_BUILD_TOOLS_EFFECTIVE)

# Where the guest environment's image is, so a recipe's build script can run a
# tool that was built for the target. Empty when the tools come from the host,
# which is what makes the scripts run them directly.
ifeq ($(FERRIC_BUILD_TOOLS_EFFECTIVE),guest)
export FERRIC_GUEST_TOOLS_DIR:=$(abspath $(GUEST_TOOL_IMAGE))
else
export FERRIC_GUEST_TOOLS_DIR:=
endif

# build_tools is the prerequisite the image targets use for whatever this
# configuration resolved to: the guest environment, or the host tools.
ifneq ($(FERRIC_BUILD_TOOLS_EFFECTIVE),guest)
build_tools: $(FSTOOLS)
else ifneq ($(FERRIC_GUEST_BUILD_ENV),1)
build_tools: $(FSTOOLS)
else
build_tools: $(GUEST_TOOL_STAMP)
endif

# The final image installs the tools only when build.keep_build_tools (its
# general.build_tools effect) asks for them; the list is the same one the
# guest environment was built from, so both agree on what the tools are.
BUILD_TOOLS_INSTALLER_OPT=
ifeq ($(FERRIC_KEEP_BUILD_TOOLS),1)
BUILD_TOOLS_INSTALLER_OPT=--build-tools-list $(abspath $(GUEST_TOOL_MANIFEST))
build_tools: $(GUEST_TOOL_MANIFEST)
endif

# The guest environment is a cook of its own ($(REPO_BIN) cook $(GUEST_TOOL_MANIFEST)),
# so it must be reachable without the image's cook: when the environment does
# not exist yet the tools are built there with the host's, which is what
# features.toml documents. $(GUEST_TOOLS_EDGE) is the prerequisite the image's
# cook (mk/repo.mk) adds: the environment when this build cooks with it, and
# otherwise nothing.
#
# It is $(GUEST_TOOL_STAMP) and not build_tools because $(REPO_TAG), whose
# recipe is the image's cook, waits for it. build_tools also carries the
# manifest named by --build-tools-list, and the environment's own installation
# reads that same manifest, so a cook that waited for build_tools would make
# the environment depend on the cook that needs it.
ifeq ($(FERRIC_BUILD_TOOLS_EFFECTIVE),guest)
GUEST_TOOLS_EDGE=$(GUEST_TOOL_STAMP)
endif

$(GUEST_TOOL_MANIFEST): $(FSTOOLS_TAG) $(FILESYSTEM_CONFIG)
	mkdir -p $(@D)
	# The tools the image's own package set needs at build time; an explicit
	# GUEST_TOOL_RECIPES overrides it for a narrower environment.
	$(REPO_BIN) build-tools-list $(if $(GUEST_TOOL_RECIPES),$(GUEST_TOOL_RECIPES),\
		--filesystem=$(FILESYSTEM_CONFIG)) > $@.partial
	mv $@.partial $@

$(GUEST_TOOL_CONFIG): $(GUEST_TOOL_MANIFEST)
	@printf '%s\n' \
		'# Generated by mk/fstools.mk: the guest build environment.' \
		'# One guest holding every tool the image'"'"'s recipes build with, so the' \
		'# tools can run where they are built instead of on the host.' \
		'[general]' \
		'filesystem_size = $(GUEST_TOOL_SIZE_MIB)' \
		'no_mount = true' \
		'cookbook = "$(ROOT)"' \
		'build_tools = { keep = true, list = "$(abspath $(GUEST_TOOL_MANIFEST))" }' \
		'' \
		'[packages]' \
		'base = {}' \
		'bootloader = {}' \
		'kernel = {}' \
		'redoxfs = {}' \
		> $@.partial
	mv $@.partial $@

# The manifest and configuration live inside $(GUEST_TOOL_ENV), because that is
# where the tool list `repo build-tools-list` writes and where the installer's
# `list` names the manifest: a run must find both at the same paths it recorded
# when it built the stamp. The environment image is installed into a staging
# directory and moved in afterwards; sweeping $(GUEST_TOOL_ENV) would take the
# stamp's own prerequisites with it and make every build look stale.
$(GUEST_TOOL_STAMP): $(GUEST_TOOL_CONFIG) $(GUEST_TOOL_MANIFEST) $(FSTOOLS)
ifeq ($(FSTOOLS_IN_PODMAN),1)
	$(PODMAN_RUN) make $@
else
	rm -rf $(GUEST_TOOL_ENV_PARTIAL)
	mkdir -p $(GUEST_TOOL_ENV_PARTIAL)
	cp $(GUEST_TOOL_CONFIG) $(GUEST_TOOL_ENV_PARTIAL)/build-tools.toml
	# The tools are the cookbook's own packages, so they are cooked before the
	# environment installs them; the environment is a client of the repository,
	# not a second way to build them.
	$(REPO_BIN) cook $$(cat $(GUEST_TOOL_MANIFEST)) $(COOKBOOK_OPTS)
	$(HOST_CARGO) run --quiet --locked -p ferric-build -- sparse-file \
		$(GUEST_TOOL_IMAGE_PARTIAL) --mib $(GUEST_TOOL_SIZE_MIB)
	# The environment is a scratch image, not the image being built: it takes
	# the packages' build tools and none of the image's fox/SBOM finishing.
	# The configuration names the cookbook absolutely: the installer sizes the
	# image before installing, and a relative --cookbook does not survive that.
	$(INSTALLER) --config-name=$(CONFIG_NAME) \
		$(if $(filter 1,$(FSTOOLS_NO_MOUNT)),--no-mount) \
		--config=$(GUEST_TOOL_ENV_PARTIAL)/build-tools.toml \
		$(GUEST_TOOL_IMAGE_PARTIAL)
	# Only the image is the installation's output. The manifest and the
	# configuration stay where the stamp expects them, so the next run finds
	# them newer than their own prerequisites and older than the stamp.
	mv $(GUEST_TOOL_IMAGE_PARTIAL) $(GUEST_TOOL_IMAGE)
	rm -rf $(GUEST_TOOL_ENV_PARTIAL)
	touch $@
endif

build_tools_clean: FORCE
	rm -rf $(GUEST_TOOL_ENV) $(GUEST_TOOL_ENV_PARTIAL) $(GUEST_TOOL_STAMP)

fstools_clean: FORCE
	rm -rf target
	rm -rf $(FSTOOLS)
	rm -rf $(FSTOOLS)-target
	rm -f $(FSTOOLS_TAG)
	rm -rf $(GUEST_TOOL_ENV) $(GUEST_TOOL_ENV_PARTIAL) $(GUEST_TOOL_STAMP)
