//! The line format of `/scheme/sys/proc`, the kernel's per-process introspection file.
//!
//! Reading the file (as root) gives one block of lines per context, written by the kernel's
//! `sys` scheme with this crate's [`fmt::Display`] implementations and read back by tools
//! with [`Line::parse`] (or `processes`, with the `alloc` feature). One definition serves
//! both sides, so the format cannot drift between them.
//!
//! ```text
//! context 12 pid 9 euid 0 egid 0 status sleeping cpu 1 running no active_ns 5210000 sched_ns 61000 name /usr/bin/ion
//! memory private 1236992 shared 65536 kernel 81920
//! fd 0 scheme pipe number 3
//! fd 4 scheme user:7 number 1
//! grant 0x400000 pages 12 provider allocated prot r-x
//! ```
//!
//! - A [`ContextLine`] starts each context's block: its debug id (the id `sched.switch`
//!   trace records carry), process id, effective ids, scheduling state, CPU and CPU time.
//! - A [`MemoryLine`] follows with the bytes the context's address space holds privately and
//!   shares, and the kernel memory the context itself uses (its kernel stack and FPU state).
//! - An [`FdLine`] for each open file descriptor: its number, the scheme it is open on and
//!   the scheme's handle number. A kernel scheme is named; a user-space scheme has no name the
//!   kernel knows, so it is given by its scheme id.
//! - A [`GrantLine`] for each grant (mapping) of the address space: base address, size in
//!   pages, where the memory comes from and its protection.
//!
//! Each line is space-separated `key value` pairs in a fixed order after the first keyword,
//! ending in `\n`. The context's name comes last and runs to the end of the line; the kernel
//! replaces control characters in it with `?` ([`Name`]), so a name cannot start a line of
//! its own. Contexts whose address space is shared (threads of one process) each list the
//! same grants.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

#[cfg(feature = "alloc")]
extern crate alloc;

use core::fmt;
use core::str::FromStr;

/// The file's name in the `sys` scheme: `/scheme/sys/proc`.
pub const SYS_FILE: &str = "proc";

/// Why a line does not parse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LineError {
    /// The first word is not a known line kind.
    Kind,
    /// The line has fewer or more fields than its kind, or a key is not the expected one.
    Fields,
    /// A field that should be a number is not one.
    Number,
    /// The status is not a [`Status`] name.
    Status,
    /// The provider is not a [`Provider`] name.
    Provider,
    /// The protection is not three of `r`, `w`, `x` or `-`.
    Prot,
    /// A yes-or-no field is neither.
    Flag,
}

impl fmt::Display for LineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            LineError::Kind => "unknown kind of line",
            LineError::Fields => "wrong fields",
            LineError::Number => "a field is not a number",
            LineError::Status => "unknown status",
            LineError::Provider => "unknown provider",
            LineError::Prot => "bad protection",
            LineError::Flag => "a flag is neither yes nor no",
        })
    }
}

impl core::error::Error for LineError {}

/// A context's scheduling state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Status {
    /// Ready to run, or running.
    Runnable,
    /// Blocked with a wake-up time set (sleeping).
    Sleeping,
    /// Blocked until something wakes it.
    Blocked,
    /// Exited, waiting to be reaped.
    Dead,
}

impl Status {
    fn name(self) -> &'static str {
        match self {
            Status::Runnable => "runnable",
            Status::Sleeping => "sleeping",
            Status::Blocked => "blocked",
            Status::Dead => "dead",
        }
    }
}

impl FromStr for Status {
    type Err = LineError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        [
            Status::Runnable,
            Status::Sleeping,
            Status::Blocked,
            Status::Dead,
        ]
        .into_iter()
        .find(|s| s.name() == text)
        .ok_or(LineError::Status)
    }
}

/// A context's name as the kernel prints it: control characters become `?`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Name<'a>(pub &'a str);

