//! Loading and validation through the public API.

// Test helpers outside #[test] fns are not covered by clippy.toml.
#![allow(clippy::unwrap_used)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use niox_config::{
    Audio, AudioMode, ConfigError, DEFAULT_AUDIO_LATENCY, DEFAULT_HOTKEY, DEFAULT_PORT, Hotkey,
    LoadError, PrivateKey, Role, load, parse, read,
};

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn pubkey() -> String {
    PrivateKey::generate().unwrap().public_key().to_string()
}

/// A config with a fresh identity, plus whatever `rest` adds.
fn config_text(rest: &str) -> String {
    let private = base64_of(PrivateKey::generate().unwrap().as_bytes());
    format!("[identity]\nprivate_key = \"{private}\"\n\n{rest}")
}

fn base64_of(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

fn peer(name: &str, address: &str, key: &str, role: &str) -> String {
    format!(
        "[[peer]]\nname = \"{name}\"\naddress = \"{address}\"\npublic_key = \"{key}\"\nrole = \"{role}\"\n\n"
    )
}

/// Asserts `text` fails validation on `field`, with `message` in the error.
fn assert_invalid(text: &str, field: &str, message: &str) {
    match parse(text) {
        Err(ConfigError::Invalid {
            field: f,
            message: m,
        }) => {
            assert_eq!(f, field, "message was {m:?}");
            assert!(m.contains(message), "{m:?} doesn't contain {message:?}");
        }
        other => panic!("expected invalid {field}, got {other:?}"),
    }
}

/// Asserts `text` is rejected by the TOML parser with `needle` in the
/// error (the parser's errors name the field and line).
fn assert_syntax(text: &str, needle: &str) {
    match parse(text) {
        Err(e @ ConfigError::Syntax(_)) => {
            let msg = e.to_string();
            assert!(msg.contains(needle), "{msg:?} doesn't contain {needle:?}");
        }
        other => panic!("expected syntax error containing {needle:?}, got {other:?}"),
    }
}

// --- first run ---------------------------------------------------------

#[test]
fn first_run_writes_default_config_with_identity() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("etc/niox/niox.toml");

    let first = load(&path).unwrap();
    assert!(first.created_identity);
    assert_eq!(mode(&path), 0o600);
    assert!(path.exists());
    assert!(!dir.path().join("etc/niox/.niox.toml.tmp").exists());
    assert_eq!(first.config.transport.port, DEFAULT_PORT);
    assert_eq!(first.config.km.hotkey, DEFAULT_HOTKEY);
    // The written defaults match the built-in ones.
    assert_eq!(first.config.audio, Audio::default());
    assert!(first.config.peers.is_empty());

    // Steady state: same identity, nothing rewritten.
    let before = fs::read_to_string(&path).unwrap();
    let second = load(&path).unwrap();
    assert!(!second.created_identity);
    assert_eq!(
        second.config.identity.private_key.as_bytes(),
        first.config.identity.private_key.as_bytes()
    );
    assert_eq!(
        second.config.identity.public_key,
        first.config.identity.public_key
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), before);
}

#[test]
fn first_run_ignores_umask_and_stale_temp_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("niox.toml");
    let tmp = dir.path().join(".niox.toml.tmp");
    fs::write(&tmp, "junk").unwrap();
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644)).unwrap();

    load(&path).unwrap();
    assert_eq!(mode(&path), 0o600);
    assert!(!tmp.exists());
}

#[test]
fn identity_is_appended_to_existing_config_without_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("niox.toml");
    let key = pubkey();
    let original = format!(
        "# my comment\n[km]\nhotkey = [97]\n\n{}",
        peer("desk", "10.0.0.2", &key, "client")
    );
    fs::write(&path, &original).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

    let loaded = load(&path).unwrap();
    assert!(loaded.created_identity);
    assert_eq!(mode(&path), 0o600);
    let written = fs::read_to_string(&path).unwrap();
    assert!(
        written.starts_with(&original),
        "existing content was changed"
    );
    assert!(written.contains("[identity]"));
    assert_eq!(
        loaded.config.km.hotkey,
        Hotkey {
            key1: 97,
            key2: None
        }
    );
    assert_eq!(loaded.config.peers[0].name, "desk");

    let reloaded = load(&path).unwrap();
    assert!(!reloaded.created_identity);
    assert_eq!(
        reloaded.config.identity.public_key,
        loaded.config.identity.public_key
    );
}

