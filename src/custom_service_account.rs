use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use std::{env, fmt};

use async_trait::async_trait;
use base64::{engine::general_purpose::URL_SAFE, Engine};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tracing::{instrument, Level};

use crate::authentication_manager::ServiceAccount;
use crate::error::Error;
use crate::types::{self, HyperClient, HyperExt, Signer, Token};

/// A custom service account containing credentials
///
/// Once initialized, a [`CustomServiceAccount`] can be converted into an [`AuthenticationManager`]
/// using the applicable `From` implementation.
///
/// [`AuthenticationManager`]: crate::AuthenticationManager
#[derive(Debug)]
pub struct CustomServiceAccount {
    client: HyperClient,
    credentials: ApplicationCredentials,
    signer: Signer,
    tokens: RwLock<HashMap<Vec<String>, Arc<Token>>>,
    subject: Option<String>,
}

impl CustomServiceAccount {
    /// Check `GOOGLE_APPLICATION_CREDENTIALS` environment variable for a path to JSON credentials
    pub fn from_env() -> Result<Option<Self>, Error> {
        match ApplicationCredentials::from_env()? {
            Some(credentials) => Self::new(credentials, types::client()?).map(Some),
            None => Ok(None),
        }
    }

    /// Read service account credentials from the given JSON file
    pub fn from_file<T: AsRef<Path>>(path: T) -> Result<Self, Error> {
        Self::new(ApplicationCredentials::from_file(path)?, types::client()?)
    }

    /// Read service account credentials from the given JSON string
    pub fn from_json(s: &str) -> Result<Self, Error> {
        Self::new(ApplicationCredentials::from_str(s)?, types::client()?)
    }

    /// Set the `subject` to impersonate a user
    pub fn with_subject(mut self, subject: String) -> Self {
        self.subject = Some(subject);
        self
    }

    fn new(credentials: ApplicationCredentials, client: HyperClient) -> Result<Self, Error> {
        Ok(Self {
            client,
            signer: Signer::new(&credentials.private_key)?,
            credentials,
            tokens: RwLock::new(HashMap::new()),
            subject: None,
        })
    }

    /// The RSA PKCS1 SHA256 [`Signer`] used to sign JWT tokens
    pub fn signer(&self) -> &Signer {
        &self.signer
    }

    /// The project ID as found in the credentials
    pub fn project_id(&self) -> Option<&str> {
        self.credentials.project_id.as_deref()
    }

    /// The private key as found in the credentials
    pub fn private_key_pem(&self) -> &str {
        &self.credentials.private_key
    }
}

#[async_trait]
impl ServiceAccount for CustomServiceAccount {
    async fn project_id(&self) -> Result<Arc<str>, Error> {
        match &self.credentials.project_id {
            Some(pid) => Ok(pid.clone()),
            None => Err(Error::ProjectIdNotFound),
        }
    }

    fn get_token(&self, scopes: &[&str]) -> Option<Arc<Token>> {
        let key: Vec<_> = scopes.iter().map(|x| x.to_string()).collect();
        self.tokens.read().unwrap().get(&key).cloned()
    }

    #[instrument(level = Level::DEBUG)]
    async fn refresh_token(&self, scopes: &[&str]) -> Result<Arc<Token>, Error> {
        use hyper::header;
        use url::form_urlencoded;

        let jwt =
            Claims::new(&self.credentials, scopes, self.subject.as_deref()).to_jwt(&self.signer)?;
        let rqbody = form_urlencoded::Serializer::new(String::new())
            .extend_pairs(&[("grant_type", GRANT_TYPE), ("assertion", jwt.as_str())])
            .finish();

        let mut retries = 0;
        let response = loop {
            let request = hyper::Request::post(&self.credentials.token_uri)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(hyper::Body::from(rqbody.clone()))
                .unwrap();

            tracing::debug!("requesting token from service account: {request:?}");
            let err = match self.client.request(request).await {
                // Early return when the request succeeds
                Ok(response) => break response,
                Err(err) => err,
            };

            tracing::warn!(
                "Failed to refresh token with GCP oauth2 token endpoint: {err}, trying again..."
            );
            retries += 1;
            if retries >= RETRY_COUNT {
                return Err(Error::OAuthConnectionError(err));
            }
        };

        let token = Arc::new(response.deserialize::<Token>().await?);

        let key = scopes.iter().map(|x| (*x).to_string()).collect();
        self.tokens.write().unwrap().insert(key, token.clone());
        Ok(token)
    }
}

