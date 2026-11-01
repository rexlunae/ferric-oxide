//! A single-block TFTP exchange for the owned-UDP test, following RFC 1350:
//! <https://www.rfc-editor.org/rfc/rfc1350.html>.
//!
//! This is a fixture probe, not a general TFTP client. It requests octet mode with
//! no options and accepts only the exact fixture in DATA block 1, from the gateway
//! to the test's address and owned port. The final ACK uses the server's chosen
//! transfer port. A fixture shorter than 512 bytes finishes in that one block.

use std::io;
use std::net::Ipv4Addr;

use etherparse::{NetSlice, SlicedPacket, TransportSlice};
use simple_endian::{BigEndian, Endianize, read_native, write_native, write_specific};

/// The well-known TFTP port used for the initial read request.
pub(crate) const SERVER_PORT: u16 = 69;
const FILE_NAME: &[u8] = b"ferric-netq.txt\0";
const CONTENTS: &[u8] = include_bytes!("../fixtures/ferric-netq.txt");
const _: () = assert!(CONTENTS.len() < 512);

/// The common DATA/ACK header, with network byte order supplied by Endianize.
#[derive(Endianize)]
#[endian(be)]
#[repr(C)]
struct BlockHeader {
    opcode: u16,
    block: u16,
}

/// An octet-mode read request for the fixture.
///
/// # Errors
/// Returns an I/O error if the endian codec cannot write the request header.
pub(crate) fn request() -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    write_specific(&mut bytes, &BigEndian::from(1_u16))?;
    bytes.extend_from_slice(FILE_NAME);
    bytes.extend_from_slice(b"octet\0");
    Ok(bytes)
}

/// The server's transfer port if this frame contains the complete expected reply.
pub(crate) fn answer(frame: &[u8], server: Ipv4Addr, client: Ipv4Addr, port: u16) -> Option<u16> {
    let packet = SlicedPacket::from_ethernet(frame).ok()?;
    let Some(NetSlice::Ipv4(ip)) = packet.net else {
        return None;
    };
    if ip.header().source() != server.octets() || ip.header().destination() != client.octets() {
        return None;
    }
    let Some(TransportSlice::Udp(udp)) = packet.transport else {
        return None;
    };
    if udp.destination_port() != port || udp.source_port() == 0 {
        return None;
    }
    let mut payload = udp.payload();
    let header: BlockHeader = read_native::<_, BlockHeaderWire, _>(&mut payload).ok()?;
    (header.opcode == 3 && header.block == 1 && payload == CONTENTS).then_some(udp.source_port())
}

/// The final acknowledgment for DATA block 1.
///
/// # Errors
/// Returns an I/O error if the endian codec cannot write the acknowledgment header.
pub(crate) fn acknowledgment() -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    write_native::<_, BlockHeaderWire, _>(
        &mut bytes,
        BlockHeader {
            opcode: 4,
            block: 1,
        },
    )?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use etherparse::PacketBuilder;

    use super::*;

    const SERVER: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
    const CLIENT: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
    const CLIENT_PORT: u16 = 47353;

    fn frame(server: Ipv4Addr, client: Ipv4Addr, from: u16, to: u16, data: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        PacketBuilder::ethernet2([2; 6], [4; 6])
            .ipv4(server.octets(), client.octets(), 64)
            .udp(from, to)
            .write_to_vec(&mut bytes, data)
            .unwrap();
        bytes
    }

    fn data() -> Vec<u8> {
        // RFC 1350 DATA, block 1, followed by the exact file bytes.
        let mut bytes = vec![0, 3, 0, 1];
        bytes.extend_from_slice(CONTENTS);
        bytes
    }

    #[test]
    fn request_and_ack_match_rfc_byte_fixtures() {
        assert_eq!(request().unwrap(), b"\0\x01ferric-netq.txt\0octet\0");
        assert_eq!(acknowledgment().unwrap(), [0, 4, 0, 1]);
        assert_eq!(size_of::<BlockHeaderWire>(), 4);
    }

    #[test]
    fn accepts_exact_data_and_preserves_the_server_transfer_port() {
        for transfer_port in [SERVER_PORT, 32768] {
            let frame = frame(SERVER, CLIENT, transfer_port, CLIENT_PORT, &data());
            assert_eq!(
                answer(&frame, SERVER, CLIENT, CLIENT_PORT),
                Some(transfer_port)
            );
        }
    }

    #[test]
    fn rejects_other_addresses_and_ports() {
        let other = Ipv4Addr::new(10, 0, 2, 3);
        for (server, client, from, to) in [
            (other, CLIENT, SERVER_PORT, CLIENT_PORT),
            (SERVER, other, SERVER_PORT, CLIENT_PORT),
            (SERVER, CLIENT, SERVER_PORT, CLIENT_PORT + 1),
            (SERVER, CLIENT, 0, CLIENT_PORT),
        ] {
            assert_eq!(
                answer(
                    &frame(server, client, from, to, &data()),
                    SERVER,
                    CLIENT,
                    CLIENT_PORT
                ),
                None
            );
        }
    }

    #[test]
    fn rejects_wrong_opcodes_blocks_contents_and_trailing_bytes() {
        let valid = data();
        for index in [0, 1, 2, 3, 4, valid.len() - 1] {
            let mut bytes = valid.clone();
            bytes[index] ^= 1;
            assert_eq!(
                answer(
                    &frame(SERVER, CLIENT, SERVER_PORT, CLIENT_PORT, &bytes),
                    SERVER,
                    CLIENT,
                    CLIENT_PORT
                ),
                None
            );
        }
        let mut extended = valid;
        extended.push(0);
        assert_eq!(
            answer(
                &frame(SERVER, CLIENT, SERVER_PORT, CLIENT_PORT, &extended),
                SERVER,
                CLIENT,
                CLIENT_PORT
            ),
            None
        );
    }

    #[test]
    fn rejects_truncated_headers_and_data() {
        let bytes = data();
        for len in 0..bytes.len() {
            assert_eq!(
                answer(
                    &frame(SERVER, CLIENT, SERVER_PORT, CLIENT_PORT, &bytes[..len]),
                    SERVER,
                    CLIENT,
                    CLIENT_PORT
                ),
                None
            );
        }
        let frame = frame(SERVER, CLIENT, SERVER_PORT, CLIENT_PORT, &bytes);
        for len in 0..frame.len() {
            assert_eq!(answer(&frame[..len], SERVER, CLIENT, CLIENT_PORT), None);
        }
    }
}
