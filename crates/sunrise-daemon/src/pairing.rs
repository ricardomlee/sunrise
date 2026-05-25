use std::{
    collections::{HashMap, HashSet},
    io::{self, Write},
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
};

use aes::{
    Aes128,
    cipher::{BlockDecrypt, BlockEncrypt, KeyInit, generic_array::GenericArray},
};
use anyhow::{Context, Result, anyhow, bail};
use axum::{
    extract::{ConnectInfo, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use rand::RngCore;
use sha2::{Digest, Sha256};
use sunrise_config::{PairedClient, SunriseConfig};
use sunrise_protocol::pair_xml;
use tokio::sync::Mutex;
use tracing::{info, warn};
use x509_parser::prelude::FromDer;

use crate::identity::ServerIdentity;

#[derive(Clone)]
pub struct PairingState {
    identity: Arc<ServerIdentity>,
    sessions: Arc<Mutex<HashMap<String, PairSession>>>,
    paired_unique_ids: Arc<Mutex<HashSet<String>>>,
    config_path: PathBuf,
    config: Arc<Mutex<SunriseConfig>>,
}

impl PairingState {
    pub fn new(
        identity: Arc<ServerIdentity>,
        config_path: PathBuf,
        config: Arc<Mutex<SunriseConfig>>,
        paired_unique_ids: HashSet<String>,
    ) -> Self {
        Self {
            identity,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            paired_unique_ids: Arc::new(Mutex::new(paired_unique_ids)),
            config_path,
            config,
        }
    }

    pub async fn is_paired(&self, unique_id: &str) -> bool {
        self.paired_unique_ids.lock().await.contains(unique_id)
    }
}

#[derive(Clone)]
struct PairSession {
    aes_key: [u8; 16],
    client_cert_der: Vec<u8>,
    client_cert_pem: String,
    client_cert_signature: Vec<u8>,
    client_challenge: Option<Vec<u8>>,
    server_secret: Option<[u8; 16]>,
    server_challenge: Option<[u8; 16]>,
    client_hash: Option<Vec<u8>>,
}

pub async fn pair(
    State(state): State<crate::AppState>,
    ConnectInfo(remote): ConnectInfo<SocketAddr>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let result = handle_pair(&state.pairing, remote, query).await;
    let body = match result {
        Ok(body) => body,
        Err(err) => {
            warn!(%remote, error = %err, "pairing request failed");
            pair_xml([("paired", "0")])
        }
    };

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
        body,
    )
        .into_response()
}

pub async fn unpair(
    State(state): State<crate::AppState>,
    ConnectInfo(remote): ConnectInfo<SocketAddr>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let unique_id = query.get("uniqueid").cloned().unwrap_or_default();
    if unique_id.is_empty() {
        warn!(%remote, "unpair request missing uniqueid");
    } else {
        state.pairing.sessions.lock().await.remove(&unique_id);
        state
            .pairing
            .paired_unique_ids
            .lock()
            .await
            .remove(&unique_id);
        if let Err(err) = persist_unpaired_client(&state.pairing, &unique_id).await {
            warn!(%remote, unique_id = %unique_id, error = %err, "failed to persist unpair");
        }
        info!(%remote, unique_id = %unique_id, "client unpaired");
    }

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
        pair_xml([("paired", "0")]),
    )
        .into_response()
}

async fn handle_pair(
    pairing: &PairingState,
    remote: SocketAddr,
    query: HashMap<String, String>,
) -> Result<String> {
    let phrase = pairing_phrase(&query)?;
    let unique_id = required(&query, "uniqueid")?.to_string();
    info!(%remote, unique_id = %unique_id, phrase = %phrase, "pairing phase requested");

    match phrase {
        "getservercert" => get_server_cert(pairing, remote, &unique_id, &query).await,
        "clientchallenge" => client_challenge(pairing, &unique_id, &query).await,
        "serverchallengeresp" => server_challenge_response(pairing, &unique_id, &query).await,
        "clientpairingsecret" => client_pairing_secret(pairing, &unique_id, &query).await,
        "cancel" => {
            pairing.sessions.lock().await.remove(&unique_id);
            Ok(pair_xml([("paired", "0")]))
        }
        other => bail!("unsupported pairing phrase {other}"),
    }
}

