use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Deserializer};
use std::{
    collections::{BTreeMap, HashSet},
    path::PathBuf,
    time::Duration,
};

fn flag<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    match String::deserialize(d)?.as_str() {
        "yes" | "true" => Ok(true),
        "no" | "false" => Ok(false),
        _ => Err(serde::de::Error::custom("expected yes or no")),
    }
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "kebab-case")]
pub struct Options {
    pub scheduler: Option<String>,
    pub download_timeout: Option<String>,
    pub download_retry: Option<usize>,
    pub download_order: Option<String>,
    pub check_onstart: Option<String>,
    pub task_timeout: Option<String>,
    pub optimize_cidr: Option<String>,
    pub diff_update: Option<String>,
    pub update_method: Option<String>,
    pub min_record_size: Option<usize>,
    pub max_record_size: Option<usize>,
    pub record_diff_threshold: Option<String>,
    pub apply_endpoints: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Global {
    #[serde(deserialize_with = "flag")]
    pub first_run: bool,
    pub work_dir: PathBuf,
    pub log_dir: PathBuf,
    pub log_level: String,
    pub default_scheduler: String,
    pub ros_comment_prefix: String,
    pub inline_comment_prefix: String,
    #[serde(deserialize_with = "flag")]
    pub use_etag: bool,
    pub diff_algo: String,
    #[serde(rename = "ip-type")]
    pub _ip_type: Option<String>,
    #[serde(flatten)]
    pub options: Options,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_yaml::Value>,
}
#[derive(Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Endpoint {
    pub name: String,
    pub address: String,
    pub port: u16,
    #[serde(deserialize_with = "flag")]
    pub enable: bool,
    #[serde(default = "yes", deserialize_with = "flag")]
    pub ssl: bool,
    pub username: String,
    pub password: String,
    pub ssh_user: Option<String>,
    pub ssh_port: Option<u16>,
    pub ssh_key: Option<PathBuf>,
}
fn yes() -> bool {
    true
}

