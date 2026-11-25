//! Data snapshots around a program switch: `config.toml`, `modes.json` and
//! one consistent copy of the history database, saved under
//! `$XDG_STATE_HOME/voiceless/update/<id>/` with 0700 directories and 0600
//! files (the configuration holds API keys). Models and recordings are not
//! copied: they stay in the data directory whatever the version runs.
//!
//! Consistency. The configuration is read and replaced under the settings
//! writers' lock, like every save from the console or the CLI. The history
//! database is copied with `VACUUM INTO`, one read transaction, so a daemon
//! recording at the same moment changes neither the copy nor what it writes.
//! A restore rebuilds the live database inside one transaction and swaps each
//! file by rename, keeps everything it replaces under `displaced-*/` of the
//! snapshot, and refuses to run while a runtime holds the data directory.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags};
use semver::Version;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use super::{
    at, check_owner_and_mode, copy_and_hash, create_directories, create_file,
    create_private_directory, exists, owned_directory, parent, parse_json, sync_directory,
    valid_digest,
};

/// Snapshots live in `<state dir>/update/<id>/`.
const ROOT: &str = "update";
/// Written once every file is in place; a directory without a valid one is
/// not a snapshot.
const MANIFEST: &str = "snapshot.json";
const CONFIG: &str = "config.toml";
const MODES: &str = "modes.json";
const HISTORY: &str = "history.sqlite3";
/// The configuration files a snapshot covers, by their names in the config
/// directory.
const SETTINGS: [&str; 2] = [CONFIG, MODES];
/// A restore keeps what it replaces in `<snapshot>/displaced-<time>-<suffix>/`.
const DISPLACED: &str = "displaced-";
/// Automatic snapshots beyond the newest this many are removed. One asked
/// for by hand, or whose restore replaced data, is kept.
const KEEP_AUTOMATIC: usize = 3;
/// How long a snapshot or a restore waits for the settings writers' lock,
/// and how often it looks again.
const LOCK_WAIT: Duration = Duration::from_secs(5);
const LOCK_POLL: Duration = Duration::from_millis(50);
/// How long SQLite waits for a lock another process holds.
const BUSY: Duration = Duration::from_secs(5);
/// The time in an id and the time the report shows.
const STAMP: &str = "[year][month][day]T[hour][minute][second][subsecond digits:6]Z";
const SHOWN: &str = "[year]-[month]-[day]T[hour]:[minute]:[second]Z";
/// The schema objects to drop, newest first, and those to create, in the
/// order they were created. Internal objects (`sqlite_%`) are left to SQLite.
const DROPPING: &str =
    "SELECT type, name, sql FROM main.sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY rowid DESC";
const CREATING: &str =
    "SELECT type, name, sql FROM saved.sqlite_master WHERE name NOT LIKE 'sqlite_%' AND sql IS NOT NULL ORDER BY rowid";
/// Whether both databases have the table behind `AUTOINCREMENT`, and how its
/// counters are copied.
const SEQUENCE: &str =
    "SELECT EXISTS(SELECT 1 FROM main.sqlite_master WHERE name = 'sqlite_sequence') AND EXISTS(SELECT 1 FROM saved.sqlite_master WHERE name = 'sqlite_sequence')";
const COPY_SEQUENCE: &str =
    "DELETE FROM main.sqlite_sequence; INSERT INTO main.sqlite_sequence SELECT * FROM saved.sqlite_sequence;";

/// The user's directories the updater reads and writes beside the program.
pub(super) struct Layout {
    /// `$XDG_CONFIG_HOME/voiceless`: `config.toml` and `modes.json`.
    pub config_dir: PathBuf,
    /// `$XDG_DATA_HOME/voiceless`: `history.sqlite3` and the runtime lock.
    pub data_dir: PathBuf,
    /// `$XDG_STATE_HOME/voiceless`: the snapshots.
    pub state_dir: PathBuf,
}

impl Layout {
    /// The directories as the running program resolves them.
    pub(super) fn from_environment() -> Result<Self, String> {
        let state_root = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|root| root.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .filter(|home| home.is_absolute())
                    .map(|home| home.join(".local/state"))
            })
            .ok_or("data snapshots require an absolute XDG state directory")?;
        Ok(Self {
            config_dir: crate::models::default_config_dir(),
            data_dir: crate::models::default_data_dir(),
            state_dir: state_root.join("voiceless"),
        })
    }

    /// Where the live file `name` is.
    fn live(&self, name: &str) -> PathBuf {
        if name == HISTORY {
            self.data_dir.join(name)
        } else {
            self.config_dir.join(name)
        }
    }

    fn root(&self) -> PathBuf {
        self.state_dir.join(ROOT)
    }
}

/// Why a snapshot is taken and which program versions it spans.
pub(super) struct Request {
    pub reason: Reason,
    /// The install channel that takes it.
    pub channel: &'static str,
    /// Version of the program taking it.
    pub from: Version,
    /// Version the program switches to afterwards.
    pub to: Option<Version>,
}

