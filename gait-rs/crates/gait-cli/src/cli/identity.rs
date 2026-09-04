#![allow(clippy::print_stdout, clippy::print_stderr, clippy::exit)]

use clap::{Parser, Subcommand};
use gait_ops::identity::{self_proof, SelfProofParams};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
pub struct IdentityArgs {
    #[arg(long)]
    pub gait_dir: PathBuf,
    #[command(subcommand)]
    pub kind: IdentityKind,
}

#[derive(Subcommand, Debug)]
pub enum IdentityKind {
    SelfProof {
        #[arg(long)]
        nonce_hex: String,
    },
}

pub fn cmd_identity(args: IdentityArgs) {
    let result = match args.kind {
        IdentityKind::SelfProof { nonce_hex } => hex::decode(&nonce_hex)
            .map_err(|e| gait_ops::OpError::Other(anyhow::anyhow!("nonce-hex is not valid hex: {e}")))
            .and_then(|nonce| self_proof(SelfProofParams { gait_dir: args.gait_dir.clone(), nonce })),
    };
    match result {
        Ok(outcome) => {
            println!("{}", hex::encode(&outcome.public_key));
            println!("{}", hex::encode(&outcome.signature));
        }
        Err(e) => {
            eprintln!("[gait] identity: {e}");
            std::process::exit(1);
        }
    }
}
