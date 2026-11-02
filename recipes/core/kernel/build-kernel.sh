# Ferric Oxide: the kernel build shared by recipes/core/kernel and recipes/core/kernel-trace.
# A recipe's build script sources this file and calls ferric_kernel_build.
#
# ferric_kernel_build NAME [FEATURES]
#   NAME      the file name in usr/lib/boot (kernel, kernel-trace); NAME.all and NAME.sym go
#             beside it, and the TLSF notices go in usr/share/licenses/NAME
#   FEATURES  the kernel's Cargo features, instead of its default ones; without the argument
#             the defaults are built, as upstream
#
# FERRIC_KERNEL_LINK_ARGS and FERRIC_KERNEL_CONFIG from ferric.toml apply to every kernel.
#
# The cookbook marks a recipe stale only when its recipe.toml or its source changes, so after
# editing this file run `make cr.kernel` (and `make cr.kernel-trace`).

ferric_kernel_build() {
    local name="$1"
    local dep crate
    # The kernel uses the shared crates beside it in crates/ as ../<name> path
    # dependencies. The cookbook builds a copy of crates/kernel/ in the recipe's
    # source/, so give that copy the same neighbours.
    for dep in "${COOKBOOK_ROOT}"/crates/*/
    do
        crate="$(basename "${dep}")"
        [ "${crate}" = kernel ] || ln -sfn "${dep%/}" "${COOKBOOK_SOURCE}/../${crate}"
    done
    cd "${COOKBOOK_SOURCE}" || return 1
    local make_args=("KERNEL_INSTALL_NAME=${name}")
    local features=upstream
    if [ "$#" -ge 2 ]
    then
        features="$2"
        # The kernel Makefile splices this into --features=..., so the flag rides along.
        make_args+=("KERNEL_CARGO_FEATURES=${features} --no-default-features")
    fi
    # Extra linker arguments, such as --emit-relocs for kernel image KASLR.
    if [ -n "${FERRIC_KERNEL_LINK_ARGS}" ]
    then
        local link_args="" arg
        for arg in ${FERRIC_KERNEL_LINK_ARGS}
        do
            link_args+=" -C link-arg=${arg}"
        done
        make_args+=("KERNEL_LINK_ARGS=${link_args}")
    fi
    if [ -n "${FERRIC_KERNEL_CONFIG}" ] && ! cmp -s "${FERRIC_KERNEL_CONFIG}" config.toml
    then
        cp -v "${FERRIC_KERNEL_CONFIG}" config.toml
    fi
    # The kernel Makefile only tracks *.rs, so relink when features or config change.
    local stamp
    stamp="$(printf '%s\n' "${features}" "${FERRIC_KERNEL_LINK_ARGS}"; cat config.toml 2>/dev/null || true)"
    if [ "$(cat "${COOKBOOK_BUILD}/ferric.stamp" 2>/dev/null)" != "${stamp}" ]
    then
        rm -f "${COOKBOOK_BUILD}/kernel.all"
    fi
    make install BUILD="${COOKBOOK_BUILD}" DESTDIR="${COOKBOOK_STAGE}" "${make_args[@]}" || return 1
    # Keep the MIT notices of the TLSF runtime dependencies in binary images.
    mkdir -p "${COOKBOOK_STAGE}/usr/share/licenses/${name}"
    cp -v "${COOKBOOK_SOURCE}"/licenses/*.txt "${COOKBOOK_STAGE}/usr/share/licenses/${name}/"
    printf '%s' "${stamp}" > "${COOKBOOK_BUILD}/ferric.stamp"
}
