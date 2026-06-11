use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::io::Write;
use std::net::TcpListener;
use std::path::{Component, Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::config::{
    ConfigStore, DockerImagePolicy, DockerPortMapping, DockerProtocol, DockerServerConfig,
    DockerUserMode, RconMode, ServerConfig,
};
use crate::docker_api::{encode_path, encode_query, DockerClient};
use crate::rcon;
use crate::text_encoding;

const DEFAULT_NETWORK: &str = "remiaft";
const DEFAULT_GAME_PORT: u16 = 25565;
const LABEL_MANAGER: &str = "remiaft.manager";
const LABEL_MANAGED_LEGACY: &str = "com.remiaft.managed";
const LABEL_SERVER_ID: &str = "com.remiaft.server_id";
const LABEL_SERVER_NAME: &str = "com.remiaft.server_name";
const LABEL_VERSION: &str = "com.remiaft.version";
const LABEL_CONFIG_HASH: &str = "com.remiaft.config_hash";
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(500);

pub fn prepare_server(server: &mut ServerConfig, reserved_ports: &[u16]) -> Result<()> {
    let docker = &mut server.runtime.docker;

    if !docker.ports.iter().any(|port| {
        port.container_port == DEFAULT_GAME_PORT && port.protocol == DockerProtocol::Tcp
    }) {
        docker
            .ports
            .push(DockerPortMapping::tcp("minecraft", DEFAULT_GAME_PORT, None));
    }

    if matches!(docker.rcon.mode, RconMode::Auto) && docker.rcon.password.is_none() {
        docker.rcon.password = Some(format!("remiaft-{}", Uuid::new_v4().simple()));
    }

    if matches!(docker.rcon.mode, RconMode::Auto | RconMode::Manual) {
        let rcon_host_port = docker
            .rcon
            .host_port
            .or_else(|| find_existing_port_mapping(docker, docker.rcon.container_port));
        let rcon_host_port = match rcon_host_port {
            Some(port) => port,
            None if matches!(docker.rcon.mode, RconMode::Auto) => allocate_port(
                docker.rcon.port_range_start,
                docker.rcon.port_range_end,
                reserved_ports,
            )?,
            None => return Ok(()),
        };
        docker.rcon.host_port = Some(rcon_host_port);
        ensure_rcon_port_mapping(docker, rcon_host_port);
    }

    Ok(())
}

pub fn validate_server_config(server: &ServerConfig) -> Result<()> {
    validate_server_security(server)
}

pub fn runtime_status(server: &ServerConfig) -> RuntimeStatus {
    if validate_server_security(server).is_err() {
        return RuntimeStatus::Stale;
    }
    let Ok(client) = DockerClient::connect() else {
        return RuntimeStatus::Stopped;
    };
    let name = container_name(server);
    match inspect_container(&client, &name) {
        Ok(Some(container)) if !is_manageable_container(&container, server) => RuntimeStatus::Stale,
        Ok(Some(container)) if validate_container_security(&container, server).is_err() => {
            RuntimeStatus::Stale
        }
        Ok(Some(container)) if container.running() => RuntimeStatus::Running,
        Ok(Some(_)) => RuntimeStatus::Stopped,
        _ => RuntimeStatus::Stopped,
    }
}

pub fn start_server(store: &ConfigStore, server: &ServerConfig) -> Result<()> {
    validate_server_security(server)?;
    let client = DockerClient::connect()?;
    client.ping()?;

    let name = container_name(server);
    let desired_config_hash = container_config_hash(server)?;
    if let Some(container) = inspect_container(&client, &name)? {
        ensure_manageable_container(&container, server)?;
        if container.config_hash() != Some(desired_config_hash.as_str()) {
            if container.running() {
                bail!(
                    "Docker config changed for {}; stop it before Remiaft recreates the container",
                    server.name
                );
            }
            remove_container(&client, &name)?;
        } else if container.running() {
            sync_logs(store, server)?;
            return Ok(());
        } else {
            client.post_empty(&format!("/containers/{}/start", encode_path(&name)))?;
            sync_logs(store, server)?;
            return Ok(());
        }
    }

    configure_rcon_if_possible(store, server)?;
    let image = ensure_image(&client, server)?;
    ensure_network(&client, &network_name(server))?;
    create_container(&client, server, &name, &image, &desired_config_hash)?;
    client.post_empty(&format!("/containers/{}/start", encode_path(&name)))?;
    sync_logs(store, server)?;
    Ok(())
}

fn remove_container(client: &DockerClient, name: &str) -> Result<()> {
    let response = client.request(
        "DELETE",
        &format!("/containers/{}?v=0", encode_path(name)),
        None,
    )?;
    if response.status == 404 {
        return Ok(());
    }
    response.ensure_success()
}

pub fn stop_server(store: &ConfigStore, server: &ServerConfig) -> Result<()> {
    ensure_existing_manageable_container(server)?;
    if let Err(err) = send_rcon_command_no_response(server, "stop") {
        append_runtime_note(
            store,
            server,
            &format!("RCON stop failed, falling back to Docker stop: {err}\n"),
        );
    }

    let start = Instant::now();
    while start.elapsed() < STOP_TIMEOUT {
        if runtime_status(server) != RuntimeStatus::Running {
            let _ = sync_logs(store, server);
            return Ok(());
        }
        thread::sleep(STOP_POLL_INTERVAL);
    }

    let client = DockerClient::connect()?;
    let name = container_name(server);
    client.post_empty(&format!("/containers/{}/stop?t=10", encode_path(&name)))?;
    let _ = sync_logs(store, server);
    Ok(())
}

pub fn interrupt_server(store: &ConfigStore, server: &ServerConfig) -> Result<()> {
    ensure_existing_manageable_container(server)?;
    match send_rcon_command_no_response(server, "stop") {
        Ok(()) => Ok(()),
        Err(err) => {
            append_runtime_note(
                store,
                server,
                &format!("RCON interrupt failed, falling back to Docker stop: {err}\n"),
            );
            stop_server(store, server)
        }
    }
}

pub fn send_rcon_command(server: &ServerConfig, command: &str) -> Result<String> {
    ensure_existing_manageable_container(server)?;
    let rcon = &server.runtime.docker.rcon;
    if matches!(rcon.mode, RconMode::Disabled) {
        bail!("RCON is disabled for {}", server.name);
    }
    let password = rcon
        .password
        .as_deref()
        .ok_or_else(|| anyhow!("missing RCON password for {}", server.name))?;
    let host_port = rcon
        .host_port
        .ok_or_else(|| anyhow!("missing RCON host port for {}", server.name))?;
    rcon::send_command(&rcon.host, host_port, password, command)
}

fn send_rcon_command_no_response(server: &ServerConfig, command: &str) -> Result<()> {
    let rcon = &server.runtime.docker.rcon;
    if matches!(rcon.mode, RconMode::Disabled) {
        bail!("RCON is disabled for {}", server.name);
    }
    let password = rcon
        .password
        .as_deref()
        .ok_or_else(|| anyhow!("missing RCON password for {}", server.name))?;
    let host_port = rcon
        .host_port
        .ok_or_else(|| anyhow!("missing RCON host port for {}", server.name))?;
    rcon::send_command_no_response(&rcon.host, host_port, password, command)
}

pub fn attach_rcon_console(store: &ConfigStore, server: &ServerConfig) -> Result<()> {
    sync_logs(store, server)?;
    println!("-- remiaft RCON console: {} --", server.name);
    println!("Type commands and press Enter. Type exit to detach.");
    println!();
    print_recent_log(store, server)?;

    let mut input = String::new();
    loop {
        input.clear();
        print!("rcon> ");
        std::io::stdout().flush()?;
        if std::io::stdin().read_line(&mut input)? == 0 {
            break;
        }
        let command = input.trim();
        if command.eq_ignore_ascii_case("exit") || command.eq_ignore_ascii_case("quit") {
            break;
        }
        if command.is_empty() {
            continue;
        }
        match send_rcon_command(server, command) {
            Ok(response) if response.trim().is_empty() => {}
            Ok(response) => println!("{}", response.trim_end()),
            Err(err) => println!("RCON failed: {err}"),
        }
        let _ = sync_logs(store, server);
    }
    Ok(())
}

pub fn sync_logs(store: &ConfigStore, server: &ServerConfig) -> Result<()> {
    let client = DockerClient::connect()?;
    let name = container_name(server);
    let Some(container) = inspect_container(&client, &name)? else {
        return Ok(());
    };
    ensure_manageable_container(&container, server)?;
    let response = client.request(
        "GET",
        &format!(
            "/containers/{}/logs?stdout=1&stderr=1&tail=5000&timestamps=0",
            encode_path(&name)
        ),
        None,
    )?;
    if response.status == 404 {
        return Ok(());
    }
    response.ensure_success()?;
    let log = decode_docker_log(&response.body);
    let path = minecraft_log_path(store, server);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, log)?;
    Ok(())
}