impl fmt::Display for Name<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for c in self.0.chars() {
            if c.is_control() {
                f.write_str("?")?;
            } else {
                fmt::Write::write_char(f, c)?;
            }
        }
        Ok(())
    }
}

/// The first line of a context's block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContextLine<'a> {
    /// The context's debug id, as `sched.switch` records give it.
    pub id: u32,
    /// Its process id.
    pub pid: u64,
    /// Its effective user id.
    pub euid: u32,
    /// Its effective group id.
    pub egid: u32,
    /// Its scheduling state.
    pub status: Status,
    /// The CPU it is assigned to, if any.
    pub cpu: Option<u32>,
    /// Whether it is running on that CPU now.
    pub running: bool,
    /// Nanoseconds it has run.
    pub active_ns: u64,
    /// Nanoseconds the scheduler has spent on it.
    pub sched_ns: u64,
    /// Its name.
    pub name: Name<'a>,
}

/// The memory of a context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryLine {
    /// Bytes of grants the address space owns (anonymous and copy-on-write memory).
    pub private: u64,
    /// Bytes of grants shared with other address spaces or schemes. Borrowed physical memory
    /// (device registers, say) is in neither count.
    pub shared: u64,
    /// Bytes of kernel memory the context uses: its kernel stack and FPU state.
    pub kernel: u64,
}

/// The scheme a file descriptor is open on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SchemeRef<'a> {
    /// A kernel scheme, by name (`pipe`, `memory`, `kernel.trace`, ...).
    Kernel(&'a str),
    /// A user-space scheme, by scheme id: `user:<id>`.
    User(u64),
    /// The capability to create schemes: `cap`.
    Capability,
    /// The scheme manager: `mgr`.
    Manager,
    /// A scheme the kernel no longer has: `-`.
    Gone,
}

impl fmt::Display for SchemeRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SchemeRef::Kernel(name) => f.write_str(name),
            SchemeRef::User(id) => write!(f, "user:{id}"),
            SchemeRef::Capability => f.write_str("cap"),
            SchemeRef::Manager => f.write_str("mgr"),
            SchemeRef::Gone => f.write_str("-"),
        }
    }
}

impl<'a> SchemeRef<'a> {
    fn parse(text: &'a str) -> Result<Self, LineError> {
        Ok(match text {
            "cap" => SchemeRef::Capability,
            "mgr" => SchemeRef::Manager,
            "-" => SchemeRef::Gone,
            "" => return Err(LineError::Fields),
            _ => match text.strip_prefix("user:") {
                Some(id) => SchemeRef::User(number(id)?),
                None => SchemeRef::Kernel(text),
            },
        })
    }
}

/// An open file descriptor of a context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FdLine<'a> {
    /// The descriptor's number in the context's table.
    pub fd: u64,
    /// The scheme it is open on.
    pub scheme: SchemeRef<'a>,
    /// The scheme's handle number for it.
    pub number: u64,
}

/// Where a grant's memory comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Provider {
    /// Owned memory, possibly copy-on-write shared after a fork: `allocated`.
    Allocated,
    /// Owned memory that stays shared across forks (`MAP_SHARED` anonymous or `memory:`):
    /// `allocated_shared`.
    AllocatedShared,
    /// Physical memory that is not RAM the kernel allocates, such as device registers:
    /// `phys`.
    PhysBorrowed,
    /// Memory borrowed from another address space: `external`.
    External,
    /// Memory a scheme mapped (`fmap`): `fmap`.
    FmapBorrowed,
}

impl Provider {
    fn name(self) -> &'static str {
        match self {
            Provider::Allocated => "allocated",
            Provider::AllocatedShared => "allocated_shared",
            Provider::PhysBorrowed => "phys",
            Provider::External => "external",
            Provider::FmapBorrowed => "fmap",
        }
    }
}

