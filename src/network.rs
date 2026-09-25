use crate::config::{Config, JAIL_BASE};
use crate::jail;
use anyhow::{bail, Context, Result};
use std::fs;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

const NETWORK_JAIL: &str = "network";
const UNBOUND_PATH: &str = "/var/unbound";
const JAIL_UNBOUND_PATH: &str = "var/unbound";

pub fn init(config: &Config) -> Result<()> {
    if !config.use_ipv4 && !config.use_ipv6 {
        bail!("use_ipv4 or use_ipv6 must be enabled");
    }
    configure_host_network(config)?;
    let root = Path::new(JAIL_BASE).join(NETWORK_JAIL);
    if !jail::path_exists(NETWORK_JAIL, config) {
        jail::create(NETWORK_JAIL, None, None, false, None, None, config)?;
    }

    install_packages(&root)?;
    configure_network(&root, config)?;
    configure_services(&root, config)?;
    mount_host_unbound(&root)?;
    jail::start(NETWORK_JAIL, config)?;
    configure_host_unbound(config)
}

fn mount_host_unbound(root: &Path) -> Result<()> {
    let host_path = Path::new(UNBOUND_PATH);
    let jail_path = root.join(JAIL_UNBOUND_PATH);
    fs::create_dir_all(host_path)?;
    fs::create_dir_all(&jail_path)?;
    crate::util::cmd::message(&format!(
        "Mounting {UNBOUND_PATH} at {}",
        jail_path.display()
    ));
    crate::util::cmd::run(
        "mount",
        &[
            "-t",
            "nullfs",
            &host_path.display().to_string(),
            &jail_path.display().to_string(),
        ],
    )?;
    Ok(())
}

fn configure_host_network(config: &Config) -> Result<()> {
    let mut members = String::new();
    for member in &config.bridge_members {
        members.push_str(&format!(" addm {member}"));
    }
    let mut arguments = vec![
        "cloned_interfaces+=bridge0".to_string(),
        format!("ifconfig_bridge0_name={}", config.bridge),
    ];
    if config.use_ipv4 {
        arguments.push(format!(
            "ifconfig_{}=inet {} netmask 255.255.255.0{}",
            config.bridge, config.bridge_ip, members
        ));
    }
    if config.use_ipv6 {
        arguments.push(format!(
            "ifconfig_{}_ipv6=inet6 -ifdisabled auto_linklocal {}{}",
            config.bridge, config.ipv6_prefix, config.bridge_ip6
        ));
    }
    let arguments = arguments.iter().map(String::as_str).collect::<Vec<_>>();
    crate::util::cmd::run("sysrc", &arguments)?;
    crate::util::cmd::run("service", &["netif", "cloneup"])?;
    Ok(())
}

fn install_packages(root: &Path) -> Result<()> {
    let root = root.display().to_string();
    crate::util::cmd::message("Installing Kea DHCP and Knot DNS");
    crate::util::cmd::run("pkg", &["-r", &root, "install", "-y", "kea", "knot3"])?;
    Ok(())
}

fn configure_network(root: &Path, config: &Config) -> Result<()> {
    let root_path = root.display().to_string();
    let mut arguments = vec!["-R".to_string(), root_path];
    if config.use_ipv4 {
        let network_ip = config
            .network_ip
            .parse::<Ipv4Addr>()
            .with_context(|| format!("invalid network_ip: {}", config.network_ip))?;
        let bridge_ip = config
            .bridge_ip
            .parse::<Ipv4Addr>()
            .with_context(|| format!("invalid bridge_ip: {}", config.bridge_ip))?;
        arguments.push(format!(
            "ifconfig_eth0=inet {network_ip} netmask 255.255.255.0"
        ));
        arguments.push(format!("defaultrouter={bridge_ip}"));
    } else {
        arguments.push("ifconfig_eth0=NONE".to_string());
    }
    if config.use_ipv6 {
        arguments.push(format!(
            "ifconfig_eth0_ipv6=inet6 {prefix}{network_ip6}/64",
            prefix = config.ipv6_prefix,
            network_ip6 = config.network_ip6
        ));
        arguments.push(format!(
            "ipv6_defaultrouter={prefix}{bridge_ip6}",
            prefix = config.ipv6_prefix,
            bridge_ip6 = config.bridge_ip6
        ));
    } else {
        arguments.push("ifconfig_eth0_ipv6=NONE".to_string());
    }
    let arguments = arguments.iter().map(String::as_str).collect::<Vec<_>>();
    crate::util::cmd::run("sysrc", &arguments)?;
    Ok(())
}

