//! The runtime's CA, trusted by session pods, and the listener certificate it issues (ADR 11).

use std::collections::BTreeMap;

use anyhow::Context;
use k8s_openapi::ByteString;
use k8s_openapi::api::core::v1::Secret;
use kube::api::{Api, PostParams};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};

pub struct Ca {
    pub cert_pem: String,
    key: KeyPair,
}

fn ca_params() -> CertificateParams {
    let mut params = CertificateParams::default();
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, "sloppod egress CA");
    name.push(DnType::OrganizationName, "sloppod");
    params.distinguished_name = name;
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.not_before = rcgen::date_time_ymd(2026, 1, 1);
    params.not_after = rcgen::date_time_ymd(2036, 1, 1);
    params
}

impl Ca {
    /// Loads the CA from its Secret, creating both on first start.
    pub async fn load_or_create(
        client: kube::Client,
        namespace: &str,
        name: &str,
    ) -> anyhow::Result<Self> {
        let secrets: Api<Secret> = Api::namespaced(client, namespace);
        if let Some(secret) = secrets.get_opt(name).await? {
            let data = secret.data.unwrap_or_default();
            let field = |k: &str| -> anyhow::Result<String> {
                let bytes = data
                    .get(k)
                    .with_context(|| format!("CA secret {name} lacks {k}"))?;
                Ok(String::from_utf8(bytes.0.clone())?)
            };
            let key = KeyPair::from_pem(&field("tls.key")?)?;
            return Ok(Self {
                cert_pem: field("tls.crt")?,
                key,
            });
        }
        let key = KeyPair::generate()?;
        let cert = ca_params().self_signed(&key)?;
        let ca = Self {
            cert_pem: cert.pem(),
            key,
        };
        let secret = Secret {
            metadata: kube::api::ObjectMeta {
                name: Some(name.to_owned()),
                ..Default::default()
            },
            type_: Some("kubernetes.io/tls".into()),
            data: Some(BTreeMap::from([
                (
                    "tls.crt".into(),
                    ByteString(ca.cert_pem.clone().into_bytes()),
                ),
                (
                    "tls.key".into(),
                    ByteString(ca.key.serialize_pem().into_bytes()),
                ),
            ])),
            ..Default::default()
        };
        secrets.create(&PostParams::default(), &secret).await?;
        Ok(ca)
    }

    /// A server certificate for the given hosts, as (cert PEM, key PEM).
    pub fn issue(&self, hosts: &[String]) -> anyhow::Result<(String, String)> {
        let issuer = Issuer::new(ca_params(), &self.key);
        let key = KeyPair::generate()?;
        let mut params = CertificateParams::new(hosts.to_vec())?;
        params
            .distinguished_name
            .push(DnType::CommonName, "sloppod egress");
        // Clients here are Node, git and curl, which don't enforce the browser limit on leaf lifetimes.
        params.not_before = rcgen::date_time_ymd(2026, 1, 1);
        params.not_after = rcgen::date_time_ymd(2036, 1, 1);
        params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyEncipherment,
        ];
        params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        let cert = params.signed_by(&key, &issuer)?;
        Ok((cert.pem(), key.serialize_pem()))
    }
}
