//! Mailbox management for the optional Postfix + Dovecot virtual mail stack
//! (only compiled in when the `mail` cargo feature is enabled).
//!
//! Mailboxes are stored as Dovecot virtual users; `doveadm pw` hashes
//! passwords (never shelled out with the raw password in a format! string —
//! it is passed as a `Command` argument) and the resulting map is rebuilt
//! with `postmap`.

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

    // Hash the password out-of-band via doveadm; the plaintext password
    // never appears in a shell string, only as a discrete Command argument.
    let hashed = run("doveadm", &["pw", "-s", "SHA512-CRYPT", "-p", password])
        .await?
        .trim()
        .to_string();

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
