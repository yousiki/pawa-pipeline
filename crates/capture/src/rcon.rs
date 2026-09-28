//! A blocking Source RCON client, hand-rolled.
//!
//! The obvious dependency is the `rcon` crate, and it was rejected: 0.6 is async-only (its own
//! summary is "Asynchronous API for the RCON protocol"), so using it would pull tokio or async-std
//! into a crate that is otherwise threads-and-polling by design — the shape the coordinator has
//! always had. The protocol it would provide is the 60 lines below.
//!
//! What this deliberately does NOT do: multi-packet replies. Vanilla splits a long response into
//! 4096-byte packets, and this client reads exactly one packet per command. Every command the
//! coordinator issues (`list`, `time query gametime`, `data get entity`, `fill`, `locate biome`)
//! replies in one packet, so reading one is enough; a reader that concatenated until an empty
//! sentinel would change what a long `list` returns.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use anyhow::{Context, Result, bail};

/// Packet types from the Source RCON spec: 3 authenticates, 2 executes a command.
const TYPE_AUTH: i32 = 3;
const TYPE_EXEC: i32 = 2;
/// Request ids we tag our two packets with. The server echoes them back, and an auth reply of -1
/// means the password was rejected.
const ID_AUTH: i32 = 1;
const ID_EXEC: i32 = 2;

/// Largest reply we will allocate for. The protocol caps a packet at ~4 KiB of payload; the ceiling
/// is generous rather than exact, and exists so a desynchronized stream cannot ask for gigabytes.
const MAX_PACKET: i32 = 64 * 1024;

fn frame(request_id: i32, kind: i32, body: &str) -> Vec<u8> {
    let mut payload = Vec::with_capacity(body.len() + 10);
    payload.extend_from_slice(&request_id.to_le_bytes());
    payload.extend_from_slice(&kind.to_le_bytes());
    payload.extend_from_slice(body.as_bytes());
    payload.extend_from_slice(&[0, 0]); // body terminator + empty trailing string
    let mut out = (payload.len() as i32).to_le_bytes().to_vec();
    out.extend_from_slice(&payload);
    out
}

fn read_bytes(stream: &mut TcpStream, size: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; size];
    stream.read_exact(&mut buf).context("short RCON packet")?;
    Ok(buf)
}

/// Read one packet: `(request_id, type, body)`.
fn read_packet(stream: &mut TcpStream) -> Result<(i32, i32, String)> {
    let size = i32::from_le_bytes(read_bytes(stream, 4)?.try_into().expect("4 bytes"));
    if !(10..=MAX_PACKET).contains(&size) {
        bail!("implausible RCON packet size {size}");
    }
    let data = read_bytes(stream, size as usize)?;
    let request_id = i32::from_le_bytes(data[0..4].try_into().expect("4 bytes"));
    let kind = i32::from_le_bytes(data[4..8].try_into().expect("4 bytes"));
    // The trailing two bytes are the body terminator and the empty second string.
    let body = String::from_utf8_lossy(&data[8..data.len() - 2]).into_owned();
    Ok((request_id, kind, body))
}

/// Authenticate and run one command on a fresh connection, returning the reply body.
///
/// One connection per command: the coordinator issues a few hundred
/// commands across an episode, spaced by sleeps, and a pooled connection would add a keep-alive
/// failure mode to a path whose entire job is to be boring.
pub(crate) fn exec(
    address: SocketAddr,
    password: &str,
    command: &str,
    timeout: Duration,
) -> Result<String> {
    let mut stream = TcpStream::connect_timeout(&address, timeout)
        .with_context(|| format!("connect {address}"))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.write_all(&frame(ID_AUTH, TYPE_AUTH, password))?;
    let (auth_id, _, _) = read_packet(&mut stream)?;
    if auth_id == -1 {
        bail!("RCON authentication failed");
    }
    stream.write_all(&frame(ID_EXEC, TYPE_EXEC, command))?;
    let (response_id, _, body) = read_packet(&mut stream)?;
    if response_id != ID_EXEC {
        bail!("unexpected RCON response id {response_id}");
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Byte-for-byte against the Source RCON wire layout: little-endian `i32` length, `i32` request
    /// id, `i32` type, the body, then two NUL bytes.
    #[test]
    fn a_frame_matches_the_rcon_wire_layout_byte_for_byte() {
        let bytes = frame(2, TYPE_EXEC, "list");
        assert_eq!(
            bytes,
            [
                14, 0, 0, 0, // payload length: 4 + 4 + 4 + 2
                2, 0, 0, 0, // request id
                2, 0, 0, 0, // type
                b'l', b'i', b's', b't', 0, 0,
            ]
        );
    }
}
