//! UFW firewall management.

use super::exec::run;
use panel_core::error::CoreResult;
use panel_core::types::FirewallProtocol;

/// `ufw` arguments that add (`delete == false`) or delete a rule.
fn rule_args(port: u16, protocol: FirewallProtocol, allow: bool, delete: bool) -> Vec<String> {
    let action = if allow { "allow" } else { "deny" };
    let mut args = Vec::with_capacity(3);
    if delete {
        args.push("delete".to_string());
    }
    args.push(action.to_string());
    args.push(format!("{port}/{protocol}"));
    args
}

pub async fn apply_rule(port: u16, protocol: FirewallProtocol, allow: bool) -> CoreResult<()> {
    let args = rule_args(port, protocol, allow, false);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    run("ufw", &args).await?;
    Ok(())
}

/// Deletes the matching rule. UFW answers a delete of a rule it does not have
/// with "Could not delete non-existent rule" and exit status 0, so removing a
/// rule that is already gone succeeds.
pub async fn remove_rule(port: u16, protocol: FirewallProtocol, allow: bool) -> CoreResult<()> {
    let args = rule_args(port, protocol, allow, true);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    run("ufw", &args).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_argv_adds_and_deletes_the_same_rule() {
        assert_eq!(
            rule_args(8080, FirewallProtocol::Tcp, true, false),
            ["allow", "8080/tcp"]
        );
        assert_eq!(
            rule_args(53, FirewallProtocol::Udp, false, true),
            ["delete", "deny", "53/udp"]
        );
    }
}
