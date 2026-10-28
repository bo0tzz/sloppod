//! Cilium as the enforcer (ADR 11): per session, an Envoy listener whose `ext_authz` calls the runtime and names the
//! session, and a default-deny CiliumNetworkPolicy that redirects the session's intercepted hosts to it. The clusters
//! and the listeners' certificate are shared, in one CiliumEnvoyConfig the session ones refer to (#196).

use std::collections::BTreeMap;
use std::net::SocketAddr;

use async_trait::async_trait;
use kube::api::{
    Api, ApiResource, DeleteParams, DynamicObject, GroupVersionKind, Patch, PatchParams,
};
use serde_json::{Value, json};
use sloppod_contracts::{EgressSpec, Enforcer, Error, Manifest, Result};

use crate::tls::Ca;

/// The shared CiliumEnvoyConfig: the upstream and authorizer clusters, and the listeners' certificate.
const SHARED: &str = "sloppod-egress";
const CERT: &str = "sloppod-egress-cert";

/// A session's CiliumEnvoyConfig, and its one listener.
pub fn listener_name(session_id: &str) -> String {
    format!("sloppod-egress-{session_id}")
}

/// A resource in the shared config, as another CiliumEnvoyConfig names it: Cilium prefixes every resource with
/// `<namespace>/<config>/`, and leaves a name that already has slashes alone.
fn shared(namespace: &str, name: &str) -> String {
    format!("{namespace}/{SHARED}/{name}")
}

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

    /// Writes the shared config: the certificate the session listeners terminate TLS with, issued by the runtime's
    /// CA, the cluster their `ext_authz` reaches the runtime at `authz` by, and the one TLS is re-originated to the
    /// original destination by.
    pub async fn install_listener(
        &self,
        ca: &Ca,
        hosts: &[String],
        authz: SocketAddr,
        trusted_ca: &str,
    ) -> anyhow::Result<()> {
        let (cert, key) = ca.issue(hosts)?;
        let config = shared_config(&cert, &key, authz, trusted_ca);
        self.envoy_configs
            .patch(
                SHARED,
                &PatchParams::apply("sloppod").force(),
                &Patch::Apply(&config),
            )
            .await?;
        Ok(())
    }
}

