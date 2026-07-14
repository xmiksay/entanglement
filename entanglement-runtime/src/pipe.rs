//! Bidirectional NDJSON relay (stdin: InMsg, stdout: OutEvent).
//!
//! Lines that aren't valid InMsg JSON fall back to being treated as a Prompt
//! on the default session.

use anyhow::Result;
use entanglement_core::{Holly, InMsg, SessionId};
use std::io::{stdout, Write};
use tokio::io::{stdin, AsyncBufReadExt, BufReader};
use tokio::sync::oneshot;

/// Continuous NDJSON relay.
pub async fn pipe(holly: &Holly, default_session: &SessionId) -> Result<()> {
    let mut sub = holly.subscribe();
    let (done_tx, mut done_rx) = oneshot::channel::<()>();

    let holly2 = holly.clone();
    let default_session = default_session.clone();
    tokio::spawn(async move {
        let stdin = stdin();
        let mut lines = BufReader::new(stdin).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<InMsg>(trimmed) {
                Ok(msg) => {
                    if holly2.send(msg).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = holly2
                        .send(InMsg::prompt(default_session.clone(), trimmed.to_string()))
                        .await;
                    eprintln!("note: treated line as prompt for default session ({e})");
                }
            }
        }
        let _ = done_tx.send(());
    });

    let stdout = stdout();
    let mut out = stdout.lock();
    loop {
        tokio::select! {
            ev = sub.recv() => match ev {
                Ok(ev) => {
                    writeln!(out, "{}", serde_json::to_string(&ev)?)?;
                    out.flush()?;
                }
                Err(_) => break,
            },
            _ = &mut done_rx => {
                while let Ok(ev) = tokio::time::timeout(std::time::Duration::from_millis(200), sub.recv()).await {
                    if let Ok(ev) = ev {
                        writeln!(out, "{}", serde_json::to_string(&ev)?)?;
                    }
                }
                break;
            }
        }
    }
    Ok(())
}
