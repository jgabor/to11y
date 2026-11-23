//! Reading the sign-ins the harnesses store. to11y only reads them: it
//! never refreshes a token or writes a login back.

use std::fmt;
use std::io::{self, Read};
use std::path::Path;

use aes_gcm::aead::consts::U16;
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::aes::Aes256;
use aes_gcm::{AesGcm, Nonce};
use serde_json::Value;

use crate::base64::{self, Alphabet};
use crate::dbus;
use crate::env::Env;
use crate::model::{Harness, Unavailable};

const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// A token or identifier. It can't be printed by accident.
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}

/// Where a sign-in was read from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Source {
    File,
    Keyring,
    /// A file whose key is in the keyring.
    EncryptedFile,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::File => "file",
            Source::Keyring => "keyring",
            Source::EncryptedFile => "encrypted_file",
        }
    }
}

#[derive(Debug)]
pub struct Login {
    pub token: Secret,
    /// The ChatGPT account a Codex token belongs to.
    pub account: Option<Secret>,
    /// Unix seconds.
    pub expires_at: Option<i64>,
    pub plan: Option<String>,
    pub tier: Option<String>,
    pub source: Source,
}

impl Login {
    /// Whether the token is gone, or will be within `margin` seconds.
    pub fn expires_within(&self, now: i64, margin: i64) -> bool {
        self.expires_at.is_some_and(|at| at <= now + margin)
    }
}

/// The parts of the Secret Service that to11y uses.
pub trait Keyring: Send + Sync {
    /// The secret of the one item with these attributes, if there is one.
    fn find(&self, attributes: &[(&str, &str)]) -> Result<Option<Vec<u8>>, Unavailable>;
}

/// The Secret Service on the session bus. It never unlocks anything, so it
/// never prompts.
pub struct SecretService {
    pub address: Option<String>,
}

impl Keyring for SecretService {
    fn find(&self, attributes: &[(&str, &str)]) -> Result<Option<Vec<u8>>, Unavailable> {
        let address = self.address.as_deref().ok_or(Unavailable::KeyringMissing)?;
        match dbus::lookup(address, attributes) {
            Ok(dbus::Lookup::Found(secret)) => Ok(Some(secret)),
            Ok(dbus::Lookup::Missing) => Ok(None),
            Ok(dbus::Lookup::Locked) => Err(Unavailable::KeyringLocked),
            Err(dbus::Error::NoBus | dbus::Error::NoService) => Err(Unavailable::KeyringMissing),
            Err(error) => Err(Unavailable::Failed(format!(
                "The keyring couldn't be read: {error}."
            ))),
        }
    }
}

/// The stored sign-in for a harness, without checking whether it has
/// expired.
pub fn login(harness: Harness, env: &Env, keyring: &dyn Keyring) -> Result<Login, Unavailable> {
    match harness {
        Harness::ClaudeCode => claude(env),
        Harness::Codex => codex(env),
        Harness::Droid => droid(env, keyring),
        Harness::Antigravity => antigravity(env, keyring),
    }
}

/// `.credentials.json` in Claude Code's config directory.
pub fn claude(env: &Env) -> Result<Login, Unavailable> {
    let dir = env.claude_dir().ok_or(Unavailable::NotInstalled)?;
    let json = read_json(
        &dir.join(".credentials.json"),
        &dir,
        "Claude Code's credentials",
    )?;
    let oauth = &json["claudeAiOauth"];
    let token = oauth["accessToken"]
        .as_str()
        .filter(|token| !token.is_empty())
        .ok_or(Unavailable::NotSignedIn)?;
    Ok(Login {
        token: Secret(token.to_owned()),
        account: None,
        expires_at: oauth["expiresAt"].as_i64().map(|ms| ms.div_euclid(1000)),
        plan: text(&oauth["subscriptionType"]),
        tier: text(&oauth["rateLimitTier"]),
        source: Source::File,
    })
}