fn configure_services(root: &Path, config: &Config) -> Result<()> {
    let domain = domain(config)?;
    let hostname = host_hostname()?;
    let network_ip = config.network_ip.parse::<Ipv4Addr>()?;
    let bridge_ip = config.bridge_ip.parse::<Ipv4Addr>()?;
    let network_ip6 =
        format!("{}{}", config.ipv6_prefix, config.network_ip6).parse::<Ipv6Addr>()?;
    let ddns_key = generate_ddns_key()?;
    let octets = bridge_ip.octets();
    let subnet = format!("{}.{}.{}.0/24", octets[0], octets[1], octets[2]);
    let pool_start = format!("{}.{}.{}.100", octets[0], octets[1], octets[2]);
    let pool_end = format!("{}.{}.{}.200", octets[0], octets[1], octets[2]);
    let service_dir = root.join("usr/local/etc");
    let kea_dir = service_dir.join("kea");
    let knot_dir = service_dir.join("knot");
    fs::create_dir_all(&kea_dir)?;
    fs::create_dir_all(&knot_dir)?;
    fs::create_dir_all(root.join("var/db/knot"))?;
    let kea_key = kea_dir.join("ddns.key");
    fs::write(&kea_key, format!("{ddns_key}\n"))?;
    fs::set_permissions(&kea_key, fs::Permissions::from_mode(0o600))?;
    let knot_key = knot_dir.join("ddns-key.conf");
    fs::write(
        &knot_key,
        format!("key:\n  - id: ddns\n    algorithm: hmac-sha256\n    secret: \"{ddns_key}\"\n"),
    )?;
    fs::set_permissions(knot_key, fs::Permissions::from_mode(0o600))?;
    let hook_script = root.join("usr/local/share/kea/scripts/priosun-unbound-flush");
    fs::create_dir_all(hook_script.parent().expect("hook script has a parent"))?;
    fs::write(&hook_script, unbound_flush_hook(&domain))?;
    fs::set_permissions(&hook_script, fs::Permissions::from_mode(0o700))?;
    let hook_config = r#"
    "hooks-libraries": [{
      "library": "/usr/local/lib/kea/hooks/libdhcp_run_script.so",
      "parameters": {
        "name": "/usr/local/share/kea/scripts/priosun-unbound-flush",
        "sync": false
      }
    }],"#;
    fs::write(
        kea_dir.join("kea-dhcp4.conf"),
        format!(
            r#"{{
    "Dhcp4": {{
    "interfaces-config": {{ "interfaces": [ "eth0" ] }},
    {hook_config}
    "match-client-id": false,
    "lease-database": {{ "type": "memfile", "persist": true }},
    "valid-lifetime": 3600,
    "ddns-send-updates": true,
    "ddns-update-on-renew": true,
    "ddns-override-no-update": true,
    "ddns-override-client-update": true,
    "ddns-qualifying-suffix": "{domain}",
    "dhcp-ddns": {{ "enable-updates": true }},
    "subnet4": [{{
      "id": 1,
      "subnet": "{subnet}",
      "pools": [{{ "pool": "{pool_start}-{pool_end}" }}],
      "option-data": [
        {{ "name": "routers", "data": "{bridge_ip}" }},
        {{ "name": "domain-name-servers", "data": "{bridge_ip}" }},
        {{ "name": "domain-name", "data": "{hostname}" }}
      ]
    }}],
    "loggers": [{{
      "name": "kea-dhcp4",
      "output-options": [{{
        "output": "kea-dhcp4.log",
        "flush": true
      }}],
      "severity": "INFO"
    }}]
  }}
}}
"#
        ),
    )?;
    let bridge_ip6 = format!("{}{}", config.ipv6_prefix, config.bridge_ip6);
    fs::write(
        kea_dir.join("kea-dhcp6.conf"),
        format!(
            r#"{{
    "Dhcp6": {{
    "interfaces-config": {{ "interfaces": [ "eth0" ] }},
    {hook_config}
    "lease-database": {{ "type": "memfile", "persist": true }},
    "valid-lifetime": 3600,
    "ddns-send-updates": true,
    "ddns-update-on-renew": true,
    "ddns-override-no-update": true,
    "ddns-override-client-update": true,
    "ddns-qualifying-suffix": "{domain}",
    "dhcp-ddns": {{ "enable-updates": true }},
    "subnet6": [{{
      "id": 1,
      "subnet": "{prefix}/64",
      "pools": [{{ "pool": "{prefix}:100-{prefix}:ffff" }}],
      "option-data": [{{ "name": "dns-servers", "data": "{bridge_ip6}" }}]
    }}],
    "loggers": [{{
      "name": "kea-dhcp6",
      "output-options": [{{
        "output": "kea-dhcp6.log",
        "flush": true
      }}],
      "severity": "INFO"
    }}]
  }}
}}
"#,
            prefix = config.ipv6_prefix.trim_end_matches(':'),
            bridge_ip6 = bridge_ip6
        ),
    )?;
    let listeners = [
        config.use_ipv4.then_some(network_ip.to_string()),
        config
            .use_ipv6
            .then_some(format!("{}{}", config.ipv6_prefix, config.network_ip6)),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    let mut zone = format!(
        "$ORIGIN {domain}.\n$TTL 3600\n@ SOA network.{domain}. hostmaster.{domain}. ( 1 1h 15m 1w 1h )\n@ NS network.{domain}.\n"
    );
    if config.use_ipv4 {
        zone.push_str(&format!("@ A {bridge_ip}\nnetwork A {network_ip}\n"));
    }
    if config.use_ipv6 {
        zone.push_str(&format!(
            "@ AAAA {}{}\nnetwork AAAA {}{}\n",
            config.ipv6_prefix, config.bridge_ip6, config.ipv6_prefix, config.network_ip6
        ));
    }
    let reverse_zone = ipv4_reverse_zone(network_ip);
    let reverse_zone6 = ipv6_reverse_zone(network_ip6);
    let mut reverse_domains = Vec::new();
    if config.use_ipv4 {
        reverse_domains.push(format!(
            "{{ \"name\": \"{reverse_zone}.\", \"key-name\": \"ddns\", \"dns-servers\": [{{ \"ip-address\": \"127.0.0.1\" }}] }}"
        ));
    }
    if config.use_ipv6 {
        reverse_domains.push(format!(
            "{{ \"name\": \"{reverse_zone6}.\", \"key-name\": \"ddns\", \"dns-servers\": [{{ \"ip-address\": \"::1\" }}] }}"
        ));
    }
    fs::write(
        kea_dir.join("kea-dhcp-ddns.conf"),
        format!(
            r#"{{
  "DhcpDdns": {{
    "control-socket": {{ "socket-type": "unix", "socket-name": "kea-ddns-ctrl-socket" }},
    "tsig-keys": [{{ "name": "ddns", "algorithm": "hmac-sha256", "secret-file": "/usr/local/etc/kea/ddns.key" }}],
    "forward-ddns": {{ "ddns-domains": [{{ "name": "{domain}.", "key-name": "ddns", "dns-servers": [{{ "ip-address": "127.0.0.1" }}] }}] }},
    "reverse-ddns": {{ "ddns-domains": [{reverse_domains}] }},
    "loggers": [{{ "name": "kea-dhcp-ddns", "output-options": [{{ "output": "kea-ddns.log", "flush": true }}], "severity": "INFO" }}]
  }}
}}
"#,
            reverse_domains = reverse_domains.join(", "),
        ),
    )?;
    let mut knot_zones =
        format!("  - domain: {domain}.\n    file: {domain}.zone\n    acl: ddns_acl\n");
    if config.use_ipv4 {
        knot_zones.push_str(&format!(
            "  - domain: {reverse_zone}.\n    file: {reverse_zone}.zone\n    acl: ddns_acl\n"
        ));
    }
    if config.use_ipv6 {
        knot_zones.push_str(&format!(
            "  - domain: {reverse_zone6}.\n    file: {reverse_zone6}.zone\n    acl: ddns_acl\n"
        ));
    }
    fs::write(
        knot_dir.join("knot.conf"),
        format!(
            "include: /usr/local/etc/knot/ddns-key.conf\n\nserver:\n  listen: [ 127.0.0.1@53, {} ]\n\nacl:\n  - id: ddns_acl\n    key: ddns\n    action: update\n\nlog:\n  - target: syslog\n    any: info\n\ndatabase:\n  storage: /var/db/knot\n\ntemplate:\n  - id: default\n    storage: /var/db/knot\n    file: %s.zone\n\nzone:\n{knot_zones}",
            listeners.iter().map(|ip| format!("{ip}@53")).collect::<Vec<_>>().join(", "),
        ),
    )?;
    fs::write(
        root.join("var/db/knot").join(format!("{domain}.zone")),
        zone,
    )?;
    if config.use_ipv4 {
        fs::write(
            root.join("var/db/knot").join(format!("{reverse_zone}.zone")),
            format!(
                "$ORIGIN {reverse_zone}.\n$TTL 3600\n@ SOA network.{domain}. hostmaster.{domain}. ( 1 1h 15m 1w 1h )\n@ NS network.{domain}.\n"
            ),
        )?;
    }
    if config.use_ipv6 {
        fs::write(
            root.join("var/db/knot").join(format!("{reverse_zone6}.zone")),
            format!(
                "$ORIGIN {reverse_zone6}.\n$TTL 3600\n@ SOA network.{domain}. hostmaster.{domain}. ( 1 1h 15m 1w 1h )\n@ NS network.{domain}.\n"
            ),
        )?;
    }
    let root_path = root.display().to_string();
    fs::write(
        root.join("usr/local/etc/kea/keactrl.conf"),
        format!(
            "prefix=\"/usr/local\"\n\
exec_prefix=\"/usr/local\"\n\
kea_dhcp4_config_file=\"/usr/local/etc/kea/kea-dhcp4.conf\"\n\
kea_dhcp6_config_file=\"/usr/local/etc/kea/kea-dhcp6.conf\"\n\
kea_dhcp_ddns_config_file=\"/usr/local/etc/kea/kea-dhcp-ddns.conf\"\n\
kea_ctrl_agent_config_file=\"/usr/local/etc/kea/kea-ctrl-agent.conf\"\n\
kea_netconf_config_file=\"/usr/local/etc/kea/kea-netconf.conf\"\n\
dhcp4_srv=\"/usr/local/sbin/kea-dhcp4\"\n\
dhcp6_srv=\"/usr/local/sbin/kea-dhcp6\"\n\
dhcp_ddns_srv=\"/usr/local/sbin/kea-dhcp-ddns\"\n\
ctrl_agent_srv=\"/usr/local/sbin/kea-ctrl-agent\"\n\
netconf_srv=\"/usr/local/sbin/kea-netconf\"\n\
dhcp4={}\ndhcp6={}\ndhcp_ddns=yes\n\
ctrl_agent=no\nnetconf=no\nkea_verbose=no\n",
            if config.use_ipv4 { "yes" } else { "no" },
            if config.use_ipv6 { "yes" } else { "no" }
        ),
    )?;
    crate::util::cmd::run(
        "sysrc",
        &["-R", &root_path, "kea_enable=YES", "knot_enable=YES"],
    )?;
    Ok(())
}

