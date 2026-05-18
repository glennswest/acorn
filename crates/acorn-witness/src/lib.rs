//! `acorn-witness` — tamper-evident audit trail + Ed25519 device custody.
//!
//! Two independent primitives:
//!
//! * [`WitnessChain`] — append-only SHA-256 hash-linked log. Each on-disk
//!   entry is the 32-byte hash `H(prev_hash || record_bytes)`, with the
//!   genesis `prev_hash` being all-zeros. Verifying the chain means
//!   re-walking it from the underlying store and comparing computed hashes
//!   against the on-disk hashes.
//!
//! * [`Custody`] — Ed25519 device keypair persisted on first boot. Used to
//!   sign attestations of `(epoch, vector_count, witness_head)`.

#![forbid(unsafe_code)]

use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use parking_lot::Mutex;
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Length of one chain entry / device key in bytes.
pub const HASH_LEN: usize = 32;
const SECRET_KEY_LEN: usize = 32;
const SIGNATURE_LEN: usize = 64;

/// A 32-byte SHA-256 chain head.
pub type Hash = [u8; HASH_LEN];

#[derive(Debug, Error)]
pub enum WitnessError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("malformed witness log (length {len} not a multiple of {HASH_LEN})")]
    Malformed { len: u64 },
    #[error("hash mismatch at entry {index}: expected {expected}, got {got}")]
    HashMismatch {
        index: u64,
        expected: String,
        got: String,
    },
    #[error("signature verification failed")]
    BadSignature,
    #[error("custody key file has wrong length: got {got}, want {SECRET_KEY_LEN}")]
    BadKeyLength { got: usize },
}

// ---------------------------------------------------------------------------
// WitnessChain
// ---------------------------------------------------------------------------

/// Append-only SHA-256 hash-linked log.
pub struct WitnessChain {
    path: PathBuf,
    file: Mutex<File>,
    state: Mutex<ChainState>,
}

#[derive(Clone, Copy)]
struct ChainState {
    head: Hash,
    count: u64,
}

impl WitnessChain {
    /// Open or create the chain at `path`.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, WitnessError> {
        let path = path.into();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        let len = file.metadata()?.len();
        if len % HASH_LEN as u64 != 0 {
            return Err(WitnessError::Malformed { len });
        }
        let count = len / HASH_LEN as u64;
        let mut head: Hash = [0u8; HASH_LEN];
        if count > 0 {
            file.seek(SeekFrom::Start(len - HASH_LEN as u64))?;
            file.read_exact(&mut head)?;
        }
        Ok(Self {
            path,
            file: Mutex::new(file),
            state: Mutex::new(ChainState { head, count }),
        })
    }

    /// Append a record to the chain and return the new head.
    pub fn append(&self, record_bytes: &[u8]) -> Result<Hash, WitnessError> {
        let mut state = self.state.lock();
        let mut h = Sha256::new();
        h.update(state.head);
        h.update(record_bytes);
        let next: Hash = h.finalize().into();

        let mut file = self.file.lock();
        file.seek(SeekFrom::End(0))?;
        file.write_all(&next)?;
        file.sync_data()?;
        state.head = next;
        state.count += 1;
        Ok(next)
    }

    pub fn head(&self) -> Hash {
        self.state.lock().head
    }

    pub fn head_hex(&self) -> String {
        hex::encode(self.head())
    }

    pub fn count(&self) -> u64 {
        self.state.lock().count
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Recompute the chain from `records` (in original append order) and
    /// check each computed hash against the stored hash.
    ///
    /// Returns `(entries_checked, final_head)` on success.
    pub fn verify<'a, I>(&self, records: I) -> Result<(u64, Hash), WitnessError>
    where
        I: IntoIterator<Item = &'a [u8]>,
    {
        let stored = self.read_all()?;
        let count = (stored.len() / HASH_LEN) as u64;

        let mut prev: Hash = [0u8; HASH_LEN];
        let mut index = 0u64;
        for rec in records {
            if index >= count {
                return Err(WitnessError::Malformed {
                    len: stored.len() as u64,
                });
            }
            let mut h = Sha256::new();
            h.update(prev);
            h.update(rec);
            let computed: Hash = h.finalize().into();
            let off = (index as usize) * HASH_LEN;
            let stored_slice = &stored[off..off + HASH_LEN];
            if computed.as_slice() != stored_slice {
                return Err(WitnessError::HashMismatch {
                    index,
                    expected: hex::encode(stored_slice),
                    got: hex::encode(computed),
                });
            }
            prev = computed;
            index += 1;
        }
        if index != count {
            return Err(WitnessError::Malformed {
                len: stored.len() as u64,
            });
        }
        Ok((index, prev))
    }

    fn read_all(&self) -> Result<Vec<u8>, WitnessError> {
        let mut file = self.file.lock();
        file.seek(SeekFrom::Start(0))?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        if buf.len() % HASH_LEN != 0 {
            return Err(WitnessError::Malformed {
                len: buf.len() as u64,
            });
        }
        Ok(buf)
    }
}

// ---------------------------------------------------------------------------
// Custody
// ---------------------------------------------------------------------------

/// Ed25519 device keypair persisted on first boot.
pub struct Custody {
    signing: SigningKey,
}

