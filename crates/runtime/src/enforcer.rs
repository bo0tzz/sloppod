//! Cilium as the enforcer (ADR 11): a shared Envoy listener whose `ext_authz` calls the runtime, and a
//! default-deny CiliumNetworkPolicy per session that redirects intercepted hosts to it.

use std::net::SocketAddr;

use async_trait::async_trait;
use kube::api::{
    Api, ApiResource, DeleteParams, DynamicObject, GroupVersionKind, Patch, PatchParams,
};
use serde_json::{Value, json};
use sloppod_contracts::{EgressSpec, Enforcer, Error, Manifest, Result};

use crate::tls::Ca;

const LISTENER: &str = "sloppod-egress";

/// The longest a git push through the listener may take, start to end. Work that needs a branch to hold still can
/// count on a push the authorizer let through having landed or failed once this has passed.
pub const PUSH_TIME_LIMIT: std::time::Duration = std::time::Duration::from_secs(5 * 60);

pub struct Cilium {
    namespace: String,
    policies: Api<DynamicObject>,
    envoy_configs: Api<DynamicObject>,
    runtime_labels: Vec<(String, String)>,
    runtime_port: u16,
    cluster_domain: String,
}

fn api(client: &kube::Client, namespace: &str, kind: &str, plural: &str) -> Api<DynamicObject> {
    let gvk = GroupVersionKind::gvk("cilium.io", "v2", kind);
    Api::namespaced_with(
        client.clone(),
        namespace,
        &ApiResource::from_gvk_with_plural(&gvk, plural),
    )
}

fn err(e: impl std::fmt::Display) -> Error {
    Error::new(e.to_string())
}

impl Cilium {
    pub fn new(
        client: &kube::Client,
        namespace: &str,
        runtime_labels: Vec<(String, String)>,
        runtime_port: u16,
        cluster_domain: &str,
    ) -> Self {
        Self {
            namespace: namespace.to_owned(),
            policies: api(
                client,
                namespace,
                "CiliumNetworkPolicy",
                "ciliumnetworkpolicies",
            ),
            envoy_configs: api(client, namespace, "CiliumEnvoyConfig", "ciliumenvoyconfigs"),
            runtime_labels,
            runtime_port,
            cluster_domain: cluster_domain.to_owned(),
        }
    }

    /// Writes the shared listener: TLS terminated with a certificate from the runtime's CA, `ext_authz`
    /// to the runtime at `authz`, and TLS re-originated to the original destination.
    pub async fn install_listener(
        &self,
        ca: &Ca,
        hosts: &[String],
        authz: SocketAddr,
        trusted_ca: &str,
    ) -> anyhow::Result<()> {
        let (cert, key) = ca.issue(hosts)?;
        let config = envoy_config(&self.namespace, &cert, &key, authz, trusted_ca);
        self.envoy_configs
            .patch(
                LISTENER,
                &PatchParams::apply("sloppod").force(),
                &Patch::Apply(&config),
            )
            .await?;
        Ok(())
    }
}

