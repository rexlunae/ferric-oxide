# Components

Ferric Oxide is made up of several discrete components. The core ones are in
this repository; [Architecture and component boundaries](./ferric-architecture.md)
says where each one lives.

## Core

- bootloader - Kernel bootstrap
- kernel - System manager
- bootstrap - User-space bootstrap
- init
- initfs
- drivers - Device drivers
- redoxfs - Filesystem
- audiod - Audio daemon
- netstack - TCP/UDP stack
- ps2d - PS/2 driver
- relibc - C library
- randd
- zerod
- ion - Terminal shell (recipe)
- fox - Package manager
- orbital - Desktop environment (recipe)

## Orbital

- orblogin - Login manager
- launcher - App panel
- background - Wallpaper program
- viewer - Image viewer
- calculator - Math program
- COSMIC Files - File manager
- COSMIC Editor - Text editor
- COSMIC Terminal - Terminal emulator
- COSMIC Reader - Document viewer