pub fn minecraft_log_path(store: &ConfigStore, server: &ServerConfig) -> PathBuf {
    store.runtime_dir().join(&server.id).join("minecraft.log")
}

fn configure_rcon_if_possible(store: &ConfigStore, server: &ServerConfig) -> Result<()> {
    let docker = &server.runtime.docker;
    if !matches!(docker.rcon.mode, RconMode::Auto) {
        return Ok(());
    }
    if !docker.mount_server_directory {
        append_runtime_note(
            store,
            server,
            "RCON auto configuration skipped: server directory is not bind-mounted\n",
        );
        return Ok(());
    }
    let Some(password) = docker.rcon.password.as_deref() else {
        append_runtime_note(
            store,
            server,
            "RCON auto configuration skipped: missing generated password\n",
        );
        return Ok(());
    };
    if let Err(err) =
        rcon::configure_server_properties(&server.directory, docker.rcon.container_port, password)
    {
        append_runtime_note(
            store,
            server,
            &format!("RCON auto configuration failed; configure manually: {err}\n"),
        );
    }
    Ok(())
}

fn ensure_image(client: &DockerClient, server: &ServerConfig) -> Result<String> {
    let docker = &server.runtime.docker;
    let candidates = image_candidates(server);
    let mut last_error = None;
    for image in candidates {
        let present = client.image_exists(&image)?;
        if present && docker.image_policy != DockerImagePolicy::Always {
            return Ok(image);
        }
        if docker.image_policy == DockerImagePolicy::Never {
            if present {
                return Ok(image);
            }
            bail!("Docker image not present and pull policy is never: {image}");
        }
        match client.pull_image(&image) {
            Ok(()) => return Ok(image),
            Err(err) => last_error = Some(err),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("no Docker image candidates configured")))
}

