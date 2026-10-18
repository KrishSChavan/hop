//! This device's long-term identity: an Ed25519 key, the self-signed
//! certificate rebuilt from it, and the fingerprint and device id that
//! follow from the key (docs/M2-DESIGN.md, "Identity").

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use hop_types::DeviceId;
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, PKCS_ED25519};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use zeroize::{Zeroize, Zeroizing};

use crate::Fingerprint;

/// The certificate's only name. It names nothing on purpose: any device
/// that connects receives the certificate.
const CERT_COMMON_NAME: &str = "hop";

/// Longest key file Hop reads. A PEM Ed25519 key is about 120 bytes; the
/// cap keeps a wrong path (a large file) from being read into memory.
const MAX_KEY_FILE_LEN: u64 = 16 * 1024;

/// This device's key pair and certificate.
///
/// Create one with [`Identity::load_or_create`] (the agent and `hop pair`)
/// or [`Identity::generate`] (tests). The certificate is rebuilt from the
/// key every time, so only the key is ever stored; it says only `CN=hop`
/// and keeps rcgen's default validity (1975 to 4096), which Hop never
/// checks.
///
/// The private key never leaves this value except into rustls, and
/// [`fmt::Debug`] shows only the fingerprint. The key bytes held here are
/// wiped when it is dropped (copies inside rustls and ring are outside
/// Hop's reach).
pub struct Identity {
    key: PrivatePkcs8KeyDer<'static>,
    cert: CertificateDer<'static>,
    fingerprint: Fingerprint,
}

/// What [`Identity::load_or_create`] found.
#[derive(Debug)]
pub struct LoadedIdentity {
    /// The identity.
    pub identity: Identity,
    /// True when no key file existed and a new identity was made and saved.
    /// Every laptop paired with the old one must pair again.
    pub created: bool,
    /// True when the key file was readable by other users and its
    /// permissions were tightened back to owner-only (Unix only). Worth a
    /// warning: another account may have copied the key.
    pub tightened: bool,
}

impl Identity {
    /// Makes a new random identity in memory, without saving it.
    ///
    /// # Errors
    ///
    /// [`IdentityError::Generate`] if the system random number generator or
    /// the certificate builder fails.
    pub fn generate() -> Result<Identity, IdentityError> {
        let key_pair = KeyPair::generate_for(&PKCS_ED25519)
            .map_err(|err| IdentityError::Generate(err.to_string()))?;
        Self::from_key_pair(&key_pair)
    }

    /// Reads an identity from a PKCS#8 PEM Ed25519 private key, the format
    /// of the key file.
    ///
    /// # Errors
    ///
    /// [`IdentityError::Parse`] if the text is not an Ed25519 PKCS#8 key.
    /// The error never quotes the text.
    pub fn from_pkcs8_pem(pem: &str) -> Result<Identity, IdentityError> {
        let key_pair = KeyPair::from_pkcs8_pem_and_sign_algo(pem, &PKCS_ED25519)
            .map_err(|_| IdentityError::Parse { path: None })?;
        Self::from_key_pair(&key_pair)
    }

    /// The key as PKCS#8 PEM, as the key file stores it. Wiped from memory
    /// when the returned value is dropped.
    pub fn to_pkcs8_pem(&self) -> Zeroizing<String> {
        // PEM is base64 in lines of 64, between the PKCS#8 labels (RFC
        // 7468). Written here rather than through a PEM library so the
        // only copies of the key are ones this function wipes.
        let encoded = Zeroizing::new(base64(self.key.secret_pkcs8_der()));
        let mut pem = Zeroizing::new(String::from("-----BEGIN PRIVATE KEY-----\n"));
        for line in encoded.as_bytes().chunks(64) {
            pem.push_str(&String::from_utf8_lossy(line));
            pem.push('\n');
        }
        pem.push_str("-----END PRIVATE KEY-----\n");
        pem
    }