/// Permissions requested for a JWT.
/// See https://developers.google.com/identity/protocols/OAuth2ServiceAccount#authorizingrequests.
#[derive(Serialize, Debug)]
pub(crate) struct Claims<'a> {
    iss: &'a str,
    aud: &'a str,
    exp: i64,
    iat: i64,
    sub: Option<&'a str>,
    scope: String,
}

impl<'a> Claims<'a> {
    pub(crate) fn new(
        key: &'a ApplicationCredentials,
        scopes: &[&str],
        sub: Option<&'a str>,
    ) -> Self {
        let mut scope = String::with_capacity(16);
        for (i, s) in scopes.iter().enumerate() {
            if i != 0 {
                scope.push(' ');
            }

            scope.push_str(s);
        }

        let iat = Utc::now().timestamp();
        Claims {
            iss: &key.client_email,
            aud: &key.token_uri,
            exp: iat + 3600 - 5, // Max validity is 1h
            iat,
            sub,
            scope,
        }
    }

    pub(crate) fn to_jwt(&self, signer: &Signer) -> Result<String, Error> {
        let mut jwt = String::new();
        URL_SAFE.encode_string(GOOGLE_RS256_HEAD, &mut jwt);
        jwt.push('.');
        URL_SAFE.encode_string(&serde_json::to_string(self).unwrap(), &mut jwt);

        let signature = signer.sign(jwt.as_bytes())?;
        jwt.push('.');
        URL_SAFE.encode_string(&signature, &mut jwt);
        Ok(jwt)
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct ApplicationCredentials {
    pub(crate) r#type: Option<String>,
    /// project_id
    pub(crate) project_id: Option<Arc<str>>,
    /// private_key_id
    pub(crate) private_key_id: Option<String>,
    /// private_key
    pub(crate) private_key: String,
    /// client_email
    pub(crate) client_email: String,
    /// client_id
    pub(crate) client_id: Option<String>,
    /// auth_uri
    pub(crate) auth_uri: Option<String>,
    /// token_uri
    pub(crate) token_uri: String,
    /// auth_provider_x509_cert_url
    pub(crate) auth_provider_x509_cert_url: Option<String>,
    /// client_x509_cert_url
    pub(crate) client_x509_cert_url: Option<String>,
}

impl ApplicationCredentials {
    fn from_env() -> Result<Option<Self>, Error> {
        env::var_os("GOOGLE_APPLICATION_CREDENTIALS")
            .map(|path| {
                tracing::debug!(
                    "Reading credentials file from GOOGLE_APPLICATION_CREDENTIALS env var"
                );
                Self::from_file(PathBuf::from(path))
            })
            .transpose()
    }

    fn from_file(path: impl AsRef<Path>) -> Result<Self, Error> {
        let file = File::open(path.as_ref()).map_err(Error::CustomServiceAccountPath)?;
        serde_json::from_reader::<_, ApplicationCredentials>(file)
            .map_err(Error::CustomServiceAccountCredentials)
    }
}

impl FromStr for ApplicationCredentials {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        serde_json::from_str::<ApplicationCredentials>(s)
            .map_err(Error::CustomServiceAccountCredentials)
    }
}

impl fmt::Debug for ApplicationCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApplicationCredentials")
            .field("client_email", &self.client_email)
            .field("project_id", &self.project_id)
            .finish()
    }
}

pub(crate) const GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
const GOOGLE_RS256_HEAD: &str = r#"{"alg":"RS256","typ":"JWT"}"#;

/// How many times to attempt to fetch a token from the set credentials token endpoint.
const RETRY_COUNT: u8 = 5;
