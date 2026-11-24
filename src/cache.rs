use anyhow::{Context, Result, ensure};
use rand::seq::SliceRandom;
use reqwest::{Client, StatusCode, header};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use crate::config::Effective;

pub fn hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path.parent().context("file has no parent")?;
    fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(data)?;
    temp.persist(path)?;
    Ok(())
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-') {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

pub fn component(name: &str) -> String {
    match name {
        "" => "%00".into(),
        "." => "%2E".into(),
        ".." => "%2E%2E".into(),
        _ => encode(name).replace("%5F", "_"),
    }
}

pub fn migrate_file(old: &Path, new: &Path) -> Result<()> {
    if !new.try_exists()? && old.try_exists()? {
        let parent = new.parent().context("migration path has no parent")?;
        fs::create_dir_all(parent)?;
        fs::rename(old, new)?;
    }
    Ok(())
}

fn decode(s: &str) -> Option<String> {
    let mut bytes = s.bytes();
    let mut data = Vec::new();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let digits = [bytes.next()?, bytes.next()?];
            data.push(u8::from_str_radix(std::str::from_utf8(&digits).ok()?, 16).ok()?);
        } else {
            data.push(b);
        }
    }
    String::from_utf8(data).ok()
}

pub struct Sources<'a> {
    pub client: &'a Client,
    pub work: &'a Path,
    pub base: &'a Path,
    pub group: &'a str,
    pub nocache: bool,
    pub etag: bool,
}