fn unbound_flush_hook(domain: &str) -> String {
    r#"#!/bin/sh

domain="__DOMAIN__"

flush_name() {
    if [ -n "$1" ]; then
        /usr/sbin/local-unbound-control flush "$1" >/dev/null 2>&1
    fi
}

forward_name() {
    case "$1" in
        "") ;;
        *.*) printf '%s\n' "$1" ;;
        *) printf '%s.%s\n' "$1" "$domain" ;;
    esac
}

reverse_ipv4() {
    printf '%s\n' "$1" | awk -F. 'NF == 4 { print $4 "." $3 "." $2 "." $1 ".in-addr.arpa." }'
}

reverse_ipv6() {
    printf '%s\n' "$1" | awk '
        function padded(value) {
            while (length(value) < 4) value = "0" value
            return value
        }
        function reverse(value, result, i) {
            for (i = length(value); i > 0; i--) result = result substr(value, i, 1)
            return result ".ip6.arpa."
        }
        index($0, ":") {
            count = split($0, halves, "::")
            left_count = 0
            right_count = 0
            if (halves[1] != "") left_count = split(halves[1], left, ":")
            if (count == 2 && halves[2] != "") right_count = split(halves[2], right, ":")
            expanded = ""
            for (i = 1; i <= left_count; i++) expanded = expanded padded(left[i])
            for (i = 0; i < 8 - left_count - right_count; i++) expanded = expanded "0000"
            for (i = 1; i <= right_count; i++) expanded = expanded padded(right[i])
            if (length(expanded) == 32) print reverse(expanded)
        }
    '
}