fn envoy_config(
    namespace: &str,
    cert: &str,
    key: &str,
    authz: SocketAddr,
    trusted_ca: &str,
) -> Value {
    json!({
        "apiVersion": "cilium.io/v2",
        "kind": "CiliumEnvoyConfig",
        "metadata": {
            "name": LISTENER,
            // Upstream connections come from Envoy itself, as in the ADR 11 spike.
            "annotations": {"cec.cilium.io/use-original-source-address": "false"},
        },
        "spec": {"resources": [
            {
                "@type": "type.googleapis.com/envoy.config.listener.v3.Listener",
                "name": LISTENER,
                "filter_chains": [{
                    "transport_socket": {
                        "name": "envoy.transport_sockets.tls",
                        "typed_config": {
                            "@type": "type.googleapis.com/envoy.extensions.transport_sockets.tls.v3.DownstreamTlsContext",
                            "common_tls_context": {
                                "alpn_protocols": ["http/1.1"],
                                "tls_certificates": [{
                                    "certificate_chain": {"inline_string": cert},
                                    "private_key": {"inline_string": key},
                                }],
                            },
                        },
                    },
                    "filters": [{
                        "name": "envoy.filters.network.http_connection_manager",
                        "typed_config": {
                            "@type": "type.googleapis.com/envoy.extensions.filters.network.http_connection_manager.v3.HttpConnectionManager",
                            "stat_prefix": LISTENER,
                            "stream_idle_timeout": "900s",
                            "request_timeout": "0s",
                            "route_config": {
                                "name": LISTENER,
                                "virtual_hosts": [{
                                    "name": "all",
                                    "domains": ["*"],
                                    "routes": [
                                        {
                                            "match": {"safe_regex": {"regex": ".*/git-receive-pack"}},
                                            "route": {
                                                "cluster": "sloppod-upstream",
                                                "timeout": "0s",
                                                "max_stream_duration": {
                                                    "max_stream_duration": format!("{}s", PUSH_TIME_LIMIT.as_secs()),
                                                },
                                            },
                                        },
                                        {
                                            "match": {"prefix": "/"},
                                            "route": {"cluster": "sloppod-upstream", "timeout": "0s"},
                                        },
                                    ],
                                }],
                            },
                            "http_filters": [
                                {
                                    "name": "envoy.filters.http.ext_authz",
                                    "typed_config": {
                                        "@type": "type.googleapis.com/envoy.extensions.filters.http.ext_authz.v3.ExtAuthz",
                                        "transport_api_version": "V3",
                                        "failure_mode_allow": false,
                                        "status_on_error": {"code": 503},
                                        "with_request_body": {
                                            "max_request_bytes": 65536,
                                            "allow_partial_message": true,
                                            "pack_as_bytes": true,
                                        },
                                        "grpc_service": {
                                            // Cilium prefixes resource names with `<namespace>/<config>/`, but only
                                            // rewrites the references it knows about. The authority must not
                                            // contain those slashes: HTTP/2 servers reject it.
                                            "envoy_grpc": {
                                                "cluster_name": format!("{namespace}/{LISTENER}/sloppod-authz"),
                                                "authority": "sloppod-authz",
                                            },
                                            "timeout": "10s",
                                        },
                                    },
                                },
                                {
                                    "name": "envoy.filters.http.router",
                                    "typed_config": {"@type": "type.googleapis.com/envoy.extensions.filters.http.router.v3.Router"},
                                },
                            ],
                        },
                    }],
                }],
            },
            {
                "@type": "type.googleapis.com/envoy.config.cluster.v3.Cluster",
                "name": "sloppod-upstream",
                "type": "ORIGINAL_DST",
                "connect_timeout": "10s",
                "lb_policy": "CLUSTER_PROVIDED",
                "typed_extension_protocol_options": {
                    "envoy.extensions.upstreams.http.v3.HttpProtocolOptions": {
                        "@type": "type.googleapis.com/envoy.extensions.upstreams.http.v3.HttpProtocolOptions",
                        "upstream_http_protocol_options": {"auto_sni": true, "auto_san_validation": true},
                        "explicit_http_config": {"http_protocol_options": {}},
                    },
                },
                "transport_socket": {
                    "name": "envoy.transport_sockets.tls",
                    "typed_config": {
                        "@type": "type.googleapis.com/envoy.extensions.transport_sockets.tls.v3.UpstreamTlsContext",
                        "common_tls_context": {"validation_context": {"trusted_ca": {"filename": trusted_ca}}},
                        // One context serves every upstream, and its session cache isn't keyed by SNI: a resumed
                        // session can carry another host's certificate, which then fails the SAN check.
                        "max_session_keys": 0,
                    },
                },
            },
            {
                "@type": "type.googleapis.com/envoy.config.cluster.v3.Cluster",
                "name": "sloppod-authz",
                "type": "STATIC",
                "connect_timeout": "5s",
                "typed_extension_protocol_options": {
                    "envoy.extensions.upstreams.http.v3.HttpProtocolOptions": {
                        "@type": "type.googleapis.com/envoy.extensions.upstreams.http.v3.HttpProtocolOptions",
                        "explicit_http_config": {"http2_protocol_options": {}},
                    },
                },
                "load_assignment": {
                    "cluster_name": "sloppod-authz",
                    "endpoints": [{"lb_endpoints": [{"endpoint": {"address": {"socket_address": {
                        "address": authz.ip().to_string(),
                        "port_value": authz.port(),
                    }}}}]}],
                },
            },
        ]},
    })
}

pub fn policy_name(session_id: &str) -> String {
    format!("sloppod-session-{session_id}")
}

/// A Cilium `toFQDNs` selector; hosts with a `*` are patterns.
fn fqdn_selector(host: &str) -> Value {
    if host.contains('*') {
        json!({"matchPattern": host})
    } else {
        json!({"matchName": host})
    }
}

/// Names a session may look up: the hosts it may reach, and in-cluster names, which cluster DNS answers itself. The
/// latter cover the runtime's Service and the resolver's search-list expansions (`github.com.<ns>.svc.cluster.local`),
/// which have to come back NXDOMAIN rather than refused, or resolvers give up before trying the name as given.
fn dns_rules(egress: &EgressSpec, cluster_domain: &str) -> Vec<Value> {
    egress
        .intercepted_hosts
        .iter()
        .chain(&egress.direct_hosts)
        .map(|h| fqdn_selector(h))
        .chain([json!({"matchPattern": format!("**.{cluster_domain}")})])
        .collect()
}

#[async_trait]
impl Enforcer for Cilium {
    fn manifest(&self) -> Manifest {
        Manifest::builtin("enforcer-cilium")
    }

