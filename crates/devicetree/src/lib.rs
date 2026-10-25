//! Borrowed, allocation-free access to firmware device trees.
//!
//! Uses the published `fdt` parser in its fallible, byte-aligned mode. This
//! boundary keeps its prerelease parser types out of kernel/bootloader APIs.
//! Property helpers implement the Devicetree Specification v0.4, sections 2.3
//! (standard properties), 3.6 (`chosen`) and 5 (flattened format).
//! Invalid optional property encodings return `None`; iterators stop at errors.
//!
//! # Examples
//!
//! ```
//! use ferric_devicetree::Tree;
//! assert!(Tree::new(&[]).is_err());
//! ```

#![no_std]
#![deny(missing_docs)]
#![forbid(unsafe_code)]

use core::{fmt, str};
use fdt::nodes::AsNode;
use fdt::{
    helpers::UnalignedFallibleNode,
    parsing::{NoPanic, unaligned::UnalignedParser},
};

/// Address and size cell counts, with the specification's default values.
pub use fdt::properties::cells::CellSizes;

type Parser<'a> = (UnalignedParser<'a>, NoPanic);

/// A failure to construct a device tree.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The header's blocks overflow, overlap, or lie outside the declared blob.
    Bounds,
    /// The published parser rejected the header, structure, or string table.
    Parse(fdt::FdtError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bounds => f.write_str("invalid device-tree block bounds"),
            Self::Parse(err) => err.fmt(f),
        }
    }
}

impl core::error::Error for Error {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            // The published `fdt` prerelease's `FdtError` implements only
            // `Debug` and `Display`, not `core::error::Error`, so it cannot be
            // returned as a source. The `Parse` variant still retains the inner
            // `fdt::FdtError` by value for typed diagnostics.
            Self::Bounds => None,
            Self::Parse(_) => None,
        }
    }
}

impl From<fdt::FdtError> for Error {
    fn from(value: fdt::FdtError) -> Self {
        Self::Parse(value)
    }
}

fn word(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        data.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn check_bounds(data: &[u8]) -> Option<&[u8]> {
    let total = usize::try_from(word(data, 4)?).ok()?;
    let data = data.get(..total)?;
    if total < 40 {
        return None;
    }
    let block = |offset, size| -> Option<core::ops::Range<usize>> {
        let start = usize::try_from(word(data, offset)?).ok()?;
        let end = start.checked_add(usize::try_from(word(data, size)?).ok()?)?;
        (start >= 40 && end <= total).then_some(start..end)
    };
    let structure = block(8, 36)?;
    let strings = block(12, 32)?;
    if structure.start % 4 != 0 || structure.start < strings.end && strings.start < structure.end {
        return None;
    }
    Some(data)
}

/// A validated tree borrowing its original binary blob.
#[derive(Clone, Copy, Debug)]
pub struct Tree<'a> {
    root: fdt::nodes::root::Root<'a, Parser<'a>>,
    data: &'a [u8],
}

impl<'a> Tree<'a> {
    /// Checks block bounds and walks the structure and property names.
    ///
    /// Property values and the reservation map are interpreted by consumers;
    /// constructing a tree does not validate every device binding.
    ///
    /// # Errors
    /// Returns [`Error::Bounds`] for invalid block extents or [`Error::Parse`]
    /// for a malformed header, tree structure or property name.
    pub fn new(data: &'a [u8]) -> Result<Self, Error> {
        // Check in usize before fdt's constructor adds offsets as u32.
        let data = check_bounds(data).ok_or(Error::Bounds)?;
        let raw = fdt::Fdt::new_unaligned_fallible(data)?;
        let root = raw.root()?;
        for property in root.as_node().properties()? {
            property?;
        }
        for entry in raw.all_nodes()? {
            let (_, node) = entry?;
            if node.name()?.name.contains('/') {
                return Err(fdt::FdtError::InvalidNodeName.into());
            }
            for property in node.properties()? {
                property?;
            }
        }
        Ok(Self { root, data })
    }

