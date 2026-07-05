//! Emission of the `pub mod client { ... }` module from a resolved
//! [`ClientPlan`]: the `Client` / `ClientBuilder` pair and one async
//! method per operation, with the `auth` and `support` submodules
//! spliced in. Everything rides the normal AST → render pipeline, so
//! doc normalization, item spacing, and prettyplease formatting apply
//! for free.

use proc_macro2::TokenStream;
use quote::quote;

use super::auth::auth_tokens;
use super::plan::{ClientPlan, OperationPlan, ParamPlan, ParamType};
use super::support::support_tokens;
use crate::spec::ParamLocation;

/// The complete `pub mod client { ... }` item.
pub(crate) fn client_tokens(plan: &ClientPlan) -> TokenStream {
    let auth = auth_tokens(&plan.auth);
    let support = support_tokens();

    let title = plan.title.as_deref().unwrap_or("the API");
    let version = plan
        .version
        .as_ref()
        .map(|version| format!(" (v{version})"))
        .unwrap_or_default();
    let client_doc = match &plan.default_base_url {
        Some(base_url) => format!(
            "Client for {title}{version}.\n\nConstruct via [`Client::builder`]; the \
             base URL defaults to the spec's first server, `{base_url}`.",
        ),
        None => format!(
            "Client for {title}{version}.\n\nConstruct via [`Client::builder`], \
             which takes the base URL (the spec declares no `servers`).",
        ),
    };

    let (builder_new, builder_default, client_builder_fn) = builder_constructor(plan);
    let methods = plan.operations.iter().map(operation_method);

    quote! {
        /// Generated API client: `client::Client`, its builder, the
        /// `auth` providers, and the `support` error/helper surface.
        pub mod client {
            pub mod auth {
                #auth
            }

            pub mod support {
                #support
            }

            use self::auth::AuthProvider;
            use self::support::{BodyHook, Error, OperationInfo};

            #[doc = #client_doc]
            #[derive(Clone)]
            pub struct Client {
                base_url: ::std::string::String,
                client: ::reqwest_middleware::ClientWithMiddleware,
                auth: ::std::sync::Arc<dyn AuthProvider>,
                body_hooks: ::std::vec::Vec<::std::sync::Arc<BodyHook>>,
            }

            impl ::std::fmt::Debug for Client {
                fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                    f.debug_struct("Client")
                        .field("base_url", &self.base_url)
                        .finish_non_exhaustive()
                }
            }

            /// Builder for [`Client`].
            pub struct ClientBuilder {
                base_url: ::std::string::String,
                client: ::std::option::Option<::reqwest_middleware::ClientWithMiddleware>,
                auth: ::std::sync::Arc<dyn AuthProvider>,
                body_hooks: ::std::vec::Vec<::std::sync::Arc<BodyHook>>,
            }

            impl ClientBuilder {
                #builder_new

                /// Override the base URL operation paths are appended
                /// to. A trailing `/` is trimmed at build time.
                pub fn base_url(
                    mut self,
                    base_url: impl ::std::convert::Into<::std::string::String>,
                ) -> Self {
                    self.base_url = base_url.into();
                    self
                }

                #[doc = "Supply the HTTP client requests are sent through: a \
                         `reqwest_middleware::ClientWithMiddleware` carrying a \
                         middleware stack, or a plain `reqwest::Client` (converted \
                         middleware-free via `From`). Defaults to a fresh plain \
                         client."]
                pub fn client(
                    mut self,
                    client: impl ::std::convert::Into<::reqwest_middleware::ClientWithMiddleware>,
                ) -> Self {
                    self.client = ::std::option::Option::Some(client.into());
                    self
                }

                /// Set the authentication provider; see [`auth`] for the
                /// generated implementations. Defaults to
                /// [`auth::NoAuth`].
                pub fn auth(mut self, auth: impl AuthProvider + 'static) -> Self {
                    self.auth = ::std::sync::Arc::new(auth);
                    self
                }

                #[doc = "Register a request-body hook: after a request body is \
                         serialized to JSON and before it is sent, every hook runs \
                         against the `serde_json::Value` in registration order. The \
                         seam for cross-cutting request edits (inject a tenant field, \
                         fill a default) without forking generated code; \
                         `OperationInfo` tells the hook which operation is sending."]
                pub fn body_hook(
                    mut self,
                    hook: impl ::std::ops::Fn(&OperationInfo, &mut ::serde_json::Value)
                        + ::std::marker::Send
                        + ::std::marker::Sync
                        + 'static,
                ) -> Self {
                    self.body_hooks.push(::std::sync::Arc::new(hook));
                    self
                }

                pub fn build(self) -> Client {
                    Client {
                        base_url: self.base_url.trim_end_matches('/').to_string(),
                        client: self
                            .client
                            .unwrap_or_else(|| ::reqwest::Client::new().into()),
                        auth: self.auth,
                        body_hooks: self.body_hooks,
                    }
                }
            }

            #builder_default

            impl Client {
                #client_builder_fn

                /// The configured base URL (trailing `/` trimmed).
                pub fn base_url(&self) -> &str {
                    &self.base_url
                }

                #(#methods)*
            }
        }
    }
}