impl FromStr for Provider {
    type Err = LineError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        [
            Provider::Allocated,
            Provider::AllocatedShared,
            Provider::PhysBorrowed,
            Provider::External,
            Provider::FmapBorrowed,
        ]
        .into_iter()
        .find(|p| p.name() == text)
        .ok_or(LineError::Provider)
    }
}

/// A grant's protection, written `rwx` with `-` for each right it lacks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Prot {
    /// Readable.
    pub read: bool,
    /// Writable.
    pub write: bool,
    /// Executable.
    pub execute: bool,
}

impl fmt::Display for Prot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (on, c) in [(self.read, "r"), (self.write, "w"), (self.execute, "x")] {
            f.write_str(if on { c } else { "-" })?;
        }
        Ok(())
    }
}

impl FromStr for Prot {
    type Err = LineError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let bytes = text.as_bytes();
        let flag = |i: usize, c: u8| match bytes.get(i) {
            Some(&b) if b == c => Ok(true),
            Some(b'-') => Ok(false),
            _ => Err(LineError::Prot),
        };
        if bytes.len() != 3 {
            return Err(LineError::Prot);
        }
        Ok(Self {
            read: flag(0, b'r')?,
            write: flag(1, b'w')?,
            execute: flag(2, b'x')?,
        })
    }
}

/// A grant (mapping) of a context's address space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GrantLine {
    /// Its first address.
    pub base: u64,
    /// Its size in pages.
    pub pages: u64,
    /// Where its memory comes from.
    pub provider: Provider,
    /// Its protection.
    pub prot: Prot,
}

/// One line of `/scheme/sys/proc`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Line<'a> {
    /// The start of a context's block.
    Context(ContextLine<'a>),
    /// The context's memory.
    Memory(MemoryLine),
    /// One of its file descriptors.
    Fd(FdLine<'a>),
    /// One of its grants.
    Grant(GrantLine),
}

impl fmt::Display for ContextLine<'_> {
    /// The line, with its `\n`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "context {} pid {} euid {} egid {} status {} cpu ",
            self.id,
            self.pid,
            self.euid,
            self.egid,
            self.status.name()
        )?;
        match self.cpu {
            Some(cpu) => write!(f, "{cpu}")?,
            None => f.write_str("-")?,
        }
        writeln!(
            f,
            " running {} active_ns {} sched_ns {} name {}",
            yes_no(self.running),
            self.active_ns,
            self.sched_ns,
            self.name
        )
    }
}

impl fmt::Display for MemoryLine {
    /// The line, with its `\n`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "memory private {} shared {} kernel {}",
            self.private, self.shared, self.kernel
        )
    }
}

impl fmt::Display for FdLine<'_> {
    /// The line, with its `\n`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "fd {} scheme {} number {}",
            self.fd, self.scheme, self.number
        )
    }
}

impl fmt::Display for GrantLine {
    /// The line, with its `\n`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "grant {:#x} pages {} provider {} prot {}",
            self.base,
            self.pages,
            self.provider.name(),
            self.prot
        )
    }
}

impl fmt::Display for Line<'_> {
    /// The line, with its `\n`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Line::Context(line) => line.fmt(f),
            Line::Memory(line) => line.fmt(f),
            Line::Fd(line) => line.fmt(f),
            Line::Grant(line) => line.fmt(f),
        }
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn number<T: FromStr>(text: &str) -> Result<T, LineError> {
    text.parse().map_err(|_| LineError::Number)
}

fn hex(text: &str) -> Result<u64, LineError> {
    let digits = text.strip_prefix("0x").ok_or(LineError::Number)?;
    u64::from_str_radix(digits, 16).map_err(|_| LineError::Number)
}

/// The words of a line, each value read after checking its key.
struct Fields<'a> {
    words: core::str::SplitN<'a, char>,
}

