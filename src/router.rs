use anyhow::{Context, Result, ensure};
use reqwest::{Client, Method};
use ros_addresslist_updater::{Network, rules::parse_rule};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::process::Command;

use crate::config::{Endpoint, Task};

#[derive(Deserialize)]
struct Row {
    #[serde(rename = ".id")]
    id: String,
    address: String,
    #[serde(default)]
    comment: String,
    #[serde(default)]
    dynamic: String,
    #[serde(default)]
    disabled: String,
}

fn route(version: u8) -> &'static str {
    if version == 4 {
        "ip/firewall/address-list"
    } else {
        "ipv6/firewall/address-list"
    }
}

pub struct Router<'a> {
    pub endpoint: &'a Endpoint,
    pub client: &'a Client,
    pub base: &'a Path,
    pub work: &'a Path,
}

impl Router<'_> {
    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<reqwest::Response> {
        let e = self.endpoint;
        let scheme = if e.ssl { "https" } else { "http" };
        let host = if e.address.contains(':') && !e.address.starts_with('[') {
            format!("[{}]", e.address)
        } else {
            e.address.clone()
        };
        let mut req = self
            .client
            .request(method, format!("{scheme}://{host}:{}/rest/{path}", e.port))
            .basic_auth(&e.username, Some(&e.password));
        if let Some(body) = body {
            req = req.json(&body);
        }
        let response = req.send().await.map_err(|e| e.without_url())?;
        ensure!(
            response.status().is_success(),
            "RouterOS REST returned {}",
            response.status()
        );
        Ok(response)
    }

    async fn rows(&self, t: &Task) -> Result<Vec<Row>> {
        let data=self.request(Method::POST,&format!("{}/print",route(t.ip_version)),Some(json!({
            ".proplist":[".id","address","comment","dynamic","disabled"],".query":[format!("list={}",t.name)]
        }))).await?;
        Ok(data.json().await?)
    }

    pub async fn live(
        &self,
        t: &Task,
        address: std::net::IpAddr,
        ttl: u64,
        mark: &str,
    ) -> Result<()> {
        self.request(Method::PUT,route(t.ip_version),Some(json!({
            "list":t.name,"address":address.to_string(),"timeout":format!("{ttl}s"),"comment":mark
        }))).await?;
        Ok(())
    }

    pub async fn sync(
        &self,
        t: &Task,
        desired: &BTreeMap<Network, Option<String>>,
        prefix: &str,
        diff: bool,
        rsc: bool,
        init: bool,
    ) -> Result<()> {
        let bits = if t.ip_version == 4 { 32 } else { 128 };
        let rows = if init {
            Vec::new()
        } else {
            self.rows(t).await?
        };
        let live_mark = live_comment(prefix, &t.name);
        let mut existing = BTreeSet::new();
        let mut delete = BTreeSet::new();
        let mut edit = BTreeMap::new();
        for row in rows.iter().filter(|row| row.static_entry(&live_mark)) {
            if diff
                && let Some(n) = row.network(bits)
                && let Some(note) = desired.get(&n)
            {
                ensure!(
                    existing.insert(n),
                    "duplicate static CIDR {} in {}",
                    n.text(bits),
                    t.name
                );
                let comment = custom_comment(note.as_deref(), prefix);
                if !row.enabled() || row.comment != comment {
                    edit.insert(row.address.clone(), comment);
                }
            } else {
                delete.insert(row.address.clone());
            }
        }
        let add: Vec<_> = desired
            .iter()
            .filter(|(n, _)| !existing.contains(n))
            .map(|(n, note)| (n.text(bits), custom_comment(note.as_deref(), prefix)))
            .collect();
        tracing::info!(task=%t.name,node=%self.endpoint.name,add=add.len(),delete=delete.len(),edit=edit.len(),clear_list=init,"RouterOS update plan");
        if !init && add.is_empty() && delete.is_empty() && edit.is_empty() {
            return Ok(());
        }

        let route = route(t.ip_version);
        let outcome = if rsc || init {
            let mut script = String::new();
            if init {
                let list = quote(&t.name);
                script.push_str(&format!(
                    ":do {{ :local entries [/{route} find where list={list}]; :if ([:len $entries] > 0) do={{ /{route} remove $entries }} }}\n"
                ));
            } else if !diff {
                for address in &delete {
                    script_action(&mut script, route, t, address, &live_mark, None);
                }
            }
            for (ip, comment) in &add {
                script.push_str(&format!(
                    "/{route} add list={} address={}",
                    quote(&t.name),
                    quote(ip)
                ));
                if !comment.is_empty() {
                    script.push_str(&format!(" comment={}", quote(comment)));
                }
                script.push('\n');
            }
            for (address, comment) in &edit {
                script_action(&mut script, route, t, address, &live_mark, Some(comment));
            }
            if diff {
                for address in &delete {
                    script_action(&mut script, route, t, address, &live_mark, None);
                }
            }
            self.import(&script).await.map(Some)
        } else {
            async {
                if !diff {
                    self.change_static(t, &live_mark, &delete, &edit).await?;
                }
                for (ip, comment) in &add {
                    let mut payload = json!({"list":t.name,"address":ip});
                    if !comment.is_empty() {
                        payload["comment"] = json!(comment);
                    }
                    self.request(Method::PUT, route, Some(payload)).await?;
                }
                if diff {
                    self.change_static(t, &live_mark, &delete, &edit).await?;
                }
                Ok(None)
            }
            .await
        };
        let after = self.rows(t).await;
        let script_name = outcome?;
        let mut seen = BTreeSet::new();
        for row in after?.iter().filter(|row| row.static_entry(&live_mark)) {
            let n = row
                .network(bits)
                .context("invalid static address after write")?;
            let expected = desired
                .get(&n)
                .context("unexpected static address after write")?;
            ensure!(
                row.enabled()
                    && seen.insert(n)
                    && row.comment == custom_comment(expected.as_deref(), prefix),
                "unexpected static entry after write"
            );
        }
        ensure!(
            seen.len() == desired.len(),
            "read-back is missing expected entries"
        );
        if let Some(name) = script_name {
            self.sftp(&format!("rm {name}\n")).await?;
        }
        Ok(())
    }

    async fn change_static(
        &self,
        t: &Task,
        live_mark: &str,
        delete: &BTreeSet<String>,
        edit: &BTreeMap<String, String>,
    ) -> Result<()> {
        if delete.is_empty() && edit.is_empty() {
            return Ok(());
        }
        let route = route(t.ip_version);
        // Resolve IDs at apply time from the current list and exact raw address.
        for row in self
            .rows(t)
            .await?
            .iter()
            .filter(|row| row.static_entry(live_mark))
        {
            let change = if delete.contains(&row.address) {
                (Method::DELETE, None)
            } else if let Some(comment) = edit.get(&row.address) {
                (
                    Method::PATCH,
                    Some(json!({"comment":comment,"disabled":"false"})),
                )
            } else {
                continue;
            };
            ensure!(valid_id(&row.id), "unexpected RouterOS entry ID");
            self.request(change.0, &format!("{route}/{}", row.id), change.1)
                .await?;
        }
        Ok(())
    }

    fn ssh_command(&self, tool: &str) -> Result<(Command, Option<tempfile::TempDir>)> {
        let e = self.endpoint;
        let user = e.ssh_user.as_ref().unwrap_or(&e.username);
        ensure!(
            !user.is_empty() && !user.starts_with('-') && !user.contains(['@', '\n', '\r']),
            "invalid SSH user"
        );
        let host = e.address.trim_matches(['[', ']']);
        let port = e.ssh_port.unwrap_or(22).to_string();
        let mut cmd = Command::new(tool);
        cmd.kill_on_drop(true).args([
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "IdentitiesOnly=yes",
        ]);
        let askpass = if let Some(key) = &e.ssh_key {
            cmd.args(["-o", "BatchMode=yes"])
                .arg("-i")
                .arg(self.base.join(key));
            None
        } else {
            use std::os::unix::fs::PermissionsExt;

            // SFTP reads its batch commands on stdin, so let OpenSSH obtain
            // the configured password through a separate askpass subprocess.
            let dir = tempfile::tempdir()?;
            let helper = dir.path().join("askpass");
            std::fs::write(
                &helper,
                "#!/bin/sh\nprintf '%s\\n' \"$ROS_UPDATER_ASKPASS_PASSWORD\"\n",
            )?;
            std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700))?;
            cmd.args([
                "-o",
                "BatchMode=no",
                "-o",
                "PreferredAuthentications=password",
                "-o",
                "PubkeyAuthentication=no",
                "-o",
                "NumberOfPasswordPrompts=1",
            ])
            .env("SSH_ASKPASS", &helper)
            .env("SSH_ASKPASS_REQUIRE", "force")
            .env("ROS_UPDATER_ASKPASS_PASSWORD", &e.password);
            Some(dir)
        };
        if tool == "sftp" {
            let host = if host.contains(':') {
                format!("[{host}]")
            } else {
                host.to_owned()
            };
            cmd.args(["-P", &port, "-b", "-", "--", &format!("{user}@{host}")]);
        } else {
            cmd.args(["-T", "-p", &port, "-l", user, "--", host]);
        }
        Ok((cmd, askpass))
    }

    async fn sftp(&self, batch: &str) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        let (mut cmd, _askpass) = self.ssh_command("sftp")?;
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .context("SFTP stdin unavailable")?
            .write_all(batch.as_bytes())
            .await?;
        let output = child.wait_with_output().await?;
        ensure!(
            output.status.success(),
            "SFTP failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(())
    }

    async fn import(&self, script: &str) -> Result<String> {
        let dir = self.work.join("rsc");
        std::fs::create_dir_all(&dir)?;
        let file = tempfile::Builder::new()
            .prefix("ros-updater-")
            .suffix(".rsc")
            .tempfile_in(&dir)?;
        std::fs::write(file.path(), script)?;
        let path: PathBuf = file.path().canonicalize()?;
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .context("invalid RSC filename")?;
        let path = path.to_str().context("invalid RSC path")?;
        ensure!(
            !path.contains(['\n', '\r', '"', '\\']),
            "unsupported work directory for SFTP"
        );
        self.sftp(&format!("put \"{path}\" {name}\n")).await?;
        let (mut cmd, _askpass) = self.ssh_command("ssh")?;
        let output = cmd
            .arg(format!("/import file-name={name} verbose=no"))
            .output()
            .await?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::debug!(%stdout,%stderr,"RouterOS import output");
        ensure!(
            output.status.success()
                && stdout.contains("Script file loaded and executed successfully")
                && stderr.trim().is_empty(),
            "RouterOS import did not confirm success: {} {}",
            stdout.trim(),
            stderr.trim()
        );
        Ok(name.to_owned())
    }
}

