// SPDX-License-Identifier: Apache-2.0

use sha2::{Digest, Sha256};

#[must_use]
pub fn signing_bytes(
    challenge_id: &str,
    challenge: &str,
    method: &str,
    path: &str,
    client_node_id: &str,
    body: &[u8],
) -> Vec<u8> {
    let digest = format!("{:x}", Sha256::digest(body));
    serde_json::to_vec(&[
        "winwincode.device-auth.v1",
        challenge_id,
        challenge,
        method,
        path,
        client_node_id,
        &digest,
    ])
    .expect("string array serializes")
}
