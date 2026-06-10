// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Print the api_key / api_secret pairs the seeder mints for an environment's
// KEK, so an operator can hand them to clients. The derivation is the exact
// production one — `crypto::derive_api_secret` plus the seeder's api_key
// naming (`crates/infrastructure/src/seed.rs`) — so the output is what clients
// must sign requests with. Secrets print in the clear: run in a trusted shell,
// never log or commit the output.
//
//   cargo run -p dodex-api --bin dump_creds -- --kek <64-hex KEK> --count 10

use std::fmt::Write as _;
use std::process::ExitCode;

use dodex_infrastructure::crypto::derive_api_secret;
use dodex_infrastructure::crypto::Kek;

fn main() -> ExitCode {
    let mut kek_hex: Option<String> = None;
    let mut count: u32 = 0;

    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--kek" => kek_hex = argv.next(),
            "--count" | "-n" => match argv.next().and_then(|v| v.parse::<u32>().ok()) {
                Some(c) => count = c,
                None => return usage("--count requires a positive integer"),
            },
            "--help" | "-h" => return usage(""),
            other => return usage(&format!("unknown arg `{other}`")),
        }
    }

    let Some(kek_hex) = kek_hex else {
        return usage("--kek <64-hex> is required");
    };
    if count == 0 {
        return usage("--count must be >= 1");
    }
    let kek = match Kek::from_hex(&kek_hex) {
        Ok(k) => k,
        Err(e) => return usage(&format!("invalid --kek: {e:#}")),
    };

    // Same shape as the seeder: note at array index `i` -> dk_live_test_{i+1}.
    println!("api_key\tapi_secret");
    for i in 0..count {
        let mut secret = String::with_capacity(64);
        for b in derive_api_secret(&kek, i) {
            write!(secret, "{b:02x}").expect("write to String is infallible");
        }
        println!("dk_live_test_{:03}\t{secret}", i + 1);
    }
    ExitCode::SUCCESS
}

fn usage(err: &str) -> ExitCode {
    if !err.is_empty() {
        eprintln!("error: {err}\n");
    }
    eprintln!(
        "usage: dump_creds --kek <64-hex KEK> --count <N>\n\n  \
         Prints the api_key / api_secret the seeder mints for each of the first\n  \
         N note slots under the given environment KEK (auth.kek_hex). Matches the\n  \
         production derivation: api_secret = HMAC-SHA256(KEK, \"dodex/api-secret/v1\"\n  \
         || u32_be(index)); api_key = dk_live_test_{{index+1:03}}. Secrets are\n  \
         printed in cleartext — handle accordingly."
    );
    ExitCode::FAILURE
}