fn image_candidates(server: &ServerConfig) -> Vec<String> {
    image_candidates_for_region(server, docker_registry_region())
}

fn image_candidates_for_region(server: &ServerConfig, region: DockerRegistryRegion) -> Vec<String> {
    let docker = &server.runtime.docker;
    if let Some(image) = docker
        .image
        .as_deref()
        .filter(|image| !image.trim().is_empty())
    {
        return vec![image.trim().to_string()];
    }
    if !docker.image_candidates.is_empty() {
        return docker.image_candidates.clone();
    }
    let java = minecraft_java_major(server.version.as_deref());
    let base = vec![
        format!("eclipse-temurin:{java}-jre"),
        format!("amazoncorretto:{java}"),
        format!("bellsoft/liberica-openjdk-debian:{java}"),
    ];
    if region != DockerRegistryRegion::China {
        return base;
    }

    let mut candidates = Vec::new();
    for image in &base {
        if let Some(mirror) = docker_hub_mirror_image(image) {
            push_unique(&mut candidates, mirror);
        }
    }
    for image in base {
        push_unique(&mut candidates, image);
    }
    candidates
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum DockerRegistryRegion {
    Global,
    China,
}

fn docker_registry_region() -> DockerRegistryRegion {
    for key in ["REMIAFT_DOCKER_REGISTRY_REGION", "REMIAFT_REGION", "REGION"] {
        if let Ok(value) = env::var(key) {
            if let Some(region) = parse_registry_region(&value) {
                return region;
            }
        }
    }

    for key in ["TZ", "LC_ALL", "LC_MESSAGES", "LANG"] {
        if env::var(key)
            .ok()
            .is_some_and(|value| contains_china_region_hint(&value))
        {
            return DockerRegistryRegion::China;
        }
    }

    if fs::read_to_string("/etc/timezone")
        .ok()
        .is_some_and(|value| contains_china_region_hint(&value))
    {
        return DockerRegistryRegion::China;
    }

    if fs::read_link("/etc/localtime")
        .ok()
        .and_then(|path| path.to_str().map(ToString::to_string))
        .is_some_and(|value| contains_china_region_hint(&value))
    {
        return DockerRegistryRegion::China;
    }

    DockerRegistryRegion::Global
}

fn parse_registry_region(value: &str) -> Option<DockerRegistryRegion> {
    match value.trim().to_ascii_lowercase().as_str() {
        "cn" | "china" | "mainland-china" | "zh-cn" => Some(DockerRegistryRegion::China),
        "global" | "dockerhub" | "docker-hub" | "us" | "default" => {
            Some(DockerRegistryRegion::Global)
        }
        _ => None,
    }
}

fn contains_china_region_hint(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    value.contains("asia/shanghai")
        || value.contains("asia/chongqing")
        || value.contains("asia/harbin")
        || value.contains("asia/urumqi")
        || value.contains("zh_cn")
        || value.contains("china")
}

fn docker_hub_mirror_image(image: &str) -> Option<String> {
    let has_namespace = image.contains('/');
    let first = image.split('/').next().unwrap_or_default();
    if has_namespace && (first.contains('.') || first.contains(':') || first == "localhost") {
        return None;
    }
    let repository = if has_namespace {
        image.to_string()
    } else {
        format!("library/{image}")
    };
    Some(format!("docker.m.daocloud.io/{repository}"))
}

fn push_unique(candidates: &mut Vec<String>, image: String) {
    if !candidates.contains(&image) {
        candidates.push(image);
    }
}

fn minecraft_java_major(version: Option<&str>) -> u16 {
    let Some(version) = version else {
        return 21;
    };
    let parts = version
        .split(['.', '-'])
        .filter_map(|part| part.parse::<u16>().ok())
        .collect::<Vec<_>>();
    match parts.as_slice() {
        [1, minor, ..] if *minor <= 16 => 8,
        [1, 17, ..] => 16,
        [1, minor, ..] if *minor <= 20 => 17,
        _ => 21,
    }
}

fn create_container(
    client: &DockerClient,
    server: &ServerConfig,
    name: &str,
    image: &str,
    config_hash: &str,
) -> Result<()> {
    validate_server_security(server)?;
    let docker = &server.runtime.docker;
    let port_bindings = port_bindings(docker);
    let exposed_ports = exposed_ports(docker);
    let mut host_config = json!({
        "Binds": bind_mounts(server),
        "PortBindings": port_bindings,
        "AutoRemove": docker.auto_remove,
        "NetworkMode": network_name(server),
        "Privileged": false,
        "PidMode": "",
    });
    if let Some(host_config) = host_config.as_object_mut() {
        host_config.retain(|_, value| !value.is_null());
    }

    let mut container = json!({
        "Image": image,
        "Env": environment(server),
        "Labels": labels(server, config_hash),
        "ExposedPorts": exposed_ports,
        "HostConfig": host_config,
        "WorkingDir": working_dir(server),
        "Tty": false,
        "OpenStdin": false,
    });

    if let Some(user) = container_user(server)? {
        container["User"] = Value::String(user);
    }
    if let Some(cmd) = container_command(server) {
        container["Cmd"] = json!(["sh", "-lc", cmd]);
    }

    let response = client.request(
        "POST",
        &format!("/containers/create?name={}", encode_query(name)),
        Some(container),
    )?;
    if response.status == 409 {
        bail!("Docker container name already exists: {name}");
    }
    response.ensure_success()?;
    Ok(())
}

fn ensure_network(client: &DockerClient, network: &str) -> Result<()> {
    let response = client.request("GET", &format!("/networks/{}", encode_path(network)), None)?;
    if response.status == 200 {
        return Ok(());
    }
    if response.status != 404 {
        response.ensure_success()?;
    }
    let body = json!({
        "Name": network,
        "Driver": "bridge",
        "Labels": {
            LABEL_MANAGER: "true",
            LABEL_MANAGED_LEGACY: "true",
            LABEL_VERSION: env!("CARGO_PKG_VERSION"),
        }
    });
    client
        .request("POST", "/networks/create", Some(body))?
        .ensure_success()
}

fn inspect_container(client: &DockerClient, name: &str) -> Result<Option<ContainerInspect>> {
    let response = client.request(
        "GET",
        &format!("/containers/{}/json", encode_path(name)),
        None,
    )?;
    if response.status == 404 {
        return Ok(None);
    }
    response.ensure_success()?;
    serde_json::from_slice(&response.body).context("parse Docker container inspect")
}

fn ensure_existing_manageable_container(server: &ServerConfig) -> Result<()> {
    validate_server_security(server)?;
    let client = DockerClient::connect()?;
    let name = container_name(server);
    let Some(container) = inspect_container(&client, &name)? else {
        bail!("Docker container does not exist: {name}");
    };
    ensure_manageable_container(&container, server)
}

fn ensure_manageable_container(container: &ContainerInspect, server: &ServerConfig) -> Result<()> {
    if !is_manageable_container(container, server) {
        bail!(
            "Docker container name conflicts with a container Remiaft is not allowed to manage: {}",
            container.name.as_deref().unwrap_or("<unknown>")
        );
    }
    validate_container_security(container, server)
}

fn is_manageable_container(container: &ContainerInspect, server: &ServerConfig) -> bool {
    let labels = container
        .config
        .as_ref()
        .and_then(|config| config.labels.as_ref());
    labels.is_some_and(|labels| {
        labels.get(LABEL_MANAGER).map(String::as_str) == Some("true")
            && labels.get(LABEL_SERVER_ID).map(String::as_str) == Some(server.id.as_str())
    })
}

fn container_name(server: &ServerConfig) -> String {
    if let Some(name) = server
        .runtime
        .docker
        .container_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        return name.to_string();
    }
    let short_id = server.id.rsplit('-').next().unwrap_or(&server.id);
    format!("remiaft-{}-{short_id}", slug(&server.name))
}

