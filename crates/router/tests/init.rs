//! `phoebis init` writes the storage key, the environment file when one is named, and the bootstrap file, once and at
//! their modes, and prints the admin secret once on stdout unless the environment file takes it. It refuses a file that
//! exists, a data directory that holds a registry and `--owner` off root, and every refusal leaves nothing behind.
//! Under `--tls self-signed` it writes a local CA and a leaf per network listener under `tls/`, and run again against
//! the file it wrote it renews those leaves under the same CA.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use boring::asn1::Asn1Time;
use boring::stack::Stack;
use boring::x509::store::X509StoreBuilder;
use boring::x509::{X509StoreContext, X509};
use phoebis_router::config::ListenerTls;

fn phoebis(dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_phoebis"));
    command
        .current_dir(dir)
        .env_remove("PHOEBIS_ADMIN_SECRET")
        .env_remove("PHOEBIS_CONFIG");
    command
}

fn init(dir: &Path, args: &[&str]) -> Output {
    phoebis(dir).arg("init").args(args).output().unwrap()
}

fn printed(out: &Output) -> String {
    format!(
        "exit {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn text(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

/// Every entry under `dir`, sorted, so a refusal can be shown to have left the tree as it was.
fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// `entries`, with the sixteen hex digits that end each generation directory's name written as `*`.
fn layout(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = entries(dir)
        .into_iter()
        .map(|name| match name.split_once('.') {
            Some((listener, digits))
                if digits.len() == 16 && digits.bytes().all(|b| b.is_ascii_hexdigit()) =>
            {
                format!("{listener}.*")
            }
            _ => name,
        })
        .collect();
    names.sort();
    names
}

/// The secret from the one stdout line `<NAME>=<secret>`, asserting that the line is all stdout carries and that the
/// secret is 43 characters of base64url.
fn secret_line(stdout: &[u8], name: &str) -> String {
    let stdout = std::str::from_utf8(stdout).unwrap();
    let secret = stdout
        .strip_prefix(&format!("{name}="))
        .and_then(|rest| rest.strip_suffix('\n'))
        .unwrap_or_else(|| panic!("stdout is not one {name} line: {stdout:?}"));
    assert_eq!(secret.len(), 43, "{stdout:?}");
    assert!(
        secret
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
        "{stdout:?}"
    );
    secret.to_string()
}

fn is_key_text(key: &str) -> bool {
    key.len() == 65
        && key.ends_with('\n')
        && key[..64]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn path_arg(path: &Path) -> &str {
    path.to_str().unwrap()
}

fn certificate(path: &Path) -> X509 {
    X509::from_pem(&std::fs::read(path).unwrap()).unwrap()
}

/// Whether `leaf` chains to `ca` under a store that holds `ca` and nothing else.
fn verifies_under(ca: &X509, leaf: &X509) -> bool {
    let mut store = X509StoreBuilder::new().unwrap();
    store.add_cert(ca.clone()).unwrap();
    let store = store.build();
    X509StoreContext::new()
        .unwrap()
        .init(&store, leaf, &Stack::<X509>::new().unwrap(), |context| {
            context.verify_cert()
        })
        .unwrap()
}

fn days_left(cert: &X509) -> i32 {
    Asn1Time::days_from_now(0)
        .unwrap()
        .diff(cert.not_after())
        .unwrap()
        .days
}

/// The subject alternative names, DNS names and addresses alike, as text.
fn alt_names(cert: &X509) -> Vec<String> {
    cert.subject_alt_names()
        .unwrap()
        .iter()
        .map(|name| match (name.dnsname(), name.ipaddress()) {
            (Some(dns), _) => dns.to_owned(),
            (None, Some(octets)) if octets.len() == 4 => {
                std::net::IpAddr::from(<[u8; 4]>::try_from(octets).unwrap()).to_string()
            }
            (None, Some(octets)) => {
                std::net::IpAddr::from(<[u8; 16]>::try_from(octets).unwrap()).to_string()
            }
            (None, None) => panic!("a subject alternative name that is neither DNS nor IP"),
        })
        .collect()
}

fn contents(paths: &[PathBuf]) -> Vec<Vec<u8>> {
    paths
        .iter()
        .map(|path| std::fs::read(path).unwrap())
        .collect()
}

/// The first run of the self-signed shape on the proxy alone, off loopback, as the renewal cases start from.
fn init_self_signed(dir: &Path) -> (PathBuf, Output) {
    let config = dir.join("config.yaml");
    let out = init(
        dir,
        &[
            "--tls",
            "self-signed",
            "--proxy-listen",
            "0.0.0.0:0",
            "--non-interactive",
            "--config",
            path_arg(&config),
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", printed(&out));
    (config, out)
}

#[test]
fn init_writes_the_three_artifacts_at_their_modes_and_prints_the_secret_once() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.yaml");
    let out = init(
        dir.path(),
        &["--non-interactive", "--config", path_arg(&config)],
    );
    assert_eq!(out.status.code(), Some(0), "{}", printed(&out));
    let secret = secret_line(&out.stdout, "PHOEBIS_ADMIN_SECRET");
    let key = dir.path().join("storage.key");
    let data = dir.path().join("data");
    assert_eq!(mode(&config), 0o644);
    assert_eq!(mode(&key), 0o600);
    let key_text = text(&key);
    assert!(is_key_text(&key_text), "{key_text:?}");
    assert_eq!(mode(&data), 0o700);
    assert!(entries(&data).is_empty());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains(&secret), "{stderr}");
    for stream in [&out.stdout, &out.stderr] {
        assert!(!String::from_utf8_lossy(stream).contains(key_text.trim_end()));
    }

    let check = |path: &Path| {
        phoebis(dir.path())
            .env("PHOEBIS_ADMIN_SECRET", &secret)
            .args(["check-config", "--config"])
            .arg(path)
            .output()
            .unwrap()
    };
    let checked = check(&config);
    assert_eq!(checked.status.code(), Some(0), "{}", printed(&checked));
    // The required keys alone, the two database paths and the two table files: whatever this file prints from the
    // fifth line on is the binary's own defaults, so the written file spells no value that differs from them.
    let minimal = dir.path().join("minimal.yaml");
    std::fs::write(
        &minimal,
        format!(
            "admin:\n  listen: 127.0.0.1:8081\n  secret_env: PHOEBIS_ADMIN_SECRET\n\
             proxy:\n  listen: 127.0.0.1:8317\n\
             stats:\n  api_prices_path: {}\n  ledger_path: {}\n  subscription_costs_path: {}\n\
             storage:\n  encryption_key_file: {}\n  path: {}\n",
            dir.path().join("api_prices.yaml").display(),
            data.join("ledger.sqlite").display(),
            dir.path().join("subscription_costs.yaml").display(),
            key.display(),
            data.join("router.sqlite").display()
        ),
    )
    .unwrap();
    let defaults = check(&minimal);
    assert_eq!(defaults.status.code(), Some(0), "{}", printed(&defaults));
    let from_the_fifth = |out: &Output| {
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .skip(4)
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    let written = from_the_fifth(&checked);
    assert!(
        written[0].starts_with("proxy: body budget"),
        "{}",
        printed(&checked)
    );
    assert_eq!(written, from_the_fifth(&defaults));
}

/// A pseudo-terminal's two ends: the test holds the near one, and the child gets the far one as stdin and stderr, which
/// is what `init` takes for a terminal to ask on.
fn terminal() -> (std::fs::File, std::fs::File) {
    // SAFETY: each call takes the descriptor this function just opened, and every result is checked before use.
    unsafe {
        let near = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        assert!(near >= 0, "{}", std::io::Error::last_os_error());
        let near = std::fs::File::from_raw_fd(near);
        assert_eq!(libc::grantpt(near.as_raw_fd()), 0);
        assert_eq!(libc::unlockpt(near.as_raw_fd()), 0);
        let mut name = [0 as libc::c_char; 128];
        assert_eq!(
            libc::ptsname_r(near.as_raw_fd(), name.as_mut_ptr(), name.len()),
            0
        );
        let far = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open(std::ffi::CStr::from_ptr(name.as_ptr()).to_str().unwrap())
            .unwrap();
        (near, far)
    }
}

#[test]
fn an_existing_configuration_refuses_before_any_question_on_a_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.yaml");
    std::fs::write(&config, "kept: as it was\n").unwrap();
    let (mut near, far) = terminal();
    let mut command = phoebis(dir.path());
    command
        .args(["init", "--config", path_arg(&config)])
        .stdin(far.try_clone().unwrap())
        .stderr(far)
        .stdout(Stdio::piped());
    let child = command.spawn().unwrap();
    // The child now holds the far end's only descriptors, so reading the near end ends when the child does.
    drop(command);
    // End of input, should a question be asked: the run then ends at once rather than wait for an answer.
    near.write_all(&[4]).unwrap();
    let out = child.wait_with_output().unwrap();
    let mut shown = Vec::new();
    // Linux answers EIO, rather than end of file, once the far end has closed; what was read before it stays.
    let _ = near.read_to_end(&mut shown);
    let shown = String::from_utf8_lossy(&shown);
    assert_eq!(out.status.code(), Some(1), "{shown}");
    assert!(
        shown.contains(&format!(
            "Error: {} already exists; init never overwrites a configuration",
            config.display()
        )),
        "{shown}"
    );
    assert!(!shown.contains("listen address"), "{shown}");
    assert!(out.stdout.is_empty());
    assert_eq!(text(&config), "kept: as it was\n");
    assert_eq!(entries(dir.path()), ["config.yaml"]);
}

#[test]
fn a_second_run_refuses_and_leaves_every_byte() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.yaml");
    let args = ["--non-interactive", "--config", path_arg(&config)];
    let first = init(dir.path(), &args);
    assert_eq!(first.status.code(), Some(0), "{}", printed(&first));
    let key = dir.path().join("storage.key");
    let data = dir.path().join("data");
    let (config_bytes, key_bytes) = (
        std::fs::read(&config).unwrap(),
        std::fs::read(&key).unwrap(),
    );

    let second = init(dir.path(), &args);
    assert_eq!(second.status.code(), Some(1), "{}", printed(&second));
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        stderr.contains(&format!(
            "Error: {} already exists; init never overwrites a configuration",
            config.display()
        )),
        "{stderr}"
    );
    assert!(second.stdout.is_empty(), "{}", printed(&second));
    assert_eq!(std::fs::read(&config).unwrap(), config_bytes);
    assert_eq!(std::fs::read(&key).unwrap(), key_bytes);
    assert_eq!(
        (mode(&config), mode(&key), mode(&data)),
        (0o644, 0o600, 0o700)
    );
    assert!(entries(&data).is_empty());
    assert_eq!(entries(dir.path()), ["config.yaml", "data", "storage.key"]);
}

#[test]
fn a_registry_or_a_ledger_in_the_data_directory_refuses_naming_it() {
    for name in ["router.sqlite", "ledger.sqlite"] {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir(&data).unwrap();
        let database = data.join(name);
        std::fs::write(&database, b"").unwrap();
        let config = dir.path().join("config.yaml");
        let out = init(
            dir.path(),
            &["--non-interactive", "--config", path_arg(&config)],
        );
        assert_eq!(out.status.code(), Some(1), "{name}: {}", printed(&out));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains(&format!(
                "Error: {} exists and a new key cannot open it",
                database.display()
            )),
            "{name}: {stderr}"
        );
        assert!(out.stdout.is_empty());
        assert_eq!(entries(dir.path()), ["data"]);
        assert_eq!(entries(&data), [name]);
    }
}

