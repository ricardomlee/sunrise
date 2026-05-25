use anyhow::{Context, Result};
use rcgen::{
    CertificateParams, DistinguishedName, DnType, KeyPair, PKCS_RSA_SHA256, RsaKeySize, SigningKey,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sunrise_config::SunriseConfig;
use x509_parser::prelude::FromDer;

pub struct ServerIdentity {
    cert_der: Vec<u8>,
    cert_pem: String,
    cert_signature: Vec<u8>,
    key_der: Vec<u8>,
    key_pair: KeyPair,
}

impl ServerIdentity {
    pub fn load_or_generate(config: &mut SunriseConfig) -> Result<(Self, bool)> {
        match (
            config.server_cert_pem.as_deref(),
            config.server_private_key_pem.as_deref(),
        ) {
            (Some(cert_pem), Some(key_pem)) => {
                Ok((Self::from_pem(cert_pem.to_string(), key_pem)?, false))
            }
            _ => {
                let identity = Self::generate(config)?;
                config.server_cert_pem = Some(identity.cert_pem.clone());
                config.server_private_key_pem = Some(identity.key_pair.serialize_pem());
                Ok((identity, true))
            }
        }
    }

    fn generate(config: &SunriseConfig) -> Result<Self> {
        let key_pair = KeyPair::generate_rsa_for(&PKCS_RSA_SHA256, RsaKeySize::_2048)
            .context("failed to generate RSA-2048 server key")?;

        let mut params = CertificateParams::new(vec![
            config.host_name.clone(),
            "localhost".to_string(),
            "127.0.0.1".to_string(),
        ])
        .context("failed to create certificate parameters")?;
        let mut distinguished_name = DistinguishedName::new();
        distinguished_name.push(DnType::CommonName, &config.host_name);
        params.distinguished_name = distinguished_name;

        let cert = params
            .self_signed(&key_pair)
            .context("failed to self-sign server certificate")?;
        let cert_pem = cert.pem();
        let cert_der = cert.der().to_vec();
        let key_der = key_pair.serialize_der();
        let (_, parsed_cert) = x509_parser::certificate::X509Certificate::from_der(&cert_der)
            .context("failed to parse generated server certificate")?;
        let cert_signature = parsed_cert.signature_value.data.to_vec();

        Ok(Self {
            cert_der,
            cert_pem,
            cert_signature,
            key_der,
            key_pair,
        })
    }

    fn from_pem(cert_pem: String, key_pem: &str) -> Result<Self> {
        let cert_der = pem::parse(&cert_pem)
            .context("failed to parse persisted server certificate PEM")?
            .contents()
            .to_vec();
        let key_pair =
            KeyPair::from_pem(key_pem).context("failed to parse persisted server private key")?;
        let key_der = key_pair.serialize_der();
        let (_, parsed_cert) = x509_parser::certificate::X509Certificate::from_der(&cert_der)
            .context("failed to parse persisted server certificate")?;
        let cert_signature = parsed_cert.signature_value.data.to_vec();

        Ok(Self {
            cert_der,
            cert_pem,
            cert_signature,
            key_der,
            key_pair,
        })
    }

    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    pub fn cert_signature(&self) -> &[u8] {
        &self.cert_signature
    }

    pub fn sign_pairing_secret(&self, server_secret: &[u8]) -> Result<Vec<u8>> {
        self.key_pair
            .sign(server_secret)
            .context("failed to sign pairing secret")
    }

    pub fn tls_config(&self) -> Result<rustls::ServerConfig> {
        let cert_der = CertificateDer::from(self.cert_der.clone());
        let key_der = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(self.key_der.clone()));

        rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key_der)
            .context("failed to build rustls server config")
    }
}
