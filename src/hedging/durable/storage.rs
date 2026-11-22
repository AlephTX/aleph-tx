use super::{Event, Ledger, LegacyEvent, SessionSpec};
use crate::live_hedge::journal::JournalSink;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub(super) const SCHEMA_LEGACY: u8 = 1;
pub(super) const SCHEMA_V2: u8 = 2;
pub(super) const SCHEMA_CURRENT: u8 = 3;
const MAX_BYTES: u64 = 64 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 1024 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Payload<E> {
    schema: u8,
    sequence: u64,
    previous_checksum: String,
    written_at_us: u64,
    record: E,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Line<E> {
    payload: Payload<E>,
    checksum: String,
}

#[derive(Deserialize)]
struct SchemaProbe {
    payload: SchemaHeader,
}

#[derive(Deserialize)]
struct SchemaHeader {
    schema: u8,
}

fn hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn directory(path: &Path, create: bool) -> Result<()> {
    ensure!(
        path.is_absolute(),
        "Execution storage must use an absolute private directory"
    );
    if create && !path.exists() {
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        builder.mode(0o700);
        builder
            .create(path)
            .context("Cannot create execution storage")?;
    }
    let meta = fs::symlink_metadata(path).context("Cannot inspect execution storage")?;
    ensure!(
        meta.is_dir() && !meta.file_type().is_symlink(),
        "Execution storage is not a real directory"
    );
    #[cfg(unix)]
    ensure!(
        meta.permissions().mode() & 0o077 == 0,
        "Execution storage must be owner-only"
    );
    Ok(())
}

fn private_file(path: &Path, write: bool) -> Result<File> {
    let before = fs::symlink_metadata(path).context("Cannot inspect execution file")?;
    ensure!(
        before.is_file() && !before.file_type().is_symlink(),
        "Execution file is not a regular file"
    );
    #[cfg(unix)]
    ensure!(
        before.permissions().mode() & 0o077 == 0 && before.nlink() == 1,
        "Execution file has unsafe permissions or hard links"
    );
    let mut options = OpenOptions::new();
    options.read(true).append(write);
    let file = options.open(path).context("Cannot open execution file")?;
    #[cfg(unix)]
    {
        let after = file.metadata()?;
        ensure!(
            before.ino() == after.ino() && before.dev() == after.dev(),
            "Execution file changed during open"
        );
    }
    Ok(file)
}

fn create_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).append(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    options
        .open(path)
        .context("Cannot create new execution file")
}

fn path(directory: &Path, spec: &SessionSpec) -> Result<PathBuf> {
    Ok(directory.join(format!(
        "{}.ant-journal.jsonl",
        uuid::Uuid::parse_str(&spec.session_id)?.simple()
    )))
}

struct AccountLock {
    _file: File,
    marker: PathBuf,
}

fn account_keys(spec: &SessionSpec) -> Result<Vec<String>> {
    let mut keys = Vec::new();
    for leg in &spec.legs {
        let id = &leg.identity;
        // Account/subaccount ownership spans markets and sessions.
        keys.push(hash(&serde_json::to_vec(&(
            &id.venue,
            &id.network,
            &id.account,
            id.subaccount,
        ))?));
    }
    keys.sort();
    keys.dedup();
    Ok(keys)
}

pub(super) fn lock_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).append(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    match options.open(path) {
        Ok(file) => Ok(file),
        // Two writers racing initial creation fall back cleanly once to
        // existing-inode/permission validation without introducing TOCTOU state.
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => private_file(path, true),
        Err(error) => Err(error).context("Cannot create execution lock file"),
    }
}

