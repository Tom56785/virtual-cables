//! config.toml: the settings and the cables, next to the log in the
//! application data folder. Command-line flags override the settings. The
//! application edits the file in place, so comments and anything added by hand
//! are kept.

use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;

use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, value};

use crate::logf;
use crate::logging::Logger;
use crate::uac2::{DEFAULT_LATENCY_MS, MAX_LATENCY_MS, MIN_LATENCY_MS};

pub const MAX_CABLES: usize = 32;
/// Cable IDs fit the USB product IDs 0xCA01-0xCAFF.
pub const MAX_CABLE_ID: u32 = 255;
pub const MAX_CABLE_NAME: usize = 64;
/// Built-in settings, used when neither config.toml nor a command-line flag
/// sets a value.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:3240";

/// The file name of the configuration.
pub const CONFIG_FILE: &str = "config.toml";
/// The key of the cable tables: one [[cable]] table per cable.
const CABLE_KEY: &str = "cable";

/// The per-user folder for the configuration and the log.
pub fn application_data_dir() -> PathBuf {
    if let Ok(local) = env::var("LOCALAPPDATA")
        && !local.trim().is_empty()
    {
        // Path joining in earlier versions normalised separators on Windows.
        let local = if cfg!(windows) { local.replace('/', "\\") } else { local };
        return PathBuf::from(local).join("Virtual Cables");
    }
    if let Some(base) = user_cache_dir() {
        return base.join("Virtual Cables");
    }
    env::temp_dir().join("Virtual Cables")
}

/// The platform's per-user cache folder
fn user_cache_dir() -> Option<PathBuf> {
    let non_empty = |name: &str| env::var(name).ok().filter(|v| !v.trim().is_empty()).map(PathBuf::from);
    if cfg!(windows) {
        non_empty("LocalAppData")
    } else if cfg!(target_os = "macos") {
        non_empty("HOME").map(|home| home.join("Library/Caches"))
    } else {
        non_empty("XDG_CACHE_HOME").or_else(|| non_empty("HOME").map(|home| home.join(".cache")))
    }
}

pub fn config_file_path() -> PathBuf {
    application_data_dir().join(CONFIG_FILE)
}

/// The configuration files read, in order; the first that exists is used.
fn config_paths() -> [PathBuf; 2] {
    [config_file_path(), PathBuf::from(CONFIG_FILE)]
}

/// The text of the first config.toml found, or "" when there is none.
pub fn load_config_content() -> String {
    for path in config_paths() {
        if let Ok(content) = fs::read(&path) {
            return String::from_utf8_lossy(&content).into_owned();
        }
    }
    String::new()
}

/// The parsed configuration; empty when there is no file.
#[derive(Default)]
pub struct Config {
    document: DocumentMut,
}

/// Parses config.toml. The error describes a TOML syntax error.
pub fn parse_config(content: &str) -> Result<Config, String> {
    let document = content.parse::<DocumentMut>().map_err(|err| format!("{CONFIG_FILE} is not valid TOML: {err}"))?;
    Ok(Config { document })
}

/// One virtual cable. Its serial number, created with the cable, is its
/// identity in Windows: a new cable is always a new device there, with its own
/// name, while an existing one keeps its settings across restarts. The ID sets
/// the USB product ID and the bus ID (1-<id>) among the running cables.
#[derive(Clone, Debug, PartialEq)]
pub struct Cable {
    pub id: u32,
    pub serial: String,
    pub name: String,
}

impl Cable {
    /// Cable `id` with its default name and a serial number derived from the
    /// ID, for --cables and the diagnostics.
    pub fn numbered(id: u32) -> Cable {
        Cable { id, serial: format!("VCABLE-{id:03}"), name: format!("Virtual Cable {id:02}") }
    }
}

/// Cables 1 to `count` with their default names.
pub fn numbered_cables(count: i64) -> Vec<Cable> {
    (1..=count.clamp(0, MAX_CABLES as i64) as u32).map(Cable::numbered).collect()
}

