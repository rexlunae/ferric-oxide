//! Flame graphs from a [`Profile`]: each sample's stack symbolized and folded into
//! Brendan Gregg's "folded stacks" text (`frame;frame;frame count` per line, root first), which
//! [`crate::svg`] draws and other flame graph tools read.
//!
//! A stack is the sample's instruction pointer and return addresses, outermost first, under a
//! root frame naming the context. Addresses are symbolized from ELF symbol tables:
//!
//! - kernel addresses (the upper half) from the kernel ELF, after taking away the KASLR slide:
//!   the profile's run-time `__text_start` minus the ELF's (or its `.text` address, for an
//!   ELF without that symbol);
//! - user addresses from the image of the context's address space that holds them, as
//!   [`Profile::images`] lists them: the ELF is read from the given root directory (the
//!   target's file system on the host, such as the cookbook's sysroot) at the image's path,
//!   and the load bias is taken away. When an image's address was reused during the
//!   recording (one library unloaded and another, or the same one again, mapped there), the
//!   sample is resolved against the occurrence that was live at its epoch
//!   ([`crate::profile::Live::covers`]), and against the newest one that holds the address
//!   when none was, or the profile does not date its samples.
//!
//! Return addresses point after their call, so they are looked up one byte earlier. An address
//! no symbol covers is shown as `[kernel]`, `[<file name>]` inside a known image, or
//! `[unknown]`. Rust symbols are demangled without their hash.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ferric_trace::profile::{KERNEL_HALF, Sample};
use object::{Object, ObjectSection, ObjectSegment, ObjectSymbol, SymbolKind};

use crate::profile::{LoadedImage, Profile};

/// Why a flame graph could not be made.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FlamegraphError {
    /// The kernel ELF could not be read.
    #[error("{}", path.display())]
    Io {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The kernel ELF does not parse.
    #[error("{} is not an ELF file", path.display())]
    Elf {
        /// The file.
        path: PathBuf,
        /// Why it does not parse.
        #[source]
        source: object::Error,
    },
}

/// The symbols of one ELF file.
#[derive(Debug)]
struct Symbols {
    /// Function start, end (exclusive, 0 when the size is unknown) and name, by start.
    functions: Vec<(u64, u64, String)>,
    /// The span of its loadable segments, in ELF addresses.
    span: (u64, u64),
    /// `__text_start`, or the `.text` address.
    text_start: Option<u64>,
}

impl Symbols {
    fn parse(data: &[u8]) -> Result<Self, object::Error> {
        let file = object::File::parse(data)?;
        let mut functions: Vec<_> = file
            .symbols()
            .filter(|s| s.kind() == SymbolKind::Text && s.is_definition() && s.address() != 0)
            .filter_map(|s| {
                let name = s.name().ok()?;
                let end = if s.size() == 0 {
                    0
                } else {
                    s.address().saturating_add(s.size())
                };
                Some((
                    s.address(),
                    end,
                    format!("{:#}", rustc_demangle::demangle(name)),
                ))
            })
            .collect();
        functions.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
        functions.dedup_by_key(|f| f.0);
        let span = file
            .segments()
            .filter(|s| s.size() > 0)
            .fold(None, |span: Option<(u64, u64)>, s| {
                let (start, end) = (s.address(), s.address().saturating_add(s.size()));
                Some(span.map_or((start, end), |(a, b)| (a.min(start), b.max(end))))
            })
            .unwrap_or((0, 0));
        let text_start = file
            .symbol_by_name("__text_start")
            .map(|s| s.address())
            .or_else(|| file.section_by_name(".text").map(|s| s.address()));
        Ok(Self {
            functions,
            span,
            text_start,
        })
    }

    /// The function holding `addr`, an ELF address.
    fn lookup(&self, addr: u64) -> Option<&str> {
        let index = self
            .functions
            .partition_point(|f| f.0 <= addr)
            .checked_sub(1)?;
        let (start, end, name) = &self.functions[index];
        // A function of unknown size covers up to the next one, or the end of the file's
        // segments.
        let covered = if *end == 0 {
            addr < self
                .functions
                .get(index + 1)
                .map_or(self.span.1, |next| next.0)
        } else {
            addr < *end
        };
        (addr >= *start && covered).then_some(name.as_str())
    }
}

/// Symbolizes addresses for one profile.
#[derive(Debug)]
pub struct Symbolizer {
    kernel: Option<(Symbols, u64)>,
    /// The root, canonical, or `None` without one (or when it cannot be resolved).
    root: Option<PathBuf>,
    /// User ELF files by target path; `None` for one that cannot be read.
    users: HashMap<String, Option<Symbols>>,
}