#[test]
fn invalid_config_without_identity_is_left_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("niox.toml");
    let original = peer("desk", "10.0.0.2", &pubkey(), "server");
    fs::write(&path, &original).unwrap();

    let err = load(&path).unwrap_err();
    assert!(matches!(err, LoadError::Config { .. }), "{err:?}");
    let cause = std::error::Error::source(&err).unwrap().to_string();
    assert!(cause.contains("role"), "{cause}");
    assert_eq!(fs::read_to_string(&path).unwrap(), original);
}

#[test]
fn load_errors_name_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("niox.toml");
    fs::write(&path, "[km]\nhotkey = [0]\n").unwrap();
    let err = load(&path).unwrap_err();
    let outer = err.to_string();
    let cause = std::error::Error::source(&err).unwrap().to_string();
    assert!(outer.contains("niox.toml"), "{outer}");
    assert!(!outer.contains("km.hotkey"), "cause repeated in {outer}");
    assert!(cause.contains("km.hotkey"), "{cause}");
}

#[test]
fn unreadable_path_is_an_io_error() {
    let dir = tempfile::tempdir().unwrap();
    // A directory where the file should be.
    let err = load(dir.path()).unwrap_err();
    assert!(
        matches!(err, LoadError::Io { action: "read", .. }),
        "{err:?}"
    );
}

#[test]
fn reload_read_refuses_config_without_identity_and_leaves_it_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("niox.toml");
    let running = load(&path).unwrap().config;

    // The file is replaced wholesale with one that has no [identity].
    let replaced = peer("desk", "10.0.0.2", &pubkey(), "client");
    fs::write(&path, &replaced).unwrap();
    let err = read(&path).unwrap_err();
    assert!(matches!(err, LoadError::Config { .. }), "{err:?}");
    let cause = std::error::Error::source(&err).unwrap().to_string();
    assert!(cause.contains("identity"), "{cause}");
    assert_eq!(fs::read_to_string(&path).unwrap(), replaced);
    assert!(!dir.path().join(".niox.toml.tmp").exists());

    // With the identity back, read returns it unchanged.
    let restored = format!(
        "[identity]\nprivate_key = \"{}\"\n\n{replaced}",
        base64_of(running.identity.private_key.as_bytes())
    );
    fs::write(&path, &restored).unwrap();
    let reread = read(&path).unwrap();
    assert_eq!(reread.identity.public_key, running.identity.public_key);
    assert_eq!(fs::read_to_string(&path).unwrap(), restored);
}

#[test]
fn reload_read_never_creates_a_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("niox.toml");
    let err = read(&path).unwrap_err();
    assert!(
        matches!(err, LoadError::Io { action: "read", .. }),
        "{err:?}"
    );
    assert!(!path.exists());
}

#[test]
fn first_run_writes_through_a_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("real.toml");
    let link = dir.path().join("niox.toml");
    let original = peer("desk", "10.0.0.2", &pubkey(), "client");
    fs::write(&target, &original).unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let loaded = load(&link).unwrap();
    assert!(loaded.created_identity);
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let written = fs::read_to_string(&target).unwrap();
    assert!(written.starts_with(&original));
    assert!(written.contains("[identity]"));
    assert_eq!(mode(&target), 0o600);
}

// --- round trip ----------------------------------------------------------

#[test]
fn full_config_parses() {
    let (k1, k2) = (pubkey(), pubkey());
    let text = config_text(&format!(
        "[transport]\nport = 5000\n\n[km]\nhotkey = [29, 56]\n\n{}{}",
        peer("desk", "192.168.1.20", &k1, "client"),
        peer("laptop", "Laptop.LAN:6000", &k2, "host"),
    ));
    let config = parse(&text).unwrap();
    assert_eq!(config.transport.port, 5000);
    assert_eq!(
        config.km.hotkey,
        Hotkey {
            key1: 29,
            key2: Some(56)
        }
    );
    let desk = config.peer("desk").unwrap();
    assert_eq!(desk.address.to_string(), "192.168.1.20:5000");
    assert_eq!(desk.public_key.to_string(), k1);
    assert_eq!(desk.role, Role::Client);
    let laptop = config.peer("laptop").unwrap();
    assert_eq!(laptop.address.to_string(), "laptop.lan:6000");
    assert_eq!(laptop.role, Role::Host);
    assert!(config.peer("nope").is_none());
    assert_eq!(config.audio, Audio::default());
}