impl Request {
    /// A snapshot asked for by hand, taken by the running program.
    pub fn manual() -> Self {
        Self {
            reason: Reason::Manual,
            channel: "manual",
            from: super::version(),
            to: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Reason {
    /// Before a newer program is activated.
    Upgrade,
    /// Before a previous program is activated again.
    Rollback,
    /// `voiceless update --snapshot`.
    Manual,
}

impl Reason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Upgrade => "upgrade",
            Self::Rollback => "rollback",
            Self::Manual => "manual",
        }
    }
}

/// A snapshot that was just saved.
#[derive(Debug)]
pub(super) struct Saved {
    /// Its name, for `voiceless update --restore`.
    pub id: String,
    directory: PathBuf,
    files: Vec<&'static str>,
}

impl std::fmt::Display for Saved {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Data snapshot {} saved in {} ({}); restore it with `voiceless update --restore {}`.",
            self.id,
            self.directory.display(),
            self.files.join(", "),
            self.id
        )
    }
}

/// `snapshot.json`: written once every file is in place.
#[derive(Deserialize, Serialize)]
struct Manifest {
    schema: u32,
    id: String,
    /// When it was taken: UTC, RFC 3339, no fraction.
    created_at: String,
    reason: Reason,
    channel: String,
    from_version: String,
    to_version: Option<String>,
    /// Each saved file with its SHA-256; a file that did not exist is absent.
    files: BTreeMap<String, String>,
}

/// One complete snapshot under the snapshot root.
struct Entry {
    directory: PathBuf,
    manifest: Manifest,
}

/// Save a snapshot for `request`; `Ok(None)` when there is no data to save.
/// A snapshot is only listed once it is complete: the directory is removed
/// when anything fails on the way.
pub(super) fn snapshot(layout: &Layout, request: Request) -> Result<Option<Saved>, String> {
    let mut any = false;
    for source in SETTINGS
        .iter()
        .map(|name| layout.live(name))
        .chain([layout.live(HISTORY)])
    {
        any |= exists(&source)?;
    }
    if !any {
        return Ok(None);
    }
    let root = layout.root();
    create_directories(parent(&root))?;
    create_private_directory(&root)?;
    let (id, created_at) = identity()?;
    let directory = root.join(&id);
    let files = match snapshot_into(layout, &directory, &id, &request, &created_at) {
        Ok(files) => files,
        Err(error) => {
            let _ = fs::remove_dir_all(&directory);
            return Err(error);
        }
    };
    // Housekeeping: a snapshot that cannot be removed simply stays.
    let _ = prune(&root);
    Ok(Some(Saved {
        id,
        directory,
        files,
    }))
}

/// A snapshot's id and its creation time. The id is the UTC microsecond with
/// a random suffix, so that ids order chronologically and two snapshots of
/// one microsecond never share a directory.
fn identity() -> Result<(String, String), String> {
    let now = OffsetDateTime::now_utc();
    let mut random = [0u8; 4];
    getrandom::getrandom(&mut random).map_err(|error| format!("snapshot id: {error}"))?;
    let [a, b, c, d] = random;
    Ok((
        format!("{}-{a:02x}{b:02x}{c:02x}{d:02x}", show(now, STAMP)?),
        show(now, SHOWN)?,
    ))
}

/// `now` in the format called `description`.
fn show(now: OffsetDateTime, description: &str) -> Result<String, String> {
    let format = time::format_description::parse_borrowed::<1>(description)
        .expect("valid time format description");
    now.format(&format[..])
        .map_err(|error| format!("snapshot time: {error}"))
}

/// Copy the data into the private, still empty `directory`; the manifest is
/// written last.
fn snapshot_into(
    layout: &Layout,
    directory: &Path,
    id: &str,
    request: &Request,
    created_at: &str,
) -> Result<Vec<&'static str>, String> {
    create_private_directory(directory)?;
    // With no configuration directory there is no writer to wait for.
    let _writers = layout
        .config_dir
        .is_dir()
        .then(|| lock_writers(&layout.live(CONFIG)))
        .transpose()?;
    let mut files = Vec::new();
    for name in SETTINGS {
        let source = layout.live(name);
        if exists(&source)? {
            copy_private(&source, &directory.join(name))?;
            files.push(name);
        }
    }
    let history = layout.live(HISTORY);
    if exists(&history)? {
        vacuum_into(&history, &directory.join(HISTORY))?;
        files.push(HISTORY);
    }
    let mut digests = BTreeMap::new();
    for name in &files {
        let path = directory.join(name);
        let (_, digest) = at(&path, copy_and_hash(private_file(&path)?, io::sink()))?;
        digests.insert((*name).to_owned(), digest);
    }
    let manifest = Manifest {
        schema: 1,
        id: id.to_owned(),
        created_at: created_at.to_owned(),
        reason: request.reason,
        channel: request.channel.to_owned(),
        from_version: request.from.to_string(),
        to_version: request.to.as_ref().map(ToString::to_string),
        files: digests,
    };
    let path = directory.join(MANIFEST);
    let mut file = create_file(&path, 0o600)?;
    serde_json::to_writer(&mut file, &manifest)
        .map_err(|error| format!("writing {}: {error}", path.display()))?;
    at(&path, file.sync_all())?;
    sync_directory(directory)?;
    Ok(files)
}

