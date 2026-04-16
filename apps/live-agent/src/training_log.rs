use std::path::Path;

use metrics::counter;
use serde::Serialize;
use tokio::fs::{OpenOptions, create_dir_all};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::mpsc;
use tracing::{error, info};

use polyhft_core::types::{DecisionBatch, ExecutionReport, SignalSnapshot};
use polyhft_paper_engine::PaperFill;

#[derive(Clone)]
pub struct TrainingLogger {
    tx: mpsc::Sender<TrainingRecord>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum TrainingRecord {
    Decision {
        ts_ms: i64,
        paper_mode: bool,
        snapshot: SignalSnapshot,
        batch: DecisionBatch,
    },
    Fill {
        ts_ms: i64,
        report: ExecutionReport,
    },
    PaperFill {
        ts_ms: i64,
        fill: PaperFill,
    },
}

impl TrainingLogger {
    pub fn maybe_spawn(path: &str, queue: usize) -> anyhow::Result<Option<Self>> {
        let path = path.trim();
        if path.is_empty() {
            return Ok(None);
        }

        let (tx, rx) = mpsc::channel(queue.max(1024));
        let path = path.to_string();
        let writer_path = path.clone();
        tokio::spawn(async move {
            if let Err(err) = run_writer(&writer_path, rx).await {
                error!(path = %writer_path, error = %err, "training logger writer failed");
            }
        });

        info!(
            path = %path,
            queue = queue.max(1024),
            "training logger enabled (decision+fill NDJSON)"
        );
        Ok(Some(Self { tx }))
    }

    pub fn log_decision(
        &self,
        ts_ms: i64,
        paper_mode: bool,
        snapshot: &SignalSnapshot,
        batch: &DecisionBatch,
    ) {
        let record = TrainingRecord::Decision {
            ts_ms,
            paper_mode,
            snapshot: snapshot.clone(),
            batch: batch.clone(),
        };
        if self.tx.try_send(record).is_err() {
            counter!("polyhft_training_log_dropped_total").increment(1);
        }
    }

    pub fn log_fill(&self, report: &ExecutionReport) {
        let record = TrainingRecord::Fill {
            ts_ms: report.timestamp_ms,
            report: report.clone(),
        };
        if self.tx.try_send(record).is_err() {
            counter!("polyhft_training_log_dropped_total").increment(1);
        }
    }

    pub fn log_paper_fill(&self, fill: &PaperFill) {
        let record = TrainingRecord::PaperFill {
            ts_ms: fill.timestamp_ms,
            fill: fill.clone(),
        };
        if self.tx.try_send(record).is_err() {
            counter!("polyhft_training_log_dropped_total").increment(1);
        }
    }
}

async fn run_writer(path: &str, mut rx: mpsc::Receiver<TrainingRecord>) -> anyhow::Result<()> {
    if let Some(parent) = Path::new(path).parent()
        && !parent.as_os_str().is_empty()
    {
        create_dir_all(parent).await?;
    }

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    let mut writer = BufWriter::new(file);

    while let Some(record) = rx.recv().await {
        let mut line = serde_json::to_vec(&record)?;
        line.push(b'\n');
        writer.write_all(&line).await?;
    }

    writer.flush().await?;
    Ok(())
}
