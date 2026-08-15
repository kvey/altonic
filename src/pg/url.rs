//! Connection URL parsing: postgres://user[:password]@host[:port]/database
//! with optional query parameters (`sslmode`, `schema`, `password`).
//! The password may also come from the PGPASSWORD environment variable.

use crate::{verr, Result};

#[derive(Debug, Clone)]
pub struct PgUrl {
    pub user: String,
    password: Option<String>,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub schema: String,
}

fn percent_decode(s: &str) -> Result<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
                .ok_or_else(|| verr(format!("bad percent-escape in URL near {s:?}")))?;
            out.push(hex);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| verr("URL percent-escape decodes to invalid UTF-8"))
}

impl PgUrl {
    pub fn is_pg_url(s: &str) -> bool {
        s.starts_with("postgres://") || s.starts_with("postgresql://")
    }

    pub fn parse(url: &str) -> Result<PgUrl> {
        let rest = url
            .strip_prefix("postgres://")
            .or_else(|| url.strip_prefix("postgresql://"))
            .ok_or_else(|| verr(format!("not a postgres URL: {url:?}")))?;

        let (rest, query) = match rest.split_once('?') {
            Some((r, q)) => (r, Some(q)),
            None => (rest, None),
        };
        let (userinfo, hostpart) = match rest.rsplit_once('@') {
            Some((u, h)) => (Some(u), h),
            None => (None, rest),
        };
        let (hostport, database) = match hostpart.split_once('/') {
            Some((hp, db)) if !db.is_empty() => (hp, percent_decode(db)?),
            _ => return Err(verr(format!("postgres URL must include a database: {url:?}"))),
        };
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) => (
                h.to_string(),
                p.parse::<u16>().map_err(|_| verr(format!("bad port in URL: {url:?}")))?,
            ),
            None => (hostport.to_string(), 5432),
        };
        if host.is_empty() {
            return Err(verr(format!("postgres URL missing host: {url:?}")));
        }

        let (user, mut password) = match userinfo {
            None => ("postgres".to_string(), None),
            Some(ui) => match ui.split_once(':') {
                Some((u, p)) => (percent_decode(u)?, Some(percent_decode(p)?)),
                None => (percent_decode(ui)?, None),
            },
        };

        let mut schema = "public".to_string();
        if let Some(q) = query {
            for pair in q.split('&').filter(|p| !p.is_empty()) {
                let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                let v = percent_decode(v)?;
                match k {
                    "sslmode" => {
                        if v != "disable" && v != "prefer" && v != "allow" {
                            return Err(verr(format!(
                                "sslmode={v} requires TLS, which altonic does not implement; use sslmode=disable against servers that accept plain TCP"
                            )));
                        }
                    }
                    "schema" => schema = v,
                    "password" => password = Some(v),
                    "connect_timeout" | "application_name" => {}
                    other => return Err(verr(format!("unsupported URL parameter {other:?}"))),
                }
            }
        }
        if password.is_none() {
            password = std::env::var("PGPASSWORD").ok().filter(|p| !p.is_empty());
        }

        Ok(PgUrl { user, password, host, port, database, schema })
    }

    pub fn password(&self) -> Result<String> {
        self.password.clone().ok_or_else(|| {
            verr("postgres server requires a password; put it in the URL (postgres://user:pass@...) or set PGPASSWORD")
        })
    }

    /// Redacted form for logs and reports.
    pub fn redacted(&self) -> String {
        format!("postgres://{}@{}:{}/{}", self.user, self.host, self.port, self.database)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_url() {
        let u = PgUrl::parse("postgres://alice:s%40crit@db.example:6432/prod?schema=app&sslmode=disable")
            .unwrap();
        assert_eq!(u.user, "alice");
        assert_eq!(u.password().unwrap(), "s@crit");
        assert_eq!(u.host, "db.example");
        assert_eq!(u.port, 6432);
        assert_eq!(u.database, "prod");
        assert_eq!(u.schema, "app");
        assert!(!u.redacted().contains("crit"));
    }

    #[test]
    fn defaults() {
        let u = PgUrl::parse("postgresql://localhost/mydb").unwrap();
        assert_eq!(u.user, "postgres");
        assert_eq!(u.port, 5432);
        assert_eq!(u.schema, "public");
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