impl<'a> Fields<'a> {
    /// The value after `key`.
    fn value(&mut self, key: &str) -> Result<&'a str, LineError> {
        match (self.words.next(), self.words.next()) {
            (Some(k), Some(value)) if k == key => Ok(value),
            _ => Err(LineError::Fields),
        }
    }

    /// The last value after `key`, which must use up the line.
    fn last(mut self, key: &str) -> Result<&'a str, LineError> {
        let value = self.value(key)?;
        match self.words.next() {
            None => Ok(value),
            Some(_) => Err(LineError::Fields),
        }
    }
}

impl<'a> Line<'a> {
    /// Parses a line, with or without its `\n`.
    ///
    /// # Errors
    ///
    /// [`LineError`] says what is wrong: an unknown first word, missing, extra or misnamed
    /// fields, or a value that is not of its field's kind.
    pub fn parse(line: &'a str) -> Result<Self, LineError> {
        let line = line.strip_suffix('\n').unwrap_or(line);
        let (kind, rest) = line.split_once(' ').ok_or(LineError::Kind)?;
        match kind {
            "context" => parse_context(rest).map(Line::Context),
            "memory" => {
                let mut f = fields(rest, 6);
                Ok(Line::Memory(MemoryLine {
                    private: number(f.value("private")?)?,
                    shared: number(f.value("shared")?)?,
                    kernel: number(f.last("kernel")?)?,
                }))
            }
            "fd" => {
                let (fd, rest) = rest.split_once(' ').ok_or(LineError::Fields)?;
                let mut f = fields(rest, 4);
                Ok(Line::Fd(FdLine {
                    fd: number(fd)?,
                    scheme: SchemeRef::parse(f.value("scheme")?)?,
                    number: number(f.last("number")?)?,
                }))
            }
            "grant" => {
                let (base, rest) = rest.split_once(' ').ok_or(LineError::Fields)?;
                let mut f = fields(rest, 6);
                Ok(Line::Grant(GrantLine {
                    base: hex(base)?,
                    pages: number(f.value("pages")?)?,
                    provider: f.value("provider")?.parse()?,
                    prot: f.last("prot")?.parse()?,
                }))
            }
            _ => Err(LineError::Kind),
        }
    }
}

/// The space-separated words of `text`: `n` of them, and then the rest of the line as one.
fn fields(text: &str, n: usize) -> Fields<'_> {
    Fields {
        words: text.splitn(n + 1, ' '),
    }
}

fn parse_context(rest: &str) -> Result<ContextLine<'_>, LineError> {
    let (id, rest) = rest.split_once(' ').ok_or(LineError::Fields)?;
    // Eight pairs before the name, and the name, which may hold spaces, after its key.
    let mut f = fields(rest, 17);
    let pid = number(f.value("pid")?)?;
    let euid = number(f.value("euid")?)?;
    let egid = number(f.value("egid")?)?;
    let status = f.value("status")?.parse()?;
    let cpu = match f.value("cpu")? {
        "-" => None,
        cpu => Some(number(cpu)?),
    };
    let running = match f.value("running")? {
        "yes" => true,
        "no" => false,
        _ => return Err(LineError::Flag),
    };
    let active_ns = number(f.value("active_ns")?)?;
    let sched_ns = number(f.value("sched_ns")?)?;
    let name = f.last("name")?;
    Ok(ContextLine {
        id: number(id)?,
        pid,
        euid,
        egid,
        status,
        cpu,
        running,
        active_ns,
        sched_ns,
        name: Name(name),
    })
}

/// One context's block, grouped by [`processes`].
#[cfg(feature = "alloc")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Process<'a> {
    /// Its first line.
    pub context: ContextLine<'a>,
    /// Its memory, if the block had a memory line.
    pub memory: Option<MemoryLine>,
    /// Its file descriptors.
    pub fds: alloc::vec::Vec<FdLine<'a>>,
    /// Its grants.
    pub grants: alloc::vec::Vec<GrantLine>,
}

/// Why a whole file does not parse.
#[cfg(feature = "alloc")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileError {
    /// The line number, from 1.
    pub line: usize,
    /// What is wrong with it.
    pub kind: FileErrorKind,
}

