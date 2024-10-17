use anyhow::Result;
use dotenv::dotenv;
use hex::ToHex;
use serde::{Deserialize, Serialize};
use serde_json;
use std::fs;
use std::time::SystemTime;

use ff::Field;
use zk_engine::nova::{
    provider::{ipa_pc, PallasEngine, VestaEngine},
    spartan::{ppsnark, snark},
    traits::{circuit::TrivialCircuit, snark::RelaxedR1CSSNARKTrait, Engine},
    CompressedSNARK, PublicParams, RecursiveSNARK,
};

use secp256k1::{ecdsa::Signature, Message, PublicKey, Secp256k1, SecretKey};
use sha2::{self, Digest};
use zk_engine::precompiles::signing::SigningCircuit;

#[derive(Serialize, Deserialize, Debug)]
struct Position {
    latitude: f64,
    longitude: f64,
    timestamp: u64,
}

type E1 = PallasEngine;
type E2 = VestaEngine;
type EE1 = ipa_pc::EvaluationEngine<E1>;
type EE2 = ipa_pc::EvaluationEngine<E2>;
type S1 = ppsnark::RelaxedR1CSSNARK<E1, EE1>;
type S2 = snark::RelaxedR1CSSNARK<E2, EE2>;

#[derive(Serialize)]
struct SendDataBody {
    data: Position,
    snark: CompressedSNARK<E1, S1, S2>,
    did: String,
}

#[derive(Deserialize)]
struct SendDataResult {
    message: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    // Simulate inputs
    let secret_key_hex = std::env::var("SECRET_KEY_HEX").expect("SECRET_KEY must be set");
    let secret_key = hex::decode(secret_key_hex).unwrap();

    // Generated from the secret_key
    let secp = Secp256k1::new();
    let public_key = SecretKey::from_byte_array(&secret_key.clone().try_into().unwrap())
        .unwrap()
        .public_key(&secp);
    println!("Public key: {:?}", public_key.serialize_uncompressed());

    let latitude = 48.8566;
    let longitude = 2.3522;
    let timestamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // build position object
    let position = Position {
        latitude,
        longitude,
        timestamp,
    };

    let hash = hash_position(&position);

    /*
     * BUILDING THE PUBLIC PARAMETERS
     */

    // create signing circuit
    type C1 = SigningCircuit<<E1 as Engine>::Scalar>;
    type C2 = TrivialCircuit<<E2 as Engine>::Scalar>;

    let circuit_primary = C1::new(hash.clone(), secret_key);
    let circuit_secondary = C2::default();

    // produce public parameters
    println!("Producing public parameters...");
    let pp = PublicParams::<E1>::setup(
        &circuit_primary,
        &circuit_secondary,
        &*S1::ck_floor(),
        &*S2::ck_floor(),
    )
    .unwrap();

    /*
     * PROVING CODE EXECUTION
     */
    let z0_primary = [<E1 as Engine>::Scalar::ZERO; 4];
    let z0_secondary = [<E2 as Engine>::Scalar::ZERO];

    // produce a recursive SNARK
    println!("Generating a RecursiveSNARK...");
    let mut recursive_snark: RecursiveSNARK<E1> = RecursiveSNARK::<E1>::new(
        &pp,
        &circuit_primary,
        &circuit_secondary,
        &z0_primary,
        &z0_secondary,
    )
    .unwrap();

    recursive_snark
        .prove_step(&pp, &circuit_primary, &circuit_secondary)
        .unwrap();

    /*
     * VERIFYING PROOF
     */

    // verify the recursive SNARK
    println!("Verifying a RecursiveSNARK...");
    let res = recursive_snark.verify(&pp, 1, &z0_primary, &z0_secondary);
    println!("RecursiveSNARK::verify: {:?}", res.is_ok());

    /*
     * COMPRESS PROOF
     */
    println!("Compressing...");
    let (pk, vk) = CompressedSNARK::<E1, S1, S2>::setup(&pp).unwrap();
    let snark = CompressedSNARK::prove(&pp, &pk, &recursive_snark).unwrap();

    let res2 = snark
        .verify(&vk, recursive_snark.num_steps(), &z0_primary, &z0_secondary)
        .unwrap();

    /*
     * RECOVERING SIGNATURE
     */

    let (signature, _) = res2;
    let mut signature_bytes: [u8; 64] = [0; 64];
    for (i, signature_part) in signature.into_iter().enumerate() {
        let part: [u8; 32] = signature_part.into();
        signature_bytes[i * 16..(i + 1) * 16].copy_from_slice(&part[0..16]);
    }
    println!("Signature : {:?}", signature_bytes.encode_hex::<String>());

    /*
     * VERIFYING SIGNATURE
     */

    let is_valid = verify_signature(&public_key, &signature_bytes, &hash);
    println!("Signature is valid: {:?}", is_valid);

    /*
     * SENDING TO SERVER
     */

    let client = reqwest::Client::new();

    let diddoc_str = fs::read_to_string("./device_register/peerDIDDoc.json").expect("file read");
    println!("DIDDoc: {}", diddoc_str);

    let diddoc_json: serde_json::Value = serde_json::from_str(&diddoc_str).expect("JSON parse");

    let did = diddoc_json["id"].as_str().expect("DID string").to_string();
    println!("DID: {}", did);

    let body = SendDataBody {
        data: position,
        snark,
        did,
    };
    let url = "http://127.0.0.1:3000/send_data";
    let response = client
        .post(url)
        .header("Content-Type", "application/json")
        .body(serde_json::to_string(&body).expect("JSON serialization"))
        .send()
        .await?;

    let result: SendDataResult = response.json().await?;
    println!("Result: {}", result.message);
    Ok(())
}

fn hash_position(position: &Position) -> Vec<u8> {
    let payload = serde_json::to_string(&position).expect("JSON serialization");
    let result = hash_message(&payload);
    result.to_vec()
}

fn hash_message(message: &str) -> Box<[u8]> {
    let mut hasher = sha2::Sha256::new();
    hasher.update(message.as_bytes());
    hasher.finalize().as_slice().into()
}

fn verify_signature(public_key: &PublicKey, sig: &[u8], hash: &[u8]) -> bool {
    let secp = Secp256k1::new();
    let message = Message::from_digest_slice(&hash).expect("32 bytes");
    let signature = Signature::from_compact(sig).expect("64 bytes");
    secp.verify_ecdsa(&message, &signature, &public_key).is_ok()
}
