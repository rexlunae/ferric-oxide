# Self-hosted Development

This page explains how to configure the build system or Redox for development with Redox.

> 📝 **Note:** This section is incomplete, as developers still need to figure out issues within the toolchain.

## Bootstrapping with QEMU

This method is used if you are doing self-hosted development in QEMU from an existing build system outside of Redox OS (e.g., Linux).

The build system is contained in a recipe called `cookbook`. This recipe contains dependencies similar to what is shown in [Native build](./building-redox.md).

Add the `cookbook` recipe to your Redox image and go to the `cookbook` directory:

```sh
make rp.cookbook qemu
```

### Inside Redox OS QEMU

Inside QEMU, open a terminal, then go to the cookbook directory:

```sh
cd cookbook
```

Build the toolchain. This toolchain will be downloaded from the package archive:

```sh
make prefix
```

Build Redox core packages from source:

```sh
make r.sys
```

Build and install your system changes:

```sh
make r.recipe-name
```

```sh
sudo make i.recipe-name
```


Or (if you changed multiple recipes)

```sh
make r.sys
```

```sh
sudo make i.sys
```

Or install all packages defined from config:

```sh
sudo make install
```

If the process is successful, you can reboot the system to load your changes:

```sh
sudo shutdown -r
```

## Bootstrapping from Real Hardware

You can follow the [Native Build](./building-redox.md#bootstrap-prerequisites-and-fetch-sources) instructions to start a self-hosted build system.

## Caveats

In this section, we explain existing problems of using self-hosted development and a possible workaround for it.

### No recovery point

Redox OS currently has no mechanism for recovering itself if a build broke the system. Before that happens, it's a good idea to have a backup or prepare a clean image from the daily build.

Having a secondary Linux OS in the same system can ease recovering the system from an unbootable position. See [RedoxFS: Working with Linux Partition](./redoxfs.md#working-with-linux-partition) for more details.

### No GCC "Cross compiler" mode

GCC that's built for Linux is configured as "Cross compiler" mode, whereas GCC that's built for Redox is configured as "Hosted compiler". The difference between these two modes affects which `relibc` variant is used during compilation, and in this case the GCC uses the `relibc` from the system installed at `/usr` to do the compilation, instead of the expected place in the `prefix` directory.

There is no way to change the mode to "Cross compiler" without rebuilding, so the easiest way to fix it is by using Clang as the default compiler. Add this config to `.config` to change that:

```sh
REDOXER_USE_CLANG=1
```