/// The `ClientBuilder::new` constructor, the builder's `Default` impl
/// (only when the spec supplies a default base URL — without `servers`,
/// `new` takes the base URL as a required argument), and the
/// `Client::builder()` sugar.
fn builder_constructor(
    plan: &ClientPlan,
) -> (TokenStream, Option<TokenStream>, TokenStream) {
    match &plan.default_base_url {
        Some(base_url) => {
            let doc = format!(
                "Start a builder with the spec's default base URL, `{base_url}`.",
            );
            let new = quote! {
                #[doc = #doc]
                pub fn new() -> Self {
                    Self {
                        base_url: #base_url.to_string(),
                        client: ::std::option::Option::None,
                        auth: ::std::sync::Arc::new(auth::NoAuth),
                        body_hooks: ::std::vec::Vec::new(),
                    }
                }
            };
            let default = quote! {
                impl ::std::default::Default for ClientBuilder {
                    fn default() -> Self {
                        Self::new()
                    }
                }
            };
            let builder_fn = quote! {
                /// Start building a client; see [`ClientBuilder`].
                pub fn builder() -> ClientBuilder {
                    ClientBuilder::new()
                }
            };
            (new, Some(default), builder_fn)
        }
        None => {
            let new = quote! {
                /// Start a builder. The spec declares no `servers`, so
                /// the base URL is required here.
                pub fn new(base_url: impl ::std::convert::Into<::std::string::String>) -> Self {
                    Self {
                        base_url: base_url.into(),
                        client: ::std::option::Option::None,
                        auth: ::std::sync::Arc::new(auth::NoAuth),
                        body_hooks: ::std::vec::Vec::new(),
                    }
                }
            };
            let builder_fn = quote! {
                /// Start building a client; see [`ClientBuilder`].
                pub fn builder(
                    base_url: impl ::std::convert::Into<::std::string::String>,
                ) -> ClientBuilder {
                    ClientBuilder::new(base_url)
                }
            };
            (new, None, builder_fn)
        }
    }
}