fn network_name(server: &ServerConfig) -> String {
    server
        .runtime
        .docker
        .network
        .as_deref()
        .map(str::trim)
        .filter(|network| !network.is_empty())
        .unwrap_or(DEFAULT_NETWORK)
        .to_string()
}

fn labels(server: &ServerConfig, config_hash: &str) -> BTreeMap<String, String> {
    let mut labels = server.runtime.docker.labels.clone();
    labels.insert(LABEL_MANAGER.to_string(), "true".to_string());
    labels.insert(LABEL_MANAGED_LEGACY.to_string(), "true".to_string());
    labels.insert(LABEL_SERVER_ID.to_string(), server.id.clone());
    labels.insert(LABEL_SERVER_NAME.to_string(), server.name.clone());
    labels.insert(
        LABEL_VERSION.to_string(),
        env!("CARGO_PKG_VERSION").to_string(),
    );
    labels.insert(LABEL_CONFIG_HASH.to_string(), config_hash.to_string());
    labels
}

fn environment(server: &ServerConfig) -> Vec<String> {
    let mut env = server.runtime.docker.environment.clone();
    env.entry("HOME".to_string())
        .or_insert_with(|| "/home/remiaft".to_string());
    env.into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect()
}

fn bind_mounts(server: &ServerConfig) -> Vec<String> {
    let docker = &server.runtime.docker;
    let mut binds = Vec::new();
    if docker.mount_server_directory {
        binds.push(format!(
            "{}:{}:rw",
            server.directory.display(),
            docker.server_dir
        ));
    }
    for mount in &docker.volumes {
        binds.push(format!(
            "{}:{}:{}",
            mount.host.display(),
            mount.container,
            if mount.read_only { "ro" } else { "rw" }
        ));
    }
    binds
}