/// `auth.json` in Codex's home directory. Only a ChatGPT sign-in has
/// subscription limits.
pub fn codex(env: &Env) -> Result<Login, Unavailable> {
    let dir = env.codex_dir().ok_or(Unavailable::NotInstalled)?;
    let json = read_json(&dir.join("auth.json"), &dir, "Codex's auth.json")?;
    let api_key = matches!(json["auth_mode"].as_str(), Some("apikey" | "api_key"))
        || (json["tokens"].is_null() && json["OPENAI_API_KEY"].is_string());
    if api_key {
        return Err(Unavailable::Unsupported(
            "Codex is signed in with an API key, which has no subscription limits.".to_owned(),
        ));
    }
    let tokens = &json["tokens"];
    let token = tokens["access_token"]
        .as_str()
        .filter(|token| !token.is_empty())
        .ok_or(Unavailable::NotSignedIn)?;
    Ok(Login {
        token: Secret(token.to_owned()),
        account: tokens["account_id"]
            .as_str()
            .map(|account| Secret(account.to_owned())),
        expires_at: jwt_expiry(token),
        plan: None,
        tier: None,
        source: Source::File,
    })
}

/// Droid's sign-in, from whichever of its two stores it wrote last:
/// `~/.factory/auth.v2.keyring`, whose key is in the system keyring, or
/// `auth.v2.file`, whose key is in `auth.v2.key` beside it. When both were
/// written at once, the keyring store counts. A newer store that can't be
/// read is a failure, never a reason to use the older one.
pub fn droid(env: &Env, keyring: &dyn Keyring) -> Result<Login, Unavailable> {
    let dir = env.factory_dir().ok_or(Unavailable::NotInstalled)?;
    let modified = |name: &str| {
        std::fs::metadata(dir.join(name))
            .and_then(|meta| meta.modified())
            .ok()
    };
    let in_keyring = match (modified("auth.v2.keyring"), modified("auth.v2.file")) {
        (None, None) if !dir.is_dir() => return Err(Unavailable::NotInstalled),
        (None, None) => return Err(Unavailable::NotSignedIn),
        (Some(keyring), Some(file)) => keyring >= file,
        (keyring, _) => keyring.is_some(),
    };
    let unreadable = || Unavailable::Failed("Droid's stored sign-in couldn't be read.".to_owned());
    let (sealed, key, source) = if in_keyring {
        let sealed = read_capped(&dir.join("auth.v2.keyring")).map_err(|_| unreadable())?;
        let key = keyring
            .find(&[
                ("service", "Factory CLI"),
                ("account", "auth-encryption-key"),
            ])?
            .ok_or(Unavailable::NotSignedIn)?;
        (sealed, key, Source::EncryptedFile)
    } else {
        let sealed = read_capped(&dir.join("auth.v2.file")).map_err(|_| unreadable())?;
        let key = read_capped(&dir.join("auth.v2.key")).map_err(|_| {
            Unavailable::Failed("Droid's stored sign-in has no readable key.".to_owned())
        })?;
        (sealed, key, Source::File)
    };
    let plain = decrypt_factory(&sealed, &key).ok_or_else(|| {
        Unavailable::Failed("Droid's stored sign-in couldn't be decrypted.".to_owned())
    })?;
    let json: Value = serde_json::from_slice(&plain)
        .map_err(|_| Unavailable::Failed("Droid's stored sign-in isn't JSON.".to_owned()))?;
    let token = json["access_token"]
        .as_str()
        .filter(|token| !token.is_empty())
        .ok_or(Unavailable::NotSignedIn)?;
    Ok(Login {
        token: Secret(token.to_owned()),
        account: None,
        expires_at: jwt_expiry(token),
        plan: None,
        tier: None,
        source,
    })
}