    fn wrap(self, raw: UnalignedFallibleNode<'a>) -> Option<Node<'a>> {
        let parsed = raw.name().ok()?;
        let name = if parsed.name == "/" {
            "/"
        } else {
            let start = (parsed.name.as_ptr() as usize).checked_sub(self.data.as_ptr() as usize)?;
            let len = parsed
                .name
                .len()
                .checked_add(parsed.unit_address.map_or(0, |unit| 1 + unit.len()))?;
            str::from_utf8(self.data.get(start..start.checked_add(len)?)?).ok()?
        };
        let identity = if name == "/" {
            0
        } else {
            (name.as_ptr() as usize).checked_sub(self.data.as_ptr() as usize)?
        };
        Some(Node {
            raw,
            tree: self,
            name,
            identity,
        })
    }

    /// The validated root node.
    pub fn root(&self) -> Node<'a> {
        Node {
            raw: self.root.as_node(),
            tree: *self,
            name: "/",
            identity: 0,
        }
    }
    /// The original blob, bounded to the header's declared total size.
    pub fn raw_data(&self) -> &'a [u8] {
        self.data
    }
    /// The declared blob size in bytes.
    pub fn total_size(&self) -> usize {
        self.data.len()
    }
    /// Finds an absolute node path, accepting omitted unit addresses.
    pub fn find_node(&self, path: &str) -> Option<Node<'a>> {
        self.wrap(self.root.find_node(path).ok()??)
    }
    /// Walks all nodes, including the root, in depth-first order without allocating.
    pub fn all_nodes(&self) -> impl Iterator<Item = Node<'a>> + use<'a> {
        let tree = *self;
        core::iter::once(tree.root()).chain(
            self.root
                .all_nodes()
                .ok()
                .into_iter()
                .flatten()
                .map_while(move |entry| {
                    let (_, raw) = entry.ok()?;
                    tree.wrap(raw)
                }),
        )
    }
    /// Finds the first node matching any supplied compatible string.
    pub fn find_compatible(&self, compatible: &[&str]) -> Option<Node<'a>> {
        self.all_nodes().find(|node| {
            node.compatible()
                .is_some_and(|list| compatible.iter().any(|s| list.all().any(|v| v == *s)))
        })
    }
    /// Translates a register region through every ancestor bus's `ranges`.
    /// Missing mappings, malformed ranges, crossings and overflow return `None`.
    pub fn translate_mmio_address(&self, device: Node<'_>, region: &MemoryRegion) -> Option<usize> {
        if !core::ptr::eq(self.data, device.tree.data) || device.same_node(self.root()) {
            return None;
        }
        let mut address = region.starting_address as usize;
        let size = region.size.unwrap_or(1);
        let mut current = device;
        while let Some(parent) = current.parent() {
            if parent.same_node(self.root()) {
                break;
            }
            let property = parent.property("ranges")?;
            if !property.value.is_empty() {
                address = parent.ranges()?.find_map(|range| {
                    let offset = address.checked_sub(range.child_bus_address)?;
                    if offset >= range.size || size.saturating_sub(1) >= range.size - offset {
                        return None;
                    }
                    range.parent_bus_address.checked_add(offset)
                })?;
            }
            current = parent;
        }
        address.checked_add(size.saturating_sub(1))?;
        Some(address)
    }

    /// Resolves a 32-bit phandle to its node.
    pub fn find_phandle(&self, phandle: u32) -> Option<Node<'a>> {
        self.wrap(
            self.root
                .resolve_phandle(fdt::properties::PHandle::new(phandle))
                .ok()??,
        )
    }
    /// Returns the `/chosen` properties, if present.
    pub fn chosen(&self) -> Chosen<'a> {
        Chosen { tree: *self }
    }
    /// Iterates CPU children of `/cpus` in firmware order.
    pub fn cpus(&self) -> impl Iterator<Item = Node<'a>> + use<'a> {
        self.find_node("/cpus")
            .into_iter()
            .flat_map(Node::children)
            .filter(|node| node.name.split('@').next() == Some("cpu"))
    }
}

/// A node whose names and properties borrow the tree's blob.
#[derive(Clone, Copy, Debug)]
pub struct Node<'a> {
    raw: UnalignedFallibleNode<'a>,
    tree: Tree<'a>,
    /// Full node name, including its unit address when present.
    pub name: &'a str,
    identity: usize,
}

