//! UFW firewall management.

use super::exec::run;
use panel_core::error::CoreResult;
use panel_core::types::FirewallProtocol;

pub async fn apply_rule(port: u16, protocol: FirewallProtocol, allow: bool) -> CoreResult<()> {
    let rule = format!("{port}/{protocol}");
    let action = if allow { "allow" } else { "deny" };
    run("ufw", &[action, &rule]).await?;
    Ok(())
}
