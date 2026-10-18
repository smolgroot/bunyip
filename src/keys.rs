//! Identity material: one ed25519 node key, from which everything else follows.
//!
//! The node key *is* the host's identity. Its libp2p peer ID is the hostname
//! operators type, and the SSH host key is derived from the same seed, so the
//! fingerprint `ssh` pins in `known_hosts` is bound to the peer ID.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use libp2p::identity::Keypair;
use libp2p::PeerId;
use russh::keys::ssh_key::authorized_keys::AuthorizedKeys;
use russh::keys::ssh_key::private::Ed25519Keypair;
use russh::keys::{PrivateKey, PublicKey};

/// Load the ed25519 node key from `path`, creating it on first use.
pub fn load_or_create(path: &Path) -> Result<Keypair> {
    if path.exists() {
        let mut bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        if bytes.len() != 32 {
            bail!(
                "{} is not a bunyip node key: expected 32 bytes, found {}",
                path.display(),
                bytes.len()
            );
        }
        return Keypair::ed25519_from_bytes(&mut bytes)
            .with_context(|| format!("decoding {}", path.display()));
    }

    let keypair = Keypair::generate_ed25519();
    let seed = seed(&keypair)?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    fs::write(path, seed).with_context(|| format!("writing {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    tracing::info!(path = %path.display(), "generated new node key");
    Ok(keypair)
}

/// The SSH host key, derived from the node key so there is only one secret to manage.
pub fn ssh_host_key(keypair: &Keypair) -> Result<PrivateKey> {
    Ok(PrivateKey::from(Ed25519Keypair::from_seed(&seed(keypair)?)))
}

fn seed(keypair: &Keypair) -> Result<[u8; 32]> {
    let ed = keypair
        .clone()
        .try_into_ed25519()
        .context("node key is not ed25519")?;
    let mut seed = [0u8; 32];
    seed.copy_from_slice(ed.secret().as_ref());
    Ok(seed)
}

/// Public keys permitted to authenticate, in `authorized_keys` format.
pub fn load_authorized_keys(path: &Path) -> Result<Vec<PublicKey>> {
    let entries =
        AuthorizedKeys::read_file(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(entries
        .into_iter()
        .map(|e| e.public_key().clone())
        .collect())
}

/// Peer IDs permitted to open a stream, one per line; `#` comments are ignored.
pub fn load_allowed_peers(path: &Path) -> Result<Vec<PeerId>> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    text.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .map(|l| {
            l.parse::<PeerId>()
                .with_context(|| format!("bad peer id {l:?}"))
        })
        .collect()
}

/// Expand a leading `~/` against `$HOME`.
pub fn expand(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(rest),
            None => PathBuf::from(path),
        },
        None => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_resolves_a_leading_tilde() {
        std::env::set_var("HOME", "/home/someone");
        assert_eq!(
            expand("~/.bunyip/node_key"),
            PathBuf::from("/home/someone/.bunyip/node_key")
        );
        assert_eq!(expand("/etc/bunyip/key"), PathBuf::from("/etc/bunyip/key"));
        assert_eq!(expand("relative/key"), PathBuf::from("relative/key"));
    }

    #[test]
    fn node_key_round_trips_and_pins_the_host_key() {
        let dir = std::env::temp_dir().join(format!("bunyip-keys-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("node_key");

        let first = load_or_create(&path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap().len(), 32);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        // Reloading gives the same peer ID, and the SSH host key the `ssh`
        // client pins is derived from that same seed.
        let again = load_or_create(&path).unwrap();
        assert_eq!(first.public().to_peer_id(), again.public().to_peer_id());
        assert_eq!(
            ssh_host_key(&first)
                .unwrap()
                .public_key()
                .fingerprint(russh::keys::ssh_key::HashAlg::Sha256),
            ssh_host_key(&again)
                .unwrap()
                .public_key()
                .fingerprint(russh::keys::ssh_key::HashAlg::Sha256)
        );

        std::fs::write(&path, b"too short").unwrap();
        assert!(load_or_create(&path).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn allowed_peers_skips_comments_and_blank_lines() {
        let dir = std::env::temp_dir().join(format!("bunyip-peers-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("allowed_peers");
        std::fs::write(
            &path,
            "# the laptop\n\
             12D3KooWFyTnEFq5oKMbTHrvdYZkxVBuqFJvgqSjpVwHmwFL8BQX\n\
             \n\
             12D3KooWH3uVF6wv47WnArKHk5p6cXgnDKepVbZupWjDDVoKT4WD # the CI box\n",
        )
        .unwrap();
        assert_eq!(load_allowed_peers(&path).unwrap().len(), 2);

        std::fs::write(&path, "not-a-peer-id\n").unwrap();
        assert!(load_allowed_peers(&path).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
