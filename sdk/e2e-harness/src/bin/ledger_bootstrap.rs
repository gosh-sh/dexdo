//! Starts a new ledger generation: parses `--dir/--run-id/--manifest`,
//! hashes the manifest file, and calls `Ledger::bootstrap`. Every error
//! path exits non-zero with a message on stderr — no defaults, no
//! fallbacks, so a run never proceeds against a manifest nobody verified.

use std::path::PathBuf;
use std::process::ExitCode;

use dodex_e2e_harness::ledger::Ledger;
use sha2::Digest;
use sha2::Sha256;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let (mut dir, mut run_id, mut manifest) = (None, None, None);
    let mut i = 1;
    while i < args.len() {
        let (flag, value) = (args[i].as_str(), args.get(i + 1));
        let value = match value {
            Some(v) => v,
            None => {
                eprintln!("missing value for {flag}");
                return ExitCode::FAILURE;
            }
        };
        match flag {
            "--dir" => dir = Some(PathBuf::from(value)),
            "--run-id" => run_id = Some(value.clone()),
            "--manifest" => manifest = Some(PathBuf::from(value)),
            other => {
                eprintln!("unknown argument: {other}");
                return ExitCode::FAILURE;
            }
        }
        i += 2;
    }

    let (Some(dir), Some(run_id), Some(manifest)) = (dir, run_id, manifest) else {
        eprintln!("usage: ledger-bootstrap --dir <path> --run-id <id> --manifest <path>");
        return ExitCode::FAILURE;
    };

    let contents = match std::fs::read(&manifest) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("failed to read manifest {}: {e}", manifest.display());
            return ExitCode::FAILURE;
        }
    };
    let manifest_hash: String =
        Sha256::digest(&contents).iter().map(|b| format!("{b:02x}")).collect();

    match Ledger::bootstrap(&dir, &run_id, Some(&manifest_hash)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ledger bootstrap failed: {e}");
            ExitCode::FAILURE
        }
    }
}
