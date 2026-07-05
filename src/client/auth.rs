//! Emission of the generated `client::auth` module: the `AuthProvider`
//! trait plus provider impls derived from the spec's `securitySchemes`.
//!
//! The trait, `NoAuth`, and `StaticBearer` are always emitted (the
//! bearer provider doubles as the "bring your own token" escape for
//! OAuth2 APIs); `BasicAuth`, `ApiKey`, and `OAuth2ClientCredentials`
//! follow the schemes the spec declares — see
//! [`AuthPlan`](super::plan::AuthPlan).

use proc_macro2::TokenStream;
use quote::quote;

use super::plan::{AuthPlan, OAuth2Plan};

/// The `pub mod auth { ... }` body for the resolved [`AuthPlan`].
pub(super) fn auth_tokens(plan: &AuthPlan) -> TokenStream {
    let basic = plan.basic.then(basic_tokens);
    let api_key = plan.api_key.then(api_key_tokens);
    let oauth2 = plan.oauth2.as_ref().map(oauth2_tokens);

    quote! {
        use super::support::{Error, OperationInfo};

        #[doc = "Authorizes outgoing requests.\n\nThe client calls \
                 [`AuthProvider::authorize`] with every request builder before the \
                 body is attached; implementations add whatever credentials the API \
                 expects. Custom providers (e.g. from the `ext` module) can implement \
                 this trait to change auth behavior without touching generated code."]
        #[::async_trait::async_trait]
        pub trait AuthProvider: ::std::marker::Send + ::std::marker::Sync {
            /// Attach credentials to `request`. `op` identifies the
            /// operation being sent, so providers can specialize.
            async fn authorize(
                &self,
                request: ::reqwest_middleware::RequestBuilder,
                op: &OperationInfo,
            ) -> ::std::result::Result<::reqwest_middleware::RequestBuilder, Error>;
        }

        /// No authentication: requests pass through untouched. The
        /// builder default.
        #[derive(Debug, Clone, Copy, Default)]
        pub struct NoAuth;

        #[::async_trait::async_trait]
        impl AuthProvider for NoAuth {
            async fn authorize(
                &self,
                request: ::reqwest_middleware::RequestBuilder,
                _op: &OperationInfo,
            ) -> ::std::result::Result<::reqwest_middleware::RequestBuilder, Error> {
                ::std::result::Result::Ok(request)
            }
        }

        /// A fixed token attached as `Authorization: Bearer <token>` on
        /// every request.
        #[derive(Clone)]
        pub struct StaticBearer {
            token: ::std::string::String,
        }

        impl StaticBearer {
            pub fn new(token: impl ::std::convert::Into<::std::string::String>) -> Self {
                Self {
                    token: token.into(),
                }
            }
        }

        #[::async_trait::async_trait]
        impl AuthProvider for StaticBearer {
            async fn authorize(
                &self,
                request: ::reqwest_middleware::RequestBuilder,
                _op: &OperationInfo,
            ) -> ::std::result::Result<::reqwest_middleware::RequestBuilder, Error> {
                ::std::result::Result::Ok(request.bearer_auth(&self.token))
            }
        }

        #basic
        #api_key
        #oauth2
    }
}

/// HTTP basic credentials, delegated to reqwest's `basic_auth` (which
/// handles the base64 encoding).
fn basic_tokens() -> TokenStream {
    quote! {
        /// HTTP basic authentication: `username:password`, base64-encoded
        /// by reqwest, on every request.
        #[derive(Clone)]
        pub struct BasicAuth {
            username: ::std::string::String,
            password: ::std::string::String,
        }

        impl BasicAuth {
            pub fn new(
                username: impl ::std::convert::Into<::std::string::String>,
                password: impl ::std::convert::Into<::std::string::String>,
            ) -> Self {
                Self {
                    username: username.into(),
                    password: password.into(),
                }
            }
        }

        #[::async_trait::async_trait]
        impl AuthProvider for BasicAuth {
            async fn authorize(
                &self,
                request: ::reqwest_middleware::RequestBuilder,
                _op: &OperationInfo,
            ) -> ::std::result::Result<::reqwest_middleware::RequestBuilder, Error> {
                ::std::result::Result::Ok(request.basic_auth(
                    &self.username,
                    ::std::option::Option::Some(&self.password),
                ))
            }
        }
    }
}

