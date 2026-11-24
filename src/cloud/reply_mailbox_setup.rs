//! Stage a dedicated ordinary mailbox bearer without changing the active mailbox registration.
use radio999k::private_file::{MAX_PRIVATE_CONFIG_BYTES, open_private_file};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
};

const TOKEN_LEN: u64 = 64;

fn read_private(path: &Path, max: u64) -> io::Result<Option<Vec<u8>>> {
    match path.symlink_metadata() {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    let mut bytes = Vec::new();
    open_private_file(path)
        .map_err(|_| io::Error::other("unsafe reply mailbox setup file"))?
        .take(max + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(io::Error::other(
            "reply mailbox setup file exceeds its size limit",
        ));
    }
    Ok(Some(bytes))
}

fn create_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::File::open(path.parent().expect("setup files have a parent"))?.sync_all()
}

pub(crate) fn stage(home: &Path, mailbox: &str) -> io::Result<Value> {
    let profile = match mailbox {
        "instinct" | "cue" => radio999k::mailbox_catalog::http_by_id(mailbox),
        _ => None,
    }
    .ok_or_else(|| io::Error::other("--operational supports instinct and cue"))?;
    let metadata = home.symlink_metadata()?;
    // SAFETY: geteuid has no arguments or preconditions.
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::other(
            "reply mailbox setup requires an owner-private hub home",
        ));
    }
    let home = home.canonicalize()?;
    let policy_path = home.join(profile.policy_file);
    let token_path = home.join(format!("pigeonmq-cloud-{mailbox}-token"));
    let policy = read_private(&policy_path, MAX_PRIVATE_CONFIG_BYTES)?;
    let created = policy.is_none();
    let token = match read_private(&token_path, TOKEN_LEN)? {
        Some(token)
            if token.len() as u64 == TOKEN_LEN && token.iter().all(u8::is_ascii_hexdigit) =>
        {
            token
        }
        Some(_) => {
            return Err(io::Error::other(
                "existing reply mailbox token is invalid; file preserved",
            ));
        }
        None if !created => {
            return Err(io::Error::other(
                "existing reply mailbox policy has no saved token; policy preserved",
            ));
        }
        None => {
            // Match local mailbox staging: two UUIDv4 values provide about 244 random bits.
            let token = format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            )
            .into_bytes();
            create_private(&token_path, &token)?;
            token
        }
    };
    let digest: String = Sha256::digest(&token)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let expected = json!({"principals":[{
        "id":mailbox,"token_sha256":digest,"operations":["read","wait","reply","send"],
        "send_to":[],"mailboxes":[mailbox],"revoked":false
    }]});
    if let Some(policy) = policy {
        let actual = serde_json::from_slice::<Value>(&policy).map_err(|_| {
            io::Error::other("existing reply mailbox policy is invalid; file preserved")
        })?;
        if actual != expected || crate::auth::validate_private(&policy_path).is_err() {
            return Err(io::Error::other(
                "existing reply mailbox policy differs from the ordinary mailbox grant or saved token; file preserved",
            ));
        }
    } else {
        create_private(&policy_path, &serde_json::to_vec_pretty(&expected)?)?;
    }
    Ok(json!({"mailbox":mailbox,"policy":policy_path,"token_file":token_path,"created":created}))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAILBOX: &str = "instinct";
    const POLICY: &str = "pigeonmq-cloud-instinct-policy.json";
    const TOKEN: &str = "pigeonmq-cloud-instinct-token";

    fn home() -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700)).unwrap();
        home
    }

    #[test]
    fn stages_ordinary_mailbox_authority_and_resumes_an_existing_token() {
        let home = home();
        let token = b"a".repeat(TOKEN_LEN as usize);
        let token_path = home.path().join(TOKEN);
        let policy_path = home.path().join(POLICY);
        create_private(&token_path, &token).unwrap();

        let first = stage(home.path(), MAILBOX).unwrap();
        assert_eq!(first["created"], true);
        assert!(
            !first
                .to_string()
                .contains(std::str::from_utf8(&token).unwrap())
        );
        assert_eq!(fs::read(&token_path).unwrap(), token);
        for path in [&token_path, &policy_path] {
            assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        }
        let mut bearer = b"Bearer ".to_vec();
        bearer.extend(&token);
        let principal = crate::auth::authenticate_private(&policy_path, &[bearer]).unwrap();
        principal.identity(MAILBOX).unwrap();
        principal.operation(crate::auth::Operation::Reply).unwrap();
        principal.operation(crate::auth::Operation::Read).unwrap();
        principal.operation(crate::auth::Operation::Send).unwrap();
        principal.operation(crate::auth::Operation::Wait).unwrap();
        for operation in [
            crate::auth::Operation::Receive,
            crate::auth::Operation::Respond,
            crate::auth::Operation::Request,
            crate::auth::Operation::InspectDlq,
            crate::auth::Operation::Status,
        ] {
            assert!(principal.operation(operation).is_err());
        }
        assert_eq!(principal.mailboxes(), [MAILBOX]);
        let original_policy = fs::read(&policy_path).unwrap();
        assert_eq!(stage(home.path(), MAILBOX).unwrap()["created"], false);
        assert_eq!(fs::read(&policy_path).unwrap(), original_policy);
        assert_eq!(fs::read_dir(home.path()).unwrap().count(), 2);
    }

    #[test]
    fn leaves_existing_files_in_place_when_staging_cannot_resume() {
        let home = home();
        let policy = home.path().join(POLICY);
        let token = home.path().join(TOKEN);
        create_private(&policy, b"{}").unwrap();
        assert!(stage(home.path(), MAILBOX).is_err());
        assert!(!token.exists());
        create_private(&token, b"partial").unwrap();
        assert!(stage(home.path(), MAILBOX).is_err());
        assert_eq!(fs::read(&policy).unwrap(), b"{}");
        assert_eq!(fs::read(&token).unwrap(), b"partial");
    }

    #[test]
    fn cue_and_instinct_stage_independent_mailbox_credentials() {
        let home = home();
        let cue = stage(home.path(), "cue").unwrap();
        let instinct = stage(home.path(), MAILBOX).unwrap();
        let cue_token = fs::read(cue["token_file"].as_str().unwrap()).unwrap();
        let instinct_token = fs::read(instinct["token_file"].as_str().unwrap()).unwrap();
        assert_ne!(cue_token, instinct_token);
        let cue_policy = Path::new(cue["policy"].as_str().unwrap());
        let authorization = |token: &[u8]| [b"Bearer ".iter().chain(token).copied().collect()];
        assert!(
            crate::auth::authenticate_private(cue_policy, &authorization(&instinct_token)).is_err()
        );
        let principal =
            crate::auth::authenticate_private(cue_policy, &authorization(&cue_token)).unwrap();
        principal.identity("cue").unwrap();
        principal.operation(crate::auth::Operation::Reply).unwrap();
        assert_eq!(principal.mailboxes(), ["cue"]);
    }
}
