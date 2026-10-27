# Installing

The installer can install the system onto a disk from a running live image, or from Linux. **Please back up your system** before attempting to install. You can't install onto a USB device or use a USB device for the root file system yet, but you can install from one.

> 📝 **Note:** This page describes the installer inherited from Redox OS. The command-line installer is built from `tools/installer/` in this repository; the GUI installer is still built from Redox's installer source. Installation has not yet been validated on real hardware for Ferric Oxide; see [Running on Real Hardware](./real-hardware.md).

## Starting the Installer from a Live Image

Desktop images usually include the installer. After starting the live image from a USB device, log in as the user named `user` with an empty password, open the app menu from the Orbital bottom bar, then open the "System" app category and click on the installer.

Or, if you want to launch it from the terminal, run the following command:

```sh
sudo redox_installer_gui
```

If you are using the `server` variant or want to use the TUI interface, open a terminal window and type:

```sh
sudo redox_installer_tui
```

At the moment, the GUI installer is more capable than the TUI version, but the TUI one will be updated.

In the TUI version, if the system recognizes your device, it will prompt you to select a device to install on. Choose carefully, as it will erase all the data on that device. Note that if your device is not recognized, it may offer you the option to install on `disk/live` (the in-memory *livedisk*). Don't do this, as it will crash the system.

The TUI version also prompts for a `redoxfs password`. This is for an encrypted filesystem. Leave the password empty and press Enter if you don't need filesystem encryption.

## Starting the Installer from Linux

From Linux, the command-line installer in this repository can write a configuration's packages to a disk or image file. Build it from the repository root:

```sh
cargo build --release -p redox_installer --features installer
```

The GUI installer is not part of this repository yet. Its recipe (`installer-gui`) builds it from Redox's installer source for the target system. It uses `libcosmic`, so on Linux it only works under Wayland.

Depending on where you install, you may need to run the installer with `sudo`.

### GUI Installer Options

The GUI installer is capable of many installation modes. This section explains each step option and its implications in detail:

#### 1. Where do you want to install Ferric Oxide?

This section asks about the general scenario where you want to install Ferric Oxide:

- **Whole Drive**

  Use the whole storage device for Ferric Oxide. It will erase all data on that device. The next section is [#2](#2-choose-a-drive).

- **Specific Partition**

  Use a storage device partition for Ferric Oxide. It will erase all data in that partition, but leave every other partition intact. It is the option that you should use if you want to dual-boot with your current Linux installation. The next section is [#3](#3-choose-a-main-partition).

- **Image File**

  Write to a new image file. This option will create an image disk file similar to the one that's hosted on the Ferric Oxide download page. The next section is [#4](#4-configure-image-file).


#### 2. Choose a drive

This section lets you choose which storage device you want to install into. When chosen, the disk will be erased, then formatted as GPT with three partitions:

- First, a BIOS boot partition for boot using legacy BIOS.
- Second, a 2MB FAT filesystem for UEFI bootloader.
- Last, a RedoxFS partition that occupies the rest of the disk.

This layout is not configurable. If you wish to do so, format your disk using `GParted` or a similar tool, then go back and choose **Specific Partition**.

*Never choose the disk that's currently active*.

#### 3. Choose a main partition

This section lets you choose which partition you want to use as the main RedoxFS partition for Ferric Oxide. This will erase the data for that partition but leave everything else untouched.

The installer is not capable of doing partitioning. If you want to do so, use another tool such as `GParted` and set up an empty partition for RedoxFS, then click **Reload** on this page after you're done partitioning.

Note that installing just for RedoxFS partition will not make it bootable; hence, there's a second option to choose where to install the bootloader:

- **Do not write**

  This will not write any bootloader and leave the installed Ferric Oxide in that partition unbootable, because Ferric Oxide requires a custom bootloader and cannot be loaded by common bootloaders such as `GRUB`, `systemd-boot`, etc.

- **Dual-boot (systemd-boot)**

  If `/boot/efi` is detected to contain `systemd-boot`, this option will be visible. This option is not visible for Linux that boots using other than `systemd-boot` such as `GRUB`.

When chosen, it will write the UEFI bootloader to these paths:

- `/boot/efi/EFI/redox/*.efi` : The bootloader EFI file
- `/boot/efi/loader/entries/redox.conf` : The bootloader entry in `systemd-boot menu

In some Linux distributions, the `systemd-boot` menu is not visible, and to make it appear, the `timeout` option in `/boot/efi/loader/loader.conf` file. See [systemd-boot loader.conf man page](https://man.archlinux.org/man/loader.conf.5#OPTIONS) for more information. 

If you choose any partition, make sure it's not mounted. **Never choose the partition that's currently mounted**.

- **Any partition in /dev/\***

This option will format the partition with the FAT filesystem containing UEFI bootloader for Ferric Oxide. The bootloader does not have to be on the same disk as RedoxFS, as the bootloader will look to all drives that contain a RedoxFS partition with the help of UEFI services.

This option is meant for installation to an external storage device, or for chainloading to any bootloader that supports it, such as `GRUB`. In some machines, the bootloader can also be directly bootable from BIOS boot device menu or boot priority on firmware setup.

Note that this option only installs the UEFI bootloader. If your machine only supports booting via BIOS, then your only option is to go back and choose the **Whole Drive** option.

---

If you do not see the `Next` button, try to maximize the window.

#### 4. Configure Image File

This section contain options for installing Ferric Oxide into a image file. This section is particularly useful for emulating the system in a virtual machine, or for advanced installation methods later on.

- **Image Path**

This asks where to install the image file. In Linux, the `Browse` button is visible to open a file dialog where the file will be saved.

You can also specify a path to a directory, and it will just install Ferric Oxide files in that directory, but this behavior is not recommended to be used.

- **Image Size**

Specify the image file size, which will also be the size of the main RedoxFS partition for Ferric Oxide.

- **Skip Partition?**

Checking this box will prevent the image file from being formatted as GPT device; rather, it will become a raw RedoxFS partition (written directly to storage device sectors). This is useful if you wanted only the RedoxFS format, which can then be used for `dd` into `/dev/partition` for example, or boot via the `-cdrom` option in `QEMU`.

- **Write Bootloader EFI**

This asks where to install the bootloader file. The EFI file is needed to boot the Ferric Oxide disk later on. It is a PE executable that is usually placed in `/EFI/BOOT/*.EFI` of a boot partition.


#### 5. Select System Profile

After entering options from either Section 2, 3, or 4, this is the last section before the actual install is performed.

- **Desktop**

  This option chooses the Ferric Oxide to be installed with desktop and server components. The installation usually takes around 3 minutes (200 seconds).

- **Server**

  This option chooses the Ferric Oxide to be installed with only server components. Its installation usually takes around 1 minute and half (90 seconds).

- **Clone**

  This option is only visible if you boot Ferric Oxide to a live disk. It will install an identical copy from the current Ferric Oxide live disk memory.

- **Install as Live Disk**

This option will make the bootloader install with live disk mode being chosen as the default.

- **Enable disk encryption password**

You can enter a password for main RedoxFS partition encryption. It is whole-disk encryption; the bootloader will ask for the password at boot.


#### 6. Installation Progress

The installation is then performed and usually will finish within minutes. Unfortunately, for the GUI installer, the progress is in the terminal where it was launched, instead of being reported in the GUI.

Once the installation completes, you can restart your computer to boot into Ferric Oxide!