/// Antigravity's sign-in: the keyring item its Go keyring library writes,
/// or `antigravity-oauth-token` in one of the CLI's folders, which agy uses
/// when there is no keyring and T3 Code's agy always uses. Of those it can
/// read, the one that stays good longest counts. A locked keyring is never
/// passed over, since agy could unlock it with a prompt.
pub fn antigravity(env: &Env, keyring: &dyn Keyring) -> Result<Login, Unavailable> {
    let bad = || Unavailable::Failed("Antigravity's stored sign-in couldn't be read.".to_owned());
    let mut logins = Vec::new();
    let mut damaged = false;
    let missing = match keyring.find(&[("service", "gemini"), ("username", "antigravity")]) {
        Ok(Some(raw)) => {
            match strip_go_keyring(&raw).map(|json| antigravity_login(&json, Source::Keyring)) {
                Some(Ok(login)) => logins.push(login),
                _ => damaged = true,
            }
            None
        }
        Ok(None) => None,
        Err(Unavailable::KeyringMissing) => Some(Unavailable::KeyringMissing),
        Err(other) => return Err(other),
    };
    let dirs = env.antigravity_cli_dirs();
    for dir in &dirs {
        match read_capped(&dir.join("antigravity-oauth-token")) {
            Ok(bytes) => match antigravity_login(&bytes, Source::File) {
                Ok(login) => logins.push(login),
                Err(_) => damaged = true,
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => damaged = true,
        }
    }
    // An expiry that can't be read counts as the oldest. A tie goes to the
    // keyring, then to the folders in order.
    if let Some(login) = logins
        .into_iter()
        .rev()
        .max_by_key(|login| login.expires_at.unwrap_or(i64::MIN))
    {
        return Ok(login);
    }
    if damaged {
        return Err(bad());
    }
    let installed = env.antigravity_dir().is_some_and(|dir| dir.is_dir())
        || dirs.iter().any(|dir| dir.is_dir())
        || env.find_program("agy").is_some();
    Err(match missing {
        Some(why) => why,
        None if installed => Unavailable::NotSignedIn,
        None => Unavailable::NotInstalled,
    })
}

/// `{"token": {"access_token", "expiry"}}`, from the keyring or the file.
/// Nothing else in it is kept.
fn antigravity_login(bytes: &[u8], source: Source) -> Result<Login, Unavailable> {
    let json: Value = serde_json::from_slice(bytes).map_err(|_| {
        Unavailable::Failed("Antigravity's stored sign-in couldn't be read.".to_owned())
    })?;
    let token = json["token"]["access_token"]
        .as_str()
        .filter(|token| !token.is_empty())
        .ok_or(Unavailable::NotSignedIn)?;
    Ok(Login {
        token: Secret(token.to_owned()),
        account: None,
        expires_at: json["token"]["expiry"]
            .as_str()
            .and_then(crate::time::parse),
        plan: None,
        tier: None,
        source,
    })
}

fn text(value: &Value) -> Option<String> {
    value.as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

fn read_capped(path: &Path) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_FILE_BYTES)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// A JSON login file. A missing file means not signed in, or not installed
/// when its directory is missing too.
fn read_json(path: &Path, dir: &Path, what: &str) -> Result<Value, Unavailable> {
    let bytes = match read_capped(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(if dir.is_dir() {
                Unavailable::NotSignedIn
            } else {
                Unavailable::NotInstalled
            });
        }
        Err(_) => {
            return Err(Unavailable::Failed(format!(
                "{what} file couldn't be read."
            )));
        }
    };
    serde_json::from_slice(&bytes)
        .map_err(|_| Unavailable::Failed(format!("{what} file isn't valid JSON.")))
}

/// The `exp` claim of a JWT, without checking its signature. No other
/// claim is kept.
pub fn jwt_expiry(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let json: Value = serde_json::from_slice(&base64::decode(payload, Alphabet::UrlSafe)?).ok()?;
    json["exp"]
        .as_i64()
        .or_else(|| json["exp"].as_f64().map(|exp| exp as i64))
}