impl Custody {
    /// Load the key from `path`, or generate-and-persist a fresh one if
    /// `path` does not exist.
    pub fn load_or_create(path: impl AsRef<Path>) -> Result<Self, WitnessError> {
        let path = path.as_ref();
        if path.exists() {
            let mut f = File::open(path)?;
            let mut buf = [0u8; SECRET_KEY_LEN];
            let n = f.read(&mut buf)?;
            if n != SECRET_KEY_LEN {
                return Err(WitnessError::BadKeyLength { got: n });
            }
            Ok(Self {
                signing: SigningKey::from_bytes(&buf),
            })
        } else {
            let mut rng = OsRng;
            let signing = SigningKey::generate(&mut rng);
            write_secret(path, &signing.to_bytes())?;
            Ok(Self { signing })
        }
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }

    /// Hex-encoded `device_id`: first 16 hex chars of SHA-256(public_key).
    pub fn device_id(&self) -> String {
        let mut h = Sha256::new();
        h.update(self.public_key());
        let d = h.finalize();
        hex::encode(&d[..8])
    }

    /// Sign `(epoch_le_u64 || count_le_u64 || head)` and return a 64-byte
    /// Ed25519 signature.
    pub fn sign_attestation(&self, epoch: u64, count: u64, head: &Hash) -> [u8; SIGNATURE_LEN] {
        let msg = attestation_message(epoch, count, head);
        self.signing.sign(&msg).to_bytes()
    }

    /// Verify a previously-issued attestation.
    pub fn verify_attestation(
        verifying: &VerifyingKey,
        epoch: u64,
        count: u64,
        head: &Hash,
        sig: &[u8; SIGNATURE_LEN],
    ) -> Result<(), WitnessError> {
        let msg = attestation_message(epoch, count, head);
        let sig = Signature::from_bytes(sig);
        verifying
            .verify(&msg, &sig)
            .map_err(|_| WitnessError::BadSignature)
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing.verifying_key()
    }
}

fn attestation_message(epoch: u64, count: u64, head: &Hash) -> [u8; 8 + 8 + HASH_LEN] {
    let mut msg = [0u8; 8 + 8 + HASH_LEN];
    msg[0..8].copy_from_slice(&epoch.to_le_bytes());
    msg[8..16].copy_from_slice(&count.to_le_bytes());
    msg[16..16 + HASH_LEN].copy_from_slice(head);
    msg
}

#[cfg(unix)]
fn write_secret(path: &Path, bytes: &[u8; SECRET_KEY_LEN]) -> Result<(), WitnessError> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_data()?;
    Ok(())
}

#[cfg(not(unix))]
fn write_secret(path: &Path, bytes: &[u8; SECRET_KEY_LEN]) -> Result<(), WitnessError> {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_data()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        p.push(format!("acorn-witness-test-{pid}-{nanos}-{name}"));
        p
    }

    #[test]
    fn chain_append_and_verify_roundtrip() {
        let path = temp_path("chain.log");
        let chain = WitnessChain::open(&path).unwrap();
        assert_eq!(chain.count(), 0);
        assert_eq!(chain.head(), [0u8; HASH_LEN]);

        let records: [&[u8]; 3] = [b"alpha", b"bravo", b"charlie"];
        for r in records {
            chain.append(r).unwrap();
        }
        assert_eq!(chain.count(), 3);

        let (entries, head) = chain.verify(records.iter().copied()).unwrap();
        assert_eq!(entries, 3);
        assert_eq!(head, chain.head());

        // Reopen and verify still works (head/count restored).
        drop(chain);
        let chain2 = WitnessChain::open(&path).unwrap();
        assert_eq!(chain2.count(), 3);
        assert_eq!(chain2.head(), head);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn chain_detects_tampering() {
        let path = temp_path("tamper.log");
        let chain = WitnessChain::open(&path).unwrap();
        chain.append(b"one").unwrap();
        chain.append(b"two").unwrap();

        let bad: [&[u8]; 2] = [b"one", b"DIFFERENT"];
        let err = chain.verify(bad.iter().copied()).unwrap_err();
        assert!(matches!(err, WitnessError::HashMismatch { index: 1, .. }));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn custody_roundtrip_attestation() {
        let path = temp_path("custody.key");
        let _ = std::fs::remove_file(&path);
        let c = Custody::load_or_create(&path).unwrap();
        let head: Hash = [7u8; HASH_LEN];
        let sig = c.sign_attestation(42, 100, &head);
        Custody::verify_attestation(&c.verifying_key(), 42, 100, &head, &sig).unwrap();

        // Reload from disk, signatures still verify.
        let c2 = Custody::load_or_create(&path).unwrap();
        assert_eq!(c.public_key(), c2.public_key());
        assert_eq!(c.device_id(), c2.device_id());
        Custody::verify_attestation(&c2.verifying_key(), 42, 100, &head, &sig).unwrap();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn custody_rejects_tampered_attestation() {
        let path = temp_path("custody-tamper.key");
        let _ = std::fs::remove_file(&path);
        let c = Custody::load_or_create(&path).unwrap();
        let head: Hash = [9u8; HASH_LEN];
        let sig = c.sign_attestation(1, 2, &head);
        // Wrong count.
        let err = Custody::verify_attestation(&c.verifying_key(), 1, 3, &head, &sig).unwrap_err();
        assert!(matches!(err, WitnessError::BadSignature));
        let _ = std::fs::remove_file(&path);
    }
}
