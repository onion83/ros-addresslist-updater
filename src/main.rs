mod cache;
mod config;
mod router;

use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router as HttpRouter,
    extract::{ConnectInfo, State},
    http::StatusCode,
    routing::post,
};
use clap::{Parser, Subcommand};
use config::{Config, Kind, Task};
use prost::Message;
use reqwest::Client;
use ros_addresslist_updater::{GeoIpList, Network, extract_entry, rules};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{sync::Mutex, task::JoinSet};

#[derive(Parser)]
#[command(version, about = "Synchronize RouterOS address lists")]
struct Cli {
    #[arg(
        short,
        long,
        global = true,
        help = "Config file (defaults to conf/config.yaml or config.yaml in conf)"
    )]
    config: Option<PathBuf>,
    /// Fetch fresh source files and continue processing even if the GeoIP DAT is unchanged.
    #[arg(long, global = true)]
    nocache: bool,
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Validate configuration without making requests or writing files.
    Check,
    /// Initialize enabled address lists by importing RSC files.
    Init,
    /// Run scheduled tasks and the Live API.
    Run,
    /// Run a named task, '*' for all, or '*geoip' to trigger the GeoIP updater.
    Task {
        selector: String,
        /// Clear each selected RouterOS list (including dynamic entries) and import it in full.
        #[arg(long)]
        init: bool,
    },
}

struct App {
    c: Config,
    base: PathBuf,
    work: PathBuf,
    nocache: bool,
    client: Client,
    static_jobs: Mutex<()>,
    live_write: Mutex<()>,
}