fn locks(directory: &Path, spec: &SessionSpec) -> Result<Vec<AccountLock>> {
    let mut held = Vec::new();
    for key in account_keys(spec)? {
        let lock_path = directory.join(format!("{key}.account.lock"));
        let file = lock_file(&lock_path)?;
        file.try_lock()
            .map_err(|_| anyhow::anyhow!("Execution account already owned by another writer"))?;
        held.push(AccountLock {
            _file: file,
            marker: directory.join(format!("{key}.active.json")),
        });
    }
    Ok(held)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActiveTip {
    schema: u8,
    session_id: String,
    sequence: u64,
    checksum: String,
}

fn active_tip(path: &Path) -> Result<Option<ActiveTip>> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        other => {
            other.context("Cannot inspect active-session marker")?;
        }
    }
    let mut raw = Vec::new();
    private_file(path, false)?
        .take(1025)
        .read_to_end(&mut raw)?;
    ensure!(
        raw.len() <= 1024 && raw.last() == Some(&b'\n'),
        "Damaged active-session marker"
    );
    let tip: ActiveTip = serde_json::from_slice(&raw)
        .map_err(|_| anyhow::anyhow!("Malformed active-session marker"))?;
    ensure!(
        tip.schema == 1 && uuid::Uuid::parse_str(&tip.session_id)?.to_string() == tip.session_id,
        "Invalid active-session marker schema or identity"
    );
    Ok(Some(tip))
}

fn publish_tip(path: &Path, tip: &ActiveTip) -> Result<()> {
    let directory = path.parent().context("Missing marker directory")?;
    let temporary = directory.join(format!("{}.tip-tmp", uuid::Uuid::new_v4().simple()));
    let mut file = create_file(&temporary)?;
    let mut bytes = serde_json::to_vec(tip)?;
    bytes.push(b'\n');
    file.write_all(&bytes)?;
    file.sync_all()?;
    // Never replace the advisory lock inode. A separate atomic marker also
    // detects whole-record tail loss, which the in-file hash chain cannot.
    fs::rename(&temporary, path)?;
    File::open(directory)?.sync_all()?;
    Ok(())
}

fn validate_tip(path: &Path, ledger: &Ledger, checksums: &[String]) -> Result<()> {
    let Some(tip) = active_tip(path)? else {
        ensure!(ledger.closed, "Active-session marker is missing");
        return Ok(());
    };
    ensure!(
        tip.session_id == ledger.spec.session_id,
        "Active-session marker differs from execution journal"
    );
    let sequence =
        usize::try_from(tip.sequence).context("Active sequence exceeds platform size")?;
    ensure!(
        checksums.get(sequence) == Some(&tip.checksum),
        "Execution journal is behind or differs from its committed marker"
    );
    // WAL ahead of a marker is possible after a crash before publication.
    // Replay those records conservatively; recovery still requires new proof.
    Ok(())
}

pub(super) struct Writer {
    sink: Box<dyn JournalSink>,
    locks: Vec<AccountLock>,
    directory: Option<PathBuf>,
    session_id: String,
    schema: u8,
    checksum: String,
    poisoned: bool,
    bytes: u64,
}

impl Writer {
    pub(super) fn create(directory_path: &Path, spec: &SessionSpec) -> Result<Self> {
        directory(directory_path, true)?;
        let held = locks(directory_path, spec)?;
        for lock in &held {
            ensure!(
                active_tip(&lock.marker)?.is_none(),
                "An earlier execution session still owns this account"
            );
        }
        let file = create_file(&path(directory_path, spec)?)?;
        file.try_lock()
            .map_err(|_| anyhow::anyhow!("Execution journal is already locked"))?;
        file.sync_all()?;
        for lock in &held {
            publish_tip(
                &lock.marker,
                &ActiveTip {
                    schema: 1,
                    session_id: spec.session_id.clone(),
                    sequence: 0,
                    checksum: String::new(),
                },
            )?;
        }
        File::open(directory_path)?.sync_all()?;
        Ok(Self {
            sink: Box::new(file),
            locks: held,
            directory: Some(directory_path.to_owned()),
            session_id: spec.session_id.clone(),
            schema: SCHEMA_CURRENT,
            checksum: String::new(),
            poisoned: false,
            bytes: 0,
        })
    }