impl<'a> Node<'a> {
    /// Looks up a raw property; malformed or missing properties return `None`.
    pub fn property(self, name: &str) -> Option<Property<'a>> {
        let p = self.raw.raw_property(name).ok()??;
        Some(Property {
            name: p.name,
            value: p.value,
        })
    }
    /// Iterates raw properties in firmware order.
    pub fn properties(self) -> impl Iterator<Item = Property<'a>> {
        self.raw
            .properties()
            .ok()
            .into_iter()
            .flatten()
            .map_while(|p| {
                let p = p.ok()?;
                Some(Property {
                    name: p.name,
                    value: p.value,
                })
            })
    }
    /// Iterates immediate child nodes.
    pub fn children(self) -> impl Iterator<Item = Self> {
        self.raw
            .children()
            .ok()
            .into_iter()
            .flatten()
            .map_while(move |node| self.tree.wrap(node.ok()?))
    }
    /// Returns the parent with its own ancestry retained. The upstream parser's
    /// direct parent view drops that ancestry, so resolve its borrowed name.
    pub fn parent(self) -> Option<Self> {
        let parent = self.tree.wrap(self.raw.parent()?)?;
        if parent.same_node(self.tree.root()) {
            return Some(self.tree.root());
        }
        self.tree.all_nodes().find(|node| node.same_node(parent))
    }
    /// Compares node identity within the same blob, independent of its name.
    pub fn same_node(self, other: Node<'_>) -> bool {
        // Root names are synthesized literals; their addresses need not be
        // pooled across codegen units. Other names occupy unique blob offsets.
        core::ptr::eq(self.tree.data, other.tree.data) && self.identity == other.identity
    }
    /// Returns each cell count independently, defaulting to two address cells
    /// and one size cell when its property is absent; malformed encodings return `None`.
    pub fn cell_sizes(self) -> Option<CellSizes> {
        let count = |name, default| match self.property(name) {
            None => Some(default),
            Some(p) if p.value.len() == 4 => p.as_usize(),
            Some(_) => None,
        };
        Some(CellSizes {
            address_cells: count("#address-cells", 2)?,
            size_cells: count("#size-cells", 1)?,
        })
    }
    /// Returns the validated compatible-string list.
    pub fn compatible(self) -> Option<Compatible<'a>> {
        let value = self.property("compatible")?.as_str()?;
        (!value.is_empty()).then_some(Compatible(value))
    }
    /// Returns the model string, or an empty string when it is unavailable.
    pub fn model(self) -> &'a str {
        self.property("model")
            .and_then(Property::as_str)
            .unwrap_or("")
    }
    /// Returns the interrupt specifier width of this controller.
    pub fn interrupt_cells(self) -> Option<usize> {
        self.property("#interrupt-cells")?.as_usize()
    }
    /// Resolves this node's explicit `interrupt-parent` phandle.
    pub fn interrupt_parent(self) -> Option<Self> {
        self.tree
            .find_phandle(u32::try_from(self.property("interrupt-parent")?.as_usize()?).ok()?)
    }
    /// Decodes complete CPU-addressable register regions. Invalid widths,
    /// truncated entries and addresses that do not fit usize return `None`.
    pub fn reg(self) -> Option<impl Iterator<Item = MemoryRegion> + 'a> {
        let sizes = self
            .parent()
            .map_or(Some(CellSizes::default()), Node::cell_sizes)?;
        let values = self.property("reg")?.value;
        let stride = tuple_stride(values, &[sizes.address_cells, sizes.size_cells], &[2, 2])?;
        if sizes.address_cells == 0 {
            return None;
        }
        Some(values.chunks_exact(stride).map_while(move |entry| {
            let split = sizes.address_cells * 4;
            let size = if sizes.size_cells == 0 {
                None
            } else {
                Some(usize::try_from(cells(&entry[split..])?).ok()?)
            };
            Some(MemoryRegion {
                starting_address: usize::try_from(cells(&entry[..split])?).ok()? as *const u8,
                size,
            })
        }))
    }
    /// Decodes child-to-parent bus ranges, retaining the high PCI address cell.
    /// Invalid widths or incomplete entries return `None`.
    pub fn ranges(self) -> Option<impl Iterator<Item = MemoryRange> + 'a> {
        let sizes = self.cell_sizes()?;
        let parent = self.parent()?.cell_sizes()?;
        let values = self.property("ranges")?.value;
        if sizes.address_cells == 0 || parent.address_cells == 0 || sizes.size_cells == 0 {
            return None;
        }
        let stride = tuple_stride(
            values,
            &[sizes.address_cells, parent.address_cells, sizes.size_cells],
            &[3, 2, 2],
        )?;
        Some(values.chunks_exact(stride).map_while(move |entry| {
            let a = sizes.address_cells * 4;
            let b = a + parent.address_cells * 4;
            let child = cells(&entry[..a])?;
            Some(MemoryRange {
                child_bus_address: usize::try_from(child & u128::from(u64::MAX)).ok()?,
                child_bus_address_hi: u32::try_from(child >> 64).ok()?,
                parent_bus_address: usize::try_from(cells(&entry[a..b])?).ok()?,
                size: usize::try_from(cells(&entry[b..])?).ok()?,
            })
        }))
    }
}