#[test]
fn an_existing_empty_data_directory_is_used_as_it_is() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o750)).unwrap();
    let config = dir.path().join("config.yaml");
    let out = init(
        dir.path(),
        &["--non-interactive", "--config", path_arg(&config)],
    );
    assert_eq!(out.status.code(), Some(0), "{}", printed(&out));
    assert_eq!(mode(&data), 0o750);
    assert!(entries(&data).is_empty());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains(&format!("{} (", data.display())),
        "{stderr}"
    );
    assert_eq!(entries(dir.path()), ["config.yaml", "data", "storage.key"]);
}

#[test]
fn an_address_off_loopback_is_written_without_a_tls_line_and_passes_check_config() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.yaml");
    let out = init(
        dir.path(),
        &[
            "--proxy-listen",
            "0.0.0.0:0",
            "--non-interactive",
            "--config",
            path_arg(&config),
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", printed(&out));
    let written = text(&config);
    assert!(!written.contains("\n  tls:"), "{written}");
    let loaded = phoebis_router::config::load_unresolved(&config).unwrap();
    assert_eq!(loaded.proxy.listen, "0.0.0.0:0".parse().unwrap());
    assert_eq!(loaded.proxy.tls, None);
    let checked = phoebis(dir.path())
        .args(["check-config", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    assert_eq!(checked.status.code(), Some(0), "{}", printed(&checked));
    assert_eq!(
        String::from_utf8_lossy(&checked.stdout).lines().nth(1),
        Some("proxy listens on 0.0.0.0:0 over plain HTTP"),
        "{}",
        printed(&checked)
    );
    assert!(!dir.path().join("tls").exists());
}

#[test]
fn tls_self_signed_writes_a_ca_and_a_leaf_per_network_listener_and_check_config_passes() {
    let dir = tempfile::tempdir().unwrap();
    let (config, out) = init_self_signed(dir.path());
    let secret = secret_line(&out.stdout, "PHOEBIS_ADMIN_SECRET");
    let tls = dir.path().join("tls");
    assert_eq!(mode(&tls), 0o700);
    assert_eq!(mode(&tls.join("ca.pem")), 0o644);
    for name in ["ca.key", "proxy", "proxy/cert.pem", "proxy/key.pem"] {
        let expected = if name == "proxy" { 0o700 } else { 0o600 };
        assert_eq!(mode(&tls.join(name)), expected, "{name}");
    }
    // The admin listener stays on loopback, so it gets no leaf; the proxy's pair sits in its generation directory
    // behind the link the file names, and nothing staged is left behind.
    assert_eq!(layout(&tls), ["ca.key", "ca.pem", "proxy", "proxy.*"]);
    assert!(std::fs::symlink_metadata(tls.join("proxy"))
        .unwrap()
        .file_type()
        .is_symlink());
    let loaded = phoebis_router::config::load_unresolved(&config).unwrap();
    assert_eq!(
        loaded.proxy.tls,
        Some(ListenerTls {
            cert_file: tls.join("proxy/cert.pem"),
            key_file: tls.join("proxy/key.pem"),
        })
    );
    assert_eq!(loaded.admin.tls, None);

    let checked = phoebis(dir.path())
        .env("PHOEBIS_ADMIN_SECRET", &secret)
        .args(["check-config", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    assert_eq!(checked.status.code(), Some(0), "{}", printed(&checked));
    let line = String::from_utf8_lossy(&checked.stdout)
        .lines()
        .nth(1)
        .unwrap_or_default()
        .to_string();
    assert!(
        line.starts_with(&format!(
            "proxy listens on 0.0.0.0:0 over TLS (certificate {}, expires ",
            tls.join("proxy/cert.pem").display()
        )),
        "{}",
        printed(&checked)
    );

    let (ca, leaf) = (
        certificate(&tls.join("ca.pem")),
        certificate(&tls.join("proxy/cert.pem")),
    );
    assert!(verifies_under(&ca, &leaf));
    let names = alt_names(&leaf);
    for name in ["localhost", "127.0.0.1", "::1"] {
        assert!(names.iter().any(|n| n == name), "{name} in {names:?}");
    }
    assert!(
        (364..=366).contains(&days_left(&leaf)),
        "{}",
        days_left(&leaf)
    );
    assert!(
        (3_649..=3_651).contains(&days_left(&ca)),
        "{}",
        days_left(&ca)
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&format!(
            "clients must trust {}; copy it to each client and set NODE_EXTRA_CA_CERTS to the copy",
            tls.join("ca.pem").display()
        )),
        "{stderr}"
    );
    assert!(!stderr.contains(&secret), "{stderr}");
}

#[test]
fn tls_files_without_the_pair_flags_refuses_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.yaml");
    let out = init(
        dir.path(),
        &[
            "--tls",
            "files",
            "--proxy-listen",
            "0.0.0.0:0",
            "--non-interactive",
            "--config",
            path_arg(&config),
        ],
    );
    assert_eq!(out.status.code(), Some(1), "{}", printed(&out));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(
            "Error: --tls files needs --proxy-cert and --proxy-key for proxy on 0.0.0.0:0"
        ),
        "{stderr}"
    );
    assert!(out.stdout.is_empty(), "{}", printed(&out));
    assert!(entries(dir.path()).is_empty());
}

#[test]
fn a_second_run_with_tls_self_signed_renews_the_leaves_under_the_same_ca_and_touches_nothing_else()
{
    let dir = tempfile::tempdir().unwrap();
    let (config, _) = init_self_signed(dir.path());
    let tls = dir.path().join("tls");
    let kept = [
        config.clone(),
        dir.path().join("storage.key"),
        tls.join("ca.pem"),
        tls.join("ca.key"),
    ];
    let renewed = [tls.join("proxy/cert.pem"), tls.join("proxy/key.pem")];
    // The modes the service unit's pair carries, so each renewed file shows it kept its own.
    std::fs::set_permissions(&renewed[0], std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::set_permissions(&renewed[1], std::fs::Permissions::from_mode(0o640)).unwrap();
    let (kept_before, renewed_before) = (contents(&kept), contents(&renewed));
    let tree = entries(dir.path());

    let out = init(
        dir.path(),
        &["--tls", "self-signed", "--config", path_arg(&config)],
    );
    assert_eq!(out.status.code(), Some(0), "{}", printed(&out));
    assert!(out.stdout.is_empty(), "{}", printed(&out));
    assert_eq!(contents(&kept), kept_before);
    let renewed_after = contents(&renewed);
    assert_ne!(renewed_after[0], renewed_before[0]);
    assert_ne!(renewed_after[1], renewed_before[1]);
    assert!(verifies_under(
        &certificate(&tls.join("ca.pem")),
        &certificate(&tls.join("proxy/cert.pem"))
    ));
    assert_eq!((mode(&renewed[0]), mode(&renewed[1])), (0o644, 0o640));
    // The link names the new generation; the one it replaced stays until the next renewal.
    assert_eq!(
        layout(&tls),
        [
            "ca.key",
            "ca.pem",
            "proxy",
            "proxy.*",
            "proxy.*",
            "renewal.lock"
        ]
    );
    assert_eq!(entries(dir.path()), tree);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&tls.join("proxy/cert.pem").display().to_string()),
        "{stderr}"
    );
    assert!(
        stderr.ends_with("next: stop and start the router, which reads the pair at start\n"),
        "{stderr}"
    );
}

#[test]
fn a_renewal_with_another_flag_or_against_a_file_without_a_generated_pair_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let (config, _) = init_self_signed(dir.path());
    let tls = dir.path().join("tls");
    let files = [
        config.clone(),
        dir.path().join("storage.key"),
        tls.join("ca.pem"),
        tls.join("ca.key"),
        tls.join("proxy/cert.pem"),
        tls.join("proxy/key.pem"),
    ];
    let before = contents(&files);
    let flagged = init(
        dir.path(),
        &[
            "--tls",
            "self-signed",
            "--proxy-listen",
            "0.0.0.0:1",
            "--config",
            path_arg(&config),
        ],
    );
    assert_eq!(flagged.status.code(), Some(1), "{}", printed(&flagged));
    assert!(
        String::from_utf8_lossy(&flagged.stderr).contains(
            "Error: init --tls self-signed against an existing bootstrap file renews its leaves and takes no other flag"
        ),
        "{}",
        printed(&flagged)
    );
    assert!(flagged.stdout.is_empty(), "{}", printed(&flagged));
    assert_eq!(contents(&files), before);

    let plain = tempfile::tempdir().unwrap();
    let config = plain.path().join("config.yaml");
    let first = init(
        plain.path(),
        &["--non-interactive", "--config", path_arg(&config)],
    );
    assert_eq!(first.status.code(), Some(0), "{}", printed(&first));
    let out = init(
        plain.path(),
        &["--tls", "self-signed", "--config", path_arg(&config)],
    );
    assert_eq!(out.status.code(), Some(1), "{}", printed(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(&format!(
            "Error: {} names no pair under {}; init --tls self-signed renews only the pairs it wrote",
            config.display(),
            plain.path().join("tls").display()
        )),
        "{}",
        printed(&out)
    );
    assert!(!plain.path().join("tls").exists());
}

/// The pair the file names for the proxy listener: the certificate then the key.
fn configured_pair(config: &Path) -> (PathBuf, PathBuf) {
    let tls = phoebis_router::config::load_unresolved(config)
        .unwrap()
        .proxy
        .tls
        .expect("the proxy listener names a pair");
    (tls.cert_file, tls.key_file)
}

/// Whether the key file holds the private half of the certificate file. The dates are not read: a leaf signed a
/// moment ago can read as not yet valid to a date check on the kernel's coarse clock.
fn consistent(cert_file: &Path, key_file: &Path) -> bool {
    let key = boring::pkey::PKey::private_key_from_pem(&std::fs::read(key_file).unwrap()).unwrap();
    certificate(cert_file).public_key().unwrap().public_eq(&key)
}

/// The line a renewal refuses with while another holds the lock under `tls`.
fn locked_line(tls: &Path) -> String {
    format!(
        "{}: another renewal holds the lock; run this one again once it finishes",
        tls.join("renewal.lock").display()
    )
}

#[test]
fn a_renewal_refuses_while_another_holds_the_lock_and_leaves_the_pair_as_it_was() {
    let dir = tempfile::tempdir().unwrap();
    let (config, _) = init_self_signed(dir.path());
    let tls = dir.path().join("tls");
    let (cert, key) = configured_pair(&config);
    let pair = [cert.clone(), key.clone()];
    let before = contents(&pair);
    let held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(tls.join("renewal.lock"))
        .unwrap();
    held.try_lock().unwrap();

    let renew = ["--tls", "self-signed", "--config", path_arg(&config)];
    let out = init(dir.path(), &renew);
    assert_eq!(out.status.code(), Some(1), "{}", printed(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(&locked_line(&tls)),
        "{}",
        printed(&out)
    );
    assert_eq!(contents(&pair), before);

    drop(held);
    let out = init(dir.path(), &renew);
    assert_eq!(out.status.code(), Some(0), "{}", printed(&out));
    assert_ne!(contents(&pair), before);
    assert!(consistent(&cert, &key));
}

#[test]
fn two_renewals_started_together_leave_one_consistent_pair_and_refuse_the_second() {
    let dir = tempfile::tempdir().unwrap();
    let (config, _) = init_self_signed(dir.path());
    let tls = dir.path().join("tls");
    let renewal = phoebis_router::init::Renewal {
        config_path: config.clone(),
        tls_dir: tls.clone(),
        listeners: vec![("proxy", "0.0.0.0:0".parse().unwrap())],
    };
    let (cert, key) = configured_pair(&config);
    for round in 0..20 {
        let start = std::sync::Barrier::new(2);
        let results: Vec<Result<(), String>> = std::thread::scope(|scope| {
            let runs: Vec<_> = (0..2)
                .map(|_| {
                    scope.spawn(|| {
                        start.wait();
                        phoebis_router::init::renew(&renewal, &mut Vec::new())
                            .map_err(|error| error.to_string())
                    })
                })
                .collect();
            runs.into_iter().map(|run| run.join().unwrap()).collect()
        });
        assert!(
            results.iter().any(Result::is_ok),
            "round {round}: {results:?}"
        );
        for result in &results {
            if let Err(error) = result {
                assert!(
                    error.ends_with(&locked_line(&tls)),
                    "round {round}: {results:?}"
                );
            }
        }
        assert!(consistent(&cert, &key), "round {round}");
    }
    // Nothing staged is left, and the generations before the one each switch replaced are gone.
    assert_eq!(
        layout(&tls),
        [
            "ca.key",
            "ca.pem",
            "proxy",
            "proxy.*",
            "proxy.*",
            "renewal.lock"
        ]
    );
}

#[test]
fn an_interruption_at_the_switch_leaves_the_previous_pair_live_and_loadable() {
    let dir = tempfile::tempdir().unwrap();
    let (config, _) = init_self_signed(dir.path());
    let tls = dir.path().join("tls");
    let (cert, key) = configured_pair(&config);
    let pair = [cert.clone(), key.clone()];
    let before = contents(&pair);
    // A directory at the switch target, holding the live pair, takes no rename onto it, so the run stops after the
    // new pair is written and before it goes live.
    let switch = cert.parent().unwrap().to_path_buf();
    let live = std::fs::canonicalize(&switch).unwrap();
    std::fs::remove_file(&switch).unwrap();
    std::fs::create_dir(&switch).unwrap();
    for file in &pair {
        std::fs::copy(live.join(file.file_name().unwrap()), file).unwrap();
    }
    let tree = entries(&tls);

    let out = init(
        dir.path(),
        &["--tls", "self-signed", "--config", path_arg(&config)],
    );
    assert_eq!(out.status.code(), Some(1), "{}", printed(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(&switch.display().to_string()),
        "{}",
        printed(&out)
    );
    assert_eq!(contents(&pair), before);
    if let Err(error) = phoebis_router::tls::load_pair("proxy.tls", &cert, &key) {
        panic!("{error}");
    }
    // Nothing the run staged is left beside the pair; the lock file stays for the next run.
    let after: Vec<String> = entries(&tls)
        .into_iter()
        .filter(|name| name != "renewal.lock")
        .collect();
    assert_eq!(after, tree);
}

/// Each path's mode, owner and bytes, so a refused run can be shown to have left them as they were.
fn kept(paths: &[PathBuf]) -> Vec<(u32, u32, Vec<u8>)> {
    paths
        .iter()
        .map(|path| {
            let metadata = std::fs::metadata(path).unwrap();
            (
                metadata.mode() & 0o777,
                metadata.uid(),
                std::fs::read(path).unwrap(),
            )
        })
        .collect()
}

#[test]
fn a_fresh_init_refuses_an_existing_leaf_and_keeps_its_bytes_owner_and_mode() {
    let dir = tempfile::tempdir().unwrap();
    let tls = dir.path().join("tls");
    // A first installation in the directory, with storage paths of its own.
    let first = dir.path().join("first.yaml");
    let out = init(
        dir.path(),
        &[
            "--tls",
            "self-signed",
            "--proxy-listen",
            "0.0.0.0:0",
            "--key-file",
            path_arg(&dir.path().join("first.key")),
            "--data-dir",
            path_arg(&dir.path().join("first-data")),
            "--non-interactive",
            "--config",
            path_arg(&first),
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", printed(&out));
    let (cert, key) = configured_pair(&first);
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o640)).unwrap();
    let pair = [cert.clone(), key.clone()];
    let (pair_before, tls_before, tree_before) = (kept(&pair), entries(&tls), entries(dir.path()));

    let second = dir.path().join("second.yaml");
    let out = init(
        dir.path(),
        &[
            "--tls",
            "self-signed",
            "--proxy-listen",
            "0.0.0.0:0",
            "--key-file",
            path_arg(&dir.path().join("second.key")),
            "--data-dir",
            path_arg(&dir.path().join("second-data")),
            "--non-interactive",
            "--config",
            path_arg(&second),
        ],
    );
    assert_eq!(out.status.code(), Some(1), "{}", printed(&out));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&format!(
            "Error: {} already exists; init never overwrites a certificate pair",
            tls.join("proxy").display()
        )),
        "{stderr}"
    );
    assert!(out.stdout.is_empty(), "{}", printed(&out));
    assert_eq!(kept(&pair), pair_before);
    assert_eq!(entries(&tls), tls_before);
    assert_eq!(entries(dir.path()), tree_before);
}

#[test]
fn a_failure_after_the_leaves_land_removes_only_this_runs_files() {
    // Root writes through the read-only directory that makes the run fail here.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let conf = dir.path().join("conf");
    let tls = conf.join("tls");
    let state = dir.path().join("state");
    std::fs::create_dir(&state).unwrap();
    // A CA the run reuses, and an operator's pair named under tls/ through the pair flags, beside the names a
    // generated leaf takes.
    phoebis_router::selfsigned::write_ca(
        &tls,
        &phoebis_router::selfsigned::hostname(),
        &phoebis_router::selfsigned::leaf_names("0.0.0.0:0".parse().unwrap()),
        phoebis_router::selfsigned::Validity::days_from_now(
            phoebis_router::selfsigned::CA_LIFETIME_DAYS,
        ),
    )
    .unwrap();
    fake_upstream::generate_pair(&["localhost"]).write_to(&tls, "proxy");
    let theirs: Vec<PathBuf> = entries(&tls).iter().map(|name| tls.join(name)).collect();
    let before = kept(&theirs);
    // The bootstrap file cannot be created, so the run stops after its leaf is live.
    std::fs::set_permissions(&conf, std::fs::Permissions::from_mode(0o555)).unwrap();
    let out = init(
        dir.path(),
        &[
            "--tls",
            "self-signed",
            "--proxy-listen",
            "0.0.0.0:0",
            "--key-file",
            path_arg(&state.join("storage.key")),
            "--data-dir",
            path_arg(&state.join("data")),
            "--non-interactive",
            "--config",
            path_arg(&conf.join("config.yaml")),
        ],
    );
    std::fs::set_permissions(&conf, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", printed(&out));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("removed what this run wrote: "), "{stderr}");
    assert!(!stderr.contains("could not remove"), "{stderr}");
    assert_eq!(
        entries(&tls)
            .iter()
            .map(|name| tls.join(name))
            .collect::<Vec<_>>(),
        theirs
    );
    assert_eq!(kept(&theirs), before);
    assert!(entries(&state).is_empty(), "{:?}", entries(&state));
}

#[test]
fn a_storage_key_or_environment_file_on_a_path_the_tls_material_takes_refuses_before_writing() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.yaml");
    let tls = dir.path().join("tls");
    for (flag, path, what, overlapped) in [
        (
            "--key-file",
            tls.join("ca.pem"),
            "storage key",
            tls.join("ca.pem"),
        ),
        (
            "--env-file",
            tls.join("proxy").join("phoebis.env"),
            "environment file",
            tls.join("proxy"),
        ),
    ] {
        let out = init(
            dir.path(),
            &[
                "--tls",
                "self-signed",
                "--proxy-listen",
                "0.0.0.0:0",
                flag,
                path_arg(&path),
                "--non-interactive",
                "--config",
                path_arg(&config),
            ],
        );
        assert_eq!(out.status.code(), Some(1), "{}", printed(&out));
        assert!(
            String::from_utf8_lossy(&out.stderr).contains(&format!(
                "Error: the {what} {} overlaps {}, which --tls self-signed writes; name the {what} elsewhere",
                path.display(),
                overlapped.display()
            )),
            "{}",
            printed(&out)
        );
        assert!(entries(dir.path()).is_empty(), "{:?}", entries(dir.path()));
    }
}

#[test]
fn a_failure_after_the_first_write_removes_what_the_run_wrote() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("new").join("config.yaml");
    // One path for both files passes every check before the first write, and the second write then fails, even
    // under root, which a read-only directory would not stop.
    let same = dir.path().join("new").join("secrets");
    let out = init(
        dir.path(),
        &[
            "--key-file",
            path_arg(&same),
            "--env-file",
            path_arg(&same),
            "--non-interactive",
            "--config",
            path_arg(&config),
        ],
    );
    assert_eq!(out.status.code(), Some(1), "{}", printed(&out));
    assert!(out.stdout.is_empty(), "{}", printed(&out));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(
        stderr.contains(&format!(
            "removed what this run wrote: {}, {}",
            same.display(),
            dir.path().join("new").display()
        )),
        "{stderr}"
    );
    assert!(entries(dir.path()).is_empty());
}