/// The saved snapshots as `voiceless update --list` shows them, one line each.
pub(super) fn list(layout: &Layout) -> Result<Vec<String>, String> {
    let root = layout.root();
    let snapshots = entries(&root)?;
    if snapshots.is_empty() {
        return Ok(vec![format!(
            "No data snapshots are saved in {}.",
            root.display()
        )]);
    }
    Ok(snapshots.iter().map(Entry::line).collect())
}

/// The newest snapshot matching the standalone upgrade a rollback reversed.
pub(super) fn before_upgrade(layout: &Layout, from: &Version, to: &Version) -> Option<String> {
    let from = from.to_string();
    let to = to.to_string();
    entries(&layout.root())
        .ok()?
        .into_iter()
        .find(|entry| {
            entry.manifest.reason == Reason::Upgrade
                && entry.manifest.channel == "standalone"
                && entry.manifest.from_version == from
                && entry.manifest.to_version.as_ref() == Some(&to)
        })
        .map(|entry| entry.id())
}

/// Every complete snapshot under `root`, newest first. A directory without a
/// valid manifest of this tool is not a snapshot and is skipped.
fn entries(root: &Path) -> Result<Vec<Entry>, String> {
    let mut entries = Vec::new();
    if !exists(root)? {
        return Ok(entries);
    }
    for entry in at(root, fs::read_dir(root))? {
        let entry = at(root, entry)?;
        let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let directory = entry.path();
        let Ok(manifest) = read_manifest(&directory, &id) else {
            continue;
        };
        entries.push(Entry {
            directory,
            manifest,
        });
    }
    // Ids start with the UTC time at microsecond precision, so they order chronologically.
    entries.sort_by(|a, b| b.manifest.id.cmp(&a.manifest.id));
    Ok(entries)
}

fn read_manifest(directory: &Path, id: &str) -> Result<Manifest, String> {
    if !snapshot_id(id) {
        return Err(format!("invalid snapshot id {id:?}"));
    }
    owned_directory(directory, true)?;
    let path = directory.join(MANIFEST);
    let mut bytes = Vec::new();
    private_file(&path)?
        .read_to_end(&mut bytes)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let manifest: Manifest = parse_json(&bytes, "snapshot manifest")?;
    if manifest.schema != 1 || manifest.id != id {
        return Err(format!(
            "snapshot {id} was not written by this version of Voiceless"
        ));
    }
    Ok(manifest)
}

/// Whether `id` is a name this tool gives a snapshot: letters, digits and
/// dashes only, so that it cannot name a path.
fn snapshot_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

impl Entry {
    fn id(&self) -> String {
        self.manifest.id.clone()
    }

    /// Id, time, reason, versions, channel and files.
    fn line(&self) -> String {
        let manifest = &self.manifest;
        let versions = match &manifest.to_version {
            Some(to) => format!("{} -> {to}", manifest.from_version),
            None => manifest.from_version.clone(),
        };
        [
            manifest.id.clone(),
            manifest.created_at.clone(),
            manifest.reason.as_str().to_owned(),
            versions,
            manifest.channel.clone(),
            manifest
                .files
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", "),
        ]
        .join("  ")
    }
}

/// Remove the automatic snapshots beyond the newest `KEEP_AUTOMATIC`. One
/// asked for by hand, or whose restore replaced data, is kept.
fn prune(root: &Path) -> Result<(), String> {
    let automatic = entries(root)?.into_iter().filter(|entry| {
        entry.manifest.reason != Reason::Manual && !has_displaced(&entry.directory)
    });
    for entry in automatic.skip(KEEP_AUTOMATIC) {
        at(&entry.directory, fs::remove_dir_all(&entry.directory))?;
    }
    sync_directory(root)
}

/// Whether a restore ever kept what it replaced in this snapshot.
fn has_displaced(directory: &Path) -> bool {
    fs::read_dir(directory).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().starts_with(DISPLACED))
    })
}

