//! Ferric Oxide: how many bytes a 16550-compatible UART's transmitter takes between reads of
//! its line status, kept apart from the port access so the host tests can check it.
//!
//! Under KVM every access to an emulated UART is a VM exit, so reading the line status before
//! every byte doubled the cost of console output. A 16550A whose FIFOs are on takes 16 bytes
//! once it reports its transmitter empty (line status bit 5). That holds for the PC's
//! port-I/O COM ports, which are 16550As. Memory-mapped UARTs such as the DesignWare APB UART
//! (LPSS, `snps,dw-apb-uart`) report the same interrupt identification bits but may be
//! configured with a smaller FIFO, which those bits do not reveal, so they keep one byte a
//! poll.

/// The transmit FIFO of a 16550A.
pub const TX_FIFO_BYTES: u8 = 16;

/// The interrupt identification register's bits 6 and 7 (read at the FIFO control port),
/// both set when the FIFOs are enabled and work (a 16550A); a 16450 has none, and a 16550
/// with the broken FIFO sets only bit 7.
pub const IIR_FIFOS_ENABLED: u8 = 0xC0;

/// How the UART is reached, which decides whether its FIFO depth is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortKind {
    /// A PC COM port through port I/O: a 16550A when its FIFOs work.
    PortIo,
    /// A memory-mapped UART, whose FIFO depth the IIR does not give.
    Mmio,
}

/// The bytes the transmitter takes after it reports empty, from the IIR read after enabling
/// the FIFOs.
pub const fn tx_burst(kind: PortKind, iir: u8) -> u8 {
    match kind {
        PortKind::PortIo if iir & IIR_FIFOS_ENABLED == IIR_FIFOS_ENABLED => TX_FIFO_BYTES,
        PortKind::PortIo | PortKind::Mmio => 1,
    }
}

/// When a sender must read the line status: before the first byte, and again once it has
/// written as many bytes as the transmitter takes after reporting empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TxCredit {
    /// Bytes the transmitter takes after it reports empty.
    burst: u8,
    /// Bytes that can still be written without reading the line status.
    room: u8,
}

impl TxCredit {
    /// One byte a poll, until [`TxCredit::set_burst`]: right for any UART.
    pub const fn new() -> Self {
        Self { burst: 1, room: 0 }
    }

    /// Takes `burst` bytes after each empty report (at least 1), and waits for one before the
    /// next byte, as after writing to the transmitter outside [`TxCredit::sent`].
    pub fn set_burst(&mut self, burst: u8) {
        self.burst = burst.max(1);
        self.room = 0;
    }

    /// Whether the line status must say empty before the next byte.
    pub const fn must_poll(&self) -> bool {
        self.room == 0
    }

    /// Records that the line status said empty.
    pub fn emptied(&mut self) {
        self.room = self.burst;
    }

    /// Records one byte written.
    pub fn sent(&mut self) {
        self.room = self.room.saturating_sub(1);
    }
}

impl Default for TxCredit {
    fn default() -> Self {
        Self::new()
    }
}
