# Writing Documentation Correctly

Documentation appears in two places:

- In the source code, as `rustdoc` comments.
- In this book, under `docs/src/`.

## Rustdoc

Every public item gets a `rustdoc` comment: what it does, its errors, and the
invariants a caller must uphold. Format and ABI crates also document the
layout they define and the specification it comes from. Several crates
enforce this with `#![deny(missing_docs)]`. Run `cargo doc` on what you change
to check the result.

## The book

The book covers how the system fits together, how to build and use it, and
why it is designed the way it is. It does not repeat API documentation; link
to the code and its Rustdoc instead.

- **Update the book in the same pull request as the code.** A change to
  behavior, architecture, a public interface, a feature default, a build
  command, a package format or a validation requirement updates the relevant
  chapter. The [Implemented System](./ferric-oxide.md) chapters and their
  status table come first.
- **Say what is true today.** Distinguish *implemented* (code and integration
  exist), *prepared* (interfaces exist but nothing uses them yet) and *planned*
  (on the [Roadmap](./ferric-roadmap.md) only). Say when a feature is off by
  default. Don't claim a test, boot, benchmark or hardware result you didn't
  observe.
- **Prefer links to copies.** Link to `features.toml`, workflows and source
  rather than copying lists that will drift.
- **Use the project's names.** The system is Ferric Oxide. Name Redox OS only
  to credit it or to name an internal component that keeps its Redox name
  (`relibc`, `RedoxFS`, the `redox` target triple).
- **Mark pages that aren't revised yet.** The book was imported from the
  Redox book. A chapter that still describes Redox and hasn't been checked
  against this repository starts with this notice:

  ```markdown
  > ⚠️ **Warning:** This page comes from the Redox OS book and has not been
  > revised for Ferric Oxide yet. Its commands, paths and hardware details may
  > not match this repository; see [Implemented System](./ferric-oxide.md).
  ```

  Remove the notice when you revise the page.

New pages go in `docs/src/SUMMARY.md`. Before submitting, run the
[documentation checks](./ferric-testing.md#documentation-checks): `./unused_pages.sh`
and `mdbook build` from `docs/`.
