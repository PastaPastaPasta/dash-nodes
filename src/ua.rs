//! User agent normalization.

/// Groups a raw BIP14 user agent into a release label:
/// `/Dash Core:23.1.8/` and `/Dash Core:23.1.8(bitcore)/` both become `23.1.8`; other clients keep
/// their name (`Lksc Core 5.18.2.1`); anything unparseable is returned trimmed.
pub fn version_label(ua: &str) -> String {
    let first = ua.trim_matches('/').split('/').next().unwrap_or("").trim();
    let Some((name, ver)) = first.split_once(':') else {
        return if first.is_empty() { "(empty)".into() } else { first.into() };
    };
    let ver = ver.split('(').next().unwrap_or(ver).trim();
    if ver.is_empty() {
        return ua.trim_matches('/').into();
    }
    if name == "Dash Core" { ver.to_string() } else { format!("{name} {ver}") }
}

pub fn is_dash_core(ua: &str) -> bool {
    ua.starts_with("/Dash Core:")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(version_label("/Dash Core:23.1.8/"), "23.1.8");
        assert_eq!(version_label("/Dash Core:23.0.2(dash.cryptoscope.io)/"), "23.0.2");
        assert_eq!(version_label("/Dash Core:24.1.0-nightly/"), "24.1.0-nightly");
        assert_eq!(version_label("/Lksc Core:5.18.2.1/"), "Lksc Core 5.18.2.1");
        assert_eq!(version_label("https:/mnowatch.org"), "https:/mnowatch.org");
        assert_eq!(version_label(""), "(empty)");
    }
}
