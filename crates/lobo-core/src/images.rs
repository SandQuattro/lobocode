//! Resolve public tags again for every new rental; never cache a prior image.
use crate::{Error, Result};
use async_trait::async_trait;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::time::Duration;

pub const REPOSITORY: &str = "1905/lobocode";
const ACCEPT: &str = "application/vnd.oci.image.manifest.v1+json, application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.v2+json, application/vnd.docker.distribution.manifest.list.v2+json";

pub fn default_image() -> String {
    format!("ghcr.io/{REPOSITORY}:latest-{}", lobo_proto::DEFAULT_MODEL)
}
#[derive(Debug, Clone, PartialEq)]
pub enum Reference {
    Tag(String),
    Digest(String),
}
/// A GHCR image from `LOBO_IMAGE`; a tag is resolved again on every start.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageRef {
    pub repository: String,
    pub reference: Reference,
}
impl ImageRef {
    pub fn parse(image: &str) -> std::result::Result<Self, String> {
        let bad = || {
            format!(
                "want ghcr.io/<owner>/<name>:<tag> or ghcr.io/<owner>/<name>@sha256:<digest>, got {image:?}"
            )
        };
        let rest = image.trim().strip_prefix("ghcr.io/").ok_or_else(bad)?;
        let (repository, reference) = if let Some((repository, digest)) = rest.split_once('@') {
            let hex = digest.strip_prefix("sha256:").ok_or_else(bad)?;
            if hex.len() != 64 || !hex.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')) {
                return Err(bad());
            }
            (repository, Reference::Digest(digest.to_owned()))
        } else {
            let (repository, tag) = rest.rsplit_once(':').ok_or_else(bad)?;
            let tag_char = |c: u8| c.is_ascii_alphanumeric() || b"_.-".contains(&c);
            if tag.len() > 128
                || !tag.bytes().all(tag_char)
                || !tag
                    .bytes()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_')
            {
                return Err(bad());
            }
            (repository, Reference::Tag(tag.to_owned()))
        };
        let repository = repository.to_lowercase();
        let name_ok = |p: &str| {
            !p.is_empty()
                && p.bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(&c))
        };
        if repository.split('/').count() < 2 || !repository.split('/').all(name_ok) {
            return Err(bad());
        }
        Ok(Self {
            repository,
            reference,
        })
    }
}