/// The [[cable]] tables of the configuration, in file order. A table with an
/// out-of-range ID, an invalid serial number or an invalid name is skipped, as
/// are repeated IDs, serial numbers or names and any beyond the first 32.
pub fn config_cables(config: &Config) -> Vec<Cable> {
    let Some(tables) = config.document.get(CABLE_KEY).and_then(Item::as_array_of_tables) else {
        return Vec::new();
    };
    let mut cables: Vec<Cable> = Vec::new();
    for table in tables.iter() {
        let id = table.get("id").and_then(Item::as_integer).and_then(|id| u32::try_from(id).ok());
        let serial = table.get("serial").and_then(Item::as_str).map(|s| s.trim().to_ascii_uppercase());
        let name = table.get("name").and_then(Item::as_str).and_then(|n| normalize_cable_name(n).ok());
        let (Some(id), Some(serial), Some(name)) = (id, serial, name) else {
            continue;
        };
        let duplicate = cables.iter().any(|c| c.id == id || c.serial == serial || same_cable_name(&c.name, &name));
        if (1..=MAX_CABLE_ID).contains(&id) && is_valid_serial(&serial) && !duplicate {
            cables.push(Cable { id, serial, name });
        }
    }
    cables.truncate(MAX_CABLES);
    cables
}

/// Whether `serial` can be a cable's USB serial number: 1 to 32 letters,
/// digits or hyphens. Windows builds the device's instance ID from it.
pub fn is_valid_serial(serial: &str) -> bool {
    (1..=32).contains(&serial.len()) && serial.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// The lowest ID no cable uses.
#[cfg_attr(not(windows), allow(dead_code))] // used by new_cable, for the Windows GUI
fn next_cable_id(cables: &[Cable]) -> Option<u32> {
    (1..=MAX_CABLE_ID).find(|id| !cables.iter().any(|c| c.id == *id))
}

/// A random serial number for a new cable: 8 hexadecimal digits, different
/// from every existing cable's.
#[cfg_attr(not(windows), allow(dead_code))] // used by new_cable, for the Windows GUI
fn new_cable_serial(cables: &[Cable]) -> String {
    use std::hash::{BuildHasher, Hasher};
    loop {
        // RandomState is seeded randomly per process and per instance.
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        hasher.write_u128(now);
        let serial = format!("{:08X}", hasher.finish() as u32);
        if !cables.iter().any(|c| c.serial == serial) {
            return serial;
        }
    }
}

/// Puts a cable name in its standard form: trimmed, with each run of spaces
/// reduced to one. Names may contain only letters, digits, spaces, hyphens,
/// underscores and parentheses, and at most 64 characters.
pub fn normalize_cable_name(name: &str) -> Result<String, String> {
    let name = name.split(' ').filter(|word| !word.is_empty()).collect::<Vec<_>>().join(" ");
    if name.is_empty() {
        return Err("Enter a name for the cable.".into());
    }
    if !name.chars().all(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '(' | ')')) {
        return Err(
            "Cable names can contain only letters, digits, spaces, hyphens, underscores and parentheses.".into()
        );
    }
    if name.chars().count() > MAX_CABLE_NAME {
        return Err(format!("Cable names can be at most {MAX_CABLE_NAME} characters long."));
    }
    Ok(name)
}

/// The cable to add for a name the user typed: the name checked and put in
/// its standard form, the lowest free ID and a new serial number. The error
/// says why the cable cannot be added.
#[cfg_attr(not(windows), allow(dead_code))] // used by the Windows GUI
pub fn new_cable(typed_name: &str, cables: &[Cable]) -> Result<Cable, String> {
    if cables.len() >= MAX_CABLES {
        return Err(format!("A maximum of {MAX_CABLES} virtual cables is supported."));
    }
    let name = validate_cable_name(typed_name, cables)?;
    let id = next_cable_id(cables).ok_or("Every cable ID is in use.")?;
    Ok(Cable { id, serial: new_cable_serial(cables), name })
}