async fn get_server_cert(
    pairing: &PairingState,
    remote: SocketAddr,
    unique_id: &str,
    query: &HashMap<String, String>,
) -> Result<String> {
    let salt = decode_hex(required(query, "salt")?)?;
    let client_cert = decode_certificate_hex(required(query, "clientcert")?)?;
    let (_, parsed_client_cert) =
        x509_parser::certificate::X509Certificate::from_der(&client_cert.der)
            .context("failed to parse Moonlight client certificate")?;
    let client_cert_signature = parsed_client_cert.signature_value.data.to_vec();

    let pin = prompt_for_pin(remote, unique_id.to_string()).await?;
    let aes_key = derive_pairing_key(&salt, &pin);

    let session = PairSession {
        aes_key,
        client_cert_der: client_cert.der,
        client_cert_pem: client_cert.pem,
        client_cert_signature,
        client_challenge: None,
        server_secret: None,
        server_challenge: None,
        client_hash: None,
    };
    pairing
        .sessions
        .lock()
        .await
        .insert(unique_id.to_string(), session);

    // Moonlight sends its certificate as hex-encoded PEM and expects the same shape back.
    let plaincert = hex::encode_upper(pairing.identity.cert_pem().as_bytes());
    info!(
        unique_id = %unique_id,
        plaincert_len = plaincert.len(),
        "returning server certificate for pairing"
    );
    Ok(pair_xml([("paired", "1"), ("plaincert", &plaincert)]))
}

async fn client_challenge(
    pairing: &PairingState,
    unique_id: &str,
    query: &HashMap<String, String>,
) -> Result<String> {
    let encrypted_challenge = decode_hex(required_any(query, &["challenge", "clientchallenge"])?)?;
    let mut sessions = pairing.sessions.lock().await;
    let session = sessions
        .get_mut(unique_id)
        .ok_or_else(|| anyhow!("pairing session not found"))?;

    let client_challenge =
        aes128_ecb_crypt(&session.aes_key, encrypted_challenge, CipherMode::Decrypt)?;
    let server_secret = random_16();
    let server_challenge = random_16();

    let mut hasher = Sha256::new();
    hasher.update(&client_challenge);
    hasher.update(pairing.identity.cert_signature());
    hasher.update(server_secret);
    let hash = hasher.finalize();

    let mut response = Vec::with_capacity(hash.len() + server_challenge.len());
    response.extend_from_slice(&hash);
    response.extend_from_slice(&server_challenge);
    let encrypted_response = aes128_ecb_crypt(&session.aes_key, response, CipherMode::Encrypt)?;

    session.client_challenge = Some(client_challenge);
    session.server_secret = Some(server_secret);
    session.server_challenge = Some(server_challenge);

    let challengeresp = hex::encode_upper(encrypted_response);
    info!(
        unique_id = %unique_id,
        challengeresp_len = challengeresp.len(),
        "returning pairing challenge response"
    );
    Ok(pair_xml([
        ("paired", "1"),
        ("challengeresponse", challengeresp.as_str()),
    ]))
}

async fn server_challenge_response(
    pairing: &PairingState,
    unique_id: &str,
    query: &HashMap<String, String>,
) -> Result<String> {
    let encrypted_response = decode_hex(required(query, "serverchallengeresp")?)?;
    let mut sessions = pairing.sessions.lock().await;
    let session = sessions
        .get_mut(unique_id)
        .ok_or_else(|| anyhow!("pairing session not found"))?;
    let server_secret = session
        .server_secret
        .ok_or_else(|| anyhow!("server secret missing from pairing session"))?;

    let client_hash = aes128_ecb_crypt(&session.aes_key, encrypted_response, CipherMode::Decrypt)?;
    session.client_hash = Some(client_hash);

    let signature = pairing.identity.sign_pairing_secret(&server_secret)?;
    let mut pairing_secret = Vec::with_capacity(server_secret.len() + signature.len());
    pairing_secret.extend_from_slice(&server_secret);
    pairing_secret.extend_from_slice(&signature);

    let pairingsecret = hex::encode_upper(pairing_secret);
    info!(
        unique_id = %unique_id,
        pairingsecret_len = pairingsecret.len(),
        "returning pairing secret"
    );
    Ok(pair_xml([
        ("paired", "1"),
        ("pairingsecret", pairingsecret.as_str()),
    ]))
}