impl Symbolizer {
    /// A symbolizer for `profile`, with the kernel ELF at `kernel` and the target's files under
    /// `root`; either may be left out, and its addresses then stay unknown.
    ///
    /// # Errors
    ///
    /// [`FlamegraphError::Io`] or [`FlamegraphError::Elf`] if the kernel ELF cannot be read.
    /// User ELF files that cannot be read are not an error: their frames show the file name.
    ///
    /// Image paths come from the target and are not trusted: a file is read only when its
    /// path, with every symbolic link resolved, stays under `root`: `..` components are
    /// refused, and no link is followed out of it.
    pub fn new(
        profile: &Profile,
        kernel: Option<&Path>,
        root: Option<&Path>,
    ) -> Result<Self, FlamegraphError> {
        let kernel = kernel
            .map(|path| {
                let data = fs::read(path).map_err(|source| FlamegraphError::Io {
                    path: path.to_owned(),
                    source,
                })?;
                let symbols = Symbols::parse(&data).map_err(|source| FlamegraphError::Elf {
                    path: path.to_owned(),
                    source,
                })?;
                let slide = profile
                    .kernel_text
                    .text_start
                    .wrapping_sub(symbols.text_start.unwrap_or(0));
                Ok((symbols, slide))
            })
            .transpose()?;
        Ok(Self {
            kernel,
            // A root that cannot be resolved holds no files, as one that is missing.
            root: root.and_then(|root| root.canonicalize().ok()),
            users: HashMap::new(),
        })
    }

    /// The name of the frame at `addr` in a context with `images` (oldest first), for a
    /// sample of `epoch` ([`crate::profile::Batch::epoch`]); `ret` for a return address.
    pub fn frame(
        &mut self,
        addr: u64,
        ret: bool,
        images: &[LoadedImage],
        epoch: Option<u32>,
    ) -> String {
        let lookup = if ret { addr.wrapping_sub(1) } else { addr };
        if addr >= KERNEL_HALF {
            return self
                .kernel
                .as_ref()
                .and_then(|(symbols, slide)| symbols.lookup(lookup.wrapping_sub(*slide)))
                .unwrap_or("[kernel]")
                .to_owned();
        }
        // The images live when the sample was taken first, then any; newest first in each,
        // as a library mapped where an unloaded one was is the likelier one.
        let live = images
            .iter()
            .rev()
            .filter(|image| matches!((image.live, epoch), (Some(live), Some(e)) if live.covers(e)));
        for image in live.chain(images.iter().rev()) {
            if let Some(name) = self.in_image(lookup, image) {
                return name;
            }
        }
        "[unknown]".to_owned()
    }

    /// The frame at `lookup` if `image` holds it.
    fn in_image(&mut self, lookup: u64, image: &LoadedImage) -> Option<String> {
        let symbols = self.user(&image.path)?;
        let elf_addr = lookup.wrapping_sub(image.base);
        if elf_addr < symbols.span.0 || elf_addr >= symbols.span.1 {
            return None;
        }
        Some(match symbols.lookup(elf_addr) {
            Some(name) => name.to_owned(),
            None => format!("[{}]", file_name(&image.path)),
        })
    }

    fn user(&mut self, path: &str) -> Option<&Symbols> {
        let root = self.root.as_ref()?;
        self.users
            .entry(path.to_owned())
            .or_insert_with(|| {
                let data = fs::read(contained(root, path)?).ok()?;
                Symbols::parse(&data).ok()
            })
            .as_ref()
    }
}

/// The host file for target path `path` under `root` (canonical), with every symbolic link
/// resolved, or `None` when there is none or it is not under `root`. Image paths come from
/// the target, which can write any path: `..` components are refused, and a link that leads
/// out of `root` (absolute target links included) is not followed out of it.
fn contained(root: &Path, path: &str) -> Option<PathBuf> {
    let relative = Path::new(path.trim_start_matches('/'));
    if relative
        .components()
        .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return None;
    }
    let resolved = root.join(relative).canonicalize().ok()?;
    resolved.starts_with(root).then_some(resolved)
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Folded stacks: each distinct stack, root first and `;`-separated, with its sample count.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Folded {
    /// The stacks and their counts.
    pub stacks: BTreeMap<String, u64>,
    /// Samples that could not be read (the rest of a CPU's buffer after a bad sample).
    pub bad: usize,
}

impl Folded {
    /// Folds every sample of `profile`.
    pub fn new(profile: &Profile, symbolizer: &mut Symbolizer) -> Self {
        let mut folded = Self::default();
        for item in profile.samples() {
            match item.sample {
                Ok(sample) => {
                    let stack = stack(profile, symbolizer, &sample, item.epoch);
                    *folded.stacks.entry(stack).or_default() += 1;
                }
                Err(_) => folded.bad += 1,
            }
        }
        folded
    }

    /// The number of samples folded.
    pub fn total(&self) -> u64 {
        self.stacks.values().sum()
    }

