//! OWNER: unit S-08 (cli/src/cmd/{vast,tunnel,hf,provider,recipe,profile,usage,smoke,doctor,compare,backend,swap,up,approvals,token,open,env,migrate}.rs). Do not edit outside that unit.
//!
//! `apexrouter hf search | files | get`.
//!
//! `files` prints the **authoritative** per-file sizes: the daemon reads them from
//! `paths-info`, never from `siblings` (which routinely omits them), and shards are one
//! group with a summed size — which is the number the fit solver and the disk check need,
//! and the number a human means when they ask "how big is Q4_K_XL?".
//!
//! `get` is a job: `POST /v1/hf/downloads` answers with a [`JobRecord`] and the download
//! streams progress onto `/ws`. Without `--no-wait` this polls that job to completion, so
//! the shell blocks until the weights are on disk — a 20 GB pull that returns instantly and
//! silently is not what anyone typing `hf get` wants.

use crate::cli::HfCmd;
use crate::cmd::Ctx;
use crate::daemon::Need;
use crate::render;
use apexrouter_client::NodeClient;
use apexrouter_protocol::{HfFileGroup, HfModel, JobRecord, JobState, ServedBy};
use std::time::Duration;

/// How often a waiting `hf get` re-reads its job row.
const POLL: Duration = Duration::from_millis(750);

/// Run `apexrouter hf …`.
///
/// # Errors
/// A daemon that will not answer, an HF credential problem it reports, or a download that
/// fails.
pub async fn run(ctx: &Ctx, cmd: &HfCmd) -> anyhow::Result<()> {
    let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
    match cmd {
        HfCmd::Search { query, limit, json } => {
            let path = format!("/v1/hf/search?q={}&limit={limit}", urlencode(query));
            let rows: Vec<HfModel> = client.get(&path).await?;
            if *json {
                return render::print_json(ServedBy::Daemon, render::now_unix(), false, &rows);
            }
            render::print_table(
                &["REPO", "DOWNLOADS", "LIKES", "GATED", "UPDATED", "TAGS"],
                rows.iter()
                    .map(|m| {
                        vec![
                            m.id.clone(),
                            render::dash(m.downloads),
                            render::dash(m.likes),
                            if m.gated { "yes" } else { "" }.to_string(),
                            m.last_modified.clone().unwrap_or_default(),
                            m.tags.join(","),
                        ]
                    })
                    .collect(),
            );
            Ok(())
        }
        HfCmd::Files { repo, json } => {
            let groups: Vec<HfFileGroup> = client
                .get(&format!("/v1/hf/models/{}/files", repo.trim_matches('/')))
                .await?;
            if *json {
                return render::print_json(ServedBy::Daemon, render::now_unix(), false, &groups);
            }
            render::print_table(
                &["QUANT", "SIZE", "SHARDS", "MMPROJ", "FILES"],
                groups.iter().map(group_row).collect(),
            );
            Ok(())
        }
        HfCmd::Get {
            repo,
            quant,
            files,
            mmproj,
            dest,
            no_wait,
            json,
        } => {
            if quant.is_none() && files.is_empty() {
                anyhow::bail!(
                    "name a --quant from `apexrouter hf files {repo}`, or one or more --file \
                     paths"
                );
            }
            let body = serde_json::json!({
                "repo": repo,
                "files": files,
                "quant": quant,
                "dest": dest,
                "mmproj": mmproj,
            });
            let job: JobRecord = client.post("/v1/hf/downloads", &body).await?;
            if *no_wait {
                if *json {
                    return render::print_json(ServedBy::Daemon, render::now_unix(), false, &job);
                }
                render::print_line(&format!(
                    "job {} ({}) — `apexrouter models ls` when it finishes",
                    job.id,
                    render::variant(&job.state)
                ));
                return Ok(());
            }
            let done = wait(&client, &job).await?;
            if *json {
                return render::print_json(ServedBy::Daemon, render::now_unix(), false, &done);
            }
            print_job(&done);
            match done.state {
                JobState::Succeeded => Ok(()),
                _ => anyhow::bail!(
                    "download {}: {}",
                    render::variant(&done.state),
                    done.error.unwrap_or_else(|| "no reason given".to_string())
                ),
            }
        }
    }
}