#[test]
fn a_pair_is_written_as_given_and_a_tls_word_flag_is_a_usage_error() {
    let word = tempfile::tempdir().unwrap();
    let config = word.path().join("config.yaml");
    let out = init(
        word.path(),
        &[
            "--proxy-tls",
            "self_signed",
            "--non-interactive",
            "--config",
            path_arg(&config),
        ],
    );
    assert_eq!(out.status.code(), Some(2), "{}", printed(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("unexpected argument"),
        "{}",
        printed(&out)
    );
    assert!(entries(word.path()).is_empty());

    let pair = tempfile::tempdir().unwrap();
    let config = pair.path().join("config.yaml");
    let out = init(
        pair.path(),
        &[
            "--proxy-cert",
            "a.pem",
            "--proxy-key",
            "a.key",
            "--non-interactive",
            "--config",
            path_arg(&config),
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", printed(&out));
    // A relative path is taken against the directory `init` ran in, which is what the operator typed it against.
    let here = pair.path().canonicalize().unwrap();
    let (cert, key) = (here.join("a.pem"), here.join("a.key"));
    let written = text(&config);
    assert!(
        written.contains(&format!(
            "proxy:\n  listen: 127.0.0.1:8317\n  tls:\n    cert_file: {}\n    key_file: {}\n",
            cert.display(),
            key.display()
        )),
        "{written}"
    );
    let loaded = phoebis_router::config::load_unresolved(&config).unwrap();
    assert_eq!(
        loaded.proxy.tls,
        Some(ListenerTls {
            cert_file: cert,
            key_file: key
        })
    );
}

#[test]
fn an_env_file_takes_the_secret_off_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.yaml");
    let env_file = dir.path().join("phoebis.env");
    let out = init(
        dir.path(),
        &[
            "--env-file",
            path_arg(&env_file),
            "--non-interactive",
            "--config",
            path_arg(&config),
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", printed(&out));
    assert!(out.stdout.is_empty(), "{}", printed(&out));
    assert_eq!(mode(&env_file), 0o600);
    let env = text(&env_file);
    let (first, rest) = env.split_once('\n').unwrap();
    let secret = secret_line(format!("{first}\n").as_bytes(), "PHOEBIS_ADMIN_SECRET");
    assert_eq!(rest, "RUST_LOG=info\n");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains(&secret), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "the admin secret is in PHOEBIS_ADMIN_SECRET, written to {}",
            env_file.display()
        )),
        "{stderr}"
    );
}

#[test]
fn the_container_shape_writes_the_compose_directory() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.yaml");
    let out = init(dir.path(), &["--container", "--config", path_arg(&config)]);
    assert_eq!(out.status.code(), Some(0), "{}", printed(&out));
    assert!(out.stdout.is_empty(), "{}", printed(&out));
    // Nothing of the container's data directory exists on the host, so none is created here.
    assert_eq!(
        entries(dir.path()),
        ["config.yaml", "phoebis.env", "storage.key"]
    );
    assert_eq!(mode(&dir.path().join("storage.key")), 0o600);
    assert!(is_key_text(&text(&dir.path().join("storage.key"))));
    assert_eq!(mode(&dir.path().join("phoebis.env")), 0o600);
    let written = text(&config);
    for line in [
        "  encryption_key_file: /run/secrets/router-storage-key\n",
        "  path: /var/lib/phoebis/router.sqlite\n",
        "  ledger_path: /var/lib/phoebis/ledger.sqlite\n",
        "  api_prices_path: /var/lib/phoebis/api_prices.yaml\n",
        "  subscription_costs_path: /var/lib/phoebis/subscription_costs.yaml\n",
        "  listen: 0.0.0.0:8317\n",
        "  listen: 0.0.0.0:8081\n",
        "  metrics_listen: 0.0.0.0:9464\n",
    ] {
        assert!(written.contains(line), "{line:?} in\n{written}");
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("own storage.key by uid 65532 at mode 0400; copy deploy/compose.yaml with the image tag set"),
        "{stderr}"
    );

    // The loader reads the YAML and checks its shape, never the key file, which lives inside the container.
    let loaded = phoebis_router::config::load_unresolved(&config).unwrap();
    assert_eq!(
        loaded.storage.encryption_key_file,
        Path::new("/run/secrets/router-storage-key")
    );
    assert_eq!(
        loaded.storage.path,
        Path::new("/var/lib/phoebis/router.sqlite")
    );
    assert_eq!(
        loaded.stats.ledger_path,
        Path::new("/var/lib/phoebis/ledger.sqlite")
    );
    assert_eq!(
        loaded.stats.api_prices_path.as_deref(),
        Some(Path::new("/var/lib/phoebis/api_prices.yaml"))
    );
    assert_eq!(
        loaded.stats.subscription_costs_path.as_deref(),
        Some(Path::new("/var/lib/phoebis/subscription_costs.yaml"))
    );
    assert_eq!(loaded.proxy.listen, "0.0.0.0:8317".parse().unwrap());
    assert_eq!(loaded.admin.listen, "0.0.0.0:8081".parse().unwrap());
    assert!(
        loaded.proxy.tls.is_none() && loaded.admin.tls.is_none(),
        "the container's listeners serve plain HTTP: {:?} {:?}",
        loaded.proxy,
        loaded.admin
    );
    assert_eq!(loaded.stats.metrics_listen, "0.0.0.0:9464".parse().unwrap());
}

