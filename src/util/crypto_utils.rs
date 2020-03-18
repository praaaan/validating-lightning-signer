use std::io::Write;

use bitcoin::util::address::Payload;
use bitcoin::util::bip32::{ChildNumber, ExtendedPrivKey};
use bitcoin::Network;
use bitcoin_hashes::hash160::Hash as BitcoinHash160;
use bitcoin_hashes::sha256::Hash as BitcoinSha256;
use bitcoin_hashes::{Hash, HashEngine};
use crypto::hkdf::{hkdf_expand, hkdf_extract};
use crypto::sha2::Sha256;
use secp256k1::{Error, PublicKey, Secp256k1, SecretKey, SignOnly};

pub fn hkdf_sha256(secret: &[u8], info: &[u8], salt: &[u8]) -> [u8; 32] {
    let digest = Sha256::new();
    let mut prk = [0u8; 32];
    hkdf_extract(digest, salt, secret, &mut prk);
    let mut result = [0u8; 32];
    hkdf_expand(digest, &prk, info, &mut result);
    result
}

pub fn hkdf_sha256_keys(secret: &[u8], info: &[u8], salt: &[u8]) -> [u8; 32 * 6] {
    let digest = Sha256::new();
    let mut prk = [0u8; 32];
    hkdf_extract(digest, salt, secret, &mut prk);
    let mut result = [0u8; 32 * 6];
    hkdf_expand(digest, &prk, info, &mut result);
    result
}

pub fn channels_seed(node_seed: &[u8]) -> [u8; 32] {
    hkdf_sha256(node_seed, "peer seed".as_bytes(), &[])
}

pub fn node_keys(secp_ctx: &Secp256k1<SignOnly>, node_seed: &[u8]) -> (PublicKey, SecretKey) {
    let node_private_bytes = hkdf_sha256(node_seed, "nodeid".as_bytes(), &[]);
    let node_secret_key = SecretKey::from_slice(&node_private_bytes).unwrap();
    let node_id = PublicKey::from_secret_key(&secp_ctx, &node_secret_key);
    (node_id, node_secret_key)
}

pub fn bip32_key(
    secp_ctx: &Secp256k1<SignOnly>,
    network: Network,
    node_seed: &[u8],
) -> ExtendedPrivKey {
    let bip32_seed = hkdf_sha256(node_seed, "bip32 seed".as_bytes(), &[]);
    let master = ExtendedPrivKey::new_master(network.clone(), &bip32_seed).unwrap();
    master
        .ckd_priv(&secp_ctx, ChildNumber::from_normal_idx(0).unwrap())
        .unwrap()
        .ckd_priv(&secp_ctx, ChildNumber::from_normal_idx(0).unwrap())
        .unwrap()
}

/// idx should start at INITIAL_COMMITMENT_NUMBER and count backwards
pub fn build_commitment_secret(commitment_seed: &[u8; 32], idx: u64) -> SecretKey {
    let mut res: [u8; 32] = commitment_seed.clone();
    for i in 0..48 {
        let bitpos = 47 - i;
        if idx & (1 << bitpos) == (1 << bitpos) {
            res[bitpos / 8] ^= 1 << (bitpos & 7);
            res = BitcoinSha256::hash(&res).into_inner();
        }
    }
    SecretKey::from_slice(&res).unwrap()
}

pub fn derive_public_key<T: secp256k1::Signing>(
    secp_ctx: &Secp256k1<T>,
    per_commitment_point: &PublicKey,
    base_point: &PublicKey,
) -> Result<PublicKey, secp256k1::Error> {
    let mut sha = BitcoinSha256::engine();
    sha.input(&per_commitment_point.serialize());
    sha.input(&base_point.serialize());
    let res = BitcoinSha256::from_engine(sha).into_inner();

    let hashkey = PublicKey::from_secret_key(&secp_ctx, &SecretKey::from_slice(&res)?);
    base_point.combine(&hashkey)
}