reverse_name() {
    case "$1" in
        *:*) reverse_ipv6 "$1" ;;
        *.*) reverse_ipv4 "$1" ;;
    esac
}

flush_lease() {
    fqdn=$(forward_name "$2")
    reverse=$(reverse_name "$1")
    flush_name "$fqdn"
    flush_name "$reverse"
}

flush_committed_v4() {
    i=0
    while [ "$i" -lt "${LEASES4_SIZE:-0}" ]; do
        eval "address=\${LEASES4_AT${i}_ADDRESS-}"
        eval "hostname=\${LEASES4_AT${i}_HOSTNAME-}"
        flush_lease "$address" "$hostname"
        i=$((i + 1))
    done
    i=0
    while [ "$i" -lt "${DELETED_LEASES4_SIZE:-0}" ]; do
        eval "address=\${DELETED_LEASE4_AT${i}_ADDRESS-}"
        eval "hostname=\${DELETED_LEASE4_AT${i}_HOSTNAME-}"
        flush_lease "$address" "$hostname"
        i=$((i + 1))
    done
}

flush_committed_v6() {
    i=0
    while [ "$i" -lt "${LEASES6_SIZE:-0}" ]; do
        eval "address=\${LEASES6_AT${i}_ADDRESS-}"
        eval "hostname=\${LEASES6_AT${i}_HOSTNAME-}"
        flush_lease "$address" "$hostname"
        i=$((i + 1))
    done
    i=0
    while [ "$i" -lt "${DELETED_LEASES6_SIZE:-0}" ]; do
        eval "address=\${DELETED_LEASE6_AT${i}_ADDRESS-}"
        eval "hostname=\${DELETED_LEASE6_AT${i}_HOSTNAME-}"
        flush_lease "$address" "$hostname"
        i=$((i + 1))
    done
}