#[test]
fn the_container_shape_with_tls_self_signed_writes_tls_beside_the_file_and_names_the_containers_paths(
) {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.yaml");
    let out = init(
        dir.path(),
        &[
            "--container",
            "--tls",
            "self-signed",
            "--config",
            path_arg(&config),
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", printed(&out));
    let tls = dir.path().join("tls");
    assert_eq!(
        entries(dir.path()),
        ["config.yaml", "phoebis.env", "storage.key", "tls"]
    );
    assert_eq!(
        layout(&tls),
        ["admin", "admin.*", "ca.key", "ca.pem", "proxy", "proxy.*"]
    );
    let loaded = phoebis_router::config::load_unresolved(&config).unwrap();
    let inside = |name: &str| ListenerTls {
        cert_file: PathBuf::from(format!("/etc/phoebis/tls/{name}/cert.pem")),
        key_file: PathBuf::from(format!("/etc/phoebis/tls/{name}/key.pem")),
    };
    assert_eq!(loaded.proxy.tls, Some(inside("proxy")));
    assert_eq!(loaded.admin.tls, Some(inside("admin")));
    let stderr = String::from_utf8_lossy(&out.stderr);
    let remaining = stderr
        .lines()
        .find(|line| line.starts_with("remaining: "))
        .unwrap_or_default();
    assert!(
        remaining.ends_with(concat!(
            "; mount ./tls read-only at /etc/phoebis/tls",
            " and own tls/proxy/key.pem and tls/admin/key.pem by uid 65532 at mode 0400"
        )),
        "{stderr}"
    );
    assert!(!remaining.contains("ca.key"), "{stderr}");
    // uid 65532 reads the directories and both certificates through the bind mount, following each relative link
    // inside it; each leaf key waits for the hand-over the line names, and the CA's key is never handed over, so the
    // container cannot read it.
    for name in ["", "proxy", "admin"] {
        assert_eq!(mode(&tls.join(name)), 0o755, "{name}");
    }
    for name in ["proxy", "admin"] {
        let target = std::fs::read_link(tls.join(name)).unwrap();
        assert!(target.is_relative(), "{}", target.display());
    }
    for name in ["ca.pem", "proxy/cert.pem", "admin/cert.pem"] {
        assert_eq!(mode(&tls.join(name)), 0o644, "{name}");
    }
    for name in ["ca.key", "proxy/key.pem", "admin/key.pem"] {
        assert_eq!(mode(&tls.join(name)), 0o600, "{name}");
    }
    // The clients copy the CA from the host, so the line names the host's path.
    assert!(
        stderr.contains(&format!(
            "clients must trust {};",
            tls.join("ca.pem").display()
        )),
        "{stderr}"
    );
}

#[test]
fn the_container_shape_renews_its_leaves_under_tls_beside_the_compose_file() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.yaml");
    let first = init(
        dir.path(),
        &[
            "--container",
            "--tls",
            "self-signed",
            "--config",
            path_arg(&config),
        ],
    );
    assert_eq!(first.status.code(), Some(0), "{}", printed(&first));
    let tls = dir.path().join("tls");
    let kept = [
        config.clone(),
        dir.path().join("storage.key"),
        dir.path().join("phoebis.env"),
        tls.join("ca.pem"),
        tls.join("ca.key"),
    ];
    let renewed = [
        tls.join("proxy/cert.pem"),
        tls.join("proxy/key.pem"),
        tls.join("admin/cert.pem"),
        tls.join("admin/key.pem"),
    ];
    let (kept_before, renewed_before) = (contents(&kept), contents(&renewed));

    // The file names the container's paths; the renewal writes the pairs mounted there, under tls/ on the host.
    let out = init(
        dir.path(),
        &["--tls", "self-signed", "--config", path_arg(&config)],
    );
    assert_eq!(out.status.code(), Some(0), "{}", printed(&out));
    assert!(out.stdout.is_empty(), "{}", printed(&out));
    assert_eq!(contents(&kept), kept_before);
    let renewed_after = contents(&renewed);
    for (at, path) in renewed.iter().enumerate() {
        assert_ne!(renewed_after[at], renewed_before[at], "{}", path.display());
    }
    let ca = certificate(&tls.join("ca.pem"));
    for name in ["proxy/cert.pem", "admin/cert.pem"] {
        assert!(verifies_under(&ca, &certificate(&tls.join(name))), "{name}");
        assert_eq!(mode(&tls.join(name)), 0o644, "{name}");
    }
    for name in ["proxy/key.pem", "admin/key.pem"] {
        assert_eq!(mode(&tls.join(name)), 0o600, "{name}");
    }
    for name in ["proxy", "admin"] {
        assert_eq!(mode(&tls.join(name)), 0o755, "{name}");
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    for name in ["proxy/cert.pem", "admin/cert.pem"] {
        assert!(
            stderr.contains(&tls.join(name).display().to_string()),
            "{stderr}"
        );
    }
    assert!(
        stderr.ends_with("next: stop and start the router, which reads the pair at start\n"),
        "{stderr}"
    );
}

#[test]
fn a_first_run_reuses_a_complete_ca_under_tls_and_a_refused_run_keeps_it() {
    use phoebis_router::selfsigned::{self, SanName, Validity, CA_LIFETIME_DAYS};

    let dir = tempfile::tempdir().unwrap();
    let tls = dir.path().join("tls");
    selfsigned::write_ca(
        &tls,
        &selfsigned::hostname(),
        &selfsigned::leaf_names("0.0.0.0:0".parse().unwrap()),
        Validity::days_from_now(CA_LIFETIME_DAYS),
    )
    .unwrap();
    let ca_files = [tls.join("ca.pem"), tls.join("ca.key")];
    let ca_before = contents(&ca_files);
    let (_, out) = init_self_signed(dir.path());
    assert_eq!(contents(&ca_files), ca_before);
    assert!(verifies_under(
        &certificate(&tls.join("ca.pem")),
        &certificate(&tls.join("proxy/cert.pem"))
    ));
    assert_eq!(layout(&tls), ["ca.key", "ca.pem", "proxy", "proxy.*"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains(&format!("wrote {}", tls.join("ca.pem").display())),
        "the reused CA is not this run's: {stderr}"
    );

    // A CA whose constraint leaves out this host's names refuses the leaf after the key file landed; the run removes
    // what it wrote and keeps the CA it found.
    let other = tempfile::tempdir().unwrap();
    let tls = other.path().join("tls");
    selfsigned::write_ca(
        &tls,
        "other.example",
        &[SanName::Dns("other.example".into())],
        Validity::days_from_now(CA_LIFETIME_DAYS),
    )
    .unwrap();
    let ca_files = [tls.join("ca.pem"), tls.join("ca.key")];
    let ca_before = contents(&ca_files);
    let config = other.path().join("config.yaml");
    let refused = init(
        other.path(),
        &[
            "--tls",
            "self-signed",
            "--proxy-listen",
            "0.0.0.0:0",
            "--non-interactive",
            "--config",
            path_arg(&config),
        ],
    );
    assert_eq!(refused.status.code(), Some(1), "{}", printed(&refused));
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("are not all within the CA's name constraint"),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "removed what this run wrote: {}",
            other.path().join("data").display()
        )),
        "{stderr}"
    );
    assert!(refused.stdout.is_empty(), "{}", printed(&refused));
    assert_eq!(contents(&ca_files), ca_before);
    assert_eq!(entries(other.path()), ["tls"]);
    assert_eq!(entries(&tls), ["ca.key", "ca.pem"]);
}