async fn client_pairing_secret(
    pairing: &PairingState,
    unique_id: &str,
    query: &HashMap<String, String>,
) -> Result<String> {
    let client_pairing_secret = decode_hex(required(query, "clientpairingsecret")?)?;
    if client_pairing_secret.len() < 17 {
        bail!("client pairing secret is too short");
    }
    let client_secret = &client_pairing_secret[..16];

    let mut sessions = pairing.sessions.lock().await;
    let session = sessions
        .get(unique_id)
        .ok_or_else(|| anyhow!("pairing session not found"))?;
    let server_challenge = session
        .server_challenge
        .ok_or_else(|| anyhow!("server challenge missing from pairing session"))?;
    let client_hash = session
        .client_hash
        .as_ref()
        .ok_or_else(|| anyhow!("client hash missing from pairing session"))?;

    let mut hasher = Sha256::new();
    hasher.update(server_challenge);
    hasher.update(&session.client_cert_signature);
    hasher.update(client_secret);
    let expected_hash = hasher.finalize();

    if expected_hash.as_slice() != client_hash.as_slice() {
        bail!("client pairing hash did not match");
    }

    // TODO: Verify the client pairing secret signature against the client cert public key.
    // The hash check proves the client completed the PIN-protected exchange, which is enough
    // for this early milestone but not sufficient for a secure host.
    let client_cert_pem = session.client_cert_pem.clone();
    let _client_cert_der = session.client_cert_der.clone();
    sessions.remove(unique_id);
    pairing
        .paired_unique_ids
        .lock()
        .await
        .insert(unique_id.to_string());
    persist_paired_client(pairing, unique_id, client_cert_pem).await?;

    info!(unique_id = %unique_id, "client paired for current daemon session");
    Ok(pair_xml([("paired", "1")]))
}

async fn persist_paired_client(
    pairing: &PairingState,
    unique_id: &str,
    client_cert_pem: String,
) -> Result<()> {
    let mut config = pairing.config.lock().await;
    config
        .paired_clients
        .retain(|client| client.unique_id != unique_id);
    config.paired_clients.push(PairedClient {
        unique_id: unique_id.to_string(),
        client_cert_pem,
    });
    config
        .write(&pairing.config_path)
        .context("failed to persist paired client")?;
    Ok(())
}

async fn persist_unpaired_client(pairing: &PairingState, unique_id: &str) -> Result<()> {
    let mut config = pairing.config.lock().await;
    config
        .paired_clients
        .retain(|client| client.unique_id != unique_id);
    config
        .write(&pairing.config_path)
        .context("failed to persist unpaired client")?;
    Ok(())
}

async fn prompt_for_pin(remote: SocketAddr, unique_id: String) -> Result<String> {
    if let Ok(pin) = std::env::var("SUNRISE_PAIRING_PIN") {
        let pin = pin.trim().to_string();
        if pin.is_empty() {
            bail!("SUNRISE_PAIRING_PIN was set but empty");
        }
        info!(%remote, unique_id = %unique_id, "using PIN from SUNRISE_PAIRING_PIN");
        return Ok(pin);
    }

    tokio::task::spawn_blocking(move || {
        print!(
            "Moonlight pairing request from {remote} ({unique_id}). Enter the PIN shown in Moonlight: "
        );
        io::stdout().flush().context("failed to flush PIN prompt")?;
        let mut pin = String::new();
        io::stdin()
            .read_line(&mut pin)
            .context("failed to read Moonlight pairing PIN")?;
        let pin = pin.trim().to_string();
        if pin.is_empty() {
            bail!("empty pairing PIN");
        }
        Ok(pin)
    })
    .await
    .context("PIN prompt task failed")?
}

fn derive_pairing_key(salt: &[u8], pin: &str) -> [u8; 16] {
    let mut hasher = Sha256::new();
    hasher.update(salt);
    hasher.update(pin.as_bytes());
    let digest = hasher.finalize();
    let mut key = [0_u8; 16];
    key.copy_from_slice(&digest[..16]);
    key
}

enum CipherMode {
    Encrypt,
    Decrypt,
}

fn aes128_ecb_crypt(key: &[u8; 16], mut data: Vec<u8>, mode: CipherMode) -> Result<Vec<u8>> {
    if data.is_empty() || data.len() % 16 != 0 {
        bail!("AES-ECB pairing payload must be non-empty and 16-byte aligned");
    }

    let cipher = Aes128::new(GenericArray::from_slice(key));
    for chunk in data.chunks_mut(16) {
        let block = GenericArray::from_mut_slice(chunk);
        match mode {
            CipherMode::Encrypt => cipher.encrypt_block(block),
            CipherMode::Decrypt => cipher.decrypt_block(block),
        }
    }
    Ok(data)
}

