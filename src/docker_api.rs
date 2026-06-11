use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

const DOCKER_SOCKET: &str = "/var/run/docker.sock";

pub struct DockerClient {
    socket_path: PathBuf,
}

impl DockerClient {
    pub fn connect() -> Result<Self> {
        #[cfg(not(unix))]
        bail!("Docker API over Unix socket is only supported on Unix in this build");

        #[cfg(unix)]
        {
            Ok(Self {
                socket_path: PathBuf::from(DOCKER_SOCKET),
            })
        }
    }

    pub fn ping(&self) -> Result<()> {
        self.request("GET", "/_ping", None)?.ensure_success()
    }

    pub fn image_exists(&self, image: &str) -> Result<bool> {
        let response =
            self.request("GET", &format!("/images/{}/json", encode_path(image)), None)?;
        match response.status {
            200 => Ok(true),
            404 => Ok(false),
            _ => {
                response.ensure_success()?;
                Ok(true)
            }
        }
    }

    pub fn pull_image(&self, image: &str) -> Result<()> {
        let (from_image, tag) = split_image(image);
        let mut path = format!("/images/create?fromImage={}", encode_query(from_image));
        if let Some(tag) = tag {
            path.push_str("&tag=");
            path.push_str(&encode_query(tag));
        }
        self.request("POST", &path, None)?.ensure_success()
    }

    pub fn post_empty(&self, path: &str) -> Result<()> {
        self.request("POST", path, None)?.ensure_success()
    }

    pub fn request(&self, method: &str, path: &str, body: Option<Value>) -> Result<DockerResponse> {
        let body = match body {
            Some(body) => serde_json::to_vec(&body)?,
            None => Vec::new(),
        };

        audit_docker_request(method, path, None, None)?;
        let result = self.request_inner(method, path, body);
        match &result {
            Ok(response) => audit_docker_request(method, path, Some(response.status), None)?,
            Err(err) => audit_docker_request(method, path, None, Some(&err.to_string()))?,
        }
        result
    }

    fn request_inner(&self, method: &str, path: &str, body: Vec<u8>) -> Result<DockerResponse> {
        #[cfg(unix)]
        {
            let mut stream = UnixStream::connect(&self.socket_path)
                .with_context(|| format!("connect Docker socket {}", self.socket_path.display()))?;

            let request = format!(
                "{method} {path} HTTP/1.1\r\nHost: docker\r\nUser-Agent: remiaft/{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                env!("CARGO_PKG_VERSION"),
                body.len()
            );
            stream.write_all(request.as_bytes())?;
            if !body.is_empty() {
                stream.write_all(&body)?;
            }
            stream.flush()?;

            let mut response = Vec::new();
            stream.read_to_end(&mut response)?;
            DockerResponse::parse(response)
        }

        #[cfg(not(unix))]
        {
            let _ = (method, path, body);
            bail!("Docker API over Unix socket is only supported on Unix in this build")
        }
    }
}

fn audit_docker_request(
    method: &str,
    path: &str,
    status: Option<u16>,
    error: Option<&str>,
) -> Result<()> {
    let data_dir =
        dirs::data_local_dir().ok_or_else(|| anyhow!("cannot resolve user data directory"))?;
    let audit_dir = data_dir.join("remiaft").join("runtime");
    fs::create_dir_all(&audit_dir)
        .with_context(|| format!("create Docker audit dir {}", audit_dir.display()))?;
    let audit_path = audit_dir.join("docker-audit.log");
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    let entry = json!({
        "timestamp_unix": timestamp,
        "pid": std::process::id(),
        "method": method,
        "path": path,
        "status": status,
        "error": error,
    });
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&audit_path)
        .with_context(|| format!("open Docker audit log {}", audit_path.display()))?;
    writeln!(file, "{entry}")?;
    Ok(())
}

pub struct DockerResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

impl DockerResponse {
    fn parse(raw: Vec<u8>) -> Result<Self> {
        let split = raw
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .ok_or_else(|| anyhow!("invalid Docker HTTP response"))?;
        let headers_raw = String::from_utf8_lossy(&raw[..split]);
        let body = raw[split + 4..].to_vec();
        let mut lines = headers_raw.lines();
        let status_line = lines
            .next()
            .ok_or_else(|| anyhow!("missing Docker HTTP status line"))?;
        let status = status_line
            .split_whitespace()
            .nth(1)
            .ok_or_else(|| anyhow!("missing Docker HTTP status code"))?
            .parse::<u16>()?;

        let mut headers = BTreeMap::new();
        for line in lines {
            if let Some((key, value)) = line.split_once(':') {
                headers.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
            }
        }
        let body = if headers
            .get("transfer-encoding")
            .is_some_and(|value| value.eq_ignore_ascii_case("chunked"))
        {
            decode_chunked(&body)?
        } else {
            body
        };
        Ok(Self { status, body })
    }

    pub fn ensure_success(&self) -> Result<()> {
        if (200..300).contains(&self.status) || self.status == 304 {
            return Ok(());
        }
        let message = serde_json::from_slice::<Value>(&self.body)
            .ok()
            .and_then(|value| {
                value
                    .get("message")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
            })
            .unwrap_or_else(|| String::from_utf8_lossy(&self.body).trim().to_string());
        bail!("Docker API returned {}: {}", self.status, message)
    }
}

pub fn encode_path(value: &str) -> String {
    percent_encode(value, true)
}

pub fn encode_query(value: &str) -> String {
    percent_encode(value, false)
}

fn percent_encode(value: &str, encode_slash: bool) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        let allowed = byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_' | b'.' | b'~')
            || (!encode_slash && byte == b'/');
        if allowed {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn split_image(image: &str) -> (&str, Option<&str>) {
    let last_slash = image.rfind('/').unwrap_or(0);
    if let Some(colon) = image.rfind(':') {
        if colon > last_slash {
            return (&image[..colon], Some(&image[colon + 1..]));
        }
    }
    (image, None)
}

fn decode_chunked(body: &[u8]) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut index = 0;
    loop {
        let line_end = body[index..]
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or_else(|| anyhow!("invalid chunked Docker response"))?
            + index;
        let size_raw = String::from_utf8_lossy(&body[index..line_end]);
        let size = usize::from_str_radix(size_raw.trim(), 16)?;
        index = line_end + 2;
        if size == 0 {
            break;
        }
        if index + size > body.len() {
            bail!("truncated chunked Docker response");
        }
        output.extend_from_slice(&body[index..index + size]);
        index += size + 2;
    }
    Ok(output)
}
