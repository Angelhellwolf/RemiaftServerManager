use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

const SERVERDATA_AUTH: i32 = 3;
const SERVERDATA_AUTH_RESPONSE: i32 = 2;
const SERVERDATA_EXECCOMMAND: i32 = 2;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

pub fn send_command(host: &str, port: u16, password: &str, command: &str) -> Result<String> {
    let mut client = RconClient::connect(host, port, password)?;
    client.command(command)
}

pub fn send_command_no_response(
    host: &str,
    port: u16,
    password: &str,
    command: &str,
) -> Result<()> {
    let mut client = RconClient::connect(host, port, password)?;
    client.command_no_response(command)
}

pub fn configure_server_properties(
    server_dir: &Path,
    rcon_port: u16,
    password: &str,
) -> Result<()> {
    fs::create_dir_all(server_dir)
        .with_context(|| format!("create server directory {}", server_dir.display()))?;
    let path = server_dir.join("server.properties");
    upsert_properties(
        &path,
        &[
            ("enable-rcon", "true".to_string()),
            ("rcon.port", rcon_port.to_string()),
            ("rcon.password", password.to_string()),
        ],
    )
}

fn upsert_properties(path: &Path, entries: &[(&str, String)]) -> Result<()> {
    let raw = fs::read_to_string(path).unwrap_or_default();
    let mut lines = if raw.is_empty() {
        Vec::new()
    } else {
        raw.lines().map(ToString::to_string).collect::<Vec<_>>()
    };

    for (key, value) in entries {
        let replacement = format!("{key}={value}");
        if let Some(line) = lines
            .iter_mut()
            .find(|line| property_key(line).as_deref() == Some(*key))
        {
            *line = replacement;
        } else {
            lines.push(replacement);
        }
    }

    let mut next = lines.join("\n");
    next.push('\n');
    fs::write(path, next).with_context(|| format!("write {}", path.display()))
}

fn property_key(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('!') {
        return None;
    }
    let (key, _) = trimmed.split_once('=')?;
    Some(key.trim().to_string())
}

struct RconClient {
    stream: TcpStream,
    next_id: i32,
}

impl RconClient {
    fn connect(host: &str, port: u16, password: &str) -> Result<Self> {
        let address = format!("{host}:{port}");
        let stream =
            TcpStream::connect(&address).with_context(|| format!("connect RCON {address}"))?;
        stream.set_read_timeout(Some(DEFAULT_TIMEOUT))?;
        stream.set_write_timeout(Some(DEFAULT_TIMEOUT))?;

        let mut client = Self { stream, next_id: 1 };
        let id = client.next_packet_id();
        client.write_packet(id, SERVERDATA_AUTH, password)?;
        let response = client.read_packet()?;
        if response.id == -1 || response.kind != SERVERDATA_AUTH_RESPONSE {
            return Err(anyhow!("RCON authentication failed"));
        }
        Ok(client)
    }

    fn command(&mut self, command: &str) -> Result<String> {
        let id = self.next_packet_id();
        self.write_packet(id, SERVERDATA_EXECCOMMAND, command)?;
        let response = self.read_packet()?;
        if response.id != id {
            return Err(anyhow!("unexpected RCON response id {}", response.id));
        }
        Ok(response.body)
    }

    fn command_no_response(&mut self, command: &str) -> Result<()> {
        let id = self.next_packet_id();
        self.write_packet(id, SERVERDATA_EXECCOMMAND, command)
    }

    fn next_packet_id(&mut self) -> i32 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    fn write_packet(&mut self, id: i32, kind: i32, body: &str) -> Result<()> {
        let body = body.as_bytes();
        let length = (4 + 4 + body.len() + 2) as i32;
        self.stream.write_all(&length.to_le_bytes())?;
        self.stream.write_all(&id.to_le_bytes())?;
        self.stream.write_all(&kind.to_le_bytes())?;
        self.stream.write_all(body)?;
        self.stream.write_all(&[0, 0])?;
        self.stream.flush()?;
        Ok(())
    }

    fn read_packet(&mut self) -> Result<RconPacket> {
        let mut length = [0_u8; 4];
        self.stream.read_exact(&mut length)?;
        let length = i32::from_le_bytes(length);
        if length < 10 {
            return Err(anyhow!("invalid RCON packet length {length}"));
        }
        let mut payload = vec![0_u8; length as usize];
        self.stream.read_exact(&mut payload)?;

        let id = i32::from_le_bytes(payload[0..4].try_into()?);
        let kind = i32::from_le_bytes(payload[4..8].try_into()?);
        let body_end = payload.len().saturating_sub(2);
        let body = String::from_utf8_lossy(&payload[8..body_end]).into_owned();
        Ok(RconPacket { id, kind, body })
    }
}

impl Drop for RconClient {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

struct RconPacket {
    id: i32,
    kind: i32,
    body: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn property_updates_preserve_unrelated_lines() {
        let dir =
            std::env::temp_dir().join(format!("remiaft-rcon-properties-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("server.properties");
        fs::write(&path, "# comment\nenable-rcon=false\nserver-port=25565\n").unwrap();

        configure_server_properties(&dir, 25576, "secret").unwrap();

        let raw = fs::read_to_string(path).unwrap();
        assert!(raw.contains("# comment"));
        assert!(raw.contains("server-port=25565"));
        assert!(raw.contains("enable-rcon=true"));
        assert!(raw.contains("rcon.port=25576"));
        assert!(raw.contains("rcon.password=secret"));

        fs::remove_dir_all(dir).unwrap();
    }
}
