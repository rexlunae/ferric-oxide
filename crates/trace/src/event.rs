//! Event metadata, the compile-time catalog and the [`events!`](crate::events!) macro.

use core::fmt;

use crate::{EventSet, Level};

/// The most arguments an event can carry; each is one 64-bit word in a record.
pub const MAX_ARGS: usize = 8;

/// The most events a catalog can hold, the size of an [`EventSet`](crate::EventSet).
pub const MAX_EVENTS: usize = 256;

/// The part of the kernel an event belongs to, like `tracing`'s *target*.
///
/// The kernel compiles each subsystem's tracepoints in or out with its own Cargo feature
/// (`trace_<name>`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
#[repr(u8)]
pub enum Subsystem {
    /// System call entry and exit.
    Syscall = 0,
    /// The scheduler: context switches and futex waits, resumes and wakes.
    Sched = 1,
    /// User-space scheme requests and responses, tagged with their ring SQE/CQE tags.
    Scheme = 2,
    /// Memory management: page faults.
    Mm = 3,
    /// Interrupts the kernel hands to user-space drivers, and their acknowledgements.
    Irq = 4,
    /// Faults the IOMMU reports: blocked DMA and interrupt requests.
    Iommu = 5,
}

impl Subsystem {
    /// Every subsystem.
    pub const ALL: [Subsystem; 6] = [
        Subsystem::Syscall,
        Subsystem::Sched,
        Subsystem::Scheme,
        Subsystem::Mm,
        Subsystem::Irq,
        Subsystem::Iommu,
    ];

    /// The subsystem's name: the first part of its events' names, and its Cargo feature's suffix.
    pub const fn name(self) -> &'static str {
        match self {
            Subsystem::Syscall => "syscall",
            Subsystem::Sched => "sched",
            Subsystem::Scheme => "scheme",
            Subsystem::Mm => "mm",
            Subsystem::Irq => "irq",
            Subsystem::Iommu => "iommu",
        }
    }

    /// The subsystem named `name`, if there is one.
    pub fn from_name(name: &str) -> Option<Self> {
        Subsystem::ALL.into_iter().find(|s| s.name() == name)
    }

    /// The kernel Cargo feature that compiles the subsystem's tracepoints in, `trace_<name>`.
    pub const fn kernel_feature(self) -> &'static str {
        match self {
            Subsystem::Syscall => "trace_syscall",
            Subsystem::Sched => "trace_sched",
            Subsystem::Scheme => "trace_scheme",
            Subsystem::Mm => "trace_mm",
            Subsystem::Irq => "trace_irq",
            Subsystem::Iommu => "trace_iommu",
        }
    }
}

impl fmt::Display for Subsystem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// An event's number: its index in its [`Catalog`] and the id in every record of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventId(u16);

impl EventId {
    /// The id `raw`.
    pub const fn new(raw: u16) -> Self {
        Self(raw)
    }

    /// The raw id.
    pub const fn get(self) -> u16 {
        self.0
    }

    /// The id as an index into a catalog or an [`EventSet`](crate::EventSet).
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// One kind of trace event: its id, name, subsystem, level and argument names.
///
/// Events are `const` items made by [`events!`](crate::events!); the kernel uses them in `trace_event!` and
/// decoders look them up in the [`Catalog`] by the id in a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    id: EventId,
    name: &'static str,
    subsystem: Subsystem,
    level: Level,
    args: &'static [&'static str],
}

impl Event {
    /// An event. Use [`events!`](crate::events!) rather than calling this directly, so the catalog checks run.
    ///
    /// # Panics
    ///
    /// When `args` has more than [`MAX_ARGS`] names; in a `const` item this is a compile error.
    pub const fn new(
        id: EventId,
        name: &'static str,
        subsystem: Subsystem,
        level: Level,
        args: &'static [&'static str],
    ) -> Self {
        assert!(
            args.len() <= MAX_ARGS,
            "an event has at most MAX_ARGS arguments"
        );
        Self {
            id,
            name,
            subsystem,
            level,
            args,
        }
    }

    /// The event's id.
    pub const fn id(&self) -> EventId {
        self.id
    }

    /// Whether a kernel has the event's tracepoints: when its subsystem is compiled in
    /// (`subsystem_compiled`) and its level is within `max_level`, the most verbose level
    /// compiled in.
    pub const fn compiled_with(&self, subsystem_compiled: bool, max_level: Level) -> bool {
        subsystem_compiled && self.level.within(max_level)
    }

    /// The event's full name, `<subsystem>.<event>`.
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// The subsystem the event belongs to.
    pub const fn subsystem(&self) -> Subsystem {
        self.subsystem
    }

    /// The event's level.
    pub const fn level(&self) -> Level {
        self.level
    }

    /// The names of the event's arguments, in record order.
    pub const fn args(&self) -> &'static [&'static str] {
        self.args
    }
}