/// Poll a job until it leaves `Pending`/`Running`, printing each distinct message once.
///
/// Polling rather than the WebSocket: a download reports hundreds of times, `/ws` coalesces
/// and the job row is the thing that is durable. A message that has not changed is not
/// reprinted, so a 20 GB pull does not scroll the terminal off the screen.
///
/// # Errors
/// A daemon that stops answering mid-download.
async fn wait(client: &NodeClient, job: &JobRecord) -> anyhow::Result<JobRecord> {
    let mut last = String::new();
    let mut current = job.clone();
    loop {
        if matches!(
            current.state,
            JobState::Succeeded | JobState::Failed | JobState::Cancelled
        ) {
            return Ok(current);
        }
        let line = progress_line(&current);
        if line != last {
            render::print_line(&line);
            last = line;
        }
        tokio::time::sleep(POLL).await;
        current = client.get(&format!("/v1/jobs/{}", current.id)).await?;
    }
}

/// One progress line: percentage when the job knows it, and whatever it is doing now.
pub fn progress_line(j: &JobRecord) -> String {
    format!(
        "{}{}",
        j.pct
            .map(|p| format!("{p:5.1}%  "))
            .unwrap_or_else(|| "       ".to_string()),
        j.message
            .clone()
            .unwrap_or_else(|| render::variant(&j.state))
    )
}

/// The finished job, with elapsed time — the number that tells you whether the box was slow.
fn print_job(j: &JobRecord) {
    render::print_line(&format!(
        "{}  {}{}",
        render::variant(&j.state),
        j.finished_unix
            .map(|f| render::human_secs(f - j.started_unix))
            .unwrap_or_else(|| "?".to_string()),
        j.error
            .as_ref()
            .map(|e| format!("  — {e}"))
            .unwrap_or_default()
    ));
    if let Some(result) = &j.result {
        if let Some(files) = result.get("files").and_then(|f| f.as_array()) {
            for f in files {
                if let Some(p) = f.as_str() {
                    render::print_line(&format!("  {p}"));
                }
            }
        }
    }
}

/// One row of the grouped file listing.
fn group_row(g: &HfFileGroup) -> Vec<String> {
    vec![
        g.quant.clone().unwrap_or_else(|| g.label.clone()),
        render::human_bytes(g.total_bytes),
        g.files.len().to_string(),
        if g.mmproj.is_empty() { "" } else { "yes" }.to_string(),
        g.files
            .iter()
            .map(|f| f.rfilename.as_str())
            .collect::<Vec<_>>()
            .join(", "),
    ]
}

/// Percent-encode a search string so a query with a space or a `&` in it survives the trip.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '~' => out.push(ch),
            _ => {
                let mut buf = [0u8; 4];
                for b in ch.encode_utf8(&mut buf).as_bytes() {
                    out.push_str(&format!("%{b:02X}"));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexrouter_protocol::HfFile;

    fn group() -> HfFileGroup {
        HfFileGroup {
            label: "UD-Q4_K_XL".to_string(),
            quant: Some("UD-Q4_K_XL".to_string()),
            total_bytes: 18_400_000_000,
            files: vec![
                HfFile {
                    rfilename: "a-00001-of-00002.gguf".to_string(),
                    size: Some(9_200_000_000),
                    quant: Some("UD-Q4_K_XL".to_string()),
                    is_mmproj: false,
                    shard_of: Some((1, 2)),
                },
                HfFile {
                    rfilename: "a-00002-of-00002.gguf".to_string(),
                    size: Some(9_200_000_000),
                    quant: Some("UD-Q4_K_XL".to_string()),
                    is_mmproj: false,
                    shard_of: Some((2, 2)),
                },
            ],
            mmproj: Vec::new(),
        }
    }

    fn job(state: JobState, pct: Option<f32>, message: Option<&str>) -> JobRecord {
        JobRecord {
            id: apexrouter_protocol::JobId(Default::default()),
            kind: "hf.download".to_string(),
            state,
            pct,
            message: message.map(str::to_string),
            started_unix: 0,
            finished_unix: None,
            result: None,
            error: None,
        }
    }

    #[test]
    fn shards_are_one_group_with_a_summed_size() {
        let r = group_row(&group());
        assert_eq!(r[0], "UD-Q4_K_XL");
        assert_eq!(r[1], "18.4 GB", "the sum, not one shard");
        assert_eq!(r[2], "2");
    }

    #[test]
    fn a_query_survives_spaces_and_ampersands() {
        assert_eq!(urlencode("qwen3 gguf"), "qwen3%20gguf");
        assert_eq!(urlencode("a&b"), "a%26b");
        assert_eq!(urlencode("unsloth-Qwen3.GGUF"), "unsloth-Qwen3.GGUF");
    }

    #[test]
    fn a_job_with_no_percentage_still_says_what_it_is_doing() {
        assert_eq!(
            progress_line(&job(JobState::Running, Some(42.5), Some("shard 1/2"))),
            " 42.5%  shard 1/2"
        );
        assert_eq!(
            progress_line(&job(JobState::Pending, None, None)),
            "       pending"
        );
    }
}
