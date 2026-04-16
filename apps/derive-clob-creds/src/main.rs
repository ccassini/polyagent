//! Prints CLOB L2 credentials (api_key, api_secret, api_passphrase) using L1 private key.
//! Relayer-only keys from the Polymarket website do not include secret/passphrase; this uses the CLOB API.

use std::str::FromStr;

use alloy_signer_local::PrivateKeySigner;
use anyhow::Context;
use clap::Parser;
use polyhft_core::config::AppConfig;
use polymarket_client_sdk::POLYGON;
use polymarket_client_sdk::auth::{ExposeSecret, Signer};
use polymarket_client_sdk::clob::{Client as ClobClient, Config as ClobConfig};
use serde_json::json;
use toml_edit::{DocumentMut, value};

#[derive(Parser, Debug)]
#[command(
    name = "polyhft-derive-clob-creds",
    about = "Derive or create Polymarket CLOB API credentials (L2) for [auth] in config.toml"
)]
struct Args {
    #[arg(long, default_value = "config.toml")]
    config: String,

    /// Optional nonce (default 0). Use the same nonce as when the key was first created to derive.
    #[arg(long)]
    nonce: Option<u32>,

    /// JSON one-liner instead of TOML-style lines
    #[arg(long, default_value_t = false)]
    json: bool,

    /// Write api_key, api_secret, api_passphrase, wallet_address into [auth] of --config (does not print secrets).
    #[arg(long, default_value_t = false)]
    merge: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let cfg = AppConfig::load(&args.config)
        .with_context(|| format!("failed to load config from {}", args.config))?;

    let pk = cfg
        .auth
        .private_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .context(
            "auth.private_key is empty in config. Set it or use POLY_HFT__AUTH__PRIVATE_KEY and a config that loads it.",
        )?;

    let clob_cfg = ClobConfig::builder()
        .use_server_time(cfg.execution.use_server_time)
        .build();

    let host = cfg.clob.rest_url.trim_end_matches('/').to_string();
    let client = ClobClient::new(host.as_str(), clob_cfg)
        .context("failed to create unauthenticated CLOB client")?;

    let signer = PrivateKeySigner::from_str(pk)
        .context("failed to parse private key")?
        .with_chain_id(Some(POLYGON));

    let creds = client
        .create_or_derive_api_key(&signer, args.nonce)
        .await
        .context(
            "create_or_derive_api_key failed (network or CLOB auth). See https://docs.polymarket.com/developers/CLOB/authentication",
        )?;

    if args.merge {
        merge_auth_creds(
            &args.config,
            &creds.key().to_string(),
            creds.secret().expose_secret(),
            creds.passphrase().expose_secret(),
            &format!("{}", signer.address()),
        )
        .with_context(|| format!("failed to merge CLOB creds into {}", args.config))?;
        eprintln!(
            "Merged api_key, api_secret, api_passphrase, wallet_address into {}",
            args.config
        );
        return Ok(());
    }

    if args.json {
        let out = json!({
            "api_key": creds.key().to_string(),
            "api_secret": creds.secret().expose_secret(),
            "api_passphrase": creds.passphrase().expose_secret(),
            "wallet_address": format!("{}", signer.address()),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        eprintln!(
            "# Paste into [auth] (do not commit). wallet_address must match your Polymarket signer."
        );
        println!("wallet_address = \"{}\"", signer.address());
        println!("api_key = \"{}\"", creds.key());
        println!("api_secret = \"{}\"", creds.secret().expose_secret());
        println!(
            "api_passphrase = \"{}\"",
            creds.passphrase().expose_secret()
        );
    }

    Ok(())
}

fn merge_auth_creds(
    path: &str,
    api_key: &str,
    api_secret: &str,
    api_passphrase: &str,
    wallet_address: &str,
) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(path)?;
    let mut doc: DocumentMut = content
        .parse()
        .with_context(|| format!("{} is not valid TOML", path))?;
    let auth = doc
        .get_mut("auth")
        .and_then(|v| v.as_table_mut())
        .context("config has no [auth] table")?;
    auth.insert("api_key", value(api_key));
    auth.insert("api_secret", value(api_secret));
    auth.insert("api_passphrase", value(api_passphrase));
    auth.insert("wallet_address", value(wallet_address));
    std::fs::write(path, doc.to_string())?;
    Ok(())
}
