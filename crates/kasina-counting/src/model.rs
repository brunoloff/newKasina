use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub filename: String,
    pub url: String,
    pub bytes: u64,
    pub sha256: String,
}
pub fn manifest() -> Manifest {
    serde_json::from_str(include_str!("../assets/model.json")).expect("bundled model manifest")
}
pub fn verify(path: &Path) -> Result<()> {
    let expected = manifest();
    let mut file = fs::File::open(path)?;
    if file.metadata()?.len() != expected.bytes {
        bail!("The speech model is incomplete; download it again.");
    }
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    if format!("{:x}", hash.finalize()) != expected.sha256 {
        bail!("The speech model checksum is incorrect; download it again.");
    }
    Ok(())
}
pub fn find(cache: &Path) -> Option<PathBuf> {
    let name = manifest().filename;
    let mut paths = vec![cache.join(&name)];
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        paths.push(parent.join("../Resources/models").join(&name));
        paths.push(parent.join("models").join(&name));
        paths.push(parent.join("../speech-models").join(&name));
    }
    paths.into_iter().find(|path| path.is_file())
}
pub fn download(cache: &Path) -> Result<PathBuf> {
    let spec = manifest();
    fs::create_dir_all(cache)?;
    let path = cache.join(&spec.filename);
    let temporary = path.with_extension(format!("download-{}", std::process::id()));
    let result = (|| {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(300)))
            .build()
            .new_agent();
        let mut response = agent
            .get(&spec.url)
            .call()
            .context("Download the English speech model")?;
        let mut reader = response.body_mut().as_reader();
        let mut file = fs::File::create(&temporary)?;
        let mut buffer = [0; 65536];
        let mut total = 0;
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            total += count as u64;
            if total > spec.bytes {
                bail!("Unexpected speech model size");
            }
            file.write_all(&buffer[..count])?;
        }
        file.sync_all()?;
        drop(file);
        verify(&temporary)?;
        // Windows cannot replace an existing destination with rename.
        if path.exists() {
            fs::remove_file(&path)?;
        }
        fs::rename(&temporary, &path)?;
        Ok(path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}