/// A static API key in a header or query parameter.
fn api_key_tokens() -> TokenStream {
    quote! {
        /// A static API key, attached as a header or a query parameter
        /// per the spec's `apiKey` scheme.
        #[derive(Clone)]
        pub struct ApiKey {
            name: ::std::string::String,
            value: ::std::string::String,
            in_query: bool,
        }

        impl ApiKey {
            /// An API key sent as the `name` header.
            pub fn header(
                name: impl ::std::convert::Into<::std::string::String>,
                value: impl ::std::convert::Into<::std::string::String>,
            ) -> Self {
                Self {
                    name: name.into(),
                    value: value.into(),
                    in_query: false,
                }
            }

            /// An API key sent as the `name` query parameter.
            pub fn query(
                name: impl ::std::convert::Into<::std::string::String>,
                value: impl ::std::convert::Into<::std::string::String>,
            ) -> Self {
                Self {
                    name: name.into(),
                    value: value.into(),
                    in_query: true,
                }
            }
        }

        #[::async_trait::async_trait]
        impl AuthProvider for ApiKey {
            async fn authorize(
                &self,
                request: ::reqwest_middleware::RequestBuilder,
                _op: &OperationInfo,
            ) -> ::std::result::Result<::reqwest_middleware::RequestBuilder, Error> {
                ::std::result::Result::Ok(if self.in_query {
                    request.query(&[(self.name.as_str(), self.value.as_str())])
                } else {
                    request.header(self.name.as_str(), self.value.as_str())
                })
            }
        }
    }
}