fn tuple_stride(values: &[u8], widths: &[usize], maximums: &[usize]) -> Option<usize> {
    let mut stride = 0usize;
    for (&width, &max) in widths.iter().zip(maximums) {
        if width > max {
            return None;
        }
        stride = stride.checked_add(width.checked_mul(4)?)?;
    }
    (stride != 0 && values.len().is_multiple_of(stride)).then_some(stride)
}

fn cells(bytes: &[u8]) -> Option<u128> {
    if bytes.len() > 12 || !bytes.len().is_multiple_of(4) {
        return None;
    }
    Some(
        bytes
            .iter()
            .fold(0u128, |n, byte| (n << 8) | u128::from(*byte)),
    )
}

/// A property borrowing its original name and bytes.
#[derive(Clone, Copy, Debug)]
pub struct Property<'a> {
    /// Property name.
    pub name: &'a str,
    /// Encoded property bytes.
    pub value: &'a [u8],
}

impl<'a> Property<'a> {
    /// Decodes exactly one or two big-endian cells into a machine integer.
    pub fn as_usize(self) -> Option<usize> {
        if !matches!(self.value.len(), 4 | 8) {
            return None;
        }
        usize::try_from(cells(self.value)?).ok()
    }
    /// Decodes a NUL-terminated UTF-8 string or string list, removing the terminator.
    pub fn as_str(self) -> Option<&'a str> {
        str::from_utf8(self.value.strip_suffix(&[0])?).ok()
    }
    /// Iterates a valid NUL-terminated string list.
    pub fn iter_str(self) -> impl Iterator<Item = &'a str> {
        self.as_str().into_iter().flat_map(|s| s.split('\0'))
    }
}

/// A nonempty list of compatible identifiers.
#[derive(Clone, Copy, Debug)]
pub struct Compatible<'a>(&'a str);
impl<'a> Compatible<'a> {
    /// The first compatible identifier.
    pub fn first(self) -> &'a str {
        self.0.split('\0').next().unwrap_or("")
    }
    /// All compatible identifiers in firmware order.
    pub fn all(self) -> impl Iterator<Item = &'a str> {
        self.0.split('\0')
    }
}

/// Console and boot argument properties from `/chosen`.
pub struct Chosen<'a> {
    tree: Tree<'a>,
}
impl<'a> Chosen<'a> {
    /// Resolves `stdout-path`, including aliases and optional parameters.
    pub fn stdout(self) -> Option<Console<'a>> {
        let chosen = self.tree.root.chosen().ok()?;
        let out = chosen.stdout().ok()??;
        Some(Console {
            node: self.tree.wrap(out.node)?,
            params: out.params,
        })
    }
    /// The kernel command line, if supplied.
    pub fn bootargs(self) -> Option<&'a str> {
        self.tree
            .find_node("/chosen")?
            .property("bootargs")?
            .as_str()
    }
}

/// A resolved console node and its optional configuration suffix.
pub struct Console<'a> {
    node: Node<'a>,
    params: Option<&'a str>,
}
impl<'a> Console<'a> {
    /// The UART or other device selected by firmware.
    pub fn node(&self) -> Node<'a> {
        self.node
    }
    /// Parameters following the colon in the console path.
    pub fn params(&self) -> Option<&'a str> {
        self.params
    }
}

/// A CPU-addressable register region.
#[derive(Clone, Copy, Debug)]
pub struct MemoryRegion {
    /// Physical or bus-relative starting address.
    pub starting_address: *const u8,
    /// Region length, absent when `#size-cells` is zero.
    pub size: Option<usize>,
}

/// A range mapping a child bus into its parent address space.
#[derive(Clone, Copy, Debug)]
pub struct MemoryRange {
    /// Low 64 bits of the child address, fitting in usize.
    pub child_bus_address: usize,
    /// High PCI address cell, or zero for ordinary buses.
    pub child_bus_address_hi: u32,
    /// Starting address in the parent bus.
    pub parent_bus_address: usize,
    /// Range length in bytes.
    pub size: usize,
}

#[cfg(test)]
mod tests;