impl App {
    fn nodes<'a>(
        &'a self,
        opts: &config::Effective<'_>,
    ) -> impl Iterator<Item = &'a config::Endpoint> {
        let selected = opts.endpoints.cloned();
        self.c
            .ros_endpoints
            .iter()
            .filter(move |e| e.enable && selected.as_ref().is_none_or(|x| x.contains(&e.name)))
    }

    fn router<'a>(&'a self, endpoint: &'a config::Endpoint) -> router::Router<'a> {
        router::Router {
            endpoint,
            client: &self.client,
            base: &self.base,
            work: &self.work,
        }
    }

    fn sources<'a>(&'a self, group: &'a str) -> cache::Sources<'a> {
        cache::Sources {
            client: &self.client,
            work: &self.work,
            base: &self.base,
            group,
            nocache: self.nocache,
            etag: self.c.global_settings.use_etag,
        }
    }

    async fn geo(&self) -> Result<(GeoIpList, String)> {
        let geo = self
            .c
            .geoip_updater
            .as_ref()
            .context("GeoIP updater is not configured")?;
        let opts = self.c.effective(&geo.options)?;
        let result = tokio::time::timeout(
            opts.timeout,
            self.sources("_geoip").fetch(&geo.source, &opts, |bytes| {
                let data = GeoIpList::decode(bytes)?;
                ensure!(!data.entry.is_empty(), "empty GeoIP database");
                Ok(data)
            }),
        )
        .await
        .context("GeoIP source check timed out")??;
        Ok(result)
    }

    fn networks(&self, bytes: &[u8], bits: u8, strict: bool) -> Result<Vec<Network>> {
        let mut out = Vec::new();
        for (line, value) in std::str::from_utf8(bytes)?.lines().enumerate() {
            let text = self.strip_comment(value).trim();
            if text.is_empty() {
                continue;
            }
            let rule = rules::parse_rule(text)
                .with_context(|| format!("invalid CIDR on line {}", line + 1))?;
            ensure!(
                !strict || rule.bits == bits,
                "wrong address family on line {}",
                line + 1
            );
            if rule.bits == bits {
                out.push(rule.network);
            }
        }
        Ok(out)
    }

    fn strip_comment<'a>(&self, value: &'a str) -> &'a str {
        value
            .split_once(&self.c.global_settings.inline_comment_prefix)
            .map_or(value, |(address, _)| address)
    }

    fn read_rules(
        &self,
        inline: &[String],
        file: Option<&Path>,
        bits: u8,
    ) -> Result<(Vec<rules::Rule>, usize)> {
        let values = file
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| {
                let path = self.base.join(p);
                fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))
            })
            .transpose()?;
        let mut out = Vec::new();
        let mut original = 0;
        for line in inline
            .iter()
            .map(String::as_str)
            .chain(values.as_deref().unwrap_or("").lines())
        {
            let prefix = &self.c.global_settings.inline_comment_prefix;
            let parsed = if prefix == "#" {
                rules::parse_rule(line)
            } else if let Some((address, note)) = line.split_once(prefix) {
                rules::parse_rule(&format!("{address} #{note}"))
            } else {
                rules::parse_rule(line)
            };
            if let Ok(rule) = parsed
                && rule.bits == bits
            {
                original += 1;
                rules::insert_rule(&mut out, rule);
            }
        }
        Ok((out, original))
    }

    async fn prepare(
        &self,
        t: &Task,
        geo: Option<&GeoIpList>,
        init: bool,
    ) -> Result<(BTreeMap<Network, Option<String>>, PathBuf, Vec<u8>)> {
        let opts = self.c.effective(&t.options)?;
        let bits = if t.ip_version == 4 { 32 } else { 128 };
        let (networks, counts) = match t.kind {
            Kind::Txt => {
                let networks = self
                    .sources(&t.name)
                    .fetch(&t.source, &opts, |data| {
                        let networks = self.networks(data, bits, true)?;
                        ensure!(!networks.is_empty(), "TXT contains no usable addresses");
                        Ok(networks)
                    })
                    .await?
                    .0;
                let counts = format!("original: {}", networks.len());
                (networks, counts)
            }
            Kind::Geoip => {
                let geo = geo.context("missing GeoIP snapshot")?;
                let mut nets = Vec::new();
                let mut counts = Vec::new();
                for tag in &t.tags {
                    let (v4, v6) = extract_entry(geo, tag)?;
                    let networks = if bits == 32 { v4 } else { v6 };
                    counts.push(format!("{tag}:{}", networks.len()));
                    nets.extend(networks);
                }
                (nets, counts.join(" "))
            }
        };
        let (mut white, original_white) =
            self.read_rules(&t.whitelist, t.whitelist_file.as_deref(), bits)?;
        let (mut black, original_black) =
            self.read_rules(&t.blacklist, t.blacklist_file.as_deref(), bits)?;
        let rule_duplicates = original_white - white.len() + (original_black - black.len());
        for conflict in rules::cancel_conflicts(&mut white, &mut black) {
            tracing::warn!(task=%t.name,%conflict,"conflicting rules canceled");
        }
        let applied = rules::apply(networks, vec![], &white, &black, bits, opts.optimize);
        let desired: BTreeMap<_, _> = applied
            .networks
            .into_iter()
            .zip(applied.custom.into_iter().zip(applied.comments))
            .map(|(network, (custom, comment))| (network, custom.then_some(comment)))
            .collect();
        let final_count = desired.len();
        let duplicates = rule_duplicates + applied.duplicates;
        tracing::debug!(
            task = %t.name,
            ip_version = t.ip_version,
            "record counts: {counts} white-list: {original_white} black-list: {original_black} duplicates: {duplicates} optimize: {} final: {final_count} white-added: {} black-removed: {}",
            applied.optimized,
            applied.added,
            applied.removed
        );
        ensure!(
            (opts.min..=opts.max).contains(&desired.len()),
            "record count {} outside {}..={}",
            desired.len(),
            opts.min,
            opts.max
        );
        let key = cache::hash(format!("{}:{}", t.ip_version, t.name).as_bytes());
        let tasks = self.work.join("tasks");
        let path = tasks
            .join(cache::component(&t.name))
            .join(format!("ipv{}.txt", t.ip_version));
        cache::migrate_file(&tasks.join(format!("{key}.txt")), &path)?;
        if !init {
            match fs::read(&path) {
                Ok(old) => {
                    let prior: BTreeSet<_> = self.networks(&old, bits, true)?.into_iter().collect();
                    if !prior.is_empty() {
                        let current: BTreeSet<_> = desired.keys().copied().collect();
                        let added = current.difference(&prior).count();
                        let removed = prior.difference(&current).count();
                        let percent = (added as u128 + removed as u128) * 100;
                        if percent > opts.threshold.saturating_mul(prior.len() as u128) {
                            tracing::warn!(task=%t.name, old=prior.len(),new=desired.len(),added,removed,ratio=percent as f64/prior.len() as f64,threshold=opts.threshold,baseline=%cache::hash(&old),"record change threshold exceeded");
                            anyhow::bail!("record change threshold exceeded");
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        let mut data = String::new();
        for n in desired.keys() {
            data.push_str(&n.text(bits));
            data.push('\n');
        }
        Ok((desired, path, data.into_bytes()))
    }

    async fn apply(
        &self,
        t: &Task,
        desired: &BTreeMap<Network, Option<String>>,
        init: bool,
    ) -> Result<()> {
        let opts = self.c.effective(&t.options)?;
        let prefix = &self.c.global_settings.ros_comment_prefix;
        let mut failed = false;
        let mut any = false;
        for node in self.nodes(&opts) {
            any = true;
            match self
                .router(node)
                .sync(t, desired, prefix, opts.diff, opts.rsc, init)
                .await
            {
                Ok(()) => tracing::info!(task=%t.name,node=%node.name,"node synchronized"),
                Err(e) => {
                    failed = true;
                    tracing::error!(task=%t.name,node=%node.name,error=%e,"node sync failed; next run will read actual state");
                }
            }
        }
        if !any {
            tracing::warn!(task=%t.name,"no enabled target nodes");
        }
        ensure!(!failed, "one or more nodes failed");
        Ok(())
    }

    async fn execute(&self, t: &Task, geo: Option<&GeoIpList>) -> Result<()> {
        let opts = self.c.effective(&t.options)?;
        let start = std::time::Instant::now();
        let result = tokio::time::timeout(opts.timeout, async {
            let (desired, path, data) = self.prepare(t, geo, false).await?;
            cache::write_atomic(&path, &data)?;
            self.apply(t, &desired, false).await
        })
        .await
        .context("task timed out; node state may be unknown")
        .and_then(|r| r);
        tracing::info!(task=%t.name,elapsed_ms=start.elapsed().as_millis(),success=result.is_ok(),"task completed");
        result
    }

    async fn batch(&self, tasks: Vec<&Task>, init: bool, trigger: bool) -> Result<()> {
        let _guard = self.static_jobs.lock().await;
        let needs_geo = trigger || tasks.iter().any(|t| t.kind == Kind::Geoip);
        let geo = self.work.join("geoip-applied");
        let (data, digest, mut failed) = if needs_geo {
            match self.geo().await {
                Ok((data, digest)) => (Some(data), Some(digest), false),
                Err(e) if !init => {
                    tracing::error!(error=%e,"GeoIP source check failed");
                    (None, None, true)
                }
                Err(e) => return Err(e),
            }
        } else {
            (None, None, false)
        };
        if trigger
            && !init
            && !self.nocache
            && digest
                .as_deref()
                .is_some_and(|d| fs::read_to_string(&geo).is_ok_and(|old| old == d))
        {
            tracing::info!("GeoIP snapshot already processed");
            return Ok(());
        }
        let all_geo = self
            .c
            .task_lists
            .iter()
            .filter(|t| t.enable && t.kind == Kind::Geoip)
            .count();
        let covers_geo = tasks.iter().filter(|t| t.kind == Kind::Geoip).count() == all_geo;
        let mut geo_failed = false;
        if init {
            let mut prepared = Vec::new();
            for t in tasks {
                let opts = self.c.effective(&t.options)?;
                let start = std::time::Instant::now();
                let entry =
                    tokio::time::timeout(opts.timeout, self.prepare(t, data.as_ref(), true))
                        .await
                        .context("init preparation timed out")??;
                prepared.push((t, entry, opts.timeout.saturating_sub(start.elapsed())));
            }
            for (t, (set, path, bytes), remaining) in prepared {
                let outcome = tokio::time::timeout(remaining, self.apply(t, &set, true))
                    .await
                    .context("init apply timed out; node state may be unknown")
                    .and_then(|r| r);
                match outcome {
                    Ok(()) => cache::write_atomic(&path, &bytes)?,
                    Err(e) => {
                        failed = true;
                        geo_failed |= t.kind == Kind::Geoip;
                        tracing::error!(task=%t.name,error=%e,"initialization failed");
                    }
                }
            }
        } else {
            for t in tasks {
                if t.kind == Kind::Geoip && data.is_none() {
                    continue;
                }
                if let Err(e) = self.execute(t, data.as_ref()).await {
                    failed = true;
                    geo_failed |= t.kind == Kind::Geoip;
                    tracing::error!(task=%t.name,error=%e,"task failed");
                }
            }
        }
        if covers_geo
            && !geo_failed
            && let Some(digest) = digest
        {
            cache::write_atomic(&geo, digest.as_bytes())?;
        }
        ensure!(!failed, "one or more tasks failed");
        Ok(())
    }

    fn enabled(&self) -> impl Iterator<Item = &Task> {
        self.c.task_lists.iter().filter(|t| t.enable)
    }

    async fn run(self: Arc<Self>) -> Result<()> {
        let mut jobs: JoinSet<Result<()>> = JoinSet::new();
        if let Some(api) = self.c.api.as_ref().filter(|api| api.enable) {
            let addr = SocketAddr::new(api.listen.parse()?, api.port);
            let listener = tokio::net::TcpListener::bind(addr).await?;
            let app = HttpRouter::new()
                .route(&api.path, post(live))
                .with_state(self.clone());
            jobs.spawn(async move {
                axum::serve(
                    listener,
                    app.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .await?;
                Ok(())
            });
        }
        let start = self.clone();
        jobs.spawn(async move {
            let tasks = start
                .enabled()
                .filter(|t| start.c.effective(&t.options).is_ok_and(|o| o.startup))
                .collect();
            if let Err(e) = start.batch(tasks, false, false).await {
                tracing::error!(error=%e,"startup checks incomplete");
            }
            Ok(())
        });
        for t in self.enabled().filter(|t| t.kind == Kind::Txt) {
            let cron = config::cron(self.c.effective(&t.options)?.scheduler)?;
            let name = t.name.clone();
            let app = self.clone();
            jobs.spawn(async move {
                let t = app
                    .enabled()
                    .find(|t| t.name == name)
                    .context("missing scheduled task")?;
                loop {
                    sleep_until(&cron).await?;
                    if let Err(e) = app.batch(vec![t], false, false).await {
                        tracing::error!(task=%name,error=%e,"scheduled task failed");
                    }
                }
                #[allow(unreachable_code)]
                Ok(())
            });
        }
        if let Some(geo) = &self.c.geoip_updater {
            let cron = config::cron(&geo.scheduler)?;
            let app = self.clone();
            jobs.spawn(async move {
                loop {
                    sleep_until(&cron).await?;
                    if let Err(e) = app
                        .batch(
                            app.enabled().filter(|t| t.kind == Kind::Geoip).collect(),
                            false,
                            true,
                        )
                        .await
                    {
                        tracing::error!(error=%e,"scheduled GeoIP check failed");
                    }
                }
                #[allow(unreachable_code)]
                Ok(())
            });
        }
        tracing::info!(timezone=%chrono::Local::now().offset(),"scheduler running");
        loop {
            tokio::select! {
                signal=tokio::signal::ctrl_c()=>{signal?; return Ok(());}
                job=jobs.join_next(), if !jobs.is_empty()=>{if let Some(job)=job {job??;}}
            }
        }
    }
}

async fn sleep_until(cron: &croner::Cron) -> Result<()> {
    let now = chrono::Local::now();
    let next = cron.find_next_occurrence(&now, false)?;
    tokio::time::sleep((next - now).to_std()?).await;
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Injection {
    list: String,
    address: IpAddr,
    ttl: Option<u64>,
}

async fn live(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(input): Json<Injection>,
) -> (StatusCode, Json<Value>) {
    let Some(api) = &app.c.api else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error":"Live API unavailable"})),
        );
    };
    let ip = match peer.ip() {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(peer.ip(), IpAddr::V4),
        ip => ip,
    };
    if !api.network_acl.iter().any(|net| net.contains(&ip))
        || !api.address_list_allow.contains(&input.list)
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error":"network or list not allowed"})),
        );
    }
    let Some(task) = app.enabled().find(|t| t.name == input.list) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error":"list not enabled"})),
        );
    };
    let ttl = input.ttl.unwrap_or(api.default_ttl);
    if ttl == 0 || input.address.is_ipv4() != (task.ip_version == 4) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"invalid TTL or address family"})),
        );
    }
    let Ok(opts) = app.c.effective(&task.options) else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"invalid task settings"})),
        );
    };
    let mut results = Vec::new();
    let mut failures = 0;
    let mark = router::live_comment(&app.c.global_settings.ros_comment_prefix, &task.name);
    let _guard = app.live_write.lock().await;
    for node in app.nodes(&opts) {
        let result = app.router(node).live(task, input.address, ttl, &mark).await;
        tracing::info!(task=%task.name,node=%node.name,address=%input.address,ttl,success=result.is_ok(),"Live request forwarded");
        if let Err(e) = result {
            failures += 1;
            tracing::warn!(node=%node.name,error=%e,"Live forwarding failed");
            results.push(json!({"node":node.name,"ok":false,"error":e.to_string()}));
        } else {
            results.push(json!({"node":node.name,"ok":true}));
        }
    }
    let status = if results.is_empty() {
        StatusCode::SERVICE_UNAVAILABLE
    } else if failures == results.len() {
        StatusCode::BAD_GATEWAY
    } else if failures > 0 {
        StatusCode::MULTI_STATUS
    } else {
        StatusCode::OK
    };
    (status, Json(json!({"results":results})))
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = cli.config.unwrap_or_else(|| {
        [
            "conf/config.yaml",
            "config.yaml",
            "conf/conf.yaml",
            "conf.yaml",
            "conf/mind.yaml",
            "mind.yaml",
        ]
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("conf/config.yaml"))
    });
    let file = config
        .canonicalize()
        .with_context(|| format!("configuration {}", config.display()))?;
    let base = file
        .parent()
        .context("configuration has no parent")?
        .to_path_buf();
    let c: Config =
        serde_yaml::from_str(&fs::read_to_string(&file)?).context("invalid YAML configuration")?;
    c.validate()?;
    if matches!(cli.command, Action::Check) {
        println!("Configuration OK: {}", file.display());
        return Ok(());
    }
    let full_init = match &cli.command {
        Action::Init => true,
        Action::Task {
            selector,
            init: true,
        } => selector == "*",
        _ => false,
    };
    ensure!(
        !full_init || c.global_settings.first_run,
        "init and task '*' --init require first-run: yes"
    );
    let work = base.join(&c.global_settings.work_dir);
    let logs = base.join(&c.global_settings.log_dir);
    fs::create_dir_all(&work)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(work.join(".lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock).context("another process is using work-dir")?;
    fs::create_dir_all(&logs)?;
    let writer = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("updater")
        .filename_suffix("log")
        .build(logs)?;
    let (writer, _log_guard) = tracing_appender::non_blocking(writer);
    use tracing_subscriber::{fmt::writer::MakeWriterExt, prelude::*};
    let level = c.global_settings.log_level.parse::<tracing::Level>()?;
    let targets =
        tracing_subscriber::filter::Targets::new().with_target("ros_addresslist_updater", level);
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(writer.and(std::io::stderr))
                .with_filter(targets),
        )
        .init();
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()?;
    let app = Arc::new(App {
        c,
        base,
        work,
        nocache: cli.nocache,
        client,
        static_jobs: Mutex::new(()),
        live_write: Mutex::new(()),
    });
    match cli.command {
        Action::Check => unreachable!(),
        Action::Run => app.run().await,
        Action::Init => app.batch(app.enabled().collect(), true, false).await,
        Action::Task { selector, init } => match selector.as_str() {
            "*" => app.batch(app.enabled().collect(), init, false).await,
            "*geoip" => {
                app.batch(
                    app.enabled().filter(|t| t.kind == Kind::Geoip).collect(),
                    init,
                    true,
                )
                .await
            }
            name => {
                let task = app
                    .enabled()
                    .find(|t| t.name == name)
                    .with_context(|| format!("task {name:?} is unknown or disabled"))?;
                app.batch(vec![task], init, false).await
            }
        },
    }
}