#[async_trait]
pub trait ImageResolver: Send + Sync {
    async fn latest(&self, model: &str) -> Result<String>;
}
pub struct PublicImages {
    client: reqwest::Client,
    base: String,
    image: String,
}
impl Default for PublicImages {
    fn default() -> Self {
        Self::new(&default_image())
    }
}
impl PublicImages {
    /// The image is parsed at start, so a bad setting never blocks status or stop.
    pub fn new(image: &str) -> Self {
        Self {
            client: crate::http::client(Duration::from_secs(30)),
            base: "https://ghcr.io".into(),
            image: image.trim().to_owned(),
        }
    }
}
#[async_trait]
impl ImageResolver for PublicImages {
    async fn latest(&self, model: &str) -> Result<String> {
        lobo_proto::catalog::get(model).map_err(|e| Error::Config(e.to_string()))?;
        let image =
            ImageRef::parse(&self.image).map_err(|e| Error::Config(format!("LOBO_IMAGE: {e}")))?;
        let tag = match image.reference {
            Reference::Digest(digest) => {
                return Ok(format!("ghcr.io/{}@{digest}", image.repository));
            }
            Reference::Tag(tag) => tag,
        };
        #[derive(Deserialize)]
        struct Token {
            token: String,
        }
        let token = self
            .client
            .get(format!("{}/token", self.base))
            .query(&[
                ("service", "ghcr.io"),
                ("scope", &format!("repository:{}:pull", image.repository)),
            ])
            .header("Cache-Control", "no-cache")
            .send()
            .await?;
        if !token.status().is_success() {
            return Err(Error::Other(format!(
                "cannot access the public GPU image: HTTP {}",
                token.status()
            )));
        }
        let token: Token = token.json().await?;
        let mut response = self
            .client
            .get(format!(
                "{}/v2/{}/manifests/{tag}",
                self.base, image.repository
            ))
            .bearer_auth(token.token)
            .header("Accept", ACCEPT)
            .header("Cache-Control", "no-cache, no-store")
            .query(&[(
                "t",
                chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default(),
            )])
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(Error::Other(format!(
                "cannot resolve GPU image {}: HTTP {}; no GPU was rented",
                self.image,
                response.status()
            )));
        }
        let expected = response
            .headers()
            .get("Docker-Content-Digest")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        const MAX_MANIFEST_BYTES: usize = 4 * 1024 * 1024;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if bytes.len().saturating_add(chunk.len()) > MAX_MANIFEST_BYTES {
                return Err(Error::Other(
                    "public GPU image manifest is too large".into(),
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        let manifest: serde_json::Value = serde_json::from_slice(&bytes)?;
        let populated = |key: &str| manifest[key].as_array().is_some_and(|v| !v.is_empty());
        if manifest["schemaVersion"] != 2 || !(populated("layers") || populated("manifests")) {
            return Err(Error::Other(
                "public GPU image returned an invalid manifest".into(),
            ));
        }
        let digest = format!("sha256:{}", hex::encode(Sha256::digest(&bytes)));
        if expected.as_ref().is_some_and(|v| v != &digest) {
            return Err(Error::Other(
                "public GPU image manifest hash does not match".into(),
            ));
        }
        Ok(format!("ghcr.io/{}@{digest}", image.repository))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, path, query_param},
    };
    #[tokio::test]
    async fn latest_is_refetched_and_registry_failure_never_returns_stale_image() {
        let server = MockServer::start().await;
        let client = PublicImages {
            base: server.uri(),
            ..PublicImages::default()
        };
        let mut previous = String::new();
        for n in 0..3 {
            server.reset().await;
            Mock::given(path("/token"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({"token":"anonymous"})),
                )
                .mount(&server)
                .await;
            Mock::given(path("/v2/1905/lobocode/manifests/latest-qwen38-q6"))
                .and(header("Authorization", "Bearer anonymous"))
                .and(|r: &wiremock::Request| r.headers.get("cache-control").is_some_and(|h| {
                    h.to_str().is_ok_and(|v| v.contains("no-cache") && v.contains("no-store"))
                }))
                .respond_with(if n == 2 {
                    ResponseTemplate::new(503)
                } else {
                    ResponseTemplate::new(200).set_body_json(
                        serde_json::json!({"schemaVersion":2,"layers":[{"digest":"sha256:fixture"}],"generation":n}),
                    )
                })
                .expect(1)
                .mount(&server)
                .await;
            let result = client.latest("qwen38-q6").await;
            if n == 2 {
                assert!(result.is_err());
            } else {
                let image = result.unwrap();
                assert!(image.starts_with("ghcr.io/1905/lobocode@sha256:"));
                assert_ne!(image, previous);
                previous = image;
            }
        }
    }
    #[tokio::test]
    async fn refuses_wrong_manifest_digest() {
        let server = MockServer::start().await;
        let client = PublicImages {
            base: server.uri(),
            ..PublicImages::default()
        };
        Mock::given(path("/token"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"token":"anonymous"})),
            )
            .mount(&server)
            .await;
        Mock::given(path("/v2/1905/lobocode/manifests/latest-qwen38-q6"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Docker-Content-Digest", "sha256:bad")
                    .set_body_json(serde_json::json!({"schemaVersion":2,"layers":[{"digest":"sha256:fixture"}]})),
            )
            .mount(&server)
            .await;
        assert!(
            client
                .latest("qwen38-q6")
                .await
                .unwrap_err()
                .to_string()
                .contains("hash")
        );
    }
    #[tokio::test]
    async fn rejects_invalid_manifest_and_unknown_model() {
        let server = MockServer::start().await;
        let client = PublicImages {
            base: server.uri(),
            ..PublicImages::default()
        };
        assert!(client.latest("q2").await.is_err());
        assert!(server.received_requests().await.unwrap().is_empty());
        Mock::given(path("/token"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"token":"anonymous"})),
            )
            .mount(&server)
            .await;
        Mock::given(path("/v2/1905/lobocode/manifests/latest-qwen38-q6"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"schemaVersion":2,"layers":[]})),
            )
            .mount(&server)
            .await;
        assert!(
            client
                .latest("qwen38-q6")
                .await
                .unwrap_err()
                .to_string()
                .contains("invalid manifest")
        );
    }
    #[test]
    fn parses_tag_and_digest_references() {
        let digest = format!("sha256:{}", "a".repeat(64));
        assert_eq!(default_image(), "ghcr.io/1905/lobocode:latest-qwen38-q6");
        assert_eq!(
            ImageRef::parse(" ghcr.io/SandQuattro/lobocode:test-Q5_K.1 ").unwrap(),
            ImageRef {
                repository: "sandquattro/lobocode".into(),
                reference: Reference::Tag("test-Q5_K.1".into()),
            }
        );
        assert_eq!(
            ImageRef::parse(&format!("ghcr.io/1905/lobocode@{digest}")).unwrap(),
            ImageRef {
                repository: "1905/lobocode".into(),
                reference: Reference::Digest(digest.clone()),
            }
        );
        for bad in [
            "",
            "docker.io/1905/lobocode:latest-qwen38-q6",
            "1905/lobocode:latest-qwen38-q6",
            "ghcr.io/1905/lobocode",
            "ghcr.io/lobocode:latest-qwen38-q6",
            "ghcr.io/1905/lobocode:",
            "ghcr.io/1905/lobocode:.tag",
            "ghcr.io/1905//lobocode:tag",
            "ghcr.io/1905/lobo code:tag",
            "ghcr.io/1905/lobocode@sha256:abc",
            &format!("ghcr.io/1905/lobocode@sha256:{}", "A".repeat(64)),
            &format!("ghcr.io/1905/lobocode:{}", "t".repeat(129)),
        ] {
            assert!(ImageRef::parse(bad).is_err(), "{bad:?}");
        }
    }
    #[tokio::test]
    async fn digest_reference_passes_without_registry_calls() {
        let server = MockServer::start().await;
        let image = format!("ghcr.io/SandQuattro/lobocode@sha256:{}", "b".repeat(64));
        let client = PublicImages {
            base: server.uri(),
            ..PublicImages::new(&image)
        };
        assert_eq!(
            client.latest("qwen38-q6").await.unwrap(),
            format!("ghcr.io/sandquattro/lobocode@sha256:{}", "b".repeat(64))
        );
        assert!(server.received_requests().await.unwrap().is_empty());
    }
    #[tokio::test]
    async fn invalid_image_fails_before_registry_calls() {
        let server = MockServer::start().await;
        let client = PublicImages {
            base: server.uri(),
            ..PublicImages::new("docker.io/1905/lobocode:latest-qwen38-q6")
        };
        let err = client.latest("qwen38-q6").await.unwrap_err().to_string();
        assert!(err.contains("LOBO_IMAGE"), "{err}");
        assert!(server.received_requests().await.unwrap().is_empty());
    }
    #[tokio::test]
    async fn resolves_configured_repository_and_tag() {
        let server = MockServer::start().await;
        let client = PublicImages {
            base: server.uri(),
            ..PublicImages::new("ghcr.io/SandQuattro/lobocode:test-q5")
        };
        Mock::given(path("/token"))
            .and(query_param("scope", "repository:sandquattro/lobocode:pull"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"token":"anonymous"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/v2/sandquattro/lobocode/manifests/test-q5"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"schemaVersion":2,"layers":[{"digest":"sha256:fixture"}]}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        assert!(
            client
                .latest("qwen38-q6")
                .await
                .unwrap()
                .starts_with("ghcr.io/sandquattro/lobocode@sha256:")
        );
    }
    #[tokio::test]
    async fn unpublished_tag_fails_before_rental() {
        let server = MockServer::start().await;
        let client = PublicImages {
            base: server.uri(),
            ..PublicImages::default()
        };
        Mock::given(path("/token"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"token":"anonymous"})),
            )
            .mount(&server)
            .await;
        Mock::given(path("/v2/1905/lobocode/manifests/latest-qwen38-q6"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let err = client.latest("qwen38-q6").await.unwrap_err().to_string();
        assert!(
            err.contains("HTTP 404") && err.contains("no GPU was rented"),
            "{err}"
        );
    }
}