/// Factory's `base64(iv):base64(tag):base64(ciphertext)`, sealed with
/// AES-256-GCM under a 16-byte nonce. The key is base64 text.
pub fn decrypt_factory(sealed: &[u8], key: &[u8]) -> Option<Vec<u8>> {
    let key = base64::decode(std::str::from_utf8(key).ok()?.trim(), Alphabet::Standard)?;
    let parts = std::str::from_utf8(sealed)
        .ok()?
        .trim()
        .split(':')
        .map(|part| base64::decode(part, Alphabet::Standard))
        .collect::<Option<Vec<_>>>()?;
    let [iv, tag, ciphertext] = parts.as_slice() else {
        return None;
    };
    if tag.len() != 16 {
        return None;
    }
    let cipher = AesGcm::<Aes256, U16>::new_from_slice(&key).ok()?;
    let nonce = Nonce::<U16>::try_from(iv.as_slice()).ok()?;
    let mut message = ciphertext.clone();
    message.extend_from_slice(tag);
    cipher.decrypt(&nonce, message.as_slice()).ok()
}

/// Go's keyring library may store a value as `go-keyring-base64:` and its
/// base64. Other values are stored as they are.
pub fn strip_go_keyring(raw: &[u8]) -> Option<Vec<u8>> {
    const PREFIX: &[u8] = b"go-keyring-base64:";
    match raw.strip_prefix(PREFIX) {
        Some(rest) => base64::decode(std::str::from_utf8(rest).ok()?.trim(), Alphabet::Standard),
        None => Some(raw.to_vec()),
    }
}

/// An item's attributes and its secret.
#[cfg(test)]
type Item = (Vec<(String, String)>, Vec<u8>);

/// A keyring in memory, for tests. It can act locked.
#[cfg(test)]
#[derive(Default)]
pub struct MemoryKeyring {
    pub items: Vec<Item>,
    pub locked: bool,
}

#[cfg(test)]
impl MemoryKeyring {
    pub fn with(mut self, attributes: &[(&str, &str)], secret: &[u8]) -> Self {
        let attributes = attributes
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        self.items.push((attributes, secret.to_vec()));
        self
    }
}

