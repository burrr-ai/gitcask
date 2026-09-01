use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result};

pub(crate) async fn mint(
    config: &gitcask_config::Config,
    key: &Path,
    principal: &str,
    scopes: &[String],
    ttl: Duration,
) -> Result<()> {
    let private_key = tokio::fs::read_to_string(key)
        .await
        .with_context(|| format!("reading Ed25519 private key {}", key.display()))?;
    let token = gitcask_server::auth::mint_token(
        &private_key,
        &config.auth.jwt.issuer,
        config.auth.jwt.audience.as_deref(),
        principal,
        scopes,
        ttl,
    )?;
    println!("{token}");
    Ok(())
}

pub(crate) fn keygen(private_key: &Path, public_key: &Path) -> Result<()> {
    let (private_pem, public_pem) = gitcask_server::auth::generate_key_pair_pem()?;
    let mut public = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(public_key)
        .with_context(|| format!("creating {}", public_key.display()))?;

    #[cfg(unix)]
    let private_result = {
        use std::os::unix::fs::OpenOptionsExt as _;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(private_key)
    };
    #[cfg(not(unix))]
    let private_result = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(private_key);

    let mut private = match private_result {
        Ok(file) => file,
        Err(error) => {
            drop(public);
            let _ = std::fs::remove_file(public_key);
            return Err(error).with_context(|| format!("creating {}", private_key.display()));
        }
    };
    public
        .write_all(public_pem.as_bytes())
        .with_context(|| format!("writing {}", public_key.display()))?;
    private
        .write_all(private_pem.as_bytes())
        .with_context(|| format!("writing {}", private_key.display()))?;
    eprintln!("private key: {}", private_key.display());
    eprintln!("public key:  {}", public_key.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keygen_writes_parseable_files_and_refuses_overwrite() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let private = directory.path().join("private.pem");
        let public = directory.path().join("public.pem");
        keygen(&private, &public)?;
        let private_pem = std::fs::read_to_string(&private)?;
        let public_pem = std::fs::read_to_string(&public)?;
        let token = gitcask_server::auth::mint_token(
            &private_pem,
            "issuer",
            None,
            "principal",
            &["owner/repo:read".into()],
            Duration::from_mins(1),
        )?;
        assert_eq!(token.split('.').count(), 3);
        assert!(public_pem.contains("BEGIN PUBLIC KEY"));
        assert!(keygen(&private, &public).is_err());
        Ok(())
    }
}
