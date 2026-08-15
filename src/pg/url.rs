//! Connection URL handling.
//!
//! The `postgres` driver does the real parsing (percent-decoding, multi-host
//! lists, libpq keywords); this layer only splits out altonic's own `schema`
//! parameter — which the driver would reject as unknown — supplies the
//! PGPASSWORD fallback, and keeps a redacted form for logs and reports.

use crate::{rerr, verr, Result};
use postgres::{Client, Config, NoTls};
use std::str::FromStr;

#[derive(Debug, Clone)]
pub struct PgUrl {
    config: Config,
    /// Namespace altonic reads from and writes to (`?schema=`).
    pub schema: String,
    redacted: String,
}

impl PgUrl {
    pub fn is_pg_url(s: &str) -> bool {
        s.starts_with("postgres://") || s.starts_with("postgresql://")
    }

    pub fn parse(text: &str) -> Result<PgUrl> {
        if !PgUrl::is_pg_url(text) {
            return Err(verr(format!("not a postgres URL: {text:?}")));
        }
        let mut url = url::Url::parse(text)
            .map_err(|e| verr(format!("bad postgres URL {text:?}: {e}")))?;
        if url.host_str().unwrap_or("").is_empty() {
            return Err(verr(format!("postgres URL missing host: {text:?}")));
        }
        if url.path().trim_start_matches('/').is_empty() {
            return Err(verr(format!("postgres URL must include a database: {text:?}")));
        }

        // `schema` is ours; everything else is passed through to the driver.
        let pairs: Vec<(String, String)> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        let mut schema = "public".to_string();
        let mut passthrough: Vec<(String, String)> = Vec::new();
        for (k, v) in pairs {
            match k.as_str() {
                "schema" => schema = v,
                // altonic links no TLS backend, so a mode that mandates TLS
                // is rejected here rather than failing at connect time.
                "sslmode" if !matches!(v.as_str(), "disable" | "prefer" | "allow") => {
                    return Err(verr(format!(
                        "sslmode={v} requires TLS, which altonic is not built with; \
                         use sslmode=disable against servers that accept plain TCP"
                    )))
                }
                _ => passthrough.push((k, v)),
            }
        }
        set_query(&mut url, &passthrough);

        let mut config = Config::from_str(url.as_str())
            .map_err(|e| verr(format!("bad postgres URL {text:?}: {e}")))?;
        if config.get_user().is_none() {
            config.user("postgres");
        }
        if config.get_password().is_none() {
            if let Some(pw) = std::env::var("PGPASSWORD").ok().filter(|p| !p.is_empty()) {
                config.password(pw);
            }
        }

        // Redacted form: no userinfo password, no `password=` parameter.
        let mut shown = url.clone();
        let _ = shown.set_password(None);
        let kept: Vec<(String, String)> =
            passthrough.into_iter().filter(|(k, _)| k != "password").collect();
        set_query(&mut shown, &kept);

        Ok(PgUrl { config, schema, redacted: shown.to_string() })
    }

    /// Open a connection with the session pinned to UTC, so timestamptz text
    /// is stable (+00) and shift arithmetic in datetime generators is correct.
    pub fn connect(&self) -> Result<Client> {
        let mut client = self
            .config
            .connect(NoTls)
            .map_err(|e| rerr(format!("cannot connect to {}: {e}", self.redacted)))?;
        client
            .batch_execute("SET TIME ZONE 'UTC'")
            .map_err(|e| rerr(format!("postgres: {e}")))?;
        Ok(client)
    }

    /// Redacted form for logs and reports.
    pub fn redacted(&self) -> &str {
        &self.redacted
    }
}

fn set_query(url: &mut url::Url, pairs: &[(String, String)]) {
    if pairs.is_empty() {
        url.set_query(None);
    } else {
        url.query_pairs_mut()
            .clear()
            .extend_pairs(pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_url() {
        let u = PgUrl::parse(
            "postgres://alice:s%40crit@db.example:6432/prod?schema=app&sslmode=disable",
        )
        .unwrap();
        assert_eq!(u.config.get_user(), Some("alice"));
        assert_eq!(u.config.get_password(), Some(&b"s@crit"[..]));
        assert_eq!(u.config.get_dbname(), Some("prod"));
        assert_eq!(u.schema, "app");
        assert!(!u.redacted().contains("crit"));
    }

    #[test]
    fn defaults() {
        let u = PgUrl::parse("postgresql://localhost/mydb").unwrap();
        assert_eq!(u.config.get_user(), Some("postgres"));
        assert_eq!(u.schema, "public");
    }

    #[test]
    fn password_parameter_is_redacted() {
        let u = PgUrl::parse("postgres://bob@h/db?password=hunter2").unwrap();
        assert_eq!(u.config.get_password(), Some(&b"hunter2"[..]));
        assert!(!u.redacted().contains("hunter2"), "{}", u.redacted());
    }

    #[test]
    fn rejects_bad_urls() {
        assert!(PgUrl::parse("mysql://x/y").is_err());
        assert!(PgUrl::parse("postgres://host").is_err()); // no db
        assert!(PgUrl::parse("postgres://host:notaport/db").is_err());
        assert!(PgUrl::parse("postgres://u@h/db?sslmode=require").is_err()); // needs TLS
        assert!(PgUrl::parse("postgres://u@h/db?bogus=1").is_err());
    }
}