    /// Loads the identity from `path`, or makes and saves a new one if no
    /// file is there.
    ///
    /// - A new file is created atomically and never replaces an existing
    ///   one: the key is written to a temporary file that is then
    ///   hard-linked into place, which fails if another Hop process got
    ///   there first, and then that process's file is loaded.
    /// - On Unix the file is created with mode 0600 (not tightened
    ///   afterwards); folders Hop creates for it get mode 0700. An existing
    ///   key file that others can read is tightened back to 0600 once it
    ///   has parsed, and [`LoadedIdentity::tightened`] says so. Existing
    ///   folders, and files that are not a key, are never changed.
    /// - On Windows the file inherits the folder's permissions; pass a
    ///   path under `%LOCALAPPDATA%`, whose profile permissions limit it to
    ///   the user, SYSTEM and Administrators, and which never roams to
    ///   another PC. An explicit access list comes with the move to
    ///   Credential Manager.
    ///
    /// # Errors
    ///
    /// A file that exists but cannot be read or parsed is an error, never
    /// a reason to make a new identity: that would silently unpair every
    /// laptop. Deleting the file on purpose makes a new identity.
    pub fn load_or_create(path: &Path) -> Result<LoadedIdentity, IdentityError> {
        match Self::load(path) {
            Ok(loaded) => return Ok(loaded),
            Err(IdentityError::Read { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            }
            Err(err) => return Err(err),
        }
        let identity = Identity::generate()?;
        let pem = identity.to_pkcs8_pem();
        match write_new_private_file(path, pem.as_bytes()) {
            Ok(()) => Ok(LoadedIdentity {
                identity,
                created: true,
                tightened: false,
            }),
            // Another Hop process (say `hop pair` and `hop run` started
            // together) created the file first: use its identity.
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Self::load(path),
            Err(source) => Err(IdentityError::Write {
                path: path.to_owned(),
                source,
            }),
        }
    }

    /// Loads the identity from an existing key file.
    ///
    /// # Errors
    ///
    /// [`IdentityError::Read`] if the file cannot be read or is not a
    /// regular file (its `source` is `NotFound` when there is no file),
    /// [`IdentityError::Parse`] if it is not an Ed25519 PKCS#8 PEM key, and
    /// [`IdentityError::Permissions`] if its permissions needed tightening
    /// and could not be changed.
    pub fn load(path: &Path) -> Result<LoadedIdentity, IdentityError> {
        let read_error = |source| IdentityError::Read {
            path: path.to_owned(),
            source,
        };
        let mut file = File::open(path).map_err(read_error)?;
        // Only a regular file can hold the key. Anything else (a folder
        // given by mistake) is an error, and is never touched: tightening a
        // folder to 0600 would lock its owner out of everything inside.
        if !file.metadata().map_err(read_error)?.is_file() {
            return Err(read_error(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a regular file",
            )));
        }
        let mut bytes = Zeroizing::new(Vec::new());
        Read::by_ref(&mut file)
            .take(MAX_KEY_FILE_LEN + 1)
            .read_to_end(&mut bytes)
            .map_err(read_error)?;
        let parse_error = || IdentityError::Parse {
            path: Some(path.to_owned()),
        };
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_KEY_FILE_LEN {
            return Err(parse_error());
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| parse_error())?;
        let identity = Identity::from_pkcs8_pem(text).map_err(|_| parse_error())?;
        // Permissions change only once the file is known to be Hop's key; a
        // file that is not one is left exactly as it was.
        let tightened = tighten_file(path, &file)?;
        if tightened {
            tracing::warn!(
                path = %path.display(),
                "the identity key file was readable by other users; it is now readable only by you"
            );
        }
        Ok(LoadedIdentity {
            identity,
            created: false,
            tightened,
        })
    }

    /// This device's fingerprint.
    pub fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    /// This device's id: the first 32 hex characters of the fingerprint.
    pub fn device_id(&self) -> DeviceId {
        self.fingerprint.device_id()
    }