/// Every event of one producer (the kernel), indexed by id.
#[derive(Clone, Copy, Debug)]
pub struct Catalog {
    events: &'static [Event],
}

impl Catalog {
    /// A catalog of `events`. [`events!`](crate::events!) calls this in a `const` item, so the checks below are
    /// compile errors there.
    ///
    /// # Panics
    ///
    /// When an event's id is not its index in `events` (ids must be dense and in order, which
    /// also makes them unique), when two events share a name, or when there are more than
    /// [`MAX_EVENTS`] events.
    pub const fn new(events: &'static [Event]) -> Self {
        assert!(
            events.len() <= MAX_EVENTS,
            "a catalog has at most MAX_EVENTS events"
        );
        let mut i = 0;
        while i < events.len() {
            assert!(
                events[i].id.index() == i,
                "event ids must be 0, 1, 2, ... in declaration order"
            );
            let mut j = 0;
            while j < i {
                assert!(
                    !str_eq(events[i].name, events[j].name),
                    "two events have the same name"
                );
                j += 1;
            }
            i += 1;
        }
        Self { events }
    }

    /// The event with id `id`, if the catalog has it.
    pub fn get(&self, id: EventId) -> Option<&'static Event> {
        self.events.get(id.index())
    }

    /// The event named `name`, if the catalog has it.
    pub fn find(&self, name: &str) -> Option<&'static Event> {
        self.events.iter().find(|event| event.name == name)
    }

    /// Every event, in id order.
    pub fn events(&self) -> &'static [Event] {
        self.events
    }

    /// The events a kernel built with the Cargo features for which `has_feature` is true has:
    /// see [`Subsystem::kernel_feature`], [`Level::kernel_feature`] and
    /// [`Event::compiled_with`]. Host tools use it to check selections against a configured
    /// kernel; the kernel itself decides with `cfg!` by the same rule.
    pub fn compiled_for(&self, has_feature: impl Fn(&str) -> bool) -> EventSet {
        let max_level = Level::ALL
            .into_iter()
            .rev()
            .find(|level| level.kernel_feature().is_none_or(&has_feature))
            .unwrap_or(Level::Error);
        let mut set = EventSet::new();
        for event in self.events {
            let subsystem = has_feature(event.subsystem.kernel_feature());
            if event.compiled_with(subsystem, max_level) {
                set.insert(event.id);
            }
        }
        set
    }

    /// The number of events.
    pub const fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether the catalog has no events.
    pub const fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

