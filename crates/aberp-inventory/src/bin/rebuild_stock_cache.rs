//! `rebuild-stock-cache` — the recovery binary per ADR-0061 §3.
//!
//! Walks every product in the named tenant's DuckDB file and re-derives
//! `stock_qty` (+ `last_movement_at`) from `SUM(stock_movements.qty_delta)`
//! in one transaction. Idempotent; safe to re-run.
//!
//! Usage:
//!
//! ```text
//! cargo run -p aberp-inventory --bin rebuild-stock-cache -- \
//!     --tenant <tenant_id> --db <path-to-duckdb>
//! ```
//!
//! No flags beyond `--tenant` + `--db`; the binary is intentionally
//! single-purpose. A future operator-friendly wrapper (e.g. as a
//! Tauri-shell command) can call [`aberp_inventory::rebuild_stock_cache_for_tenant`]
//! directly without going through this binary.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use duckdb::Connection;

fn print_usage_and_exit() -> ExitCode {
    eprintln!(
        "rebuild-stock-cache --tenant <tenant_id> --db <path-to-duckdb>\n\
         \n\
         Re-derives products.stock_qty from SUM(stock_movements.qty_delta)\n\
         per ADR-0061 §3. Run when the cache and ledger disagree."
    );
    ExitCode::from(2)
}

fn parse_args() -> Result<(String, PathBuf)> {
    let mut args = std::env::args().skip(1);
    let mut tenant: Option<String> = None;
    let mut db: Option<PathBuf> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--tenant" => {
                tenant = args.next();
            }
            "--db" => {
                db = args.next().map(PathBuf::from);
            }
            "-h" | "--help" => {
                anyhow::bail!("help requested");
            }
            other => anyhow::bail!("unknown argument {other:?}"),
        }
    }
    Ok((
        tenant.context("--tenant is required")?,
        db.context("--db is required")?,
    ))
}

fn run() -> Result<u64> {
    let (tenant, db_path) = parse_args()?;
    let mut conn = Connection::open(&db_path)
        .with_context(|| format!("open tenant DuckDB at {}", db_path.display()))?;

    // Idempotent schema-ensure so the binary works against a fresh
    // tenant DB that has products but has not yet booted aberp serve
    // (the boot path is where ensure_schema would ordinarily run).
    aberp_inventory::ensure_schema(&conn).context("ensure inventory schema before rebuild")?;

    let touched = aberp_inventory::rebuild_stock_cache_for_tenant(&mut conn, &tenant)
        .context("rebuild_stock_cache_for_tenant")?;
    Ok(touched)
}

fn main() -> ExitCode {
    match run() {
        Ok(touched) => {
            println!(
                "rebuild-stock-cache: reconciled {} product(s) against their ledger SUM",
                touched
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("rebuild-stock-cache: error: {e:?}");
            print_usage_and_exit()
        }
    }
}
