//! Короткий STUN Binding, чтобы узнать внешний адрес сокета.
//! Нужен только не на localhost: сервер ICE-lite сам кандидатов не ищет.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAGIC: u32 = 0x2112_A442;
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const XOR_MAPPED_ADDRESS: u16 = 0x0020;

pub fn lookup(socket: &UdpSocket) -> Option<SocketAddr> {
    let request = binding_request();
    let stun = ("stun.l.google.com", 19302)
        .to_socket_addrs_v4()
        .into_iter()
        .next()?;
    socket.send_to(&request, stun).ok()?;
    socket.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    let mut buf = [0u8; 512];
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        match socket.recv_from(&mut buf) {
            Ok((size, from)) if from.ip() == stun.ip() => {
                if let Some(addr) = parse_binding(&buf[..size], &request[8..20]) {
                    return Some(addr);
                }
            }
            Ok(_) => {}
            Err(err)
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut =>
            {
                break;
            }
            Err(_) => break,
        }
    }
    None
}

fn binding_request() -> [u8; 20] {
    let mut packet = [0u8; 20];
    packet[0..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    packet[4..8].copy_from_slice(&MAGIC.to_be_bytes());
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    packet[8..16].copy_from_slice(&nanos.to_le_bytes()[..8]);
    packet[16..20].copy_from_slice(&std::process::id().to_le_bytes());
    packet
}

pub(crate) fn parse_binding(packet: &[u8], txid: &[u8]) -> Option<SocketAddr> {
    if packet.len() < 20 || txid.len() != 12 {
        return None;
    }
    let kind = u16::from_be_bytes([packet[0], packet[1]]);
    if kind != BINDING_SUCCESS {
        return None;
    }
    let magic = u32::from_be_bytes(packet[4..8].try_into().ok()?);
    if magic != MAGIC || packet[8..20] != *txid {
        return None;
    }
    let mut offset = 20;
    while offset + 4 <= packet.len() {
        let attr = u16::from_be_bytes([packet[offset], packet[offset + 1]]);
        let len = u16::from_be_bytes([packet[offset + 2], packet[offset + 3]]) as usize;
        let start = offset + 4;
        let end = start.checked_add(len)?;
        if end > packet.len() {
            return None;
        }
        if attr == XOR_MAPPED_ADDRESS {
            return xor_mapped(&packet[start..end]);
        }
        let padded = len + (4 - len % 4) % 4;
        offset = start + padded;
    }
    None
}

fn xor_mapped(value: &[u8]) -> Option<SocketAddr> {
    if value.len() < 8 || value[1] != 0x01 {
        return None;
    }
    let port = u16::from_be_bytes([value[2], value[3]]) ^ ((MAGIC >> 16) as u16);
    let ip = u32::from_be_bytes(value[4..8].try_into().ok()?) ^ MAGIC;
    Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip)), port))
}

trait ToSocketAddrsV4 {
    fn to_socket_addrs_v4(self) -> Vec<SocketAddr>;
}

impl ToSocketAddrsV4 for (&'static str, u16) {
    fn to_socket_addrs_v4(self) -> Vec<SocketAddr> {
        use std::net::ToSocketAddrs;
        self.to_socket_addrs()
            .ok()
            .into_iter()
            .flatten()
            .filter(|addr| addr.is_ipv4())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_xor_mapped_ipv4() {
        let txid = [9u8; 12];
        let mut packet = vec![0u8; 20];
        packet[0..2].copy_from_slice(&BINDING_SUCCESS.to_be_bytes());
        packet[4..8].copy_from_slice(&MAGIC.to_be_bytes());
        packet[8..20].copy_from_slice(&txid);
        let port = 9000u16 ^ ((MAGIC >> 16) as u16);
        let ip = u32::from(Ipv4Addr::new(203, 0, 113, 10)) ^ MAGIC;
        packet.extend_from_slice(&XOR_MAPPED_ADDRESS.to_be_bytes());
        packet.extend_from_slice(&8u16.to_be_bytes());
        packet.extend_from_slice(&[0, 0x01]);
        packet.extend_from_slice(&port.to_be_bytes());
        packet.extend_from_slice(&ip.to_be_bytes());
        let parsed = parse_binding(&packet, &txid).unwrap();
        assert_eq!(
            parsed,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10)), 9000)
        );
    }
}
