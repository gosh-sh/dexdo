// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

// Dumps the OpenAPI document built by `dodex_api::openapi_doc()` to disk
// as YAML. Stateless: no DB, no auth, no network. Intended to run from
// CI or `openapi/generate.sh` to refresh `docs/openapi.yaml` after
// any change to the api handlers or DTOs.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

const DEFAULT_OUT: &str = "docs/openapi.yaml";

fn main() -> ExitCode {
    let out = match parse_args() {
        Ok(path) => path,
        Err(msg) => {
            eprintln!("{msg}");
            eprintln!("usage: gen-openapi [--out PATH]");
            return ExitCode::from(2);
        }
    };

    let doc = dodex_api::openapi_doc();
    let yaml = match doc.to_yaml() {
        Ok(s) => s,
        Err(err) => {
            eprintln!("serialize openapi → yaml failed: {err}");
            return ExitCode::FAILURE;
        }
    };

    if let Some(parent) = out.parent()
        && !parent.as_os_str().is_empty()
        && let Err(err) = fs::create_dir_all(parent)
    {
        eprintln!("create {} failed: {err}", parent.display());
        return ExitCode::FAILURE;
    }

    if let Err(err) = fs::write(&out, yaml) {
        eprintln!("write {} failed: {err}", out.display());
        return ExitCode::FAILURE;
    }

    println!("wrote {}", out.display());
    ExitCode::SUCCESS
}

fn parse_args() -> Result<PathBuf, String> {
    let mut args = env::args().skip(1);
    let mut out: Option<PathBuf> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out" | "-o" => {
                let value = args.next().ok_or_else(|| format!("{arg} requires a value"))?;
                out = Some(PathBuf::from(value));
            }
            "-h" | "--help" => return Err("help".to_string()),
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(out.unwrap_or_else(|| PathBuf::from(DEFAULT_OUT)))
}