/// The HTTP side of a session's listener: paths normalised, every request authorized by the runtime with the
/// session named in `context_extensions`, then sent on to its original destination.
fn connection_manager(namespace: &str, session_id: &str) -> Value {
    json!({
        "@type": "type.googleapis.com/envoy.extensions.filters.network.http_connection_manager.v3.HttpConnectionManager",
        "stat_prefix": listener_name(session_id),
        // One canonical path for the authorizer and the upstream alike: dot segments resolved and repeated slashes
        // merged before any policy looks, so no policy has to get that right itself. Escaped slashes are left as
        // sent.
        "normalize_path": true,
        "merge_slashes": true,
        "stream_idle_timeout": "900s",
        "request_timeout": "0s",
        "route_config": {
            "name": listener_name(session_id),
            "virtual_hosts": [{
                "name": "all",
                "domains": ["*"],
                "routes": [{
                    "match": {"prefix": "/"},
                    "route": {"cluster": shared(namespace, "sloppod-upstream"), "timeout": "0s"},
                }],
                // Which session this is: the listener is the session's own, chosen by Cilium from the session's
                // policy, so the pod can't pick another.
                "typed_per_filter_config": {
                    "envoy.filters.http.ext_authz": {
                        "@type": "type.googleapis.com/envoy.extensions.filters.http.ext_authz.v3.ExtAuthzPerRoute",
                        "check_settings": {"context_extensions": {"session": session_id}},
                    },
                },
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
                        // The authority must not contain the qualified name's slashes: HTTP/2 servers reject it.
                        "envoy_grpc": {
                            "cluster_name": shared(namespace, "sloppod-authz"),
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
    })
}

/// A session's CiliumEnvoyConfig: one listener, terminating TLS with the shared certificate.
fn session_config(namespace: &str, session_id: &str) -> Value {
    json!({
        "apiVersion": "cilium.io/v2",
        "kind": "CiliumEnvoyConfig",
        "metadata": {
            "name": listener_name(session_id),
            "labels": {"sloppod.dev/session": session_id},
            // Upstream connections come from Envoy itself, as in the ADR 11 spike.
            "annotations": {"cec.cilium.io/use-original-source-address": "false"},
        },
        "spec": {"resources": [{
            "@type": "type.googleapis.com/envoy.config.listener.v3.Listener",
            "name": listener_name(session_id),
            "filter_chains": [{
                "transport_socket": {
                    "name": "envoy.transport_sockets.tls",
                    "typed_config": {
                        "@type": "type.googleapis.com/envoy.extensions.transport_sockets.tls.v3.DownstreamTlsContext",
                        "common_tls_context": {
                            "alpn_protocols": ["http/1.1"],
                            "tls_certificate_sds_secret_configs": [{"name": shared(namespace, CERT)}],
                        },
                    },
                },
                "filters": [{
                    "name": "envoy.filters.network.http_connection_manager",
                    "typed_config": connection_manager(namespace, session_id),
                }],
            }],
        }]},
    })
}

/// The shared CiliumEnvoyConfig: the listeners' certificate and the clusters they send to.
fn shared_config(cert: &str, key: &str, authz: SocketAddr, trusted_ca: &str) -> Value {
    json!({
        "apiVersion": "cilium.io/v2",
        "kind": "CiliumEnvoyConfig",
        "metadata": {
            "name": SHARED,
            "annotations": {"cec.cilium.io/use-original-source-address": "false"},
        },
        "spec": {"resources": [
            {
                "@type": "type.googleapis.com/envoy.extensions.transport_sockets.tls.v3.Secret",
                "name": CERT,
                "tls_certificate": {
                    "certificate_chain": {"inline_string": cert},
                    "private_key": {"inline_string": key},
                },
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

/// A delete's outcome: done, or already gone.
fn gone_or_deleted<T>(result: kube::Result<T>) -> Result<()> {
    match result {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
        Err(e) => Err(err(e)),
    }
}

impl Cilium {
    async fn patch_policy(&self, session_id: &str, policy: Value) -> Result<()> {
        self.policies
            .patch(
                &policy_name(session_id),
                &PatchParams::apply("sloppod").force(),
                &Patch::Apply(&policy),
            )
            .await
            .map_err(err)?;
        Ok(())
    }
}

/// A session's CiliumNetworkPolicy: the pod is denied all egress but what `rules` (under `key`) allow, and all
/// ingress.
fn session_policy(
    session_id: &str,
    pod_labels: &BTreeMap<String, String>,
    key: &str,
    rules: Vec<Value>,
) -> Value {
    let mut policy = json!({
        "apiVersion": "cilium.io/v2",
        "kind": "CiliumNetworkPolicy",
        "metadata": {"name": policy_name(session_id), "labels": {"sloppod.dev/session": session_id}},
        "spec": {
            "endpointSelector": {"matchLabels": pod_labels},
            "enableDefaultDeny": {"egress": true, "ingress": true},
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
    policy["spec"][key] = Value::from(rules);
    policy
}

#[async_trait]
impl Enforcer for Cilium {
    fn manifest(&self) -> Manifest {
        Manifest::builtin("enforcer-cilium")
    }

    async fn apply(&self, egress: &EgressSpec) -> Result<()> {
        // The listener first, so the policy that redirects to it never points at nothing.
        self.envoy_configs
            .patch(
                &listener_name(&egress.session_id),
                &PatchParams::apply("sloppod").force(),
                &Patch::Apply(&session_config(&self.namespace, &egress.session_id)),
            )
            .await
            .map_err(err)?;
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
                    "listener": {
                        "envoyConfig": {"kind": "CiliumEnvoyConfig", "name": listener_name(&egress.session_id)},
                        "name": listener_name(&egress.session_id),
                    },
                }],
            }),
        ];
        if !egress.direct_hosts.is_empty() {
            rules.push(json!({
                "toFQDNs": fqdns(&egress.direct_hosts),
                "toPorts": [{"ports": [{"port": "443", "protocol": "TCP"}, {"port": "80", "protocol": "TCP"}]}],
            }));
        }
        self.patch_policy(
            &egress.session_id,
            session_policy(&egress.session_id, &egress.pod_labels, "egress", rules),
        )
        .await
    }

    async fn release(&self, session_id: &str, pod_labels: &BTreeMap<String, String>) -> Result<()> {
        // The pod may run on for a while after it's told to stop, so it's fenced in before its listener goes.
        let deny = vec![json!({"toEntities": ["all"]})];
        self.patch_policy(
            session_id,
            session_policy(session_id, pod_labels, "egressDeny", deny),
        )
        .await?;
        gone_or_deleted(
            self.envoy_configs
                .delete(&listener_name(session_id), &DeleteParams::default())
                .await,
        )
    }

    async fn remove(&self, session_id: &str) -> Result<()> {
        // The policy first, so nothing is redirected to a listener that's gone.
        gone_or_deleted(
            self.policies
                .delete(&policy_name(session_id), &DeleteParams::default())
                .await,
        )?;
        gone_or_deleted(
            self.envoy_configs
                .delete(&listener_name(session_id), &DeleteParams::default())
                .await,
        )
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
    fn a_session_listener_names_its_session_and_uses_the_shared_parts() {
        let config = session_config("slop", "s1");
        assert_eq!(config["metadata"]["name"], "sloppod-egress-s1");
        let chain = &config["spec"]["resources"][0]["filter_chains"][0];
        assert_eq!(
            chain["transport_socket"]["typed_config"]["common_tls_context"]["tls_certificate_sds_secret_configs"]
                [0]["name"],
            "slop/sloppod-egress/sloppod-egress-cert"
        );
        let manager = &chain["filters"][0]["typed_config"];
        assert_eq!(manager["normalize_path"], true);
        assert_eq!(manager["merge_slashes"], true);
        let host = &manager["route_config"]["virtual_hosts"][0];
        assert_eq!(
            host["routes"][0]["route"]["cluster"],
            "slop/sloppod-egress/sloppod-upstream"
        );
        assert_eq!(
            host["typed_per_filter_config"]["envoy.filters.http.ext_authz"]["check_settings"]["context_extensions"]
                ["session"],
            "s1"
        );
        assert_eq!(
            manager["http_filters"][0]["typed_config"]["grpc_service"]["envoy_grpc"]["cluster_name"],
            "slop/sloppod-egress/sloppod-authz"
        );
    }

    #[test]
    fn a_released_session_is_denied_all_egress() {
        let labels = BTreeMap::from([("sloppod.dev/session".to_owned(), "s1".to_owned())]);
        let policy = session_policy(
            "s1",
            &labels,
            "egressDeny",
            vec![json!({"toEntities": ["all"]})],
        );
        let spec = &policy["spec"];
        assert_eq!(policy["metadata"]["name"], policy_name("s1"));
        assert_eq!(
            spec["endpointSelector"]["matchLabels"]["sloppod.dev/session"],
            "s1"
        );
        assert_eq!(spec["enableDefaultDeny"]["egress"], true);
        assert_eq!(spec["egressDeny"], json!([{"toEntities": ["all"]}]));
        // Nothing allowed, and no listener left to redirect to.
        assert!(spec.get("egress").is_none());
        assert!(!policy.to_string().contains("listener"));
    }

    #[test]
    fn the_shared_config_has_no_listener_of_its_own() {
        let config = shared_config("cert", "key", "127.0.0.1:9001".parse().unwrap(), "/ca.crt");
        let resources = config["spec"]["resources"].as_array().unwrap();
        assert!(
            resources
                .iter()
                .all(|r| !r["@type"].as_str().unwrap().ends_with(".Listener"))
        );
        assert!(resources.iter().any(|r| r["name"] == "sloppod-egress-cert"));
    }

    #[test]
    fn upstream_connections_never_resume_tls_sessions() {
        let config = shared_config("cert", "key", "127.0.0.1:9001".parse().unwrap(), "/ca.crt");
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
}