    async fn apply(&self, egress: &EgressSpec) -> Result<()> {
        let fqdns = |hosts: &[String]| hosts.iter().map(|h| fqdn_selector(h)).collect::<Vec<_>>();
        let runtime: serde_json::Map<String, Value> = self
            .runtime_labels
            .iter()
            .map(|(k, v)| (k.clone(), json!(v)))
            .collect();
        let mut rules = vec![
            json!({
                "toEndpoints": [{"matchLabels": {"k8s:io.kubernetes.pod.namespace": "kube-system", "k8s-app": "kube-dns"}}],
                "toPorts": [{"ports": [{"port": "53", "protocol": "ANY"}], "rules": {"dns": dns_rules(egress, &self.cluster_domain)}}],
            }),
            json!({
                "toEndpoints": [{"matchLabels": runtime}],
                "toPorts": [{"ports": [{"port": self.runtime_port.to_string(), "protocol": "TCP"}]}],
            }),
            json!({
                "toFQDNs": fqdns(&egress.intercepted_hosts),
                "toPorts": [{
                    "ports": [{"port": "443", "protocol": "TCP"}],
                    "listener": {"envoyConfig": {"kind": "CiliumEnvoyConfig", "name": LISTENER}, "name": LISTENER},
                }],
            }),
        ];
        if !egress.direct_hosts.is_empty() {
            rules.push(json!({
                "toFQDNs": fqdns(&egress.direct_hosts),
                "toPorts": [{"ports": [{"port": "443", "protocol": "TCP"}, {"port": "80", "protocol": "TCP"}]}],
            }));
        }
        let policy = json!({
            "apiVersion": "cilium.io/v2",
            "kind": "CiliumNetworkPolicy",
            "metadata": {"name": policy_name(&egress.session_id), "labels": {"sloppod.dev/session": egress.session_id}},
            "spec": {
                "endpointSelector": {"matchLabels": egress.pod_labels},
                "enableDefaultDeny": {"egress": true, "ingress": true},
                "egress": rules,
                // With Cilium's `policyDenyResponse: icmp`, refused connections get a Destination Unreachable that
                // has to pass ingress policy; otherwise they still hang until the client times out.
                "ingress": [{
                    "fromEntities": ["all"],
                    "icmps": [{"fields": [
                        {"family": "IPv4", "type": "DestinationUnreachable"},
                        {"family": "IPv6", "type": "DestinationUnreachable"},
                    ]}],
                }],
            },
        });
        self.policies
            .patch(
                &policy_name(&egress.session_id),
                &PatchParams::apply("sloppod").force(),
                &Patch::Apply(&policy),
            )
            .await
            .map_err(err)?;
        Ok(())
    }

    async fn remove(&self, session_id: &str) -> Result<()> {
        match self
            .policies
            .delete(&policy_name(session_id), &DeleteParams::default())
            .await
        {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
            Err(e) => Err(err(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_names_and_patterns() {
        assert_eq!(
            fqdn_selector("crates.io"),
            json!({"matchName": "crates.io"})
        );
        assert_eq!(
            fqdn_selector("productionresultssa*.blob.core.windows.net"),
            json!({"matchPattern": "productionresultssa*.blob.core.windows.net"})
        );
    }

    #[test]
    fn resolves_only_reachable_and_cluster_names() {
        let egress = EgressSpec {
            session_id: "s".into(),
            pod_labels: Default::default(),
            intercepted_hosts: vec!["api.anthropic.com".into()],
            direct_hosts: vec!["crates.io".into()],
        };
        assert_eq!(
            dns_rules(&egress, "cluster.local"),
            vec![
                json!({"matchName": "api.anthropic.com"}),
                json!({"matchName": "crates.io"}),
                json!({"matchPattern": "**.cluster.local"}),
            ]
        );
    }

    #[test]
    fn upstream_connections_never_resume_tls_sessions() {
        let config = envoy_config(
            "slop",
            "cert",
            "key",
            "127.0.0.1:9001".parse().unwrap(),
            "/ca.crt",
        );
        let upstream = config["spec"]["resources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == "sloppod-upstream")
            .unwrap();
        assert_eq!(
            upstream["transport_socket"]["typed_config"]["max_session_keys"],
            0
        );
    }
    #[test]
    fn pushes_have_a_time_limit_and_nothing_else_does() {
        let config = envoy_config(
            "slop",
            "cert",
            "key",
            "127.0.0.1:9001".parse().unwrap(),
            "/ca.crt",
        );
        let listener = config["spec"]["resources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == LISTENER)
            .unwrap();
        let routes = &listener["filter_chains"][0]["filters"][0]["typed_config"]["route_config"]["virtual_hosts"]
            [0]["routes"];
        assert_eq!(
            routes[0]["match"]["safe_regex"]["regex"],
            ".*/git-receive-pack"
        );
        assert_eq!(
            routes[0]["route"]["max_stream_duration"]["max_stream_duration"],
            "300s"
        );
        assert_eq!(routes[1]["match"]["prefix"], "/");
        assert!(routes[1]["route"].get("max_stream_duration").is_none());
    }
}
