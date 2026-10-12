//! Per-run records (JSONL, appended as the matrix runs so an interrupted run
//! still leaves usable data) and the markdown summary the ADR cites.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub model: String,
    pub thinking: bool,
    pub variant: String,
    pub discovery: String,
    pub seeded: bool,
    pub task: String,
    pub run: u32,
    pub reached: bool,
    pub args_ok: bool,
    pub discover_calls: u32,
    pub rounds: u32,
    pub completion_tokens: u64,
    pub cached_first: f64,
    pub error: Option<String>,
}

impl Record {
    /// Table column: the encoding, plus the discovery shape for an unseeded cell.
    pub fn column(&self) -> String {
        if self.seeded {
            self.variant.clone()
        } else {
            format!("{}+{}", self.variant, self.discovery)
        }
    }
}

pub fn append_jsonl(path: &Path, r: &Record) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(
        f,
        "{}",
        serde_json::to_string(r).expect("record serializes")
    )
}

pub fn load_jsonl(path: &Path) -> Vec<Record> {
    std::fs::read_to_string(path)
        .map(|s| {
            s.lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Default)]
struct Agg {
    n: u32,
    reached: u32,
    args_ok: u32,
    discover: u32,
    rounds: u32,
    tokens: u64,
    cached: f64,
    errors: u32,
}

impl Agg {
    fn add(&mut self, r: &Record) {
        self.n += 1;
        self.reached += u32::from(r.reached);
        self.args_ok += u32::from(r.args_ok);
        self.discover += r.discover_calls;
        self.rounds += r.rounds;
        self.tokens += r.completion_tokens;
        self.cached += r.cached_first;
        self.errors += u32::from(r.error.is_some());
    }

    fn cell(&self) -> String {
        if self.n == 0 {
            return "—".into();
        }
        let n = f64::from(self.n);
        let mut s = format!(
            "{}/{} ok {} · d{:.1} r{:.1} t{:.0}",
            self.args_ok,
            self.n,
            self.reached,
            f64::from(self.discover) / n,
            f64::from(self.rounds) / n,
            self.tokens as f64 / n
        );
        if self.errors > 0 {
            s.push_str(&format!(" · {} err", self.errors));
        }
        s
    }
}

/// Columns in a stable order: the seeded encodings first (matrix order),
/// then the unseeded discovery-shape cells.
fn columns(records: &[Record], variants: &[&str]) -> Vec<String> {
    let present: BTreeSet<String> = records.iter().map(Record::column).collect();
    let mut cols: Vec<String> = variants
        .iter()
        .map(|v| v.to_string())
        .filter(|v| present.contains(v))
        .collect();
    let mut extra: Vec<String> = present.into_iter().filter(|c| c.contains('+')).collect();
    extra.sort();
    cols.extend(extra);
    cols
}

/// One table per task (rows: model × thinking, columns: encodings and
/// discovery shapes) and a closing per-column summary over every model and task.
pub fn markdown(records: &[Record], variants: &[&str], tasks: &[&str]) -> String {
    let cols = columns(records, variants);
    let mut out = String::new();
    out.push_str("# Live tool-use matrix (z.ai)\n\n");
    out.push_str(
        "Cell: `args_ok/N ok reached · d=mean discovery calls the model made itself · r=mean rounds · \
         t=mean completion tokens`. `reached` = the expected tool was called at all; `args_ok` = called with \
         the right arguments (composition: both calls in order and the answer carries the read value). \
         A `variant+shape` column is an unseeded cell: the model started from the bare task and had to \
         discover the server and its tools through that shape.\n\n",
    );
    let rows: BTreeSet<(String, bool)> = records
        .iter()
        .map(|r| (r.model.clone(), r.thinking))
        .collect();

    for task in tasks {
        out.push_str(&format!(
            "## task `{task}`\n\n| model | thinking | {} |\n",
            cols.join(" | ")
        ));
        out.push_str(&format!("| --- | --- |{}\n", " --- |".repeat(cols.len())));
        for (model, thinking) in &rows {
            let mut line = format!("| {model} | {} |", if *thinking { "on" } else { "off" });
            for c in &cols {
                let mut a = Agg::default();
                records
                    .iter()
                    .filter(|r| {
                        &r.model == model
                            && r.thinking == *thinking
                            && r.task == *task
                            && r.column() == *c
                    })
                    .for_each(|r| a.add(r));
                line.push_str(&format!(" {} |", a.cell()));
            }
            out.push_str(&line);
            out.push('\n');
        }
        out.push('\n');
    }

    out.push_str("## per-column summary (all models, all tasks)\n\n");
    out.push_str(
        "| column | runs | reached % | args ok % | mean discovery calls | mean rounds | mean completion tokens | mean cache (1st req) | errors |\n",
    );
    out.push_str("| --- | --- | --- | --- | --- | --- | --- | --- | --- |\n");
    let mut by_col: BTreeMap<String, Agg> = BTreeMap::new();
    for r in records {
        by_col.entry(r.column()).or_default().add(r);
    }
    for c in &cols {
        let Some(a) = by_col.get(c) else { continue };
        let n = f64::from(a.n.max(1));
        out.push_str(&format!(
            "| {c} | {} | {:.0} | {:.0} | {:.2} | {:.2} | {:.0} | {:.0} % | {} |\n",
            a.n,
            100.0 * f64::from(a.reached) / n,
            100.0 * f64::from(a.args_ok) / n,
            f64::from(a.discover) / n,
            f64::from(a.rounds) / n,
            a.tokens as f64 / n,
            100.0 * a.cached / n,
            a.errors
        ));
    }
    out
}