/// The OAuth2 client-credentials provider with the token fetch + TTL
/// cache, reproducing the hand-written reference client: form-encoded
/// `grant_type=client_credentials` POST, `Basic` header (optionally
/// double-base64 per `x-base64-encode-client-credentials`), bearer
/// token attached to API requests, token cached until `expires_in`.
fn oauth2_tokens(plan: &OAuth2Plan) -> TokenStream {
    let scheme_name = &plan.scheme_name;
    let token_url = &plan.token_url;
    let base64_encode = plan.base64_encode_credentials;
    let provider_doc = format!(
        "OAuth2 client-credentials flow with a TTL token cache, generated from \
         the `{scheme_name}` security scheme.\n\nFetches `POST {token_url}` with \
         `grant_type=client_credentials` and a `Basic` header carrying the client \
         credentials, caches the returned token until `expires_in` elapses, and \
         attaches it to API requests as a bearer token.{}",
        if base64_encode {
            "\n\nPer the scheme's `x-base64-encode-client-credentials: true`, the \
             id and secret are individually base64-encoded before the standard \
             `base64(id:secret)` basic-auth encoding."
        } else {
            ""
        },
    );
    let token_url_doc = format!(
        "The spec's token endpoint (`securitySchemes.{scheme_name}.flows.\
         clientCredentials.tokenUrl`)."
    );

    quote! {
        #[doc = #provider_doc]
        pub struct OAuth2ClientCredentials {
            token_url: ::std::string::String,
            client_id: ::std::string::String,
            client_secret: ::std::string::String,
            base64_encode_client_credentials: bool,
            http: ::reqwest::Client,
            token: ::std::sync::Mutex<::std::option::Option<CachedToken>>,
        }

        #[derive(Clone)]
        struct CachedToken {
            access_token: ::std::string::String,
            obtained_at: ::std::time::Instant,
            ttl: ::std::time::Duration,
        }

        #[derive(::serde::Deserialize)]
        struct TokenResponse {
            access_token: ::std::string::String,
            /// Token lifetime in seconds; when the endpoint omits it,
            /// the token is used but not cached.
            #[serde(default)]
            expires_in: ::std::option::Option<u64>,
        }

        impl OAuth2ClientCredentials {
            #[doc = #token_url_doc]
            pub const DEFAULT_TOKEN_URL: &'static str = #token_url;

            /// A provider with the spec defaults: [`Self::DEFAULT_TOKEN_URL`],
            /// the spec's credential encoding, and a fresh plain
            /// `reqwest::Client` for token fetches.
            pub fn new(
                client_id: impl ::std::convert::Into<::std::string::String>,
                client_secret: impl ::std::convert::Into<::std::string::String>,
            ) -> Self {
                Self {
                    token_url: Self::DEFAULT_TOKEN_URL.to_string(),
                    client_id: client_id.into(),
                    client_secret: client_secret.into(),
                    base64_encode_client_credentials: #base64_encode,
                    http: ::reqwest::Client::new(),
                    token: ::std::sync::Mutex::new(::std::option::Option::None),
                }
            }

            /// Override the token endpoint.
            pub fn token_url(
                mut self,
                token_url: impl ::std::convert::Into<::std::string::String>,
            ) -> Self {
                self.token_url = token_url.into();
                self
            }

            /// Override the credential encoding: when `true`, the id and
            /// secret are individually base64-encoded before the standard
            /// basic-auth encoding.
            pub fn base64_encode_client_credentials(mut self, enabled: bool) -> Self {
                self.base64_encode_client_credentials = enabled;
                self
            }

            /// Override the HTTP client used for token fetches.
            pub fn http_client(mut self, client: ::reqwest::Client) -> Self {
                self.http = client;
                self
            }

            /// The `Basic ...` header value for the token request.
            fn credentials_header(&self) -> ::std::string::String {
                use ::base64::Engine as _;
                let engine = ::base64::engine::general_purpose::STANDARD;
                let credentials = if self.base64_encode_client_credentials {
                    let id = engine.encode(&self.client_id);
                    let secret = engine.encode(&self.client_secret);
                    ::std::format!("{id}:{secret}")
                } else {
                    ::std::format!("{}:{}", self.client_id, self.client_secret)
                };
                let encoded = engine.encode(credentials);
                ::std::format!("Basic {encoded}")
            }

            #[doc = "The cached access token, refreshed when missing or expired.\n\n\
                     Lock discipline: lock, check, clone, drop — the mutex guard is \
                     never held across an await. Two callers racing an expired token \
                     may both fetch; each stores a valid token and the last store \
                     wins, so the duplicate fetch is accepted rather than \
                     serializing every call through the cache."]
            async fn access_token(
                &self,
                op: &OperationInfo,
            ) -> ::std::result::Result<::std::string::String, Error> {
                {
                    let guard = self
                        .token
                        .lock()
                        .unwrap_or_else(::std::sync::PoisonError::into_inner);
                    if let ::std::option::Option::Some(cached) = guard.as_ref()
                        && cached.obtained_at.elapsed() < cached.ttl
                    {
                        return ::std::result::Result::Ok(cached.access_token.clone());
                    }
                }
                let fetched = self.fetch_token(op).await?;
                if let ::std::option::Option::Some(expires_in) = fetched.expires_in {
                    let mut guard = self
                        .token
                        .lock()
                        .unwrap_or_else(::std::sync::PoisonError::into_inner);
                    *guard = ::std::option::Option::Some(CachedToken {
                        access_token: fetched.access_token.clone(),
                        obtained_at: ::std::time::Instant::now(),
                        ttl: ::std::time::Duration::from_secs(expires_in),
                    });
                }
                ::std::result::Result::Ok(fetched.access_token)
            }

            /// One `grant_type=client_credentials` POST to the token
            /// endpoint. Failures surface as [`Error::Auth`] naming the
            /// operation that needed the token.
            async fn fetch_token(
                &self,
                op: &OperationInfo,
            ) -> ::std::result::Result<TokenResponse, Error> {
                let response = self
                    .http
                    .post(&self.token_url)
                    .header(::reqwest::header::AUTHORIZATION, self.credentials_header())
                    .form(&[("grant_type", "client_credentials")])
                    .send()
                    .await
                    .map_err(|source| Error::Auth {
                        op: op.operation_id,
                        message: ::std::format!("token request failed: {source}"),
                    })?;
                let status = response.status();
                let body = response.text().await.map_err(|source| Error::Auth {
                    op: op.operation_id,
                    message: ::std::format!("token response read failed: {source}"),
                })?;
                if !status.is_success() {
                    return ::std::result::Result::Err(Error::Auth {
                        op: op.operation_id,
                        message: ::std::format!("token endpoint answered {status}: {body}"),
                    });
                }
                ::serde_json::from_str(&body).map_err(|source| Error::Auth {
                    op: op.operation_id,
                    message: ::std::format!(
                        "token response decode failed: {source} (line {line}, column {column}); raw body: {body}",
                        line = source.line(),
                        column = source.column(),
                    ),
                })
            }
        }

        #[::async_trait::async_trait]
        impl AuthProvider for OAuth2ClientCredentials {
            async fn authorize(
                &self,
                request: ::reqwest_middleware::RequestBuilder,
                op: &OperationInfo,
            ) -> ::std::result::Result<::reqwest_middleware::RequestBuilder, Error> {
                let token = self.access_token(op).await?;
                ::std::result::Result::Ok(request.bearer_auth(token))
            }
        }
    }
}