#[test]
fn the_parents_this_run_creates_are_0700_and_an_existing_one_is_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let have = dir.path().join("have");
    std::fs::create_dir(&have).unwrap();
    std::fs::set_permissions(&have, std::fs::Permissions::from_mode(0o755)).unwrap();
    let new = dir.path().join("new");
    let secrets = new.join("secrets");
    let key = secrets.join("storage.key");
    let data = have.join("data");
    let config = dir.path().join("config.yaml");
    let args = [
        "--key-file",
        path_arg(&key),
        "--data-dir",
        path_arg(&data),
        "--non-interactive",
        "--config",
        path_arg(&config),
    ];
    let out = init(dir.path(), &args);
    assert_eq!(out.status.code(), Some(0), "{}", printed(&out));
    assert_eq!(
        (mode(&new), mode(&secrets), mode(&have), mode(&data)),
        (0o700, 0o700, 0o755, 0o700)
    );
    assert_eq!(mode(&key), 0o600);
    let stderr = String::from_utf8_lossy(&out.stderr);
    for created in [&new, &secrets, &data] {
        assert!(
            stderr.contains(&format!(
                "created {} (directory, mode 0700",
                created.display()
            )),
            "{} in\n{stderr}",
            created.display()
        );
    }
    assert!(
        !stderr.contains(&format!("{} (", have.display())),
        "{stderr}"
    );

    let second = init(dir.path(), &args);
    assert_eq!(second.status.code(), Some(1), "{}", printed(&second));
    assert_eq!(
        (mode(&new), mode(&secrets), mode(&have), mode(&data)),
        (0o700, 0o700, 0o755, 0o700)
    );
    assert_eq!(entries(dir.path()), ["config.yaml", "have", "new"]);
    assert_eq!(entries(&new), ["secrets"]);
    assert_eq!(entries(&secrets), ["storage.key"]);
    assert_eq!(entries(&have), ["data"]);
    assert!(entries(&data).is_empty());
}

