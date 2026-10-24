//! Descriptor command words for the RTL8125's legacy 16-byte descriptor format.
//!
//! The RTL8125B can operate with either its new descriptor format or the
//! RTL8168 format. The driver clears bit 0 of MAC OCP register `0xEB58` during
//! initialisation, which selects the legacy format described here. Each
//! descriptor is four 32-bit words: command, VLAN/status, buffer low and buffer
//! high.

/// Size of one descriptor in bytes.
pub const DESCRIPTOR_SIZE: usize = 16;

/// The controller owns the descriptor.
pub const OWN: u32 = 1 << 31;
/// Last descriptor in the ring.
pub const EOR: u32 = 1 << 30;
/// First descriptor of a frame (transmit).
pub const SOF: u32 = 1 << 29;
/// Last descriptor of a frame (transmit).
pub const EOF: u32 = 1 << 28;

/// Receive length mask in the command/status word.
pub const RX_LEN_MASK: u32 = 0x0000_3FFF;
/// Transmit length mask in the command word.
pub const TX_LEN_MASK: u32 = 0x0000_FFFF;

/// Build the command word that hands a receive buffer to the controller.
///
/// The length is the size of the buffer, not the size of a received frame.
#[must_use]
pub const fn rx_command(buffer_len: usize, end_of_ring: bool) -> u32 {
    let mut command = OWN | (buffer_len as u32 & RX_LEN_MASK);
    if end_of_ring {
        command |= EOR;
    }
    command
}

/// Build the command word that queues one complete frame for transmission.
#[must_use]
pub const fn tx_command(frame_len: usize, end_of_ring: bool) -> u32 {
    let mut command = OWN | SOF | EOF | (frame_len as u32 & TX_LEN_MASK);
    if end_of_ring {
        command |= EOR;
    }
    command
}

/// Whether the controller still owns a descriptor.
#[must_use]
pub const fn is_owned(command: u32) -> bool {
    command & OWN != 0
}

/// Length reported in a receive descriptor.
#[must_use]
pub const fn rx_length(command: u32) -> usize {
    (command & RX_LEN_MASK) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receive_command_sets_owner_length_and_ring_end() {
        assert_eq!(rx_command(2048, false), OWN | 2048);
        assert_eq!(rx_command(2048, true), OWN | EOR | 2048);
        assert!(is_owned(rx_command(2048, false)));
    }

    #[test]
    fn transmit_command_marks_a_complete_frame() {
        let command = tx_command(1514, true);
        assert_eq!(command & (OWN | SOF | EOF | EOR), OWN | SOF | EOF | EOR);
        assert_eq!(command & TX_LEN_MASK, 1514);
    }

    #[test]
    fn receive_length_ignores_status_bits() {
        assert_eq!(rx_length(OWN | EOR | SOF | EOF | 0x123), 0x123);
    }
}
