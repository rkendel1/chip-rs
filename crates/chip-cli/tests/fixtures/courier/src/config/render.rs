//! Canonical text form of a configuration, as printed by `courier config show`.

use super::schema::Config;
use crate::util::duration::format_duration;

pub fn render(config: &Config) -> String {
    let mut out = String::new();
    let c = &config.client;
    out.push_str("[client]\n");
    if let Some(b) = &c.base_url {
        out.push_str(&format!("base_url = {b}\n"));
    }
    out.push_str(&format!("timeout = {}\n", format_duration(c.timeout)));
    if let Some(d) = c.deadline {
        out.push_str(&format!("deadline = {}\n", format_duration(d)));
    }
    if let Some(w) = c.max_wait {
        out.push_str(&format!("max_wait = {}\n", format_duration(w)));
    }
    out.push_str(&format!("max_inflight = {}\n", c.max_inflight));
    out.push_str(&format!("user_agent = \"{}\"\n", c.user_agent));

    out.push_str(&format!("\n[auth]\nscheme = {}\n", config.auth.scheme));
    out.push_str(&format!(
        "\n[journal]\nenabled = {}\n",
        config.journal.enabled
    ));
    if let Some(p) = &config.journal.path {
        out.push_str(&format!("path = {p}\n"));
    }
    out.push_str(&format!(
        "\n[circuit]\nenabled = {}\nthreshold = {}\ncooldown = {}\n",
        config.circuit.enabled,
        config.circuit.threshold,
        format_duration(config.circuit.cooldown)
    ));
    out.push_str(&format!(
        "\n[rate_limit]\nenabled = {}\nrate = {}\nburst = {}\n",
        config.rate_limit.enabled, config.rate_limit.rate, config.rate_limit.burst
    ));
    for r in &config.routes {
        out.push_str(&format!("\n[route.{}]\nprefix = {}\n", r.name, r.prefix));
        if let Some(t) = r.timeout {
            out.push_str(&format!("timeout = {}\n", format_duration(t)));
        }
        if let Some(d) = r.deadline {
            out.push_str(&format!("deadline = {}\n", format_duration(d)));
        }
        if let Some(w) = r.max_wait {
            out.push_str(&format!("max_wait = {}\n", format_duration(w)));
        }
        if let Some(n) = r.max_inflight {
            out.push_str(&format!("max_inflight = {n}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load;

    #[test]
    fn rendering_then_loading_is_a_fixed_point() {
        let text = "[client]\ntimeout = 3s\nmax_wait = 20s\n[route.r]\nprefix = /r\nmax_inflight = 2\n";
        let first = load(text, &[]).unwrap().config;
        let again = load(&render(&first), &[]).unwrap().config;
        assert_eq!(first, again);
    }

    #[test]
    fn defaults_render_readably() {
        let text = render(&load("", &[]).unwrap().config);
        assert!(text.starts_with("[client]\ntimeout = 10s\n"));
        assert!(text.contains("[circuit]\nenabled = true\nthreshold = 5\ncooldown = 30s\n"));
    }
}
