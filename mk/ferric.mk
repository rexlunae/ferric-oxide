# Feature configuration: make menuconfig, ferric.toml and build/ferric/
# (see tools/ferric-config/README.md)

FERRIC_CONFIG_TOOL=env -u RUSTUP_TOOLCHAIN -u CC -u TARGET -u RUSTFLAGS cargo run --quiet --release \
	--manifest-path tools/ferric-config/Cargo.toml -- --config $(FERRIC_CONFIG) --out $(FERRIC_OUT)

# Regenerated whenever ferric.toml or the catalog changes; make then restarts
# with the new values.
ifneq ($(wildcard $(FERRIC_CONFIG)),)
$(FERRIC_OUT)/config.mk: $(FERRIC_CONFIG) features.toml $(wildcard tools/ferric-config/src/*.rs)
	$(FERRIC_CONFIG_TOOL) generate
endif

# Interactive editor; creates ferric.toml on first save
menuconfig: FORCE
	$(FERRIC_CONFIG_TOOL) menu

# Validate ferric.toml without building
config-check: FORCE
	$(FERRIC_CONFIG_TOOL) check

# Print every option, its value and where the value came from
config-list: FORCE
	@$(FERRIC_CONFIG_TOOL) list

# Regenerate build/ferric/ now
config-generate: FORCE
	$(FERRIC_CONFIG_TOOL) generate