#[test]
fn identity_public_key_is_derived() {
    let key = PrivateKey::generate().unwrap();
    let text = format!(
        "[identity]\nprivate_key = \"{}\"\n",
        base64_of(key.as_bytes())
    );
    assert_eq!(parse(&text).unwrap().identity.public_key, key.public_key());
}

#[test]
fn debug_output_hides_private_key() {
    let text = config_text("");
    let config = parse(&text).unwrap();
    let private = base64_of(config.identity.private_key.as_bytes());
    assert!(!format!("{config:?}").contains(&private));
}

// --- malformed keys ----------------------------------------------------

#[test]
fn rejects_malformed_private_key() {
    for (key, msg) in [
        ("nope!", "base64"),
        ("AAAA", "3 bytes"),
        (&*base64_of(&[0; 32]), "all zeros"),
    ] {
        assert_invalid(
            &format!("[identity]\nprivate_key = \"{key}\"\n"),
            "identity.private_key",
            msg,
        );
    }
}

#[test]
fn rejects_malformed_peer_key() {
    for (key, msg) in [
        ("nope!", "base64"),
        (&*base64_of(&[1; 16]), "16 bytes"),
        (&*base64_of(&[0; 32]), "all zeros"),
    ] {
        assert_invalid(
            &config_text(&peer("desk", "10.0.0.2", key, "client")),
            "peer[0] (\"desk\").public_key",
            msg,
        );
    }
}

#[test]
fn rejects_missing_identity() {
    assert_invalid("", "identity", "missing");
    assert_syntax("[identity]\n", "private_key");
}

// --- peers ---------------------------------------------------------------

#[test]
fn rejects_duplicate_peer_names() {
    let text = config_text(&format!(
        "{}{}",
        peer("desk", "10.0.0.2", &pubkey(), "client"),
        peer("desk", "10.0.0.3", &pubkey(), "client"),
    ));
    assert_invalid(&text, "peer[1] (\"desk\").name", "duplicate of peer[0]");
}

#[test]
fn rejects_duplicate_peer_keys() {
    let key = pubkey();
    let text = config_text(&format!(
        "{}{}",
        peer("desk", "10.0.0.2", &key, "client"),
        peer("laptop", "10.0.0.3", &key, "client"),
    ));
    assert_invalid(
        &text,
        "peer[1] (\"laptop\").public_key",
        "duplicate of peer[0]",
    );
}

#[test]
fn rejects_duplicate_peer_addresses() {
    // Same endpoint once the default port and hostname case are applied.
    let text = config_text(&format!(
        "{}{}",
        peer("desk", "Desk.lan", &pubkey(), "client"),
        peer("laptop", "desk.LAN:4777", &pubkey(), "client"),
    ));
    assert_invalid(&text, "peer[1] (\"laptop\").address", "already used");

    // Different ports on one IP are fine.
    let text = config_text(&format!(
        "{}{}",
        peer("a", "10.0.0.2:1000", &pubkey(), "client"),
        peer("b", "10.0.0.2:2000", &pubkey(), "client"),
    ));
    parse(&text).unwrap();
}

#[test]
fn rejects_own_key_as_peer() {
    let key = PrivateKey::generate().unwrap();
    let text = format!(
        "[identity]\nprivate_key = \"{}\"\n\n{}",
        base64_of(key.as_bytes()),
        peer("me", "10.0.0.1", &key.public_key().to_string(), "client"),
    );
    assert_invalid(&text, "peer[0] (\"me\").public_key", "own public key");
}

#[test]
fn rejects_unknown_role() {
    for role in ["server", "Host", ""] {
        assert_invalid(
            &config_text(&peer("desk", "10.0.0.2", &pubkey(), role)),
            "peer[0] (\"desk\").role",
            "unknown role",
        );
    }
}

