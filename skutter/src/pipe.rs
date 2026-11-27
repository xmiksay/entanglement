//! Bidirectional NDJSON relay (stdin: InMsg, stdout: OutEvent).
//!
//! Lines that aren't valid InMsg JSON fall back to being treated as a Prompt
//! on the default session.

use anyhow::Result;
use entanglement_core::{Holly, InMsg, SessionId, WireError};
use std::io::{stdout, Write};
use tokio::io::{stdin, AsyncBufReadExt, BufReader};
use tokio::sync::broadcast::error::RecvError;
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
                    // Untrusted wire frame: enforce the trusted/untrusted split
                    // (#155). `send_from_wire` refuses a runtime-authored
                    // `ToolResult`/`Spawn` — folding a tool result or minting a
                    // sub-agent is the executor's privileged in-process job, never
                    // a wire head's. A closed inbox ends the relay; a refused
                    // frame is logged and skipped, not fatal.
                    match holly2.send_from_wire(msg).await {
                        Ok(()) => {}
                        Err(WireError::Closed) => break,
                        Err(e @ WireError::Privileged(_)) => {
                            eprintln!("note: {e}");
                        }
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
                // A broadcast lag is a dropped-events gap, not end-of-stream: log and
                // keep relaying instead of silently killing the relay mid-conversation.
                Err(RecvError::Lagged(n)) => {
                    eprintln!("note: pipe relay lagged, skipped {n} events");
                }
                Err(RecvError::Closed) => break,
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
