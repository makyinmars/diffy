use std::fs;
use std::io::Write;
use std::path::Path;

use keyring::Entry;

use crate::core::error::{DiffyError, Result};
use crate::core::review::ReviewForge;

const SERVICE: &str = "diffy";
const OPENAI_ACCOUNT: &str = "ai.openai_api_key";
const ANTHROPIC_ACCOUNT: &str = "ai.anthropic_api_key";

fn entry_for(account: &str) -> Result<Entry> {
    Entry::new(SERVICE, account).map_err(to_diffy_error)
}

fn to_diffy_error(error: keyring::Error) -> DiffyError {
    DiffyError::General(format!("keyring: {error}"))
}

fn load(account: &str) -> Result<Option<String>> {
    match entry_for(account)?.get_password() {
        Ok(value) => Ok(Some(value)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(to_diffy_error(error)),
    }
}

fn save(account: &str, value: &str) -> Result<()> {
    entry_for(account)?
        .set_password(value)
        .map_err(to_diffy_error)
}

fn clear(account: &str) -> Result<()> {
    match entry_for(account)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(to_diffy_error(error)),
    }
}

fn token_account(forge: ReviewForge) -> String {
    format!("{}.access_token", forge.key())
}

pub fn load_forge_token(forge: ReviewForge) -> Result<Option<String>> {
    load(&token_account(forge))
}

pub fn save_forge_token(forge: ReviewForge, token: &str) -> Result<()> {
    save(&token_account(forge), token)
}

pub fn clear_forge_token(forge: ReviewForge) -> Result<()> {
    clear(&token_account(forge))
}

pub fn load_token_file(path: &Path) -> Result<Option<String>> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let token = contents.trim().to_owned();
    Ok((!token.is_empty()).then_some(token))
}

pub fn save_token_file(path: &Path, token: &str) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        DiffyError::General(format!(
            "token file path has no parent directory: {}",
            path.display()
        ))
    })?;
    fs::create_dir_all(parent)?;

    let mut options = fs::OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(token.trim().as_bytes())?;
    file.write_all(b"\n")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub fn clear_token_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiKeyKind {
    OpenAi,
    Anthropic,
}

impl AiKeyKind {
    fn account(self) -> &'static str {
        match self {
            Self::OpenAi => OPENAI_ACCOUNT,
            Self::Anthropic => ANTHROPIC_ACCOUNT,
        }
    }
}

pub fn load_ai_key(kind: AiKeyKind) -> Result<Option<String>> {
    load(kind.account())
}

pub fn save_ai_key(kind: AiKeyKind, value: &str) -> Result<()> {
    save(kind.account(), value)
}

pub fn clear_ai_key(kind: AiKeyKind) -> Result<()> {
    clear(kind.account())
}

#[cfg(test)]
mod tests {
    use super::{clear_token_file, load_token_file, save_token_file, token_account};
    use crate::core::review::ReviewForge;

    #[test]
    fn github_token_file_round_trips() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("github-token.dev");

        assert_eq!(load_token_file(&path).unwrap(), None);
        save_token_file(&path, "  gho_test_token  ").unwrap();
        assert_eq!(
            load_token_file(&path).unwrap().as_deref(),
            Some("gho_test_token")
        );
        clear_token_file(&path).unwrap();
        assert_eq!(load_token_file(&path).unwrap(), None);
    }

    #[test]
    fn github_keeps_its_keyring_account() {
        assert_eq!(token_account(ReviewForge::GitHub), "github.access_token");
    }
}
