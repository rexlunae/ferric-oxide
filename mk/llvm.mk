# Native toolchain: host Clang, rustup's Rust/LLVM tools and target runtime data.
# Keep artifacts separate from the inherited GCC/LLVM compiler distribution.
PREFIX=prefix/llvm/$(TARGET)
PREFIX_INSTALL=$(PREFIX)/sysroot/
PREFIX_PATH=$(ROOT)/$(PREFIX_INSTALL)/bin
export RUSTUP_TOOLCHAIN=$(ROOT)/$(PREFIX_INSTALL)
export REDOXER_TOOLCHAIN=$(RUSTUP_TOOLCHAIN)
export RUST_COMPILER_RT_ROOT=$(RUSTUP_TOOLCHAIN)/lib/rustlib/src/rust/library/compiler-builtins/compiler-builtins
export REDOXER_USE_CLANG=1
export FERRIC_LLVM_TOOLCHAIN=1
export FERRIC_NATIVE_SYSROOT_STAMP=$(ROOT)/$(PREFIX)/ready
export FERRIC_UEFI_ONLY=1
export PREFIX_RUSTFLAGS=-L $(ROOT)/$(PREFIX_INSTALL)/$(TARGET)/lib
# autoreconf must use Redox's libtoolize from the sysroot, which matches the
# libtool macros there; Homebrew's autoreconf otherwise runs glibtoolize.
export LIBTOOLIZE=$(PREFIX_PATH)/libtoolize

.PHONY: prefix prefix_clean
prefix: $(PREFIX)/ready
prefix_clean:
	rm -rf $(PREFIX)

$(PREFIX)/sysroot: $(PREFIX)/ready

# Track the complete installation inputs, including headers, Cargo/build config,
# submodules and relibc's shared path dependencies. Directory mtimes also catch
# removals. Keep generated outputs and Git internals outside the dependency set.
RELIBC_SOURCE_INPUTS:=$(shell find crates/relibc crates/abi crates/hwrand crates/procinfo crates/kernel/syscall \
	\( -name .git -o -name target -o -path crates/relibc/build \
	-o -path crates/relibc/sysroot -o -path crates/relibc/prefix \) -prune \
	-o \( -type f -o -type d -o -type l \) -print)
# Recipe shortcuts such as debug.% must not be applied to source prerequisites.
$(RELIBC_SOURCE_INPUTS): ;
$(PREFIX)/ready: $(RELIBC_SOURCE_INPUTS)
$(PREFIX)/ready: scripts/native-toolchain.sh scripts/native/runtime.ld rust-toolchain.toml recipes/core/relibc/recipe.toml | $(FSTOOLS_TAG)
	bash scripts/native-toolchain.sh "$(ROOT)/$(PREFIX)" "$(TARGET)"
	printf '[packages]\nrelibc = "source"\n' > "$(PREFIX)/relibc.toml"
	$(REPO_BIN) clean-target relibc --filesystem="$(PREFIX)/relibc.toml"
	export PATH="$(PREFIX_PATH):$$PATH" && \
	COOKBOOK_CLEAN_BUILD=true COOKBOOK_CLEAN_TARGET=false COOKBOOK_VERBOSE=true \
	$(REPO_BIN) cook relibc --filesystem="$(PREFIX)/relibc.toml"
	cp -R "recipes/core/relibc/target/$(TARGET)/stage/usr/." "$(PREFIX)/sysroot/$(TARGET)/"
	touch "$@"