    /// The DER certificate. Certificates are public; this is for
    /// diagnostics and tests.
    pub fn certificate_der(&self) -> &[u8] {
        self.cert.as_ref()
    }

    /// The certificate, for rustls.
    pub(crate) fn cert(&self) -> CertificateDer<'static> {
        self.cert.clone()
    }

    /// A copy of the private key, for rustls.
    pub(crate) fn key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(self.key.clone_key())
    }

    fn from_key_pair(key_pair: &KeyPair) -> Result<Identity, IdentityError> {
        let mut params = CertificateParams::default();
        let mut name = DistinguishedName::new();
        name.push(DnType::CommonName, CERT_COMMON_NAME);
        params.distinguished_name = name;
        let cert = params
            .self_signed(key_pair)
            .map_err(|err| IdentityError::Generate(err.to_string()))?
            .der()
            .clone();
        let fingerprint =
            Fingerprint::of_cert(&cert).map_err(|err| IdentityError::Generate(err.to_string()))?;
        Ok(Identity {
            key: PrivatePkcs8KeyDer::from(key_pair.serialize_der()),
            cert,
            fingerprint,
        })
    }
}

impl Drop for Identity {
    fn drop(&mut self) {
        self.key.zeroize();
    }
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Identity")
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

/// What can go wrong with the identity key file.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    /// A new key or its certificate could not be made.
    #[error("could not create a device key: {0}")]
    Generate(String),
    /// The key file could not be read.
    #[error("could not read the identity key file {}", .path.display())]
    Read {
        /// The file.
        path: PathBuf,
        /// Why; `NotFound` when there is no file.
        #[source]
        source: io::Error,
    },
    /// The key file is not an Ed25519 PKCS#8 PEM key. Hop never replaces
    /// it on its own, because a new key unpairs every laptop.
    #[error(
        "{} is not a valid Hop identity key; Hop will not replace it on its own. \
         Delete the file to make a new identity, then pair the laptops again",
        .path.as_deref().map_or_else(|| "the key".to_owned(), |path| path.display().to_string())
    )]
    Parse {
        /// The file, if the key came from one.
        path: Option<PathBuf>,
    },
    /// A new key file could not be written.
    #[error("could not write the identity key file {}", .path.display())]
    Write {
        /// The file.
        path: PathBuf,
        /// Why.
        #[source]
        source: io::Error,
    },
    /// The key file or its folder was readable by others and could not be
    /// made private.
    #[error("could not make {} readable only by you", .path.display())]
    Permissions {
        /// The file or folder.
        path: PathBuf,
        /// Why.
        #[source]
        source: io::Error,
    },
}

/// Writes `contents` to a new file at `path`, readable only by the user,
/// without ever replacing an existing file. Fails with `AlreadyExists` if
/// `path` exists.
fn write_new_private_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_owned(),
        _ => PathBuf::from("."),
    };
    create_private_dir(&dir)?;
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "identity.pem".to_owned());
    let mut suffix = [0u8; 8];
    getrandom::fill(&mut suffix).map_err(|err| io::Error::other(err.to_string()))?;
    let suffix: String = suffix.iter().map(|byte| format!("{byte:02x}")).collect();
    let temp = dir.join(format!(".{file_name}.{suffix}.tmp"));

    let result = write_and_link(&temp, path, contents);
    // The temporary name is only a staging step; remove it whatever
    // happened. A failure here leaves a harmless private file behind.
    let _ = fs::remove_file(&temp);
    result
}

fn write_and_link(temp: &Path, path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut file = create_private_file(temp)?;
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    match fs::hard_link(temp, path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Err(err),
        Err(_) => {
            // Some file systems (FAT, some network shares) have no hard
            // links. Exclusive creation still never replaces a file; only
            // atomicity is lost, and a half-written key fails to parse
            // with a clear error instead of being replaced.
            let mut file = create_private_file(path)?;
            file.write_all(contents)?;
            file.sync_all()
        }
    }
}