    pub(super) fn recover(directory_path: &Path, expected: &SessionSpec) -> Result<(Self, Ledger)> {
        directory(directory_path, false)?;
        let held = locks(directory_path, expected)?;
        let mut file = private_file(&path(directory_path, expected)?, true)?;
        file.try_lock()
            .map_err(|_| anyhow::anyhow!("Execution journal is already locked"))?;
        let (ledger, checksums, bytes, schema) = read(&mut file, expected)?;
        for lock in &held {
            validate_tip(&lock.marker, &ledger, &checksums)?;
        }
        // A complete tail may have survived a failed fsync. Make it durable
        // before a recovered Closed record can release ownership.
        file.sync_all()?;
        let checksum = checksums.last().unwrap().clone();
        Ok((
            Self {
                sink: Box::new(file),
                locks: held,
                directory: Some(directory_path.to_owned()),
                session_id: expected.session_id.clone(),
                schema,
                checksum,
                poisoned: false,
                bytes,
            },
            ledger,
        ))
    }

    pub(super) fn schema(&self) -> u8 {
        self.schema
    }

    pub(super) fn poisoned(&self) -> bool {
        self.poisoned
    }

    pub(super) fn release_markers(&mut self) -> Result<()> {
        ensure!(
            !self.poisoned,
            "Execution journal stopped after a write failure"
        );
        // The caller has durably recorded Closed. Clear only this session's
        // markers; partial cleanup remains recoverable from that record.
        for lock in &self.locks {
            let active = active_tip(&lock.marker)?;
            ensure!(
                active
                    .as_ref()
                    .is_none_or(|tip| tip.session_id == self.session_id),
                "Cannot release another session's marker"
            );
        }
        for lock in &self.locks {
            match fs::remove_file(&lock.marker) {
                Ok(()) => (),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return Err(error.into()),
            }
        }
        if let Some(directory) = &self.directory {
            File::open(directory)?.sync_all()?;
        }
        Ok(())
    }

    pub(super) fn append(
        &mut self,
        record: &Event,
        sequence: u64,
        written_at_us: u64,
    ) -> Result<()> {
        ensure!(
            !self.poisoned,
            "Execution journal stopped after a write failure"
        );
        let (checksum, bytes) = if self.schema >= SCHEMA_V2 {
            encode_line(Payload {
                schema: self.schema,
                sequence,
                previous_checksum: self.checksum.clone(),
                written_at_us,
                record: record.clone(),
            })?
        } else {
            encode_line(Payload {
                schema: SCHEMA_LEGACY,
                sequence,
                previous_checksum: self.checksum.clone(),
                written_at_us,
                record: LegacyEvent::from_non_start(record)?,
            })?
        };
        ensure!(
            bytes.len() <= MAX_LINE_BYTES && self.bytes + bytes.len() as u64 <= MAX_BYTES,
            "Execution journal capacity exceeded"
        );
        // Poison before I/O: partial write or failed fsync must never be
        // followed by another record or an outward dispatch permission.
        self.poisoned = true;
        self.sink
            .write_line(&bytes)
            .map_err(|_| anyhow::anyhow!("Execution journal write failed"))?;
        self.sink
            .sync()
            .map_err(|_| anyhow::anyhow!("Execution journal sync failed"))?;
        for lock in &self.locks {
            publish_tip(
                &lock.marker,
                &ActiveTip {
                    schema: 1,
                    session_id: self.session_id.clone(),
                    sequence,
                    checksum: checksum.clone(),
                },
            )
            .map_err(|_| anyhow::anyhow!("Execution committed marker sync failed"))?;
        }
        self.checksum = checksum;
        self.bytes += bytes.len() as u64;
        self.poisoned = false;
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn with_sink(sink: Box<dyn JournalSink>) -> Self {
        Self {
            sink,
            locks: Vec::new(),
            directory: None,
            session_id: String::new(),
            schema: SCHEMA_CURRENT,
            checksum: String::new(),
            poisoned: false,
            bytes: 0,
        }
    }
}

fn encode_line<E: Serialize>(payload: Payload<E>) -> Result<(String, Vec<u8>)> {
    let checksum = hash(&serde_json::to_vec(&payload)?);
    let mut bytes = serde_json::to_vec(&Line {
        payload,
        checksum: checksum.clone(),
    })?;
    bytes.push(b'\n');
    Ok((checksum, bytes))
}

fn read_lines<'a, E: Serialize + DeserializeOwned>(
    schema: u8,
    mut ledger: Ledger,
    raw_lines: impl Iterator<Item = &'a [u8]>,
    to_event: impl Fn(E) -> Result<Event>,
) -> Result<(Ledger, Vec<String>)> {
    let mut checksums = vec![String::new()];
    let mut previous = String::new();
    for raw in raw_lines {
        ensure!(
            !raw.is_empty() && raw.len() < MAX_LINE_BYTES,
            "Invalid execution record length"
        );
        let line: Line<E> = serde_json::from_slice(raw)
            .map_err(|_| anyhow::anyhow!("Malformed execution journal record"))?;
        ensure!(
            line.payload.schema == schema
                && line.payload.sequence == ledger.sequence + 1
                && line.payload.previous_checksum == previous,
            "Execution journal schema/sequence/chain mismatch"
        );
        ensure!(
            line.checksum == hash(&serde_json::to_vec(&line.payload)?),
            "Execution record checksum mismatch"
        );
        let event = to_event(line.payload.record)?;
        ensure!(
            ledger.sequence != 0 || matches!(event, Event::Start(_)),
            "Execution header missing"
        );
        ledger.apply_replay(&event, line.payload.written_at_us)?;
        previous = line.checksum;
        checksums.push(previous.clone());
    }
    Ok((ledger, checksums))
}