// FIXME - copied from chan_utils, lobby to increase visibility.
pub fn derive_public_revocation_key<T: secp256k1::Verification>(
    secp_ctx: &Secp256k1<T>,
    per_commitment_point: &PublicKey,
    revocation_base_point: &PublicKey,
) -> Result<PublicKey, secp256k1::Error> {
    let rev_append_commit_hash_key = {
        let mut sha = BitcoinSha256::engine();
        sha.input(&revocation_base_point.serialize());
        sha.input(&per_commitment_point.serialize());

        BitcoinSha256::from_engine(sha).into_inner()
    };
    let commit_append_rev_hash_key = {
        let mut sha = BitcoinSha256::engine();
        sha.input(&per_commitment_point.serialize());
        sha.input(&revocation_base_point.serialize());

        BitcoinSha256::from_engine(sha).into_inner()
    };

    let mut part_a = revocation_base_point.clone();
    part_a.mul_assign(&secp_ctx, &rev_append_commit_hash_key)?;
    let mut part_b = per_commitment_point.clone();
    part_b.mul_assign(&secp_ctx, &commit_append_rev_hash_key)?;
    part_a.combine(&part_b)
}

// FIXME - copied from chan_utils, lobby to increase visibility.
pub fn derive_private_revocation_key<T: secp256k1::Signing>(
    secp_ctx: &Secp256k1<T>,
    per_commitment_secret: &SecretKey,
    revocation_base_secret: &SecretKey,
) -> Result<SecretKey, secp256k1::Error> {
    let revocation_base_point = PublicKey::from_secret_key(&secp_ctx, &revocation_base_secret);
    let per_commitment_point = PublicKey::from_secret_key(&secp_ctx, &per_commitment_secret);

    let rev_append_commit_hash_key = {
        let mut sha = BitcoinSha256::engine();
        sha.input(&revocation_base_point.serialize());
        sha.input(&per_commitment_point.serialize());

        BitcoinSha256::from_engine(sha).into_inner()
    };
    let commit_append_rev_hash_key = {
        let mut sha = BitcoinSha256::engine();
        sha.input(&per_commitment_point.serialize());
        sha.input(&revocation_base_point.serialize());

        BitcoinSha256::from_engine(sha).into_inner()
    };

    let mut part_a = revocation_base_secret.clone();
    part_a.mul_assign(&rev_append_commit_hash_key)?;
    let mut part_b = per_commitment_secret.clone();
    part_b.mul_assign(&commit_append_rev_hash_key)?;
    part_a.add_assign(&part_b[..])?;
    Ok(part_a)
}

pub fn payload_for_p2wpkh(key: &PublicKey) -> Payload {
    let mut hash_engine = BitcoinHash160::engine();
    hash_engine
        .write_all(&key.serialize())
        .expect("failed to serialize pubkey");
    Payload::WitnessProgram {
        version: bech32::u5::try_from_u8(0).expect("0<32"),
        program: BitcoinHash160::from_engine(hash_engine)[..].to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_keys_test() -> Result<(), ()> {
        let secp_ctx = Secp256k1::signing_only();
        let (node_id, _) = node_keys(&secp_ctx, &[0u8; 32]);
        let node_id_bytes = node_id.serialize().to_vec();
        assert!(
            hex::encode(&node_id_bytes)
                == "02058e8b6c2ad363ec59aa136429256d745164c2bdc87f98f0a68690ec2c5c9b0b"
        );
        Ok(())
    }

    #[test]
    fn channels_seed_test() -> Result<(), ()> {
        let seed = channels_seed(&[0u8; 32]);
        assert!(
            hex::encode(&seed)
                == "ab7f29780659755f14afb82342dc19db7d817ace8c312e759a244648dfc25e53"
        );
        Ok(())
    }

    #[test]
    fn bip32_key_test() -> Result<(), ()> {
        let secp_ctx = Secp256k1::signing_only();
        let key = bip32_key(&secp_ctx, Network::Testnet, &[0u8; 32]);
        assert!(format!("{}", key) == "tprv8ejySXSgpWvEBguEGNFYNcHz29W7QxEodgnwbfLzBCccBnxGAq4vBkgqUYPGR5EnCbLvJE7YQsod6qpid85JhvAfizVpqPg3WsWB6UG3fEL");
        Ok(())
    }
}

pub fn public_key_from_raw(raw: &[u8]) -> Result<PublicKey, Error> {
    let mut x = raw[0..32].to_vec();
    x.reverse();
    let mut y = raw[32..64].to_vec();
    y.reverse();
    let mut z = x;
    z.append(&mut y);
    z.insert(0, 4u8);
    PublicKey::from_slice(z.as_slice())
}