fn validate_server_security(server: &ServerConfig) -> Result<()> {
    validate_network_mode(server, &network_name(server))?;
    let docker = &server.runtime.docker;
    if docker.mount_server_directory {
        validate_bind_mount(
            &server.directory,
            &docker.server_dir,
            "server directory bind mount",
        )?;
    }
    for mount in &docker.volumes {
        validate_bind_mount(&mount.host, &mount.container, "custom volume bind mount")?;
    }
    Ok(())
}

fn validate_container_security(container: &ContainerInspect, server: &ServerConfig) -> Result<()> {
    let Some(host_config) = container.host_config.as_ref() else {
        return Ok(());
    };
    if host_config.privileged.unwrap_or(false) {
        bail!(
            "refusing to manage privileged Docker container: {}",
            container.name.as_deref().unwrap_or("<unknown>")
        );
    }
    if host_config
        .pid_mode
        .as_deref()
        .is_some_and(|mode| mode.eq_ignore_ascii_case("host"))
    {
        bail!(
            "refusing to manage Docker container using host PID namespace: {}",
            container.name.as_deref().unwrap_or("<unknown>")
        );
    }
    if let Some(network_mode) = host_config.network_mode.as_deref() {
        validate_network_mode(server, network_mode)?;
    }
    if let Some(binds) = host_config.binds.as_ref() {
        for bind in binds {
            validate_bind_spec(bind)?;
        }
    }
    Ok(())
}

fn validate_network_mode(server: &ServerConfig, network: &str) -> Result<()> {
    if network.trim().eq_ignore_ascii_case("host") && !backend_allows_host_network(server) {
        bail!("Docker host networking is not allowed for {}", server.name);
    }
    Ok(())
}

fn backend_allows_host_network(_server: &ServerConfig) -> bool {
    false
}

fn validate_bind_spec(bind: &str) -> Result<()> {
    let mut parts = bind.split(':');
    let Some(host) = parts.next() else {
        bail!("invalid Docker bind mount: {bind}");
    };
    let Some(container) = parts.next() else {
        bail!("invalid Docker bind mount: {bind}");
    };
    validate_bind_mount(Path::new(host), container, "existing container bind mount")
}

fn validate_bind_mount(host: &Path, container: &str, label: &str) -> Result<()> {
    let host = normalize_host_mount_path(host)?;
    if is_dangerous_mount_path(&host) {
        bail!("{label} uses a dangerous host path: {}", host.display());
    }
    if let Ok(canonical) = fs::canonicalize(&host) {
        let canonical = normalize_absolute_path(&canonical)?;
        if is_dangerous_mount_path(&canonical) {
            bail!(
                "{label} resolves to a dangerous host path: {}",
                canonical.display()
            );
        }
    }

    let container = normalize_container_mount_path(container)?;
    if is_dangerous_mount_path(&container) {
        bail!(
            "{label} uses a dangerous container path: {}",
            container.display()
        );
    }
    Ok(())
}

fn normalize_host_mount_path(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()?.join(path)
    };
    normalize_absolute_path(&absolute)
}

fn normalize_container_mount_path(path: &str) -> Result<PathBuf> {
    let path = Path::new(path.trim());
    if !path.is_absolute() {
        bail!(
            "Docker bind mount container path must be absolute: {}",
            path.display()
        );
    }
    normalize_absolute_path(path)
}

fn normalize_absolute_path(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!(
            "Docker bind mount path must be absolute: {}",
            path.display()
        );
    }
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
            Component::Prefix(_) => bail!("unsupported Docker bind mount path: {}", path.display()),
        }
    }
    Ok(normalized)
}

fn is_dangerous_mount_path(path: &Path) -> bool {
    path == Path::new("/")
        || path.starts_with("/etc")
        || path.starts_with("/root")
        || path.starts_with("/proc")
        || path.starts_with("/sys")
        || path.starts_with("/dev")
        || path.starts_with("/var/run/docker.sock")
        || path.starts_with("/run/docker.sock")
}

fn working_dir(server: &ServerConfig) -> String {
    server
        .runtime
        .docker
        .working_dir
        .clone()
        .unwrap_or_else(|| server.runtime.docker.server_dir.clone())
}

fn container_user(server: &ServerConfig) -> Result<Option<String>> {
    let docker = &server.runtime.docker;
    match docker.user.mode {
        DockerUserMode::Image => Ok(None),
        DockerUserMode::Custom => {
            let uid = docker
                .user
                .uid
                .ok_or_else(|| anyhow!("custom Docker user requires uid"))?;
            let gid = docker
                .user
                .gid
                .ok_or_else(|| anyhow!("custom Docker user requires gid"))?;
            Ok(Some(format!("{uid}:{gid}")))
        }
        DockerUserMode::Host => Ok(Some(host_user())),
        DockerUserMode::Auto => {
            if docker.mount_server_directory || !docker.volumes.is_empty() {
                Ok(Some(host_user()))
            } else {
                Ok(None)
            }
        }
    }
}

