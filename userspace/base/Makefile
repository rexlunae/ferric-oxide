TARGET ?= x86_64-unknown-redox
LINKER ?= $(shell redoxer env which $(shell redoxer env printenv LD))
BUILD_TYPE ?= release
BUILD_FLAGS ?= --release
CARGO ?= redoxer
CARGO_HOST ?= env -u CARGO -u RUSTFLAGS cargo

SRC_DIR ?= $(CURDIR)
BUILD_DIR ?= $(shell pwd)/target/$(TARGET)/build
DESTDIR ?= ./sysroot
SYSROOT ?= $(shell pwd)/target/$(TARGET)/sysroot
TARGET_DIR = $(BUILD_DIR)/$(TARGET)/$(BUILD_TYPE)
BUILD_FLAGS +=  --target-dir $(BUILD_DIR)

INITFS_BINS = ipcd init logd ramfs randd zerod \
	acpid fbbootlogd hwd inputd lived \
	pcid pcid-spawner rtcd vesad
INITFS_DRIVERS_BINS = nvmed virtio-blkd  virtio-gpud
BASE_BINS = fbcond getty inputd pcid pcid-spawner redoxerd audiod dhcpd ptyd netstack nbdd
DRIVERS_BINS = e1000d ihdad ihdgd ixgbed rtl8139d rtl8168d \
	usbctl usbhidd usbhubd usbscsid virtio-netd xhcid

ifneq (,$(filter i586-unknown-redox i686-unknown-redox x86_64-unknown-redox,$(TARGET)))
    INITFS_BINS += ps2d
    INITFS_DRIVERS_BINS += ahcid ided
    DRIVERS_BINS += ac97d sb16d vboxd
endif

ifeq ($(TARGET),aarch64-unknown-redox)
	INITFS_BINS += bcm2835-sdhcid
endif

# For testing/benchmarking only
# BASE_BINS += drawbench

INITFS_CARGO_ARGS = $(foreach bin,$(INITFS_BINS),-p $(bin))
# Ferric Oxide: Cargo features of the initfs daemons and drivers from ferric.toml (such as
# nvmed/grant-dma), passed in the environment. Unset, the build is upstream's.
FERRIC_INITFS_FEATURES ?=
FERRIC_INITFS_FEATURE_ARGS = $(if $(strip $(FERRIC_INITFS_FEATURES)),--features "$(strip $(FERRIC_INITFS_FEATURES))")
# Ferric Oxide: Cargo features of the other daemons and drivers from ferric.toml (such as
# e1000d/bypass), passed in the environment. Unset, the build is upstream's.
FERRIC_BASE_FEATURES ?=
FERRIC_BASE_FEATURE_ARGS = $(if $(strip $(FERRIC_BASE_FEATURES)),--features "$(strip $(FERRIC_BASE_FEATURES))")
# Ferric Oxide: the kernel trace configuration bootstrap applies (ferric-config's
# kernel.trace_initfs_* options write it), passed in the environment. Unset, there is none.
FERRIC_INITFS_TRACE ?=
INITFS_DRIVERS_CARGO_ARGS = $(foreach bin,$(INITFS_DRIVERS_BINS),-p $(bin))
BASE_CARGO_ARGS = $(foreach bin,$(BASE_BINS),-p $(bin))
DRIVERS_CARGO_ARGS = $(foreach bin,$(DRIVERS_BINS),-p $(bin))

.PHONY: all base install install-base test

all: base
install: install-base

clean:
	rm -rf $(SRC_DIR)/target $(SRC_DIR)/sysroot $(SYSROOT) $(TARGET_DIR)

# test if booting
test: all
	$(MAKE) install
	REDOXER_SYSROOT=$(DESTDIR) redoxer exec true

# test with interactive gui
test-gui: all
	$(MAKE) install
	REDOXER_SYSROOT=$(DESTDIR) redoxer exec --gui ion

clippy:
	redoxer clippy
	cd bootstrap && redoxer clippy

# -----------------------------------------------------------------------------
# base
# -----------------------------------------------------------------------------
$(SYSROOT)/bin/redoxfs:
	REDOXER_SYSROOT=$(SYSROOT) redoxer pkg redoxfs

base:
	@mkdir -pv "$(BUILD_DIR)"
# Build daemons and drivers
	CARGO_PROFILE_RELEASE_OPT_LEVEL=s CARGO_PROFILE_RELEASE_PANIC=abort \
		$(CARGO) build $(BUILD_FLAGS) \
		--manifest-path "$(SRC_DIR)/Cargo.toml" \
		$(BASE_CARGO_ARGS) $(DRIVERS_CARGO_ARGS) $(FERRIC_BASE_FEATURE_ARGS)