case "$1" in
    leases4_committed) flush_committed_v4 ;;
    leases6_committed) flush_committed_v6 ;;
    lease4_release|lease4_expire|lease4_decline) flush_lease "$LEASE4_ADDRESS" "$LEASE4_HOSTNAME" ;;
    lease6_release|lease6_expire|lease6_decline) flush_lease "$LEASE6_ADDRESS" "$LEASE6_HOSTNAME" ;;
    addr6_register)
        flush_lease "$NEW_LEASE6_ADDRESS" "$NEW_LEASE6_HOSTNAME"
        flush_lease "$OLD_LEASE6_ADDRESS" "$OLD_LEASE6_HOSTNAME"
        ;;
esac
"#
    .replace("__DOMAIN__", domain)
}

fn generate_ddns_key() -> Result<String> {
    let output = crate::util::cmd::run("openssl", &["rand", "-base64", "32"])?;
    let key = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .collect::<String>();
    if key.is_empty() {
        bail!("openssl generated an empty DDNS key");
    }
    Ok(key)
}

fn domain(config: &Config) -> Result<String> {
    if !config.domain.trim().is_empty() {
        return Ok(config.domain.trim().to_string());
    }
    host_hostname()
}

fn host_hostname() -> Result<String> {
    let output = Command::new("hostname").output()?;
    if !output.status.success() {
        bail!("failed to read system hostname");
    }
    let hostname = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if hostname.is_empty() {
        bail!("system hostname is empty");
    }
    Ok(hostname)
}