/// Return the configuration and history to snapshot `id`. Everything the
/// restore replaces is kept under a `displaced-*` directory of the snapshot,
/// so nothing is lost. Refused while a runtime holds the data directory: a
/// daemon or a stream would keep writing the files that change.
pub(super) fn restore(layout: &Layout, id: &str) -> Result<Vec<String>, String> {
    if !snapshot_id(id) {
        return Err(format!("invalid snapshot id {id:?}"));
    }
    let root = layout.root();
    let directory = root.join(id);
    if !exists(&directory)? {
        return Err(format!("no snapshot named {id} in {}", root.display()));
    }
    let manifest = read_manifest(&directory, id)?;
    for (name, expected) in &manifest.files {
        if ![CONFIG, MODES, HISTORY].contains(&name.as_str()) {
            return Err(format!(
                "snapshot {id} lists {name}, which this version of Voiceless cannot restore"
            ));
        }
        let path = directory.join(name);
        let (_, actual) = at(&path, copy_and_hash(private_file(&path)?, io::sink()))?;
        if !valid_digest(expected) || &actual != expected {
            return Err(format!("snapshot file SHA-256 mismatch for {name}"));
        }
    }

    let _runtime = crate::runtime::lock_runtime(&layout.data_dir).map_err(|error| {
        format!(
            "cannot restore while a Voiceless daemon or stream uses {}: stop it and let any recording finish first ({error})",
            layout.data_dir.display()
        )
    })?;
    let capture = layout.data_dir.join("capture.lock");
    let _capture = exclusive_lock(
        &capture,
        Duration::ZERO,
        "a recording is active; let it finish before restoring",
    )?;
    let _writers = lock_writers(&layout.live(CONFIG))?;
    let live = live_names(layout)?;
    let displaced = if live.is_empty() {
        None
    } else {
        let path = directory.join(format!("{DISPLACED}{}", identity()?.0));
        create_private_directory(&path)?;
        Some(path)
    };

    // Preserve settings and stage their replacements before touching live data.
    if let Some(displaced) = &displaced {
        for name in SETTINGS {
            if live.contains(&name) {
                let source = layout.live(name);
                copy_private(&source, &displaced.join(name))?;
            }
        }
        // A database the snapshot does not have is removed, so it is kept
        // here. One that is rebuilt is kept by the rebuild, under its lock.
        if live.contains(&HISTORY) && !manifest.files.contains_key(HISTORY) {
            vacuum_into(&layout.live(HISTORY), &displaced.join(HISTORY))?;
        }
    }
    let has_settings = SETTINGS
        .iter()
        .any(|name| manifest.files.contains_key(*name));
    let mut staged = Vec::new();
    if has_settings {
        create_directories(&layout.config_dir)?;
        for name in SETTINGS {
            if manifest.files.contains_key(name) {
                staged.push((name, stage(&directory.join(name), &layout.config_dir)?));
            }
        }
    }

    // From here every step is one transaction or one rename.
    if manifest.files.contains_key(HISTORY) {
        restore_history(
            &layout.live(HISTORY),
            &directory.join(HISTORY),
            displaced.as_deref(),
        )?;
    } else if live.contains(&HISTORY) {
        remove_history(&layout.live(HISTORY))?;
    }
    for (name, file) in staged {
        let target = layout.live(name);
        let persisted = file.persist(&target);
        persisted.map_err(|error| format!("{}: {}", target.display(), error.error))?;
    }
    for name in SETTINGS {
        let target = layout.live(name);
        // A file that did not exist when the snapshot was taken goes back to not existing.
        if !manifest.files.contains_key(name) && exists(&target)? {
            at(&target, fs::remove_file(&target))?;
        }
    }
    if layout.config_dir.is_dir() {
        sync_directory(&layout.config_dir)?;
    }

    let mut restored: Vec<&str> = SETTINGS
        .into_iter()
        .filter(|name| manifest.files.contains_key(*name))
        .collect();
    if manifest.files.contains_key(HISTORY) {
        restored.push(HISTORY);
    }
    let mut lines = vec![format!(
        "Restored {} from snapshot {id}, taken {}.",
        restored.join(", "),
        manifest.created_at
    )];
    if let Some(displaced) = displaced {
        lines.push(format!(
            "What it replaced is kept in {}.",
            displaced.display()
        ));
    }
    Ok(lines)
}

/// The files the snapshot covers that exist now.
fn live_names(layout: &Layout) -> Result<Vec<&'static str>, String> {
    let mut live = Vec::new();
    for name in SETTINGS.into_iter().chain([HISTORY]) {
        if exists(&layout.live(name))? {
            live.push(name);
        }
    }
    Ok(live)
}

/// Make the database at `live` hold exactly what `saved` holds. With no live
/// database the snapshot file, complete on its own, is renamed into place.
/// Otherwise `main` is rebuilt inside one transaction on `live`: readers see
/// one state or the other, and a failure leaves `live` as it was.
fn restore_history(live: &Path, saved: &Path, displaced: Option<&Path>) -> Result<(), String> {
    if !exists(live)? {
        stage(saved, parent(live))?
            .persist(live)
            .map_err(|error| format!("{}: {}", live.display(), error.error))?;
        return sync_directory(parent(live));
    }
    let target = connect(live)?;
    let failed = |error: rusqlite::Error| format!("restoring {}: {error}", live.display());
    target
        .execute("ATTACH DATABASE ?1 AS saved", [sql_path(saved)?])
        .map_err(failed)?;
    // From here no other process writes the live database until the transaction ends.
    target.execute_batch("BEGIN IMMEDIATE").map_err(failed)?;
    let committed = replace_database(&target, live, displaced)
        .and_then(|()| target.execute_batch("COMMIT").map_err(failed));
    if let Err(error) = committed {
        // The open transaction is rolled back here or, at the latest, when the connection closes.
        let _ = target.execute_batch("ROLLBACK");
        return Err(error);
    }
    Ok(())
}