/// `a == b` for `const` code.
const fn str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Declares trace events as `const` [`Event`]s and a `const` [`Catalog`] of all of them.
///
/// Each event names its id, full name, [`Subsystem`] variant, [`Level`] variant and argument
/// names. Ids must count up from 0 in declaration order and names must be unique: the macro adds a
/// `const _` item that evaluates the catalog, so a mistake is a compile error.
///
/// # Examples
///
/// ```
/// ferric_trace::events! {
///     /// The demo catalog.
///     pub catalog DEMO;
///
///     /// A context switch.
///     pub const SWITCH = 0 {
///         name: "sched.switch", subsystem: Sched, level: Info, args: ["prev", "next"],
///     }
///     /// A system call.
///     pub const ENTER = 1 {
///         name: "syscall.enter", subsystem: Syscall, level: Debug, args: ["nr"],
///     }
/// }
///
/// assert_eq!(DEMO.len(), 2);
/// assert_eq!(DEMO.find("syscall.enter"), Some(&ENTER));
/// assert_eq!(SWITCH.args(), ["prev", "next"]);
/// ```
///
/// Two events with the same id do not compile:
///
/// ```compile_fail
/// ferric_trace::events! {
///     /// Broken.
///     pub catalog BROKEN;
///     /// One.
///     pub const A = 0 { name: "sched.a", subsystem: Sched, level: Info, args: [] }
///     /// Two.
///     pub const B = 0 { name: "sched.b", subsystem: Sched, level: Info, args: [] }
/// }
/// ```
#[macro_export]
macro_rules! events {
    (
        $(#[$cmeta:meta])*
        $cvis:vis catalog $catalog:ident;
        $(
            $(#[$meta:meta])*
            $vis:vis const $event:ident = $id:literal {
                name: $name:literal,
                subsystem: $subsystem:ident,
                level: $level:ident,
                args: [$($arg:literal),* $(,)?] $(,)?
            }
        )*
    ) => {
        $(
            $(#[$meta])*
            $vis const $event: $crate::Event = $crate::Event::new(
                $crate::EventId::new($id),
                $name,
                $crate::Subsystem::$subsystem,
                $crate::Level::$level,
                &[$($arg),*],
            );
        )*

        $(#[$cmeta])*
        $cvis const $catalog: $crate::Catalog = $crate::Catalog::new(&[$($event),*]);

        // Evaluates the catalog (and so every check in `Catalog::new` and `Event::new`) even
        // when nothing else uses it.
        const _: $crate::Catalog = $catalog;
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::events! {
        /// A test catalog.
        catalog TEST;
        /// First.
        const FIRST = 0 { name: "syscall.first", subsystem: Syscall, level: Info, args: ["a"] }
        /// Second.
        const SECOND = 1 {
            name: "sched.second",
            subsystem: Sched,
            level: Trace,
            args: ["a", "b", "c", "d", "e", "f", "g", "h"],
        }
    }

    #[test]
    fn catalog_lookup() {
        assert_eq!(TEST.len(), 2);
        assert!(!TEST.is_empty());
        assert_eq!(TEST.get(EventId::new(1)), Some(&SECOND));
        assert_eq!(TEST.get(EventId::new(2)), None);
        assert_eq!(TEST.find("syscall.first"), Some(&FIRST));
        assert_eq!(TEST.find("syscall"), None);
        assert_eq!(SECOND.args().len(), MAX_ARGS);
        assert_eq!(FIRST.subsystem(), Subsystem::Syscall);
        assert_eq!(SECOND.level(), Level::Trace);
    }

    #[test]
    #[should_panic(expected = "same name")]
    fn duplicate_names_are_rejected() {
        static EVENTS: [Event; 2] = [
            Event::new(
                EventId::new(0),
                "sched.x",
                Subsystem::Sched,
                Level::Info,
                &[],
            ),
            Event::new(
                EventId::new(1),
                "sched.x",
                Subsystem::Sched,
                Level::Info,
                &[],
            ),
        ];
        let _ = core::hint::black_box(Catalog::new)(&EVENTS);
    }

    #[test]
    #[should_panic(expected = "event ids")]
    fn sparse_ids_are_rejected() {
        static EVENTS: [Event; 1] = [Event::new(
            EventId::new(3),
            "sched.x",
            Subsystem::Sched,
            Level::Info,
            &[],
        )];
        let _ = core::hint::black_box(Catalog::new)(&EVENTS);
    }

    #[test]
    #[should_panic(expected = "MAX_ARGS")]
    fn too_many_args_are_rejected() {
        let _ = core::hint::black_box(Event::new)(
            EventId::new(0),
            "sched.x",
            Subsystem::Sched,
            Level::Info,
            &["1", "2", "3", "4", "5", "6", "7", "8", "9"],
        );
    }

    #[test]
    fn subsystem_names() {
        for s in Subsystem::ALL {
            assert_eq!(Subsystem::from_name(s.name()), Some(s));
        }
        assert_eq!(Subsystem::from_name("vm"), None);
    }

    #[test]
    fn compiled_for_features() {
        let all = TEST.compiled_for(|_| true);
        assert_eq!(all.len(), TEST.len());
        let none = TEST.compiled_for(|_| false);
        assert!(
            none.iter()
                .all(|id| TEST.get(id).is_some_and(|e| e.level() == Level::Error))
        );
        let no_sched = TEST.compiled_for(|f| f != Subsystem::Sched.kernel_feature());
        assert!(no_sched.contains(FIRST.id()));
        assert!(!no_sched.contains(SECOND.id()));
        // Without trace_level_info, Info events are out even with their subsystem.
        let warn_only = TEST.compiled_for(|f| {
            !matches!(
                f,
                "trace_level_info" | "trace_level_debug" | "trace_level_trace"
            )
        });
        assert!(!warn_only.contains(FIRST.id()));
    }
}