#[test]
fn rejects_bad_peer_name_and_address() {
    assert_invalid(
        &config_text(&peer("  ", "10.0.0.2", &pubkey(), "client")),
        "peer[0].name",
        "empty",
    );
    let long = "x".repeat(256);
    assert_invalid(
        &config_text(&peer(&long, "10.0.0.2", &pubkey(), "client")),
        &format!("peer[0] ({long:?}).name"),
        "longer than 255 bytes",
    );
    parse(&config_text(&peer(
        &"x".repeat(255),
        "10.0.0.2",
        &pubkey(),
        "client",
    )))
    .unwrap();
    assert_invalid(
        &config_text(&peer("desk", "bad_host", &pubkey(), "client")),
        "peer[0] (\"desk\").address",
        "not an IP address or hostname",
    );
}

#[test]
fn rejects_peer_with_missing_field() {
    let text =
        config_text("[[peer]]\nname = \"desk\"\naddress = \"10.0.0.2\"\nrole = \"client\"\n");
    assert_syntax(&text, "public_key");
}

// --- other settings ------------------------------------------------------

#[test]
fn rejects_unknown_fields() {
    assert_syntax(&config_text("[km]\nhotkeys = [1]\n"), "hotkeys");
    assert_syntax(&config_text("[video]\n"), "video");
    assert_syntax(
        &config_text(&peer("d", "10.0.0.2", &pubkey(), "client").replace("role", "rol")),
        "rol",
    );
}

#[test]
fn validates_hotkey() {
    let ok =
        |keys: &str| parse(&config_text(&format!("[km]\nhotkey = {keys}\n"))).map(|c| c.km.hotkey);
    assert_eq!(
        ok("[125]").unwrap(),
        Hotkey {
            key1: 125,
            key2: None
        }
    );
    assert_eq!(ok("[125, 41]").unwrap(), DEFAULT_HOTKEY);

    for (keys, msg) in [
        ("[]", "got 0"),
        ("[1, 2, 3]", "got 3"),
        ("[41, 41]", "different"),
        ("[0]", "not an evdev key code"),
        ("[768]", "not an evdev key code"),
    ] {
        assert_invalid(
            &config_text(&format!("[km]\nhotkey = {keys}\n")),
            "km.hotkey",
            msg,
        );
    }
    assert_syntax(&config_text("[km]\nhotkey = [70000]\n"), "hotkey");
    assert_syntax(&config_text("[km]\nhotkey = 125\n"), "hotkey");
}

#[test]
fn validates_audio() {
    let audio = |table: &str| parse(&config_text(&format!("[audio]\n{table}\n"))).map(|c| c.audio);
    assert_eq!(
        audio("enabled = false\nmode = \"follow-focus\"\nlatency_ms = 120").unwrap(),
        Audio {
            enabled: false,
            mode: AudioMode::FollowFocus,
            latency: Duration::from_millis(120),
        }
    );
    assert_eq!(
        audio("mode = \"always-mix\"").unwrap().mode,
        AudioMode::AlwaysMix
    );
    assert_eq!(audio("").unwrap(), Audio::default());
    assert_eq!(Audio::default().latency, DEFAULT_AUDIO_LATENCY);
    assert_invalid(
        &config_text("[audio]\nmode = \"mix\"\n"),
        "audio.mode",
        "unknown mode \"mix\"",
    );
    for ms in [0, 9, 2001] {
        assert_invalid(
            &config_text(&format!("[audio]\nlatency_ms = {ms}\n")),
            "audio.latency_ms",
            "out of range",
        );
    }
    for ms in [10, 2000] {
        assert!(audio(&format!("latency_ms = {ms}")).is_ok());
    }
    assert_syntax(&config_text("[audio]\nenabled = 1\n"), "enabled");
    assert_syntax(&config_text("[audio]\nlatency = 1\n"), "latency");
}

#[test]
fn validates_port() {
    assert_invalid(
        &config_text("[transport]\nport = 0\n"),
        "transport.port",
        "0",
    );
    assert_syntax(&config_text("[transport]\nport = 65536\n"), "port");
}