/// The uid and the primary gid of `nobody`.
fn nobody() -> (u32, u32) {
    let entry = unsafe { libc::getpwnam(c"nobody".as_ptr()) };
    assert!(!entry.is_null(), "this host has no user nobody");
    unsafe { ((*entry).pw_uid, (*entry).pw_gid) }
}

#[test]
#[ignore = "needs uid 0; the owner runs it"]
fn the_owner_receives_the_key_file_the_data_directory_and_the_created_parents() {
    use std::os::unix::process::CommandExt;

    assert_eq!(unsafe { libc::geteuid() }, 0, "run this test as root");
    let (uid, gid) = nobody();
    let dir = tempfile::tempdir().unwrap();
    // The test's own directory stands where /etc does for the service: an existing parent the owner may traverse.
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let new = dir.path().join("new");
    let secrets = new.join("secrets");
    let key = secrets.join("storage.key");
    let data = dir.path().join("data");
    let config = dir.path().join("config.yaml");
    let env_dir = dir.path().join("env");
    let env_file = env_dir.join("phoebis.env");
    let out = init(
        dir.path(),
        &[
            "--owner",
            "nobody",
            "--key-file",
            path_arg(&key),
            "--data-dir",
            path_arg(&data),
            "--env-file",
            path_arg(&env_file),
            "--tls",
            "self-signed",
            "--proxy-listen",
            "0.0.0.0:0",
            "--non-interactive",
            "--config",
            path_arg(&config),
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", printed(&out));
    for path in [&new, &secrets, &key, &data] {
        let metadata = std::fs::metadata(path).unwrap();
        assert_eq!(
            (metadata.uid(), metadata.gid()),
            (uid, gid),
            "{}",
            path.display()
        );
    }
    assert_eq!((mode(&new), mode(&secrets)), (0o700, 0o700));
    // The environment file, and the directory created for it alone, stay root's: systemd reads the file as root.
    for path in [
        dir.path(),
        config.as_path(),
        env_dir.as_path(),
        env_file.as_path(),
    ] {
        assert_eq!(
            std::fs::metadata(path).unwrap().uid(),
            0,
            "{}",
            path.display()
        );
    }
    let read = Command::new("cat")
        .uid(uid)
        .gid(gid)
        .arg(&key)
        .output()
        .unwrap();
    assert_eq!(read.status.code(), Some(0), "{}", printed(&read));
    assert!(is_key_text(&String::from_utf8_lossy(&read.stdout)));

    // The pair as the unit wants an operator's: the service reads its leaf and key through the group and never signs.
    let tls = dir.path().join("tls");
    let owned = |path: &Path| {
        let metadata = std::fs::metadata(path).unwrap();
        (metadata.mode() & 0o777, metadata.uid(), metadata.gid())
    };
    assert_eq!(owned(&tls), (0o750, 0, gid));
    assert_eq!(owned(&tls.join("proxy")), (0o750, 0, gid));
    assert_eq!(owned(&tls.join("proxy/cert.pem")).0, 0o644);
    assert_eq!(owned(&tls.join("proxy/cert.pem")).1, 0);
    assert_eq!(owned(&tls.join("proxy/key.pem")), (0o640, 0, gid));
    assert_eq!(owned(&tls.join("ca.key")), (0o600, 0, 0));
    let read_as_owner = |path: &Path| {
        Command::new("cat")
            .uid(uid)
            .gid(gid)
            .arg(path)
            .output()
            .unwrap()
    };
    let leaf_key = read_as_owner(&tls.join("proxy/key.pem"));
    assert_eq!(leaf_key.status.code(), Some(0), "{}", printed(&leaf_key));
    let ca_key = read_as_owner(&tls.join("ca.key"));
    assert_ne!(ca_key.status.code(), Some(0), "{}", printed(&ca_key));
}

#[test]
fn owner_off_root_refuses() {
    // Under root `--owner` is allowed, which the ignored root case proves in this same binary.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let config: PathBuf = dir.path().join("config.yaml");
    let out = init(
        dir.path(),
        &[
            "--owner",
            "nobody",
            "--non-interactive",
            "--config",
            path_arg(&config),
        ],
    );
    assert_eq!(out.status.code(), Some(1), "{}", printed(&out));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Error: --owner needs root"), "{stderr}");
    assert!(entries(dir.path()).is_empty());
}