    /// The folded-stacks text, one `stack count` line each.
    pub fn text(&self) -> String {
        self.stacks
            .iter()
            .map(|(stack, count)| format!("{stack} {count}\n"))
            .collect()
    }
}

fn stack(
    profile: &Profile,
    symbolizer: &mut Symbolizer,
    sample: &Sample<'_>,
    epoch: Option<u32>,
) -> String {
    let images = profile
        .images
        .get(&sample.context)
        .map_or(&[][..], Vec::as_slice);
    let root = profile
        .names
        .get(&sample.context)
        .cloned()
        .unwrap_or_else(|| format!("context {}", sample.context));
    let mut frames: Vec<String> = sample
        .stack()
        .enumerate()
        .map(|(depth, addr)| symbolizer.frame(addr, depth > 0, images, epoch))
        .collect();
    frames.push(root);
    frames.reverse();
    frames
        .iter()
        .map(|frame| frame.replace([';', '\n'], ":"))
        .collect::<Vec<_>>()
        .join(";")
}

#[cfg(test)]
mod tests {
    use ferric_trace::profile::{ControlLine, SAMPLE_MAGIC};

    use super::*;
    use crate::profile::Live;

    #[inline(never)]
    fn marker_function() -> u64 {
        std::hint::black_box(7)
    }

    fn exe() -> (PathBuf, Symbols) {
        let path = std::env::current_exe().expect("test binary");
        let symbols = Symbols::parse(&fs::read(&path).expect("read")).expect("ELF");
        (path, symbols)
    }

    fn marker(symbols: &Symbols) -> u64 {
        assert_eq!(marker_function(), 7);
        symbols
            .functions
            .iter()
            .find(|f| f.2.ends_with("tests::marker_function"))
            .expect("the marker has a symbol")
            .0
    }

    fn profile(words: Vec<u64>, text_start: u64, images: Vec<LoadedImage>) -> Profile {
        Profile {
            kernel_text: ControlLine { text_start },
            cpus: vec![words],
            names: BTreeMap::from([(7, "prog".to_owned())]),
            images: BTreeMap::from([(7, images)]),
            batches: Vec::new(),
        }
    }

    #[test]
    fn symbolizes_user_and_kernel_frames() {
        let (path, symbols) = exe();
        let at = marker(&symbols);
        let text = symbols.text_start.expect("a .text section");
        let base = 0x5555_0000_0000;
        let slide = KERNEL_HALF + 0x4000_0000;
        let words = vec![
            SAMPLE_MAGIC | 7,
            slide + at + 2,                  // a kernel leaf in the marker
            100,                             // tsc
            7,                               // context
            slide + symbols.span.1 + 0x1000, // a kernel return address past the file
            base + at + 1,                   // a user return address just inside the marker
            0x10,                            // nothing is mapped there
        ];
        let image = LoadedImage {
            base,
            path: path.to_str().expect("UTF-8").to_owned(),
            live: None,
        };
        let profile = profile(words, slide + text, vec![image]);
        let mut symbolizer = Symbolizer::new(&profile, Some(&path), Some(Path::new("/"))).unwrap();
        let folded = Folded::new(&profile, &mut symbolizer);
        assert_eq!(folded.total(), 1);
        let (stack, count) = folded.stacks.iter().next().unwrap();
        let frames: Vec<&str> = stack.split(';').collect();
        assert_eq!(frames.len(), 5, "{stack}");
        assert_eq!(frames[0], "prog");
        assert_eq!(frames[1], "[unknown]");
        assert!(frames[2].ends_with("marker_function"), "{stack}");
        assert_eq!(frames[3], "[kernel]");
        assert!(frames[4].ends_with("marker_function"), "{stack}");
        assert_eq!(*count, 1);
        assert!(folded.text().ends_with(" 1\n"));
    }

    #[test]
    fn unknown_without_files() {
        let words = vec![
            SAMPLE_MAGIC | 4,
            0x40_1000,
            1,
            9,
            SAMPLE_MAGIC | 4,
            0x40_1000,
            2,
            9,
            3,
        ];
        let profile = profile(words, 0, Vec::new());
        let mut symbolizer = Symbolizer::new(&profile, None, None).unwrap();
        let folded = Folded::new(&profile, &mut symbolizer);
        assert_eq!(folded.text(), "context 9;[unknown] 2\n");
        assert_eq!(folded.bad, 1);
    }

    #[test]
    fn an_unreadable_image_is_skipped() {
        let image = LoadedImage {
            base: 0,
            path: "/no/such/libfoo.so".to_owned(),
            live: None,
        };
        let profile = profile(Vec::new(), 0, Vec::new());
        let mut symbolizer = Symbolizer::new(&profile, None, Some(Path::new("/"))).unwrap();
        assert_eq!(symbolizer.frame(0x10, false, &[image], None), "[unknown]");
    }