/// One generated `pub async fn <operation>` method.
fn operation_method(operation: &OperationPlan) -> TokenStream {
    let method_name = &operation.method_name;
    let operation_id = &operation.operation_id;
    let http_method = &operation.http_method;
    let path = &operation.path;

    let doc = method_doc(operation);
    let params = operation.params.iter().map(param_signature);
    let body_param = operation.body.as_ref().map(|ty| quote! { body: &#ty, });
    let return_type = match &operation.response {
        Some(ty) => quote! { #ty },
        None => quote! { () },
    };

    let url = url_expression(operation);
    let query = query_statements(operation);
    let headers = header_statements(operation);
    let send_body = operation.body.as_ref().map(|_| {
        quote! {
            let mut body = ::serde_json::to_value(body).map_err(|source| Error::Serialize {
                op: OP.operation_id,
                source,
            })?;
            for hook in &self.body_hooks {
                hook(&OP, &mut body);
            }
            request = request.json(&body);
        }
    });
    let finish = match &operation.response {
        Some(_) => quote! { support::decode_json(OP.operation_id, response).await },
        None => quote! { support::expect_success(OP.operation_id, response).await },
    };

    let http_method_ident = quote::format_ident!("{http_method}");

    quote! {
        #[doc = #doc]
        pub async fn #method_name(
            &self,
            #(#params)*
            #body_param
        ) -> ::std::result::Result<#return_type, Error> {
            const OP: OperationInfo = OperationInfo {
                operation_id: #operation_id,
                method: #http_method,
                path: #path,
            };
            let url = #url;
            let mut request = self
                .client
                .request(::reqwest::Method::#http_method_ident, url);
            #query
            #headers
            request = self.auth.authorize(request, &OP).await?;
            #send_body
            let response = request.send().await.map_err(|source| Error::Request {
                op: OP.operation_id,
                source,
            })?;
            #finish
        }
    }
}

/// The method's doc text: spec summary/description plus a
/// `METHOD /path` line.
fn method_doc(operation: &OperationPlan) -> String {
    let wire_line = format!("`{} {}`", operation.http_method, operation.path);
    match &operation.doc {
        Some(doc) => format!("{doc}\n\n{wire_line}"),
        None => wire_line,
    }
}

/// One `name: type,` signature fragment.
fn param_signature(param: &ParamPlan) -> TokenStream {
    let name = &param.rust_name;
    let ty = match &param.ty {
        ParamType::String => quote! { &str },
        ParamType::Copy(ty) => quote! { #ty },
        ParamType::Display(ty) => quote! { &#ty },
    };
    if param.required {
        quote! { #name: #ty, }
    } else {
        quote! { #name: ::std::option::Option<#ty>, }
    }
}

/// The `format!(...)` expression building the request URL: the path
/// template with each `{param}` replaced by a percent-encoded argument.
fn url_expression(operation: &OperationPlan) -> TokenStream {
    let mut format_string = String::from("{}");
    let mut args: Vec<TokenStream> = Vec::new();

    let mut rest = operation.path.as_str();
    while let Some(start) = rest.find('{') {
        let (literal, after) = rest.split_at(start);
        format_string.push_str(&literal.replace('{', "{{").replace('}', "}}"));
        let after = &after[1..];
        let end = after.find('}').expect("template validated in plan resolution");
        let wire_name = &after[..end];
        let param = operation
            .params
            .iter()
            .find(|param| {
                param.location == ParamLocation::Path && param.wire_name == wire_name
            })
            .expect("template params validated in plan resolution");
        let name = &param.rust_name;
        format_string.push_str("{}");
        args.push(match &param.ty {
            ParamType::String => quote! { support::encode_path(#name) },
            ParamType::Copy(_) | ParamType::Display(_) => {
                quote! { support::encode_path(&#name.to_string()) }
            }
        });
        rest = &after[end + 1..];
    }
    format_string.push_str(&rest.replace('{', "{{").replace('}', "}}"));

    if args.is_empty() {
        quote! { ::std::format!(#format_string, self.base_url) }
    } else {
        quote! { ::std::format!(#format_string, self.base_url, #(#args),*) }
    }
}

/// Statements collecting query parameters and attaching them to the
/// request; empty when the operation has none.
fn query_statements(operation: &OperationPlan) -> TokenStream {
    let query_params: Vec<&ParamPlan> = operation
        .params
        .iter()
        .filter(|param| param.location == ParamLocation::Query)
        .collect();
    if query_params.is_empty() {
        return quote! {};
    }

    let pushes = query_params.iter().map(|param| {
        let name = &param.rust_name;
        let wire_name = &param.wire_name;
        if param.required {
            quote! { query.push((#wire_name, #name.to_string())); }
        } else {
            quote! {
                if let ::std::option::Option::Some(value) = #name {
                    query.push((#wire_name, value.to_string()));
                }
            }
        }
    });

    quote! {
        let mut query: ::std::vec::Vec<(&str, ::std::string::String)> =
            ::std::vec::Vec::new();
        #(#pushes)*
        if !query.is_empty() {
            request = request.query(&query);
        }
    }
}

/// Statements attaching non-auth header parameters.
fn header_statements(operation: &OperationPlan) -> TokenStream {
    let header_params: Vec<&ParamPlan> = operation
        .params
        .iter()
        .filter(|param| param.location == ParamLocation::Header)
        .collect();
    if header_params.is_empty() {
        return quote! {};
    }

    let sets = header_params.iter().map(|param| {
        let name = &param.rust_name;
        let wire_name = &param.wire_name;
        if param.required {
            let value = match &param.ty {
                ParamType::String => quote! { #name },
                ParamType::Copy(_) | ParamType::Display(_) => quote! { #name.to_string() },
            };
            quote! { request = request.header(#wire_name, #value); }
        } else {
            let value = match &param.ty {
                ParamType::String => quote! { value },
                ParamType::Copy(_) | ParamType::Display(_) => quote! { value.to_string() },
            };
            quote! {
                if let ::std::option::Option::Some(value) = #name {
                    request = request.header(#wire_name, #value);
                }
            }
        }
    });

    quote! { #(#sets)* }
}
