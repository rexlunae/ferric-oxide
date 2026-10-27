# Rusting Properly

Some general guidelines. The project-wide rules, including module structure,
dependencies and documentation, are in
[AGENTS.md](https://github.com/rexlunae/ferric-oxide/blob/main/AGENTS.md#rust-conventions).

* Use `std::mem::replace`, `std::mem::take` and `std::mem::swap` when you can.
* Prefer passing references to the data over owned data. (Don't take `String`, take `&str`. Don't take `Vec<T>`, take `&[T]`.)
* Use generics, traits, and other abstractions Rust provides.
* Avoid lossy conversions. Use `From`/`TryFrom` rather than `as` (for example, `u16::try_from(my_u32)` rather than `my_u32 as u16`), and widen rather than narrow when comparing.
* Use fixed-size integers for data formats and ABIs, and `usize` for sizes and indexes of in-memory data.
* Follow the usual idioms of programming, such as "composition over inheritance", "let your program be divided in smaller pieces", and "resource acquisition is initialization".
* When `unsafe` is unnecessary, don't use it. 10 lines longer safe code is better than more compact unsafe code! When you do need it, keep the `unsafe` block small and write a `// SAFETY:` comment explaining why it is sound.
* Mark parts that need work with `TODO`, `FIXME` or `UNOPTIMIZED`, and say what is missing.
* Use the compiler hint attributes, such as `#[inline]`, `#[cold]`, etc. when it makes sense to do so.
* Return typed errors (an `enum` implementing `Error` and `Display`) rather than strings; strings are for display only.
* Try to banish `unwrap()` and `expect()` from your code in order to manage errors properly. Panicking must indicate a bug in the program (not an error you didn't want to manage). If you cannot recover from an error, print a nice error to stderr and exit. See the Rust book on [error handling](https://doc.rust-lang.org/book/ch09-00-error-handling.html).
* Write library code `no_std`-first where it doesn't need the operating system, so the kernel, bootloader and host tools can share it.
