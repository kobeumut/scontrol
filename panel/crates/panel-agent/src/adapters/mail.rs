//! Mailbox management for the optional Postfix + Dovecot virtual mail stack
//! (only compiled in when the `mail` cargo feature is enabled).
//!
//! Mailboxes are stored as Dovecot virtual users. Passwords are hashed
//! in-process as SHA512-CRYPT, so the plaintext never appears on a command
//! line (`doveadm pw -p` would put it where `ps` can read it), and the
//! resulting map is rebuilt with `postmap`.

use super::exec::run;
use panel_core::error::{CoreError, CoreResult};
use panel_core::types::MailboxInfo;

const VMAILBOX_PATH: &str = "/etc/postfix/vmailbox";
const VIRTUAL_USERS_PATH: &str = "/etc/dovecot/users";

fn split_address(address: &str) -> CoreResult<(String, String)> {
    let mut parts = address.splitn(2, '@');
    let user = parts.next().unwrap_or_default();
    let domain = parts.next().unwrap_or_default();
    if user.is_empty()
        || domain.is_empty()
        || !panel_core::types::validate::is_valid_domain(domain)
        || !user
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
    {
        return Err(CoreError::Validation(format!(
            "invalid mail address: {address}"
        )));
    }
    Ok((user.to_string(), domain.to_string()))
}

pub async fn create_mailbox(
    address: &str,
    password: &str,
    quota_mb: u32,
) -> CoreResult<MailboxInfo> {
    let (user, domain) = split_address(address)?;

    let hashed = hash_password(password)?;

    append_line(
        VIRTUAL_USERS_PATH,
        &format!(
            "{address}:{hashed}::::::userdb_quota_rule=*:bytes={}M",
            quota_mb
        ),
    )
    .await?;
    append_line(VMAILBOX_PATH, &format!("{address} {domain}/{user}/")).await?;

    run("postmap", &[VMAILBOX_PATH]).await?;
    run("systemctl", &["reload", "postfix"]).await?;
    run("systemctl", &["reload", "dovecot"]).await?;

    Ok(MailboxInfo {
        address: address.to_string(),
        domain,
        quota_mb,
    })
}

/// Dovecot `{SHA512-CRYPT}$6$...` hash with a random salt, the same format
/// `doveadm pw -s SHA512-CRYPT` prints.
fn hash_password(password: &str) -> CoreResult<String> {
    if password.is_empty() || password.len() > 1024 || password.contains(['\0', '\n', '\r']) {
        return Err(CoreError::Validation(
            "mailbox password must be 1..=1024 characters on one line".into(),
        ));
    }
    let params = sha_crypt::Sha512Params::new(sha_crypt::ROUNDS_DEFAULT)
        .map_err(|e| CoreError::External(format!("SHA512-CRYPT parameters: {e:?}")))?;
    let hash = sha_crypt::sha512_simple(password, &params)
        .map_err(|e| CoreError::External(format!("hashing mailbox password: {e:?}")))?;
    Ok(format!("{{SHA512-CRYPT}}{hash}"))
}

pub async fn delete_mailbox(address: &str) -> CoreResult<()> {
    split_address(address)?;
    remove_matching_lines(VIRTUAL_USERS_PATH, address).await?;
    remove_matching_lines(VMAILBOX_PATH, address).await?;
    run("postmap", &[VMAILBOX_PATH]).await?;
    run("systemctl", &["reload", "postfix"]).await?;
    run("systemctl", &["reload", "dovecot"]).await?;
    Ok(())
}

async fn append_line(path: &str, line: &str) -> CoreResult<()> {
    use tokio::io::AsyncWriteExt;
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .map_err(|e| CoreError::Io(format!("opening {path}: {e}")))?;
    file.write_all(format!("{line}\n").as_bytes())
        .await
        .map_err(|e| CoreError::Io(format!("writing {path}: {e}")))?;
    Ok(())
}

async fn remove_matching_lines(path: &str, needle: &str) -> CoreResult<()> {
    let content = tokio::fs::read_to_string(path).await.unwrap_or_default();
    let filtered: String = content
        .lines()
        .filter(|l| !l.starts_with(needle))
        .map(|l| format!("{l}\n"))
        .collect();
    tokio::fs::write(path, filtered)
        .await
        .map_err(|e| CoreError::Io(format!("writing {path}: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_hash_in_process_to_dovecot_sha512_crypt() {
        let hashed = hash_password("correct horse:battery").unwrap();
        let crypt = hashed.strip_prefix("{SHA512-CRYPT}$6$").unwrap();
        assert!(!hashed.contains("correct horse"));
        // The users file is colon-separated; a hash must never add a field.
        assert!(!crypt.contains(':'));
        sha_crypt::sha512_check("correct horse:battery", &hashed["{SHA512-CRYPT}".len()..])
            .unwrap();
        assert_ne!(
            hashed,
            hash_password("correct horse:battery").unwrap(),
            "salt must be random"
        );
        for bad in ["", "two\nlines", "nul\0byte"] {
            assert!(matches!(hash_password(bad), Err(CoreError::Validation(_))));
        }
    }
}
