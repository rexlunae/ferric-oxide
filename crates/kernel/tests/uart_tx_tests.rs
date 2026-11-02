//! The 16550 driver's transmit pacing (`devices/uart_tx.rs`): which UARTs take 16-byte
//! bursts, and on which bytes the line status is read.

use crate::uart_tx::{tx_burst, PortKind, TxCredit, TX_FIFO_BYTES};

/// The bytes, out of `count` sent with `credit`, before which the line status is read.
fn polls(mut credit: TxCredit, count: usize) -> Vec<usize> {
    let mut polled = Vec::new();
    for byte in 0..count {
        if credit.must_poll() {
            polled.push(byte);
            credit.emptied();
        }
        credit.sent();
    }
    polled
}

#[test]
fn only_a_port_io_16550a_with_working_fifos_takes_bursts() {
    assert_eq!(tx_burst(PortKind::PortIo, 0xC1), TX_FIFO_BYTES);
    // A 16450 (no FIFO) and a 16550 with the broken FIFO (bit 7 only).
    assert_eq!(tx_burst(PortKind::PortIo, 0x01), 1);
    assert_eq!(tx_burst(PortKind::PortIo, 0x81), 1);
    // A memory-mapped UART may have a smaller FIFO than the IIR suggests.
    assert_eq!(tx_burst(PortKind::Mmio, 0xC1), 1);
}

#[test]
fn a_burst_reads_the_line_status_once_every_16_bytes() {
    let mut credit = TxCredit::new();
    credit.set_burst(TX_FIFO_BYTES);
    assert_eq!(polls(credit, 40), [0, 16, 32]);
}

#[test]
fn before_init_and_without_a_fifo_every_byte_is_polled() {
    assert_eq!(polls(TxCredit::new(), 5), [0, 1, 2, 3, 4]);
    let mut credit = TxCredit::new();
    credit.set_burst(1);
    assert_eq!(polls(credit, 3), [0, 1, 2]);
    // A burst of 0 cannot stop output: it is treated as 1.
    credit.set_burst(0);
    assert_eq!(polls(credit, 3), [0, 1, 2]);
}

#[test]
fn setting_the_burst_waits_for_the_transmitter_to_empty_first() {
    let mut credit = TxCredit::new();
    credit.set_burst(TX_FIFO_BYTES);
    credit.emptied();
    credit.sent();
    credit.set_burst(TX_FIFO_BYTES);
    assert!(credit.must_poll());
}