    #[test]
    fn a_bad_kernel_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("kernel");
        fs::write(&bad, b"not an ELF").unwrap();
        let profile = profile(Vec::new(), 0, Vec::new());
        assert!(matches!(
            Symbolizer::new(&profile, Some(&bad), None),
            Err(FlamegraphError::Elf { .. })
        ));
        assert!(matches!(
            Symbolizer::new(&profile, Some(&dir.path().join("none")), None),
            Err(FlamegraphError::Io { .. })
        ));
    }

    #[test]
    fn image_paths_stay_under_the_root() {
        let outside = tempfile::tempdir().expect("tempdir");
        fs::write(outside.path().join("secret"), b"host file").expect("write");
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonical");
        fs::create_dir_all(root.join("usr/bin")).expect("mkdir");
        fs::write(root.join("usr/bin/app"), b"elf").expect("write");
        assert_eq!(
            contained(&root, "/usr/bin/app"),
            Some(root.join("usr/bin/app"))
        );
        assert_eq!(
            contained(&root, "usr/bin/app"),
            Some(root.join("usr/bin/app"))
        );
        let escape = format!("/../{}", outside.path().display());
        for forged in [
            "/usr/bin/../../etc/passwd",
            "../secret",
            escape.as_str(),
            "/usr/bin/missing",
        ] {
            assert_eq!(contained(&root, forged), None, "{forged}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            // A link out of the root is not followed, an absolute one or a relative one.
            symlink(outside.path().join("secret"), root.join("usr/bin/out")).expect("link");
            symlink("../../../secret", root.join("usr/bin/up")).expect("link");
            assert_eq!(contained(&root, "/usr/bin/out"), None);
            assert_eq!(contained(&root, "/usr/bin/up"), None);
            // A link that stays inside is fine, and resolves to its target.
            symlink("app", root.join("usr/bin/alias")).expect("link");
            assert_eq!(
                contained(&root, "/usr/bin/alias"),
                Some(root.join("usr/bin/app"))
            );
        }
    }

    /// Three hard links (or copies) of the test binary under a root, as `/a`, `/b`, `/c`, and
    /// the frame name the first byte of its segments gets, which no function covers.
    fn three_images() -> (tempfile::TempDir, u64) {
        let (exe_path, symbols) = exe();
        let dir = tempfile::tempdir().unwrap();
        for name in ["a", "b", "c"] {
            let to = dir.path().join(name);
            if fs::hard_link(&exe_path, &to).is_err() {
                fs::copy(&exe_path, &to).unwrap();
            }
        }
        (dir, symbols.span.0)
    }

    fn at(path: &str, live: Option<(u32, u32)>) -> LoadedImage {
        LoadedImage {
            base: 0x5555_0000_0000,
            path: path.to_owned(),
            live: live.map(|(first, last)| Live { first, last }),
        }
    }

    #[test]
    fn the_newest_image_at_an_address_wins_without_dates() {
        // Review 4320670165: `a` was unloaded and `b` mapped at the same base.
        let (dir, start) = three_images();
        let images = [at("/a", None), at("/b", None)];
        let profile = profile(Vec::new(), 0, Vec::new());
        let mut symbolizer = Symbolizer::new(&profile, None, Some(dir.path())).unwrap();
        let addr = 0x5555_0000_0000 + start;
        assert_eq!(symbolizer.frame(addr, false, &images, None), "[b]");
        assert_eq!(symbolizer.frame(addr, false, &images, Some(3)), "[b]");
    }

    #[test]
    fn a_sample_resolves_against_the_image_live_at_its_epoch() {
        // Reviews 4320761258 and 4320761392: `a`, then `b`, then `a` again at one base, and
        // `c` after them. Old samples keep their own library.
        let (dir, start) = three_images();
        let images = [
            at("/a", Some((0, 2))),
            at("/b", Some((4, 6))),
            at("/a", Some((8, 9))),
            at("/c", Some((12, 20))),
        ];
        let profile = profile(Vec::new(), 0, Vec::new());
        let mut symbolizer = Symbolizer::new(&profile, None, Some(dir.path())).unwrap();
        let addr = 0x5555_0000_0000 + start;
        let mut name = |epoch| symbolizer.frame(addr, false, &images, Some(epoch));
        assert_eq!(name(1), "[a]", "before the reuse");
        assert_eq!(name(5), "[b]", "between the two loads of a");
        assert_eq!(name(9), "[a]", "A again, after B");
        assert_eq!(name(15), "[c]");
        // A batch no occurrence covers (nothing was mapped there in it) falls back to the
        // newest.
        assert_eq!(name(11), "[c]");
    }
}