fn configure_host_unbound(config: &Config) -> Result<()> {
    let domain = domain(config)?;
    let bridge_ip = config.bridge_ip.parse::<Ipv4Addr>()?;
    let network_ip = config.network_ip.parse::<Ipv4Addr>()?;
    let bridge_ip6 = format!("{}{}", config.ipv6_prefix, config.bridge_ip6).parse::<Ipv6Addr>()?;
    let network_ip6 =
        format!("{}{}", config.ipv6_prefix, config.network_ip6).parse::<Ipv6Addr>()?;
    let reverse_zone = ipv4_reverse_zone(bridge_ip);
    let reverse_zone6 = ipv6_reverse_zone(network_ip6);
    let unbound = Path::new("/var/unbound");
    let mut interfaces = String::from("  interface: 127.0.0.1\n");
    let mut access_control = String::from("  access-control: 127.0.0.0/8 allow\n");
    if config.use_ipv4 {
        interfaces.push_str(&format!("  interface: {bridge_ip}\n"));
        access_control.push_str("  access-control: 0.0.0.0/0 allow_snoop\n");
    }
    if config.use_ipv6 {
        interfaces.push_str(&format!("  interface: {bridge_ip6}\n"));
        access_control.push_str("  access-control: ::0/0 allow_snoop\n");
    }
    let mut priosun_config = format!(
        "forward-zone:\n  name: \"{domain}\"\n  forward-addr: {}\n",
        if config.use_ipv4 {
            network_ip.to_string()
        } else {
            network_ip6.to_string()
        }
    );
    if config.use_ipv4 {
        priosun_config.push_str(&format!(
            "\nforward-zone:\n  name: \"{reverse_zone}\"\n  forward-addr: {network_ip}\n"
        ));
    }
    if config.use_ipv6 {
        priosun_config.push_str(&format!(
            "\nforward-zone:\n  name: \"{reverse_zone6}\"\n  forward-addr: {network_ip6}\n"
        ));
    }
    crate::util::cmd::message("Configuring host Unbound");
    fs::create_dir_all(unbound.join("conf.d"))?;
    fs::create_dir_all(unbound.join("zones"))?;
    if !unbound.join("root.hints").exists() {
        crate::util::cmd::run(
            "fetch",
            &[
                "-o",
                "/var/unbound/root.hints",
                "https://www.internic.net/domain/named.cache",
            ],
        )?;
    }
    crate::util::cmd::run(
        "sysrc",
        &[
            "local_unbound_enable=YES",
            "local_unbound_tls=NO",
            "local_unbound_svcj=YES",
        ],
    )?;
    fs::write(
        unbound.join("unbound.conf"),
        format!(
            "server:\n  verbosity: 1\n  username: unbound\n  directory: /var/unbound\n  chroot: /var/unbound\n  pidfile: /var/run/local_unbound.pid\n  auto-trust-anchor-file: /var/unbound/root.key\n  root-hints: /var/unbound/root.hints\n{interfaces}{access_control}  val-permissive-mode: yes\n\ninclude: /var/unbound/priosun.conf\ninclude: /var/unbound/control.conf\ninclude: /var/unbound/forward.conf\ninclude: /var/unbound/lan-zones.conf\ninclude: /var/unbound/conf.d/*.conf\n"
        ),
    )?;
    fs::write(unbound.join("priosun.conf"), priosun_config)?;
    fs::write(
        unbound.join("control.conf"),
        "remote-control:\n  control-enable: yes\n  control-use-cert: no\n  control-interface: /var/unbound/local.ctl\n",
    )?;
    fs::write(unbound.join("lan-zones.conf"), "")?;
    fs::write(
        unbound.join("forward.conf"),
        upstream_forward_config(config)?,
    )?;
    fs::write(
        "/etc/resolvconf.conf",
        "name_servers=127.0.0.1\nresolv_conf_local_only=YES\nunbound_conf=\"/var/unbound/forward.conf\"\nunbound_pid=\"/var/run/local_unbound.pid\"\nunbound_service=\"local_unbound\"\nunbound_restart=\"service local_unbound reload\"\n",
    )?;
    crate::util::cmd::run("chown", &["-R", "unbound:unbound", "/var/unbound"])?;
    crate::util::cmd::run("service", &["local_unbound", "restart"])?;
    crate::util::cmd::run("resolvconf", &["-u"])?;
    Ok(())
}

fn upstream_forward_config(config: &Config) -> Result<String> {
    let mut nameservers = config.dns_override.clone();
    if nameservers.is_empty() && config.resolv_conf.exists() {
        let content = fs::read_to_string(&config.resolv_conf)?;
        nameservers = content
            .lines()
            .filter_map(|line| line.strip_prefix("nameserver "))
            .map(str::trim)
            .filter(|server| !server.is_empty() && *server != "127.0.0.1" && *server != "::1")
            .map(str::to_string)
            .collect();
    }
    if nameservers.is_empty() {
        nameservers.push("1.1.1.1".to_string());
    }
    let mut output = String::from("forward-zone:\n  name: \".\"\n");
    for nameserver in nameservers {
        output.push_str(&format!("  forward-addr: {nameserver}\n"));
    }
    Ok(output)
}

fn ipv4_reverse_zone(address: Ipv4Addr) -> String {
    let octets = address.octets();
    format!("{}.{}.{}.in-addr.arpa", octets[2], octets[1], octets[0])
}

fn ipv6_reverse_zone(address: Ipv6Addr) -> String {
    let hex = address
        .segments()
        .iter()
        .map(|segment| format!("{segment:04x}"))
        .collect::<String>();
    hex[..16]
        .chars()
        .rev()
        .map(|character| format!("{character}."))
        .collect::<String>()
        + "ip6.arpa"
}