fn random_16() -> [u8; 16] {
    let mut value = [0_u8; 16];
    rand::rng().fill_bytes(&mut value);
    value
}

fn decode_hex(value: &str) -> Result<Vec<u8>> {
    hex::decode(value).context("failed to decode hex query parameter")
}

struct DecodedCertificate {
    der: Vec<u8>,
    pem: String,
}

fn decode_certificate_hex(value: &str) -> Result<DecodedCertificate> {
    let decoded = decode_hex(value)?;
    if decoded.starts_with(b"-----BEGIN") {
        let pem_text = std::str::from_utf8(&decoded).context("certificate PEM was not UTF-8")?;
        let cert = pem::parse(pem_text).context("failed to parse PEM certificate")?;
        if cert.tag() != "CERTIFICATE" {
            bail!("expected PEM CERTIFICATE, got {}", cert.tag());
        }
        return Ok(DecodedCertificate {
            der: cert.contents().to_vec(),
            pem: pem_text.to_string(),
        });
    }

    Ok(DecodedCertificate {
        der: decoded.clone(),
        pem: pem::encode(&pem::Pem::new("CERTIFICATE", decoded)),
    })
}

fn required<'a>(query: &'a HashMap<String, String>, key: &str) -> Result<&'a str> {
    query
        .get(key)
        .map(String::as_str)
        .ok_or_else(|| anyhow!("missing required query parameter {key}"))
}

fn required_any<'a>(query: &'a HashMap<String, String>, keys: &[&str]) -> Result<&'a str> {
    for key in keys {
        if let Some(value) = query.get(*key) {
            return Ok(value);
        }
    }
    Err(anyhow!(
        "missing required query parameter; expected one of {}",
        keys.join(", ")
    ))
}

fn pairing_phrase(query: &HashMap<String, String>) -> Result<&'static str> {
    if let Some(phrase) = query.get("phrase") {
        return match phrase.to_ascii_lowercase().as_str() {
            "getservercert" => Ok("getservercert"),
            "clientchallenge" | "pairchallenge" => Ok("clientchallenge"),
            "serverchallengeresp" => Ok("serverchallengeresp"),
            "clientpairingsecret" => Ok("clientpairingsecret"),
            "cancel" => Ok("cancel"),
            other => Err(anyhow!("unsupported pairing phrase {other}")),
        };
    }

    if query.contains_key("clientchallenge") {
        return Ok("clientchallenge");
    }
    if query.contains_key("serverchallengeresp") {
        return Ok("serverchallengeresp");
    }
    if query.contains_key("clientpairingsecret") {
        return Ok("clientpairingsecret");
    }

    Err(anyhow!("missing required query parameter phrase"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_hex_encoded_der_certificate_as_der() {
        let der = vec![0x30, 0x03, 0x02, 0x01, 0x01];
        let decoded = decode_certificate_hex(&hex::encode(&der)).unwrap();

        assert_eq!(decoded.der, der);
        assert!(decoded.pem.starts_with("-----BEGIN CERTIFICATE-----"));
    }

    #[test]
    fn decodes_hex_encoded_pem_certificate_to_der() {
        let pem = "-----BEGIN CERTIFICATE-----\nAQIDBA==\n-----END CERTIFICATE-----\n";
        let decoded = decode_certificate_hex(&hex::encode(pem)).unwrap();

        assert_eq!(decoded.der, vec![1, 2, 3, 4]);
        assert_eq!(decoded.pem, pem);
    }

    #[test]
    fn infers_pairing_phase_from_moonlight_parameter_names() {
        let query = HashMap::from([("clientchallenge".to_string(), "abcd".to_string())]);

        assert_eq!(pairing_phrase(&query).unwrap(), "clientchallenge");
    }

    #[test]
    fn normalizes_pairchallenge_phrase_to_clientchallenge() {
        let query = HashMap::from([("phrase".to_string(), "pairchallenge".to_string())]);

        assert_eq!(pairing_phrase(&query).unwrap(), "clientchallenge");
    }

    #[test]
    fn pairing_key_uses_salt_then_pin() {
        let salt = [1_u8; 16];
        let key = derive_pairing_key(&salt, "1234");
        let expected = Sha256::digest([salt.as_slice(), b"1234".as_slice()].concat());

        assert_eq!(&key, &expected[..16]);
    }
}