impl Sources<'_> {
    pub async fn fetch<T>(
        &self,
        sources: &[String],
        opts: &Effective<'_>,
        validate: impl Fn(&[u8]) -> Result<T>,
    ) -> Result<(T, String)> {
        let mut order: Vec<_> = sources.iter().collect();
        if opts.random {
            order.shuffle(&mut rand::rng());
        }
        let mut error = None;
        for (i, source) in order.into_iter().enumerate() {
            let attempts = if remote(source) {
                opts.download_retry
            } else {
                0
            };
            for attempt in 0..=attempts {
                match self.fetch_one(source, opts, &validate).await {
                    Ok(value) => return Ok(value),
                    Err(e) => {
                        tracing::warn!(candidate=i+1, attempt=attempt+1, error=%e,"source check failed");
                        error = Some(e);
                    }
                }
            }
        }
        Err(error.unwrap_or_else(|| anyhow::anyhow!("no sources available")))
    }

    async fn fetch_one<T>(
        &self,
        source: &str,
        opts: &Effective<'_>,
        validate: &impl Fn(&[u8]) -> Result<T>,
    ) -> Result<(T, String)> {
        let url = remote(source)
            .then(|| reqwest::Url::parse(source))
            .transpose()
            .map_err(|_| anyhow::anyhow!("invalid source URL"))?;
        let local = if url.is_none() {
            Some(self.base.join(source).canonicalize()?)
        } else {
            None
        };
        let key = local
            .as_ref()
            .map_or_else(|| source.into(), |p| p.to_string_lossy());
        let root = self.work.join("sources");
        let source_key = hash(key.as_bytes());
        let host = url
            .as_ref()
            .and_then(reqwest::Url::host_str)
            .unwrap_or("local");
        let name = if let Some(url) = &url {
            url.path_segments()
                .and_then(Iterator::last)
                .filter(|name| !name.is_empty())
                .unwrap_or("index")
        } else {
            local
                .as_ref()
                .and_then(|p| p.file_name())
                .and_then(|name| name.to_str())
                .unwrap_or("index")
        };
        let label = component(if url.is_some() { host } else { name });
        // Avoid oversized components without cutting a UTF-8 code point (encoded ASCII).
        let label = &label[..label.len().min(96)];
        let dir = root
            .join(component(self.group))
            .join(format!("{label}-{}", &source_key[..16]));
        fs::create_dir_all(&dir)?;
        let current = if self.nocache {
            tracing::debug!(group = self.group, "bypassing source cache");
            None
        } else if let Some(current) = newest(&dir)? {
            Some(current)
        } else {
            // Old snapshots were shared globally, so other tasks may still need this file.
            let legacy = root.join(&source_key);
            if legacy.try_exists()? {
                if let Some(old) = newest(&legacy)? {
                    let dest = dir.join(old.file_name().context("invalid old snapshot name")?);
                    write_atomic(&dest, &fs::read(old)?)?;
                    Some(dest)
                } else {
                    None
                }
            } else {
                None
            }
        };
        let existing = current.as_ref().and_then(|p| {
            let file = p.file_name()?.to_str()?;
            let parts: Vec<_> = file.split('_').collect();
            let bytes = fs::read(p).ok()?;
            (parts.len() == 5 && hash(&bytes) == parts[3])
                .then(|| (bytes, decode(parts[2]).filter(|e| e != "none")))
        });
        let (bytes, etag) = if let Some(url) = &url {
            let mut req = self.client.get(url.clone()).timeout(opts.download_timeout);
            let old_etag = existing
                .as_ref()
                .and_then(|(_, etag)| etag.as_ref())
                .filter(|_| self.etag);
            if let Some(tag) = old_etag {
                req = req.header(header::IF_NONE_MATCH, tag);
            }
            let response = req.send().await.map_err(|e| e.without_url())?;
            if response.status() == StatusCode::NOT_MODIFIED {
                ensure!(
                    old_etag.is_some(),
                    "unexpected 304 without a conditional request"
                );
                let (bytes, _) = existing.context("304 without valid cache")?;
                let digest = hash(&bytes);
                tracing::debug!(%digest,"using cached file after 304");
                return Ok((validate(&bytes)?, digest));
            }
            ensure!(
                response.status() == StatusCode::OK,
                "source returned {}",
                response.status()
            );
            let etag = response
                .headers()
                .get(header::ETAG)
                .and_then(|v| v.to_str().ok())
                .filter(|_| self.etag)
                .map(str::to_owned);
            (
                response
                    .bytes()
                    .await
                    .map_err(|e| e.without_url())?
                    .to_vec(),
                etag,
            )
        } else {
            (
                fs::read(local.as_ref().context("missing local source")?)?,
                None,
            )
        };
        ensure!(!bytes.is_empty(), "empty source file");
        let digest = hash(&bytes);
        let parsed = validate(&bytes)?;
        if existing
            .as_ref()
            .is_some_and(|(old, tag)| hash(old) == digest && *tag == etag)
        {
            tracing::debug!(%digest,"source content and ETag are unchanged");
            return Ok((parsed, digest));
        }
        let stamp = chrono::Utc::now().format("%Y%m%d%H%M%S");
        let filename = format!(
            "{}_{}_{}_{digest}_{stamp}",
            encode(host),
            encode(name),
            encode(etag.as_deref().unwrap_or("none"))
        );
        ensure!(
            filename.len() <= 255,
            "encoded source filename exceeds 255 bytes"
        );
        let dest = dir.join(filename);
        write_atomic(&dest, &bytes)?;
        tracing::info!(%digest,"published source snapshot");
        for path in snapshots(&dir)? {
            if path != dest
                && let Err(e) = fs::remove_file(&path)
            {
                tracing::warn!(error=%e,"could not remove old source snapshot");
            }
        }
        Ok((parsed, digest))
    }
}

fn remote(source: &str) -> bool {
    source.starts_with("http://") || source.starts_with("https://")
}
fn snapshots(dir: &Path) -> Result<Vec<PathBuf>> {
    Ok(fs::read_dir(dir)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|p| {
            p.is_file()
                && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                    let parts: Vec<_> = n.split('_').collect();
                    parts.len() == 5
                        && parts[3].len() == 64
                        && parts[4].len() == 14
                        && parts[4].bytes().all(|b| b.is_ascii_digit())
                })
        })
        .collect())
}
fn newest(dir: &Path) -> Result<Option<PathBuf>> {
    Ok(snapshots(dir)?.into_iter().max_by_key(|p| {
        let name = p.file_name().unwrap_or_default().to_string_lossy();
        (
            name.rsplit('_').next().unwrap_or_default().to_owned(),
            p.metadata().and_then(|m| m.modified()).ok(),
        )
    }))
}