/// Whether two names in their standard form are the same name: letter case
/// does not tell cables apart in Windows Sound.
fn same_cable_name(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

/// Checks a name for a new cable and returns it in its standard form. Names
/// must be unique so the cables can be told apart in Windows Sound.
#[cfg_attr(not(windows), allow(dead_code))] // used by new_cable, for the Windows GUI
fn validate_cable_name(name: &str, cables: &[Cable]) -> Result<String, String> {
    let name = normalize_cable_name(name)?;
    if cables.iter().any(|c| same_cable_name(&c.name, &name)) {
        return Err(format!("There is already a cable called \"{name}\"."));
    }
    Ok(name)
}

/// Returns an integer setting, or `fallback` when it is missing or not an
/// integer. Range checks are left to the caller, which logs them.
pub fn config_int(config: &Config, key: &str, fallback: i64) -> i64 {
    config.document.get(key).and_then(Item::as_integer).unwrap_or(fallback)
}

/// Returns a boolean setting, or `fallback` when it is missing or not a
/// boolean.
pub fn config_bool(config: &Config, key: &str, fallback: bool) -> bool {
    config.document.get(key).and_then(Item::as_bool).unwrap_or(fallback)
}

/// Returns a string setting, or `fallback` when it is missing, empty or not a
/// string.
pub fn config_string(config: &Config, key: &str, fallback: &str) -> String {
    match config.document.get(key).and_then(Item::as_str) {
        Some(value) if !value.is_empty() => value.to_string(),
        _ => fallback.to_string(),
    }
}

/// Parses a boolean command-line value
pub fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

/// Written when config.toml does not exist yet, so every available setting is
/// visible in the file. There are no cables until the user adds one.
pub fn default_config_content() -> String {
    [
        "# Virtual Cables configuration. The cables are added and removed in the".to_string(),
        "# application, which keeps the rest of this file as it is.".to_string(),
        String::new(),
        "# Address of the local USB/IP server.".to_string(),
        format!("listen = \"{DEFAULT_LISTEN}\""),
        format!("# Delay between each cable's playback and recording sides, {MIN_LATENCY_MS}-{MAX_LATENCY_MS} ms."),
        "# Playback that Windows sends later than this plays after a short gap.".to_string(),
        format!("latency_ms = {DEFAULT_LATENCY_MS}"),
        "# Detailed USB/IP traces in the log, for troubleshooting.".to_string(),
        "diagnostics = false".to_string(),
        "# For troubleshooting: hold all audio transfers this many milliseconds once a".to_string(),
        "# minute, to reproduce a system stall. 0 turns it off.".to_string(),
        "stall_test_ms = 0".to_string(),
    ]
    .iter()
    .map(|line| format!("{line}\n"))
    .collect()
}

/// Replaces the [[cable]] tables in `content` with one per cable and keeps
/// everything else, comments included. Fails when `content` is not valid
/// TOML, so that a file being edited by hand is not overwritten.
pub fn set_cable_entries(content: &str, cables: &[Cable]) -> Result<String, String> {
    let mut document = parse_config(content)?.document;
    document.remove(CABLE_KEY);
    if !cables.is_empty() {
        let mut tables = ArrayOfTables::new();
        for cable in cables {
            let mut table = Table::new();
            table.insert("id", value(i64::from(cable.id)));
            table.insert("serial", value(cable.serial.as_str()));
            table.insert("name", value(cable.name.as_str()));
            // A blank line before each cable keeps the file easy to read.
            table.decor_mut().set_prefix("\n");
            tables.push(table);
        }
        document.insert(CABLE_KEY, Item::ArrayOfTables(tables));
    }
    let text = document.to_string();
    // Keep a file written with Windows line endings consistent.
    if content.contains("\r\n") { Ok(text.replace("\r\n", "\n").replace('\n', "\r\n")) } else { Ok(text) }
}

#[cfg_attr(not(windows), allow(dead_code))] // used by the Windows GUI
pub fn save_cables(cables: &[Cable]) -> io::Result<()> {
    fs::create_dir_all(application_data_dir())?;
    let path = config_file_path();
    let content = match fs::read(&path) {
        Ok(existing) => String::from_utf8_lossy(&existing).into_owned(),
        Err(_) => default_config_content(),
    };
    let updated = set_cable_entries(&content, cables).map_err(io::Error::other)?;
    fs::write(path, updated)
}

/// Writes config.toml with the default settings when no configuration file
/// exists, so every available setting can be edited.
pub fn ensure_config_file(logger: &Logger) {
    if config_paths().iter().any(|path| path.exists()) {
        return;
    }
    let dir = application_data_dir();
    if let Err(err) = fs::create_dir_all(&dir) {
        logf!(logger, "Could not create {}: {err}", dir.display());
        return;
    }
    if let Err(err) = fs::write(config_file_path(), default_config_content()) {
        logf!(logger, "Could not write default configuration: {err}");
        return;
    }
    logf!(logger, "Wrote default configuration to {}", config_file_path().display());
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serializes tests that change LOCALAPPDATA.
    pub(crate) static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Points LOCALAPPDATA at a fresh temporary folder for the test.
    pub(crate) fn temp_app_data(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("virtualcables-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // SAFETY: callers hold ENV_LOCK, so no other test reads the environment concurrently.
        unsafe { env::set_var("LOCALAPPDATA", &dir) };
        dir
    }

    fn cable(id: u32, serial: &str, name: &str) -> Cable {
        Cable { id, serial: serial.into(), name: name.into() }
    }

    fn cables_of(content: &str) -> Vec<Cable> {
        config_cables(&parse_config(content).unwrap())
    }

    /// One [[cable]] table, as a user might write it.
    fn cable_table(id: &str, serial: &str, name: &str) -> String {
        format!("[[cable]]\nid = {id}\nserial = {serial}\nname = {name}\n\n")
    }

    #[test]
    fn reads_typed_settings() {
        let config = parse_config(
            "# comment\nlatency_ms = 30\nstall_test_ms = 250\ndiagnostics = true\nlisten = \"0.0.0.0:4000\"\n",
        )
        .unwrap();
        assert_eq!(config_int(&config, "latency_ms", 40), 30);
        assert_eq!(config_int(&config, "stall_test_ms", 1), 250);
        assert!(config_bool(&config, "diagnostics", false));
        assert_eq!(config_string(&config, "listen", DEFAULT_LISTEN), "0.0.0.0:4000");

        // Settings of the wrong type fall back, as do missing ones.
        let config =
            parse_config("stall_test_ms = \"250\"\ndiagnostics = 1\nlisten = 3240\nlatency_ms = 1.5\n").unwrap();
        assert_eq!(config_int(&config, "stall_test_ms", 99), 99);
        assert!(!config_bool(&config, "diagnostics", false));
        assert_eq!(config_string(&config, "listen", DEFAULT_LISTEN), DEFAULT_LISTEN);
        assert_eq!(config_int(&config, "latency_ms", 40), 40);
        assert_eq!(config_string(&parse_config("listen = \"\"").unwrap(), "listen", DEFAULT_LISTEN), DEFAULT_LISTEN);
        assert_eq!(config_int(&Config::default(), "missing", 7), 7);
    }

    #[test]
    fn invalid_toml_is_reported_and_not_overwritten() {
        assert!(parse_config("listen = \n").is_err());
        assert!(parse_config("CABLES=3 x\n").is_err());
        assert!(set_cable_entries("[[cable]\n", &[Cable::numbered(1)]).is_err());
    }

    #[test]
    fn command_line_booleans_parse_as_expected() {
        for (text, expected) in [("1", true), ("t", true), ("TRUE", true), ("0", false), ("False", false)] {
            assert_eq!(parse_bool(text), Some(expected), "{text}");
        }
        assert_eq!(parse_bool("maybe"), None);
    }

    #[test]
    fn ensure_config_file_writes_defaults() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        temp_app_data("config");
        let logger = Logger::discard();
        ensure_config_file(&logger);
        let written = fs::read_to_string(config_file_path()).unwrap();
        assert_eq!(written, default_config_content());
        let config = parse_config(&written).unwrap();
        assert_eq!(config_string(&config, "listen", ""), DEFAULT_LISTEN);
        assert_eq!(config_int(&config, "latency_ms", 0), DEFAULT_LATENCY_MS);
        assert_eq!(config_int(&config, "stall_test_ms", 1), 0);
        assert!(!config_bool(&config, "diagnostics", true));
        assert_eq!(config_cables(&config), []);

        // An existing file is left alone.
        fs::write(config_file_path(), "listen = \"x\"\n").unwrap();
        ensure_config_file(&logger);
        assert_eq!(fs::read_to_string(config_file_path()).unwrap(), "listen = \"x\"\n");

        // Saving the cables keeps the comments and the other settings.
        let mine = "# mine\nlisten = \"127.0.0.1:4000\" # my port\nlatency_ms = 30\n\n";
        fs::write(config_file_path(), format!("{mine}{}", cable_table("1", "\"VCABLE-001\"", "\"Virtual Cable 01\"")))
            .unwrap();
        let cables = [Cable::numbered(1), cable(3, "8F3A2C1D", "Game")];
        save_cables(&cables).unwrap();
        let saved = fs::read_to_string(config_file_path()).unwrap();
        assert!(saved.starts_with(mine), "{saved:?}");
        assert!(saved.contains("[[cable]]\nid = 3\nserial = \"8F3A2C1D\"\nname = \"Game\"\n"), "{saved:?}");
        assert_eq!(cables_of(&saved), cables);
    }

    #[test]
    fn default_config_has_no_cables() {
        assert_eq!(cables_of(&default_config_content()), []);
        assert_eq!(cables_of(""), []);
    }

    #[test]
    fn reads_cables_in_file_order() {
        let content = [
            cable_table("4", "\"8f3a2c1d\"", "\" Discord \""),
            cable_table("2", "\"0000BEEF\"", "\"Game   audio  2\""),
            cable_table("4", "\"11111111\"", "\"Repeated ID\""),
            cable_table("5", "\"8F3A2C1D\"", "\"Repeated serial\""),
            cable_table("0", "\"22222222\"", "\"Zero\""),
            cable_table("256", "\"33333333\"", "\"Too big\""),
            cable_table("\"7\"", "\"44444444\"", "\"ID as text\""),
            cable_table("8", "\"bad serial!\"", "\"Bad serial\""),
            cable_table("9", "\"55555555\"", "\"Game=Audio, live\""),
            cable_table("10", "\"66666666\"", "\"\""),
            "[[cable]]\nid = 11\nname = \"No serial\"\n".to_string(),
        ]
        .concat();
        assert_eq!(cables_of(&content), [cable(4, "8F3A2C1D", "Discord"), cable(2, "0000BEEF", "Game audio 2")]);
        let many: String =
            (1..=40).map(|id| cable_table(&id.to_string(), &format!("\"S{id}\""), &format!("\"C{id}\""))).collect();
        assert_eq!(cables_of(&many).len(), MAX_CABLES);
    }

    #[test]
    fn repeated_names_in_the_config_are_skipped() {
        // The first cable with a name wins, however a later one spaces or
        // capitalises it.
        let content = [
            cable_table("1", "\"AAAA0001\"", "\"Stream Mix 2\""),
            cable_table("2", "\"AAAA0002\"", "\"  stream   MIX  2 \""),
            cable_table("3", "\"AAAA0003\"", "\"STREAM MIX 2\""),
            cable_table("4", "\"AAAA0004\"", "\"Stream Mix 3\""),
        ]
        .concat();
        assert_eq!(cables_of(&content), [cable(1, "AAAA0001", "Stream Mix 2"), cable(4, "AAAA0004", "Stream Mix 3")]);
    }

    #[test]
    fn set_cable_entries_replaces_all_cable_tables() {
        let settings = "# c\nlisten = \"x\" # kept\n\n";
        let content =
            format!("{settings}{}{}", cable_table("1", "\"A1\"", "\"A\""), cable_table("2", "\"B2\"", "\"B\""));
        let cables = [cable(2, "B2", "B"), cable(5, "E5", "E")];

        let updated = set_cable_entries(&content, &cables).unwrap();
        assert!(updated.starts_with(settings), "{updated:?}");
        assert_eq!(cables_of(&updated), cables);

        let none = set_cable_entries(&content, &[]).unwrap();
        assert!(none.starts_with(settings) && !none.contains("[[cable]]"), "removing the last cable: {none:?}");
        assert_eq!(set_cable_entries("", &[]).unwrap(), "");
        assert_eq!(cables_of(&set_cable_entries("", &cables).unwrap()), cables);

        // A file with Windows line endings keeps them.
        let windows = set_cable_entries("listen = \"x\"\r\n", &cables).unwrap();
        assert!(!windows.replace("\r\n", "").contains('\n'), "{windows:?}");
        assert_eq!(cables_of(&windows), cables);
    }

    #[test]
    fn next_cable_id_takes_the_lowest_free_id() {
        assert_eq!(next_cable_id(&[]), Some(1));
        assert_eq!(next_cable_id(&[Cable::numbered(3), Cable::numbered(1)]), Some(2));
        let full: Vec<Cable> = (1..=MAX_CABLE_ID).map(Cable::numbered).collect();
        assert_eq!(next_cable_id(&full), None);
    }

    #[test]
    fn a_new_cable_is_saved_with_its_name_in_standard_form() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        temp_app_data("new-cable");
        let existing = [cable(1, "AAAA0001", "Game")];

        let added = new_cable("  Stream   Mix  2 ", &existing).unwrap();
        assert_eq!((added.id, added.name.as_str()), (2, "Stream Mix 2"));
        assert!(is_valid_serial(&added.serial) && added.serial != "AAAA0001");

        let cables = [existing[0].clone(), added.clone()];
        save_cables(&cables).unwrap();
        let saved = fs::read_to_string(config_file_path()).unwrap();
        let table = format!("[[cable]]\nid = 2\nserial = \"{}\"\nname = \"Stream Mix 2\"\n", added.serial);
        assert!(saved.contains(&table), "{saved:?}");
        assert_eq!(cables_of(&saved), cables, "the saved cables read back unchanged");
    }

    #[test]
    fn new_cables_are_refused_for_bad_names_or_when_full() {
        let existing = [cable(1, "AAAA0001", "Game")];
        assert_eq!(new_cable(" game ", &existing), Err("There is already a cable called \"game\".".into()));
        assert!(new_cable("Game, live", &existing).is_err());
        assert!(new_cable("   ", &existing).is_err());
        let full: Vec<Cable> = (1..=MAX_CABLES as u32).map(Cable::numbered).collect();
        assert_eq!(new_cable("Another", &full), Err(format!("A maximum of {MAX_CABLES} virtual cables is supported.")));
    }

    #[test]
    fn new_serials_are_valid_and_unique() {
        let mut cables = Vec::new();
        for id in 1..=MAX_CABLES as u32 {
            let serial = new_cable_serial(&cables);
            assert!(serial.len() == 8 && serial.bytes().all(|b| b.is_ascii_hexdigit()), "{serial}");
            assert!(is_valid_serial(&serial));
            cables.push(cable(id, &serial, "x"));
        }
        let mut serials: Vec<&str> = cables.iter().map(|c| c.serial.as_str()).collect();
        serials.sort();
        serials.dedup();
        assert_eq!(serials.len(), MAX_CABLES);
    }

    #[test]
    fn cable_names_are_checked() {
        let cables = [cable(1, "A1", "Game")];
        assert_eq!(validate_cable_name("  Music ", &cables), Ok("Music".into()));
        assert_eq!(validate_cable_name(" Stream   Mix  2 ", &cables), Ok("Stream Mix 2".into()));
        assert_eq!(validate_cable_name("Música", &cables), Ok("Música".into()), "letters beyond ASCII");
        assert!(validate_cable_name("", &cables).is_err());
        assert!(validate_cable_name("   ", &cables).is_err());
        assert!(validate_cable_name("game", &cables).is_err());
        assert!(validate_cable_name("GAME", &cables).is_err());
        assert_eq!(validate_cable_name("Game-audio_mix (2)", &cables), Ok("Game-audio_mix (2)".into()));
        for bad in ["Game, live", "Game/2", "Game\taudio", "Game [2]", "A=B", "A.B"] {
            assert!(validate_cable_name(bad, &cables).is_err(), "{bad:?}");
        }
        assert!(validate_cable_name(&"x".repeat(MAX_CABLE_NAME + 1), &cables).is_err());
        assert!(validate_cable_name(&"x".repeat(MAX_CABLE_NAME), &cables).is_ok());
    }

    #[test]
    fn names_differing_only_in_spacing_or_case_are_duplicates() {
        // Names are compared in their standard form, so extra spaces or another
        // letter case do not make a second cable with the same name.
        let cables = [cable(1, "A1", "Stream Mix 2"), cable(2, "B2", "Música")];
        for same in ["Stream Mix 2", "  Stream   Mix  2 ", "stream MIX 2", " STREAM  mix   2", "MÚSICA", " música "] {
            assert!(validate_cable_name(same, &cables).is_err(), "{same:?} was not a duplicate");
        }
        // The duplicate message shows the name as it would have been saved.
        assert_eq!(
            validate_cable_name("  stream   mix 2 ", &cables),
            Err("There is already a cable called \"stream mix 2\".".into())
        );
        // A different word or extra character is a different name.
        for different in ["Stream Mix 3", "StreamMix 2", "Stream Mix 2 (B)", "Stream-Mix 2"] {
            assert!(validate_cable_name(different, &cables).is_ok(), "{different:?} was taken for a duplicate");
        }
    }
}