# Build initfs daemons and drivers
# FIXME fix whatever issue (feature unification?) causes most logs to be omitted
# if this is merged with the above build command.
	CARGO_PROFILE_RELEASE_OPT_LEVEL=s CARGO_PROFILE_RELEASE_PANIC=abort \
		$(CARGO) build $(BUILD_FLAGS) \
		--manifest-path "$(SRC_DIR)/Cargo.toml" \
		$(INITFS_CARGO_ARGS) $(INITFS_DRIVERS_CARGO_ARGS) $(FERRIC_INITFS_FEATURE_ARGS)
# Build bootstrap
	cd "$(SRC_DIR)/bootstrap" && RUSTFLAGS= $(CARGO) rustc $(BUILD_FLAGS) \
		-- -Ctarget-feature=+crt-static -Clinker="$(LINKER)"

install-base: base $(SYSROOT)/bin/redoxfs
	@mkdir -pv "$(DESTDIR)/usr/bin" "$(DESTDIR)/usr/lib/drivers"
# Distribute binaries
	@for bin in $(BASE_BINS); do \
		cp -v "$(TARGET_DIR)/$$bin" "$(DESTDIR)/usr/bin"; \
	done
	@for bin in $(DRIVERS_BINS); do \
		cp -v "$(TARGET_DIR)/$$bin" "$(DESTDIR)/usr/lib/drivers"; \
	done
# Copy configurations
	@mkdir -pv "$(DESTDIR)/usr/lib/init.d/" "$(DESTDIR)/usr/lib/pcid.d/" "$(DESTDIR)/usr/lib/xhcid.d/"
	@cp -v "$(SRC_DIR)/init.d"/* "$(DESTDIR)/usr/lib/init.d/"
	@find "$(SRC_DIR)/drivers" -maxdepth 3 -type f -name 'config.toml' | while read -r conf; do \
		driver=$$(basename "$$(dirname "$$conf")"); \
		cp -v "$$conf" "$(DESTDIR)/usr/lib/pcid.d/$$driver.toml"; \
	done
	@cp -v "$(SRC_DIR)/drivers/usb/xhcid/drivers.toml" "$(DESTDIR)/usr/lib/xhcid.d/"

	rm -rf "$(BUILD_DIR)/initfs"
# Distribute initfs binaries
	@mkdir -pv "$(BUILD_DIR)/initfs/bin" "$(BUILD_DIR)/initfs/lib/drivers"
	for bin in $(INITFS_BINS); do \
		cp -v "$(TARGET_DIR)/$$bin" "$(BUILD_DIR)/initfs/bin"; \
	done
	for bin in $(INITFS_DRIVERS_BINS); do \
		cp -v "$(TARGET_DIR)/$$bin" "$(BUILD_DIR)/initfs/lib/drivers"; \
	done
	cp "$(SYSROOT)/bin/redoxfs" "$(BUILD_DIR)/initfs/bin"
# Copy initfs config files
	@mkdir -p "$(BUILD_DIR)/initfs/lib/init.d" "$(BUILD_DIR)/initfs/lib/pcid.d"
	cp "$(SRC_DIR)/init.initfs.d"/* "$(BUILD_DIR)/initfs/lib/init.d/"
	cp "$(SRC_DIR)/drivers/initfs.toml" "$(BUILD_DIR)/initfs/lib/pcid.d/initfs.toml"
	$(if $(strip $(FERRIC_INITFS_TRACE)),cp "$(FERRIC_INITFS_TRACE)" "$(BUILD_DIR)/initfs/lib/trace.toml")
# Build initfs
	$(CARGO_HOST) run --manifest-path "$(SRC_DIR)/initfs/tools/Cargo.toml" --bin redox-initfs-ar -- \
		"$(BUILD_DIR)/initfs" "$(TARGET_DIR)/bootstrap" -o "$(BUILD_DIR)/initfs.img"
# Distribute initfs
	@mkdir -pv "$(DESTDIR)/usr/lib/boot"
	cp -v "$(BUILD_DIR)/initfs.img" "$(DESTDIR)/usr/lib/boot/initfs"

# Device file symlinks
	@mkdir -pv "$(DESTDIR)/dev"
	ln -sf /scheme/null $(DESTDIR)/dev/null
	ln -sf /scheme/pty/ptmx $(DESTDIR)/dev/ptmx
	ln -sf /scheme/rand $(DESTDIR)/dev/random
	ln -sf /scheme/rand $(DESTDIR)/dev/urandom
	ln -sf /scheme/zero $(DESTDIR)/dev/zero
	ln -sf /scheme/libc/tty $(DESTDIR)/dev/tty
	ln -sf /scheme/libc/stdin $(DESTDIR)/dev/stdin
	ln -sf /scheme/libc/stdout $(DESTDIR)/dev/stdout
	ln -sf /scheme/libc/stderr $(DESTDIR)/dev/stderr
