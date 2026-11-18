#[cfg(target_os = "macos")]
// Use the safe Security.framework wrapper for Keychain access and randomness.
use security_framework::os::macos::keychain::SecKeychain;
use security_framework::os::macos::passwords::find_generic_password;
use security_framework::random::SecRandom;
const ITEM_NOT_FOUND: i32 = -25300;
const DUPLICATE_ITEM: i32 = -25299;
use super::KEYCHAIN_SERVICE;
use std::io;

fn keychain_error(error: security_framework::base::Error) -> io::Error {
    io::Error::other(format!("macOS Keychain error ({})", error.code()))
}

fn add_missing(account: &str, bearer: &[u8]) -> security_framework::base::Result<()> {
    SecKeychain::default()?.add_generic_password(KEYCHAIN_SERVICE, account, bearer)
}

pub(super) fn get(account: &str) -> io::Result<Option<Vec<u8>>> {
    let Some(value) = get_raw(account, 256)? else {
        return Ok(None);
    };
    if !value.iter().all(|byte| (33..=126).contains(byte)) {
        return Err(io::Error::other("invalid bearer in macOS Keychain"));
    }
    Ok(Some(value))
}

pub(super) fn get_raw(account: &str, max: usize) -> io::Result<Option<Vec<u8>>> {
    match find_generic_password(None, KEYCHAIN_SERVICE, account) {
        Ok((password, _item)) => {
            let bytes = password.as_ref();
            if bytes.is_empty() || bytes.len() > max {
                return Err(io::Error::other("invalid macOS Keychain item size"));
            }
            Ok(Some(bytes.to_vec()))
        }
        Err(error) if error.code() == ITEM_NOT_FOUND => Ok(None),
        Err(error) => Err(keychain_error(error)),
    }
}

pub(super) fn get_or_create(account: &str) -> io::Result<Vec<u8>> {
    if let Some(secret) = get(account)? {
        return Ok(secret);
    }
    let bearer = random_bearer()?;
    match add_missing(account, &bearer) {
        Ok(()) => Ok(bearer),
        Err(error) if error.code() == DUPLICATE_ITEM => {
            get(account)?.ok_or_else(|| keychain_error(error))
        }
        Err(error) => Err(keychain_error(error)),
    }
}

pub(super) fn random_bearer() -> io::Result<Vec<u8>> {
    use base64::Engine;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(random_bytes()?)
        .into_bytes())
}

pub(super) fn random_repair_bearer() -> io::Result<Vec<u8>> {
    Ok(random_bytes()?
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
        .into_bytes())
}

fn random_bytes() -> io::Result<[u8; 32]> {
    let mut random = [0; 32];
    // The safe wrapper reports an I/O error, not the original Security OSStatus.
    SecRandom::default()
        .copy_bytes(&mut random)
        .map_err(|error| io::Error::other(format!("macOS random generation failed: {error}")))?;
    Ok(random)
}

pub(super) fn put_missing(account: &str, bearer: &[u8]) -> io::Result<()> {
    super::valid_bearer(bearer)?;
    put_raw_missing(account, bearer, 256)
}

pub(super) fn put_raw_missing(account: &str, bearer: &[u8], max: usize) -> io::Result<()> {
    if bearer.is_empty() || bearer.len() > max {
        return Err(io::Error::other("invalid macOS Keychain item size"));
    }
    match add_missing(account, bearer) {
        Ok(()) => Ok(()),
        Err(error) if error.code() == DUPLICATE_ITEM => {
            if get_raw(account, max)?.as_deref() == Some(bearer) {
                Ok(())
            } else {
                Err(io::Error::other(
                    "stored item differs; no credential replaced",
                ))
            }
        }
        Err(error) => Err(keychain_error(error)),
    }
}
