//! The ABI of Ferric Oxide's kernel-bypass network queues (roadmap Phase 2.5, design in
//! `io/netbypass-design.md` in the project files).
//!
//! An application opens a *queue socket* on a network driver, registers a region of its own
//! memory (the *UMEM*) split into fixed-size frames, and exchanges frames with the driver
//! through four rings in shared memory: Fill and TX from the application, RX and Completion
//! from the driver. Steering filters say which received frames go to the socket instead of
//! netstack. The driver either programs the filters into the NIC and gives the socket its own
//! hardware queue (zero-copy mode), or matches frames in software and copies them (copy mode).
//!
//! This crate is the one definition the drivers, `driver-network` and applications share:
//!
//! - [`umem`]: the UMEM's frame geometry and the check that a frame offset is one of its
//!   frames.
//! - [`desc`]: the ring entries (8-byte frame entries for Fill and Completion, 16-byte packet
//!   descriptors for RX and TX), decoded and checked against the UMEM, since neither side
//!   trusts the other's entries.
//! - [`frame`]: the header fields of a received Ethernet frame that filters match on.
//! - [`filter`]: steering filters, their validation, their fixed-size encoding and the software
//!   match copy mode uses.
//! - [`call`]: the queue socket's path, its `SYS_CALL` requests (within the three metadata
//!   words the kernel passes a scheme) and the bind and statistics records.
//! - [`ring`]: the four rings' layout in their shared region, and each side's handles, on
//!   `ferric-ring`'s index protocol, hostile-peer checks and need-wakeup.
//! - [`copy`]: the driver's side of a socket in copy mode: software filters, and copies
//!   between received or sent frames and the UMEM, with the socket's counters.
//!
//! Nothing here touches hardware, and the only memory it touches is ring and UMEM memory,
//! through atomic accesses.
//!
//! All multi-byte integers in shared memory and call payloads are little-endian; addresses and
//! MAC addresses are octets in network order. Frame formats follow IEEE 802.3 (Ethernet II),
//! IEEE 802.1Q (VLAN tags), RFC 791 (IPv4), RFC 8200 (IPv6), RFC 768 (UDP) and RFC 9293 (TCP),
//! parsed with the `etherparse` crate.
//!
//! # Examples
//!
//! ```
//! use ferric_netq::desc::Packet;
//! use ferric_netq::filter::{Filter, FilterSpec, MatchSpec};
//! use ferric_netq::frame::Headers;
//! use ferric_netq::umem::{FrameSize, Umem};
//!
//! // A 1 MiB UMEM of 2 KiB frames.
//! let umem = Umem::new(1 << 20, FrameSize::Bytes2048).expect("a valid UMEM");
//! assert_eq!(umem.frames(), 512);
//!
//! // Steer UDP to port 7000 to the socket.
//! let spec = FilterSpec::Match(MatchSpec {
//!     ip_protocol: Some(17),
//!     dst_port: Some(7000),
//!     ..MatchSpec::default()
//! });
//! let filter = Filter::new(spec).expect("a valid filter");
//! assert_eq!(Filter::from_bytes(&filter.to_bytes()), Ok(filter));
//!
//! // A UDP frame to port 7000 matches.
//! let mut frame = [0u8; 42];
//! frame[0..6].copy_from_slice(&[2, 0, 0, 0, 0, 1]); // destination MAC
//! frame[12..14].copy_from_slice(&[0x08, 0x00]); // IPv4
//! frame[14] = 0x45; // version 4, 5-word header
//! frame[16..18].copy_from_slice(&28u16.to_be_bytes()); // total length
//! frame[22] = 64; // TTL
//! frame[23] = 17; // UDP
//! frame[26..30].copy_from_slice(&[10, 0, 0, 1]);
//! frame[30..34].copy_from_slice(&[10, 0, 0, 2]);
//! frame[34..36].copy_from_slice(&5000u16.to_be_bytes());
//! frame[36..38].copy_from_slice(&7000u16.to_be_bytes());
//! frame[38..40].copy_from_slice(&8u16.to_be_bytes());
//! let headers = Headers::parse(&frame).expect("a well-formed frame");
//! assert!(filter.matches(&headers));
//!
//! // The driver publishes it on the RX ring as a packet descriptor.
//! let slot = umem.frame(3).expect("frame 3");
//! let rx = Packet::new(&umem, slot, 42).expect("fits the frame");
//! assert_eq!(Packet::from_bytes(rx.to_bytes(), &umem), Ok(rx));
//! ```

#![no_std]
#![deny(missing_docs)]
#![forbid(unsafe_code)]

pub mod call;
pub mod copy;
pub mod desc;
pub mod filter;
pub mod frame;
pub mod ring;
pub mod umem;
