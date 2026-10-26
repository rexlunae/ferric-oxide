# Porting Case Study

As a non-trivial example of porting a Rust app, let's look at what was done to port [gitoxide](https://github.com/Byron/gitoxide).
This port was done for Redox before Ferric Oxide forked, so it is now much simpler, but perhaps some of these steps will apply to you.

> 📝 **Note:** The commands below have been adapted to this repository, where
> relibc lives in `crates/relibc/` beside the cookbook instead of in its own
> repository. Error messages and the order of discoveries are from the
> original port.

The goal when porting is to capture all the necessary configuration in recipes and scripts, and to avoid requiring a fork of the program repository or upstreaming changes.
This is not always feasible, but forking/upstreaming should be avoided when it can be.

## Build on Linux

Before we start, we need to build the software for our Linux system and make sure it works.
This is not part of the porting, it's just to make sure our problems are not coming from the Linux version of the software.
We follow the normal build instructions for the software we are porting:

```sh
cd ~
```

```sh
git clone https://github.com/Byron/gitoxide.git
```

```sh
cd gitoxide
```

```sh
cargo run --bin ein
```

## Set up the working tree

Fork [ferric-oxide](https://github.com/rexlunae/ferric-oxide) on GitHub, then
clone the fork and create a branch:

```sh
cd ~
```

```sh
git clone https://github.com/MY_USERNAME/ferric-oxide.git
```

```sh
cd ferric-oxide
```

```sh
git remote add upstream https://github.com/rexlunae/ferric-oxide.git
```

```sh
git checkout -b gitoxide-port
```

## Create a Recipe

To create a recipe, we need to make a new directory in `recipes` with the name the package will have, in this case `gitoxide`, and create a `recipe.toml` file with a first-draft recipe.

```sh
mkdir -p ~/ferric-oxide/recipes/gitoxide
```

```sh
nano ~/ferric-oxide/recipes/gitoxide/recipe.toml
```

Start with the following content in the `recipe.toml` file.

```toml
[source]
git = "https://github.com/Byron/gitoxide"
[build]
template = "cargo"
```

## First Attempt

Next we attempt to build the recipe. Note that the first attempt may require the toolchain to be updated, so we run `make prefix`, which may take quite a while.

```sh
cd ~/ferric-oxide
```

```sh
make prefix
```

```sh
make r.gitoxide |& tee gitoxide.log
```

We get our first round of errors (among other messages):

```
error[E0425]: cannot find value `POLLRDNORM` in crate `libc`
error[E0425]: cannot find value `POLLWRBAND` in crate `libc`
```

## Make a Local Copy of libc

We suspect the problem is that these items have not been defined in the Redox edition of `libc`.
`libc` is not a Redox crate, it is a rust-lang crate, but it has parts that are Redox-specific.
We need to work with a local copy of `libc`, and then later ask someone with authority to upstream the required changes.

First, clone `libc` into our `gitoxide` directory.

```sh
cd ~/ferric-oxide/recipes/gitoxide
```

```sh
git clone https://github.com/rust-lang/libc.git
```

Try to find the missing constants.

```sh
cd ~/ferric-oxide/recipes/gitoxide/libc
```

```sh
grep -nrw "POLLRDNORM" --include "*.rs"
```

```sh
grep -nrw "POLLWRBAND" --include "*.rs"
```

Looks like the value is not defined for the `redox` target in the `libc` crate. Let's see if it's in `relibc`.

```sh
cd ~/ferric-oxide/crates/relibc
```

```sh
grep -nrw "POLLRDNORM" --include "*.rs"
```

```sh
grep -nrw "POLLWRBAND" --include "*.rs"
```

Yes, both are already defined in `relibc`, and after a bit of poking around, it looks like they have an implementation.
They just need to get published in `libc`. Let's do that.

## Make Changes to libc

Let's add our constants to our local `libc`. We are not going to bother with `git` because these changes are just for debugging purposes.
Copy the constant declarations from `relibc`, and paste them in the appropriate sections of `libc/src/unix/redox/mod.rs`.
In addition to copying the constants, we have to change the type `c_short` to `::c_short` to conform to `libc` style.

```sh
cd ~/ferric-oxide/recipes/gitoxide
```

```sh
nano libc/src/unix/redox/mod.rs
```

We add the following lines to `mod.rs`:

```rust
pub const POLLRDNORM: ::c_short = 0x040;
pub const POLLRDBAND: ::c_short = 0x080;
pub const POLLWRNORM: ::c_short = 0x100;
pub const POLLWRBAND: ::c_short = 0x200;
```

In order to test our changes, we will have to modify our `gitoxide` clone for now.
Once the changes to `libc` are upstreamed, we won't need a modified `gitoxide` clone.
To avoid overwriting our work, we want to turn off future fetches of the `gitoxide` source during build, so change `recipe.toml` to comment out the source section: `nano recipe.toml`.

```toml
#[source]
#git = "https://github.com/Byron/gitoxide"
[build]
template = "cargo"
```

We edit `gitoxide`'s `Cargo.toml` so we use our `libc`.

```
nano ~/ferric-oxide/recipes/gitoxide/source/Cargo.toml
```

After the `[dependencies]` section, but before the `[profile]` sections, add the following to `Cargo.toml`:

```toml
[patch.crates-io]
libc = { path = "../libc" }
```

Bump the version number on our `libc`, so it will take priority.

```
nano ~/ferric-oxide/recipes/gitoxide/libc/Cargo.toml
```

```toml
version = "0.2.143"
```

Update `gitoxide`'s `Cargo.lock`.

```sh
cd ~/ferric-oxide/recipes/gitoxide/source
```

```sh
cargo update
```

Make sure we have saved all the files we just edited, and let's try building.

```sh
cd ~/ferric-oxide
```

```sh
make r.gitoxide
```

Our `libc` errors are solved! These changes still need to go into the `libc` crate itself: open a pull request on [rust-lang/libc](https://github.com/rust-lang/libc) to add the constants for the `redox` target.

## Creating a Custom Recipe

In looking at what is included in `gitoxide`, we see that it uses [OpenSSL](https://docs.rs/openssl/latest/openssl/), which has some custom build instructions described in the docs.
There is already a fork of `openssl`, from the Redox project, that adds the `redox` target, so we will set up our environment to use that.

In order to do this, we are going to need a custom recipe. Let's start with a simple custom recipe, just to get us going.
Edit our previously created recipe, `recipes/gitoxide/recipe.toml`, changing it to look like this.

```toml
#[source]
#git = "https://github.com/Byron/gitoxide.git"
[build]
template = "custom"
script = """
printenv
"""
```

In this version of our recipe, we are just going to print the environment variables during `cook`,
so we can see what we might make use of in our custom script.
We are not actually attempting to build `gitoxide`.
Now, when we run `make r.gitoxide` in `~/ferric-oxide`, we see some useful variables such as `TARGET` and `COOKBOOK_ROOT`.

Two key shell functions are provided by the custom script mechanism, `cookbook_cargo` and `cookbook_configure`.
If you need a custom script for building a Rust program, your script should set up the environment, then call `cookbook_cargo`, which calls `cargo` set up for the target.
If you need a custom script for using a `Makefile`, your script should set up the environment, then call `cookbook_configure`.
If you have a custom build process, or you have a patch-and-build script, you can just include that in the `script` section and not use either of the above functions.
If you are interested in looking at the code that runs custom scripts, see `tools/cookbook/src/cook/cook_build.rs`.

Adding a dependency on `openssl` ensures that the build of `openssl` will happen before attempting to build `gitoxide`, so we can trust that the library contents are in the target directory of the ssl package.
And we need to set the environment variables as described in the [OpenSSL bindings](https://docs.rs/openssl/latest/openssl/) crate docs.

Our recipe now looks like this:

```toml
#[source]
#git = "https://github.com/Byron/gitoxide.git"
[build]
dependencies = [
    "openssl",
]
template = "custom"
script = """
export OPENSSL_DIR="${COOKBOOK_SYSROOT}"
export OPENSSL_STATIC="true"
cookbook_cargo
"""
```

## Linker Errors

Now we get to the point where the linker is trying to statically link the program and libraries into the executable. This program, called `ld`, will report errors if there are any undefined functions or missing static variable definitions.

```
undefined reference to `tzset'
undefined reference to `cfmakeraw'
```

In our case we find we are missing `tzset`, which is a timezone function. We are also missing `cfmakeraw` from `termios`. Both of these functions are normally part of `libc`. In our case, they are defined in the `libc` crate, but they are not implemented by our C library, `relibc`. We need to add these functions.

## Add Missing Functions to relibc

Let's modify `relibc`. It is part of this repository, in `crates/relibc/`, so
the changes go on the same `gitoxide-port` branch.

Now we need to make our changes to `relibc`...

After a fair bit of work, which we omit here, the functions `tzset` and `cfmakeraw` are implemented in `relibc`. An important note is that in order to publish the functions, they need to be preceded with:

```
#[no_mangle]
extern "C" fn tzset() ...
```

Now let's rebuild relibc and gitoxide:

```sh
cd ~/ferric-oxide
```

```sh
make crp.relibc,gitoxide
```

## Testing in QEMU

Now we need to build a full image and run it in QEMU. Let's make a configuration file.

```sh
cd ~/ferric-oxide/config
```

```sh
cp desktop.toml my_desktop.toml
```

```sh
nano my_desktop.toml
```

Note that the prefix "my_" at the beginning of the config file name means that it is gitignore'd, so it is preferred that you prefix your config name with "my_".

In `my_desktop.toml`, at the end of the list of packages, after `uutils = {}`, add

```toml
gitoxide = {}
```

Now let's tell `make` about our new config definition, build the system, and test our new command.

```sh
cd ~/ferric-oxide
```

```sh
echo "CONFIG_NAME?=my_desktop" >> .config
```

```sh
make qemu
```

Log in as `user` with no password, and type:

```sh
gix clone https://github.com/rexlunae/ferric-oxide
```

We get some errors, but we are making progress.

## Submitting the changes

- Before committing the new recipe, uncomment the `[source]` section: edit `recipes/gitoxide/recipe.toml` to remove the `#` from the start of the first two lines.
- Commit the recipe and the `relibc` changes on your branch. Rebuild the system and test it in QEMU, checking anything else the `relibc` changes might affect.
- Open a pull request against `rexlunae/ferric-oxide`, following [Creating Proper Pull Requests](./creating-proper-pull-requests.md).
- Changes to the `libc` crate belong in [rust-lang/libc](https://github.com/rust-lang/libc), under its rules for the `redox` target.
- Don't send changes to the Redox project on Ferric Oxide's behalf. Redox does not accept AI-assisted contributions, and Ferric Oxide's history includes AI-assisted work.
