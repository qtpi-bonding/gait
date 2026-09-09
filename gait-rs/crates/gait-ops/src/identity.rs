use crate::OpError;
use std::path::PathBuf;

pub struct SelfProofParams {
    pub gait_dir: PathBuf,
    pub nonce: Vec<u8>,
}

#[derive(Debug)]
pub struct SelfProofOutcome {
    pub public_key: Vec<u8>,
    pub signature: Vec<u8>,
}

pub fn self_proof(params: SelfProofParams) -> Result<SelfProofOutcome, OpError> {
    let keyring = gait_identity::Keyring::load(&params.gait_dir)
        .map_err(|e| OpError::NotFound(format!("no keyring at {}: {e}", params.gait_dir.display())))?;
    let proof = keyring.sign_self_proof(&params.nonce)?;
    Ok(SelfProofOutcome { public_key: proof.public_key, signature: proof.signature })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_proof_returns_a_verifiable_signature_for_a_real_keyring() {
        let dir = tempfile::TempDir::new().unwrap();
        gait_identity::Keyring::generate().save(dir.path()).unwrap();

        let outcome = self_proof(SelfProofParams { gait_dir: dir.path().to_path_buf(), nonce: b"n".to_vec() }).unwrap();
        let public_key_hex = hex::encode(&outcome.public_key);
        let transcript = identity_crypto::transcripts::self_proof_transcript(
            b"n", &outcome.public_key.clone().try_into().unwrap(), 1,
        );
        assert!(identity_crypto::verify_ed25519(&public_key_hex, &transcript, &outcome.signature).is_ok());
    }

    #[test]
    fn self_proof_errors_with_not_found_when_no_keyring_exists() {
        let dir = tempfile::TempDir::new().unwrap();
        let err = self_proof(SelfProofParams { gait_dir: dir.path().to_path_buf(), nonce: b"n".to_vec() }).unwrap_err();
        assert!(matches!(err, OpError::NotFound(_)));
    }
}
