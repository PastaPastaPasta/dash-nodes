//! MNAUTH (Dash Core's evo/mnauth.cpp). When our `version` carries a non-null challenge, a
//! masternode answers our `verack` with its ProTx hash and a BLS signature over the challenge, on
//! whichever network we reached it. That ties an onion, I2P or IPv6 address to a masternode its
//! ProTx doesn't list: Dash Core never tells a clearnet peer its onion or I2P address, and before
//! Dash Core 24 a ProTx can only hold one IPv4 address.

use anyhow::{Result, bail};
use blst::BLST_ERROR;
use blst::min_pk::{PublicKey, Signature};
use sha2::{Digest, Sha256};

/// Dash Core signs MNAUTH with the basic scheme (`BLSScheme::Basic`), Chia's BasicSchemeMPL.
const DST: &[u8] = b"BLS_SIG_BLS12381G2_XMD:SHA-256_SSWU_RO_NUL_";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MnAuth {
    /// The ProTx hash the node claims, as Dash Core prints it (byte-reversed hex).
    pub pro_tx_hash: String,
    pub sig: [u8; 96],
    /// The challenge we sent; the signature covers it.
    pub challenge: [u8; 32],
}

/// Parses an `mnauth` payload: proRegTxHash (32 bytes, little-endian), then a 96-byte signature.
pub fn parse(payload: &[u8], challenge: [u8; 32]) -> Result<MnAuth> {
    if payload.len() != 32 + 96 {
        bail!("mnauth of {} bytes", payload.len());
    }
    let mut hash = payload[..32].to_vec();
    hash.reverse();
    Ok(MnAuth { pro_tx_hash: to_hex(&hash), sig: payload[32..].try_into().unwrap(), challenge })
}

impl MnAuth {
    /// Whether the signature is the operator's, for a masternode that announced `protocol` in its
    /// `version`. `pubkey_operator` is the key as `protx list` prints it: basic serialization, or
    /// the legacy one for ProTx version 1, which isn't told apart by its bytes; either may match.
    ///
    /// The signed hash is SHA256d(operator key in basic form || challenge || inbound || protocol),
    /// inbound being true since we connected to the masternode.
    pub fn verify(&self, protocol: i32, pubkey_operator: &str) -> bool {
        let Some(key) = from_hex(pubkey_operator).and_then(|k| <[u8; 48]>::try_from(k).ok()) else {
            return false;
        };
        let Ok(sig) = Signature::from_bytes(&self.sig) else { return false };
        [Some(key), legacy_to_basic(key)].into_iter().flatten().any(|key| {
            let Ok(pk) = PublicKey::key_validate(&key) else { return false };
            let mut pre = Vec::with_capacity(48 + 32 + 1 + 4);
            pre.extend_from_slice(&key);
            pre.extend_from_slice(&self.challenge);
            pre.push(1);
            pre.extend_from_slice(&protocol.to_le_bytes());
            let hash = Sha256::digest(Sha256::digest(&pre));
            sig.verify(true, &hash, DST, &[], &pk, false) == BLST_ERROR::BLST_SUCCESS
        })
    }
}

/// Legacy G1 serialization (bls-signatures `fLegacy`) puts the y sign in the top bit and has no
/// compression flag; the basic one sets 0x80 for compressed and 0x20 for the sign.
fn legacy_to_basic(mut key: [u8; 48]) -> Option<[u8; 48]> {
    let sign = key[0] & 0x80 != 0;
    key[0] &= 0x7f;
    // A field element leaves the top three bits clear.
    if key[0] & 0xe0 != 0 {
        return None;
    }
    key[0] |= 0x80 | if sign { 0x20 } else { 0 };
    Some(key)
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

/// MNAUTH replies captured from live masternodes (2026-11-16) to a challenge we sent, with each
/// one's operator key from its ProTx.
#[cfg(test)]
pub mod vectors {
    use super::*;

    pub struct Vector {
        pub protx: &'static str,
        pub protocol: i32,
        pub challenge: &'static str,
        pub sig: &'static str,
        pub pubkey: &'static str,
    }

    pub const VECTORS: &[Vector] = include!("testdata/mnauth_vectors.rs");

    pub fn auth(v: &Vector) -> MnAuth {
        let mut payload = from_hex(v.protx).unwrap();
        payload.reverse();
        payload.extend(from_hex(v.sig).unwrap());
        parse(&payload, from_hex(v.challenge).unwrap().try_into().unwrap()).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::vectors::*;
    use super::*;

    #[test]
    fn live_signatures_verify() {
        for v in VECTORS {
            let a = auth(v);
            assert_eq!(a.pro_tx_hash, v.protx);
            assert!(a.verify(v.protocol, v.pubkey), "{}", v.protx);
        }
    }

    #[test]
    fn legacy_and_basic_keys_both_seen() {
        // ProTx version 1 keys print in the legacy form (top bits 00 here), newer ones in basic (10).
        assert!(VECTORS.iter().any(|v| from_hex(v.pubkey).unwrap()[0] & 0xc0 != 0x80));
        assert!(VECTORS.iter().any(|v| from_hex(v.pubkey).unwrap()[0] & 0xc0 == 0x80));
    }

    #[test]
    fn rejects_what_isnt_the_operators() {
        let v = &VECTORS[0];
        let a = auth(v);
        // Another masternode's key, another challenge, another protocol, a flipped signature bit.
        assert!(!a.verify(v.protocol, VECTORS[1].pubkey));
        assert!(!MnAuth { challenge: [7; 32], ..a.clone() }.verify(v.protocol, v.pubkey));
        assert!(!a.verify(v.protocol - 1, v.pubkey));
        let mut sig = a.sig;
        sig[95] ^= 1;
        assert!(!MnAuth { sig, ..a.clone() }.verify(v.protocol, v.pubkey));
        assert!(!a.verify(v.protocol, ""));
        assert!(!a.verify(v.protocol, "zz"));
        assert!(!a.verify(v.protocol, &v.pubkey[2..]));
    }

    /// A legacy key with the sign bit set (0x80) also reads as a valid basic key for the negated
    /// point, so only the fallback finds the operator's key. Rewrites basic keys with the sign set.
    #[test]
    fn legacy_key_with_sign_bit_verifies() {
        let mut seen = 0;
        for v in VECTORS.iter().filter(|v| from_hex(v.pubkey).unwrap()[0] & 0xe0 == 0xa0) {
            let mut key = from_hex(v.pubkey).unwrap();
            key[0] = (key[0] & 0x1f) | 0x80;
            assert!(auth(v).verify(v.protocol, &to_hex(&key)), "{}", v.protx);
            seen += 1;
        }
        assert!(seen > 0);
    }

    #[test]
    fn parse_checks_length() {
        assert!(parse(&[0; 127], [0; 32]).is_err());
        assert!(parse(&[0; 129], [0; 32]).is_err());
    }

    #[test]
    fn legacy_conversion() {
        let mut k = [0u8; 48];
        k[0] = 0x85;
        assert_eq!(legacy_to_basic(k).unwrap()[0], 0xa5);
        k[0] = 0x05;
        assert_eq!(legacy_to_basic(k).unwrap()[0], 0x85);
        k[0] = 0xc5;
        assert_eq!(legacy_to_basic(k), None);
    }
}