/// What is wrong with a line of a whole file.
#[cfg(feature = "alloc")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FileErrorKind {
    /// The line does not parse.
    Line(LineError),
    /// A memory, fd or grant line comes before the first context line.
    Orphan,
}

#[cfg(feature = "alloc")]
impl fmt::Display for FileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            FileErrorKind::Line(_) => write!(f, "line {} does not parse", self.line),
            FileErrorKind::Orphan => write!(f, "line {} comes before any context", self.line),
        }
    }
}

#[cfg(feature = "alloc")]
impl core::error::Error for FileError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match &self.kind {
            FileErrorKind::Line(err) => Some(err),
            FileErrorKind::Orphan => None,
        }
    }
}

/// Groups the text of `/scheme/sys/proc` into one [`Process`] per context, in file order.
///
/// # Errors
///
/// [`FileError`] for the first line that does not parse or that comes before any context.
///
/// # Examples
///
/// ```
/// let text = "context 3 pid 2 euid 0 egid 0 status runnable cpu 0 running yes active_ns 10 sched_ns 2 name init\n\
///             memory private 4096 shared 0 kernel 8192\n\
///             fd 0 scheme pipe number 1\n";
/// let all = ferric_procinfo::processes(text).expect("valid");
/// assert_eq!(all[0].context.name.0, "init");
/// assert_eq!(all[0].fds.len(), 1);
/// ```
#[cfg(feature = "alloc")]
pub fn processes(text: &str) -> Result<alloc::vec::Vec<Process<'_>>, FileError> {
    let mut out: alloc::vec::Vec<Process<'_>> = alloc::vec::Vec::new();
    for (index, line) in text.lines().enumerate() {
        let error = |kind| FileError {
            line: index + 1,
            kind,
        };
        let parsed = Line::parse(line).map_err(|err| error(FileErrorKind::Line(err)))?;
        if let Line::Context(context) = parsed {
            out.push(Process {
                context,
                memory: None,
                fds: alloc::vec::Vec::new(),
                grants: alloc::vec::Vec::new(),
            });
            continue;
        }
        let process = out.last_mut().ok_or(error(FileErrorKind::Orphan))?;
        match parsed {
            Line::Memory(memory) => process.memory = Some(memory),
            Line::Fd(fd) => process.fds.push(fd),
            Line::Grant(grant) => process.grants.push(grant),
            Line::Context(_) => {}
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::string::ToString;

    use super::*;

    const CONTEXT: ContextLine<'static> = ContextLine {
        id: 12,
        pid: 9,
        euid: 0,
        egid: 1,
        status: Status::Sleeping,
        cpu: Some(1),
        running: false,
        active_ns: 5_210_000,
        sched_ns: 61_000,
        name: Name("/usr/bin/a b"),
    };

    #[test]
    fn every_line_round_trips() {
        let lines = [
            Line::Context(CONTEXT),
            Line::Context(ContextLine {
                cpu: None,
                running: true,
                status: Status::Dead,
                ..CONTEXT
            }),
            Line::Memory(MemoryLine {
                private: 1,
                shared: 2,
                kernel: 3,
            }),
            Line::Fd(FdLine {
                fd: 4,
                scheme: SchemeRef::Kernel("kernel.trace"),
                number: 5,
            }),
            Line::Fd(FdLine {
                fd: 6,
                scheme: SchemeRef::User(7),
                number: 8,
            }),
            Line::Fd(FdLine {
                fd: 9,
                scheme: SchemeRef::Gone,
                number: 0,
            }),
            Line::Grant(GrantLine {
                base: 0x40_0000,
                pages: 12,
                provider: Provider::AllocatedShared,
                prot: Prot {
                    read: true,
                    write: false,
                    execute: true,
                },
            }),
        ];
        for line in lines {
            let text = line.to_string();
            assert!(text.ends_with('\n'));
            assert_eq!(Line::parse(&text), Ok(line), "{text}");
        }
    }

    #[test]
    fn the_documented_lines_parse() {
        let text = "context 12 pid 9 euid 0 egid 1 status sleeping cpu 1 running no active_ns 5210000 sched_ns 61000 name /usr/bin/a b\n";
        assert_eq!(Line::parse(text), Ok(Line::Context(CONTEXT)));
        assert_eq!(Line::Context(CONTEXT).to_string(), text);
        assert_eq!(
            Line::parse("grant 0x400000 pages 12 provider allocated prot r-x"),
            Ok(Line::Grant(GrantLine {
                base: 0x40_0000,
                pages: 12,
                provider: Provider::Allocated,
                prot: Prot {
                    read: true,
                    write: false,
                    execute: true
                },
            }))
        );
    }

    #[test]
    fn control_characters_in_a_name_cannot_start_a_line() {
        let line = ContextLine {
            name: Name("evil\nfd 0 scheme pipe number 1"),
            ..CONTEXT
        }
        .to_string();
        assert_eq!(line.lines().count(), 1);
        let Ok(Line::Context(parsed)) = Line::parse(&line) else {
            panic!("{line}");
        };
        assert_eq!(parsed.name.0, "evil?fd 0 scheme pipe number 1");
    }

    #[test]
    fn bad_lines_say_why() {
        for (line, err) in [
            ("", LineError::Kind),
            ("process 1", LineError::Kind),
            ("memory private 1 shared 2", LineError::Fields),
            (
                "memory private 1 shared 2 kernel 3 extra 4",
                LineError::Fields,
            ),
            ("memory private x shared 2 kernel 3", LineError::Number),
            ("memory shared 1 private 2 kernel 3", LineError::Fields),
            ("fd 1 scheme  number 2", LineError::Fields),
            ("fd 1 scheme user:x number 2", LineError::Number),
            (
                "grant 400000 pages 1 provider allocated prot rwx",
                LineError::Number,
            ),
            (
                "grant 0x1 pages 1 provider lent prot rwx",
                LineError::Provider,
            ),
            ("grant 0x1 pages 1 provider phys prot rw", LineError::Prot),
            ("grant 0x1 pages 1 provider phys prot wrx", LineError::Prot),
            (
                "context 1 pid 1 euid 0 egid 0 status asleep cpu - running no active_ns 0 sched_ns 0 name x",
                LineError::Status,
            ),
            (
                "context 1 pid 1 euid 0 egid 0 status dead cpu - running maybe active_ns 0 sched_ns 0 name x",
                LineError::Flag,
            ),
            (
                "context 1 pid 1 euid 0 egid 0 status dead cpu - running no active_ns 0 sched_ns 0",
                LineError::Fields,
            ),
        ] {
            assert_eq!(Line::parse(line), Err(err), "{line:?}");
        }
    }

    #[cfg(feature = "alloc")]
    #[test]
    fn processes_group_their_lines() {
        let mut text = std::string::String::new();
        text.push_str(&CONTEXT.to_string());
        text.push_str("memory private 1 shared 2 kernel 3\nfd 0 scheme pipe number 1\n");
        text.push_str("grant 0x1000 pages 1 provider fmap prot rw-\n");
        text.push_str(&ContextLine { id: 13, ..CONTEXT }.to_string());
        let all = processes(&text).expect("valid");
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].fds.len(), 1);
        assert_eq!(all[0].grants[0].provider, Provider::FmapBorrowed);
        assert_eq!(all[1].context.id, 13);
        assert!(all[1].memory.is_none());
        assert_eq!(
            processes("fd 0 scheme pipe number 1\n"),
            Err(FileError {
                line: 1,
                kind: FileErrorKind::Orphan
            })
        );
        assert_eq!(
            processes(&std::format!("{text}bogus line\n")).map(|_| ()),
            Err(FileError {
                line: 6,
                kind: FileErrorKind::Line(LineError::Kind)
            })
        );
    }
}