/// Creates a new file that only the user can read, failing if it exists.
#[cfg(unix)]
fn create_private_file(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    // The mode is set as the file is created; the umask can only remove
    // bits from it.
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// Creates a new file, failing if it exists. It inherits its folder's
/// access list.
#[cfg(not(unix))]
fn create_private_file(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

/// Creates `dir` (and any missing parents) with mode 0700. An existing
/// folder is left as it is, since it may be shared (a home folder, a temp
/// folder); the key file itself is always 0600. A folder that other users
/// can write to gets a warning, because they could replace the key file.
#[cfg(unix)]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let mode = fs::metadata(dir)?.permissions().mode();
    if mode & 0o022 != 0 {
        tracing::warn!(
            path = %dir.display(),
            "other users can write to the folder holding Hop's identity key"
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)
}

/// Tightens an existing key file that others can read back to 0600.
/// Returns whether it had to.
#[cfg(unix)]
fn tighten_file(path: &Path, file: &File) -> Result<bool, IdentityError> {
    use std::os::unix::fs::PermissionsExt;
    let permissions_error = |source| IdentityError::Permissions {
        path: path.to_owned(),
        source,
    };
    let mode = file
        .metadata()
        .map_err(permissions_error)?
        .permissions()
        .mode();
    if mode & 0o077 == 0 {
        return Ok(false);
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(permissions_error)?;
    Ok(true)
}

#[cfg(not(unix))]
fn tighten_file(_path: &Path, _file: &File) -> Result<bool, IdentityError> {
    Ok(false)
}

/// Standard base64 with padding (RFC 4648), for PEM.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let symbol = |index: u32| {
        ALPHABET
            .get(usize::try_from(index & 63).unwrap_or(0))
            .map_or('=', |&byte| char::from(byte))
    };
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk.first().copied().unwrap_or(0));
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(symbol(triple >> 18));
        out.push(symbol(triple >> 12));
        out.push(if chunk.len() > 1 {
            symbol(triple >> 6)
        } else {
            '='
        });
        out.push(if chunk.len() > 2 { symbol(triple) } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, empty folder under the system temp folder.
    fn temp_dir(name: &str) -> PathBuf {
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).unwrap();
        let suffix: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let dir = std::env::temp_dir().join(format!("hop-net-{name}-{suffix}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn base64_matches_rfc_4648_vectors() {
        let vectors = [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ];
        for (input, output) in vectors {
            assert_eq!(base64(input.as_bytes()), output);
        }
    }

    #[test]
    fn a_new_identity_has_a_matching_id_and_a_nameless_certificate() {
        let identity = Identity::generate().unwrap();
        let fingerprint = identity.fingerprint();
        assert_eq!(fingerprint.to_hex().len(), 64);
        assert_eq!(
            identity.device_id().as_str(),
            &fingerprint.to_hex()[..32],
            "the id is the first half of the fingerprint"
        );
        assert_eq!(
            Fingerprint::of_certificate(identity.certificate_der()).unwrap(),
            fingerprint,
            "one function computes the fingerprint for both sides"
        );
        assert!(crate::fingerprint::cert_is_ed25519(&identity.cert()));
        // The subject is exactly CN=hop: the DER holds "hop" as a
        // UTF8String and nothing from rcgen's default name.
        let der = identity.certificate_der();
        assert!(der.windows(5).any(|w| w == [0x0c, 0x03, b'h', b'o', b'p']));
        assert!(!der.windows(5).any(|w| w == b"rcgen"));
        assert_ne!(Identity::generate().unwrap().fingerprint(), fingerprint);
    }

    #[test]
    fn pem_round_trips_and_rebuilds_the_same_pin() {
        let identity = Identity::generate().unwrap();
        let pem = identity.to_pkcs8_pem();
        assert!(pem.starts_with("-----BEGIN PRIVATE KEY-----\n"));
        let again = Identity::from_pkcs8_pem(&pem).unwrap();
        assert_eq!(again.fingerprint(), identity.fingerprint());
    }

    #[test]
    fn other_key_types_and_garbage_are_refused() {
        let p256 = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        assert!(matches!(
            Identity::from_pkcs8_pem(&p256.serialize_pem()),
            Err(IdentityError::Parse { path: None })
        ));
        assert!(Identity::from_pkcs8_pem("not a key").is_err());
    }

    #[test]
    fn debug_shows_only_the_fingerprint() {
        let identity = Identity::generate().unwrap();
        let shown = format!("{identity:?}");
        assert!(shown.contains(&identity.fingerprint().to_hex()));
        assert!(!shown.contains("BEGIN"));
    }

    #[test]
    fn load_or_create_makes_a_private_file_once_and_then_loads_it() {
        let dir = temp_dir("identity");
        let path = dir.join("Hop").join("identity.pem");
        let first = Identity::load_or_create(&path).unwrap();
        assert!(first.created);
        assert!(!first.tightened);
        let second = Identity::load_or_create(&path).unwrap();
        assert!(!second.created);
        assert_eq!(
            second.identity.fingerprint(),
            first.identity.fingerprint(),
            "an existing file is loaded, never replaced"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            let dir_mode = fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(dir_mode & 0o777, 0o700);
        }
        // No temporary files are left behind.
        let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, vec![std::ffi::OsString::from("identity.pem")]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn concurrent_first_runs_agree_on_one_identity() {
        let dir = temp_dir("race");
        let path = dir.join("identity.pem");
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || {
                    Identity::load_or_create(&path)
                        .unwrap()
                        .identity
                        .fingerprint()
                })
            })
            .collect();
        let fingerprints: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert!(fingerprints.windows(2).all(|w| w[0] == w[1]));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_damaged_key_file_is_an_error_and_is_left_alone() {
        let dir = temp_dir("damaged");
        let path = dir.join("identity.pem");
        fs::write(
            &path,
            "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n",
        )
        .unwrap();
        let before = fs::read(&path).unwrap();
        let error = Identity::load_or_create(&path).unwrap_err();
        assert!(
            matches!(error, IdentityError::Parse { path: Some(_) }),
            "{error:?}"
        );
        assert!(error.to_string().contains("will not replace it"));
        assert_eq!(fs::read(&path).unwrap(), before);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A folder given as the key path by mistake, or a file that is not a
    /// key, is an error and keeps its permissions: Hop changes a file's
    /// mode only once it knows the file is its key. (Before, a folder was
    /// set to 0600, which locks its owner out of everything inside.)
    #[cfg(unix)]
    #[test]
    fn a_path_that_is_not_a_key_file_keeps_its_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let dir = temp_dir("not-a-key");
        let folder = dir.join("Hop");
        fs::create_dir(&folder).unwrap();
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o755)).unwrap();
        let error = Identity::load_or_create(&folder).unwrap_err();
        assert!(matches!(error, IdentityError::Read { .. }), "{error:?}");
        assert_eq!(mode(&folder), 0o755);

        let notes = dir.join("notes.txt");
        fs::write(&notes, "not a key").unwrap();
        fs::set_permissions(&notes, fs::Permissions::from_mode(0o644)).unwrap();
        let error = Identity::load_or_create(&notes).unwrap_err();
        assert!(matches!(error, IdentityError::Parse { .. }), "{error:?}");
        assert_eq!(mode(&notes), 0o644);
        assert_eq!(fs::read(&notes).unwrap(), b"not a key");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_key_file_readable_by_others_is_tightened() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("loose");
        let path = dir.join("identity.pem");
        let identity = Identity::generate().unwrap();
        fs::write(&path, identity.to_pkcs8_pem().as_bytes()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let loaded = Identity::load_or_create(&path).unwrap();
        assert!(loaded.tightened);
        assert!(!loaded.created);
        assert_eq!(loaded.identity.fingerprint(), identity.fingerprint());
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        fs::remove_dir_all(&dir).unwrap();
    }
}
