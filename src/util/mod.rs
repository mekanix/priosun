pub mod cmd;

use anyhow::{Context, Result};
use std::fs::File;
use std::io::Read;

pub fn random_password() -> Result<String> {
    const CHARACTERS: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut random = [0_u8; 32];
    File::open("/dev/urandom")
        .context("failed to open /dev/urandom")?
        .read_exact(&mut random)
        .context("failed to read random root password")?;
    Ok(random
        .iter()
        .map(|byte| CHARACTERS[usize::from(*byte) % CHARACTERS.len()] as char)
        .collect())
}
