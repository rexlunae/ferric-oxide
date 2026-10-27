# From Nothing To Hello World

This page shows the quickest way to get your own program running in the guest.

> ⚠️ **Warning:** These steps come from the Redox OS book and have only been partly updated for Ferric Oxide's native build. Step 9 uses `make rp.*`, which builds the recipe and copies it into the existing image through FUSE, so it needs FUSE (or the [Podman build](./podman-build.md)). To include a program in every build instead, add it to `image.packages_add` in `ferric.toml` (see [Including Programs](./including-programs.md)). They have not been re-verified on the native build.

In this example we will use a "Hello World" program written in Rust.

 1. Get the source and set up the toolchain (see [Building Ferric Oxide](./building-redox.md) for the prerequisites).

    ```sh
    git clone https://github.com/rexlunae/ferric-oxide ~/ferric-oxide
    cd ~/ferric-oxide
    cargo xtask setup
    ```

 2. Build the image once, so there is an image to add the program to.

    ```sh
    cargo xtask build
    ```

 3. Create the `hello-world` recipe folder.

    ```sh
    mkdir recipes/other/hello-world
    ```

 4. Create the `source` folder for the recipe.

    ```sh
    mkdir recipes/other/hello-world/source
    ```

 5. Navigate to the recipe's `source` folder.

    ```sh
    cd recipes/other/hello-world/source
    ```

 6. Initialize a Cargo project with the "Hello World" string.

    ```sh
    cargo init --name="hello-world"
    ```

 7. Create the `hello-world` recipe configuration.

    ```sh
    cd ~/ferric-oxide
    ```

    ```sh
    nano recipes/other/hello-world/recipe.toml
    ```

 8. Add the following to the recipe configuration:

    ```toml
    [build]
    template = "cargo"
    ```

 9. Build the Hello World program and add it to the image.

    ```sh
    make prefix rp.hello-world
    ```

10. Start the virtual machine without a GUI.

    ```sh
    cargo xtask run --no-build --headless
    ```

11. At the login prompt, write "user" for the user name and press Enter.

12. Run the "Hello World" program.

    ```sh
    hello-world
    ```

13. Shut down the virtual machine.

    ```sh
    sudo shutdown
    ```