#[cfg(unix)]
fn host_user() -> String {
    unsafe { format!("{}:{}", libc::getuid(), libc::getgid()) }
}

#[cfg(not(unix))]
fn host_user() -> String {
    "1000:1000".to_string()
}

fn container_command(server: &ServerConfig) -> Option<String> {
    let docker = &server.runtime.docker;
    if docker.use_image_entrypoint {
        return None;
    }
    if let Some(command) = docker
        .startup_command
        .as_deref()
        .map(str::trim)
        .filter(|command| !command.is_empty())
    {
        return Some(command.to_string());
    }

    let mut parts = vec![
        "java".to_string(),
        format!("-Xms{}M", server.min_memory_mb),
        format!("-Xmx{}M", server.max_memory_mb),
    ];
    parts.extend(server.java_args.clone());
    parts.push("-jar".to_string());
    parts.push(container_jar_path(server));
    parts.extend(server.server_args.clone());
    Some(parts.join(" "))
}

fn container_jar_path(server: &ServerConfig) -> String {
    let jar = &server.jar_path;
    if jar.is_relative() {
        return jar.to_string_lossy().to_string();
    }
    if let Ok(relative) = jar.strip_prefix(&server.directory) {
        return format!(
            "{}/{}",
            server.runtime.docker.server_dir.trim_end_matches('/'),
            relative.to_string_lossy()
        );
    }
    jar.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| jar.to_string_lossy().to_string())
}

fn exposed_ports(docker: &DockerServerConfig) -> Value {
    let mut exposed = serde_json::Map::new();
    for port in &docker.ports {
        exposed.insert(port_key(port.container_port, port.protocol), json!({}));
    }
    Value::Object(exposed)
}

fn port_bindings(docker: &DockerServerConfig) -> Value {
    let mut bindings = serde_json::Map::new();
    for port in &docker.ports {
        let mut binding = serde_json::Map::new();
        if let Some(host_ip) = port.host_ip.as_deref().filter(|value| !value.is_empty()) {
            binding.insert("HostIp".to_string(), Value::String(host_ip.to_string()));
        }
        binding.insert(
            "HostPort".to_string(),
            Value::String(
                port.host_port
                    .map(|port| port.to_string())
                    .unwrap_or_default(),
            ),
        );
        bindings.insert(
            port_key(port.container_port, port.protocol),
            Value::Array(vec![Value::Object(binding)]),
        );
    }
    Value::Object(bindings)
}

fn port_key(port: u16, protocol: DockerProtocol) -> String {
    format!("{port}/{}", protocol.as_str())
}

fn container_config_hash(server: &ServerConfig) -> Result<String> {
    let docker = &server.runtime.docker;
    let payload = json!({
        "image": image_config_identity(server),
        "network": network_name(server),
        "labels": docker.labels,
        "environment": docker.environment,
        "ports": docker.ports,
        "binds": bind_mounts(server),
        "mount_server_directory": docker.mount_server_directory,
        "server_dir": docker.server_dir,
        "working_dir": working_dir(server),
        "startup_command": docker.startup_command,
        "use_image_entrypoint": docker.use_image_entrypoint,
        "user": container_user(server)?,
        "auto_remove": docker.auto_remove,
        "command": container_command(server),
        "rcon": {
            "mode": docker.rcon.mode,
            "container_port": docker.rcon.container_port,
            "host_port": docker.rcon.host_port,
            "host": docker.rcon.host,
        },
    });
    Ok(format!("{:016x}", fnv1a64(&serde_json::to_vec(&payload)?)))
}

fn image_config_identity(server: &ServerConfig) -> Value {
    let docker = &server.runtime.docker;
    if let Some(image) = docker
        .image
        .as_deref()
        .map(str::trim)
        .filter(|image| !image.is_empty())
    {
        return json!({"kind": "explicit", "image": image});
    }
    if !docker.image_candidates.is_empty() {
        return json!({"kind": "candidates", "images": docker.image_candidates});
    }
    json!({
        "kind": "minecraft-java",
        "major": minecraft_java_major(server.version.as_deref()),
    })
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn find_existing_port_mapping(docker: &DockerServerConfig, container_port: u16) -> Option<u16> {
    docker
        .ports
        .iter()
        .find(|port| port.container_port == container_port && port.protocol == DockerProtocol::Tcp)
        .and_then(|port| port.host_port)
}

fn ensure_rcon_port_mapping(docker: &mut DockerServerConfig, host_port: u16) {
    if let Some(port) = docker.ports.iter_mut().find(|port| {
        port.container_port == docker.rcon.container_port && port.protocol == DockerProtocol::Tcp
    }) {
        port.host_port.get_or_insert(host_port);
        port.host_ip.get_or_insert_with(|| docker.rcon.host.clone());
        port.name.get_or_insert_with(|| "rcon".to_string());
        return;
    }
    let mut port = DockerPortMapping::tcp("rcon", docker.rcon.container_port, Some(host_port));
    port.host_ip = Some(docker.rcon.host.clone());
    docker.ports.push(port);
}

fn allocate_port(start: u16, end: u16, reserved_ports: &[u16]) -> Result<u16> {
    let reserved = reserved_ports.iter().copied().collect::<BTreeSet<_>>();
    for port in start..=end {
        if reserved.contains(&port) {
            continue;
        }
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }
    bail!("no free RCON port in range {start}-{end}")
}

fn print_recent_log(store: &ConfigStore, server: &ServerConfig) -> Result<()> {
    let path = minecraft_log_path(store, server);
    let raw = text_encoding::read_console_text(path).unwrap_or_default();
    let lines = raw.lines().rev().take(40).collect::<Vec<_>>();
    for line in lines.into_iter().rev() {
        println!("{line}");
    }
    Ok(())
}

fn append_runtime_note(store: &ConfigStore, server: &ServerConfig, note: &str) {
    let path = store.runtime_dir().join(&server.id).join("supervisor.log");
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(note.as_bytes());
    }
}

