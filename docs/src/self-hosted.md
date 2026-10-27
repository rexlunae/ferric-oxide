# Self-hosted Development

> ⚠️ **Warning:** This page comes from the Redox OS book and has not been
> revised for Ferric Oxide yet. Its commands, paths and hardware details may
> not match this repository; see [Implemented System](./ferric-oxide.md).

This page explain how to configure the build system or Redox for development from Redox.

## QEMU in Linux

This method is used if you are doing self-hosted development in QEMU from Linux.

- Add the `cookbook` recipe to your image and go to the `cookbook` directory:

```
make rp.cookbook qemu
```

```
cd cookbook
```

- Build Redox from source

```
make prefix r.sys
```

- Build and install your system changes

```
make ri.recipe-name
```

Or (if you changed multiple recipes)

```
make ri.sys
```

If the process is successful you can reboot the system to load your changes:

```
sudo shutdown -r
```

## Real Hardware

TODO