fn read(file: &mut File, expected: &SessionSpec) -> Result<(Ledger, Vec<String>, u64, u8)> {
    ensure!(
        file.metadata()?.len() <= MAX_BYTES,
        "Execution journal exceeds capacity"
    );
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        !bytes.is_empty() && bytes.len() as u64 <= MAX_BYTES && bytes.last() == Some(&b'\n'),
        "Empty, oversized or torn execution journal"
    );
    let first = bytes[..bytes.len() - 1]
        .split(|b| *b == b'\n')
        .next()
        .context("Empty execution journal")?;
    ensure!(
        !first.is_empty() && first.len() < MAX_LINE_BYTES,
        "Invalid execution record length"
    );
    let probe: SchemaProbe = serde_json::from_slice(first)
        .map_err(|_| anyhow::anyhow!("Malformed execution journal record"))?;
    let schema = probe.payload.schema;
    let raw_lines = bytes[..bytes.len() - 1].split(|b| *b == b'\n');
    let (ledger, checksums) = match schema {
        SCHEMA_CURRENT => read_lines::<Event>(
            SCHEMA_CURRENT,
            Ledger::new(expected.clone())?,
            raw_lines,
            Ok,
        )?,
        SCHEMA_V2 => {
            read_lines::<Event>(SCHEMA_V2, Ledger::new_v2(expected.clone())?, raw_lines, Ok)?
        }
        SCHEMA_LEGACY => read_lines::<LegacyEvent>(
            SCHEMA_LEGACY,
            Ledger::new_legacy(expected.clone())?,
            raw_lines,
            |record| record.into_non_start(expected),
        )?,
        _ => anyhow::bail!("Unsupported execution journal schema"),
    };
    Ok((ledger, checksums, bytes.len() as u64, schema))
}

pub(super) fn audit(directory_path: &Path, expected: &SessionSpec) -> Result<Ledger> {
    expected.validate()?;
    directory(directory_path, false)?;
    let mut file = private_file(&path(directory_path, expected)?, false)?;
    file.try_lock_shared()
        .map_err(|_| anyhow::anyhow!("Execution journal is being written"))?;
    let (ledger, checksums, _, _) = read(&mut file, expected)?;
    if !ledger.closed {
        for key in account_keys(expected)? {
            validate_tip(
                &directory_path.join(format!("{key}.active.json")),
                &ledger,
                &checksums,
            )?;
        }
    }
    // Closed historical journals may belong to accounts with a newer session.
    Ok(ledger)
}
