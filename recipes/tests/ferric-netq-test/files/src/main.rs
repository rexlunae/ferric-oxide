//! Kernel-bypass queue sockets in copy mode: on-target test for Ferric Oxide
//! (`drivers.net_bypass`, io/netbypass-design.md in the project files, PR 2).
//!
//! The program opens a queue socket on the network driver (`/scheme/network.<nic>/queue`),
//! registers a UMEM and a ring region in a `kernel.regbuf` region, and checks each step:
//!
//! - `netq-netstack`: netstack answers an ICMP echo to the gateway before any socket exists,
//!   which the later steps compare against.
//! - `netq-attach`: the driver maps the UMEM grant and the ring grant (`Attach`).
//! - `netq-checks`: the driver refuses `AddFilter` and `Stats` before `Bind` (`ENOTCONN`), a
//!   zero-copy `Bind` (`ENOSPC`, copy mode only), a second `Bind` (`EBUSY`), an unknown
//!   filter ID (`ENOENT`) and a short `Stats` payload (`EINVAL`), and `Bind` with
//!   `Mode::Auto` reports copy mode.
//! - `netq-arp`: with an ARP filter, the test sends an ARP request for the gateway through its
//!   TX ring and receives the reply on its RX ring, byte for byte as the gateway sent it.
//! - `netq-steer`: while the socket takes ARP, netstack still gets every other frame, so its
//!   ICMP echo still works.
//! - `netq-ping`: with the ARP filter removed and an ICMP filter for the gateway added, an echo
//!   request the test builds and sends itself is answered on its RX ring.
//! - `netq-stats`: the driver's counters agree with what the test sent and received, no
//!   descriptor was invalid, and no frame was dropped (Fill empty or RX overflow).
//! - `netq-own`: a second socket, bound without raw TX, owns a UDP port through a netstack
//!   socket bound to the interface's address; filters on unowned ports or on the owned port at
//!   another address, an `Own` with a file and an `Own` with a socket bound to `0.0.0.0` are
//!   refused.
//! - `netq-udp`: a DNS query and its answer through the owned port.
//! - `netq-spoof`: frames from a foreign source MAC, an unowned port, the owned port at another
//!   source address, and ICMP are dropped and counted.
//! - `netq-tx-bounds`: TX descriptors outside the UMEM (past its end, not at a frame's start,
//!   longer than a frame, with a reserved flag) are counted as invalid and never sent.
//! - `netq-bench`: 60-byte UDP frames through the socket and through netstack
//!   (`FERRIC_BENCH netq-tx-udp60`).
//! - `netq-latency`: ICMP echo round trips to the gateway through the socket and through
//!   netstack's `icmp` scheme, p50 and p99 of each (`FERRIC_BENCH netq-echo-rtt`).
//! - `netq-close`: closing the socket with requests still in flight leaves the driver serving
//!   netstack, which answers an echo again; a second socket then opened on fresh grants of the
//!   same memory binds, and an echo the test sends through it is answered on its RX ring.
//!
//! The application's side of the rings is `ferric_netq::ring::AppRings`, and the frames go in
//! and out of the UMEM with `ferric_netq::copy`'s atomic copies, as the driver's do. Frames are
//! built and parsed with `etherparse` (RFC 826 ARP, RFC 792 ICMP echo).
//!
//! Each step prints `FERRIC_TEST_PASS <name>` or `FERRIC_TEST_FAIL <name>: <why>`; the run
//! stops at the first failure and exits non-zero. Without a network driver, a `kernel.regbuf`
//! scheme or queue sockets in the driver (`drivers.net_bypass`), it prints
//! `FERRIC_TEST_SKIP netq: <why>` and exits zero.

#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(target_os = "redox")]
mod redox;

fn main() -> std::process::ExitCode {
    #[cfg(target_os = "redox")]
    {
        redox::main()
    }
    #[cfg(not(target_os = "redox"))]
    {
        println!("FERRIC_TEST_SKIP netq: only runs on Redox");
        std::process::ExitCode::SUCCESS
    }
}
