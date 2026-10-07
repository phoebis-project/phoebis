//! `phoebis init` writes the storage key, the environment file when one is named, and the bootstrap file, once and at
//! their modes, and prints the admin secret once on stdout unless the environment file takes it. It refuses a file that
//! exists, a data directory that holds a registry and `--owner` off root, and every refusal leaves nothing behind.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

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
    // The required keys alone and the two database paths: whatever this file prints from the fifth line on is the
    // binary's own defaults, so the written file spells no value that differs from them.
    let minimal = dir.path().join("minimal.yaml");
    std::fs::write(
        &minimal,
        format!(
            "admin:\n  listen: 127.0.0.1:8081\n  secret_env: PHOEBIS_ADMIN_SECRET\n\
             proxy:\n  listen: 127.0.0.1:8317\n\
             stats:\n  ledger_path: {}\n\
             storage:\n  encryption_key_file: {}\n  path: {}\n",
            data.join("ledger.sqlite").display(),
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

/// `init` with `args` on a terminal that types `answers` and then ends its input, so a question the answers leave open
/// ends the run rather than wait: the run's output and everything the terminal showed.
fn init_on_a_terminal(dir: &Path, args: &[&str], answers: &str) -> (Output, String) {
    let (mut near, far) = terminal();
    let mut command = phoebis(dir);
    command
        .arg("init")
        .args(args)
        .stdin(far.try_clone().unwrap())
        .stderr(far)
        .stdout(Stdio::piped());
    let child = command.spawn().unwrap();
    // The child now holds the far end's only descriptors, so reading the near end ends when the child does.
    drop(command);
    near.write_all(answers.as_bytes()).unwrap();
    near.write_all(&[4]).unwrap();
    let out = child.wait_with_output().unwrap();
    let mut shown = Vec::new();
    // Linux answers EIO, rather than end of file, once the far end has closed; what was read before it stays.
    let _ = near.read_to_end(&mut shown);
    (out, String::from_utf8_lossy(&shown).into_owned())
}

#[test]
fn the_prompt_offers_files_or_none_and_an_existing_file_refuses_in_every_run() {
    let files = tempfile::tempdir().unwrap();
    let config = files.path().join("config.yaml");
    // A word the flag refuses is no answer either, so the question is asked again.
    let refused = REFUSED_TLS_FLAGS[1][1];
    let (out, shown) = init_on_a_terminal(
        files.path(),
        &["--config", path_arg(&config)],
        &format!("0.0.0.0:0\n\n{refused}\nfiles\ncert.pem\nkey.pem\n"),
    );
    assert_eq!(out.status.code(), Some(0), "{shown}");
    let prompt = "proxy on 0.0.0.0:0 leaves loopback; tls: files, or none for plain HTTP [none]: ";
    assert_eq!(shown.matches(prompt).count(), 2, "{shown}");
    assert!(shown.contains("answer files or none"), "{shown}");
    // Neither file of the pair exists: `init` reads neither, and takes a relative path against its directory.
    let here = files.path().canonicalize().unwrap();
    let loaded = phoebis_router::config::load_unresolved(&config).unwrap();
    assert_eq!(
        loaded.proxy.tls,
        Some(ListenerTls {
            cert_file: here.join("cert.pem"),
            key_file: here.join("key.pem"),
        })
    );
    assert_eq!(loaded.admin.tls, None);

    let none = tempfile::tempdir().unwrap();
    let plain = none.path().join("config.yaml");
    let (out, shown) = init_on_a_terminal(
        none.path(),
        &["--config", path_arg(&plain)],
        "0.0.0.0:0\n\nnone\n",
    );
    assert_eq!(out.status.code(), Some(0), "{shown}");
    assert!(
        shown.contains(
            "proxy on 0.0.0.0:0 leaves loopback; tls: files, or none for plain HTTP [none]: "
        ),
        "{shown}"
    );
    let loaded = phoebis_router::config::load_unresolved(&plain).unwrap();
    assert_eq!((loaded.proxy.tls, loaded.admin.tls), (None, None));

    for (dir, config) in [(files.path(), &config), (none.path(), &plain)] {
        let (before, tree) = (std::fs::read(config).unwrap(), entries(dir));
        let again = init(dir, &["--config", path_arg(config)]);
        assert_eq!(again.status.code(), Some(1), "{}", printed(&again));
        assert!(
            String::from_utf8_lossy(&again.stderr).contains(&format!(
                "Error: {} already exists; init never overwrites a configuration",
                config.display()
            )),
            "{}",
            printed(&again)
        );
        assert!(again.stdout.is_empty(), "{}", printed(&again));
        assert_eq!(std::fs::read(config).unwrap(), before);
        assert_eq!(entries(dir), tree);
    }
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

// On one line, so a grep for the flag can tell these refused spellings apart by the constant's name.
#[rustfmt::skip]
const REFUSED_TLS_FLAGS: [[&str; 2]; 3] = [["--tls", "files"], ["--tls", "self-signed"], ["--proxy-tls", "self_signed"]];

#[test]
fn a_tls_flag_is_a_usage_error() {
    for [flag, value] in REFUSED_TLS_FLAGS {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.yaml");
        let out = init(
            dir.path(),
            &[
                flag,
                value,
                "--non-interactive",
                "--config",
                path_arg(&config),
            ],
        );
        assert_eq!(out.status.code(), Some(2), "{value}: {}", printed(&out));
        assert!(
            String::from_utf8_lossy(&out.stderr)
                .contains(&format!("error: unexpected argument '{flag}' found")),
            "{value}: {}",
            printed(&out)
        );
        assert!(out.stdout.is_empty(), "{value}: {}", printed(&out));
        assert!(entries(dir.path()).is_empty(), "{value}");
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
fn a_pair_is_written_as_given_with_both_paths_absolute() {
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
        "  pricing_path: /var/lib/phoebis/pricing.yaml\n",
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
        loaded.stats.pricing_path.as_deref(),
        Some(Path::new("/var/lib/phoebis/pricing.yaml"))
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
