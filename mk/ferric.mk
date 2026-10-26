# Feature configuration: make menuconfig, ferric.toml and build/ferric/
# (see tools/ferric-config/README.md)

# Interactive editor; creates ferric.toml on first save
menuconfig: FORCE
	$(FERRIC_CONFIG_TOOL) menu

# Validate ferric.toml without building
config-check: FORCE
	$(FERRIC_CONFIG_TOOL) check $(FERRIC_TARGET_OVERRIDES)

# Print every option, its value and where the value came from
config-list: FORCE
	@$(FERRIC_CONFIG_TOOL) list

# Regenerate build/ferric/ now
config-generate: FORCE
	$(FERRIC_CONFIG_TOOL) generate $(FERRIC_TARGET_OVERRIDES)
