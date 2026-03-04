//! Renders structured [`ReindexProgress`] events as indicatif progress bars.

use indexrs_core::ReindexProgress;
use indexrs_daemon::types::DaemonResponse;
use indicatif::{ProgressBar, ProgressStyle};
use tokio::io::{AsyncWriteExt, BufReader};

use crate::daemon::ensure_daemon;
use crate::output::ExitCode;
use crate::wire;
use indexrs_core::IndexError;
use indexrs_daemon::types::DaemonRequest;

/// Run `indexrs reindex` with indicatif progress bars.
pub async fn run_reindex_with_progress(
    repo_root: &std::path::Path,
    compact: bool,
) -> Result<ExitCode, IndexError> {
    let stream = ensure_daemon(repo_root, true).await?;
    let (reader, mut sock_writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    // Send Reindex request.
    let json = serde_json::to_string(&DaemonRequest::Reindex { compact })
        .map_err(|e| IndexError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))?;
    sock_writer
        .write_all(format!("{json}\n").as_bytes())
        .await
        .map_err(IndexError::Io)?;

    let mut renderer = ProgressRenderer::new();

    // Read TLV responses.
    loop {
        let resp = match wire::read_response(&mut reader).await {
            Ok(resp) => resp,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                renderer.finish();
                return Err(IndexError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "daemon disconnected without sending Done",
                )));
            }
            Err(e) => {
                renderer.finish();
                return Err(IndexError::Io(e));
            }
        };

        match resp {
            DaemonResponse::Progress { message } => {
                match serde_json::from_str::<ReindexProgress>(&message) {
                    Ok(event) => renderer.handle(event),
                    Err(_) => eprintln!("{message}"),
                }
            }
            DaemonResponse::Done {
                total, duration_ms, ..
            } => {
                renderer.finish();
                let secs = duration_ms as f64 / 1000.0;
                if total > 0 {
                    eprintln!("Reindex complete: {total} changes applied in {secs:.1}s");
                } else {
                    eprintln!("No changes detected.");
                }
                return Ok(ExitCode::Success);
            }
            DaemonResponse::Error { message } => {
                renderer.finish();
                return Err(IndexError::Io(std::io::Error::other(message)));
            }
            _ => {}
        }
    }
}

/// Manages indicatif progress bars for reindex phases.
struct ProgressRenderer {
    spinner: Option<ProgressBar>,
    bar: Option<ProgressBar>,
}

impl ProgressRenderer {
    fn new() -> Self {
        Self {
            spinner: None,
            bar: None,
        }
    }

    fn handle(&mut self, event: ReindexProgress) {
        match event {
            ReindexProgress::DetectingChanges => {
                self.clear();
                let sp = ProgressBar::new_spinner();
                sp.set_style(
                    ProgressStyle::with_template("{spinner:.cyan} {msg}")
                        .unwrap()
                        .tick_strings(&[
                            "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}",
                            "\u{2826}", "\u{2827}", "\u{2807}", "\u{280f}",
                        ]),
                );
                sp.set_message("Detecting changes...");
                sp.enable_steady_tick(std::time::Duration::from_millis(80));
                self.spinner = Some(sp);
            }
            ReindexProgress::ScanningFallback => {
                if let Some(sp) = &self.spinner {
                    sp.set_message("Scanning files (hash fallback)...");
                }
            }
            ReindexProgress::ChangesDetected {
                created,
                modified,
                deleted,
            } => {
                let total = created + modified + deleted;
                let mut parts = Vec::new();
                if modified > 0 {
                    parts.push(format!("{modified} modified"));
                }
                if created > 0 {
                    parts.push(format!("{created} created"));
                }
                if deleted > 0 {
                    parts.push(format!("{deleted} deleted"));
                }
                let detail = parts.join(", ");

                if let Some(sp) = self.spinner.take() {
                    sp.finish_and_clear();
                }
                eprintln!("Found {total} changes ({detail})");

                // Set up the progress bar for indexing phase.
                let bar = ProgressBar::new(total as u64);
                bar.set_style(
                    ProgressStyle::with_template(
                        "Indexing  [{bar:30.green/dim}] {pos}/{len} files  {msg}",
                    )
                    .unwrap()
                    .progress_chars("##-"),
                );
                self.bar = Some(bar);
            }
            ReindexProgress::NoChanges => {
                if let Some(sp) = self.spinner.take() {
                    sp.finish_and_clear();
                }
            }
            ReindexProgress::WaitingForLock => {
                self.clear();
                let sp = ProgressBar::new_spinner();
                sp.set_style(
                    ProgressStyle::with_template("{spinner:.cyan} {msg}")
                        .unwrap()
                        .tick_strings(&[
                            "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}",
                            "\u{2826}", "\u{2827}", "\u{2807}", "\u{280f}",
                        ]),
                );
                sp.set_message("Waiting for background indexing to finish...");
                sp.enable_steady_tick(std::time::Duration::from_millis(80));
                self.spinner = Some(sp);
            }
            ReindexProgress::PreparingFiles { current, total } => {
                if let Some(bar) = &self.bar {
                    bar.set_length(total as u64);
                    bar.set_position(current as u64);
                    bar.set_message("preparing...");
                }
            }
            ReindexProgress::BuildingSegment {
                segment_id,
                files_done,
                files_total: _,
            } => {
                if let Some(bar) = &self.bar {
                    bar.set_position(bar.position().max(files_done as u64));
                    bar.set_message(format!("seg_{segment_id:04}"));
                }
            }
            ReindexProgress::Tombstoning { count } => {
                if let Some(bar) = &self.bar {
                    bar.set_message(format!("tombstoning {count} entries"));
                }
            }
            ReindexProgress::CompactingSegments { input_segments } => {
                if let Some(bar) = self.bar.take() {
                    bar.finish_and_clear();
                }
                let sp = ProgressBar::new_spinner();
                sp.set_style(
                    ProgressStyle::with_template("{spinner:.cyan} {msg}")
                        .unwrap()
                        .tick_strings(&[
                            "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}",
                            "\u{2826}", "\u{2827}", "\u{2807}", "\u{280f}",
                        ]),
                );
                sp.set_message(format!("Compacting {input_segments} segments..."));
                sp.enable_steady_tick(std::time::Duration::from_millis(80));
                self.spinner = Some(sp);
            }
            ReindexProgress::CompactingCollected { .. }
            | ReindexProgress::CompactingFiles { .. }
            | ReindexProgress::CompactingWriting { .. }
            | ReindexProgress::CompactionComplete { .. } => {
                // TODO(task-4): render detailed compaction progress
            }
            ReindexProgress::Complete { .. } => {
                self.finish();
            }
        }
    }

    fn clear(&mut self) {
        if let Some(sp) = self.spinner.take() {
            sp.finish_and_clear();
        }
        if let Some(bar) = self.bar.take() {
            bar.finish_and_clear();
        }
    }

    fn finish(&mut self) {
        if let Some(sp) = self.spinner.take() {
            sp.finish_and_clear();
        }
        if let Some(bar) = self.bar.take() {
            bar.finish_and_clear();
        }
    }
}