/// Inside the transaction that holds the write lock: keep what is in `live`
/// now, then make `main` hold what the attached `saved` database holds.
fn replace_database(
    target: &Connection,
    live: &Path,
    displaced: Option<&Path>,
) -> Result<(), String> {
    if let Some(displaced) = displaced {
        // `VACUUM` cannot run inside a transaction, so a second connection
        // reads a database the first one keeps from changing.
        vacuum_into(live, &displaced.join(HISTORY))?;
    }
    plain_schema(target, "main")?;
    plain_schema(target, "saved")?;
    rebuild(target).map_err(|error| format!("restoring {}: {error}", live.display()))
}

/// Refuse virtual tables: their shadow tables are not listed as what they
/// are, and a restore would rebuild them as plain tables.
fn plain_schema(connection: &Connection, database: &str) -> Result<(), String> {
    let sql = format!(
        "SELECT COUNT(*) FROM {database}.sqlite_master WHERE lower(sql) LIKE 'create virtual%'"
    );
    let count: i64 = connection
        .query_row(&sql, (), |row| row.get(0))
        .map_err(|error| error.to_string())?;
    if count != 0 {
        return Err(format!(
            "the history database uses a virtual table, which this restore cannot rebuild ({database})"
        ));
    }
    Ok(())
}

/// Replace the schema and rows of `main` with those of `saved`; the caller
/// owns the transaction.
fn rebuild(target: &Connection) -> rusqlite::Result<()> {
    for (kind, name, _) in objects(target, DROPPING)? {
        if !matches!(kind.as_str(), "table" | "index" | "view" | "trigger") {
            continue;
        }
        target.execute_batch(&format!("DROP {kind} IF EXISTS main.{}", quoted(&name)))?;
    }
    for (kind, name, sql) in objects(target, CREATING)? {
        // The statement is kept verbatim and names no schema, so it creates in `main`.
        target.execute_batch(&sql.expect("only objects with SQL are listed"))?;
        if kind == "table" {
            let columns = columns(target, &name)?;
            let list = columns
                .iter()
                .map(|column| quoted(column))
                .collect::<Vec<_>>()
                .join(", ");
            target.execute_batch(&format!(
                "INSERT INTO main.{}({list}) SELECT {list} FROM saved.{}",
                quoted(&name),
                quoted(&name)
            ))?;
        }
    }
    if target.query_row(SEQUENCE, (), |row| row.get::<_, bool>(0))? {
        target.execute_batch(COPY_SEQUENCE)?;
    }
    for pragma in ["user_version", "application_id"] {
        let value: i64 = target.pragma_query_value(Some("saved"), pragma, |row| row.get(0))?;
        target.pragma_update(Some("main"), pragma, value)?;
    }
    Ok(())
}

/// The rows of `sql` on `target`: `(kind, name, sql)` of schema objects.
fn objects(
    target: &Connection,
    sql: &str,
) -> rusqlite::Result<Vec<(String, String, Option<String>)>> {
    let mut statement = target.prepare(sql)?;
    let rows = statement.query_map((), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    })?;
    rows.collect()
}

/// The stored columns of `table` in `saved`: generated and hidden ones
/// cannot be inserted.
fn columns(target: &Connection, table: &str) -> rusqlite::Result<Vec<String>> {
    let mut statement =
        target.prepare(&format!("PRAGMA saved.table_xinfo({})", sql_string(table)))?;
    let columns = statement
        .query_map((), |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, i64>(6)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(columns
        .into_iter()
        .filter(|(_, hidden)| *hidden == 0)
        .map(|(name, _)| name)
        .collect())
}

fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// `value` as a quoted SQL string.
fn sql_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Bind filenames as values so SQL quoting never becomes part of a path.
fn sql_path(path: &Path) -> Result<&str, String> {
    path.to_str()
        .ok_or_else(|| format!("{} is not a UTF-8 path", path.display()))
}

/// Open the SQLite database at `path`, which must exist, and wait for locks
/// another process holds.
fn connect(path: &Path) -> Result<Connection, String> {
    let opened = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let connection = Connection::open_with_flags(path, opened)
        .map_err(|error| format!("opening {}: {error}", path.display()))?;
    connection
        .busy_timeout(BUSY)
        .map_err(|error| format!("opening {}: {error}", path.display()))?;
    Ok(connection)
}

/// One consistent copy of the database at `source` in `destination`:
/// `VACUUM INTO` reads in a single transaction, whatever is written
/// meanwhile, and leaves a single file with no WAL beside it.
fn vacuum_into(source: &Path, destination: &Path) -> Result<(), String> {
    let connection = connect(source)?;
    // VACUUM INTO accepts an empty destination. Set its permissions before
    // SQLite writes transcripts, and explicitly flush it before the manifest.
    let file = create_file(destination, 0o600)?;
    connection
        .execute("VACUUM INTO ?1", [sql_path(destination)?])
        .map_err(|error| format!("copying {}: {error}", source.display()))?;
    at(destination, file.sync_all())
}

/// The database at `live` and the WAL files beside it are removed; a
/// connection still open keeps its own copy of the files.
fn remove_history(live: &Path) -> Result<(), String> {
    for suffix in ["", "-wal", "-shm"] {
        let mut name = live.as_os_str().to_owned();
        name.push(suffix);
        let path = PathBuf::from(name);
        if exists(&path)? {
            at(&path, fs::remove_file(&path))?;
        }
    }
    sync_directory(parent(live))
}

/// The settings writers' lock: the exclusive flock on `config.toml.lock`
/// that every save from the console or the CLI holds too, so a snapshot
/// reads and a restore replaces configuration no writer is changing.
fn lock_writers(config: &Path) -> Result<File, String> {
    let lock = config.with_extension("toml.lock");
    if let Some(directory) = config.parent() {
        at(directory, fs::create_dir_all(directory))?;
    }
    exclusive_lock(
        &lock,
        LOCK_WAIT,
        "another process is saving config.toml; retry in a moment",
    )
}

fn exclusive_lock(lock: &Path, wait: Duration, busy: &str) -> Result<File, String> {
    let file = at(
        lock,
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(lock),
    )?;
    let deadline = Instant::now() + wait;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(LOCK_POLL);
            }
            Err(TryLockError::WouldBlock) => {
                return Err(busy.to_owned());
            }
            Err(TryLockError::Error(error)) => return Err(format!("{}: {error}", lock.display())),
        }
    }
}