fn slug(input: &str) -> String {
    let slug = input
        .chars()
        .filter_map(|ch| {
            if ch.is_ascii_alphanumeric() {
                Some(ch.to_ascii_lowercase())
            } else if ch.is_ascii_whitespace() || ch == '-' || ch == '_' {
                Some('-')
            } else {
                None
            }
        })
        .collect::<String>();
    let cleaned = slug
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if cleaned.is_empty() {
        "server".to_string()
    } else {
        cleaned
    }
}

fn decode_docker_log(body: &[u8]) -> Vec<u8> {
    let mut index = 0;
    let mut output = Vec::new();
    while index + 8 <= body.len() {
        let stream = body[index];
        if !matches!(stream, 1 | 2) || body[index + 1..index + 4] != [0, 0, 0] {
            return body.to_vec();
        }
        let len = u32::from_be_bytes([
            body[index + 4],
            body[index + 5],
            body[index + 6],
            body[index + 7],
        ]) as usize;
        index += 8;
        if index + len > body.len() {
            return body.to_vec();
        }
        output.extend_from_slice(&body[index..index + len]);
        index += len;
    }
    if index == body.len() {
        output
    } else {
        body.to_vec()
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum RuntimeStatus {
    Running,
    Stopped,
    Stale,
}

#[derive(Debug, Deserialize)]
struct ContainerInspect {
    #[serde(rename = "Name")]
    name: Option<String>,
    #[serde(rename = "State")]
    state: Option<ContainerState>,
    #[serde(rename = "Config")]
    config: Option<ContainerConfig>,
    #[serde(rename = "HostConfig")]
    host_config: Option<ContainerHostConfig>,
}

impl ContainerInspect {
    fn running(&self) -> bool {
        self.state.as_ref().is_some_and(|state| state.running)
    }

    fn config_hash(&self) -> Option<&str> {
        self.config
            .as_ref()
            .and_then(|config| config.labels.as_ref())
            .and_then(|labels| labels.get(LABEL_CONFIG_HASH))
            .map(String::as_str)
    }
}

#[derive(Debug, Deserialize)]
struct ContainerState {
    #[serde(rename = "Running")]
    running: bool,
}

#[derive(Debug, Deserialize)]
struct ContainerConfig {
    #[serde(rename = "Labels")]
    labels: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Deserialize)]
struct ContainerHostConfig {
    #[serde(rename = "Privileged")]
    privileged: Option<bool>,
    #[serde(rename = "PidMode")]
    pid_mode: Option<String>,
    #[serde(rename = "NetworkMode")]
    network_mode: Option<String>,
    #[serde(rename = "Binds")]
    binds: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_container_name_is_stable_and_scoped() {
        let mut config = crate::config::RemiaftConfig::default();
        config.add_server(
            "Bed Wars Room 1".to_string(),
            PathBuf::from("."),
            PathBuf::from("server.jar"),
        );
        let server = &config.servers[0];

        assert!(container_name(server).starts_with("remiaft-bed-wars-room-1-"));
    }

    #[test]
    fn default_image_tracks_minecraft_java_requirement() {
        let mut config = crate::config::RemiaftConfig::default();
        config.add_server(
            "old".to_string(),
            PathBuf::from("."),
            PathBuf::from("server.jar"),
        );
        config.servers[0].version = Some("1.16.5".to_string());

        assert_eq!(
            image_candidates_for_region(&config.servers[0], DockerRegistryRegion::Global)[0],
            "eclipse-temurin:8-jre"
        );

        config.servers[0].version = Some("1.21.1".to_string());
        assert_eq!(
            image_candidates_for_region(&config.servers[0], DockerRegistryRegion::Global)[0],
            "eclipse-temurin:21-jre"
        );
    }

    #[test]
    fn china_region_prefers_docker_hub_mirror_then_falls_back() {
        let mut config = crate::config::RemiaftConfig::default();
        config.add_server(
            "room".to_string(),
            PathBuf::from("."),
            PathBuf::from("server.jar"),
        );

        let candidates =
            image_candidates_for_region(&config.servers[0], DockerRegistryRegion::China);

        assert_eq!(
            candidates[0],
            "docker.m.daocloud.io/library/eclipse-temurin:21-jre"
        );
        assert!(candidates.contains(&"eclipse-temurin:21-jre".to_string()));
    }

    #[test]
    fn container_config_hash_changes_when_ports_change() {
        let mut config = crate::config::RemiaftConfig::default();
        config.add_server(
            "room".to_string(),
            PathBuf::from("."),
            PathBuf::from("server.jar"),
        );
        let server = &mut config.servers[0];
        server.runtime.kind = crate::config::ServerRuntimeKind::Docker;

        let before = container_config_hash(server).unwrap();
        server
            .runtime
            .docker
            .ports
            .push(DockerPortMapping::tcp("minecraft", 25565, Some(25565)));
        let after = container_config_hash(server).unwrap();

        assert_ne!(before, after);
    }

    #[test]
    fn docker_log_multiplex_headers_are_removed() {
        let body = [
            &[1, 0, 0, 0, 0, 0, 0, 5][..],
            b"hello",
            &[2, 0, 0, 0, 0, 0, 0, 6][..],
            b" world",
        ]
        .concat();

        assert_eq!(decode_docker_log(&body), b"hello world");
    }

    #[test]
    fn labels_include_required_manager_marker() {
        let mut config = crate::config::RemiaftConfig::default();
        config.add_server(
            "room".to_string(),
            PathBuf::from("."),
            PathBuf::from("server.jar"),
        );
        let labels = labels(&config.servers[0], "hash");

        assert_eq!(labels.get(LABEL_MANAGER).map(String::as_str), Some("true"));
    }

    #[test]
    fn manageable_container_requires_manager_label() {
        let mut config = crate::config::RemiaftConfig::default();
        config.add_server(
            "room".to_string(),
            PathBuf::from("."),
            PathBuf::from("server.jar"),
        );
        let server = &config.servers[0];
        let mut labels = BTreeMap::new();
        labels.insert(LABEL_MANAGED_LEGACY.to_string(), "true".to_string());
        labels.insert(LABEL_SERVER_ID.to_string(), server.id.clone());
        let container = inspect_with_labels(labels);

        assert!(!is_manageable_container(&container, server));
    }

    #[test]
    fn rejects_dangerous_custom_mounts() {
        let mut config = crate::config::RemiaftConfig::default();
        config.add_server(
            "room".to_string(),
            PathBuf::from("/srv/room"),
            PathBuf::from("server.jar"),
        );
        let server = &mut config.servers[0];
        server
            .runtime
            .docker
            .volumes
            .push(crate::config::DockerVolumeMount {
                host: PathBuf::from("/var/run/docker.sock"),
                container: "/socket".to_string(),
                read_only: true,
            });

        assert!(validate_server_security(server).is_err());
    }

    #[test]
    fn rejects_host_network_by_default() {
        let mut config = crate::config::RemiaftConfig::default();
        config.add_server(
            "room".to_string(),
            PathBuf::from("/srv/room"),
            PathBuf::from("server.jar"),
        );
        let server = &mut config.servers[0];
        server.runtime.docker.network = Some("host".to_string());

        assert!(validate_server_security(server).is_err());
    }

    #[test]
    fn rejects_privileged_managed_container() {
        let mut config = crate::config::RemiaftConfig::default();
        config.add_server(
            "room".to_string(),
            PathBuf::from("."),
            PathBuf::from("server.jar"),
        );
        let server = &config.servers[0];
        let mut container = inspect_with_labels(required_labels(server));
        container.host_config = Some(ContainerHostConfig {
            privileged: Some(true),
            pid_mode: None,
            network_mode: None,
            binds: None,
        });

        assert!(ensure_manageable_container(&container, server).is_err());
    }

    #[test]
    fn rejects_host_pid_managed_container() {
        let mut config = crate::config::RemiaftConfig::default();
        config.add_server(
            "room".to_string(),
            PathBuf::from("."),
            PathBuf::from("server.jar"),
        );
        let server = &config.servers[0];
        let mut container = inspect_with_labels(required_labels(server));
        container.host_config = Some(ContainerHostConfig {
            privileged: Some(false),
            pid_mode: Some("host".to_string()),
            network_mode: None,
            binds: None,
        });

        assert!(ensure_manageable_container(&container, server).is_err());
    }

    fn required_labels(server: &ServerConfig) -> BTreeMap<String, String> {
        let mut labels = BTreeMap::new();
        labels.insert(LABEL_MANAGER.to_string(), "true".to_string());
        labels.insert(LABEL_SERVER_ID.to_string(), server.id.clone());
        labels
    }

    fn inspect_with_labels(labels: BTreeMap<String, String>) -> ContainerInspect {
        ContainerInspect {
            name: Some("remiaft-room".to_string()),
            state: Some(ContainerState { running: false }),
            config: Some(ContainerConfig {
                labels: Some(labels),
            }),
            host_config: None,
        }
    }
}
