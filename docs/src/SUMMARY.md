# The Ferric Oxide Book

[Introduction](./introduction.md)

# Ferric Oxide System

- [Implemented System](./ferric-oxide.md)
  - [Architecture and Components](./ferric-architecture.md)
  - [Build and Configuration](./ferric-build.md)
  - [Memory and CPU Protection](./ferric-memory.md)
  - [Registered Buffers, IOMMUs and Rings](./ferric-io.md)
  - [Kernel Tracing](./ferric-tracing.md)
  - [Kernel Async Executor (design)](./ferric-async.md)
  - [Virtualization (prepared)](./ferric-virt.md)
  - [Package Tooling: fox](./ferric-packaging.md)
  - [WebAssembly Interfaces](./ferric-wasm.md)
  - [Validation and Debugging](./ferric-testing.md)
  - [Hardware Bring-up](./ferric-hardware.md)
- [Roadmap](./ferric-roadmap.md)

# Introduction

- [Introducing Ferric Oxide](./introducing-redox.md)
  - [Our Goals](./our-goals.md)
  - [Our Philosophy](./philosophy.md)
  - [Why Ferric Oxide?](./why-a-new-os.md)
  - [Why Rust?](./why-rust.md)
  - [Use Cases](./redox-use-cases.md)
  - [Comparing Ferric Oxide to Other OSes](./how-redox-compares.md)
  - [Influences](./influences.md)
  - [Hardware Support](./hardware-support.md)
  - [Important Programs](./important-programs.md)
  - [Related Projects](./side-projects.md)

# Architecture and Design

- [System Design](./system-design.md)
  - [Microkernels](./microkernels.md)
  - [Boot Process](./boot-process.md)
  - [The Kernel](./kernel.md)
  - [System Services in User Space](./user-space.md)
  - [Communication](./communication.md)
  - [Memory Management](./memory.md)
  - [Scheduling](./scheduling.md)
  - [Drivers](./drivers.md)
  - [RedoxFS](./redoxfs.md)
  - [Windowing and Graphics](./graphics-windowing.md)
  - [Security](./security.md)
  - [Features](./features.md)
  - [Package Management](./package-management.md)

- [Schemes and Resources](./schemes-resources.md)
  - [Scheme-rooted Paths](./scheme-rooted-paths.md)
  - [Resources](./resources.md)
  - [Schemes](./schemes.md)
  - ["Everything is a File"](./everything-is-a-file.md)
  - [Stitching It All Together](./stitching-it-all-together.md)
  - [Scheme Operation](./scheme-operation.md)
  - [Event Scheme](./event-scheme.md)
  - [An Example](./example.md)

- [Programs and Libraries](./programs-libraries.md)
  - [Components](./components.md)
  - [GUI](./gui.md)
  - [Shell](./shell.md)
  - [System Tools](./system-tools.md)

# Getting Started

- [Getting started](./getting-started.md)
  - [Building Ferric Oxide](./building-redox.md)
  - [Running in a Virtual Machine](./running-vm.md)
  - [Running on Real Hardware](./real-hardware.md)
  - [Installing](./installing.md)
  - [Trying Out the System](./trying-out-redox.md)
  - [Tasks](./tasks.md)
  - [Package Manager](./pkg.md)

# Contributing

- [Contributing](./contributing.md)
  - [Getting in Touch](./chat.md)

- [Best practices and guidelines](./best-practices.md)
  - [Literate programming](./literate-programming.md)
  - [Writing docs correctly](./writing-docs-correctly.md)
  - [Style](./style.md)
  - [Rusting properly](./rusting-properly.md)
  - [Avoiding Panics](./avoiding-panics.md)
  - [Testing](./testing-practices.md)

- [Working on GitHub](./using-redox-gitlab.md)
  - [Repository Structure](./repository-structure.md)
  - [Creating Proper Bug Reports](./creating-proper-bug-reports.md)
  - [Creating Proper Pull Requests](./creating-proper-pull-requests.md)
  - [Filing Issues](./filing-issues.md)

# Building and Developing

- [The Build Process](./build-process.md)
  - [Building with Podman](./podman-build.md)
  - [Self-hosted Development](./self-hosted.md)
  - [From Nothing To Hello World](./nothing-to-hello-world.md)
  - [Configuration Settings](./configuration-settings.md)
  - [Build System Reference](./build-system-reference.md)
  - [Advanced Podman Build](./advanced-podman-build.md)
  - [Advanced Native Build](./advanced-build.md)
  - [Working with ARM64](./aarch64.md)
  - [Troubleshooting the Build](./troubleshooting.md)
  - [What the Build Does](./build-phases.md)

- [Developing for Redox](./developing-for-redox.md)
  - [Developer FAQ](./developer-faq.md)
  - [References](./references.md)
  - [Libraries and APIs](./libraries-apis.md)
  - [Coding and Building](./coding-and-building.md)
  - [Including Programs](./including-programs.md)
  - [Application Porting](./porting-applications.md)
  - [Porting Case Study](./porting-case-study.md)
  - [Continuous Integration](./ci.md)
  - [Performance](./performance.md)
  - [System Call Tracing](./syscall-debug.md)
  - [Quick Workflow](./quick-workflow.md)
  - [Questions and Feedback](./asking-questions.md)