impl Row {
    fn static_entry(&self, live_mark: &str) -> bool {
        matches!(self.dynamic.as_str(), "false" | "no") && self.comment != live_mark
    }
    fn enabled(&self) -> bool {
        !matches!(self.disabled.as_str(), "true" | "yes")
    }
    fn network(&self, bits: u8) -> Option<Network> {
        parse_rule(&self.address)
            .ok()
            .filter(|r| r.bits == bits)
            .map(|r| r.network)
    }
}

pub fn live_comment(prefix: &str, task: &str) -> String {
    format!("{prefix} {task} {prefix} live")
}

fn custom_comment(note: Option<&str>, prefix: &str) -> String {
    match note {
        Some(note) if !note.is_empty() => format!("{prefix} {note} {prefix}"),
        _ => String::new(),
    }
}

fn valid_id(id: &str) -> bool {
    id.strip_prefix('*')
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_hexdigit()))
}
fn script_action(
    script: &mut String,
    route: &str,
    task: &Task,
    address: &str,
    live_mark: &str,
    comment: Option<&str>,
) {
    let list = quote(&task.name);
    let address = quote(address);
    let live_mark = quote(live_mark);
    let action = match comment {
        Some(note) => format!("set $entry disabled=no comment={}", quote(note)),
        None => "remove $entry".to_owned(),
    };
    script.push_str(&format!(
        ":foreach entry in=[/{route} find where list={list} and address={address} and dynamic=no and comment!={live_mark}] do={{ /{route} {action} }}\n"
    ));
}

fn quote(value: &str) -> String {
    let value = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('\n', "\\n")
        .replace('\r', "\\r");
    format!("\"{value}\"")
}