/// Open a snapshot file: a regular file of the user's that nobody else can
/// read or write, and not a link to something else.
fn private_file(path: &Path) -> Result<File, String> {
    let meta = at(path, fs::symlink_metadata(path))?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular snapshot file", path.display()));
    }
    check_owner_and_mode(path, &meta, true)?;
    at(path, File::open(path))
}

/// Copy `source`, following a link as the readers of the configuration do,
/// to the new 0600 file `destination`.
fn copy_private(source: &Path, destination: &Path) -> Result<(), String> {
    let mut source = at(source, File::open(source))?;
    let mut file = create_file(destination, 0o600)?;
    at(destination, io::copy(&mut source, &mut file))?;
    at(destination, file.sync_all())
}

/// A snapshot file written under its target's directory with its final
/// content, ready to be renamed over the target.
fn stage(saved: &Path, directory: &Path) -> Result<tempfile::NamedTempFile, String> {
    let mut source = private_file(saved)?;
    let mut file = at(
        directory,
        tempfile::Builder::new()
            .prefix(".voiceless-restore-")
            .tempfile_in(directory),
    )?;
    at(saved, io::copy(&mut source, &mut file))?;
    at(saved, file.as_file().sync_all()).map(|()| file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinator::HistoryEntry;
    use crate::history::HistoryStore;
    use std::os::unix::fs::PermissionsExt;

    struct Fixture {
        _scratch: tempfile::TempDir,
        layout: Layout,
    }

    /// A user with config, data and state directories under a scratch directory.
    fn fixture() -> Fixture {
        let scratch = tempfile::tempdir().unwrap();
        let layout = Layout {
            config_dir: scratch.path().join("config/voiceless"),
            data_dir: scratch.path().join("data/voiceless"),
            state_dir: scratch.path().join("state/voiceless"),
        };
        fs::create_dir_all(&layout.config_dir).unwrap();
        Fixture {
            _scratch: scratch,
            layout,
        }
    }

    fn entry(text: &str) -> HistoryEntry {
        HistoryEntry {
            raw_text: text.to_owned(),
            processed_text: None,
            final_text: text.to_owned(),
            processing_mode: "quick".to_owned(),
            status: "completed".to_owned(),
            backend: "cloud-realtime".to_owned(),
            model: None,
            duration_seconds: Some(1.5),
            audio_path: None,
        }
    }

    /// One session as the daemon records it.
    fn record(layout: &Layout, text: &str) {
        HistoryStore::open(layout.live(HISTORY))
            .unwrap()
            .insert(&entry(text), "cloud-realtime")
            .unwrap();
    }

    /// The texts a history database holds, in alphabetical order; read
    /// without the store, which refuses a schema newer than this build's.
    fn texts(database: &Path) -> Vec<String> {
        let connection = Connection::open(database).unwrap();
        let mut statement = connection
            .prepare("SELECT final_text FROM recognition_history")
            .unwrap();
        let mut texts: Vec<String> = statement
            .query_map((), |row| row.get(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        texts.sort();
        texts
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn user_version(database: &Path) -> i64 {
        Connection::open(database)
            .unwrap()
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap()
    }

    /// The directory in which the last restore of this snapshot kept what it replaced.
    fn displaced(snapshot: &Path) -> PathBuf {
        fs::read_dir(snapshot)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(DISPLACED))
            })
            .unwrap()
    }

    #[test]
    fn a_snapshot_is_private_and_consistent_while_something_writes() {
        let fixture = fixture();
        let layout = &fixture.layout;
        fs::write(layout.live(CONFIG), "[cloud]\napi_key = \"secret\"\n").unwrap();
        record(layout, "committed");

        // A daemon's connection in the middle of a write.
        let writer = Connection::open(layout.live(HISTORY)).unwrap();
        writer
            .execute_batch(
                "BEGIN IMMEDIATE;
                 INSERT INTO recognition_history (id, created_at, raw_text, final_text, status)
                 VALUES ('pending', '2026-01-01T00:00:00Z', 'not committed yet', 'not committed yet', 'completed');",
            )
            .unwrap();

        let saved = snapshot(layout, Request::manual()).unwrap().unwrap();
        writer.execute_batch("COMMIT").unwrap();
        drop(writer);

        assert_eq!(saved.files, [CONFIG, HISTORY]);
        assert_eq!(mode(&layout.root()), 0o700);
        assert_eq!(mode(&saved.directory), 0o700);
        for name in saved.files.iter().copied().chain([MANIFEST]) {
            assert_eq!(mode(&saved.directory.join(name)), 0o600, "{name}");
        }
        assert_eq!(
            fs::read_to_string(saved.directory.join(CONFIG)).unwrap(),
            "[cloud]\napi_key = \"secret\"\n"
        );
        // One file holding what was committed, nothing half written beside it.
        assert!(!saved.directory.join("history.sqlite3-wal").exists());
        assert_eq!(texts(&saved.directory.join(HISTORY)), ["committed"]);

        let later = snapshot(layout, Request::manual()).unwrap().unwrap();
        assert_eq!(
            texts(&later.directory.join(HISTORY)),
            ["committed", "not committed yet"]
        );
        assert!(later.id > saved.id, "ids order chronologically");
    }

    #[test]
    fn nothing_to_save_makes_no_snapshot() {
        let fixture = fixture();
        let layout = &fixture.layout;
        assert!(snapshot(layout, Request::manual()).unwrap().is_none());
        assert!(!layout.root().exists());
        assert_eq!(
            list(layout).unwrap(),
            [format!(
                "No data snapshots are saved in {}.",
                layout.root().display()
            )]
        );

        fs::write(layout.live(MODES), "[]").unwrap();
        let saved = snapshot(layout, Request::manual()).unwrap().unwrap();
        assert_eq!(saved.files, [MODES]);
    }

    #[test]
    fn a_restore_returns_every_file_to_the_snapshot_and_keeps_what_it_replaced() {
        let fixture = fixture();
        let layout = &fixture.layout;
        fs::write(layout.live(CONFIG), "config before").unwrap();
        record(layout, "before");
        let saved = snapshot(layout, Request::manual()).unwrap().unwrap();

        fs::write(layout.live(CONFIG), "config after").unwrap();
        fs::write(layout.live(MODES), "modes after").unwrap();
        record(layout, "after");

        let lines = restore(layout, &saved.id).unwrap();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("config.toml, history.sqlite3"));
        assert_eq!(
            fs::read_to_string(layout.live(CONFIG)).unwrap(),
            "config before"
        );
        assert!(!layout.live(MODES).exists());
        assert_eq!(texts(&layout.live(HISTORY)), ["before"]);

        let displaced = displaced(&saved.directory);
        assert_eq!(
            fs::read_to_string(displaced.join(CONFIG)).unwrap(),
            "config after"
        );
        assert_eq!(
            fs::read_to_string(displaced.join(MODES)).unwrap(),
            "modes after"
        );
        assert_eq!(texts(&displaced.join(HISTORY)), ["after", "before"]);
        for name in [CONFIG, MODES, HISTORY] {
            assert_eq!(mode(&displaced.join(name)), 0o600, "{name}");
        }

        // With no live database the snapshot file is put back as it is.
        remove_history(&layout.live(HISTORY)).unwrap();
        restore(layout, &saved.id).unwrap();
        assert_eq!(mode(&layout.live(HISTORY)), 0o600);
        assert_eq!(texts(&layout.live(HISTORY)), ["before"]);
    }

    #[test]
    fn a_restore_returns_a_database_a_newer_version_migrated_to_its_old_schema() {
        let fixture = fixture();
        let layout = &fixture.layout;
        record(layout, "before");
        let saved = snapshot(layout, Request::manual()).unwrap().unwrap();
        let own = user_version(&saved.directory.join(HISTORY));

        // The newer program added a column and a table, and stamped its schema version.
        let newer = Connection::open(layout.live(HISTORY)).unwrap();
        newer
            .execute_batch(&format!(
                "ALTER TABLE recognition_history ADD COLUMN future_column INTEGER;
                 CREATE TABLE future_table (id TEXT PRIMARY KEY);
                 INSERT INTO future_table VALUES ('one');
                 PRAGMA user_version = {};",
                own + 1
            ))
            .unwrap();
        drop(newer);
        assert!(
            HistoryStore::open(layout.live(HISTORY)).is_err(),
            "this build refuses the newer schema"
        );

        restore(layout, &saved.id).unwrap();

        assert_eq!(user_version(&layout.live(HISTORY)), own);
        // The program of this build reads what the older schema holds.
        HistoryStore::open(layout.live(HISTORY)).unwrap();
        let restored = Connection::open(layout.live(HISTORY)).unwrap();
        let tables: i64 = restored
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'future_table'",
                (),
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 0);
        let columns: i64 = restored
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('recognition_history') WHERE name = 'future_column'",
                (),
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(columns, 0);
        drop(restored);
        assert_eq!(texts(&layout.live(HISTORY)), ["before"]);
        assert_eq!(
            texts(&displaced(&saved.directory).join(HISTORY)),
            ["before"]
        );
    }

    #[test]
    fn a_restore_waits_for_the_runtime_to_leave() {
        let fixture = fixture();
        let layout = &fixture.layout;
        record(layout, "before");
        let saved = snapshot(layout, Request::manual()).unwrap().unwrap();
        record(layout, "after");

        // A daemon, or a standalone stream, holds the data directory.
        let daemon = crate::runtime::lock_runtime(&layout.data_dir).unwrap();
        let error = restore(layout, &saved.id).unwrap_err();
        assert!(error.contains("stop it"), "{error}");
        drop(daemon);
        assert_eq!(texts(&layout.live(HISTORY)), ["after", "before"]);
        assert!(!has_displaced(&saved.directory));

        restore(layout, &saved.id).unwrap();
        assert_eq!(texts(&layout.live(HISTORY)), ["before"]);
    }

    #[test]
    fn a_restore_refuses_a_snapshot_that_is_not_what_was_saved() {
        let fixture = fixture();
        let layout = &fixture.layout;
        fs::write(layout.live(CONFIG), "config").unwrap();
        let saved = snapshot(layout, Request::manual()).unwrap().unwrap();

        let file = saved.directory.join(CONFIG);
        fs::write(&file, "tampered").unwrap();
        assert!(restore(layout, &saved.id)
            .unwrap_err()
            .contains("SHA-256 mismatch"));
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(restore(layout, &saved.id)
            .unwrap_err()
            .contains("unsafe owner or permissions"));
        assert_eq!(fs::read_to_string(layout.live(CONFIG)).unwrap(), "config");
        assert!(!has_displaced(&saved.directory));

        for id in ["", ".", "../elsewhere", "state/voiceless"] {
            assert!(restore(layout, id)
                .unwrap_err()
                .contains("invalid snapshot id"));
        }
        assert!(restore(layout, "no-such-snapshot")
            .unwrap_err()
            .contains("no snapshot named no-such-snapshot"));
    }

    #[test]
    fn only_the_newest_automatic_snapshots_are_kept() {
        let fixture = fixture();
        let layout = &fixture.layout;
        record(layout, "row");
        let upgrade = |to: &str| Request {
            reason: Reason::Upgrade,
            channel: "standalone",
            from: Version::new(0, 1, 0),
            to: Some(Version::parse(to).unwrap()),
        };
        let saved = |request| {
            // Ids order by time: keep successive snapshots apart.
            std::thread::sleep(Duration::from_millis(2));
            snapshot(layout, request).unwrap().unwrap()
        };
        for to in ["0.1.1", "0.1.2", "0.1.3", "0.1.4"] {
            saved(upgrade(to));
        }
        let manual = saved(Request::manual());
        let last = saved(upgrade("0.1.5"));

        let lines = list(layout).unwrap();
        assert_eq!(lines.len(), 4, "{lines:?}");
        assert!(lines.iter().all(|line| !line.contains(" -> 0.1.1")));
        assert!(lines.iter().any(|line| line.starts_with(&manual.id)));
        assert_eq!(lines[0].split_whitespace().next(), Some(last.id.as_str()));

        // The snapshot to return to after leaving a version is the one taken before it came.
        let from = Version::new(0, 1, 0);
        let before = before_upgrade(layout, &from, &Version::new(0, 1, 4)).unwrap();
        assert!(lines
            .iter()
            .any(|line| line.starts_with(&before) && line.contains(" -> 0.1.4")));
        assert!(before_upgrade(layout, &from, &Version::new(0, 1, 1)).is_none());
        assert!(before_upgrade(layout, &Version::new(0, 1, 2), &Version::new(0, 1, 4)).is_none());
    }

    #[test]
    fn a_snapshot_that_was_restored_from_is_never_pruned() {
        let fixture = fixture();
        let layout = &fixture.layout;
        record(layout, "row");
        let automatic = |to: &str| Request {
            reason: Reason::Upgrade,
            channel: "nix",
            from: Version::new(1, 0, 0),
            to: Some(Version::parse(to).unwrap()),
        };
        let saved = |request| {
            std::thread::sleep(Duration::from_millis(2));
            snapshot(layout, request).unwrap().unwrap()
        };
        let first = saved(automatic("1.0.1"));
        record(layout, "written since");
        restore(layout, &first.id).unwrap();
        for to in ["1.0.2", "1.0.3", "1.0.4", "1.0.5"] {
            saved(automatic(to));
        }
        assert!(first.directory.is_dir());
        assert_eq!(
            texts(&displaced(&first.directory).join(HISTORY)),
            ["row", "written since"]
        );
    }
}