#[derive(Deserialize)]
pub struct Geo {
    pub source: Vec<String>,
    pub scheduler: String,
    #[serde(flatten)]
    pub options: Options,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_yaml::Value>,
}
#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Txt,
    Geoip,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Task {
    pub name: String,
    #[serde(deserialize_with = "flag")]
    pub enable: bool,
    #[serde(rename = "type")]
    pub kind: Kind,
    pub ip_version: u8,
    #[serde(default)]
    pub source: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub whitelist: Vec<String>,
    #[serde(default)]
    pub blacklist: Vec<String>,
    pub whitelist_file: Option<PathBuf>,
    pub blacklist_file: Option<PathBuf>,
    #[serde(flatten)]
    pub options: Options,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Live {
    #[serde(deserialize_with = "flag")]
    pub enable: bool,
    pub listen: String,
    pub port: u16,
    pub path: String,
    pub network_acl: Vec<ipnet::IpNet>,
    pub address_list_allow: Vec<String>,
    pub default_ttl: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Config {
    pub global_settings: Global,
    pub ros_endpoints: Vec<Endpoint>,
    pub geoip_updater: Option<Geo>,
    pub task_lists: Vec<Task>,
    pub api: Option<Live>,
}

pub struct Effective<'a> {
    pub scheduler: &'a str,
    pub download_timeout: Duration,
    pub download_retry: usize,
    pub random: bool,
    pub startup: bool,
    pub timeout: Duration,
    pub optimize: bool,
    pub diff: bool,
    pub rsc: bool,
    pub min: usize,
    pub max: usize,
    pub threshold: u128,
    pub endpoints: Option<&'a Vec<String>>,
}

pub fn cron(value: &str) -> Result<croner::Cron> {
    ensure!(
        value.split_whitespace().count() == 6,
        "cron requires six fields"
    );
    Ok(value.parse()?)
}

impl Config {
    pub fn effective<'a>(&'a self, opts: &'a Options) -> Result<Effective<'a>> {
        let g = &self.global_settings;
        macro_rules! get {
            ($field:ident) => {
                opts.$field.as_ref().or(g.options.$field.as_ref())
            };
            ($field:ident, $default:expr) => {
                get!($field).map(String::as_str).unwrap_or($default)
            };
        }
        let toggle = |s: &str| -> Result<bool> {
            match s {
                "yes" | "true" => Ok(true),
                "no" | "false" => Ok(false),
                _ => anyhow::bail!("invalid toggle {s}"),
            }
        };
        let method = get!(update_method, "api");
        ensure!(matches!(method, "api" | "rsc"), "invalid update method");
        let order = get!(download_order, "sequential");
        ensure!(
            matches!(order, "sequential" | "sequ" | "random" | "rand"),
            "invalid download order"
        );
        let threshold: u128 = get!(record_diff_threshold, "50%")
            .strip_suffix('%')
            .context("threshold requires %")?
            .parse()?;
        let min = *get!(min_record_size).unwrap_or(&10);
        let max = *get!(max_record_size).unwrap_or(&10000);
        ensure!(min > 0 && min <= max, "invalid record bounds");
        let scheduler = opts.scheduler.as_deref().unwrap_or(&g.default_scheduler);
        cron(scheduler)?;
        let endpoints = get!(apply_endpoints);
        if let Some(names) = endpoints {
            for name in names {
                ensure!(
                    self.ros_endpoints.iter().any(|e| e.name == *name),
                    "unknown node {name}"
                );
            }
        }
        let download_timeout = humantime::parse_duration(get!(download_timeout, "30s"))?;
        let timeout = humantime::parse_duration(get!(task_timeout, "60s"))?;
        ensure!(
            !download_timeout.is_zero() && !timeout.is_zero(),
            "timeouts must be positive"
        );
        Ok(Effective {
            scheduler,
            download_timeout,
            download_retry: *get!(download_retry).unwrap_or(&3),
            random: matches!(order, "random" | "rand"),
            startup: toggle(get!(check_onstart, "yes"))?,
            timeout,
            optimize: toggle(get!(optimize_cidr, "no"))?,
            diff: toggle(get!(diff_update, "yes"))?,
            rsc: method == "rsc",
            min,
            max,
            threshold,
            endpoints,
        })
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.global_settings.diff_algo == "blake3",
            "diff-algo must be blake3"
        );
        ensure!(
            !self.global_settings.inline_comment_prefix.is_empty()
                && !self.global_settings.ros_comment_prefix.is_empty(),
            "comment prefixes must be nonempty"
        );
        let _: tracing::Level = self.global_settings.log_level.parse()?;
        reject_unknown("global-settings", &self.global_settings.extra)?;
        let mut nodes = HashSet::new();
        for e in &self.ros_endpoints {
            ensure!(
                nodes.insert(e.name.to_ascii_lowercase()),
                "duplicate node name (case insensitive): {}",
                e.name
            );
            ensure!(
                crate::cache::component(&e.name).len() <= 255,
                "node name too long"
            );
            ensure!(
                !e.address.is_empty()
                    && !e.address.starts_with('-')
                    && !e.address.contains(['/', '@'])
                    && e.port != 0,
                "invalid endpoint {}",
                e.name
            );
        }
        self.effective(&Options::default())?;
        if let Some(geo) = &self.geoip_updater {
            reject_unknown("geoip-updater", &geo.extra)?;
            ensure!(!geo.source.is_empty(), "GeoIP requires a source");
            cron(&geo.scheduler)?;
            self.effective(&geo.options)?;
        }
        let mut tasks = HashSet::new();
        for t in &self.task_lists {
            reject_unknown(&format!("task {}", t.name), &t.extra)?;
            ensure!(
                t.name.starts_with(|c: char| c.is_ascii_alphanumeric())
                    && !t.name.contains('*')
                    && !t.name.chars().any(char::is_control),
                "invalid task name {:?}",
                t.name
            );
            ensure!(
                tasks.insert(t.name.to_ascii_lowercase()),
                "duplicate task name (case insensitive): {}",
                t.name
            );
            ensure!(
                crate::cache::component(&t.name).len() <= 255,
                "task name too long"
            );
            ensure!(
                matches!(t.ip_version, 4 | 6),
                "invalid IP version in {}",
                t.name
            );
            match t.kind {
                Kind::Txt => ensure!(!t.source.is_empty(), "missing source in {}", t.name),
                Kind::Geoip => ensure!(
                    !t.tags.is_empty() && self.geoip_updater.is_some(),
                    "missing GeoIP configuration for {}",
                    t.name
                ),
            }
            self.effective(&t.options)
                .with_context(|| format!("task {}", t.name))?;
        }
        if let Some(api) = &self.api {
            let _: std::net::IpAddr = api.listen.parse()?;
            ensure!(
                api.path.starts_with('/')
                    && !api.path.contains(['{', '}', '*'])
                    && api.default_ttl > 0,
                "invalid live path or TTL"
            );
            for name in &api.address_list_allow {
                ensure!(
                    self.task_lists.iter().any(|task| task.name == *name),
                    "unknown live list {name}"
                );
            }
        }
        Ok(())
    }
}

fn reject_unknown(scope: &str, fields: &BTreeMap<String, serde_yaml::Value>) -> Result<()> {
    if let Some(key) = fields.keys().next() {
        anyhow::bail!("{scope}: unknown field {key:?}");
    }
    Ok(())
}