#[cfg(test)]
impl Keyring for MemoryKeyring {
    fn find(&self, attributes: &[(&str, &str)]) -> Result<Option<Vec<u8>>, Unavailable> {
        let found = self.items.iter().find(|(item, _)| {
            attributes
                .iter()
                .all(|(k, v)| item.iter().any(|(ik, iv)| ik == k && iv == v))
        });
        match found {
            Some(_) if self.locked => Err(Unavailable::KeyringLocked),
            Some((_, secret)) => Ok(Some(secret.clone())),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    // Made with Node's crypto module, so the crate under test isn't its
    // own oracle. The token in it is a synthetic JWT.
    const FACTORY_KEY: &str = "AQgPFh0kKzI5QEdOVVxjanF4f4aNlJuiqbC3vsXM09o=";
    const FACTORY_FILE: &str = "//z59vPw7ern5OHe29jV0g==:9GCQma2nO3+ql5WjSqTfEA==:CsTg3T79CGmsYjlFZd00IISUYiwx686lKe57tsiM0xskYaYbhyjm8howAemLCIFT8eEVPxsTOyXRpbeX7f/yxqxL3M6OOsOvEyKbyvpyksWTqeDCUp8MVBDRzvqY3kNmYfzHPLmx0ThH3uzijgb36JOapdRoYuiF8wjqNjPhiauwOUPOP5n9XNQz3etfdy7Qog0H2srHZJmMyU21Nkh2xOiAwfKq2Q0scypH7iumVaFxKXIyN6LReYbmQ1kLek/RPpJtPJcnHweD5JrgLa6eyuZFOV06ix0p00bVpvU=";
    const JWT: &str = "eyJhbGciOiJub25lIn0.eyJleHAiOjE3OTU0NDc4NzAsInN1YiI6InVzZXItZml4dHVyZSIsImVtYWlsIjoiZml4dHVyZS11c2VyQGV4YW1wbGUuaW52YWxpZCJ9.c2ln";
    const FACTORY: [(&str, &str); 2] = [
        ("service", "Factory CLI"),
        ("account", "auth-encryption-key"),
    ];
    const GEMINI: [(&str, &str); 2] = [("service", "gemini"), ("username", "antigravity")];

    /// A temporary home directory.
    struct Home(TempDir);

    impl Home {
        fn new(name: &str) -> Self {
            Home(TempDir::new(&format!("creds-{name}")))
        }

        fn write(&self, relative: &str, text: &str) -> &Self {
            self.0.write(relative, text);
            self
        }

        fn env(&self) -> Env {
            Env {
                home: Some(self.0.path().to_owned()),
                ..Env::default()
            }
        }
    }

    #[test]
    fn a_factory_blob_decrypts_to_its_json() {
        let plain = decrypt_factory(FACTORY_FILE.as_bytes(), FACTORY_KEY.as_bytes()).unwrap();
        let json: Value = serde_json::from_slice(&plain).unwrap();
        assert_eq!(json["access_token"], JWT);
    }

    #[test]
    fn a_tampered_factory_blob_does_not_decrypt() {
        let mut file = FACTORY_FILE.as_bytes().to_vec();
        let last = file.len() - 2;
        file[last] = if file[last] == b'A' { b'B' } else { b'A' };
        assert_eq!(decrypt_factory(&file, FACTORY_KEY.as_bytes()), None);
        assert_eq!(decrypt_factory(b"abc:def", FACTORY_KEY.as_bytes()), None);
    }

    #[test]
    fn a_jwt_gives_only_its_expiry() {
        assert_eq!(jwt_expiry(JWT), Some(1_795_447_870));
        assert_eq!(jwt_expiry("not-a-jwt"), None);
        assert_eq!(jwt_expiry("a.!!!.c"), None);
    }

    #[test]
    fn the_go_keyring_prefix_is_stripped() {
        assert_eq!(strip_go_keyring(b"go-keyring-base64:e30=").unwrap(), b"{}");
        assert_eq!(strip_go_keyring(b"{}").unwrap(), b"{}");
        assert_eq!(strip_go_keyring(b"go-keyring-base64:!"), None);
    }

    #[test]
    fn a_droid_login_comes_from_the_file_and_the_keyring() {
        let home = Home::new("droid");
        home.write(".factory/auth.v2.keyring", FACTORY_FILE);
        let keyring = MemoryKeyring::default().with(&FACTORY, FACTORY_KEY.as_bytes());
        let login = droid(&home.env(), &keyring).unwrap();
        assert_eq!(login.token.expose(), JWT);
        assert_eq!(login.expires_at, Some(1_795_447_870));
        assert_eq!(login.source, Source::EncryptedFile);
    }

    #[test]
    fn a_locked_keyring_is_reported_without_a_prompt() {
        let home = Home::new("locked");
        home.write(".factory/auth.v2.keyring", FACTORY_FILE);
        let keyring = MemoryKeyring {
            locked: true,
            ..MemoryKeyring::default()
        }
        .with(&FACTORY, FACTORY_KEY.as_bytes())
        .with(&GEMINI, b"{}");
        assert_eq!(
            droid(&home.env(), &keyring).unwrap_err(),
            Unavailable::KeyringLocked
        );
        assert_eq!(
            antigravity(&home.env(), &keyring).unwrap_err(),
            Unavailable::KeyringLocked
        );
    }

    #[test]
    fn droid_is_not_installed_without_its_directory() {
        let home = Home::new("no-droid");
        let keyring = MemoryKeyring::default();
        assert_eq!(
            droid(&home.env(), &keyring).unwrap_err(),
            Unavailable::NotInstalled
        );
        home.write(".factory/settings.json", "{}");
        assert_eq!(
            droid(&home.env(), &keyring).unwrap_err(),
            Unavailable::NotSignedIn
        );
        home.write(".factory/auth.v2.file", "x");
        assert!(matches!(
            droid(&home.env(), &keyring),
            Err(Unavailable::Failed(_))
        ));
    }

    /// Sets the mtime of `relative` under `home`, in Unix seconds.
    fn touch(home: &Home, relative: &str, at: i64) {
        crate::tokens::tests::touch(&home.0.path().join(relative), at);
    }

    #[test]
    fn a_droid_file_sign_in_is_decrypted_with_the_key_beside_it() {
        let home = Home::new("droid-file");
        home.write(".factory/auth.v2.file", FACTORY_FILE)
            .write(".factory/auth.v2.key", &format!("{FACTORY_KEY}\n"));
        let login = droid(&home.env(), &MemoryKeyring::default()).unwrap();
        assert_eq!(login.token.expose(), JWT);
        assert_eq!(login.source, Source::File);
        let text = format!("{login:?}");
        assert!(!text.contains(FACTORY_KEY), "{text}");
    }

    #[test]
    fn a_droid_file_sign_in_without_its_key_is_a_failure() {
        let home = Home::new("droid-no-key");
        home.write(".factory/auth.v2.file", FACTORY_FILE);
        let keyring = MemoryKeyring::default().with(&FACTORY, FACTORY_KEY.as_bytes());
        assert!(matches!(
            droid(&home.env(), &keyring),
            Err(Unavailable::Failed(_))
        ));
    }

    #[test]
    fn the_droid_store_written_last_counts() {
        let home = Home::new("droid-newer");
        home.write(".factory/auth.v2.keyring", "damaged")
            .write(".factory/auth.v2.file", FACTORY_FILE)
            .write(".factory/auth.v2.key", FACTORY_KEY);
        let keyring = MemoryKeyring::default().with(&FACTORY, FACTORY_KEY.as_bytes());
        touch(&home, ".factory/auth.v2.keyring", 1_000);
        touch(&home, ".factory/auth.v2.file", 2_000);
        assert_eq!(droid(&home.env(), &keyring).unwrap().source, Source::File);
        // Equal times go to the keyring store, which can't be decrypted.
        touch(&home, ".factory/auth.v2.keyring", 2_000);
        assert!(matches!(
            droid(&home.env(), &keyring),
            Err(Unavailable::Failed(_))
        ));
        home.write(".factory/auth.v2.keyring", FACTORY_FILE)
            .write(".factory/auth.v2.file", "damaged");
        touch(&home, ".factory/auth.v2.keyring", 3_000);
        touch(&home, ".factory/auth.v2.file", 2_000);
        let login = droid(&home.env(), &keyring).unwrap();
        assert_eq!(login.source, Source::EncryptedFile);
    }

    #[test]
    fn a_locked_keyring_stays_locked_when_an_older_file_store_exists() {
        let home = Home::new("droid-locked-file");
        home.write(".factory/auth.v2.keyring", FACTORY_FILE)
            .write(".factory/auth.v2.file", FACTORY_FILE)
            .write(".factory/auth.v2.key", FACTORY_KEY);
        touch(&home, ".factory/auth.v2.file", 1_000);
        touch(&home, ".factory/auth.v2.keyring", 2_000);
        let keyring = MemoryKeyring {
            locked: true,
            ..MemoryKeyring::default()
        }
        .with(&FACTORY, FACTORY_KEY.as_bytes());
        assert_eq!(
            droid(&home.env(), &keyring).unwrap_err(),
            Unavailable::KeyringLocked
        );
    }

    #[test]
    fn a_claude_login_has_its_plan_and_expiry_in_seconds() {
        let home = Home::new("claude");
        home.write(
            ".claude/.credentials.json",
            r#"{"claudeAiOauth":{"accessToken":"tok","expiresAt":1795447870123,"subscriptionType":"max","rateLimitTier":"default_claude_max_20x"}}"#,
        );
        let login = claude(&home.env()).unwrap();
        assert_eq!(login.token.expose(), "tok");
        assert_eq!(login.expires_at, Some(1_795_447_870));
        assert_eq!(login.plan.as_deref(), Some("max"));
        assert_eq!(login.tier.as_deref(), Some("default_claude_max_20x"));
        assert!(login.expires_within(1_795_447_870, 0));
        assert!(!login.expires_within(1_795_447_869, 0));
    }

    #[test]
    fn claude_without_a_credentials_file_is_not_signed_in() {
        let home = Home::new("claude-empty");
        assert_eq!(claude(&home.env()).unwrap_err(), Unavailable::NotInstalled);
        home.write(".claude/settings.json", "{}");
        assert_eq!(claude(&home.env()).unwrap_err(), Unavailable::NotSignedIn);
        home.write(".claude/.credentials.json", "{");
        assert!(matches!(claude(&home.env()), Err(Unavailable::Failed(_))));
    }

    #[test]
    fn a_codex_api_key_sign_in_is_unsupported() {
        let home = Home::new("codex");
        home.write(
            ".codex/auth.json",
            r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk"}"#,
        );
        assert!(matches!(
            codex(&home.env()),
            Err(Unavailable::Unsupported(_))
        ));
        home.write(
            ".codex/auth.json",
            &format!(r#"{{"auth_mode":"chatgpt","tokens":{{"access_token":"{JWT}","account_id":"acct"}}}}"#),
        );
        let login = codex(&home.env()).unwrap();
        assert_eq!(login.account.unwrap().expose(), "acct");
        assert_eq!(login.expires_at, Some(1_795_447_870));
    }

    #[test]
    fn an_antigravity_login_reads_the_go_keyring_item() {
        let home = Home::new("agy");
        let item = r#"{"token":{"access_token":"ya29","expiry":"2026-11-23T15:31:10.123456789+01:00"},"auth_method":"oauth"}"#;
        let encoded = format!("go-keyring-base64:{}", encode(item.as_bytes()));
        let keyring = MemoryKeyring::default().with(&GEMINI, encoded.as_bytes());
        let login = antigravity(&home.env(), &keyring).unwrap();
        assert_eq!(login.token.expose(), "ya29");
        assert_eq!(login.expires_at, Some(1_795_444_270));
        let empty = MemoryKeyring::default();
        assert_eq!(
            antigravity(&home.env(), &empty).unwrap_err(),
            Unavailable::NotInstalled
        );
    }

    const AGY_TOKEN: &str = ".gemini/antigravity-cli/antigravity-oauth-token";

    #[test]
    fn without_a_keyring_item_antigravity_reads_its_token_file() {
        let home = Home::new("agy-file");
        home.write(
            AGY_TOKEN,
            r#"{"token":{"access_token":"ya29-file","refresh_token":"secret-refresh","expiry":"2026-11-23T14:31:10Z"}}"#,
        );
        for keyring in [
            MemoryKeyring::default(),
            MemoryKeyring::default().with(&FACTORY, b"other"),
        ] {
            let login = antigravity(&home.env(), &keyring).unwrap();
            assert_eq!(login.token.expose(), "ya29-file");
            assert_eq!(login.expires_at, Some(1_795_444_270));
            assert_eq!(login.source, Source::File);
            assert!(!format!("{login:?}").contains("secret-refresh"));
        }
        let none = SecretService { address: None };
        assert_eq!(
            antigravity(&home.env(), &none).unwrap().source,
            Source::File
        );
    }

    fn agy_token(access: &str, expiry: &str) -> String {
        format!(r#"{{"token":{{"access_token":"{access}","expiry":"{expiry}"}}}}"#)
    }

    #[test]
    fn the_antigravity_sign_in_that_stays_good_longest_counts() {
        let home = Home::new("agy-both");
        let keyring_item = agy_token("ya29-keyring", "2026-11-23T15:00:00Z");
        let keyring = MemoryKeyring::default().with(&GEMINI, keyring_item.as_bytes());
        home.write(AGY_TOKEN, &agy_token("ya29-file", "2026-11-23T15:00:00Z"));
        let login = antigravity(&home.env(), &keyring).unwrap();
        assert_eq!(
            login.token.expose(),
            "ya29-keyring",
            "a tie goes to the keyring"
        );
        home.write(AGY_TOKEN, &agy_token("ya29-file", "2026-11-23T16:00:00Z"));
        let login = antigravity(&home.env(), &keyring).unwrap();
        assert_eq!(login.token.expose(), "ya29-file");
        assert_eq!(login.source, Source::File);
    }

    #[test]
    fn t3_codes_antigravity_sign_in_is_read_through_gemini_home() {
        let home = Home::new("agy-t3");
        home.write(AGY_TOKEN, &agy_token("ya29-plain", "2026-11-21T17:40:51Z"))
            .write(
                "t3/profile/antigravity-acp/antigravity-oauth-token",
                &agy_token("ya29-t3", "2026-11-23T22:42:39.129679963+01:00"),
            );
        let env = Env {
            gemini_home: Some(home.0.path().join("t3/profile")),
            ..home.env()
        };
        let none = SecretService { address: None };
        let login = antigravity(&env, &none).unwrap();
        assert_eq!(login.token.expose(), "ya29-t3");
        assert_eq!(login.expires_at, Some(1_795_470_159));
        // Without GEMINI_HOME, only the plain agy's folders are read.
        let login = antigravity(&home.env(), &none).unwrap();
        assert_eq!(login.token.expose(), "ya29-plain");
    }

    #[test]
    fn a_damaged_antigravity_file_is_passed_over_for_a_good_one() {
        let home = Home::new("agy-damaged");
        home.write(AGY_TOKEN, "{").write(
            ".gemini/antigravity-acp/antigravity-oauth-token",
            &agy_token("ya29-acp", "2026-11-23T15:00:00Z"),
        );
        let none = SecretService { address: None };
        assert_eq!(
            antigravity(&home.env(), &none).unwrap().token.expose(),
            "ya29-acp"
        );
    }

    #[test]
    fn a_locked_keyring_is_never_passed_over_for_a_file() {
        let home = Home::new("agy-locked-file");
        home.write(AGY_TOKEN, &agy_token("ya29-file", "2026-11-23T16:00:00Z"));
        let locked = MemoryKeyring {
            locked: true,
            ..MemoryKeyring::default()
        }
        .with(&GEMINI, b"{}");
        assert_eq!(
            antigravity(&home.env(), &locked).unwrap_err(),
            Unavailable::KeyringLocked
        );
    }

    #[test]
    fn without_a_keyring_or_a_token_file_antigravity_says_which() {
        let home = Home::new("agy-none");
        let none = SecretService { address: None };
        assert_eq!(
            antigravity(&home.env(), &none).unwrap_err(),
            Unavailable::KeyringMissing
        );
        home.write(AGY_TOKEN, "{");
        assert!(matches!(
            antigravity(&home.env(), &none),
            Err(Unavailable::Failed(_))
        ));
    }

    #[test]
    fn secrets_do_not_print() {
        let login = Login {
            token: Secret("tok-secret".to_owned()),
            account: Some(Secret("acct-secret".to_owned())),
            expires_at: None,
            plan: None,
            tier: None,
            source: Source::File,
        };
        let text = format!("{login:?}");
        assert!(
            !text.contains("tok-secret") && !text.contains("acct-secret"),
            "{text}"
        );
    }

    fn encode(bytes: &[u8]) -> String {
        use ::base64::Engine;
        ::base64::engine::general_purpose::STANDARD.encode(bytes)
    }
}
